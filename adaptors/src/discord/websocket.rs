use async_std::net::UdpSocket;
use async_trait::async_trait;
use async_tungstenite::WebSocketStream;
use async_tungstenite::async_std::ConnectStream;
use async_tungstenite::tungstenite::{Error, Message};
use futures::{FutureExt, Stream, StreamExt, pending, poll};
use futures_timer::Delay;
use serde::Deserialize;
use serde_json::json;
use serde_repr::Deserialize_repr;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, SystemTime};

use crate::discord::vc_socket::VCOpcode;
use crate::types::{Chan, Identifier};
use crate::{Socket, SocketEvent, VC};

use super::Discord;

// Implementation of:
// https://discord.com/developers/docs/events/gateway

/// https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-opcodes
/// https://docs.discord.food/topics/opcodes-and-status-codes#gateway-opcodes
#[repr(u8)]
#[derive(Debug, Deserialize_repr)]
enum Opcode {
    Dispatch = 0,
    Heartbeat = 1,
    Identify = 2,
    PresenceUpdate = 3,
    VoiceStateUpdate = 4,
    Hello = 10,
    HeartbeatAck = 11,
    CallConnect = 13,
}

// https://discord.com/developers/docs/events/gateway-events#payload-structure
#[derive(Debug, Deserialize)]
pub(super) struct GateawayPayload<Opcode> {
    pub(super) op: Opcode,
    // Event type
    pub(super) t: Option<String>,
    // Sequence numbers
    pub(super) _s: Option<usize>,
    // data
    pub(super) d: serde_json::Value,
}

pub(super) struct HeartBeatingData {
    duration: Duration,
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
}
impl HeartBeatingData {
    pub(super) fn new(duration: Duration) -> Self {
        Self {
            duration,
            future: Box::pin(Delay::new(duration)),
        }
    }
    async fn is_beat_time(&mut self) -> bool {
        if poll!(&mut self.future).is_ready() {
            self.future = Box::pin(Delay::new(self.duration));
            return true;
        }
        false
    }
}

pub(super) trait AwaitingSession {
    type Next;
}
pub(super) trait AwaitingEndpoint {
    type Next;
}

#[derive(Clone)]
pub(super) struct NoData;
impl AwaitingSession for NoData {
    type Next = SessionData;
}
impl AwaitingEndpoint for NoData {
    type Next = EndpointData;
}

#[derive(Clone)]
pub(super) struct EndpointData;
impl AwaitingSession for EndpointData {
    type Next = AllData;
}

#[derive(Clone)]
pub(super) struct SessionData;
impl AwaitingEndpoint for SessionData {
    type Next = AllData;
}

pub(super) struct AllData;

#[derive(Clone)]
pub(super) struct VCLocation<DataFetched> {
    // For location_id it is guild_id for when we are in guilds,
    // and channel_id when in DMs.
    location_id: String,
    session_id: String,
    token: String,
    endpoint: String,
    _status: PhantomData<DataFetched>,
}

impl VCLocation<NoData> {
    pub(super) fn new(location_id: String) -> Self {
        Self {
            location_id,
            session_id: String::new(),
            token: String::new(),
            endpoint: String::new(),
            _status: PhantomData,
        }
    }
}
impl<Data: AwaitingEndpoint> VCLocation<Data> {
    pub(super) fn insert_endpoint(self, endpoint: String, token: String) -> VCLocation<Data::Next> {
        VCLocation {
            location_id: self.location_id,
            session_id: self.session_id,
            token,
            endpoint,
            _status: PhantomData,
        }
    }
}
impl<Data: AwaitingSession> VCLocation<Data> {
    pub(super) fn insert_session(self, session_id: String) -> VCLocation<Data::Next> {
        VCLocation {
            location_id: self.location_id,
            session_id,
            token: self.token,
            endpoint: self.endpoint,
            _status: PhantomData,
        }
    }
}

impl VCLocation<AllData> {
    pub(super) fn get_location_id(&self) -> &str {
        self.location_id.as_str()
    }
    pub(super) fn get_endpoint(&self) -> &str {
        self.endpoint.as_str()
    }
    pub(super) fn get_token(&self) -> &str {
        self.token.as_str()
    }
    pub(super) fn get_session(&self) -> &str {
        self.session_id.as_str()
    }
}

// TODO: Get rid of this, or of the noodles above.
#[derive(Default)]
pub(super) enum VCLoc {
    #[default]
    None,
    AwaitingData(VCLocation<NoData>),
    AwaitingSession(VCLocation<EndpointData>),
    AwaitingEndpoint(VCLocation<SessionData>),
    Ready(VCLocation<AllData>),
}
impl VCLoc {
    pub(super) fn insert_endpoint(&mut self, endpoint: String, token: String) {
        match self {
            VCLoc::AwaitingData(vclocation) => {
                *self = VCLoc::AwaitingSession(vclocation.clone().insert_endpoint(endpoint, token))
            }
            VCLoc::AwaitingEndpoint(vclocation) => {
                *self = VCLoc::Ready(vclocation.clone().insert_endpoint(endpoint, token))
            }
            _ => panic!(),
        };
    }
    pub(super) fn insert_session(&mut self, session_id: String) {
        match self {
            VCLoc::AwaitingData(vclocation) => {
                *self = VCLoc::AwaitingEndpoint(vclocation.clone().insert_session(session_id))
            }
            VCLoc::AwaitingSession(vclocation) => {
                *self = VCLoc::Ready(vclocation.clone().insert_session(session_id))
            }
            _ => panic!(),
        };
    }
}

pub(super) struct DiscordSockets {
    // Main
    pub(super) websocket: WebSocketStream<ConnectStream>,
    pub(super) last_sequance_number: Option<usize>,
    pub(super) heart_beating: Option<HeartBeatingData>,

    // VC
    pub(super) vc_location: VCLoc,
    pub(super) vc_websocket: Option<WebSocketStream<ConnectStream>>,
    pub(super) vc_connection: Option<UdpSocket>,
    pub(super) vc_heart_beating: Option<HeartBeatingData>,
}
impl DiscordSockets {
    pub fn new(websocket: WebSocketStream<ConnectStream>) -> Self {
        DiscordSockets {
            websocket,
            heart_beating: None,
            last_sequance_number: None,
            vc_websocket: None,
            vc_connection: None,
            vc_location: VCLoc::None,
            vc_heart_beating: None,
        }
    }
}

// TODO: Think hard about this
impl Stream for Discord {
    type Item = SocketEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.next().poll_unpin(cx)
    }
}

#[async_trait]
impl Socket for Discord {
    async fn next(self: Arc<Self>) -> Option<SocketEvent> {
        let (event, vc_event) = loop {
            {
                let mut socket = self.socket.lock().await;
                let socket = socket.as_mut()?;

                // Main socket heartbeat
                if let Some(heart_beating_data) = &mut socket.heart_beating
                    && heart_beating_data.is_beat_time().await
                {
                    println!("BEAT");
                    socket
                        .websocket
                        .send(
                            json!({
                                    "op": Opcode::Heartbeat as u8,
                                    "d": socket.last_sequance_number,
                            })
                            .to_string()
                            .into(),
                        )
                        .await
                        .unwrap();
                }

                // VC socket heartbeat
                if let Some(heart_beating_data) = &mut socket.vc_heart_beating
                    && heart_beating_data.is_beat_time().await
                {
                    socket
                        .vc_websocket
                        .as_mut()
                        .unwrap()
                        .send(
                            // https://discord.com/developers/docs/topics/voice-connections#heartbeating-example-hello-payload
                            json!({
                                "op": VCOpcode::Heartbeat as u8,
                                "d": {
                                    "t": SystemTime::now(),
                                    "seq_ack": 10,
                                }
                            })
                            .to_string()
                            .into(),
                        )
                        .await
                        .unwrap();
                }

                // Pull VC event
                let vc_event = if let Some(socket) = socket.vc_websocket.as_mut()
                    && let Poll::Ready(Some(event)) = poll!(socket.next())
                {
                    Some(event)
                } else {
                    None
                };

                // Pull socket event
                let socket_event = match poll!(socket.websocket.next()) {
                    Poll::Ready(event) => event,
                    _ => None,
                };

                if [&socket_event, &vc_event].iter().any(|e| e.is_some()) {
                    break (socket_event, vc_event);
                };
            }
            pending!()
        };
        let mut socket = self.socket.lock().await;
        let discord_stream = socket.as_mut()?;

        if let Some(vc_event) = vc_event {
            Discord::vc_event_exec(deserialize_event(vc_event).unwrap(), discord_stream)
                .await
                .unwrap();
        }

        if let Some(event) = event {
            match self
                .event_exec(deserialize_event(event).unwrap(), discord_stream)
                .await
            {
                Ok(event) => return Some(event),
                Err(e) => {
                    eprintln!("{e}");
                    return None;
                }
            };
        }
        Some(SocketEvent::Skip)
    }
}

#[async_trait]
impl VC for Discord {
    async fn connect<'a>(&'a self, location: &Identifier<Chan>) {
        let mut socket = self.socket.lock().await;
        let socket = socket.as_mut().unwrap();
        let websocket = &mut socket.websocket;

        let channels_map = self.channels_map.read().await;
        let channel = channels_map.get(location.get_id()).unwrap();

        let payload = json!({
            "op": Opcode::VoiceStateUpdate as u8,
            "d": {
                "guild_id": channel.guild_id,
                "channel_id": channel.id,
                "self_mute": false,
                "self_deaf": false
              }
        });

        socket.vc_location = VCLoc::AwaitingData(VCLocation::new(
            channel.guild_id.clone().unwrap_or(channel.id.clone()),
        ));

        // socket.vc_location_id = Some(channel.guild_id.clone().unwrap_or(channel.id.clone()));
        websocket.send(payload.to_string().into()).await.unwrap();
    }
}

fn deserialize_event<Opcode: for<'a> Deserialize<'a>>(
    event: Result<Message, Error>,
) -> Result<GateawayPayload<Opcode>, Box<dyn std::error::Error>> {
    let json = match event? {
        Message::Text(text) => serde_json::from_str::<GateawayPayload<Opcode>>(&text).unwrap(),
        Message::Binary(_) => todo!(),
        Message::Frame(frame) => {
            return Err(format!("Frame: {frame:?}").into());
        }
        Message::Close(frame) => {
            return Err(format!("Close frame: {frame:?}").into());
        }
        Message::Ping(_) => todo!(),
        Message::Pong(_) => todo!(),
    };
    Ok(json)
}
