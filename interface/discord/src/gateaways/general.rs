use std::{
    error::Error,
    io,
    sync::{OnceLock, atomic::Ordering},
    time::Duration,
};

use async_tungstenite::{
    WebSocketStream,
    async_std::{ConnectStream, connect_async},
    tungstenite::Message as WebsocketMessage,
};
use facet::Facet;
use facet_pretty::FacetPretty;
use futures::StreamExt;
use messenger_interface::{
    interface::{QueryEvent, TextEvent},
    types::{Identifier, Message as GlobalMessage},
};
use surf::http::convert::json;
use tracing::{error, info, warn};

use crate::{
    Discord, InnerDiscord, Owned,
    api_types::{self, Message, SNOWFLAKE},
    gateaways::{
        Gateaway, GateawayStream, GatewayPayload, HeartBeatingData,
        voice::{Endpoint, VoiceGateaway},
    },
};

// Implementation of:
// https://discord.com/developers/docs/events/gateway

/// <https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-opcodes>
/// <https://docs.discord.food/topics/opcodes-and-status-codes#gateway-opcodes>
#[derive(Debug, Facet)]
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
#[derive(Debug, Facet)]
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
}

#[derive(Facet)]
struct ReadyPayload {
    ssrc: u64,
    ip: String,
    port: u64,
    modes: Vec<String>,
    experiments: Vec<String>,
    // streams: Vec<?>
}

#[derive(Facet)]
struct HelloPayload {
    heartbeat_interval: u64,
    _trace: Vec<String>,
}

/// <https://docs.discord.com/developers/events/gateway-events#voice-server-update>
#[derive(Debug, Facet)]
pub struct VoiceServerUpdatePayload {
    token: String,
    guild_id: Option<SNOWFLAKE>,
    endpoint: Option<String>,
}

/// <https://docs.discord.food/resources/voice#voice-state-structure>
#[derive(Facet)]
struct VoiceStatePayload {
    guild_id: Option<String>,
    channel_id: String,
    lobby_id: Option<String>,
    user_id: String,
    //member: Vec<?>
    session_id: String,
    deaf: bool,
    mute: bool,
    self_deaf: bool,
    self_mute: bool,
    self_stream: Option<bool>,
    self_video: bool,
    suppress: bool,
    // request_to_speak_timestamp: ?
    discoverable: Option<bool>,
    user_volume: Option<f32>,
}

///https://docs.discord.food/resources/presence#session-object
#[derive(Facet)]
struct SessionObjectPayload {
    session_id: String,
    // client_info: ?
    status: String,
    // activities: Vec<?>,
    // hidden_activities: Vec<?>,
    active: Option<bool>,
}

pub struct General {
    pub voice: VoiceGateaway,
}

impl Gateaway<General> {
    const GATEWAY_URL: &str = "wss://gateway.discord.gg/?encoding=json&v=9";
    pub async fn new<T>(discord: &InnerDiscord<T>) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let (mut gateway_websocket, _) = connect_async(Self::GATEWAY_URL).await?;

        // First event send by discord has to be Hello event according to
        // https://docs.discord.food/topics/gateway#connections
        let hello_event = gateway_websocket.next_gateaway_payload().await;

        let Opcode::Hello = hello_event.op else {
            return Err(Box::new(io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Expected to recive hello event as the first event",
            )));
        };

        let hello_d = facet_value::from_value::<HelloPayload>(hello_event.d)?;
        let heart_beating_duration = Duration::from_millis(hello_d.heartbeat_interval);

        // TODO: People are dumb (me included) so later maybe check the token for trailing spaces
        // the REST_API already filters for them on Discords end anyways (presumably or at least
        // the ones trailing at the end)
        let token = discord.token.unsecure();
        gateway_websocket
            .send(WebsocketMessage::Text(
                json!({
                    "op": Opcode::Identify as u8,
                    "d": {
                        "token": token,
                        "intents": discord.intents,
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
        info!("Token send: {token:?}");

        Ok(Self {
            websocket: crate::gateaways::Websocket::new(gateway_websocket),
            heart_beating: HeartBeatingData::new(heart_beating_duration).into(),
            last_sequence_number: OnceLock::new(),
            type_specific_data: General {
                voice: VoiceGateaway::default(),
            },
        })
    }
}

impl GatewayPayload<Opcode> {
    pub(super) async fn exec<T>(
        self,
        discord: &InnerDiscord<T>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let gateaway = discord.gateaway.load();
        let Some(gateaway) = gateaway.as_ref() else {
            return Err("TODO".into());
        };

        if let Some(s) = self.s {
            gateaway
                .last_sequence_number
                .get_or_init(|| s.into())
                .store(s, Ordering::Relaxed);
        };

        match self.op {
            Opcode::Hello => {}
            Opcode::Dispatch => {
                let Some(event_name) = self.t.as_ref() else {
                    warn!("Dispatch opcode received without an event type (t)");
                    return Ok(());
                };
                info!("Dispatch event: {event_name:?}");
                // https://discord.com/developers/docs/events/gateway-events#receive-events
                match event_name {
                    GatewayEvent::Ready => {
                        info!("importing data");
                    }
                    GatewayEvent::SessionsReplace => {
                        info!("Session replace");
                        let session = facet_value::from_value::<Vec<SessionObjectPayload>>(self.d)?;
                        info!("{}", session.pretty());
                    }
                    GatewayEvent::VoiceStateUpdate => {
                        let voice_state = facet_value::from_value::<VoiceStatePayload>(self.d)?;

                        gateaway
                            .voice
                            .insert_session_id(voice_state.session_id)
                            .await;
                    }
                    GatewayEvent::VoiceServerUpdate => {
                        let server_update =
                            facet_value::from_value::<VoiceServerUpdatePayload>(self.d)?;

                        gateaway
                            .voice
                            .insert_endpoint(Endpoint::new(
                                server_update.endpoint.unwrap(),
                                server_update.token,
                            ))
                            .await;

                        let profile = discord.profile.read().await;
                        let profile = profile.as_ref();
                        let user_id = profile.unwrap().id;

                        match gateaway.voice.connect(user_id).await {
                            Ok(_) => (),
                            Err(err) => {
                                error!("{err:?}");
                            }
                        };
                    }
                    GatewayEvent::MessageCreate => {
                        let message = facet_value::from_value::<Message>(self.d)?;

                        let channel_id_hash = message.channel_id;
                        let msg_id_hash = message.id;

                        discord.text_events.push(TextEvent::MessageCreated {
                            room: Identifier::new(channel_id_hash, ()),
                            message: Identifier::new(
                                msg_id_hash,
                                GlobalMessage {
                                    text: message.content,
                                    reactions: Vec::new(),
                                },
                            ),
                        });
                    }
                    GatewayEvent::ChannelCreate => {
                        let channel = facet_value::from_value::<api_types::Channel>(self.d)?;

                        let (_name, _icon, room_data) = channel.to_room_data().await;
                        discord.query_events.push(QueryEvent::ChannelCreated {
                            r#where: channel
                                .guild_id
                                .map(|guild_id| Discord::identifier_generator(guild_id, ())),
                            room: Discord::identifier_generator(channel.id, room_data),
                        });
                    }
                    _ => warn!("Unknown event_name received: {event_name:?}",),
                }
            }
            Opcode::HeartbeatAck => {
                info!("HeartbeatAck");
            }
            _ => {
                warn!("Unkown opcode recived: {:?}", self.op)
            }
        };
        Ok(())
    }
}
