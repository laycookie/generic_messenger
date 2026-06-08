use std::{
    error::Error, io, pin::pin,
    sync::{Mutex, OnceLock, atomic::AtomicUsize},
    time::Duration,
};

use async_tungstenite::{async_std::connect_async, tungstenite::Message as WebsocketMessage};
use facet::Facet;
use futures::StreamExt;
use num_enum::TryFromPrimitive;
use surf::http::convert::json;
use tracing::debug;

use self::payloads::HelloPayload;
use crate::{
    InnerDiscord, UnitStruct,
    gateways::{
        Gateway, GatewayStreamReciver as _, HeartBeatingData, Websocket, voice::VoiceGateway,
    },
};

mod events;
pub(crate) mod payloads;
pub(crate) mod recording;

// Implementation of:
// https://discord.com/developers/docs/events/gateway

/// <https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-opcodes>
/// <https://docs.discord.food/topics/opcodes-and-status-codes#gateway-opcodes>
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
    Hello = 10,
    HeartbeatAck = 11,
    CallConnect = 13,
}

/// <https://discord.com/developers/docs/events/gateway-events#receive-events>
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
    VoiceStateUpdate,
    VoiceServerUpdate,
    MessageCreate,
    MessageUpdate,
    MessageDelete,
    MessageDeleteBulk,
    MessageReactionAdd,
    MessageReactionRemove,
}

pub struct General {
    pub voice: VoiceGateway,
    /// Number of in-flight REST calls currently recording. Non-zero
    /// gates whether event handlers acquire the state lock at all.
    /// Lives on the gateway so it drops automatically when the gateway
    /// disconnects — at that point REST becomes the unfiltered source
    /// of truth. See `crate/messenger_interface/docs/races.md`.
    pub recording_refs: AtomicUsize,
    /// Shared recording state: the event buffer plus enough bookkeeping
    /// to drop front slots no window still references. See `recording`.
    pub recorded: Mutex<recording::RecordingState>,
}

impl Gateway<General> {
    const GATEWAY_URL: &str = "wss://gateway.discord.gg/?encoding=json&v=9";
    pub async fn new<T: UnitStruct>(
        discord: &InnerDiscord<T>,
    ) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let (gateway_websocket, _) = connect_async(Self::GATEWAY_URL).await?;
        let websocket = Websocket::new(gateway_websocket);

        // First event send by discord has to be Hello event according to
        // https://docs.discord.food/topics/gateway#connections
        let hello_event = {
            let mut receiver = websocket.receiver.lock().await;
            pin!(receiver.filter_payload()).next().await
        }
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "gateway closed before receiving hello",
            )
        })?;

        let Opcode::Hello = hello_event.op else {
            return Err(Box::new(io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Expected to receive hello event as the first event",
            )));
        };

        let hello_d = facet_value::from_value::<HelloPayload>(hello_event.d)?;
        let heart_beating_duration = Duration::from_millis(hello_d.heartbeat_interval);

        // TODO: People are dumb (me included) so later maybe check the token for trailing spaces
        // the REST_API already filters for them on Discords end anyways (presumably or at least
        // the ones trailing at the end)
        let token = discord.token.unsecure();
        websocket
            .send(WebsocketMessage::Text(
                json!({
                    "op": Opcode::Identify as u8,
                    "d": {
                        "token": token,
                        "intents": discord.intents.bits(),
                        "capabilities": discord.capabilities.bits(),
                        "properties": {
                            "$os": "Linux",
                            "$browser": "Firefox",
                            "$device": ""
                        }
                    }
                })
                .to_string()
                .into(),
            ))
            .await?;
        debug!("Identify payload sent");

        Ok(Self {
            websocket,
            heart_beating: HeartBeatingData::new(heart_beating_duration).into(),
            last_sequence_number: OnceLock::new(),
            type_specific_data: General {
                voice: VoiceGateway::default(),
                recording_refs: AtomicUsize::new(0),
                recorded: Mutex::new(recording::RecordingState::default()),
            },
        })
    }
    pub async fn heartbeat(&self) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.heart_beating.lock().await.await_until_beat().await;
        self.websocket
            .send(
                json!({
                        "op": Opcode::Heartbeat as u8,
                        "d": self.last_sequence_number.get(),
                })
                .to_string()
                .into(),
            )
            .await
    }
}
