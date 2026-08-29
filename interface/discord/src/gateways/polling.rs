use std::{
    future::Future,
    pin::{Pin, pin},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_lock::MutexGuardArc;
use futures::{
    FutureExt as _, StreamExt,
    channel::{mpsc, oneshot},
    future::{self, Either},
    lock::Mutex as AsyncMutex,
    select, stream,
    stream::FuturesUnordered,
};
use futures_timer::Delay;
use messenger_interface::{
    interface::{VideoBroadcastKind, VideoEvent},
    owner::Retained,
};
use tracing::{error, trace, warn};

use super::{
    Gateway, GatewayFrame, Heartbeat, ReconnectMode,
    general::{General, Opcode},
    media::{ConnectionRole, Media, StreamGateway, StreamKey, VoiceOpcode},
};
use crate::{DiscordRuntimeState, InnerDiscord, backend::Committed, gateways::GatewayStreamExt};

const DISPATCH_QUEUE_CAPACITY: usize = 128;
const TOPOLOGY_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

enum WatchedGatewayTurn {
    Event {
        key: StreamKey,
        slot: Arc<StreamGateway>,
        gateway: Arc<Gateway<Media>>,
        event: Option<GatewayFrame<VoiceOpcode>>,
    },
    Heartbeat {
        key: StreamKey,
        slot: Arc<StreamGateway>,
        gateway: Arc<Gateway<Media>>,
        result: Result<(), async_tungstenite::tungstenite::Error>,
    },
}

enum WatchedGatewayWake {
    Event {
        key: StreamKey,
        slot: Arc<StreamGateway>,
        gateway: Arc<Gateway<Media>>,
        event: Option<GatewayFrame<VoiceOpcode>>,
    },
    HeartbeatDue {
        key: StreamKey,
        slot: Arc<StreamGateway>,
        gateway: Arc<Gateway<Media>>,
    },
}

enum GatewayAction {
    MainEvent(Option<GatewayFrame<Opcode>>),
    MainHeartbeat(Result<(), async_tungstenite::tungstenite::Error>),
    VoiceEvent {
        gateway: Arc<Gateway<Media>>,
        event: Option<GatewayFrame<VoiceOpcode>>,
    },
    VoiceHeartbeat {
        gateway: Arc<Gateway<Media>>,
        result: Result<(), async_tungstenite::tungstenite::Error>,
    },
    Watched(WatchedGatewayTurn),
    BroadcastEvent {
        gateway: Arc<Gateway<Media>>,
        event: Option<GatewayFrame<VoiceOpcode>>,
    },
    BroadcastHeartbeat {
        gateway: Arc<Gateway<Media>>,
        result: Result<(), async_tungstenite::tungstenite::Error>,
    },
    DriverStopped,
}

enum DriverWake {
    Action(GatewayAction),
    MainHeartbeatDue,
    VoiceHeartbeatDue(Arc<Gateway<Media>>),
    WatchedHeartbeatDue {
        key: StreamKey,
        slot: Arc<StreamGateway>,
        gateway: Arc<Gateway<Media>>,
    },
    BroadcastHeartbeatDue(Arc<Gateway<Media>>),
    HeartbeatFinished(HeartbeatCompletion),
    Refresh,
}

enum HeartbeatRequest {
    Main(Arc<Gateway<General>>),
    Voice(Arc<Gateway<Media>>),
    Watched {
        key: StreamKey,
        slot: Arc<StreamGateway>,
        gateway: Arc<Gateway<Media>>,
    },
    Broadcast(Arc<Gateway<Media>>),
}

enum HeartbeatCompletion {
    Main {
        gateway: Arc<Gateway<General>>,
        result: Result<(), async_tungstenite::tungstenite::Error>,
    },
    Voice {
        gateway: Arc<Gateway<Media>>,
        result: Result<(), async_tungstenite::tungstenite::Error>,
    },
    Watched {
        key: StreamKey,
        slot: Arc<StreamGateway>,
        gateway: Arc<Gateway<Media>>,
        result: Result<(), async_tungstenite::tungstenite::Error>,
    },
    Broadcast {
        gateway: Arc<Gateway<Media>>,
        result: Result<(), async_tungstenite::tungstenite::Error>,
    },
}

type HeartbeatFuture = Pin<Box<dyn Future<Output = HeartbeatCompletion> + Send>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnqueueOutcome {
    Queued,
    Overloaded,
    Closed,
}

fn try_enqueue<T>(
    sender: &mut mpsc::Sender<T>,
    terminal_failure: &AtomicBool,
    item: T,
) -> EnqueueOutcome {
    match sender.try_send(item) {
        Ok(()) => EnqueueOutcome::Queued,
        Err(error) if error.is_full() => {
            terminal_failure.store(true, Ordering::Release);
            EnqueueOutcome::Overloaded
        }
        Err(_) => EnqueueOutcome::Closed,
    }
}

/// One root gateway's transport driver and bounded ordered dispatch queue.
///
/// The driver owns websocket receive/heartbeat progress. Consumers take one
/// mutation-bearing action at a time under the supervisor turn permit, so slow
/// cache/CDN/media work cannot starve protocol liveness.
pub(super) struct GatewaySession {
    gateway: Arc<Gateway<General>>,
    receiver: AsyncMutex<mpsc::Receiver<GatewayAction>>,
    terminal_failure: Arc<AtomicBool>,
    cancel: StdMutex<Option<oneshot::Sender<()>>>,
    task: StdMutex<Option<smol::Task<()>>>,
}

impl GatewaySession {
    pub(super) fn start(gateway: Arc<Gateway<General>>) -> Arc<Self> {
        // futures mpsc grants its sole sender one extra slot in addition to
        // the buffer, so subtract it to keep the total queue cap exact.
        let (sender, receiver) = mpsc::channel(DISPATCH_QUEUE_CAPACITY - 1);
        let (cancel, canceled) = oneshot::channel();
        let terminal_failure = Arc::new(AtomicBool::new(false));
        let driver_terminal = Arc::clone(&terminal_failure);
        let driver_gateway = Arc::clone(&gateway);
        let task = smol::spawn(async move {
            let driver = run_gateway_driver(driver_gateway, sender, driver_terminal).fuse();
            let canceled = canceled.fuse();
            futures::pin_mut!(driver, canceled);
            select! {
                () = driver => {}
                _ = canceled => {}
            }
        });

        Arc::new(Self {
            gateway,
            receiver: AsyncMutex::new(receiver),
            terminal_failure,
            cancel: StdMutex::new(Some(cancel)),
            task: StdMutex::new(Some(task)),
        })
    }

    pub(super) fn gateway(&self) -> &Arc<Gateway<General>> {
        &self.gateway
    }

    pub(super) fn has_terminal_failure(&self) -> bool {
        self.terminal_failure.load(Ordering::Acquire)
    }

    async fn next_action(&self) -> Option<GatewayAction> {
        self.receiver.lock().await.next().await
    }

    pub(super) async fn stop(&self) {
        if let Some(cancel) = self
            .cancel
            .lock()
            .expect("gateway driver cancellation mutex poisoned")
            .take()
        {
            let _ = cancel.send(());
        }
        let task = self
            .task
            .lock()
            .expect("gateway driver task mutex poisoned")
            .take();
        if let Some(task) = task {
            task.await;
        }
    }
}

/// Selected ordered gateway action. Selection may be cancelled; returned turns
/// must execute to completion.
pub(crate) struct GatewayTurn {
    permit: MutexGuardArc<()>,
    gateway: Arc<Gateway<General>>,
    action: GatewayAction,
}

async fn poll_watched_gateway(
    key: StreamKey,
    slot: Arc<StreamGateway>,
    gateway: Arc<Gateway<Media>>,
) -> WatchedGatewayWake {
    enum Outcome {
        Event(Option<GatewayFrame<VoiceOpcode>>),
        HeartbeatDue,
    }

    let outcome = {
        let mut receiver_guard = gateway.websocket.receiver.lock().await;
        let mut events = pin!(receiver_guard.gateway_frames::<VoiceOpcode>());
        let mut heartbeat_due = pin!(
            async {
                if gateway.heartbeat_in_flight() {
                    future::pending().await
                } else {
                    gateway.wait_for_heartbeat().await
                }
            }
            .fuse()
        );
        select! {
            event = events.next() => Outcome::Event(event),
            () = heartbeat_due => Outcome::HeartbeatDue,
        }
    };
    match outcome {
        Outcome::Event(event) => WatchedGatewayWake::Event {
            key,
            slot,
            gateway,
            event,
        },
        Outcome::HeartbeatDue => WatchedGatewayWake::HeartbeatDue { key, slot, gateway },
    }
}

async fn poll_watched_gateways(
    streams: Vec<(StreamKey, Arc<StreamGateway>, Arc<Gateway<Media>>)>,
) -> WatchedGatewayWake {
    let polls = streams
        .into_iter()
        .map(|(key, slot, gateway)| Box::pin(poll_watched_gateway(key, slot, gateway)))
        .collect::<Vec<_>>();
    if polls.is_empty() {
        future::pending().await
    } else {
        future::select_all(polls).await.0
    }
}

fn media_is_suspended(suspended: &[Arc<Gateway<Media>>], candidate: &Arc<Gateway<Media>>) -> bool {
    suspended
        .iter()
        .any(|blocked| Arc::ptr_eq(blocked, candidate))
}

fn suspend_media(suspended: &mut Vec<Arc<Gateway<Media>>>, candidate: &Arc<Gateway<Media>>) {
    if !media_is_suspended(suspended, candidate) {
        suspended.push(Arc::clone(candidate));
    }
}

async fn poll_gateway_action(
    gateway: &Arc<Gateway<General>>,
    suspended: &mut Vec<Arc<Gateway<Media>>>,
    heartbeat_tasks: &mut FuturesUnordered<HeartbeatFuture>,
) -> DriverWake {
    // Snapshot dynamic media topology. A short refresh branch notices newly
    // published gateways even while every existing transport is otherwise idle.
    let voice_gateway = gateway.voice.load_gateway();
    let watched_gateways = gateway
        .voice
        .watched_streams_for_poll()
        .into_iter()
        .filter_map(|(key, slot)| {
            slot.load_gateway()
                .map(|stream_gateway| (key, slot, stream_gateway))
        })
        .collect::<Vec<_>>();
    let broadcast_gateway = gateway.voice.broadcast.load_gateway();

    let mut current_media = Vec::new();
    current_media.extend(voice_gateway.iter().cloned());
    current_media.extend(
        watched_gateways
            .iter()
            .map(|(_, _, stream_gateway)| Arc::clone(stream_gateway)),
    );
    current_media.extend(broadcast_gateway.iter().cloned());
    suspended.retain(|blocked| {
        current_media
            .iter()
            .any(|current| Arc::ptr_eq(blocked, current))
    });

    let voice_gateway = voice_gateway.filter(|candidate| !media_is_suspended(suspended, candidate));
    let watched_gateways: Vec<_> = watched_gateways
        .into_iter()
        .filter(|(_, _, candidate)| !media_is_suspended(suspended, candidate))
        .collect();
    let broadcast_gateway =
        broadcast_gateway.filter(|candidate| !media_is_suspended(suspended, candidate));

    // A timed-out/cancelled sink write poisons that split websocket. Never
    // reuse its receiver half; queue the same generation-aware teardown path as
    // a heartbeat write failure.
    if gateway.websocket.send_failed() && !gateway.heartbeat_in_flight() {
        return DriverWake::Action(GatewayAction::MainHeartbeat(Err(
            async_tungstenite::tungstenite::Error::ConnectionClosed,
        )));
    }
    if let Some(voice_gateway) = voice_gateway.as_ref()
        && voice_gateway.websocket.send_failed()
        && !voice_gateway.heartbeat_in_flight()
    {
        return DriverWake::Action(GatewayAction::VoiceHeartbeat {
            gateway: Arc::clone(voice_gateway),
            result: Err(async_tungstenite::tungstenite::Error::ConnectionClosed),
        });
    }
    if let Some((key, slot, stream_gateway)) =
        watched_gateways.iter().find(|(_, _, stream_gateway)| {
            stream_gateway.websocket.send_failed() && !stream_gateway.heartbeat_in_flight()
        })
    {
        return DriverWake::Action(GatewayAction::Watched(WatchedGatewayTurn::Heartbeat {
            key: key.clone(),
            slot: Arc::clone(slot),
            gateway: Arc::clone(stream_gateway),
            result: Err(async_tungstenite::tungstenite::Error::ConnectionClosed),
        }));
    }
    if let Some(broadcast_gateway) = broadcast_gateway.as_ref()
        && broadcast_gateway.websocket.send_failed()
        && !broadcast_gateway.heartbeat_in_flight()
    {
        return DriverWake::Action(GatewayAction::BroadcastHeartbeat {
            gateway: Arc::clone(broadcast_gateway),
            result: Err(async_tungstenite::tungstenite::Error::ConnectionClosed),
        });
    }

    let mut gateway_receiver_guard = gateway.websocket.receiver.lock().await;
    let mut gateway_receiver = pin!(gateway_receiver_guard.gateway_frames::<Opcode>());

    let voice_gateway_for_heartbeat = voice_gateway.clone();
    let mut voice_receiver_guard;
    let mut voice_receiver = pin!(match voice_gateway.as_ref() {
        Some(voice_gateway) => {
            trace!("gateway driver: waiting for voice receiver lock");
            voice_receiver_guard = voice_gateway.websocket.receiver.lock().await;
            trace!("gateway driver: voice receiver lock acquired");
            Either::Right(voice_receiver_guard.gateway_frames::<VoiceOpcode>())
        }
        None => Either::Left(stream::empty()),
    });

    let mut watched_gateway_turn = pin!(poll_watched_gateways(watched_gateways).fuse());

    let broadcast_gateway_for_heartbeat = broadcast_gateway.clone();
    let mut broadcast_receiver_guard;
    let mut broadcast_receiver = pin!(match broadcast_gateway.as_ref() {
        Some(broadcast_gateway) => {
            broadcast_receiver_guard = broadcast_gateway.websocket.receiver.lock().await;
            Either::Right(broadcast_receiver_guard.gateway_frames::<VoiceOpcode>())
        }
        None => Either::Left(stream::empty()),
    });

    let mut refresh = pin!(Delay::new(TOPOLOGY_REFRESH_INTERVAL).fuse());
    select! {
        event = gateway_receiver.next() => DriverWake::Action(GatewayAction::MainEvent(event)),
        event = voice_receiver.next() => DriverWake::Action(GatewayAction::VoiceEvent {
            gateway: voice_gateway
                .as_ref()
                .expect("voice event stream is pending without a gateway")
                .clone(),
            event,
        }),
        () = async {
            if gateway.heartbeat_in_flight() {
                future::pending().await
            } else {
                gateway.wait_for_heartbeat().await
            }
        }.fuse() => DriverWake::MainHeartbeatDue,
        () = async {
            match voice_gateway_for_heartbeat.as_ref() {
                Some(voice_gateway) if !voice_gateway.heartbeat_in_flight() => {
                    voice_gateway.wait_for_heartbeat().await
                }
                _ => future::pending().await,
            }
        }.fuse() => DriverWake::VoiceHeartbeatDue(
            voice_gateway
                .as_ref()
                .expect("voice heartbeat is pending without a gateway")
                .clone(),
        ),
        watched = watched_gateway_turn => match watched {
            WatchedGatewayWake::Event { key, slot, gateway, event } => {
                DriverWake::Action(GatewayAction::Watched(WatchedGatewayTurn::Event {
                    key,
                    slot,
                    gateway,
                    event,
                }))
            }
            WatchedGatewayWake::HeartbeatDue { key, slot, gateway } => {
                DriverWake::WatchedHeartbeatDue { key, slot, gateway }
            }
        },
        event = broadcast_receiver.next() => {
            DriverWake::Action(GatewayAction::BroadcastEvent {
                gateway: broadcast_gateway
                    .as_ref()
                    .expect("broadcast event stream is pending without a gateway")
                    .clone(),
                event,
            })
        },
        () = async {
            match broadcast_gateway_for_heartbeat.as_ref() {
                Some(broadcast_gateway) if !broadcast_gateway.heartbeat_in_flight() => {
                    broadcast_gateway.wait_for_heartbeat().await
                }
                _ => future::pending().await,
            }
        }.fuse() => DriverWake::BroadcastHeartbeatDue(
            broadcast_gateway
                .as_ref()
                .expect("broadcast heartbeat is pending without a gateway")
                .clone(),
        ),
        completion = async {
            if heartbeat_tasks.is_empty() {
                future::pending().await
            } else {
                heartbeat_tasks
                    .next()
                    .await
                    .expect("non-empty heartbeat task set must yield a completion")
            }
        }.fuse() => DriverWake::HeartbeatFinished(completion),
        () = refresh => DriverWake::Refresh,
    }
}

fn main_frame_requests_heartbeat(frame: &GatewayFrame<Opcode>) -> bool {
    frame
        .payload()
        .is_ok_and(|payload| matches!(payload.opcode(), Opcode::Heartbeat))
}

fn voice_frame_requests_heartbeat(frame: &GatewayFrame<VoiceOpcode>) -> bool {
    frame
        .payload()
        .is_ok_and(|payload| matches!(payload.opcode(), VoiceOpcode::Heartbeat))
}

fn requested_heartbeat(
    root: &Arc<Gateway<General>>,
    action: &GatewayAction,
) -> Option<HeartbeatRequest> {
    match action {
        GatewayAction::MainEvent(Some(frame)) if main_frame_requests_heartbeat(frame) => {
            Some(HeartbeatRequest::Main(Arc::clone(root)))
        }
        GatewayAction::VoiceEvent {
            gateway,
            event: Some(frame),
        } if voice_frame_requests_heartbeat(frame) => {
            Some(HeartbeatRequest::Voice(Arc::clone(gateway)))
        }
        GatewayAction::Watched(WatchedGatewayTurn::Event {
            key,
            slot,
            gateway,
            event: Some(frame),
        }) if frame
            .payload()
            .is_ok_and(|payload| matches!(payload.opcode(), VoiceOpcode::Heartbeat)) =>
        {
            Some(HeartbeatRequest::Watched {
                key: key.clone(),
                slot: Arc::clone(slot),
                gateway: Arc::clone(gateway),
            })
        }
        GatewayAction::BroadcastEvent {
            gateway,
            event: Some(frame),
        } if voice_frame_requests_heartbeat(frame) => {
            Some(HeartbeatRequest::Broadcast(Arc::clone(gateway)))
        }
        _ => None,
    }
}

fn observe_main_frame(gateway: &Gateway<General>, frame: &GatewayFrame<Opcode>) -> bool {
    let Ok(payload) = frame.payload() else {
        return false;
    };
    if let Some(sequence) = payload.sequence() {
        gateway
            .state
            .last_sequence_number
            .get_or_init(|| sequence.into())
            .store(sequence, Ordering::Relaxed);
    }
    match payload.opcode() {
        Opcode::HeartbeatAck => {
            gateway.state.heartbeat_acknowledged.acknowledge();
            trace!("Gateway heartbeat ACK handled by transport driver");
            true
        }
        Opcode::Heartbeat => {
            trace!("Gateway heartbeat request handled by transport driver");
            true
        }
        _ => false,
    }
}

fn observe_voice_frame(gateway: &Gateway<Media>, frame: &GatewayFrame<VoiceOpcode>) -> bool {
    let Ok(payload) = frame.payload() else {
        return false;
    };
    if let Some(sequence) = payload.sequence() {
        gateway
            .state
            .last_sequence_number
            .get_or_init(|| sequence.into())
            .store(sequence, Ordering::Relaxed);
    }
    match payload.opcode() {
        VoiceOpcode::HeartbeatACK => {
            gateway.state.heartbeat_acknowledged.acknowledge();
            trace!("Voice heartbeat ACK handled by transport driver");
            true
        }
        VoiceOpcode::Heartbeat => {
            trace!("Voice heartbeat request handled by transport driver");
            true
        }
        _ => false,
    }
}

/// Observe sequence/ACK transport state and report actions which require no
/// ordered application dispatch.
fn consume_transport_control(root: &Gateway<General>, action: &GatewayAction) -> bool {
    match action {
        GatewayAction::MainEvent(Some(frame)) => observe_main_frame(root, frame),
        GatewayAction::MainHeartbeat(Ok(())) => true,
        GatewayAction::VoiceEvent {
            gateway,
            event: Some(frame),
        }
        | GatewayAction::BroadcastEvent {
            gateway,
            event: Some(frame),
        } => observe_voice_frame(gateway, frame),
        GatewayAction::VoiceHeartbeat { result: Ok(()), .. }
        | GatewayAction::BroadcastHeartbeat { result: Ok(()), .. } => true,
        GatewayAction::Watched(WatchedGatewayTurn::Event {
            gateway,
            event: Some(frame),
            ..
        }) => observe_voice_frame(gateway, frame),
        GatewayAction::Watched(WatchedGatewayTurn::Heartbeat { result: Ok(()), .. }) => true,
        _ => false,
    }
}

fn failed_media_gateway(action: &GatewayAction) -> Option<&Arc<Gateway<Media>>> {
    match action {
        GatewayAction::VoiceEvent {
            gateway,
            event: None,
        }
        | GatewayAction::VoiceHeartbeat {
            gateway,
            result: Err(_),
        }
        | GatewayAction::BroadcastEvent {
            gateway,
            event: None,
        }
        | GatewayAction::BroadcastHeartbeat {
            gateway,
            result: Err(_),
        } => Some(gateway),
        GatewayAction::Watched(WatchedGatewayTurn::Event {
            gateway,
            event: None,
            ..
        })
        | GatewayAction::Watched(WatchedGatewayTurn::Heartbeat {
            gateway,
            result: Err(_),
            ..
        }) => Some(gateway),
        _ => None,
    }
}

fn root_transport_failed(action: &GatewayAction) -> bool {
    matches!(
        action,
        GatewayAction::MainEvent(None) | GatewayAction::MainHeartbeat(Err(_))
    )
}

fn schedule_requested_heartbeat(
    request: HeartbeatRequest,
    heartbeat_tasks: &mut FuturesUnordered<HeartbeatFuture>,
) {
    match request {
        HeartbeatRequest::Main(gateway) => {
            if gateway.try_begin_heartbeat() {
                heartbeat_tasks.push(Box::pin(async move {
                    let result = gateway.heartbeat_now().await;
                    HeartbeatCompletion::Main { gateway, result }
                }));
            }
        }
        HeartbeatRequest::Voice(gateway) => {
            if gateway.try_begin_heartbeat() {
                heartbeat_tasks.push(Box::pin(async move {
                    let result = gateway.heartbeat_now().await;
                    HeartbeatCompletion::Voice { gateway, result }
                }));
            }
        }
        HeartbeatRequest::Watched { key, slot, gateway } => {
            if gateway.try_begin_heartbeat() {
                heartbeat_tasks.push(Box::pin(async move {
                    let result = gateway.heartbeat_now().await;
                    HeartbeatCompletion::Watched {
                        key,
                        slot,
                        gateway,
                        result,
                    }
                }));
            }
        }
        HeartbeatRequest::Broadcast(gateway) => {
            if gateway.try_begin_heartbeat() {
                heartbeat_tasks.push(Box::pin(async move {
                    let result = gateway.heartbeat_now().await;
                    HeartbeatCompletion::Broadcast { gateway, result }
                }));
            }
        }
    }
}

async fn run_gateway_driver(
    gateway: Arc<Gateway<General>>,
    mut sender: mpsc::Sender<GatewayAction>,
    terminal_failure: Arc<AtomicBool>,
) {
    let mut suspended = Vec::new();
    let mut discard_mutations = false;
    let mut heartbeat_tasks = FuturesUnordered::<HeartbeatFuture>::new();

    loop {
        let action = match poll_gateway_action(&gateway, &mut suspended, &mut heartbeat_tasks).await
        {
            DriverWake::Action(action) => action,
            DriverWake::MainHeartbeatDue => {
                if gateway.try_begin_heartbeat() {
                    let heartbeat_gateway = Arc::clone(&gateway);
                    heartbeat_tasks.push(Box::pin(async move {
                        let result = heartbeat_gateway.heartbeat().await;
                        HeartbeatCompletion::Main {
                            gateway: heartbeat_gateway,
                            result,
                        }
                    }));
                }
                continue;
            }
            DriverWake::VoiceHeartbeatDue(voice_gateway) => {
                if voice_gateway.try_begin_heartbeat() {
                    heartbeat_tasks.push(Box::pin(async move {
                        let result = voice_gateway.heartbeat().await;
                        HeartbeatCompletion::Voice {
                            gateway: voice_gateway,
                            result,
                        }
                    }));
                }
                continue;
            }
            DriverWake::WatchedHeartbeatDue {
                key,
                slot,
                gateway: stream_gateway,
            } => {
                if stream_gateway.try_begin_heartbeat() {
                    heartbeat_tasks.push(Box::pin(async move {
                        let result = stream_gateway.heartbeat().await;
                        HeartbeatCompletion::Watched {
                            key,
                            slot,
                            gateway: stream_gateway,
                            result,
                        }
                    }));
                }
                continue;
            }
            DriverWake::BroadcastHeartbeatDue(broadcast_gateway) => {
                if broadcast_gateway.try_begin_heartbeat() {
                    heartbeat_tasks.push(Box::pin(async move {
                        let result = broadcast_gateway.heartbeat().await;
                        HeartbeatCompletion::Broadcast {
                            gateway: broadcast_gateway,
                            result,
                        }
                    }));
                }
                continue;
            }
            DriverWake::HeartbeatFinished(completion) => match completion {
                HeartbeatCompletion::Main {
                    gateway: heartbeat_gateway,
                    result,
                } => {
                    heartbeat_gateway.finish_heartbeat();
                    match result {
                        Ok(()) => continue,
                        Err(error) => GatewayAction::MainHeartbeat(Err(error)),
                    }
                }
                HeartbeatCompletion::Voice {
                    gateway: voice_gateway,
                    result,
                } => {
                    voice_gateway.finish_heartbeat();
                    match result {
                        Ok(()) => continue,
                        Err(error) => GatewayAction::VoiceHeartbeat {
                            gateway: voice_gateway,
                            result: Err(error),
                        },
                    }
                }
                HeartbeatCompletion::Watched {
                    key,
                    slot,
                    gateway: stream_gateway,
                    result,
                } => {
                    stream_gateway.finish_heartbeat();
                    match result {
                        Ok(()) => continue,
                        Err(error) => GatewayAction::Watched(WatchedGatewayTurn::Heartbeat {
                            key,
                            slot,
                            gateway: stream_gateway,
                            result: Err(error),
                        }),
                    }
                }
                HeartbeatCompletion::Broadcast {
                    gateway: broadcast_gateway,
                    result,
                } => {
                    broadcast_gateway.finish_heartbeat();
                    match result {
                        Ok(()) => continue,
                        Err(error) => GatewayAction::BroadcastHeartbeat {
                            gateway: broadcast_gateway,
                            result: Err(error),
                        },
                    }
                }
            },
            DriverWake::Refresh => continue,
        };

        if let Some(request) = requested_heartbeat(&gateway, &action) {
            let consumed = consume_transport_control(&gateway, &action);
            debug_assert!(consumed);
            schedule_requested_heartbeat(request, &mut heartbeat_tasks);
            continue;
        }
        if consume_transport_control(&gateway, &action) {
            continue;
        }

        if let Some(failed) = failed_media_gateway(&action) {
            suspend_media(&mut suspended, failed);
        }
        let root_failed = root_transport_failed(&action);

        if discard_mutations {
            if root_failed {
                return;
            }
            continue;
        }

        match try_enqueue(&mut sender, &terminal_failure, action) {
            EnqueueOutcome::Queued => {
                if root_failed {
                    return;
                }
            }
            EnqueueOutcome::Overloaded => {
                error!(
                    capacity = DISPATCH_QUEUE_CAPACITY,
                    "Discord gateway dispatch queue overflowed; dropping pending realtime state and reconnecting"
                );
                // Continue pumping only transport-control traffic until the
                // ordered executor observes the terminal flag and cancels us.
                discard_mutations = true;
                if root_failed {
                    return;
                }
            }
            EnqueueOutcome::Closed => return,
        }
    }
}

/// Borrowed gateway-session teardown dependencies.
struct GatewaySessions<'a> {
    runtime: &'a DiscordRuntimeState,
}

impl<'a> GatewaySessions<'a> {
    fn new(runtime: &'a DiscordRuntimeState) -> Self {
        Self { runtime }
    }

    /// Tear down watched Go Live stream and optionally remove availability.
    async fn end_watched_stream(
        &self,
        gateway: &Gateway<General>,
        key: &StreamKey,
        expected: &Arc<StreamGateway>,
        remove_availability: bool,
    ) {
        let source = expected.watched_source().await;
        if let Some(slot) = gateway.voice.remove_watched_stream_if(key, expected) {
            if remove_availability
                && let Some((user, channel)) = source
                && let Some(id) = self.runtime.media.remove_participant_video(
                    user,
                    channel,
                    VideoBroadcastKind::Screen,
                )
            {
                gateway
                    .events
                    .submit_video(VideoEvent::RemoveVideoStream { id });
            }
            slot.disconnect().await;
        }
    }

    async fn end_broadcast(&self, gateway: &Gateway<General>) {
        gateway.voice.broadcast.disconnect().await;
    }

    async fn end_voice_session(&self, gateway: &Gateway<General>) {
        for (key, slot) in gateway.voice.watched_streams() {
            self.end_watched_stream(gateway, &key, &slot, true).await;
        }
        self.end_broadcast(gateway).await;
        gateway.voice.disconnect().await;
    }

    async fn end_gateway_session(&self, gateway: &Gateway<General>, reconnect: ReconnectMode) {
        let resumable = reconnect.wants_resume() && self.runtime.gateway.can_resume(gateway);
        let Some(session) = self.runtime.gateway.unpublish(gateway) else {
            trace!("Ignoring teardown for a stale Discord gateway session");
            return;
        };
        if let Some(delay) = reconnect.delay() {
            self.runtime.gateway.defer_reconnect(delay);
        }
        if !resumable {
            // Identify cannot replay deltas missed during disconnect. Disable
            // stale Resume data and ask the query consumer for an authoritative
            // application snapshot.
            self.runtime.gateway.discard_resume_session(gateway);
            self.runtime.invalidate_realtime_state();
        }
        // Unpublish before stopping transport and dependants; the turn permit
        // blocks a replacement from being initialized until teardown completes.
        // Voice sockets are intentionally restarted even after a main-gateway
        // Resume, while their removal events remain queued in the shared hub.
        session.stop().await;
        self.end_voice_session(gateway).await;
        if resumable && gateway.events.is_invalidated() {
            // Voice teardown can itself fill an already-near-capacity route.
            // Never carry an invalid event hub into the resumed generation.
            self.runtime.gateway.discard_resume_session(gateway);
            self.runtime.invalidate_realtime_state();
        }
    }
}

fn voice_gateway_is_current(root: &Gateway<General>, candidate: &Arc<Gateway<Media>>) -> bool {
    root.voice
        .load_gateway()
        .is_some_and(|current| Arc::ptr_eq(&current, candidate))
}

fn broadcast_gateway_is_current(root: &Gateway<General>, candidate: &Arc<Gateway<Media>>) -> bool {
    root.voice
        .broadcast
        .load_gateway()
        .is_some_and(|current| Arc::ptr_eq(&current, candidate))
}

impl DiscordRuntimeState {
    /// Takes the next queued mutation under the ordering permit. Transport
    /// heartbeat/ACK progress is owned by the session driver and does not wait
    /// for this permit.
    pub(crate) async fn wait_for_gateway_turn(&self) -> Option<GatewayTurn> {
        loop {
            let session = self.gateway.session.load_full()?;
            let permit = self.gateway.turn.lock_arc().await;
            let current = self.gateway.session.load_full();
            if !current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                drop(permit);
                continue;
            }

            let gateway = Arc::clone(session.gateway());
            let action = if session.has_terminal_failure() {
                GatewayAction::DriverStopped
            } else {
                session
                    .next_action()
                    .await
                    .unwrap_or(GatewayAction::DriverStopped)
            };
            return Some(GatewayTurn {
                permit,
                gateway,
                action,
            });
        }
    }

    /// Executes one queued action before releasing its ordering permit.
    async fn execute_gateway_turn(&self, turn: GatewayTurn) -> Option<()> {
        let GatewayTurn {
            permit,
            gateway,
            action,
        } = turn;
        let sessions = GatewaySessions::new(self);

        let outcome = match action {
            GatewayAction::MainEvent(Some(event)) => {
                let sequence = event.payload().ok().and_then(|payload| payload.sequence());
                match event.exec(self, &gateway).await {
                    Ok(()) => {
                        // Commit only after the mutation-bearing handler fully
                        // succeeds. Resume from the received sequence would skip
                        // queued events if this executor lagged the transport.
                        if let Some(sequence) = sequence {
                            self.gateway.commit_sequence(&gateway, sequence);
                        }
                        trace!("Gateway main event handled");
                        Some(())
                    }
                    Err(err) => {
                        error!(
                            "Gateway event failed and may have left partial state: {err}; rebuilding the session"
                        );
                        sessions
                            .end_gateway_session(&gateway, ReconnectMode::Reidentify)
                            .await;
                        None
                    }
                }
            }
            GatewayAction::MainEvent(None) => {
                warn!("Gateway connection closed; attempting to resume its session");
                sessions
                    .end_gateway_session(&gateway, ReconnectMode::Resume)
                    .await;
                None
            }
            GatewayAction::MainHeartbeat(Ok(())) => Some(()),
            GatewayAction::MainHeartbeat(Err(err)) => {
                warn!("Gateway heartbeat failed: {err}; attempting to resume the session");
                sessions
                    .end_gateway_session(&gateway, ReconnectMode::Resume)
                    .await;
                None
            }
            GatewayAction::VoiceEvent {
                gateway: voice_gateway,
                event: Some(event),
            } => {
                if voice_gateway_is_current(&gateway, &voice_gateway) {
                    let result = event
                        .exec(self, &voice_gateway, ConnectionRole::Call)
                        .await
                        .map_err(|error| error.to_string());
                    if let Err(error) = result {
                        error!(
                            "Voice gateway event failed: {error}; tearing down the voice session"
                        );
                        sessions.end_voice_session(&gateway).await;
                    } else {
                        trace!("Voice gateway event handled");
                    }
                }
                Some(())
            }
            GatewayAction::VoiceEvent {
                gateway: voice_gateway,
                event: None,
            } => {
                if voice_gateway_is_current(&gateway, &voice_gateway) {
                    warn!("Voice gateway connection closed; tearing down the voice session");
                    sessions.end_voice_session(&gateway).await;
                }
                Some(())
            }
            GatewayAction::VoiceHeartbeat {
                gateway: _,
                result: Ok(()),
            } => Some(()),
            GatewayAction::VoiceHeartbeat {
                gateway: voice_gateway,
                result: Err(err),
            } => {
                if voice_gateway_is_current(&gateway, &voice_gateway) {
                    error!("Voice gateway heartbeat failed: {err}; tearing down the voice session");
                    sessions.end_voice_session(&gateway).await;
                }
                Some(())
            }
            GatewayAction::Watched(WatchedGatewayTurn::Event {
                key,
                slot,
                gateway: stream_gateway,
                event,
            }) => {
                let current = gateway
                    .voice
                    .watched_stream(&key)
                    .is_some_and(|current| Arc::ptr_eq(&current, &slot))
                    && slot
                        .load_gateway()
                        .is_some_and(|current| Arc::ptr_eq(&current, &stream_gateway));
                if current {
                    match event {
                        Some(event) => {
                            let result = event
                                .exec(self, &stream_gateway, ConnectionRole::Stream)
                                .await
                                .map_err(|error| error.to_string());
                            if let Err(error) = result {
                                error!(
                                    "Stream gateway event failed for {key}: {error}; ending watched stream"
                                );
                                sessions
                                    .end_watched_stream(&gateway, &key, &slot, false)
                                    .await;
                            }
                        }
                        None => {
                            warn!("Stream gateway connection closed; ending watched stream {key}");
                            sessions
                                .end_watched_stream(&gateway, &key, &slot, false)
                                .await;
                        }
                    }
                }
                Some(())
            }
            GatewayAction::Watched(WatchedGatewayTurn::Heartbeat {
                key,
                slot,
                gateway: stream_gateway,
                result,
            }) => {
                let current = gateway
                    .voice
                    .watched_stream(&key)
                    .is_some_and(|current| Arc::ptr_eq(&current, &slot))
                    && slot
                        .load_gateway()
                        .is_some_and(|current| Arc::ptr_eq(&current, &stream_gateway));
                if current && let Err(err) = result {
                    error!(
                        "Stream gateway heartbeat failed for {key}: {err}; ending watched stream"
                    );
                    sessions
                        .end_watched_stream(&gateway, &key, &slot, false)
                        .await;
                }
                Some(())
            }
            GatewayAction::BroadcastEvent {
                gateway: broadcast_gateway,
                event: Some(event),
            } => {
                if broadcast_gateway_is_current(&gateway, &broadcast_gateway) {
                    let result = event
                        .exec(self, &broadcast_gateway, ConnectionRole::Stream)
                        .await
                        .map_err(|error| error.to_string());
                    if let Err(error) = result {
                        error!(
                            "Broadcast gateway event failed: {error}; ending the Go Live broadcast"
                        );
                        sessions.end_broadcast(&gateway).await;
                    }
                }
                Some(())
            }
            GatewayAction::BroadcastEvent {
                gateway: broadcast_gateway,
                event: None,
            } => {
                if broadcast_gateway_is_current(&gateway, &broadcast_gateway) {
                    warn!("Broadcast gateway connection closed; ending the Go Live broadcast");
                    sessions.end_broadcast(&gateway).await;
                }
                Some(())
            }
            GatewayAction::BroadcastHeartbeat {
                gateway: _,
                result: Ok(()),
            } => Some(()),
            GatewayAction::BroadcastHeartbeat {
                gateway: broadcast_gateway,
                result: Err(err),
            } => {
                if broadcast_gateway_is_current(&gateway, &broadcast_gateway) {
                    error!(
                        "Broadcast gateway heartbeat failed: {err}; ending the Go Live broadcast"
                    );
                    sessions.end_broadcast(&gateway).await;
                }
                Some(())
            }
            GatewayAction::DriverStopped => {
                error!(
                    "Discord gateway transport driver stopped or overloaded; rebuilding the session"
                );
                sessions
                    .end_gateway_session(&gateway, ReconnectMode::Reidentify)
                    .await;
                None
            }
        };

        drop(permit);
        outcome
    }

    /// Unpublishes root gateway before dependent teardown.
    pub(crate) async fn end_gateway_session(
        &self,
        gateway: &Gateway<General>,
        reconnect: ReconnectMode,
    ) {
        GatewaySessions::new(self)
            .end_gateway_session(gateway, reconnect)
            .await;
    }
}

/// Executes committed turn and returns retained backend.
pub(crate) async fn finish_committed_gateway_turn(
    committed: Committed<GatewayTurn>,
) -> Option<Retained<InnerDiscord>> {
    let (discord, turn) = committed.into_parts();
    let _ = discord.get().runtime().execute_gateway_turn(turn).await;
    Some(discord)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_heartbeat_requests_are_transport_control() {
        let main = GatewayFrame::<Opcode>::try_from(async_tungstenite::tungstenite::Message::Text(
            r#"{"op":1,"d":null}"#.into(),
        ))
        .unwrap();
        let voice = GatewayFrame::<VoiceOpcode>::try_from(
            async_tungstenite::tungstenite::Message::Text(r#"{"op":3,"d":null}"#.into()),
        )
        .unwrap();

        assert!(main_frame_requests_heartbeat(&main));
        assert!(voice_frame_requests_heartbeat(&voice));
    }

    #[test]
    fn bounded_dispatch_queue_marks_terminal_overload() {
        let (mut sender, _receiver) = mpsc::channel(1);
        let terminal = AtomicBool::new(false);
        let mut overloaded = false;

        for value in 0..16 {
            match try_enqueue(&mut sender, &terminal, value) {
                EnqueueOutcome::Queued => {}
                EnqueueOutcome::Overloaded => {
                    overloaded = true;
                    break;
                }
                EnqueueOutcome::Closed => panic!("test receiver remains connected"),
            }
        }

        assert!(overloaded);
        assert!(terminal.load(Ordering::Acquire));
    }
}
