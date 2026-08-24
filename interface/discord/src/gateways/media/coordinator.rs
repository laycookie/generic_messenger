//! Shared voice-media coordination and separate audio/video processors.
//!
//! <https://docs.discord.food/topics/voice-connections#speaking>
//! <https://docs.discord.food/topics/voice-connections#video>
use std::{
    collections::HashMap,
    error::Error,
    io,
    pin::pin,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use async_tungstenite::tungstenite::Message as WebsocketMessage;
use futures::{FutureExt as _, channel::oneshot, future, select};
use futures_timer::Delay;
use messenger_interface::interface::{
    AudioEvent, FrameConsumer, VideoBroadcastKind, VideoEvent, VideoFrame, VideoInput,
    VideoResolution,
};
use simple_video::{FrameScaler, VideoCodec};
use surf::http::convert::json;
use tracing::{debug, error, trace, warn};

use super::{
    Media, StreamKey, VoiceOpcode,
    audio::{AudioProcessor, AudioWork},
    connection::{RecvCodec, RtcpFeedback, UdpPacket, VOICE_FRAME_SAMPLES, VideoSendCodec},
    gateway::MediaConnection,
};
use crate::{
    DiscordRuntimeState, LocalMediaState,
    adapter::video::VideoControls,
    gateways::{Gateway, GatewaySupervisor, GatewayTurn},
};

/// Build voice opcode 5 Speaking.
/// <https://docs.discord.food/topics/voice-connections#speaking>
fn speaking_payload(ssrc: u32, speaking: bool) -> WebsocketMessage {
    json!({
        "op": VoiceOpcode::Speaking as u8,
        "d": {
            "speaking": speaking as u8,
            "delay": 0,
            "ssrc": ssrc,
        }
    })
    .to_string()
    .into()
}

const MAX_MICROPHONE_RETRIES: u8 = 3;
/// How long the mic may stay silent before we announce we stopped speaking.
const STOP_SPEAKING: Duration = Duration::from_secs(2);
/// Idle re-verification cadence. With no audio in either direction the loop
/// still wakes this often to confirm the call is alive, so a disconnect during
/// silence tears the session down promptly without any cross-task signal.
const HEARTBEAT: Duration = Duration::from_secs(1);
const MAX_VIDEO_FRAMES_PER_SECOND: u32 = 30;
const FULL_VIDEO_QUALITY: u8 = 100;
const SCREEN_LOW_QUALITY_MAX: u8 = 33;
const SCREEN_MEDIUM_QUALITY_MAX: u8 = 66;
const SCREEN_LOW_TARGET_QUALITY: u8 = 25;
const SCREEN_MEDIUM_TARGET_QUALITY: u8 = 50;
const SCREEN_LOW_MAX_FRAMES_PER_SECOND: u32 = 10;
const SCREEN_MEDIUM_MAX_FRAMES_PER_SECOND: u32 = 20;
const SCREEN_LOW_BITRATE_BPS: u32 = 600_000;
const SCREEN_MEDIUM_BITRATE_BPS: u32 = 1_200_000;
const MIN_VIDEO_BITRATE_BPS: u32 = 250_000;
const MAX_VIDEO_BITRATE_BPS: u32 = 5_000_000;
const MAX_PENDING_VIDEO_RETRANSMITS: usize = 256;
/// Pause between the first opcode-12 Video announce and the first video RTP
/// packet, so the announce is processed before media arrives on that SSRC.
const VIDEO_ANNOUNCE_SETTLE: Duration = Duration::from_millis(200);

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn video_keyframe_due(last_keyframe: Option<Instant>, now: Instant, interval: Duration) -> bool {
    last_keyframe.is_none_or(|last| now.saturating_duration_since(last) >= interval)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VideoSendProfile {
    quality: u8,
    frames_per_second: u32,
    bitrate_bps: u32,
    resolution: VideoResolution,
}

impl VideoSendProfile {
    fn for_stream(
        kind: VideoBroadcastKind,
        source_fps: u32,
        resolution: VideoResolution,
        bitrate_bps: u32,
        quality: u8,
    ) -> Option<Self> {
        let source_fps = source_fps.clamp(1, MAX_VIDEO_FRAMES_PER_SECOND);
        let bitrate_bps = bitrate_bps.clamp(MIN_VIDEO_BITRATE_BPS, MAX_VIDEO_BITRATE_BPS);
        let quality = quality.min(FULL_VIDEO_QUALITY);
        match kind {
            VideoBroadcastKind::Camera => Some(Self {
                quality: FULL_VIDEO_QUALITY,
                frames_per_second: source_fps,
                bitrate_bps,
                resolution,
            }),
            VideoBroadcastKind::Screen => match quality {
                0 => None,
                1..=SCREEN_LOW_QUALITY_MAX => Some(Self {
                    quality: SCREEN_LOW_TARGET_QUALITY,
                    frames_per_second: source_fps.min(SCREEN_LOW_MAX_FRAMES_PER_SECOND),
                    bitrate_bps: bitrate_bps.min(SCREEN_LOW_BITRATE_BPS),
                    resolution,
                }),
                34..=SCREEN_MEDIUM_QUALITY_MAX => Some(Self {
                    quality: SCREEN_MEDIUM_TARGET_QUALITY,
                    frames_per_second: source_fps.min(SCREEN_MEDIUM_MAX_FRAMES_PER_SECOND),
                    bitrate_bps: bitrate_bps.min(SCREEN_MEDIUM_BITRATE_BPS),
                    resolution,
                }),
                _ => Some(Self {
                    quality: FULL_VIDEO_QUALITY,
                    frames_per_second: source_fps,
                    bitrate_bps,
                    resolution,
                }),
            },
        }
    }
}

enum VideoWork {
    /// Captured frame, or `None` when app input ended.
    Send(VideoBroadcastKind, Option<VideoFrame>),
    /// CPU encode and async RTP send completed. Encoder/scaler ownership is
    /// returned to persistent stream state before another frame may start.
    JobComplete(VideoBroadcastKind, VideoJobCompletion),
    /// Receiver sent RTCP PLI/FIR. Force next H264 frame to be an IDR.
    RequestVideoKeyframe(VideoBroadcastKind),
    /// Receiver reported lost primary-video RTP packets. Resend through RTX.
    RetransmitVideo(VideoBroadcastKind, Vec<u16>),
    /// RTCP receiver report updates adaptive send tier.
    VideoLossReport(VideoBroadcastKind, u8),
    Yield,
}

/// Owned result from one UDP receive. Audio is copied here because
/// [`RecvCodec`] lends its decode buffer only until the next receive.
pub(crate) enum PolledMedia {
    Audio {
        source: MediaSource,
        ssrc: u32,
        samples: Vec<i16>,
    },
    Video {
        source: MediaSource,
        ssrc: u32,
        frame: VideoFrame,
        kind: VideoBroadcastKind,
    },
    VideoFeedback {
        source: MediaSource,
        kind: VideoBroadcastKind,
        feedback: RtcpFeedback,
    },
    Consumed,
}

#[derive(Clone, Copy)]
pub(crate) enum MediaSourceKind {
    Call,
    WatchedStream,
    Broadcast,
}

/// Identifies one exact media connection at receive time. Slot generation and
/// connection pointer checks discard data from disconnected or replaced slots.
#[derive(Clone)]
pub(crate) struct MediaSource {
    pub(crate) kind: MediaSourceKind,
    pub(crate) generation: u64,
    pub(crate) stream_key: Option<StreamKey>,
    pub(crate) connection: Arc<MediaConnection>,
}

pub(crate) struct WatchedMedia<'a> {
    pub(crate) gateway: &'a Arc<Gateway<Media>>,
    pub(crate) receiver: &'a mut MediaReceiver,
    pub(crate) source: MediaSource,
}

enum MediaTurn {
    Packet(PolledMedia),
    Audio(AudioWork),
    Video(VideoWork),
    Gateway(Option<GatewayTurn>),
    Heartbeat,
}

/// Result of one media-coordinator selection. Gateway turns remain separate so
/// capability streams can stop racing their execution against newly emitted
/// interface events after a websocket frame has already been consumed.
pub(crate) enum MediaDrive {
    Complete(Option<()>),
    Gateway(GatewayTurn),
}

#[derive(Default)]
struct VideoProcessor {
    camera: OutgoingVideo,
    screen: OutgoingVideo,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct VideoSendOutcome {
    emitted: bool,
    keyframe: bool,
}

type VideoSendResult = Result<VideoSendOutcome, Box<dyn Error + Send + Sync>>;

/// Completion from one bounded outgoing-video job. Resources always return,
/// including encode/transport errors, so cancellation between coordinator
/// turns cannot duplicate or lose encoder state.
struct VideoSendCompletion {
    codec: Option<VideoSendCodec>,
    scaler: FrameScaler,
    result: VideoSendResult,
    submitted_frame_id: u64,
    emitted_frame_id: u64,
    forced_keyframe: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyframeCompletionAction {
    Complete,
    Pending,
    Retry,
}

fn keyframe_completion_action(
    codec: VideoCodec,
    outcome: VideoSendOutcome,
) -> KeyframeCompletionAction {
    if outcome.keyframe {
        KeyframeCompletionAction::Complete
    } else if codec == VideoCodec::Av1 {
        // rav1e emits an older input while a newly forced frame remains in
        // its lookahead queue. Reissuing here forces every intervening input
        // and later creates a burst of consecutive keyframes.
        KeyframeCompletionAction::Pending
    } else {
        KeyframeCompletionAction::Retry
    }
}

enum VideoJobCompletion {
    Send(VideoSendCompletion),
    Retransmit {
        requested: usize,
        result: Result<usize, Box<dyn Error + Send + Sync>>,
        elapsed_us: u64,
    },
}

/// Persistent state for one call connection generation.
struct MediaSession {
    call_identity: Arc<MediaConnection>,
    recv: RecvCodec,
    audio: AudioProcessor,
    video: VideoProcessor,
    /// Independent H264 decode state for every watched Go Live connection.
    stream_recvs: HashMap<StreamKey, MediaReceiver>,
    stream_poll_cursor: usize,
    /// Receive state for RTCP feedback arriving on our Go Live broadcast
    /// connection. No media is expected there, but PLI/FIR must wake the
    /// screen encoder just like feedback on the call wakes the camera.
    broadcast_recv: Option<MediaReceiver>,
}

/// Runtime owned by [`super::MediaGateway`], shared by audio and video streams.
#[derive(Default)]
pub(crate) struct MediaRuntime {
    session: Option<Box<MediaSession>>,
}

/// Decode/receive state tied to one auxiliary media connection. Watched Go
/// Live and own broadcast feedback share this lifecycle; replacing either
/// connection must replace its codec state too.
pub(crate) struct MediaReceiver {
    connection: Arc<MediaConnection>,
    codec: RecvCodec,
}

impl MediaReceiver {
    fn is_bound_to(&self, connection: &Arc<MediaConnection>) -> bool {
        Arc::ptr_eq(&self.connection, connection)
    }

    /// Build a receiver for this connection generation. Returns `None` if the
    /// receive codec fails to initialize.
    fn new(connection: &Arc<MediaConnection>, label: &str) -> Option<Self> {
        match RecvCodec::new() {
            Ok(codec) => Some(Self {
                connection: connection.clone(),
                codec,
            }),
            Err(err) => {
                error!("Failed to initialize {label} receive codec: {err}");
                None
            }
        }
    }

    pub(crate) async fn recv<'a>(
        &'a mut self,
        gateway: &Gateway<Media>,
    ) -> Result<UdpPacket<'a>, String> {
        if !self.connection.has_description() {
            return future::pending().await;
        }
        self.connection
            .recv_transport(
                &gateway.dave_session,
                &self.connection.ssrc_to_user_id,
                &self.connection.video_ssrc_to_user_id,
                &self.connection.rtx_to_video_ssrc,
            )
            .recv(&mut self.codec)
            .await
            .map_err(|err| err.to_string())
    }
}

/// Owned encode/announce state for one outgoing video stream. Camera and Go
/// Live screen share run the identical lazy acquire → announce → send →
/// teardown cycle (`MediaCoordinator::video_lifecycle` and
/// `MediaCoordinator::pump_video_send`); they differ only in which connection
/// carries them and which app input feeds them.
struct OutgoingVideo {
    /// Connection this state belongs to. Reconnects require a fresh opcode-12
    /// announce and encoder state even when desired stream remains enabled.
    connection: Option<Arc<MediaConnection>>,
    /// Server-selected encoder, built on first frame after stream turns on.
    codec: Option<VideoSendCodec>,
    /// Desired profile differs from codec currently in `codec`. Rebuild and
    /// retire old native state inside blocking job, never shared media turn.
    rebuild_codec: bool,
    /// Reuses software-scale sampling maps and consumed source allocations.
    scaler: FrameScaler,
    /// At most one encode/send or retransmit job may consume CPU/network per
    /// stream. Keeping task here makes losing another `select!` branch
    /// cancellation-safe; newest capture remains in latest-frame input.
    send_task: Option<smol::Task<VideoJobCompletion>>,
    /// NACK sequence numbers coalesced while encode/send or prior RTX owns the
    /// one bounded stream job. Duplicates and excess requests are discarded.
    pending_retransmits: Vec<u16>,
    /// Frame source from the app, obtained via `AddVideoInput`.
    input: Option<FrameConsumer>,
    /// Capture cadence supplied by app input.
    frames_per_second: u32,
    /// UI-selected capture and encode bound.
    resolution: Option<VideoResolution>,
    /// UI-selected encoder target before network-quality caps.
    bitrate_bps: u32,
    /// Effective encoder/network tier. Screen changes this in response to
    /// opcode-15 sink wants and RTCP loss.
    profile: Option<VideoSendProfile>,
    /// Congestion estimate independent of viewer/SFU requested quality.
    network_quality: u8,
    /// Consecutive clean reports required before cautiously increasing rate.
    clean_network_reports: u8,
    /// Sink requested quality zero. Input remains attached but is not polled,
    /// so latest-frame replacement resumes immediately without queue growth.
    sink_paused: bool,
    /// In-flight `AddVideoInput` request awaiting the app's reply.
    input_recv: Option<oneshot::Receiver<VideoInput>>,
    /// Whether the opcode-12 Video announce for the current broadcast is out
    /// (required before any video RTP).
    announced: bool,
    /// Active announce needs refreshed bitrate/FPS/resolution after tier move.
    announce_dirty: bool,
    /// Encoder received an IDR request, but has not emitted that frame yet.
    keyframe_armed: bool,
    /// PLI/FIR requested an IDR which has not yet been emitted.
    keyframe_requested: bool,
    /// MLS commit/welcome activated a new davey sender ratchet; outgoing video
    /// stays paused until Discord executes that transition.
    dave_transition_blocked: bool,
    /// Last successfully emitted forced IDR. Periodic fallback uses this
    /// instead of request time so skipped or failed frames do not count.
    last_keyframe: Option<Instant>,
    /// Earliest time another frame may enter isolated encode/send work.
    next_frame_at: Option<Instant>,
    /// Monotonic trace identity scoped to this outgoing stream generation.
    next_frame_id: u64,
}

impl Default for OutgoingVideo {
    fn default() -> Self {
        Self {
            connection: None,
            codec: None,
            rebuild_codec: false,
            scaler: FrameScaler::new(),
            send_task: None,
            pending_retransmits: Vec::new(),
            input: None,
            frames_per_second: 0,
            resolution: None,
            bitrate_bps: MIN_VIDEO_BITRATE_BPS,
            profile: None,
            network_quality: 100,
            clean_network_reports: 0,
            sink_paused: false,
            input_recv: None,
            announced: false,
            announce_dirty: false,
            keyframe_armed: false,
            keyframe_requested: false,
            dave_transition_blocked: false,
            last_keyframe: None,
            next_frame_at: None,
            next_frame_id: 0,
        }
    }
}

impl OutgoingVideo {
    fn belongs_to(&self, connection: &Arc<MediaConnection>) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, connection))
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn queue_retransmits(&mut self, sequences: impl IntoIterator<Item = u16>) {
        for sequence in sequences {
            if self.pending_retransmits.len() >= MAX_PENDING_VIDEO_RETRANSMITS {
                break;
            }
            if !self.pending_retransmits.contains(&sequence) {
                self.pending_retransmits.push(sequence);
            }
        }
    }

    fn observe_loss(&mut self, fraction_lost: u8) {
        let old = self.network_quality;
        match fraction_lost {
            // >10% interval loss: leave high tier immediately.
            26.. => {
                self.network_quality = self.network_quality.saturating_sub(34).max(25);
                self.clean_network_reports = 0;
            }
            // >3% interval loss: one smaller step.
            8..=25 => {
                self.network_quality = self.network_quality.saturating_sub(17).max(25);
                self.clean_network_reports = 0;
            }
            // <1%: require two clean reports before increasing.
            0..=2 => {
                self.clean_network_reports = self.clean_network_reports.saturating_add(1);
                if self.clean_network_reports >= 2 {
                    self.network_quality = self.network_quality.saturating_add(17).min(100);
                    self.clean_network_reports = 0;
                }
            }
            _ => self.clean_network_reports = 0,
        }
        if old != self.network_quality {
            debug!(
                "Video network quality changed: {old} -> {} (fraction_lost={fraction_lost}/256)",
                self.network_quality
            );
        }
    }
}

/// Await the next captured frame from an outgoing stream's input, or park
/// forever when the stream has no input yet — the idle form of both video send
/// arms in the coordinator `select!` (`None` means that arm is absent).
async fn next_frame_or_idle(
    input: Option<&mut FrameConsumer>,
    next_frame_at: Option<Instant>,
    kind: VideoBroadcastKind,
) -> Option<VideoFrame> {
    match input {
        Some(input) => {
            let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
            let total_started = timing_enabled.then(Instant::now);
            let mut pacing_wait_us = 0;
            if let Some(wait) =
                next_frame_at.and_then(|ready| ready.checked_duration_since(Instant::now()))
            {
                pacing_wait_us = duration_micros(wait);
                Delay::new(wait).await;
            }
            let input_started = timing_enabled.then(Instant::now);
            let frame = input.pop().await;
            if let Some(total_started) = total_started {
                let frame_timing = frame.as_ref().and_then(VideoFrame::timing);
                tracing::trace!(
                    target: "video_timing",
                    stage = "outgoing.input_wait",
                    ?kind,
                    frame_id = frame_timing.map_or(0, |timing| timing.id()),
                    pipeline_elapsed_us = frame_timing
                        .map_or(0, |timing| timing.elapsed_micros()),
                    pacing_wait_us,
                    input_wait_us = input_started.map(elapsed_micros).unwrap_or_default(),
                    total_us = elapsed_micros(total_started),
                    input_ended = frame.is_none(),
                    "video stage complete"
                );
            }
            frame
        }
        None => future::pending().await,
    }
}

/// Await a stored video job, or park forever when stream has no work in flight.
/// Task stays borrowed in [`OutgoingVideo`] while competing select arms run.
async fn task_or_idle<T>(task: Option<&mut smol::Task<T>>) -> T {
    match task {
        Some(task) => task.await,
        None => future::pending().await,
    }
}

/// Borrowed dependencies for one media-coordination turn. References only;
/// no backend state, allocation, or synchronization is duplicated.
struct MediaCoordinator<'a> {
    discord: &'a DiscordRuntimeState,
    gateway: &'a GatewaySupervisor,
    media: &'a LocalMediaState,
}

impl<'a> MediaCoordinator<'a> {
    fn new(discord: &'a DiscordRuntimeState) -> Self {
        Self {
            discord,
            gateway: &discord.gateway,
            media: &discord.media,
        }
    }

    /// Drive one shared media turn. Runtime lock is released after one selected
    /// UDP packet, output item, gateway event, or heartbeat.
    async fn drive_media(&self) -> MediaDrive {
        let Some(gateway) = self.gateway.current.load_full() else {
            return MediaDrive::Complete(None);
        };
        let mut runtime = gateway.voice.media.lock().await;
        let call = gateway
            .voice
            .load_gateway()
            .and_then(|gateway| gateway.connection.load_full());

        let stale = runtime.session.as_ref().is_some_and(|session| {
            call.as_ref()
                .is_none_or(|call| !Arc::ptr_eq(call, &session.call_identity))
        });
        if stale {
            self.release_session(&mut runtime).await;
        }

        if runtime.session.is_none()
            && let Some(call) = call.as_ref()
        {
            let (send, recv) = match call.new_codecs() {
                Ok(codecs) => codecs,
                Err(err) => {
                    error!("Failed to initialize media codecs: {err}");
                    return MediaDrive::Complete(None);
                }
            };
            let microphone = self.media.audio_manager.lock().await.microphone.take();
            runtime.session = Some(Box::new(MediaSession {
                call_identity: call.clone(),
                recv,
                audio: AudioProcessor {
                    send: Some(send),
                    microphone,
                    frame: [0.0; VOICE_FRAME_SAMPLES],
                    frame_filled: 0,
                    stop_speaking_delay: None,
                    waiting_for_dave: false,
                },
                video: VideoProcessor::default(),
                stream_recvs: HashMap::new(),
                stream_poll_cursor: 0,
                broadcast_recv: None,
            }));
        }

        let Some(session) = runtime.session.as_deref_mut() else {
            let mut gateway_turn = pin!(self.discord.wait_for_gateway_turn().fuse());
            let mut heartbeat = pin!(Delay::new(HEARTBEAT).fuse());
            let turn = select! {
                turn = gateway_turn => MediaTurn::Gateway(turn),
                _ = heartbeat => MediaTurn::Heartbeat,
            };
            return match turn {
                MediaTurn::Gateway(Some(turn)) => MediaDrive::Gateway(turn),
                MediaTurn::Gateway(None) => MediaDrive::Complete(None),
                MediaTurn::Heartbeat => MediaDrive::Complete(Some(())),
                MediaTurn::Packet(_) | MediaTurn::Audio(_) | MediaTurn::Video(_) => {
                    unreachable!("idle media selection cannot produce media work")
                }
            };
        };

        if self.prepare_microphone(&mut session.audio).await {
            return MediaDrive::Complete(Some(()));
        }
        let Some(live) = self.load_live_call_connection(&session.call_identity) else {
            self.release_session(&mut runtime).await;
            return MediaDrive::Complete(Some(()));
        };
        let Some(live_gateway) = self.load_gateway_for(&live, VideoBroadcastKind::Camera) else {
            self.release_session(&mut runtime).await;
            return MediaDrive::Complete(Some(()));
        };
        let broadcast = self.load_broadcast_connection();
        let broadcast_gateway = broadcast
            .as_ref()
            .and_then(|connection| self.load_gateway_for(connection, VideoBroadcastKind::Screen));
        let watched_streams = gateway
            .voice
            .watched_streams()
            .into_iter()
            .filter_map(|(key, slot)| {
                let stream_gateway = slot.load_gateway()?;
                let connection = stream_gateway.connection.load_full()?;
                Some((key, slot.generation(), stream_gateway, connection))
            })
            .collect::<Vec<_>>();
        session.stream_recvs.retain(|key, _| {
            watched_streams
                .iter()
                .any(|(active_key, ..)| active_key == key)
        });
        for (key, _, _, connection) in &watched_streams {
            let needs_receiver = session
                .stream_recvs
                .get(key)
                .is_none_or(|receiver| !receiver.is_bound_to(connection));
            if needs_receiver {
                match MediaReceiver::new(connection, "stream") {
                    Some(receiver) => {
                        session.stream_recvs.insert(key.clone(), receiver);
                    }
                    None => {
                        session.stream_recvs.remove(key);
                    }
                }
            }
        }
        Self::bind_receiver(&mut session.broadcast_recv, broadcast.as_ref(), "broadcast");
        let stream_poll_start = if session.stream_recvs.is_empty() {
            0
        } else {
            let start = session.stream_poll_cursor % session.stream_recvs.len();
            session.stream_poll_cursor = session.stream_poll_cursor.wrapping_add(1);
            start
        };

        let MediaSession {
            recv,
            audio,
            video,
            stream_recvs,
            broadcast_recv,
            ..
        } = session;
        let mut watched_media = stream_recvs
            .iter_mut()
            .filter_map(|(key, receiver)| {
                let (key, generation, stream_gateway, connection) = watched_streams
                    .iter()
                    .find(|(active_key, ..)| active_key == key)?;
                Some(WatchedMedia {
                    gateway: stream_gateway,
                    receiver,
                    source: MediaSource {
                        kind: MediaSourceKind::WatchedStream,
                        generation: *generation,
                        stream_key: Some(key.clone()),
                        connection: connection.clone(),
                    },
                })
            })
            .collect::<Vec<_>>();
        if !watched_media.is_empty() {
            let watched_count = watched_media.len();
            watched_media.rotate_left(stream_poll_start % watched_count);
        }
        let turn = {
            let mut media = pin!(
                gateway
                    .voice
                    .poll_media(
                        &live,
                        &live_gateway,
                        recv,
                        watched_media,
                        broadcast.as_ref().zip(broadcast_gateway.as_ref()),
                        broadcast_recv.as_mut(),
                        MediaSource {
                            kind: MediaSourceKind::Call,
                            generation: gateway.voice.call_generation(),
                            stream_key: None,
                            connection: live.clone(),
                        },
                        broadcast.as_ref().map(|connection| MediaSource {
                            kind: MediaSourceKind::Broadcast,
                            generation: gateway.voice.broadcast.generation(),
                            stream_key: None,
                            connection: connection.clone(),
                        }),
                    )
                    .fuse()
            );
            let mut audio_output = pin!(audio.poll().fuse());
            let mut video_output = pin!(
                self.poll_video_output(&live, broadcast.as_ref(), video)
                    .fuse()
            );
            let mut gateway_turn = pin!(self.discord.wait_for_gateway_turn().fuse());
            let mut heartbeat = pin!(Delay::new(HEARTBEAT).fuse());
            select! {
                packet = media => MediaTurn::Packet(packet),
                work = audio_output => MediaTurn::Audio(work),
                work = video_output => MediaTurn::Video(work),
                turn = gateway_turn => MediaTurn::Gateway(turn),
                _ = heartbeat => MediaTurn::Heartbeat,
            }
        };
        match turn {
            MediaTurn::Packet(PolledMedia::Audio {
                source,
                ssrc,
                samples,
            }) => {
                if let Some(connection) = self.resolve_media_source(source) {
                    self.process_audio(
                        &connection,
                        audio,
                        AudioWork::Receive { ssrc, samples },
                        video.camera.announced,
                    )
                    .await;
                }
            }
            MediaTurn::Packet(PolledMedia::Video {
                source,
                ssrc,
                frame,
                kind,
            }) => {
                if let Some(connection) = self.resolve_media_source(source) {
                    connection.dispatch_incoming_video(self.discord, ssrc, frame, kind);
                }
            }
            MediaTurn::Packet(PolledMedia::VideoFeedback {
                source,
                kind,
                feedback,
            }) => {
                if let Some(connection) = self.resolve_media_source(source)
                    && let Some(work) = Self::classify_video_feedback(&connection, kind, feedback)
                {
                    self.process_video(&live, video, work).await;
                }
            }
            MediaTurn::Packet(PolledMedia::Consumed) | MediaTurn::Heartbeat => {}
            MediaTurn::Audio(work) => {
                self.process_audio(&live, audio, work, video.camera.announced)
                    .await;
            }
            MediaTurn::Video(work) => self.process_video(&live, video, work).await,
            MediaTurn::Gateway(turn) => {
                let Some(turn) = turn else {
                    self.release_session(&mut runtime).await;
                    return MediaDrive::Complete(None);
                };
                return MediaDrive::Gateway(turn);
            }
        }
        MediaDrive::Complete(Some(()))
    }

    async fn release_session(&self, runtime: &mut MediaRuntime) {
        if runtime.session.is_none() {
            return;
        }
        // Keep session intact if this future is cancelled while waiting for
        // external audio state, otherwise its microphone would be dropped.
        let mut manager = self.media.audio_manager.lock().await;
        let mut session = runtime.session.take().expect("media session checked above");
        if let Some(microphone) = session.audio.microphone.take() {
            // A replacement gateway may already have acquired a newer input.
            // Never overwrite it while an old runtime tears itself down.
            if manager.microphone.is_none() {
                manager.microphone = Some(microphone);
            }
        }
    }

    /// Poll external microphone API state without awaiting app response.
    /// Returns true when an event was queued and this coordinator turn should end.
    async fn prepare_microphone(&self, audio: &mut AudioProcessor) -> bool {
        if audio.microphone.is_some() {
            return false;
        }
        let mut manager = self.media.audio_manager.lock().await;
        if let Some(microphone) = manager.microphone.take() {
            audio.microphone = Some(microphone);
            manager.microphone_retries = 0;
            return false;
        }
        if manager.microphone_retries >= MAX_MICROPHONE_RETRIES {
            return false;
        }
        let Some(mut receiver) = manager.microphone_recv.take() else {
            let (sender, receiver) = oneshot::channel();
            if let Some(gateway) = self.gateway.current.load().as_ref()
                && gateway
                    .voice
                    .submit_audio(AudioEvent::AddAudioInput(sender))
            {
                manager.microphone_recv = Some(receiver);
                return true;
            }
            return false;
        };
        match receiver.try_recv() {
            Ok(Some(microphone)) => {
                audio.microphone = Some(microphone);
                manager.microphone_retries = 0;
            }
            Ok(None) => manager.microphone_recv = Some(receiver),
            Err(_) => {
                manager.microphone_retries += 1;
                if manager.microphone_retries >= MAX_MICROPHONE_RETRIES {
                    error!(
                        "Microphone acquisition failed after {MAX_MICROPHONE_RETRIES} retries; continuing receive-only"
                    );
                } else {
                    warn!(
                        "Microphone input sender was dropped (attempt {}/{MAX_MICROPHONE_RETRIES})",
                        manager.microphone_retries
                    );
                }
            }
        }
        false
    }

    fn bind_receiver(
        receiver: &mut Option<MediaReceiver>,
        connection: Option<&Arc<MediaConnection>>,
        label: &str,
    ) {
        match connection {
            Some(connection)
                if !receiver
                    .as_ref()
                    .is_some_and(|receiver| receiver.is_bound_to(connection)) =>
            {
                *receiver = MediaReceiver::new(connection, label);
            }
            None => *receiver = None,
            _ => {}
        }
    }

    async fn poll_video_output(
        &self,
        live: &Arc<MediaConnection>,
        broadcast: Option<&Arc<MediaConnection>>,
        video: &mut VideoProcessor,
    ) -> VideoWork {
        let camera_on = self
            .gateway
            .current
            .load()
            .as_ref()
            .is_some_and(|gateway| gateway.voice.camera_send_enabled());
        let requested_camera =
            camera_on && video.camera.input.is_none() && video.camera.input_recv.is_none();
        self.video_lifecycle(
            camera_on,
            Some(live),
            &mut video.camera,
            VideoBroadcastKind::Camera,
        )
        .await;
        if requested_camera {
            return VideoWork::Yield;
        }
        let screen_on = self
            .gateway
            .current
            .load()
            .as_ref()
            .is_some_and(|gateway| gateway.voice.broadcast.send_enabled());
        let requested_screen = screen_on
            && broadcast.is_some()
            && video.screen.input.is_none()
            && video.screen.input_recv.is_none();
        self.video_lifecycle(
            screen_on,
            broadcast,
            &mut video.screen,
            VideoBroadcastKind::Screen,
        )
        .await;
        if requested_screen {
            return VideoWork::Yield;
        }

        if video.camera.send_task.is_none() && !video.camera.pending_retransmits.is_empty() {
            return VideoWork::RetransmitVideo(
                VideoBroadcastKind::Camera,
                std::mem::take(&mut video.camera.pending_retransmits),
            );
        }
        if video.screen.send_task.is_none() && !video.screen.pending_retransmits.is_empty() {
            return VideoWork::RetransmitVideo(
                VideoBroadcastKind::Screen,
                std::mem::take(&mut video.screen.pending_retransmits),
            );
        }

        let camera_at = video.camera.next_frame_at;
        let screen_at = video.screen.next_frame_at;
        let camera_busy = video.camera.send_task.is_some();
        let screen_busy = video.screen.send_task.is_some();
        let camera = if camera_busy {
            None
        } else {
            video.camera.input.as_mut()
        };
        let screen = if video.screen.sink_paused || screen_busy {
            None
        } else {
            video.screen.input.as_mut()
        };
        let mut camera_frame =
            pin!(next_frame_or_idle(camera, camera_at, VideoBroadcastKind::Camera).fuse());
        let mut screen_frame =
            pin!(next_frame_or_idle(screen, screen_at, VideoBroadcastKind::Screen).fuse());
        let mut camera_send = pin!(task_or_idle(video.camera.send_task.as_mut()).fuse());
        let mut screen_send = pin!(task_or_idle(video.screen.send_task.as_mut()).fuse());
        select! {
            result = camera_send => VideoWork::JobComplete(VideoBroadcastKind::Camera, result),
            result = screen_send => VideoWork::JobComplete(VideoBroadcastKind::Screen, result),
            frame = camera_frame => VideoWork::Send(VideoBroadcastKind::Camera, frame),
            frame = screen_frame => VideoWork::Send(VideoBroadcastKind::Screen, frame),
        }
    }

    fn classify_video_feedback(
        connection: &Arc<MediaConnection>,
        kind: VideoBroadcastKind,
        feedback: RtcpFeedback,
    ) -> Option<VideoWork> {
        let video_ssrc = connection.video_ssrc();
        match feedback {
            RtcpFeedback::RequestKeyframe { media_ssrc }
                if media_ssrc == 0 || media_ssrc == video_ssrc =>
            {
                Some(VideoWork::RequestVideoKeyframe(kind))
            }
            RtcpFeedback::Retransmit {
                media_ssrc,
                sequences,
            } if media_ssrc == 0 || media_ssrc == video_ssrc => {
                Some(VideoWork::RetransmitVideo(kind, sequences))
            }
            RtcpFeedback::ReceiverReport { reports } => reports
                .into_iter()
                .find(|report| report.media_ssrc == 0 || report.media_ssrc == video_ssrc)
                .map(|report| VideoWork::VideoLossReport(kind, report.fraction_lost)),
            _ => None,
        }
    }

    async fn process_audio(
        &self,
        live: &Arc<MediaConnection>,
        audio: &mut AudioProcessor,
        work: AudioWork,
        camera_announced: bool,
    ) {
        match work {
            AudioWork::Receive { ssrc, samples } => {
                if self.load_live_call_connection(live).is_none() {
                    return;
                }
                let gateway = self.gateway.current.load();
                let Some(gateway) = gateway.as_ref() else {
                    return;
                };
                live.dispatch_incoming_audio(
                    |event| {
                        gateway.voice.submit_audio(event);
                    },
                    ssrc,
                    &samples,
                );
            }
            AudioWork::SendMic => {
                let Some(live_gateway) = self.load_gateway_for(live, VideoBroadcastKind::Camera)
                else {
                    return;
                };
                let send_t = live.send_transport(&live_gateway.dave_session);
                if !live_gateway.dave_pending_transitions.is_empty() || !send_t.is_dave_ready() {
                    if !audio.waiting_for_dave {
                        debug!("Deferring voice audio until DAVE send state is ready");
                        audio.waiting_for_dave = true;
                    }
                    audio.frame_filled = 0;
                    return;
                }
                if let Err(err) = self
                    .update_speaking(live, VideoBroadcastKind::Camera, true)
                    .await
                {
                    warn!("Failed to send speaking update: {err}");
                }
                if let Some(codec) = audio.send.as_mut() {
                    match send_t.send_frame(codec, &audio.frame).await {
                        Ok(true) => {
                            if std::mem::take(&mut audio.waiting_for_dave) {
                                debug!("DAVE send state ready; resuming voice audio");
                            }
                            audio.stop_speaking_delay = Some(Delay::new(STOP_SPEAKING));
                        }
                        Ok(false) => {
                            if !audio.waiting_for_dave {
                                debug!("Deferring voice audio until DAVE send state is ready");
                                audio.waiting_for_dave = true;
                            }
                        }
                        Err(err) => warn!("Failed to send voice audio frame: {err}"),
                    }
                }
                audio.frame_filled = 0;
            }
            AudioWork::StopSpeaking => {
                // Camera and microphone share this voice connection. Once
                // camera startup has registered media with Speaking, clearing
                // that connection-level state on mic silence can make the SFU
                // stop forwarding camera RTP and viewers time out (2012).
                // Camera teardown clears it; an active mic restores it on its
                // next 20 ms frame.
                if !camera_announced
                    && let Err(err) = self
                        .update_speaking(live, VideoBroadcastKind::Camera, false)
                        .await
                {
                    warn!("Failed to send speaking update: {err}");
                }
                audio.stop_speaking_delay = None;
            }
            AudioWork::MicrophoneEnded => {
                debug!("Microphone capture ended; requesting replacement input");
                audio.microphone = None;
                audio.frame_filled = 0;
                audio.stop_speaking_delay = None;
                audio.waiting_for_dave = false;
                if !camera_announced
                    && let Err(err) = self
                        .update_speaking(live, VideoBroadcastKind::Camera, false)
                        .await
                {
                    warn!("Failed to clear speaking after microphone ended: {err}");
                }
            }
        }
    }

    async fn process_video(
        &self,
        live: &Arc<MediaConnection>,
        video: &mut VideoProcessor,
        work: VideoWork,
    ) {
        match work {
            VideoWork::Send(VideoBroadcastKind::Camera, frame) => {
                // Camera rides the call connection. The shared mic silence
                // timer must not clear connection-level media state while
                // camera remains announced.
                self.pump_video_send(live, &mut video.camera, VideoBroadcastKind::Camera, frame)
                    .await;
            }
            VideoWork::Send(VideoBroadcastKind::Screen, frame) => {
                // Screen share rides its own broadcast connection. If it went
                // away between polling and processing (stream torn down), drop the
                // frame; the lifecycle clears the rest next round.
                let Some(target) = self.load_broadcast_connection() else {
                    return;
                };
                if !video.screen.belongs_to(&target) {
                    // Connection changed after `pull` selected this frame.
                    // Drop old announce/encoder state; next lifecycle round
                    // binds and announces against replacement transport.
                    video.screen.reset();
                    return;
                }
                self.pump_video_send(
                    &target,
                    &mut video.screen,
                    VideoBroadcastKind::Screen,
                    frame,
                )
                .await;
            }
            VideoWork::JobComplete(kind, completion) => {
                let state = match kind {
                    VideoBroadcastKind::Camera => &mut video.camera,
                    VideoBroadcastKind::Screen => &mut video.screen,
                };
                state.send_task = None;
                match completion {
                    VideoJobCompletion::Send(completion) => {
                        // Restore exclusive resources before any future can
                        // yield or cancellation can discard completed turn.
                        state.codec = completion.codec;
                        state.scaler = completion.scaler;
                        if state.codec.is_none() {
                            match completion.result {
                                Err(err) => {
                                    error!("Failed to initialize {kind:?} video encoder: {err}")
                                }
                                Ok(_) => {
                                    error!("{kind:?} video job completed without encoder state")
                                }
                            }
                            let announced = state.announced;
                            let failed_target = state.connection.clone();
                            state.reset();
                            if announced
                                && let Some(target) = failed_target.as_ref()
                                && self.load_gateway_for(target, kind).is_some()
                            {
                                if let Err(err) =
                                    self.announce_video(target, kind, false, None, None).await
                                {
                                    warn!("Failed to retract failed {kind:?} video: {err}");
                                }
                                if let Err(err) = self.update_speaking(target, kind, false).await {
                                    warn!("Failed to clear failed {kind:?} speaking state: {err}");
                                }
                            }
                            VideoControls::new(self.discord)
                                .video_input_ended(kind)
                                .await;
                            return;
                        }
                        let codec_kind = state
                            .codec
                            .as_ref()
                            .expect("video codec checked above")
                            .codec();
                        match completion.result {
                            Ok(outcome) if state.keyframe_armed => {
                                let action = keyframe_completion_action(codec_kind, outcome);
                                tracing::trace!(
                                    target: "video_timing",
                                    stage = "outgoing.control",
                                    ?kind,
                                    codec = ?codec_kind,
                                    action = ?action,
                                    submitted_frame_id = completion.submitted_frame_id,
                                    emitted_frame_id = completion.emitted_frame_id,
                                    forced_keyframe = completion.forced_keyframe,
                                    emitted = outcome.emitted,
                                    keyframe = outcome.keyframe,
                                    "video keyframe state changed"
                                );
                                match action {
                                    KeyframeCompletionAction::Complete => {
                                        state.keyframe_armed = false;
                                        state.last_keyframe = Some(Instant::now());
                                        debug!("{kind:?} keyframe emitted");
                                    }
                                    KeyframeCompletionAction::Pending => {}
                                    KeyframeCompletionAction::Retry => {
                                        state.keyframe_armed = false;
                                        state.keyframe_requested = true;
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(err) => {
                                // Any encoded-but-unsent frame may be a future
                                // AV1/H264 reference. Recover even when this
                                // failed frame was not itself a forced IDR.
                                state.keyframe_armed = false;
                                state.keyframe_requested = true;
                                tracing::trace!(
                                    target: "video_timing",
                                    stage = "outgoing.control",
                                    ?kind,
                                    codec = ?codec_kind,
                                    action = "send_failed_recovery_requested",
                                    submitted_frame_id = completion.submitted_frame_id,
                                    emitted_frame_id = completion.emitted_frame_id,
                                    forced_keyframe = completion.forced_keyframe,
                                    error = %err,
                                    "video keyframe state changed"
                                );
                                warn!("Failed to send {kind:?} video frame: {err}");
                            }
                        }
                    }
                    VideoJobCompletion::Retransmit {
                        requested,
                        result,
                        elapsed_us,
                    } => {
                        tracing::trace!(
                            target: "video_timing",
                            stage = "outgoing.control",
                            ?kind,
                            action = "rtx_complete",
                            requested,
                            sent = result.as_ref().copied().unwrap_or_default(),
                            elapsed_us,
                            failed = result.is_err(),
                            error = ?result.as_ref().err(),
                            "video retransmit completed"
                        );
                        match result {
                            Ok(sent) => {
                                trace!("Retransmitted {sent}/{requested} {kind:?} video packets")
                            }
                            Err(err) => warn!("Failed to retransmit {kind:?} video packets: {err}"),
                        }
                    }
                }
            }
            VideoWork::RequestVideoKeyframe(kind) => {
                let state = match kind {
                    VideoBroadcastKind::Camera => &mut video.camera,
                    VideoBroadcastKind::Screen => &mut video.screen,
                };
                state.keyframe_requested = true;
                tracing::trace!(
                    target: "video_timing",
                    stage = "outgoing.control",
                    ?kind,
                    action = "receiver_keyframe_requested",
                    "video keyframe state changed"
                );
            }
            VideoWork::RetransmitVideo(kind, sequences) => {
                let state = match kind {
                    VideoBroadcastKind::Camera => &mut video.camera,
                    VideoBroadcastKind::Screen => &mut video.screen,
                };
                state.queue_retransmits(sequences);
                if state.send_task.is_some() {
                    return;
                }
                let sequences = std::mem::take(&mut state.pending_retransmits);
                if sequences.is_empty() {
                    return;
                }
                let target = match kind {
                    VideoBroadcastKind::Camera => live.clone(),
                    VideoBroadcastKind::Screen => {
                        let Some(target) = self.load_broadcast_connection() else {
                            return;
                        };
                        target
                    }
                };
                let Some(gateway) = self.load_gateway_for(&target, kind) else {
                    return;
                };
                let requested = sequences.len();
                state.send_task = Some(smol::spawn(async move {
                    let started = Instant::now();
                    let result = smol::unblock(move || {
                        smol::block_on(async move {
                            let video_t = target.video_send_transport(&gateway.dave_session);
                            video_t.retransmit(&sequences).await
                        })
                    })
                    .await;
                    VideoJobCompletion::Retransmit {
                        requested,
                        result,
                        elapsed_us: elapsed_micros(started),
                    }
                }));
            }
            VideoWork::VideoLossReport(kind, fraction_lost) => {
                let state = match kind {
                    VideoBroadcastKind::Camera => &mut video.camera,
                    VideoBroadcastKind::Screen => &mut video.screen,
                };
                let previous_quality = state.network_quality;
                state.observe_loss(fraction_lost);
                if previous_quality != state.network_quality {
                    tracing::trace!(
                        target: "video_timing",
                        stage = "outgoing.control",
                        ?kind,
                        action = "network_quality_changed",
                        fraction_lost,
                        previous_quality,
                        network_quality = state.network_quality,
                        "video profile state changed"
                    );
                }
            }
            VideoWork::Yield => {}
        }
    }

    /// Lazily acquire — or tear down — one outgoing video stream's send state
    /// to match `on`, without blocking: the app's frame source is requested
    /// once (via `AddVideoInput`) and picked up on a later round. On turn-off
    /// it retracts the opcode-12 announce on `target` and drops the encoder.
    /// `target` is `None` when the stream's connection isn't up yet (the Go
    /// Live broadcast mid-handshake) — acquisition waits, but the teardown path
    /// still clears any half-set-up state.
    async fn video_lifecycle(
        &self,
        on: bool,
        target: Option<&Arc<MediaConnection>>,
        state: &mut OutgoingVideo,
        kind: VideoBroadcastKind,
    ) {
        if on {
            let Some(target) = target else {
                // A previous transport vanished mid-stream. Never carry its
                // announce/codec state onto a replacement connection.
                state.reset();
                return;
            };
            if state.connection.is_none() || !state.belongs_to(target) {
                state.reset();
                state.connection = Some(target.clone());
            }
            if state.input.is_none() {
                match state.input_recv.take() {
                    None => {
                        let (sender, receiver) = oneshot::channel();
                        if let Some(gateway) = self.gateway.current.load().as_ref() {
                            gateway.events.submit_video(VideoEvent::AddVideoInput {
                                kind,
                                channel: sender,
                            });
                        }
                        state.input_recv = Some(receiver);
                    }
                    Some(mut receiver) => match receiver.try_recv() {
                        Ok(Some(input)) => {
                            state.frames_per_second = input
                                .frames_per_second
                                .clamp(1, MAX_VIDEO_FRAMES_PER_SECOND);
                            state.resolution = Some(input.resolution);
                            state.bitrate_bps = input.bitrate_bps;
                            state.input = Some(input.frames);
                        }
                        // Not supplied yet — keep waiting.
                        Ok(None) => state.input_recv = Some(receiver),
                        // App declined capture. Treat this as a stop instead of
                        // issuing AddVideoInput forever on every loop round.
                        Err(_) => {
                            debug!("{kind:?} video input request was declined");
                            state.reset();
                            VideoControls::new(self.discord)
                                .video_input_ended(kind)
                                .await;
                        }
                    },
                }
            }
            if state.input.is_some() {
                let Some(desired_codec) = target.video_codec() else {
                    // Session Description/Update has not confirmed a codec.
                    return;
                };
                let sink_quality = match kind {
                    // Keep working camera behavior unchanged. Screen is the
                    // high-bandwidth path needing adaptive send tiers.
                    VideoBroadcastKind::Camera => FULL_VIDEO_QUALITY,
                    VideoBroadcastKind::Screen => target
                        .outbound_video_quality
                        .load(Ordering::Acquire)
                        .min(state.network_quality),
                };
                let desired = state.resolution.and_then(|resolution| {
                    VideoSendProfile::for_stream(
                        kind,
                        state.frames_per_second,
                        resolution,
                        state.bitrate_bps,
                        sink_quality,
                    )
                });
                if desired.is_none() {
                    if !state.sink_paused {
                        tracing::trace!(
                            target: "video_timing",
                            stage = "outgoing.control",
                            ?kind,
                            codec = ?desired_codec,
                            action = "sink_paused",
                            sink_quality,
                            network_quality = state.network_quality,
                            "video profile state changed"
                        );
                    }
                    if state.announced {
                        if let Err(err) = self.announce_video(target, kind, false, None, None).await
                        {
                            warn!("Failed to pause {kind:?} video announce: {err}");
                        }
                        if let Err(err) = self.update_speaking(target, kind, false).await {
                            warn!("Failed to pause {kind:?} speaking state: {err}");
                        }
                    }
                    state.send_task = None;
                    state.pending_retransmits.clear();
                    state.codec = None;
                    state.rebuild_codec = false;
                    state.profile = None;
                    state.sink_paused = true;
                    state.announced = false;
                    state.announce_dirty = false;
                    state.next_frame_at = None;
                    return;
                }

                state.sink_paused = false;
                let desired = desired.unwrap();
                // Let current access unit finish before applying adaptive
                // profile changes. Cancelling mid-packet leaves an incomplete
                // RTP frame and throws away reusable encoder state.
                if state.send_task.is_some() {
                    return;
                }
                let codec_changed = state
                    .codec
                    .as_ref()
                    .is_none_or(|codec| codec.codec() != desired_codec);
                if state.profile == Some(desired) && !codec_changed {
                    return;
                }
                tracing::trace!(
                    target: "video_timing",
                    stage = "outgoing.control",
                    ?kind,
                    codec = ?desired_codec,
                    action = "codec_profile_rebuild_requested",
                    sink_quality,
                    network_quality = state.network_quality,
                    previous_profile = ?state.profile,
                    desired_profile = ?desired,
                    codec_changed,
                    "video profile state changed"
                );
                state.rebuild_codec = true;
                state.profile = Some(desired);
                state.announce_dirty = state.announced;
                state.keyframe_armed = false;
                state.keyframe_requested = true;
                state.last_keyframe = None;
                state.next_frame_at = None;
                debug!(
                    "{kind:?} send profile: codec={desired_codec:?} quality={} {}x{} {}fps {}bps",
                    desired.quality,
                    desired.resolution.width,
                    desired.resolution.height,
                    desired.frames_per_second,
                    desired.bitrate_bps
                );
            }
        } else if state.announced
            || state.connection.is_some()
            || state.input.is_some()
            || state.input_recv.is_some()
            || state.codec.is_some()
            || state.send_task.is_some()
        {
            let matching_target = target.filter(|target| state.belongs_to(target));
            if state.announced
                && let Some(target) = matching_target
            {
                if let Err(err) = self.announce_video(target, kind, false, None, None).await {
                    warn!("Failed to retract {kind:?} video announce: {err}");
                }
                if let Err(err) = self.update_speaking(target, kind, false).await {
                    warn!("Failed to clear {kind:?} speaking state: {err}");
                }
            }
            state.reset();
        }
    }

    /// Encode and transmit one captured frame for an outgoing stream on
    /// `target`, doing the first-frame opcode-12 announce and periodic
    /// keyframing. A `None` frame means the app's input ended — retract the
    /// announce, clear connection-level media state, and clear send state.
    async fn pump_video_send(
        &self,
        target: &Arc<MediaConnection>,
        state: &mut OutgoingVideo,
        kind: VideoBroadcastKind,
        frame: Option<VideoFrame>,
    ) {
        let Some(frame) = frame else {
            debug!("{kind:?} video input ended");
            if state.announced {
                if let Err(err) = self.announce_video(target, kind, false, None, None).await {
                    warn!("Failed to retract {kind:?} video announce: {err}");
                }
                if let Err(err) = self.update_speaking(target, kind, false).await {
                    warn!("Failed to clear {kind:?} speaking state: {err}");
                }
            }
            state.reset();
            VideoControls::new(self.discord)
                .video_input_ended(kind)
                .await;
            return;
        };
        let Some(profile) = state.profile else {
            // Sink-paused streams are not polled, but one selected frame may
            // race with a quality-zero event. Drop it instead of reviving send.
            return;
        };
        let frame_dimensions =
            match frame.dimensions_to_fit(profile.resolution.width, profile.resolution.height) {
                Ok(dimensions) => dimensions,
                Err(err) => {
                    warn!("Invalid {kind:?} video frame: {err}");
                    return;
                }
            };
        let frame_started = Instant::now();
        let frame_timing = frame.timing();
        let frame_id = frame_timing.map_or(state.next_frame_id, |timing| timing.id());
        state.next_frame_id = state.next_frame_id.wrapping_add(1);
        let frame_interval =
            Duration::from_secs_f64(1.0 / f64::from(profile.frames_per_second.max(1)));
        state.next_frame_at = Some(frame_started + frame_interval);
        tracing::trace!(
            target: "video_timing",
            stage = "outgoing.accept",
            ?kind,
            frame_id,
            pipeline_elapsed_us = frame_timing.map_or(0, |timing| timing.elapsed_micros()),
            source_width = frame.width,
            source_height = frame.height,
            output_width = frame_dimensions.0,
            output_height = frame_dimensions.1,
            frame_budget_us = duration_micros(frame_interval),
            "video stage complete"
        );

        let Some(gateway) = self.load_gateway_for(target, kind) else {
            warn!("{kind:?} gateway changed before video send");
            return;
        };
        let Some(codec_kind) = target.video_codec() else {
            trace!("Waiting for server-selected {kind:?} video codec");
            return;
        };
        let dave_transition_pending = !gateway.dave_pending_transitions.is_empty();
        let dave_ready = target
            .video_send_transport_with_transition_gate(
                &gateway.dave_session,
                &gateway.dave_pending_transitions,
            )
            .is_dave_ready();
        if dave_transition_pending || !dave_ready {
            state.dave_transition_blocked |= dave_transition_pending;
            tracing::trace!(
                target: "video_timing",
                stage = "outgoing.control",
                ?kind,
                codec = ?codec_kind,
                action = if dave_transition_pending {
                    "dave_transition_frame_deferred"
                } else {
                    "dave_busy_or_not_ready_frame_deferred"
                },
                frame_id,
                pending_transitions = gateway.dave_pending_transitions.len(),
                "video send deferred"
            );
            return;
        }
        if std::mem::take(&mut state.dave_transition_blocked) {
            if !state.keyframe_armed {
                state.keyframe_requested = true;
            }
            tracing::trace!(
                target: "video_timing",
                stage = "outgoing.control",
                ?kind,
                codec = ?codec_kind,
                action = "dave_transition_video_resumed",
                frame_id,
                keyframe_already_armed = state.keyframe_armed,
                "video send resumed"
            );
        }

        // The first Video payload must precede any video RTP — and a Speaking
        // payload must precede *any* media from this connection ("You must send
        // at least one Speaking payload before sending or receiving data"). The
        // audio path Speaks when the mic goes hot, but a silent mic means a
        // video-only session would otherwise transmit on SSRCs the voice server
        // never saw registered, and it silently discards that video. Keep this
        // connection-level state raised until video teardown: clearing it on
        // microphone silence can stop camera forwarding.
        let mut send_not_before = None;
        let signaling_started = Instant::now();
        if !state.announced || state.announce_dirty {
            let first_announce = !state.announced;
            if first_announce && let Err(err) = self.update_speaking(target, kind, true).await {
                warn!("Failed to send {kind:?} speaking update: {err}");
                return;
            }
            if let Err(err) = self
                .announce_video(target, kind, true, Some(frame_dimensions), Some(profile))
                .await
            {
                warn!("Failed to announce {kind:?} video: {err}");
                return;
            }
            state.announced = true;
            state.announce_dirty = false;
            // Give server time to process registration before first RTP, but
            // carry deadline into isolated video job. Sleeping in this media
            // turn would starve ten 20 ms audio frames.
            if first_announce {
                send_not_before = Some(Instant::now() + VIDEO_ANNOUNCE_SETTLE);
            }
        }
        let signaling_us = elapsed_micros(signaling_started);

        // PLI/FIR requests immediate recovery. Server-selected cadence applies
        // to both codec implementations and changes live through opcode 14.
        // It also provides bounded recovery when receiver feedback is absent.
        let now = Instant::now();
        let arm_keyframe = !state.keyframe_armed
            && (state.keyframe_requested
                || video_keyframe_due(state.last_keyframe, now, target.video_keyframe_interval()));
        let keyframe_reason = if state.keyframe_requested {
            "requested"
        } else {
            "periodic"
        };
        let codec = state.codec.take();
        let rebuild_codec = std::mem::take(&mut state.rebuild_codec)
            || codec
                .as_ref()
                .is_none_or(|codec| codec.codec() != codec_kind);
        if arm_keyframe {
            // Clear consumed request now; a newer PLI arriving while task runs
            // sets it again and is not erased by this task's completion.
            state.keyframe_armed = true;
            state.keyframe_requested = false;
            tracing::trace!(
                target: "video_timing",
                stage = "outgoing.control",
                ?kind,
                codec = ?codec_kind,
                action = "keyframe_armed",
                reason = keyframe_reason,
                frame_id,
                "video keyframe state changed"
            );
        }

        let mut scaler = std::mem::take(&mut state.scaler);
        let target = target.clone();
        let encode_target = target.clone();
        let max_width = profile.resolution.width;
        let max_height = profile.resolution.height;
        let bitrate_bps = profile.bitrate_bps;
        let frames_per_second = profile.frames_per_second;
        let job_queued_at = Instant::now();
        let frame_span = tracing::trace_span!(
            target: "video_timing",
            "outgoing_frame",
            ?kind,
            frame_id,
            source_width = frame.width,
            source_height = frame.height,
        );
        state.send_task = Some(smol::spawn(async move {
            // Native codec construction/destruction, RGB scaling/conversion,
            // and video encoding are CPU-bound. Blocking-pool isolation keeps
            // all of them outside audio scheduler.
            let encode_span = frame_span.clone();
            let (codec, scaler, encoded) = smol::unblock(move || {
                encode_span.in_scope(|| {
                    let worker_started = Instant::now();
                    let worker_queue_us = elapsed_micros(job_queued_at);
                    let mut codec = match (rebuild_codec, codec) {
                        (false, Some(codec)) => codec,
                        (_, retired_codec) => {
                            drop(retired_codec);
                            match encode_target.new_video_send_codec_with_bitrate(
                                codec_kind,
                                kind,
                                frames_per_second,
                                bitrate_bps,
                            ) {
                                Ok(codec) => codec,
                                Err(err) => {
                                    return (
                                        None,
                                        scaler,
                                        Err(format!("failed to initialize video encoder: {err}")),
                                    );
                                }
                            }
                        }
                    };
                    if arm_keyframe {
                        // OpenH264 retains intra request when rate control skips
                        // input, so do not reissue it until completion reports.
                        codec.request_keyframe();
                    }
                    let result = frame
                        .downscale_to_fit_with(&mut scaler, max_width, max_height)
                        .and_then(|frame| codec.encode(&frame))
                        .map_err(|err| err.to_string());
                    let (emitted, encoded_bytes) = match &result {
                        Ok(Some(encoded)) => (true, encoded.data.len()),
                        Ok(None) | Err(_) => (false, 0),
                    };
                    let emitted_frame_id = result
                        .as_ref()
                        .ok()
                        .and_then(Option::as_ref)
                        .and_then(|encoded| encoded.timing())
                        .map_or(0, |timing| timing.id());
                    tracing::trace!(
                        target: "video_timing",
                        stage = "outgoing.encode_worker",
                        ?kind,
                        frame_id,
                        emitted_frame_id,
                        pipeline_elapsed_us = frame_timing
                            .map_or(0, |timing| timing.elapsed_micros()),
                        worker_queue_us,
                        elapsed_us = elapsed_micros(worker_started),
                        encoded_bytes,
                        codec_rebuilt = rebuild_codec,
                        forced_keyframe = arm_keyframe,
                        emitted,
                        failed = result.is_err(),
                        "video stage complete"
                    );
                    (Some(codec), scaler, result)
                })
            })
            .await;

            let settle_started = Instant::now();
            if let Some(wait) =
                send_not_before.and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            {
                Delay::new(wait).await;
            }
            let announce_settle_us = elapsed_micros(settle_started);
            let transport_started = Instant::now();
            let mut completed_frame_timing = frame_timing;
            let mut completed_frame_id = frame_id;
            let mut completion_span = frame_span.clone();
            let result = match encoded {
                Ok(Some(encoded)) => {
                    completed_frame_timing = encoded.timing().or(frame_timing);
                    completed_frame_id =
                        completed_frame_timing.map_or(frame_id, |timing| timing.id());
                    completion_span = tracing::trace_span!(
                        target: "video_timing",
                        "outgoing_encoded_frame",
                        ?kind,
                        frame_id = completed_frame_id,
                    );
                    let keyframe = encoded.keyframe;
                    let transport_span = completion_span.clone();
                    smol::unblock(move || {
                        transport_span.in_scope(|| {
                            // Packetization, DAVE/AEAD, and history copies
                            // interleave async UDP sends. Local block_on keeps
                            // whole bounded transport stage off media executor.
                            smol::block_on(async move {
                                let video_t = target.video_send_transport_with_transition_gate(
                                    &gateway.dave_session,
                                    &gateway.dave_pending_transitions,
                                );
                                video_t
                                    .send_encoded_video_frame_for_timed(
                                        &encoded,
                                        bitrate_bps,
                                        kind,
                                        completed_frame_timing,
                                    )
                                    .await
                            })
                        })
                    })
                    .await
                    .map(|emitted| VideoSendOutcome {
                        emitted,
                        keyframe: emitted && keyframe,
                    })
                }
                Ok(None) => Ok(VideoSendOutcome::default()),
                Err(err) => Err(io::Error::new(io::ErrorKind::InvalidData, err).into()),
            };
            let job_us = elapsed_micros(job_queued_at);
            let discord_us = elapsed_micros(frame_started);
            let pipeline_elapsed_us =
                completed_frame_timing.map_or(discord_us, |timing| timing.elapsed_micros());
            let frame_budget_us = duration_micros(frame_interval);
            completion_span.in_scope(|| {
                tracing::trace!(
                    target: "video_timing",
                    stage = "outgoing.complete",
                    ?kind,
                    frame_id = completed_frame_id,
                    submitted_frame_id = frame_id,
                    emitted_frame_id = completed_frame_id,
                    signaling_us,
                    announce_settle_us,
                    transport_us = elapsed_micros(transport_started),
                    job_us,
                    discord_us,
                    pipeline_elapsed_us,
                    frame_budget_us,
                    over_budget = discord_us > frame_budget_us,
                    codec_rebuilt = rebuild_codec,
                    forced_keyframe = arm_keyframe,
                    keyframe = result
                        .as_ref()
                        .is_ok_and(|outcome| outcome.keyframe),
                    emitted = result.as_ref().is_ok_and(|outcome| outcome.emitted),
                    failed = result.is_err(),
                    error = ?result.as_ref().err(),
                    "video frame complete"
                );
            });
            VideoJobCompletion::Send(VideoSendCompletion {
                codec,
                scaler,
                result,
                submitted_frame_id: frame_id,
                emitted_frame_id: completed_frame_id,
                forced_keyframe: arm_keyframe,
            })
        }));
    }

    /// Announce (or retract) our own video stream via voice opcode 12. Sending
    /// `active = true` before the first frame satisfies Discord's requirement
    /// that a Video payload precede any video RTP; `active = false` (video_ssrc
    /// 0) retracts it when the camera turns off.
    ///
    /// <https://docs.discord.food/topics/voice-connections#video>
    async fn announce_video(
        &self,
        connection: &Arc<MediaConnection>,
        kind: VideoBroadcastKind,
        active: bool,
        dimensions: Option<(u32, u32)>,
        profile: Option<VideoSendProfile>,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        let Some(gateway) = self.load_gateway_for(connection, kind) else {
            return Err(async_tungstenite::tungstenite::Error::ConnectionClosed);
        };
        let audio_ssrc = connection.ssrc();
        // Retracting (active = false) announces video_ssrc 0 with no layers;
        // otherwise announce exactly the one layer we encode. A second inactive
        // stream still owns a selectable SSRC and can strand viewers on a layer
        // that never receives RTP.
        let (video_ssrc, rtx_ssrc, streams) = if active {
            let profile = profile.expect("active video announce requires send profile");
            let video_ssrc = connection.video_ssrc();
            let rtx_ssrc = connection.rtx_ssrc();
            let (width, height) = dimensions.unwrap_or((640, 360));
            let streams = vec![json!({
                "type": "video",
                "rid": "100",
                "ssrc": video_ssrc,
                "rtx_ssrc": rtx_ssrc,
                "active": true,
                "quality": 100,
                "max_bitrate": profile.bitrate_bps,
                "max_framerate": profile.frames_per_second,
                "max_resolution": { "type": "fixed", "width": width, "height": height }
            })];
            (video_ssrc, rtx_ssrc, streams)
        } else {
            (0, 0, vec![])
        };
        let payload = json!({
            "op": VoiceOpcode::Video as u8,
            "d": {
                "audio_ssrc": audio_ssrc,
                "video_ssrc": video_ssrc,
                "rtx_ssrc": rtx_ssrc,
                "streams": streams,
            }
        });
        debug!("Sending opcode 12 ({kind:?}, active={active}): {payload}");
        gateway.websocket.send(payload.to_string().into()).await
    }

    /// Send a speaking-state update, but only on an actual transition.
    async fn update_speaking(
        &self,
        connection: &Arc<MediaConnection>,
        kind: VideoBroadcastKind,
        speaking: bool,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        let Some(gateway) = self.load_gateway_for(connection, kind) else {
            return Err(async_tungstenite::tungstenite::Error::ConnectionClosed);
        };
        let previous = connection.is_speaking.swap(speaking, Ordering::AcqRel);
        if previous == speaking {
            return Ok(());
        }
        let payload = speaking_payload(connection.ssrc(), speaking);
        if let Err(err) = gateway.websocket.send(payload).await {
            // Failed signaling must remain retryable. Restore old state only
            // when no newer transition won the race while send was pending.
            let _ = connection.is_speaking.compare_exchange(
                speaking,
                previous,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            return Err(err);
        }
        Ok(())
    }

    /// Load current call's complete connection generation.
    fn load_call_connection(&self) -> Option<Arc<MediaConnection>> {
        let gateway = self.gateway.current.load();
        let voice_gateway = gateway.as_ref()?.voice.load_gateway()?;
        voice_gateway.connection.load_full()
    }

    /// Load our own Go Live broadcast's gateway and media connection, if that
    /// second voice connection is up. Loaded fresh each round like the call so
    /// it appears and vanishes with the broadcast; `None` until the op-18
    /// handshake completes or after the stream is torn down.
    fn load_broadcast_connection(&self) -> Option<Arc<MediaConnection>> {
        let gateway = self.gateway.current.load();
        let broadcast_gateway = gateway.as_ref()?.voice.broadcast.load_gateway()?;
        broadcast_gateway.connection.load_full()
    }

    /// Load gateway for `kind`, but only while it still owns `connection`.
    fn load_gateway_for(
        &self,
        connection: &Arc<MediaConnection>,
        kind: VideoBroadcastKind,
    ) -> Option<Arc<Gateway<Media>>> {
        let gateway = self.gateway.current.load();
        let voice = &gateway.as_ref()?.voice;
        let media_gateway = match kind {
            VideoBroadcastKind::Camera => voice.load_gateway()?,
            VideoBroadcastKind::Screen => voice.broadcast.load_gateway()?,
        };
        let current = media_gateway.connection.load_full()?;
        Arc::ptr_eq(&current, connection).then_some(media_gateway)
    }

    /// Like [`Self::load_call_connection`], but `None` unless loaded connection is
    /// still the exact one identified by `identity`. A different `Connection`
    /// means the original call ended (and possibly a new one took its place),
    /// so the caller must tear down rather than carry on.
    fn load_live_call_connection(
        &self,
        identity: &Arc<MediaConnection>,
    ) -> Option<Arc<MediaConnection>> {
        let call = self.load_call_connection()?;
        Arc::ptr_eq(&call, identity).then_some(call)
    }

    /// Resolve media source only if its slot still owns both the generation and
    /// exact connection captured when UDP data arrived.
    fn resolve_media_source(&self, source: MediaSource) -> Option<Arc<MediaConnection>> {
        let gateway = self.gateway.current.load();
        let voice = &gateway.as_ref()?.voice;
        let current = match source.kind {
            MediaSourceKind::Call if voice.call_generation() == source.generation => {
                voice.load_gateway()?.connection.load_full()
            }
            MediaSourceKind::WatchedStream => {
                let key = source.stream_key.as_ref()?;
                let slot = voice.watched_stream(key)?;
                if slot.generation() != source.generation {
                    return None;
                }
                slot.load_gateway()?.connection.load_full()
            }
            MediaSourceKind::Broadcast if voice.broadcast.generation() == source.generation => {
                voice.broadcast.load_gateway()?.connection.load_full()
            }
            _ => return None,
        }?;
        Arc::ptr_eq(&current, &source.connection).then_some(source.connection)
    }
}

impl DiscordRuntimeState {
    /// Thin runtime facade for capability listeners. Coordination helpers live
    /// on [`MediaCoordinator`].
    pub(crate) async fn drive_media(&self) -> MediaDrive {
        MediaCoordinator::new(self).drive_media().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_video_job_does_not_block_media_select_or_get_cancelled() {
        smol::block_on(async {
            let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let worker_gate = gate.clone();
            let (started, worker_started) = oneshot::channel();
            let mut task = smol::spawn(async move {
                smol::unblock(move || {
                    started.send(()).expect("test waiter must remain alive");
                    let (lock, wake) = &*worker_gate;
                    let released = lock.lock().unwrap();
                    drop(wake.wait_while(released, |released| !*released).unwrap());
                    42
                })
                .await
            });
            worker_started.await.expect("blocking worker must start");

            {
                let mut video = pin!(task_or_idle(Some(&mut task)).fuse());
                let mut audio = pin!(future::ready(()).fuse());
                select! {
                    _ = audio => {}
                    _ = video => panic!("blocked video job unexpectedly won select"),
                }
            }

            let (lock, wake) = &*gate;
            *lock.lock().unwrap() = true;
            wake.notify_one();
            assert_eq!(task_or_idle(Some(&mut task)).await, 42);
        });
    }

    #[test]
    fn retransmit_queue_is_deduplicated_and_bounded() {
        let mut state = OutgoingVideo::default();
        state.queue_retransmits([4, 2, 4, 3]);
        state.queue_retransmits(0..=u16::MAX);

        assert_eq!(
            state.pending_retransmits.len(),
            MAX_PENDING_VIDEO_RETRANSMITS
        );
        assert_eq!(&state.pending_retransmits[..3], &[4, 2, 3]);
        assert_eq!(
            state
                .pending_retransmits
                .iter()
                .filter(|sequence| **sequence == 4)
                .count(),
            1
        );
    }

    #[test]
    fn screen_quality_selects_bounded_send_tiers() {
        let resolution = VideoResolution::new(1920, 1080);
        assert_eq!(
            VideoSendProfile::for_stream(
                VideoBroadcastKind::Screen,
                30,
                resolution,
                2_500_000,
                100,
            ),
            Some(VideoSendProfile {
                quality: 100,
                frames_per_second: 30,
                bitrate_bps: 2_500_000,
                resolution,
            })
        );
        assert_eq!(
            VideoSendProfile::for_stream(
                VideoBroadcastKind::Screen,
                30,
                resolution,
                2_500_000,
                50,
            )
                .unwrap()
                .bitrate_bps,
            1_200_000
        );
        assert_eq!(
            VideoSendProfile::for_stream(
                VideoBroadcastKind::Screen,
                30,
                resolution,
                2_500_000,
                25,
            )
                .unwrap()
                .frames_per_second,
            10
        );
        assert_eq!(
            VideoSendProfile::for_stream(VideoBroadcastKind::Screen, 30, resolution, 2_500_000, 0,),
            None
        );
    }

    #[test]
    fn loss_reports_reduce_fast_and_recover_slowly() {
        let mut state = OutgoingVideo::default();
        state.observe_loss(26);
        assert_eq!(state.network_quality, 66);
        state.observe_loss(26);
        assert_eq!(state.network_quality, 32);
        state.observe_loss(0);
        assert_eq!(state.network_quality, 32);
        state.observe_loss(0);
        assert_eq!(state.network_quality, 49);
    }

    #[test]
    fn server_keyframe_interval_controls_periodic_request() {
        let now = Instant::now();
        let interval = Duration::from_millis(750);

        assert!(video_keyframe_due(None, now, interval));
        assert!(!video_keyframe_due(
            Some(now - Duration::from_millis(749)),
            now,
            interval
        ));
        assert!(video_keyframe_due(
            Some(now - Duration::from_millis(750)),
            now,
            interval
        ));
    }

    #[test]
    fn delayed_av1_output_does_not_reissue_pending_keyframe() {
        let older_inter = VideoSendOutcome {
            emitted: true,
            keyframe: false,
        };
        assert_eq!(
            keyframe_completion_action(VideoCodec::Av1, older_inter),
            KeyframeCompletionAction::Pending
        );
        assert_eq!(
            keyframe_completion_action(VideoCodec::H264, older_inter),
            KeyframeCompletionAction::Retry
        );
        assert_eq!(
            keyframe_completion_action(
                VideoCodec::Av1,
                VideoSendOutcome {
                    emitted: true,
                    keyframe: true,
                },
            ),
            KeyframeCompletionAction::Complete
        );
    }
}
