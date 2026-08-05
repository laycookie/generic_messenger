use std::{
    io,
    marker::PhantomData,
    num::NonZeroU16,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwapOption;
use async_tungstenite::smol::connect_async;
use dashmap::DashMap;
use davey::{DAVE_PROTOCOL_VERSION, DaveSession};
use facet_pretty::FacetPretty;
use futures::{
    channel::oneshot,
    lock::{Mutex as AsyncMutex, MutexGuard},
};
use messenger_interface::interface::{AudioEvent, VideoBroadcastKind, VideoEvent, VideoFrame};
use simple_video::VideoCodec;
use surf::http::convert::json;
use tracing::{debug, error, trace, warn};

use super::{
    AudioChannel, Endpoint, SessionId, VideoChannel, VoiceOpcode,
    connection::{Connection, Ssrc},
    payloads::{HelloPayload, SessionUpdatePayload},
};
use crate::gateways::general::EventHub;
use crate::gateways::{
    Gateway, GatewayFrame, GatewayState, Handshake, Handshaking, Heartbeat, Websocket, expect_hello,
};
use crate::{InnerDiscord, UnitStruct, adapter::video::VideoControls, api::SNOWFLAKE};

/// Frames to drop while waiting for the Speaking event that names an SSRC's
/// user before attaching the channel unbound (no per-user volume/afx). At one
/// Opus frame per 20ms this waits about a second. Video reuses the same
/// warning threshold for its op-12 (Video) announcement — ~1.7s at 30fps —
/// but never exposes video without Discord's SSRC→user mapping.
const MAX_IDENTITY_WAIT_FRAMES: u8 = 50;

pub struct Media {
    heartbeat_version: u8,
    pub channel_id: SNOWFLAKE,
    /// MLS group identity for this media session. Go Live has a separate
    /// group from its parent voice channel.
    pub(crate) dave_group_id: SNOWFLAKE,
    /// Local video layer offered by this connection. Watched Go Live
    /// connections decode only and leave this empty.
    pub outbound_video_kind: Option<VideoBroadcastKind>,
    pub dave_pending_transitions: DashMap<u16, NonZeroU16>, // transition_id, dave_protocol_version
    /// Serialized by necessity: every useful [`DaveSession`] method takes
    /// `&mut self` (the MLS ratchet and nonce state advance on each call), so
    /// this can't be ArcSwap'd or copied — duplicated ratchet state would
    /// reuse AEAD nonces. Keep guard scopes free of I/O awaits instead.
    pub dave_session: AsyncMutex<Option<DaveSession>>,
    /// Current UDP media generation. Replacing this one pointer atomically
    /// replaces every state value whose lifetime is tied to that transport.
    pub connection: ArcSwapOption<MediaConnection>,
}

/// State that must never survive replacement of its UDP media transport.
///
/// One `Arc` is the connection generation token used by media coordination. Keeping
/// routing, playback, speaking, and sink-quality state beside the transport
/// prevents a reconnect from observing a new socket with stale old-call state.
pub struct MediaConnection {
    transport: Connection,
    channel_id: SNOWFLAKE,
    /// Latest server-requested quality for our one outbound layer. Opcode 15
    /// updates this without blocking media; sender reads it before encoding.
    pub outbound_video_quality: AtomicU8,
    pub ssrc_to_audio_channel: DashMap<Ssrc, std::sync::Mutex<super::AudioChannel>>,
    pub ssrc_to_user_id: DashMap<Ssrc, SNOWFLAKE>,
    // Video SSRC bookkeeping, fed by opcode 12 (Video) events. Split from the
    // audio maps because one user owns several SSRCs at once (audio + video +
    // RTX) and the video ones must be retired independently when the camera
    // turns off.
    pub video_ssrc_to_user_id: DashMap<Ssrc, SNOWFLAKE>,
    /// Retransmission SSRC → the primary video SSRC it repairs.
    pub rtx_to_video_ssrc: DashMap<Ssrc, Ssrc>,
    pub ssrc_to_video_channel: DashMap<Ssrc, std::sync::Mutex<super::VideoChannel>>,
    pub is_speaking: AtomicBool,
}

impl MediaConnection {
    pub fn new(transport: Connection, channel_id: SNOWFLAKE) -> Self {
        Self {
            transport,
            channel_id,
            outbound_video_quality: AtomicU8::new(100),
            ssrc_to_audio_channel: DashMap::new(),
            ssrc_to_user_id: DashMap::new(),
            video_ssrc_to_user_id: DashMap::new(),
            rtx_to_video_ssrc: DashMap::new(),
            ssrc_to_video_channel: DashMap::new(),
            is_speaking: AtomicBool::new(false),
        }
    }
}

impl std::ops::Deref for MediaConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.transport
    }
}

pub(crate) struct DaveSessionGuard<'a>(MutexGuard<'a, Option<DaveSession>>);

impl std::ops::Deref for DaveSessionGuard<'_> {
    type Target = DaveSession;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for DaveSessionGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}

impl Media {
    pub(crate) async fn require_dave_session(&self) -> Result<DaveSessionGuard<'_>, io::Error> {
        // Uncontended acquisition is silent; the traces fire only when the
        // lock is actually held elsewhere (media coordinator mid-encrypt/decrypt,
        // or another DAVE event handler).
        let guard = match self.dave_session.try_lock() {
            Some(guard) => guard,
            None => {
                trace!("require_dave_session: waiting for dave_session lock");
                let guard = self.dave_session.lock().await;
                trace!("require_dave_session: dave_session lock acquired");
                guard
            }
        };
        if guard.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DAVE session not initialized",
            ));
        }
        Ok(DaveSessionGuard(guard))
    }
}

impl MediaConnection {
    /// Dispatch a decoded audio frame to the appropriate playback channel.
    ///
    /// Returns `false` if a new channel is being initialized and the caller should yield.
    pub(crate) fn dispatch_incoming_audio(
        &self,
        events: &EventHub,
        ssrc: Ssrc,
        samples: &[i16],
    ) -> bool {
        let mut channel_entry = match self.ssrc_to_audio_channel.entry(ssrc) {
            dashmap::Entry::Occupied(channel) => channel,
            dashmap::Entry::Vacant(vacant_entry) => {
                // The user mapping comes from Speaking events (and DAVE decrypt
                // requires it, so a DAVE call always has it here). On a non-DAVE
                // call the first frames can beat Speaking; wait before creating
                // the channel so it can be bound to its user.
                let Some(user) = self.ssrc_to_user_id.get(&ssrc).map(|entry| *entry) else {
                    vacant_entry.insert(AudioChannel::AwaitingIdentity(1).into());
                    return true;
                };
                let (sender, receiver) = oneshot::channel();
                vacant_entry.insert(AudioChannel::Initializing(receiver).into());
                events.submit_audio(AudioEvent::AddAudioSource {
                    user: Some(user),
                    channel: sender,
                });
                // TODO: Save samples for when channel is ready
                return false;
            }
        };
        loop {
            let mut channel = channel_entry.get().lock().unwrap();
            match &mut *channel {
                AudioChannel::AwaitingIdentity(frames_waited) => {
                    let user = self.ssrc_to_user_id.get(&ssrc).map(|entry| *entry);
                    if user.is_none() && *frames_waited < MAX_IDENTITY_WAIT_FRAMES {
                        // Still nameless: drop this frame too and keep waiting.
                        *frames_waited += 1;
                        break;
                    }
                    if user.is_none() {
                        warn!(
                            "No Speaking event named SSRC {ssrc} within \
                             {MAX_IDENTITY_WAIT_FRAMES} frames; \
                             attaching its audio without per-user controls"
                        );
                    }
                    let (sender, receiver) = oneshot::channel();
                    *channel = AudioChannel::Initializing(receiver);
                    drop(channel);
                    events.submit_audio(AudioEvent::AddAudioSource {
                        user,
                        channel: sender,
                    });
                    return false;
                }
                AudioChannel::Initializing(receiver) => match receiver.try_recv() {
                    Ok(Some(producer)) => {
                        drop(channel);
                        channel_entry.insert(
                            AudioChannel::Connected {
                                producer,
                                dropped_frames: 0,
                            }
                            .into(),
                        );
                        // Re-enter the loop so this frame is delivered to
                        // the freshly connected channel.
                        continue;
                    }
                    // Producer not ready yet: drop this frame and keep
                    // receiving. Spinning here (the previous behavior)
                    // blocks the executor thread until the UI task — which
                    // may need this very thread — sends the producer.
                    Ok(None) => break,
                    Err(_) => {
                        drop(channel);
                        channel_entry.remove();
                        warn!("Audio source sender dropped for SSRC {ssrc}, will re-request");
                        break;
                    }
                },
                AudioChannel::Connected {
                    producer,
                    dropped_frames: overflow_frames,
                } => {
                    if !producer.is_connected() {
                        drop(channel);
                        channel_entry.remove();
                        debug!("Playback mixer channel closed for SSRC {ssrc}; will re-request");
                        break;
                    }
                    // The channel is declared as i16; the mixer converts to
                    // the device format itself.
                    let samples_pushed = producer.push_iter(samples);
                    if samples.len() != samples_pushed {
                        *overflow_frames = overflow_frames.saturating_add(1);
                        if *overflow_frames == 1 || *overflow_frames % 50 == 0 {
                            error!(
                                "Audio buffer overflow. Pushed: {}, Succeeded: {}, repeated: {}",
                                samples.len(),
                                samples_pushed,
                                overflow_frames
                            );
                        }
                    } else {
                        *overflow_frames = 0;
                    }
                    break;
                }
            };
        }
        true
    }

    /// Deliver one decoded video frame to its per-SSRC playback channel —
    /// the video analog of [`Voice::dispatch_incoming_audio`]. Unlike audio
    /// it never asks the caller to yield: the `AddVideoStream` event travels
    /// the *video* event stream, so media coordination keeps running
    /// regardless.
    ///
    /// `kind` distinguishes camera video (on the call connection) from a Go
    /// Live screen share (on its own stream connection), both of which route
    /// through this same per-SSRC machinery on their respective gateways.
    pub(crate) fn dispatch_incoming_video<T: UnitStruct>(
        &self,
        discord: &InnerDiscord<T>,
        ssrc: Ssrc,
        frame: VideoFrame,
        kind: VideoBroadcastKind,
    ) {
        let user = self.video_ssrc_to_user_id.get(&ssrc).map(|entry| *entry);
        let mut channel_entry = match self.ssrc_to_video_channel.entry(ssrc) {
            dashmap::Entry::Occupied(channel) => channel,
            dashmap::Entry::Vacant(vacant_entry) => {
                // Frames can beat the op-12 Video event that names this
                // SSRC's user (UDP vs websocket). Keep waiting so transport
                // SSRC never becomes a separate app-facing stream identity.
                let Some(user) = user else {
                    vacant_entry.insert(VideoChannel::AwaitingIdentity(1).into());
                    return;
                };
                let (sender, receiver) = oneshot::channel();
                vacant_entry.insert(VideoChannel::Initializing(receiver).into());
                let (id, metadata) =
                    discord
                        .media
                        .register_participant_video(user, self.channel_id, kind);
                if let Some(gateway) = discord.gateway.current.load().as_ref() {
                    gateway.events.submit_video(VideoEvent::AddVideoStream {
                        id,
                        metadata,
                        channel: sender,
                    });
                }
                return;
            }
        };
        let mut frame = Some(frame);
        loop {
            let mut channel = channel_entry.get().lock().unwrap();
            match &mut *channel {
                VideoChannel::AwaitingIdentity(frames_waited) => {
                    let Some(user) = user else {
                        if *frames_waited == MAX_IDENTITY_WAIT_FRAMES {
                            warn!(
                                "No Video event named SSRC {ssrc} within \
                                 {MAX_IDENTITY_WAIT_FRAMES} frames; \
                                 dropping video until Discord identifies it"
                            );
                        }
                        *frames_waited = frames_waited.saturating_add(1);
                        return;
                    };
                    let (sender, receiver) = oneshot::channel();
                    *channel = VideoChannel::Initializing(receiver);
                    drop(channel);
                    let (id, metadata) =
                        discord
                            .media
                            .register_participant_video(user, self.channel_id, kind);
                    if let Some(gateway) = discord.gateway.current.load().as_ref() {
                        gateway.events.submit_video(VideoEvent::AddVideoStream {
                            id,
                            metadata,
                            channel: sender,
                        });
                    }
                    return;
                }
                VideoChannel::Initializing(receiver) => match receiver.try_recv() {
                    Ok(Some(producer)) => {
                        drop(channel);
                        channel_entry.insert(
                            VideoChannel::Connected {
                                producer,
                                dropped_frames: 0,
                            }
                            .into(),
                        );
                        // Re-enter the loop so this frame is delivered to
                        // the freshly connected channel.
                        continue;
                    }
                    // Producer not supplied yet: drop the frame, keep waiting.
                    Ok(None) => return,
                    Err(_) => {
                        drop(channel);
                        channel_entry.remove();
                        warn!("Video source sender dropped for SSRC {ssrc}, will re-request");
                        return;
                    }
                },
                VideoChannel::Connected {
                    producer,
                    dropped_frames,
                } => {
                    let frame = frame.take().expect("frame is delivered at most once");
                    if producer.push(frame) {
                        *dropped_frames = 0;
                    } else {
                        // Single-slot channel replaces stale frames internally;
                        // false means app dropped its consumer. Keep source
                        // retired until opcode-12 removal rather than re-open a
                        // channel every decoded frame.
                        *dropped_frames = dropped_frames.saturating_add(1);
                    }
                    return;
                }
            };
        }
    }

    /// Retire every video SSRC owned by `user_id` — dropping its SSRC→user,
    /// playback-channel, and RTX mappings — and, when any existed, tell the app
    /// the source is gone. Returns the retired SSRCs; empty when the user had no
    /// live video (e.g. an audio-only participant's periodic announce), so a
    /// caller can skip logging a non-event.
    pub(crate) fn retire_user_video<T: UnitStruct>(
        &self,
        discord: &InnerDiscord<T>,
        user_id: SNOWFLAKE,
        kind: VideoBroadcastKind,
    ) -> Vec<Ssrc> {
        let stale: Vec<_> = self
            .video_ssrc_to_user_id
            .iter()
            .filter(|entry| *entry.value() == user_id)
            .map(|entry| *entry.key())
            .collect();
        for ssrc in &stale {
            self.video_ssrc_to_user_id.remove(ssrc);
            self.ssrc_to_video_channel.remove(ssrc);
            self.rtx_to_video_ssrc.retain(|_, v| v != ssrc);
        }
        if !stale.is_empty() {
            VideoControls::new(discord).emit_remove_participant_video(
                user_id,
                self.channel_id,
                kind,
            );
        }
        stale
    }
}

pub(crate) struct MediaHandshake {
    identify: GatewayFrame<VoiceOpcode>,
    channel_id: SNOWFLAKE,
    dave_group_id: SNOWFLAKE,
    outbound_video_kind: Option<VideoBroadcastKind>,
}

impl GatewayState<Media> for Handshaking {
    type Data = MediaHandshake;
}

impl Gateway<Media, Handshaking> {
    const VERSION: usize = super::VOICE_GATEWAY_VERSION;
    /// `server_id` is the identify's connection target: the guild id (or the
    /// channel id for a DM) for a call, or the stream's `rtc_server_id` for a
    /// Go Live stream connection. Send-capable connections offer only rid 100;
    /// watched streams offer no outbound layer.
    ///
    /// <https://docs.discord.food/topics/voice-connections#identify-structure>
    pub async fn new(
        endpoint: &Endpoint,
        session_id: &SessionId,
        server_id: SNOWFLAKE,
        channel_id: SNOWFLAKE,
        dave_group_id: SNOWFLAKE,
        user_id: u64,
        outbound_video_kind: Option<VideoBroadcastKind>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // TLS connect has no timeout and is a freeze suspect during VC entry.
        trace!("voice gateway: connecting websocket to {}", endpoint.wss);
        let (voice_websocket, _) = connect_async(
            "wss://".to_string() + &endpoint.wss + "/?v=" + &Self::VERSION.to_string(),
        )
        .await?;
        trace!("voice gateway: websocket connected");
        let websocket = Websocket::new(voice_websocket);

        // Send-capable connections offer one real layer. Watch-only Go Live
        // connections offer none: Discord assigns an SSRC to every offer, so
        // advertising a layer without sending RTP can stall stream startup.
        // Do not add a placeholder low simulcast layer either; viewers may
        // select it and wait forever.
        let streams = match outbound_video_kind {
            Some(kind) => {
                let stream_type = match kind {
                    VideoBroadcastKind::Camera => "video",
                    VideoBroadcastKind::Screen => "screen",
                };
                vec![json!({ "type": stream_type, "rid": "100", "quality": 100 })]
            }
            None => Vec::new(),
        };

        // <https://docs.discord.food/topics/voice-connections#identify-structure>
        let identify = json!({
          "op": VoiceOpcode::Identify as u8,
          "d": {
            // The ID of the guild, private channel, stream, or lobby being
            // connected to. Snowflakes go as strings — the documented (and
            // official-client) shape; numeric forms are merely tolerated.
            "server_id": server_id.to_string(),
            "channel_id": channel_id.to_string(),
            "user_id": user_id.to_string(),
            "session_id": session_id,
            "token": endpoint.token,
            // Declares video support so server sends opcode 12 (Video) SSRC
            // announcements and incoming camera/screen-share media.
            "video": true,
            // The server assigns send SSRCs per offered layer (echoed in
            // Ready `streams`).
            "streams": streams,
            "max_dave_protocol_version": DAVE_PROTOCOL_VERSION,
          }
        });
        debug!("{identify:#?}");

        Ok(Self {
            websocket,
            state: MediaHandshake {
                identify: GatewayFrame::text(identify.to_string()),
                channel_id,
                dave_group_id,
                outbound_video_kind,
            },
            _marker: PhantomData,
        })
    }
}

impl Handshake for Gateway<Media, Handshaking> {
    type Output = Gateway<Media>;

    async fn handshake(self) -> Result<Self::Output, Box<dyn std::error::Error + Send + Sync>> {
        // Identify send and Hello wait have no timeout; both are freeze suspects
        // during VC entry.
        let Gateway {
            websocket, state, ..
        } = self;
        let MediaHandshake {
            identify,
            channel_id,
            dave_group_id,
            outbound_video_kind,
        } = state;
        websocket.send(identify.into_message()).await?;
        trace!("voice gateway: identify sent, waiting for hello");
        let hello: HelloPayload = expect_hello(
            &websocket,
            |opcode| matches!(opcode, VoiceOpcode::Hello),
            "voice gateway",
        )
        .await?;
        debug!("{}", hello.pretty());

        Ok(websocket.into_gateway(
            Duration::from_millis(hello.heartbeat_interval),
            Media {
                heartbeat_version: hello.v,
                channel_id,
                dave_group_id,
                outbound_video_kind,
                dave_pending_transitions: DashMap::new(),
                dave_session: AsyncMutex::new(None),
                connection: Default::default(),
            },
        ))
    }
}

impl Gateway<Media> {
    /// Re-prioritize outbound codecs without replacing active UDP transport.
    /// Server opcode 14 response confirms selection before encoder switches.
    pub(crate) async fn update_video_codecs(
        &self,
        preferred: VideoCodec,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Between Hello and Ready, connection is already visible to control
        // plane but protocol selection has not happened. Ready will read latest
        // preference for initial offer; sending Session Update now is invalid.
        if self.connection.load().is_none() {
            return Ok(());
        }

        let payload = SessionUpdatePayload::new(preferred, self.outbound_video_kind.is_some());
        let payload = String::from_utf8(facet_json::to_vec(&payload)?)?;
        self.websocket.send(payload.into()).await?;
        Ok(())
    }
}

impl Heartbeat for Gateway<Media> {
    /// <https://docs.discord.food/topics/voice-connections#heartbeating>
    async fn heartbeat(&self) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.state.heartbeat_timer.lock().await.wait().await;
        let sequence = self
            .state
            .last_sequence_number
            .get()
            .map(|sequence| sequence.load(Ordering::Relaxed));
        let payload = match self.heartbeat_version {
            ver if 7 < ver => json!({
                    "op": VoiceOpcode::Heartbeat as u8,
                    "d": {
                        "t": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
                        "seq_ack": sequence
                    },
            })
            .to_string(),
            _ => json!({
                    "op": VoiceOpcode::Heartbeat as u8,
                    "d": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            })
            .to_string(),
        };

        self.websocket.send(payload.into()).await
    }
}
