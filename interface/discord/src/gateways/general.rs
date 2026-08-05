use std::{error::Error, marker::PhantomData, sync::atomic::Ordering, time::Duration};

use async_tungstenite::smol::connect_async;
use facet::Facet;
use num_enum::TryFromPrimitive;
use surf::http::convert::json;
use tracing::info;

use self::payloads::HelloPayload;
use crate::gateways::{
    Gateway, GatewayFrame, GatewayState, Handshake, Handshaking, Heartbeat, Websocket,
    expect_hello, media::MediaGateway,
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

pub(crate) struct GeneralHandshake {
    identify: GatewayFrame<Opcode>,
}

impl GatewayState<General> for Handshaking {
    type Data = GeneralHandshake;
}

pub struct General {
    pub voice: MediaGateway,
    /// Lossless interface-event routing for this gateway session. Queue depth
    /// warnings expose consumers that stop draining without discarding state.
    pub(crate) events: EventHub,
    /// Per-fetch gateway event recorder. Lives on the gateway so all recorded
    /// state drops on disconnect. See `crate/messenger_interface/docs/races.md`.
    pub recording: recording::Recorder,
}

impl Gateway<General, Handshaking> {
    const GATEWAY_URL: &str = "wss://gateway.discord.gg/?encoding=json&v=9";
    pub async fn new(
        token: &SecureString,
        intents: u32,
        capabilities: u32,
    ) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let (gateway_websocket, _) = connect_async(Self::GATEWAY_URL).await?;
        let websocket = Websocket::new(gateway_websocket);

        // TODO: People are dumb (me included) so later maybe check the token for trailing spaces
        // the REST_API already filters for them on Discords end anyways (presumably or at least
        // the ones trailing at the end)
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

        Ok(Self {
            websocket,
            state: GeneralHandshake { identify },
            _marker: PhantomData,
        })
    }
}

impl Handshake for Gateway<General, Handshaking> {
    type Output = Gateway<General>;

    async fn handshake(self) -> Result<Self::Output, Box<dyn Error + Send + Sync>> {
        let Gateway {
            websocket, state, ..
        } = self;

        let hello = expect_hello::<_, HelloPayload>(
            &websocket,
            |opcode| matches!(opcode, Opcode::Hello),
            "gateway",
        )
        .await?;

        let GeneralHandshake { identify } = state;
        websocket.send(identify.into_message()).await?;

        Ok(websocket.into_gateway(
            Duration::from_millis(hello.heartbeat_interval),
            General {
                voice: MediaGateway::default(),
                events: EventHub::default(),
                recording: recording::Recorder::default(),
            },
        ))
    }
}

impl Heartbeat for Gateway<General> {
    /// <https://docs.discord.food/gateway/using-gateway#sending-heartbeats>
    async fn heartbeat(&self) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.state.heartbeat_timer.lock().await.wait().await;
        let sequence = self
            .state
            .last_sequence_number
            .get()
            .map(|sequence| sequence.load(Ordering::Relaxed));
        self.websocket
            .send(
                json!({
                "op": Opcode::Heartbeat as u8,
                "d": sequence,
                })
                .to_string()
                .into(),
            )
            .await?;
        info!(?sequence, "Gateway heartbeat sent");
        Ok(())
    }
}
