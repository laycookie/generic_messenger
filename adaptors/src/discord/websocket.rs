use async_trait::async_trait;
use futures::poll;
use futures_timer::Delay;
use serde_json::json;
use serde_repr::Deserialize_repr;
use std::marker::PhantomData;
use std::pin::Pin;
use std::time::Duration;

use crate::VC;
use crate::types::{Chan, Identifier};

use super::Discord;

// Implementation of:
// https://discord.com/developers/docs/events/gateway

/// <https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-opcodes>
/// <https://docs.discord.food/topics/opcodes-and-status-codes#gateway-opcodes>
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

pub(in crate::discord) struct HeartBeatingData {
    version: Option<u8>,
    duration: Duration,
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
}
impl HeartBeatingData {
    pub(super) fn new(duration: Duration, version: Option<u8>) -> Self {
        Self {
            version,
            duration,
            future: Box::pin(Delay::new(duration)),
        }
    }
    pub(super) fn version(&self) -> Option<u8> {
        self.version
    }
    pub(super) async fn is_beat_time(&mut self) -> bool {
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

#[derive(Debug, Clone)]
pub(super) struct NoData;
impl AwaitingSession for NoData {
    type Next = SessionData;
}
impl AwaitingEndpoint for NoData {
    type Next = EndpointData;
}

#[derive(Debug, Clone)]
pub(super) struct EndpointData;
impl AwaitingSession for EndpointData {
    type Next = AllData;
}

#[derive(Debug, Clone)]
pub(super) struct SessionData;
impl AwaitingEndpoint for SessionData {
    type Next = AllData;
}

#[derive(Debug, Clone)]
pub(super) struct AllData;

#[derive(Debug, Clone)]
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
    fn clear(self) -> VCLocation<NoData> {
        VCLocation {
            location_id: self.location_id,
            session_id: self.session_id,
            token: self.token,
            endpoint: self.endpoint,
            _status: PhantomData,
        }
    }
}

// TODO: Get rid of this, or of the noodles above.
#[derive(Debug, Default)]
pub(super) enum VCLoc {
    #[default]
    None,
    AwaitingData(VCLocation<NoData>),
    AwaitingSession(VCLocation<EndpointData>),
    AwaitingEndpoint(VCLocation<SessionData>),
    Ready(VCLocation<AllData>),
}
impl VCLoc {
    pub(super) fn clear(&mut self) {
        match self {
            VCLoc::Ready(vc_location) => *self = VCLoc::AwaitingData(vc_location.clone().clear()),
            _ => todo!(),
        };
    }
    pub(super) fn insert_endpoint(&mut self, endpoint: String, token: String) {
        match self {
            VCLoc::AwaitingData(vc_location) => {
                *self = VCLoc::AwaitingSession(vc_location.clone().insert_endpoint(endpoint, token))
            }
            VCLoc::AwaitingEndpoint(vc_location) => {
                *self = VCLoc::Ready(vc_location.clone().insert_endpoint(endpoint, token))
            }
            VCLoc::Ready(vc_location) => {
                vc_location.endpoint = endpoint;
                vc_location.token = token;
            }
            _ => panic!(),
        };
    }
    pub(super) fn insert_session(&mut self, session_id: String) {
        match self {
            VCLoc::AwaitingData(vc_location) => {
                *self = VCLoc::AwaitingEndpoint(vc_location.clone().insert_session(session_id))
            }
            VCLoc::AwaitingSession(vc_location) => {
                *self = VCLoc::Ready(vc_location.clone().insert_session(session_id))
            }
            VCLoc::Ready(vc_location) => {
                println!(
                    "Session id being replaced from: {}\n   To: {}",
                    vc_location.session_id, session_id
                );
                vc_location.session_id = session_id;
            }
            _ => panic!("Self:{:#?}\nDesc:{:#?}", &self, session_id),
        };
    }
}

#[async_trait]
impl VC for Discord {
    async fn connect<'a>(&'a self, location: &Identifier<Chan>) {
        let mut socket = self.socket.lock().await;

        let channels_map = self.channel_id_mappings.read().await;
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
        socket
            .gateway_websocket
            .as_mut()
            .unwrap()
            .send(payload.to_string().into())
            .await
            .unwrap();
    }

    async fn disconnect<'a>(&'a self, location: &Identifier<Chan>) {
        println!("Disconecting");
        let mut socket = self.socket.lock().await;
        println!("Locked");

        let channels_map = self.channel_id_mappings.read().await;
        let channel = channels_map.get(location.get_id()).unwrap();

        let payload = json!({
            "op": Opcode::VoiceStateUpdate as u8,
            "d": {
                "guild_id": channel.guild_id,
                "channel_id": None::<String>,
                "self_mute": false,
                "self_deaf": false
              }
        });

        println!("Removing vc data");
        socket.nuke_vc_gateway();

        socket
            .gateway_websocket
            .as_mut()
            .unwrap()
            .send(payload.to_string().into())
            .await
            .unwrap();
    }
}
