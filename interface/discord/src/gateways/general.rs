use std::{
    error::Error,
    io,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_tungstenite::smol::connect_async;
use facet::Facet;
use num_enum::TryFromPrimitive;
use surf::http::convert::json;
use tracing::info;

use self::payloads::HelloPayload;
use crate::gateways::{
    Gateway, GatewayFrame, GatewayState, Handshake, Handshaking, Heartbeat, Websocket,
    expect_hello, media::MediaGateway, with_timeout,
};
use secure_string::SecureString;

mod commands;
mod dispatcher;
mod event_hub;
mod events;
pub(crate) mod payloads;
pub(crate) mod recording;
mod voice_state;

pub(crate) use commands::{SelfVoiceFlags, VoiceCommand};
pub(crate) use event_hub::EventHub;
pub(crate) use voice_state::VoiceRoster;

// Main Gateway lifecycle:
// <https://docs.discord.food/gateway/using-gateway>

const GATEWAY_OPERATION_TIMEOUT: Duration = Duration::from_secs(15);

/// <https://docs.discord.food/gateway/opcodes-and-close-codes#gateway-opcodes>
#[derive(Facet, TryFromPrimitive)]
#[facet(is_numeric)]
#[non_exhaustive]
#[repr(u8)]
pub enum Opcode {
    Dispatch = 0,
    Heartbeat = 1,
    Identify = 2,
    PresenceUpdate = 3,
    VoiceStateUpdate = 4,
    Resume = 6,
    Reconnect = 7,
    InvalidSession = 9,
    Hello = 10,
    HeartbeatAck = 11,
    CallConnect = 13,
    // "Go Live" screen-share opcodes. A broadcast is a *second* voice
    // connection whose handshake runs over these plus the STREAM_* dispatches:
    // StreamCreate (18) opens our broadcast, StreamDelete (19) ends it,
    // StreamWatch (20) subscribes to someone else's, StreamSetPaused (22)
    // toggles our broadcast's paused flag.
    // <https://docs.discord.food/gateway/gateway-events#create-stream>
    StreamCreate = 18,
    StreamDelete = 19,
    StreamWatch = 20,
    StreamPing = 21,
    StreamSetPaused = 22,
}

/// Dispatch event names handled from Gateway opcode 0.
/// <https://docs.discord.food/gateway/gateway-events>
#[derive(Facet)]
#[facet(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
#[repr(u8)]
pub enum GatewayEvent {
    Hello,
    Ready,
    Resumed,
    Reconnect,
    RateLimited,
    InvalidSession,
    SessionsReplace,
    ApplicationCommandPermissionsUpdate,
    AutoModerationRuleCreate,
    AutoModerationRuleUpdate,
    AutoModerationRuleDelete,
    AutoModerationActionExecution,
    ChannelCreate,
    ChannelUpdate,
    ChannelDelete,
    ChannelPinsUpdate,
    CallCreate,
    CallUpdate,
    CallDelete,
    ThreadCreate,
    ThreadUpdate,
    ThreadDelete,
    ThreadListSync,
    ThreadMemberUpdate,
    ThreadMembersUpdate,
    EntitlementCreate,
    EntitlementUpdate,
    EntitlementDelete,
    GuildCreate,
    GuildUpdate,
    GuildDelete,
    GuildAuditLogEntryCreate,
    GuildBanAdd,
    GuildBanRemove,
    GuildEmojisUpdate,
    GuildStickersUpdate,
    GuildIntegrationsUpdate,
    GuildMemberAdd,
    GuildMemberRemove,
    RelationshipAdd,
    RelationshipUpdate,
    RelationshipRemove,
    PresenceUpdate,
    VoiceStateUpdate,
    VoiceServerUpdate,
    StreamCreate,
    StreamUpdate,
    StreamDelete,
    StreamServerUpdate,
    MessageCreate,
    MessageUpdate,
    MessageDelete,
    MessageDeleteBulk,
    MessageReactionAdd,
    MessageReactionRemove,
}

enum GeneralHandshakeCommand {
    Identify(GatewayFrame<Opcode>),
    Resume(GatewayFrame<Opcode>),
}

pub(crate) struct GeneralHandshake {
    command: GeneralHandshakeCommand,
    continuity: GatewayContinuity,
}

impl GatewayState<General> for Handshaking {
    type Data = GeneralHandshake;
}

/// Application-facing queues that must survive a successful protocol Resume.
/// A reconnecting transport receives the same hub and recorder so no delivered
/// dispatch can disappear between websocket generations.
#[derive(Clone)]
pub(crate) struct GatewayContinuity {
    events: Arc<EventHub>,
    recording: Arc<recording::Recorder>,
}

impl Default for GatewayContinuity {
    fn default() -> Self {
        Self {
            events: Arc::new(EventHub::default()),
            recording: Arc::new(recording::Recorder::default()),
        }
    }
}

pub struct General {
    pub voice: MediaGateway,
    /// Gap-free interface-event routing for this resumable Discord session. If
    /// a route reaches its memory bound, Resume is disabled and the application
    /// requests an authoritative snapshot instead of discarding a delta.
    pub(crate) events: Arc<EventHub>,
    /// Per-fetch gateway event recorder, shared across resumed transports. See
    /// `crate/messenger_interface/docs/races.md`.
    pub recording: Arc<recording::Recorder>,
    /// True between sending opcode 6 and successfully applying RESUMED.
    /// The supervisor permits only a bounded number of retries in this window,
    /// avoiding both premature Identify and an infinite rejected-session loop.
    awaiting_resumed: AtomicBool,
}

impl General {
    fn from_continuity(continuity: GatewayContinuity, awaiting_resumed: bool) -> Self {
        Self {
            voice: MediaGateway::default(),
            events: continuity.events,
            recording: continuity.recording,
            awaiting_resumed: AtomicBool::new(awaiting_resumed),
        }
    }

    pub(crate) fn continuity(&self) -> GatewayContinuity {
        GatewayContinuity {
            events: Arc::clone(&self.events),
            recording: Arc::clone(&self.recording),
        }
    }

    pub(crate) fn awaiting_resumed(&self) -> bool {
        self.awaiting_resumed.load(Ordering::Acquire)
    }

    pub(crate) fn mark_resumed(&self) {
        self.awaiting_resumed.store(false, Ordering::Release);
    }
}

impl GatewayContinuity {
    pub(crate) fn belongs_to(&self, gateway: &General) -> bool {
        Arc::ptr_eq(&self.events, &gateway.events)
            && Arc::ptr_eq(&self.recording, &gateway.recording)
    }
}

fn resume_command(token: &str, session_id: &str, sequence: u64) -> GatewayFrame<Opcode> {
    GatewayFrame::text(
        json!({
            "op": Opcode::Resume as u8,
            "d": {
                "token": token,
                "session_id": session_id,
                "seq": sequence,
            }
        })
        .to_string(),
    )
}

fn normalize_resume_gateway_url(raw: &str) -> Result<surf::Url, io::Error> {
    let mut url = surf::Url::parse(raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Discord Resume gateway URL: {error}"),
        )
    })?;
    let host = url.host_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Discord Resume gateway URL has no host",
        )
    })?;
    let trusted_host = host == "discord.gg"
        || host.ends_with(".discord.gg")
        || host == "discord.com"
        || host.ends_with(".discord.com");
    if url.scheme() != "wss"
        || !trusted_host
        || url.port().is_some_and(|port| port != 443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Discord supplied an untrusted Resume gateway URL",
        ));
    }

    // READY normally supplies a bare origin. Replace, rather than append to,
    // any query so a stale or server-added encoding cannot conflict with ours.
    url.set_path("/");
    url.set_query(Some("encoding=json&v=10"));
    url.set_fragment(None);
    Ok(url)
}

impl Gateway<General, Handshaking> {
    const GATEWAY_URL: &str = "wss://gateway.discord.gg/?encoding=json&v=10";

    async fn connect(
        url: &str,
        command: GeneralHandshakeCommand,
        continuity: GatewayContinuity,
    ) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let (gateway_websocket, _) = with_timeout(
            connect_async(url),
            GATEWAY_OPERATION_TIMEOUT,
            "Discord gateway connect",
        )
        .await?;
        Ok(Self {
            websocket: Websocket::new(gateway_websocket),
            state: GeneralHandshake {
                command,
                continuity,
            },
            _marker: PhantomData,
        })
    }

    pub async fn new(
        token: &SecureString,
        intents: u32,
        capabilities: u32,
    ) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let token = token.unsecure().trim();
        // <https://docs.discord.food/gateway/using-gateway#identifying>
        let identify = GatewayFrame::text(
            json!({
                    "op": Opcode::Identify as u8,
                    "d": {
                        "token": token,
                        "intents": intents,
                        "capabilities": capabilities,
                        "properties": {
                            "$os": "Linux",
                            "$browser": "Firefox",
                            "$device": ""
                        }
                    }
            })
            .to_string(),
        );
        Self::connect(
            Self::GATEWAY_URL,
            GeneralHandshakeCommand::Identify(identify),
            GatewayContinuity::default(),
        )
        .await
    }

    pub(crate) async fn resume(
        token: &SecureString,
        session_id: &str,
        resume_gateway_url: &str,
        sequence: u64,
        continuity: GatewayContinuity,
    ) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let url = normalize_resume_gateway_url(resume_gateway_url)?;
        let resume = resume_command(token.unsecure().trim(), session_id, sequence);
        Self::connect(
            url.as_str(),
            GeneralHandshakeCommand::Resume(resume),
            continuity,
        )
        .await
    }
}

impl Handshake for Gateway<General, Handshaking> {
    type Output = Gateway<General>;

    async fn handshake(self) -> Result<Self::Output, Box<dyn Error + Send + Sync>> {
        let Gateway {
            websocket, state, ..
        } = self;

        let hello = with_timeout(
            expect_hello::<_, HelloPayload>(
                &websocket,
                |opcode| matches!(opcode, Opcode::Hello),
                "gateway",
            ),
            GATEWAY_OPERATION_TIMEOUT,
            "Discord gateway Hello",
        )
        .await?;

        let GeneralHandshake {
            command,
            continuity,
        } = state;
        let (command, operation, awaiting_resumed) = match command {
            GeneralHandshakeCommand::Identify(command) => {
                (command, "Discord gateway Identify", false)
            }
            GeneralHandshakeCommand::Resume(command) => (command, "Discord gateway Resume", true),
        };
        with_timeout(
            websocket.send(command.into_message()),
            GATEWAY_OPERATION_TIMEOUT,
            operation,
        )
        .await?;

        Ok(websocket.into_gateway(
            Duration::from_millis(hello.heartbeat_interval),
            General::from_continuity(continuity, awaiting_resumed),
        ))
    }
}

impl Gateway<General> {
    async fn send_heartbeat(
        &self,
        wait_for_deadline: bool,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        let mut timer = self.state.heartbeat_timer.lock().await;
        if wait_for_deadline {
            timer.wait().await;
        }
        self.state
            .heartbeat_acknowledged
            .allow_dispatch_grace()
            .await;
        self.state.heartbeat_acknowledged.check_before_heartbeat()?;
        let sequence = self
            .state
            .last_sequence_number
            .get()
            .map(|sequence| sequence.load(Ordering::Relaxed));
        // Mark before the concurrent transport write so an exceptionally fast
        // ACK cannot clear the state and then be overwritten back to pending.
        self.state.heartbeat_acknowledged.mark_sent();
        if let Err(error) = self
            .websocket
            .send(
                json!({
                "op": Opcode::Heartbeat as u8,
                "d": sequence,
                })
                .to_string()
                .into(),
            )
            .await
        {
            self.state.heartbeat_acknowledged.acknowledge();
            return Err(error);
        }
        // A server-requested heartbeat satisfies this interval too; resetting
        // avoids immediately sending a duplicate when its deadline races op 1.
        timer.mark_sent();
        info!(?sequence, "Gateway heartbeat sent");
        Ok(())
    }
}

impl Heartbeat for Gateway<General> {
    /// <https://docs.discord.food/gateway/using-gateway#sending-heartbeats>
    async fn heartbeat(&self) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.send_heartbeat(true).await
    }

    async fn heartbeat_now(&self) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.send_heartbeat(false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Facet)]
    struct ResumePayload {
        token: String,
        session_id: String,
        seq: u64,
    }

    #[test]
    fn resume_command_contains_committed_session_position() {
        let frame = resume_command("secret", "session", 42);
        let payload = frame.payload().unwrap();

        assert!(matches!(payload.opcode(), Opcode::Resume));
        let resume = payload.decode::<ResumePayload>().unwrap();
        assert_eq!(resume.token, "secret");
        assert_eq!(resume.session_id, "session");
        assert_eq!(resume.seq, 42);
    }

    #[test]
    fn resume_url_is_restricted_and_protocol_query_is_authoritative() {
        let url = normalize_resume_gateway_url(
            "wss://gateway-us-west1.discord.gg/stale?encoding=etf&v=9#fragment",
        )
        .unwrap();

        assert_eq!(url.scheme(), "wss");
        assert_eq!(url.host_str(), Some("gateway-us-west1.discord.gg"));
        assert_eq!(url.path(), "/");
        assert_eq!(url.query(), Some("encoding=json&v=10"));
        assert!(url.fragment().is_none());
    }

    #[test]
    fn resume_url_rejects_credential_exfiltration_targets() {
        for url in [
            "ws://gateway.discord.gg",
            "wss://gateway.discord.gg.evil.example",
            "wss://user@gateway.discord.gg",
            "wss://gateway.discord.gg:8443",
        ] {
            assert!(normalize_resume_gateway_url(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn resumed_transport_reuses_application_continuity() {
        let continuity = GatewayContinuity::default();
        let gateway = General::from_continuity(continuity.clone(), true);

        assert!(continuity.belongs_to(&gateway));
        assert!(gateway.awaiting_resumed());
        gateway.mark_resumed();
        assert!(!gateway.awaiting_resumed());
    }
}
