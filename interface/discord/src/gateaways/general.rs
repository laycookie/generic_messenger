use std::{
    error::Error,
    io, mem,
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use async_tungstenite::{
    WebSocketStream,
    async_std::{ConnectStream, connect_async},
    tungstenite::Message as WebsocketMessage,
};
use facet::Facet;
use facet_pretty::FacetPretty;
use futures::StreamExt;
use messenger_interface::{
    interface::{SocketEvent, Voice},
    types::{Identifier, Message as GlobalMessage, Place, Room},
};
use surf::http::convert::json;
use tracing::{error, info, warn};

use crate::{
    Discord,
    api_types::{self, Message},
    gateaways::{
        Gateaway, GatewayPayload, HeartBeatingData, deserialize_event,
        voice::{Endpoint, VoiceGateawayState},
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

#[derive(Debug, Facet)]
pub struct ServerUpdatePayload {
    token: String,
    guild_id: Option<String>,
    channel_id: Option<String>,
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

trait GateawayStream<Op> {
    async fn next_gateaway_payload(&mut self) -> GatewayPayload<Op>;
}

impl GateawayStream<Opcode> for WebSocketStream<ConnectStream> {
    async fn next_gateaway_payload(&mut self) -> GatewayPayload<Opcode> {
        match self.next().await.unwrap().unwrap() {
            WebsocketMessage::Text(utf8_bytes) => {
                facet_json::from_str::<GatewayPayload<Opcode>>(&utf8_bytes).unwrap()
            }
            _ => todo!(),
        }
    }
}

pub struct General;
impl Gateaway<General> {
    const GATEWAY_URL: &str = "wss://gateway.discord.gg/?encoding=json&v=9";
    pub async fn new(discord: &Discord) -> Result<Self, Box<dyn Error + Sync + Send>> {
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
            .await
            .expect("Failed to send identify payload");

        Ok(Self {
            websocket: gateway_websocket,
            heart_beating: HeartBeatingData::new(heart_beating_duration),
            last_sequence_number: None,
            type_specific_data: General,
        })
    }

    pub fn fetch_event(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<GatewayPayload<Opcode>, Box<dyn std::error::Error + Send + Sync>>> {
        match self.websocket.poll_next_unpin(cx)? {
            Poll::Ready(Some(event)) => Poll::Ready(deserialize_event::<Opcode>(&event)),
            Poll::Ready(None) => Poll::Ready(Err("Stream ended".into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl GatewayPayload<Opcode> {
    pub(super) async fn exec(
        self,
        discord: &Discord,
    ) -> Result<SocketEvent, Box<dyn std::error::Error + Send + Sync>> {
        let mut gateaway = discord.gateaway.lock().await;
        if let Some(gateaway) = gateaway.as_mut()
            && let Some(s) = self.s
        {
            gateaway.last_sequence_number = Some(s);
        };
        drop(gateaway);

        match self.op {
            Opcode::Hello => {
                // Discord sends Hello when the connection is established (and sometimes on resume flows).
                // We already handle heartbeat scheduling elsewhere, so for now we can ignore it safely.
                //
                // TODO(discord-migration): correctly handle Hello/resume/reconnect flows and surface
                // `SocketEvent::Disconnected` when appropriate.
                return Ok(SocketEvent::Skip);
            }
            Opcode::Dispatch => {
                let Some(event_name) = self.t.as_ref() else {
                    warn!("Dispatch opcode received without an event type (t)");
                    return Ok(SocketEvent::Skip);
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

                        let mut voice_gateaway = discord.voice_gateaway.lock().await;
                        voice_gateaway.insert_session_id(voice_state.session_id);
                    }
                    GatewayEvent::VoiceServerUpdate => {
                        let server_update = facet_value::from_value::<ServerUpdatePayload>(self.d)?;

                        let mut voice_gateaway = discord.voice_gateaway.lock().await;
                        voice_gateaway.insert_endpoint(Endpoint::new(
                            server_update.endpoint.unwrap(),
                            server_update.token,
                        ));

                        let profile = discord.profile.read().await;
                        let profile = profile.as_ref();
                        let user_id = profile.unwrap().id.as_str();

                        let vc_location = match server_update.guild_id {
                            Some(guild_id) => guild_id,
                            None => server_update.channel_id.unwrap(),
                        };

                        *voice_gateaway = match mem::take(voice_gateaway.as_mut())
                            .connect(user_id, &vc_location)
                            .await
                        {
                            Ok(state) => state,
                            Err(err) => {
                                error!("{err:?}");
                                VoiceGateawayState::AwaitingData
                            }
                        };
                    }
                    GatewayEvent::MessageCreate => {
                        let message = facet_value::from_value::<Message>(self.d)?;

                        let channel_id_hash =
                            Discord::discord_id_to_internal_id(message.channel_id.as_str());
                        let msg_id_hash = Discord::discord_id_to_internal_id(message.id.as_str());

                        return Ok(SocketEvent::MessageCreated {
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
                        return Ok(SocketEvent::ChannelCreated {
                            r#where: channel
                                .guild_id
                                .as_deref()
                                .map(|guild_id| Discord::identifier_generator(guild_id, ())),
                            room: Discord::identifier_generator(&channel.id, room_data),
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
        Ok(SocketEvent::Skip)
    }
}
