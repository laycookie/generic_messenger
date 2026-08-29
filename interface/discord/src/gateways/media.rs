use std::{
    error::Error,
    fmt, io, mem,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use dashmap::DashMap;
use facet::Facet;
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt as _, StreamExt as _, channel::mpsc, channel::oneshot, future, select};
use messenger_interface::{
    interface::{AudioEvent, FrameProducer, VideoBroadcastKind},
    stream::ArcStream,
};
use num_enum::TryFromPrimitive;
use simple_audio_channels::output::SampleProducer as AudioSampleProducer;
use tracing::{debug, error, trace};

use crate::api::SNOWFLAKE;
use crate::gateways::{Gateway, Handshake as _, Handshaking};

pub(crate) use self::gateway::Media;

use self::{
    connection::{RecvCodec, UdpPacket},
    coordinator::{MediaReceiver, MediaSource, MediaSourceKind, PolledMedia, WatchedMedia},
    gateway::MediaConnection,
};

pub(crate) mod audio;
mod codecs;
pub(super) mod connection;
pub(crate) mod coordinator;
mod dispatcher;
mod events;
mod gateway;
mod payloads;

pub(crate) use codecs::{AtomicDiscordVideoCodec, DiscordAudioCodec, DiscordVideoCodec};

/// Voice gateway protocol version.
pub(crate) const VOICE_GATEWAY_VERSION: usize = 9;
const MAX_PENDING_CALL_AUDIO_EVENTS: usize = 128;

pub type SessionId = String;

/// Minimal call destination retained during voice handshake. Full channel
/// payloads remain in `DiscordCache`; media routing needs only these IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VoiceTarget {
    pub(crate) guild_id: Option<SNOWFLAKE>,
    pub(crate) channel_id: SNOWFLAKE,
}

/// Discord Go Live stream identifier. Keeps stream routing distinct from
/// unrelated protocol strings while preserving its exact wire representation.
///
/// <https://docs.discord.food/topics/voice-connections#stream-key>
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StreamKey(String);

impl StreamKey {
    /// Build the deterministic key shared by broadcast and watch requests.
    pub fn for_broadcaster(
        guild_id: Option<SNOWFLAKE>,
        channel_id: SNOWFLAKE,
        user_id: SNOWFLAKE,
    ) -> Self {
        Self(match guild_id {
            Some(guild_id) => format!("guild:{guild_id}:{channel_id}:{user_id}"),
            None => format!("call:{channel_id}:{user_id}"),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for StreamKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for StreamKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone)]
pub struct Endpoint {
    pub wss: String,
    pub token: String,
}
impl Endpoint {
    pub fn new(wss: String, token: String) -> Self {
        Self { wss, token }
    }
}

/// Which voice connection a gateway handler ([`events::GatewayPayload::exec`])
/// runs against. The two diverge in exactly two spots: a stream carries remote
/// *screen share* rather than camera video, and only the call publishes a
/// per-call audio source on its first SessionDescription (a stream's media is
/// pumped by the call's existing coordinator).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionRole {
    Call,
    Stream,
}

impl ConnectionRole {
    /// The kind of remote video a source on this connection broadcasts.
    pub(crate) fn remote_video_kind(self) -> VideoBroadcastKind {
        match self {
            Self::Call => VideoBroadcastKind::Camera,
            Self::Stream => VideoBroadcastKind::Screen,
        }
    }
}

/// <https://docs.discord.food/topics/voice-connections#voice-opcodes>
#[repr(u8)]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Facet, TryFromPrimitive)]
#[facet(is_numeric)]
pub enum VoiceOpcode {
    Identify = 0,
    SelectProtocol = 1,
    Ready = 2,
    Heartbeat = 3,
    SessionDescription = 4,
    Speaking = 5,
    HeartbeatACK = 6,
    Resume = 7,
    Hello = 8,
    Resumed = 9,
    ClientConnect = 11,
    Video = 12,
    ClientDisconnect = 13,
    SessionUpdate = 14,
    MediaSinkWants = 15,
    VoiceBackendVersion = 16,
    ClientFlags = 18,
    SpeedTest = 19,
    ClientPlatform = 20,
    DAVEProtocolPrepareTransition = 21,
    DAVEProtocolExecuteTransition = 22,
    DAVEProtocolTransitionReady = 23,
    DAVEProtocolPrepareEpoch = 24,
    MLSExternalSenderPackage = 25,
    MLSKeyPackage = 26,
    MLSProposals = 27,
    MLSCommitWelcome = 28,
    MLSAnnounceCommitTransition = 29,
    MLSWelcome = 30,
    MLSInvalidCommitWelcome = 31,
}

/// Parsed binary frame from Discord's voice gateway.
///
/// v7 wire format:  `[opcode: u8][payload: ...]`
/// v8+ wire format: `[sequence: u16 BE][opcode: u8][payload: ...]`
/// <https://docs.discord.food/topics/voice-connections#binary-websocket-messages>
pub struct VoiceBinaryFrame<'a> {
    pub(crate) sequence: Option<u16>,
    pub(crate) opcode: VoiceOpcode,
    pub(crate) payload: &'a [u8],
}

impl<'a> VoiceBinaryFrame<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, io::Error> {
        if VOICE_GATEWAY_VERSION > 7 {
            // v8+: [sequence: u16 BE][opcode: u8][payload: ...]
            if bytes.len() < 3 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "binary frame too short for v8+ format",
                ));
            }
            let sequence = u16::from_be_bytes([bytes[0], bytes[1]]);
            let opcode = VoiceOpcode::try_from(bytes[2]).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown voice opcode {}: {err}", bytes[2]),
                )
            })?;
            Ok(Self {
                sequence: Some(sequence),
                opcode,
                payload: &bytes[3..],
            })
        } else {
            // v7: [opcode: u8][payload: ...]
            let (&opcode_byte, payload) = bytes
                .split_first()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty binary frame"))?;
            let opcode = VoiceOpcode::try_from(opcode_byte).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown voice opcode {opcode_byte}: {err}"),
                )
            })?;
            Ok(Self {
                sequence: None,
                opcode,
                payload,
            })
        }
    }
}

/// Local state machine for Discord's voice connection handshake.
/// <https://docs.discord.food/topics/voice-connections#connecting-to-voice>
#[derive(Default)]
pub enum VoiceStatus {
    #[default]
    Closed,
    AwaitingData {
        target: VoiceTarget,
    },
    AwaitingEndpoint {
        target: VoiceTarget,
        session_id: SessionId,
    },
    AwaitingSession {
        target: VoiceTarget,
        endpoint: Endpoint,
    },
    Ready {
        target: VoiceTarget,
        endpoint: Endpoint,
        session_id: SessionId,
    },
}
impl VoiceStatus {
    pub fn insert_endpoint(&mut self, endpoint: Endpoint) {
        *self = match mem::take(self) {
            Self::Closed => Self::Closed,
            Self::AwaitingData { target } => Self::AwaitingSession { endpoint, target },
            Self::AwaitingEndpoint { target, session_id } => Self::Ready {
                endpoint,
                session_id,
                target,
            },
            Self::AwaitingSession { target, .. } => Self::AwaitingSession { endpoint, target },
            Self::Ready {
                session_id, target, ..
            } => Self::Ready {
                endpoint,
                session_id,
                target,
            },
        }
    }
    pub fn insert_session_id(&mut self, session_id: SessionId) {
        *self = match mem::take(self) {
            Self::Closed => Self::Closed,
            Self::AwaitingData { target } => Self::AwaitingEndpoint { session_id, target },
            Self::AwaitingEndpoint { target, session_id } => {
                Self::AwaitingEndpoint { target, session_id }
            }
            Self::AwaitingSession { target, endpoint } => Self::Ready {
                endpoint,
                session_id,
                target,
            },
            Self::Ready {
                endpoint, target, ..
            } => Self::Ready {
                endpoint,
                session_id,
                target,
            },
        }
    }
}

/// One voice-websocket connection slot: a handshake status that accumulates the
/// data needed to identify, plus the [`Gateway<Media>`] once it is open. The
/// call's own connection and each Go Live stream connection are the same slot
/// with a different handshake; the [`MediaConnectState`] trait supplies the only two
/// things they don't share — the extra data `connect` needs, and whether an
/// existing connection is replaced or kept.
#[derive(Default)]
pub struct MediaGatewaySlot<H> {
    status: AsyncMutex<H>,
    gateway: ArcSwapOption<Gateway<Media>>,
    /// User-requested outbound media. Independent from connection capability:
    /// a call always negotiates camera support, but sends only when enabled.
    send_enabled: AtomicBool,
    /// Serializes costly websocket opens without blocking status changes such
    /// as disconnect/replacement while an open is in flight.
    connect_lock: AsyncMutex<()>,
    /// Bumped with every handshake/status replacement. A connection finishing
    /// for an older generation is discarded instead of resurrecting stale
    /// state after disconnect or stream replacement.
    generation: AtomicU64,
}

/// One Go Live stream's separate voice connection — a *second* connection to a
/// different RTC server, protocol-identical to the call's; only how the details
/// are collected differs. Each watched stream gets one slot, while the singleton
/// broadcast slot carries our own screen share. Who sends media on the resulting
/// connection is decided by the shared media coordinator, not here.
pub type StreamGateway = MediaGatewaySlot<StreamStatus>;

/// Per-call mixer-handshake source. It owns only its call channel; gateway and
/// backend ownership stay outside the source.
pub(crate) struct CallAudioEventStream {
    receiver: AsyncMutex<mpsc::Receiver<AudioEvent>>,
    closed: AtomicBool,
}

impl CallAudioEventStream {
    fn new(receiver: mpsc::Receiver<AudioEvent>) -> Self {
        Self {
            receiver: AsyncMutex::new(receiver),
            closed: AtomicBool::new(false),
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

#[async_trait]
impl ArcStream for CallAudioEventStream {
    type Item = AudioEvent;

    async fn next(self: Arc<Self>) -> Option<Self::Item> {
        if self.is_closed() {
            return None;
        }

        let mut receiver = self.receiver.lock().await;
        if self.is_closed() {
            return None;
        }

        let event = receiver.next().await?;
        (!self.is_closed()).then_some(event)
    }
}

/// Strong source retention and producer endpoint for one call generation.
struct CallAudioEvents {
    generation: u64,
    source: Arc<CallAudioEventStream>,
    // `futures::mpsc` grants one extra slot per Sender clone. Keep exactly one
    // sender behind a short synchronous lock so repeated submissions cannot
    // inflate the nominal bounded capacity.
    sender: StdMutex<mpsc::Sender<AudioEvent>>,
}

impl CallAudioEvents {
    fn new(generation: u64) -> Self {
        // futures mpsc adds one guaranteed slot per sender; this type keeps
        // exactly one sender, so subtract it to make the named cap exact.
        let (sender, receiver) = mpsc::channel(MAX_PENDING_CALL_AUDIO_EVENTS - 1);
        Self {
            generation,
            source: Arc::new(CallAudioEventStream::new(receiver)),
            sender: StdMutex::new(sender),
        }
    }

    fn close(&self) {
        self.source.close();
        self.sender
            .lock()
            .expect("call audio event sender mutex poisoned")
            .close_channel();
    }

    fn submit(&self, event: AudioEvent) -> bool {
        if self.source.is_closed() {
            return false;
        }
        self.sender
            .lock()
            .expect("call audio event sender mutex poisoned")
            .try_send(event)
            .is_ok()
    }
}

impl Drop for CallAudioEvents {
    fn drop(&mut self) {
        self.close();
    }
}

/// A call's primary media connection and every secondary Go Live connection
/// derived from its voice session. Keeping the slots under one owner makes
/// leaving or replacing the call tear down all dependent media connections.
#[derive(Default)]
pub struct MediaGateway {
    call: MediaGatewaySlot<VoiceStatus>,
    /// Active call's audio event source. Replacement closes old source before
    /// publishing a new generation, so reconnect consumers never share a queue.
    audio_events: ArcSwapOption<CallAudioEvents>,
    pub broadcast: StreamGateway,
    watched_streams: DashMap<StreamKey, Arc<StreamGateway>>,
    watched_poll_cursor: AtomicUsize,
    /// Shared media coordinator. Audio and video listeners both drive it, while
    /// this lock guarantees one UDP reader across all voice connections.
    pub(crate) media: AsyncMutex<coordinator::MediaRuntime>,
}

/// Connection data `MediaGatewaySlot::connect` hands to the pending media
/// gateway, extracted once a [`MediaConnectState`] has collected everything.
pub struct ConnectParams {
    endpoint: Endpoint,
    session_id: SessionId,
    /// Identify `server_id`: the guild (or channel id for a DM call) for the
    /// call's connection, or the stream's RTC server id.
    server_id: SNOWFLAKE,
    channel_id: SNOWFLAKE,
    /// MLS group identity. Calls use channel ID; Go Live uses its distinct
    /// media-session ID (`rtc_server_id - 1`).
    dave_group_id: SNOWFLAKE,
    /// Local video layer offered by this connection. Calls offer camera,
    /// broadcasts offer screen, and watched Go Live streams are receive-only.
    outbound_video_kind: Option<VideoBroadcastKind>,
}

/// Progress made by one connection attempt. Explicit variants avoid treating
/// an already-open stream as newly connected and repeating follow-up commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectProgress {
    WaitingForData,
    AlreadyConnected,
    Connected,
    Superseded,
}

/// State that accumulates prerequisites for a media connection. This precedes
/// the WebSocket [`crate::gateways::Handshake`]: once parameters are complete,
/// [`MediaGatewaySlot`] creates `Gateway<Media, Handshaking>` and performs that
/// protocol handshake.
pub trait MediaConnectState: Default + Send {
    /// Data `connect` needs that the handshake doesn't hold itself: `()` for the
    /// call (its session id is in the handshake), the call's session id for a
    /// stream (whose identify reuses it).
    type ConnectContext: Send;
    /// Whether calling `connect` while already connected re-opens the websocket
    /// (`true`) or keeps the live one (`false`). The call re-opens so a fresh
    /// VOICE_SERVER_UPDATE (region change / token refresh) re-negotiates; a
    /// stream keeps its connection so the repeated STREAM_* dispatches that
    /// complete its handshake don't tear it down and rebuild it.
    const REOPEN_IF_CONNECTED: bool;
    /// The connection data once the handshake is complete, else `None` — the
    /// dispatches that supply it race, so callers retry until it is `Some`.
    fn connect_params(&self, context: &Self::ConnectContext) -> Option<ConnectParams>;
}

impl<H: MediaConnectState> MediaGatewaySlot<H> {
    pub fn load_gateway(&self) -> Option<Arc<Gateway<Media>>> {
        self.gateway.load_full()
    }

    pub fn set_send_enabled(&self, enabled: bool) -> bool {
        self.send_enabled.swap(enabled, Ordering::AcqRel)
    }

    pub fn send_enabled(&self) -> bool {
        self.send_enabled.load(Ordering::Acquire)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Open the voice websocket once handshake data is complete. Callers retry
    /// from whichever dispatch supplies missing data. Result distinguishes a
    /// new connection from an existing or superseded generation.
    pub async fn connect(
        &self,
        context: H::ConnectContext,
        user_id: SNOWFLAKE,
    ) -> Result<ConnectProgress, Box<dyn Error + Send + Sync>> {
        let _connect_guard = self.connect_lock.lock().await;
        if !H::REOPEN_IF_CONNECTED && self.gateway.load().is_some() {
            return Ok(ConnectProgress::AlreadyConnected);
        }
        let (generation, params) = {
            let status = self.status.lock().await;
            (
                self.generation.load(Ordering::Acquire),
                status.connect_params(&context),
            )
        };
        let Some(params) = params else {
            return Ok(ConnectProgress::WaitingForData);
        };
        trace!("MediaGatewaySlot::connect: handshake data ready, opening voice websocket");
        let gateway = Gateway::<Media, Handshaking>::new(
            &params.endpoint,
            &params.session_id,
            params.server_id,
            params.channel_id,
            params.dave_group_id,
            user_id,
            params.outbound_video_kind,
        )
        .await?
        .handshake()
        .await?;

        // Re-lock for commit. Disconnect/begin_watch can run during network
        // setup; their generation bump makes this stale result a no-op.
        let _status = self.status.lock().await;
        if self.generation.load(Ordering::Acquire) != generation {
            trace!("Discarding voice gateway opened for stale handshake generation");
            return Ok(ConnectProgress::Superseded);
        }
        self.gateway.store(Some(Arc::new(gateway)));
        Ok(ConnectProgress::Connected)
    }

    pub async fn disconnect(&self) {
        trace!("MediaGatewaySlot::disconnect: clearing status and stored gateway");
        let mut status = self.status.lock().await;
        *status = H::default();
        self.send_enabled.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.gateway.store(None);
    }
}

impl MediaGateway {
    fn open_audio_events(&self) -> Arc<CallAudioEvents> {
        let generation = self.call_generation();
        if let Some(events) = self.audio_events.load_full()
            && events.generation == generation
            && !events.source.is_closed()
        {
            return events;
        }

        let events = Arc::new(CallAudioEvents::new(generation));
        if let Some(previous) = self.audio_events.swap(Some(events.clone())) {
            previous.close();
        }
        events
    }

    /// Opens a source only for the call gateway that is still installed. A
    /// disconnect racing selected voice work cannot resurrect a retired call.
    pub(crate) fn open_audio_source_for(
        &self,
        expected: &Arc<Gateway<Media>>,
    ) -> Option<Arc<CallAudioEventStream>> {
        let is_current = || {
            self.call
                .load_gateway()
                .is_some_and(|current| Arc::ptr_eq(&current, expected))
        };
        if !is_current() {
            return None;
        }

        let events = self.open_audio_events();
        if is_current() {
            Some(events.source.clone())
        } else {
            events.close();
            None
        }
    }

    fn active_audio_events(&self) -> Option<Arc<CallAudioEvents>> {
        let events = self.audio_events.load_full()?;
        (events.generation == self.call_generation() && !events.source.is_closed())
            .then_some(events)
    }

    pub(crate) fn submit_audio(&self, event: AudioEvent) -> bool {
        self.active_audio_events()
            .is_some_and(|events| events.submit(event))
    }

    fn close_audio_source(&self) {
        if let Some(events) = self.audio_events.swap(None) {
            events.close();
        }
    }

    pub fn watched_stream(&self, key: &StreamKey) -> Option<Arc<StreamGateway>> {
        self.watched_streams
            .get(key)
            .map(|entry| entry.value().clone())
    }

    pub fn watched_streams(&self) -> Vec<(StreamKey, Arc<StreamGateway>)> {
        self.watched_streams
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    pub fn watched_streams_for_poll(&self) -> Vec<(StreamKey, Arc<StreamGateway>)> {
        let mut streams = self.watched_streams();
        if !streams.is_empty() {
            let start = self.watched_poll_cursor.fetch_add(1, Ordering::Relaxed) % streams.len();
            streams.rotate_left(start);
        }
        streams
    }

    /// Insert one independently watched Go Live stream. The returned boolean is
    /// true only for the caller that created the slot and must send Stream Watch.
    pub fn begin_watching_stream(
        &self,
        stream_key: StreamKey,
        channel_id: SNOWFLAKE,
        source_user: SNOWFLAKE,
    ) -> (Arc<StreamGateway>, bool) {
        match self.watched_streams.entry(stream_key.clone()) {
            dashmap::Entry::Occupied(entry) => (entry.get().clone(), false),
            dashmap::Entry::Vacant(entry) => {
                let slot = Arc::new(StreamGateway::new_watching(
                    stream_key,
                    channel_id,
                    Some(source_user),
                ));
                entry.insert(slot.clone());
                (slot, true)
            }
        }
    }

    /// Remove only the exact slot expected by an in-flight operation. Pointer
    /// identity prevents stale websocket/heartbeat results from deleting a new
    /// subscription that reused the same stream key.
    pub fn remove_watched_stream_if(
        &self,
        stream_key: &StreamKey,
        expected: &Arc<StreamGateway>,
    ) -> Option<Arc<StreamGateway>> {
        let dashmap::Entry::Occupied(entry) = self.watched_streams.entry(stream_key.clone()) else {
            return None;
        };
        if !Arc::ptr_eq(entry.get(), expected) {
            return None;
        }
        Some(entry.remove())
    }

    pub fn remove_watched_stream(&self, stream_key: &StreamKey) -> Option<Arc<StreamGateway>> {
        self.watched_streams
            .remove(stream_key)
            .map(|(_, slot)| slot)
    }

    pub async fn remove_watched_source(
        &self,
        user_id: SNOWFLAKE,
        channel_id: SNOWFLAKE,
    ) -> Vec<Arc<StreamGateway>> {
        let mut removed = Vec::new();
        for (key, slot) in self.watched_streams() {
            if slot.watched_source().await == Some((user_id, channel_id))
                && let Some(slot) = self.remove_watched_stream_if(&key, &slot)
            {
                removed.push(slot);
            }
        }
        removed
    }

    pub async fn remove_watched_channel(&self, channel_id: SNOWFLAKE) -> Vec<Arc<StreamGateway>> {
        let mut removed = Vec::new();
        for (key, slot) in self.watched_streams() {
            if slot
                .watched_source()
                .await
                .is_some_and(|(_, source_channel)| source_channel == channel_id)
                && let Some(slot) = self.remove_watched_stream_if(&key, &slot)
            {
                removed.push(slot);
            }
        }
        removed
    }

    pub async fn watched_sources(&self) -> Vec<(SNOWFLAKE, SNOWFLAKE)> {
        let mut sources = Vec::new();
        for (_, slot) in self.watched_streams() {
            if let Some(source) = slot.watched_source().await {
                sources.push(source);
            }
        }
        sources
    }

    /// Await UDP only, returning owned classified media from any live voice
    /// transport. Processing stays with the audio/video coordinator.
    #[allow(
        clippy::too_many_arguments,
        reason = "arguments expose disjoint mutable borrows used by one select round"
    )]
    pub(crate) async fn poll_media(
        &self,
        live: &Arc<MediaConnection>,
        live_gateway: &Arc<Gateway<Media>>,
        recv: &mut RecvCodec,
        watched: Vec<WatchedMedia<'_>>,
        broadcast: Option<(&Arc<MediaConnection>, &Arc<Gateway<Media>>)>,
        broadcast_recv: Option<&mut MediaReceiver>,
        call_source: MediaSource,
        broadcast_source: Option<MediaSource>,
    ) -> PolledMedia {
        let transport = live.recv_transport(
            &live_gateway.dave_session,
            &live.ssrc_to_user_id,
            &live.video_ssrc_to_user_id,
            &live.rtx_to_video_ssrc,
        );
        let mut incoming = std::pin::pin!(
            async {
                if !live.has_description() {
                    return future::pending::<Result<UdpPacket<'_>, String>>().await;
                }
                transport.recv(recv).await.map_err(|err| err.to_string())
            }
            .fuse()
        );
        let watched_incoming = watched
            .into_iter()
            .map(|watched| {
                Box::pin(async move {
                    Self::classify_udp(watched.receiver.recv(watched.gateway).await, watched.source)
                })
            })
            .collect::<Vec<_>>();
        let mut watched_incoming = std::pin::pin!(
            async move {
                if watched_incoming.is_empty() {
                    future::pending().await
                } else {
                    future::select_all(watched_incoming).await.0
                }
            }
            .fuse()
        );
        let mut broadcast_incoming =
            std::pin::pin!(Self::poll_aux_udp(broadcast, broadcast_recv).fuse());
        select! {
            packet = incoming => Self::classify_udp(packet, call_source),
            packet = watched_incoming => packet,
            packet = broadcast_incoming => Self::classify_udp(
                packet,
                broadcast_source.expect("broadcast receive requires a media source"),
            ),
        }
    }

    async fn poll_aux_udp<'a>(
        media: Option<(&Arc<MediaConnection>, &Arc<Gateway<Media>>)>,
        receiver: Option<&'a mut MediaReceiver>,
    ) -> Result<UdpPacket<'a>, String> {
        match (media, receiver) {
            (Some((_, gateway)), Some(receiver)) => receiver.recv(gateway).await,
            _ => future::pending().await,
        }
    }

    fn classify_udp(packet: Result<UdpPacket<'_>, String>, source: MediaSource) -> PolledMedia {
        let source_kind = source.kind;
        match (source_kind, packet) {
            (MediaSourceKind::Call, Ok(UdpPacket::Voice { ssrc, samples })) => PolledMedia::Audio {
                source,
                ssrc,
                samples: samples.to_vec(),
            },
            (
                MediaSourceKind::Call | MediaSourceKind::WatchedStream,
                Ok(UdpPacket::Video { ssrc, frame }),
            ) => PolledMedia::Video {
                source,
                ssrc,
                frame,
                kind: match source_kind {
                    MediaSourceKind::Call => VideoBroadcastKind::Camera,
                    MediaSourceKind::WatchedStream => VideoBroadcastKind::Screen,
                    MediaSourceKind::Broadcast => unreachable!(),
                },
            },
            (
                MediaSourceKind::Call | MediaSourceKind::Broadcast,
                Ok(UdpPacket::Rtcp {
                    feedback: Some(value),
                    ..
                }),
            ) => PolledMedia::VideoFeedback {
                source,
                kind: match source_kind {
                    MediaSourceKind::Call => VideoBroadcastKind::Camera,
                    MediaSourceKind::Broadcast => VideoBroadcastKind::Screen,
                    MediaSourceKind::WatchedStream => unreachable!(),
                },
                feedback: value,
            },
            (_, Ok(UdpPacket::Rtcp { packet_type, .. })) => {
                trace!("Media RTCP: {packet_type:?}");
                PolledMedia::Consumed
            }
            (_, Ok(UdpPacket::UnhandledRtp { ssrc, payload_type })) => {
                debug!("Unhandled RTP payload type {payload_type} from SSRC {ssrc}");
                PolledMedia::Consumed
            }
            (_, Ok(UdpPacket::Voice { .. } | UdpPacket::Video { .. } | UdpPacket::Consumed)) => {
                PolledMedia::Consumed
            }
            (_, Err(err)) => {
                error!("Media receive failed: {err}");
                PolledMedia::Consumed
            }
        }
    }

    pub fn load_gateway(&self) -> Option<Arc<Gateway<Media>>> {
        self.call.load_gateway()
    }

    pub fn set_camera_send_enabled(&self, enabled: bool) -> bool {
        self.call.set_send_enabled(enabled)
    }

    pub fn camera_send_enabled(&self) -> bool {
        self.call.send_enabled()
    }

    pub fn call_generation(&self) -> u64 {
        self.call.generation()
    }

    pub async fn connect(
        &self,
        user_id: SNOWFLAKE,
    ) -> Result<ConnectProgress, Box<dyn Error + Send + Sync>> {
        self.call.connect((), user_id).await
    }

    pub async fn initiate_connection(&self, target: VoiceTarget) {
        self.disconnect().await;
        self.call.initiate_connection(target).await;
    }

    pub async fn insert_endpoint(&self, endpoint: Endpoint) {
        self.close_audio_source();
        self.call.insert_endpoint(endpoint).await;
        self.close_audio_source();
    }

    pub async fn insert_session_id(&self, session_id: SessionId) {
        self.close_audio_source();
        self.call.insert_session_id(session_id).await;
        self.close_audio_source();
    }

    pub async fn session_id(&self) -> Option<SessionId> {
        self.call.session_id().await
    }

    /// End the call and every Go Live connection derived from its session.
    pub async fn disconnect(&self) {
        self.close_audio_source();
        for (key, slot) in self.watched_streams() {
            if let Some(slot) = self.remove_watched_stream_if(&key, &slot) {
                slot.disconnect().await;
            }
        }
        self.broadcast.disconnect().await;
        self.call.disconnect().await;
        self.close_audio_source();
    }
}

impl MediaConnectState for VoiceStatus {
    type ConnectContext = ();
    // The call re-opens so a re-sent VOICE_SERVER_UPDATE re-negotiates.
    const REOPEN_IF_CONNECTED: bool = true;

    fn connect_params(&self, _: &()) -> Option<ConnectParams> {
        let VoiceStatus::Ready {
            endpoint,
            session_id,
            target,
        } = self
        else {
            return None;
        };
        Some(ConnectParams {
            endpoint: endpoint.clone(),
            session_id: session_id.clone(),
            server_id: target.guild_id.unwrap_or(target.channel_id),
            channel_id: target.channel_id,
            dave_group_id: target.channel_id,
            outbound_video_kind: Some(VideoBroadcastKind::Camera),
        })
    }
}

impl MediaGatewaySlot<VoiceStatus> {
    pub async fn initiate_connection(&self, target: VoiceTarget) {
        let mut status = self.status.lock().await;
        *status = VoiceStatus::AwaitingData { target };
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.gateway.store(None);
    }
    pub async fn insert_endpoint(&self, endpoint: Endpoint) {
        let mut status = self.status.lock().await;
        status.insert_endpoint(endpoint);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
    pub async fn insert_session_id(&self, session_id: SessionId) {
        let mut status = self.status.lock().await;
        status.insert_session_id(session_id);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
    /// The voice session id, once the call handshake has collected it. A Go Live
    /// stream connection reuses this same session id in its identify.
    pub async fn session_id(&self) -> Option<SessionId> {
        match &*self.status.lock().await {
            VoiceStatus::AwaitingEndpoint { session_id, .. }
            | VoiceStatus::Ready { session_id, .. } => Some(session_id.clone()),
            _ => None,
        }
    }
}

impl MediaConnectState for StreamStatus {
    type ConnectContext = SessionId;
    // A stream keeps its connection across the repeated STREAM_* dispatches that
    // complete the handshake, so they don't rebuild it.
    const REOPEN_IF_CONNECTED: bool = false;

    fn connect_params(&self, session_id: &SessionId) -> Option<ConnectParams> {
        let StreamStatus::Watching {
            channel_id,
            source_user,
            rtc_server_id: Some(rtc_server_id),
            endpoint: Some(endpoint),
            ..
        } = self
        else {
            return None;
        };
        Some(ConnectParams {
            endpoint: endpoint.clone(),
            session_id: session_id.clone(),
            server_id: *rtc_server_id,
            channel_id: *channel_id,
            dave_group_id: rtc_server_id.checked_sub(1)?,
            outbound_video_kind: source_user.is_none().then_some(VideoBroadcastKind::Screen),
        })
    }
}

#[derive(Default)]
pub enum StreamStatus {
    #[default]
    Idle,
    /// Watching `stream_key`, accumulating the RTC server id (from
    /// STREAM_CREATE) and endpoint+token (from STREAM_SERVER_UPDATE), which
    /// arrive in separate dispatches.
    Watching {
        stream_key: StreamKey,
        channel_id: SNOWFLAKE,
        /// Remote broadcaster selected by UI. `None` for our own broadcast.
        source_user: Option<SNOWFLAKE>,
        rtc_server_id: Option<SNOWFLAKE>,
        endpoint: Option<Endpoint>,
    },
}

impl MediaGatewaySlot<StreamStatus> {
    fn new_watching(
        stream_key: StreamKey,
        channel_id: SNOWFLAKE,
        source_user: Option<SNOWFLAKE>,
    ) -> Self {
        Self {
            status: AsyncMutex::new(StreamStatus::Watching {
                stream_key,
                channel_id,
                source_user,
                rtc_server_id: None,
                endpoint: None,
            }),
            gateway: ArcSwapOption::empty(),
            send_enabled: AtomicBool::new(false),
            connect_lock: AsyncMutex::new(()),
            generation: AtomicU64::new(0),
        }
    }

    /// The stream key currently being watched, if any.
    pub async fn watching(&self) -> Option<StreamKey> {
        match &*self.status.lock().await {
            StreamStatus::Watching { stream_key, .. } => Some(stream_key.clone()),
            StreamStatus::Idle => None,
        }
    }

    /// Remote broadcaster and call channel selected by an explicit watch
    /// request. `None` for an idle slot and for our own broadcast slot.
    pub async fn watched_source(&self) -> Option<(SNOWFLAKE, SNOWFLAKE)> {
        match &*self.status.lock().await {
            StreamStatus::Watching {
                source_user: Some(user),
                channel_id,
                ..
            } => Some((*user, *channel_id)),
            _ => None,
        }
    }

    /// Begin watching `stream_key`; the connection details follow via
    /// [`Self::set_rtc_server_id`] and [`Self::set_server`].
    pub async fn begin_watch(
        &self,
        stream_key: StreamKey,
        channel_id: SNOWFLAKE,
        source_user: Option<SNOWFLAKE>,
    ) {
        let mut status = self.status.lock().await;
        *status = StreamStatus::Watching {
            stream_key,
            channel_id,
            source_user,
            rtc_server_id: None,
            endpoint: None,
        };
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.gateway.store(None);
    }

    pub async fn set_rtc_server_id(&self, key: &StreamKey, rtc_server_id: SNOWFLAKE) {
        if let StreamStatus::Watching {
            stream_key,
            rtc_server_id: slot,
            ..
        } = &mut *self.status.lock().await
            && stream_key == key
        {
            *slot = Some(rtc_server_id);
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub async fn set_server(&self, key: &StreamKey, endpoint: Endpoint) {
        if let StreamStatus::Watching {
            stream_key,
            endpoint: slot,
            ..
        } = &mut *self.status.lock().await
            && stream_key == key
        {
            *slot = Some(endpoint);
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Per-SSRC playback channel for one media stream, generic over the producer
/// half `M` the app hands back ([`AudioSampleProducer`] for audio,
/// [`FrameProducer`] for video — see the [`AudioChannel`] / [`VideoChannel`]
/// aliases). Both kinds share the lifecycle: frames can beat the gateway event
/// that names the SSRC's user (UDP vs websocket), a requested channel waits for
/// the app to supply its producer, and a connected one just pushes frames.
pub enum MediaChannel<M> {
    /// Frames arrived before the event mapping this SSRC to a user (UDP vs
    /// websocket — no ordering guarantee). Counts frames dropped while waiting
    /// so the wait can be bounded; the channel is only requested once the user
    /// is known (or the wait gives up), so per-user controls can bind to it.
    AwaitingIdentity(u8),
    Initializing(oneshot::Receiver<M>),
    Connected {
        producer: M,
        /// Consecutive frames the producer refused. Audio uses this to
        /// rate-limit overflow logs; video uses it only as generic channel
        /// state after its consumer closes (stale frames are replaced inside
        /// the latest-frame slot).
        dropped_frames: u32,
    },
}

/// Per-audio-SSRC playback channel; see [`MediaChannel`].
pub type AudioChannel = MediaChannel<AudioSampleProducer>;
/// Per-video-SSRC playback channel; see [`MediaChannel`].
pub type VideoChannel = MediaChannel<FrameProducer>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_key_formats_guild_and_private_calls() {
        assert_eq!(
            StreamKey::for_broadcaster(Some(1), 2, 3).as_str(),
            "guild:1:2:3"
        );
        assert_eq!(StreamKey::for_broadcaster(None, 2, 3).as_str(), "call:2:3");
    }

    #[test]
    fn watched_stream_is_receive_only_but_broadcast_can_send() {
        let status = |source_user| StreamStatus::Watching {
            stream_key: StreamKey::from(String::from("guild:1:2:3")),
            channel_id: 2,
            source_user,
            rtc_server_id: Some(4),
            endpoint: Some(Endpoint::new(
                String::from("example"),
                String::from("token"),
            )),
        };
        let session_id = String::from("session");

        assert!(
            status(Some(3))
                .connect_params(&session_id)
                .unwrap()
                .outbound_video_kind
                .is_none()
        );
        assert_eq!(
            status(None)
                .connect_params(&session_id)
                .unwrap()
                .outbound_video_kind,
            Some(VideoBroadcastKind::Screen)
        );
        assert_eq!(
            status(None)
                .connect_params(&session_id)
                .unwrap()
                .dave_group_id,
            3
        );
    }

    #[test]
    fn voice_disconnect_clears_dependent_streams() {
        smol::block_on(async {
            let voice = MediaGateway::default();
            let first_key = StreamKey::from(String::from("watched:first"));
            let second_key = StreamKey::from(String::from("watched:second"));
            assert!(voice.begin_watching_stream(first_key.clone(), 1, 2).1);
            assert!(voice.begin_watching_stream(second_key.clone(), 1, 3).1);
            voice
                .broadcast
                .begin_watch(String::from("broadcast").into(), 1, None)
                .await;

            voice.disconnect().await;

            assert!(voice.watched_stream(&first_key).is_none());
            assert!(voice.watched_stream(&second_key).is_none());
            assert!(voice.broadcast.watching().await.is_none());
        });
    }

    #[test]
    fn audio_source_ends_when_call_disconnects() {
        smol::block_on(async {
            let voice = MediaGateway::default();
            let source = voice.open_audio_events().source.clone();

            let (event, ()) = futures::join!(source.clone().next(), voice.disconnect());

            assert!(event.is_none());
            assert!(voice.audio_events.load().is_none());
        });
    }

    #[test]
    fn call_audio_event_queue_has_a_strict_fixed_capacity() {
        smol::block_on(async {
            let events = CallAudioEvents::new(1);
            for _ in 0..MAX_PENDING_CALL_AUDIO_EVENTS {
                let (sender, _receiver) = oneshot::channel();
                assert!(events.submit(AudioEvent::AddAudioInput(sender)));
            }
            let (overflow, _receiver) = oneshot::channel();
            assert!(!events.submit(AudioEvent::AddAudioInput(overflow)));

            assert!(events.source.clone().next().await.is_some());
            let (replacement, _receiver) = oneshot::channel();
            assert!(events.submit(AudioEvent::AddAudioInput(replacement)));
        });
    }

    #[test]
    fn reconnect_audio_source_is_distinct_and_drops_stale_events() {
        smol::block_on(async {
            let voice = MediaGateway::default();
            let first = voice.open_audio_events().source.clone();
            let (stale_sender, _stale_receiver) = oneshot::channel();
            assert!(voice.submit_audio(AudioEvent::AddAudioInput(stale_sender)));

            voice.disconnect().await;
            let second = voice.open_audio_events().source.clone();

            assert!(!Arc::ptr_eq(&first, &second));
            assert!(first.clone().next().await.is_none());

            let (current_sender, _current_receiver) = oneshot::channel();
            assert!(voice.submit_audio(AudioEvent::AddAudioInput(current_sender)));
            assert!(matches!(
                second.next().await,
                Some(AudioEvent::AddAudioInput(_))
            ));
        });
    }

    #[test]
    fn dropping_call_owner_wakes_pending_audio_source() {
        smol::block_on(async {
            let voice = MediaGateway::default();
            let source = voice.open_audio_events().source.clone();
            drop(voice);

            assert!(source.next().await.is_none());
        });
    }

    #[test]
    fn watched_streams_are_keyed_and_duplicate_subscriptions_are_idempotent() {
        let voice = MediaGateway::default();
        let first_key = StreamKey::from(String::from("guild:1:2:3"));
        let second_key = StreamKey::from(String::from("guild:1:2:4"));

        let (first, first_inserted) = voice.begin_watching_stream(first_key.clone(), 2, 3);
        let (duplicate, duplicate_inserted) = voice.begin_watching_stream(first_key.clone(), 2, 3);
        let (_, second_inserted) = voice.begin_watching_stream(second_key.clone(), 2, 4);

        assert!(first_inserted);
        assert!(!duplicate_inserted);
        assert!(Arc::ptr_eq(&first, &duplicate));
        assert!(second_inserted);
        assert_eq!(voice.watched_streams().len(), 2);
        assert!(voice.watched_stream(&first_key).is_some());
        assert!(voice.watched_stream(&second_key).is_some());

        let removed = voice.remove_watched_stream_if(&first_key, &first).unwrap();
        let (replacement, replacement_inserted) =
            voice.begin_watching_stream(first_key.clone(), 2, 3);
        assert!(replacement_inserted);
        assert!(!Arc::ptr_eq(&removed, &replacement));
        assert!(
            voice
                .remove_watched_stream_if(&first_key, &removed)
                .is_none()
        );
        assert!(
            voice
                .watched_stream(&first_key)
                .is_some_and(|current| Arc::ptr_eq(&current, &replacement))
        );
    }

    #[test]
    fn watched_stream_handshakes_do_not_mix_connection_data() {
        smol::block_on(async {
            let voice = MediaGateway::default();
            let first_key = StreamKey::from(String::from("guild:1:2:3"));
            let second_key = StreamKey::from(String::from("guild:1:2:4"));
            let (first, _) = voice.begin_watching_stream(first_key.clone(), 2, 3);
            let (second, _) = voice.begin_watching_stream(second_key.clone(), 2, 4);
            let session_id = String::from("session");

            first.set_rtc_server_id(&first_key, 30).await;
            second
                .set_server(
                    &second_key,
                    Endpoint::new(String::from("second.example"), String::from("second-token")),
                )
                .await;

            assert!(
                first
                    .status
                    .lock()
                    .await
                    .connect_params(&session_id)
                    .is_none()
            );
            assert!(
                second
                    .status
                    .lock()
                    .await
                    .connect_params(&session_id)
                    .is_none()
            );

            first
                .set_server(
                    &first_key,
                    Endpoint::new(String::from("first.example"), String::from("first-token")),
                )
                .await;
            second.set_rtc_server_id(&second_key, 40).await;

            let first_params = first
                .status
                .lock()
                .await
                .connect_params(&session_id)
                .unwrap();
            let second_params = second
                .status
                .lock()
                .await
                .connect_params(&session_id)
                .unwrap();
            assert_eq!(first_params.server_id, 30);
            assert_eq!(first_params.dave_group_id, 29);
            assert_eq!(first_params.endpoint.wss, "first.example");
            assert_eq!(second_params.server_id, 40);
            assert_eq!(second_params.dave_group_id, 39);
            assert_eq!(second_params.endpoint.wss, "second.example");
        });
    }
}
