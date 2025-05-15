use std::{
    fmt::Debug,
    net::TcpStream,
    sync::{Mutex, RwLock},
};

use async_tungstenite::WebSocketStream;
use uuid::Uuid;

use crate::{Messanger, MessangerQuery, ParameterizedMessangerQuery};

pub mod json_structs;
pub mod rest_api;
pub mod websocket;

pub struct Discord {
    uuid: Uuid,
    token: String, // TODO: Make it secure
    intents: u32,

    socket: Mutex<Option<WebSocketStream<TcpStream>>>,
    // Data
    dms: RwLock<Vec<json_structs::Channel>>,
}

impl Discord {
    pub fn new(token: &str) -> Discord {
        Discord {
            uuid: Uuid::new_v4(),
            token: token.into(),
            intents: 161789, // 32767,
            socket: None.into(),

            dms: RwLock::new(Vec::new()),
        }
    }
}

impl Debug for Discord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discord").finish()
    }
}

impl Messanger for Discord {
    fn name(&self) -> String {
        "Discord".into()
    }
    fn auth(&self) -> String {
        self.token.clone()
    }
    fn uuid(&self) -> Uuid {
        self.uuid
    }
    fn query(&self) -> Option<&dyn MessangerQuery> {
        Some(self)
    }
    fn param_query(&self) -> Option<&dyn ParameterizedMessangerQuery> {
        Some(self)
    }
    fn socket(&self) -> Option<&dyn crate::Socket> {
        Some(self)
    }
}
