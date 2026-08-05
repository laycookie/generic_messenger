use std::{
    pin::pin,
    sync::{Arc, atomic::Ordering},
};

use futures::{
    FutureExt as _, StreamExt,
    future::{self, Either},
    lock::MutexGuard,
    select, stream,
};
use messenger_interface::interface::VideoBroadcastKind;
use tracing::{debug, error, trace, warn};

use super::{
    Gateway, GatewayFrame, Heartbeat,
    general::{General, Opcode},
    media::{ConnectionRole, Media, StreamGateway, StreamKey, VoiceOpcode},
};
use crate::{InnerDiscord, UnitStruct, adapter::video::VideoControls, gateways::GatewayStreamExt};

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

enum GatewayAction {
    MainEvent(Option<GatewayFrame<Opcode>>),
    MainHeartbeat(Result<(), async_tungstenite::tungstenite::Error>),
    VoiceEvent {
        gateway: Arc<Gateway<Media>>,
        event: Option<GatewayFrame<VoiceOpcode>>,
    },
    VoiceHeartbeat(Result<(), async_tungstenite::tungstenite::Error>),
    Watched(WatchedGatewayTurn),
    BroadcastEvent {
        gateway: Arc<Gateway<Media>>,
        event: Option<GatewayFrame<VoiceOpcode>>,
    },
    BroadcastHeartbeat(Result<(), async_tungstenite::tungstenite::Error>),
}

/// One already-selected gateway action plus exclusive permission to execute
/// it. Selection is cancellation-safe: no frame lives here until websocket
/// `next()` returned. Once returned, callers must execute this turn outside any
/// competing `select!`; the permit preserves gateway ordering until completion.
pub(crate) struct GatewayTurn<'a> {
    permit: MutexGuard<'a, ()>,
    gateway: Arc<Gateway<General>>,
    action: GatewayAction,
}

enum MainGatewayAction {
    Event(Option<GatewayFrame<Opcode>>),
    Heartbeat(Result<(), async_tungstenite::tungstenite::Error>),
}

/// Main-gateway-only turn used by cache-first queries before capability
/// listeners own an `Arc<InnerDiscord<_>>`. Voice/media turns require that Arc
/// and remain exclusive to [`GatewayTurn`].
pub(crate) struct MainGatewayTurn<'a> {
    permit: MutexGuard<'a, ()>,
    gateway: Arc<Gateway<General>>,
    action: MainGatewayAction,
}

async fn poll_watched_gateway(
    key: StreamKey,
    slot: Arc<StreamGateway>,
    gateway: Arc<Gateway<Media>>,
) -> WatchedGatewayTurn {
    enum Outcome {
        Event(Option<GatewayFrame<VoiceOpcode>>),
        Heartbeat(Result<(), async_tungstenite::tungstenite::Error>),
    }

    let outcome = {
        let mut receiver_guard = gateway.websocket.receiver.lock().await;
        let mut events = pin!(receiver_guard.gateway_frames::<VoiceOpcode>());
        let mut heartbeat = pin!(gateway.heartbeat().fuse());
        select! {
            event = events.next() => Outcome::Event(event),
            result = heartbeat => Outcome::Heartbeat(result),
        }
    };
    match outcome {
        Outcome::Event(event) => WatchedGatewayTurn::Event {
            key,
            slot,
            gateway,
            event,
        },
        Outcome::Heartbeat(result) => WatchedGatewayTurn::Heartbeat {
            key,
            slot,
            gateway,
            result,
        },
    }
}

async fn poll_watched_gateways(
    streams: Vec<(StreamKey, Arc<StreamGateway>, Arc<Gateway<Media>>)>,
) -> WatchedGatewayTurn {
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

/// Borrowed gateway-session teardown dependencies. Holds references only;
/// lifecycle cleanup no longer expands `InnerDiscord`'s inherent API.
struct GatewaySessions<'a, T: UnitStruct> {
    discord: &'a InnerDiscord<T>,
}

impl<'a, T: UnitStruct> GatewaySessions<'a, T> {
    fn new(discord: &'a InnerDiscord<T>) -> Self {
        Self { discord }
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
            if remove_availability && let Some((user, channel)) = source {
                VideoControls::new(self.discord).emit_remove_participant_video(
                    user,
                    channel,
                    VideoBroadcastKind::Screen,
                );
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

    async fn end_gateway_session(&self, gateway: &Gateway<General>) {
        self.end_voice_session(gateway).await;
        self.discord.gateway.current.store(None);
    }
}

impl<T: UnitStruct> InnerDiscord<T> {
    pub(crate) async fn wait_for_main_gateway_turn(&self) -> Option<MainGatewayTurn<'_>> {
        if self.gateway.killed.load(Ordering::Acquire) {
            return None;
        }
        let permit = self.gateway.turn.lock().await;
        if self.gateway.killed.load(Ordering::Acquire) {
            return None;
        }
        let Some(gateway) = self.gateway.current.load_full() else {
            debug!("Gateway not connected (never started, disconnected, or killed)");
            return None;
        };
        let action = {
            let mut receiver_guard = gateway.websocket.receiver.lock().await;
            let mut receiver = pin!(receiver_guard.gateway_frames::<Opcode>());
            select! {
                event = receiver.next() => MainGatewayAction::Event(event),
                result = gateway.heartbeat().fuse() => MainGatewayAction::Heartbeat(result),
            }
        };
        Some(MainGatewayTurn {
            permit,
            gateway,
            action,
        })
    }

    pub(crate) async fn execute_main_gateway_turn(&self, turn: MainGatewayTurn<'_>) -> Option<()> {
        let MainGatewayTurn {
            permit,
            gateway,
            action,
        } = turn;
        let sessions = GatewaySessions::new(self);
        let outcome = match action {
            MainGatewayAction::Event(Some(event)) => {
                if let Err(err) = event.exec(self, &gateway).await {
                    warn!("Failed to execute gateway event: {err}");
                }
                trace!("Gateway cache event handled");
                Some(())
            }
            MainGatewayAction::Event(None) => {
                error!("Gateway connection closed; tearing it down");
                sessions.end_gateway_session(&gateway).await;
                None
            }
            MainGatewayAction::Heartbeat(Ok(())) => Some(()),
            MainGatewayAction::Heartbeat(Err(err)) => {
                error!("Gateway heartbeat failed: {err}; tearing down the connection");
                sessions.end_gateway_session(&gateway).await;
                None
            }
        };
        drop(permit);
        outcome
    }

    /// Wait until one gateway frame, heartbeat, or connection closure wins.
    ///
    /// This phase is safe to race against interface events and media work: if
    /// cancelled, either no frame was consumed or a completed action was
    /// returned to the caller. [`Self::execute_gateway_turn`] must run a
    /// returned turn outside the race so consumed frames reach completion.
    pub(crate) async fn wait_for_gateway_turn(&self) -> Option<GatewayTurn<'_>> {
        if self.gateway.killed.load(Ordering::Acquire) {
            return None;
        }
        let permit = self.gateway.turn.lock().await;
        if self.gateway.killed.load(Ordering::Acquire) {
            return None;
        }
        let Some(ref_gateway) = self.gateway.current.load_full() else {
            debug!("Gateway not connected (never started, disconnected, or killed)");
            return None;
        };

        let action = {
            // Main receiver also acts as root session liveness source. The
            // outer turn permit, rather than this receiver guard, remains held
            // while the selected action executes.
            let mut gateway_receiver_guard = ref_gateway.websocket.receiver.lock().await;
            let mut gateway_receiver = pin!(gateway_receiver_guard.gateway_frames::<Opcode>());

            // === Voice Gateway ===
            let voice_gateway = ref_gateway.voice.load_gateway();
            let voice_gateway_for_heartbeat = voice_gateway.clone();
            let mut voice_receiver_guard;
            let mut voice_receiver = pin!(match voice_gateway.as_ref() {
                Some(voice_gateway) => {
                    trace!("wait_for_gateway_turn: waiting for voice receiver lock");
                    voice_receiver_guard = voice_gateway.websocket.receiver.lock().await;
                    trace!("wait_for_gateway_turn: voice receiver lock acquired");
                    Either::Right(voice_receiver_guard.gateway_frames::<VoiceOpcode>())
                }
                None => Either::Left(stream::empty()),
            });

            // === Watched Go Live gateways ===
            let watched_gateways = ref_gateway
                .voice
                .watched_streams_for_poll()
                .into_iter()
                .filter_map(|(key, slot)| {
                    slot.load_gateway()
                        .map(|stream_gateway| (key, slot, stream_gateway))
                })
                .collect();
            let mut watched_gateway_turn = pin!(poll_watched_gateways(watched_gateways).fuse());

            // === Go Live broadcast gateway ===
            let broadcast_gateway = ref_gateway.voice.broadcast.load_gateway();
            let broadcast_gateway_for_heartbeat = broadcast_gateway.clone();
            let mut broadcast_receiver_guard;
            let mut broadcast_receiver = pin!(match broadcast_gateway.as_ref() {
                Some(broadcast_gateway) => {
                    broadcast_receiver_guard = broadcast_gateway.websocket.receiver.lock().await;
                    Either::Right(broadcast_receiver_guard.gateway_frames::<VoiceOpcode>())
                }
                None => Either::Left(stream::empty()),
            });

            select! {
                event = gateway_receiver.next() => GatewayAction::MainEvent(event),
                event = voice_receiver.next() => GatewayAction::VoiceEvent {
                    gateway: voice_gateway
                        .as_ref()
                        .expect("voice event stream is pending without a gateway")
                        .clone(),
                    event,
                },
                result = ref_gateway.heartbeat().fuse() => GatewayAction::MainHeartbeat(result),
                result = async {
                    match voice_gateway_for_heartbeat {
                        Some(voice_gateway) => voice_gateway.heartbeat().await,
                        None => future::pending().await,
                    }
                }.fuse() => GatewayAction::VoiceHeartbeat(result),
                watched = watched_gateway_turn => GatewayAction::Watched(watched),
                event = broadcast_receiver.next() => GatewayAction::BroadcastEvent {
                    gateway: broadcast_gateway
                        .as_ref()
                        .expect("broadcast event stream is pending without a gateway")
                        .clone(),
                    event,
                },
                result = async {
                    match broadcast_gateway_for_heartbeat {
                        Some(broadcast_gateway) => broadcast_gateway.heartbeat().await,
                        None => future::pending().await,
                    }
                }.fuse() => GatewayAction::BroadcastHeartbeat(result),
            }
        };

        Some(GatewayTurn {
            permit,
            gateway: ref_gateway,
            action,
        })
    }

    /// Execute one action returned by [`Self::wait_for_gateway_turn`] to
    /// completion before releasing its ordering permit.
    pub(crate) async fn execute_gateway_turn(
        self: &Arc<Self>,
        turn: GatewayTurn<'_>,
    ) -> Option<()> {
        let GatewayTurn {
            permit,
            gateway,
            action,
        } = turn;
        let sessions = GatewaySessions::new(self.as_ref());

        let outcome = match action {
            GatewayAction::MainEvent(Some(event)) => {
                if let Err(err) = event.exec(self.as_ref(), &gateway).await {
                    warn!("Failed to execute gateway event: {err}");
                }
                trace!("Gateway main event handled");
                Some(())
            }
            GatewayAction::MainEvent(None) => {
                error!("Gateway connection closed; tearing it down");
                sessions.end_gateway_session(&gateway).await;
                None
            }
            GatewayAction::MainHeartbeat(Ok(())) => Some(()),
            GatewayAction::MainHeartbeat(Err(err)) => {
                error!("Gateway heartbeat failed: {err}; tearing down the connection");
                sessions.end_gateway_session(&gateway).await;
                None
            }
            GatewayAction::VoiceEvent {
                gateway: voice_gateway,
                event: Some(event),
            } => {
                if let Err(err) = event.exec(self, &voice_gateway, ConnectionRole::Call).await {
                    warn!("Failed to execute voice gateway event: {err}");
                }
                trace!("Voice gateway event handled");
                Some(())
            }
            GatewayAction::VoiceEvent { event: None, .. } => {
                warn!("Voice gateway connection closed; tearing down the voice session");
                sessions.end_voice_session(&gateway).await;
                Some(())
            }
            GatewayAction::VoiceHeartbeat(Ok(())) => Some(()),
            GatewayAction::VoiceHeartbeat(Err(err)) => {
                error!("Voice gateway heartbeat failed: {err}; tearing down the voice session");
                sessions.end_voice_session(&gateway).await;
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
                            if let Err(err) = event
                                .exec(self, &stream_gateway, ConnectionRole::Stream)
                                .await
                            {
                                warn!("Failed to execute stream gateway event for {key}: {err}");
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
                if let Err(err) = event
                    .exec(self, &broadcast_gateway, ConnectionRole::Stream)
                    .await
                {
                    warn!("Failed to execute broadcast gateway event: {err}");
                }
                Some(())
            }
            GatewayAction::BroadcastEvent { event: None, .. } => {
                warn!("Broadcast gateway connection closed; ending the Go Live broadcast");
                sessions.end_broadcast(&gateway).await;
                Some(())
            }
            GatewayAction::BroadcastHeartbeat(Ok(())) => Some(()),
            GatewayAction::BroadcastHeartbeat(Err(err)) => {
                error!("Broadcast gateway heartbeat failed: {err}; ending the Go Live broadcast");
                sessions.end_broadcast(&gateway).await;
                Some(())
            }
        };

        drop(permit);
        outcome
    }

    /// Root gateway loss also ends its voice session before publishing the root
    /// disconnect. Thin facade needed by general gateway dispatcher.
    pub(crate) async fn end_gateway_session(&self, gateway: &Gateway<General>) {
        GatewaySessions::new(self)
            .end_gateway_session(gateway)
            .await;
    }
}
