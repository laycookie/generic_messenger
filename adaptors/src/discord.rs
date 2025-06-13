use std::{
    fmt::Debug,
    sync::{Arc, RwLock, Weak},
};

use async_trait::async_trait;
use async_tungstenite::{
    WebSocketStream,
    async_std::{ConnectStream, connect_async},
};
use futures::lock::Mutex;
use uuid::Uuid;

use crate::{Messanger, MessangerQuery, ParameterizedMessangerQuery, Socket};

pub mod json_structs;
pub mod rest_api;
pub mod websocket;

pub struct Discord {
    // Metadata
    uuid: Uuid,
    token: String, // TODO: Make it secure
    intents: u32,

    // Owned data
    socket: Mutex<Option<WebSocketStream<ConnectStream>>>,
    // Cache
    dms: RwLock<Vec<json_structs::Channel>>,
}

impl Discord {
    pub fn new(token: &str) -> Arc<dyn Messanger> {
        Arc::new(Arc::new(Discord {
            uuid: Uuid::new_v4(),
            token: token.into(),
            intents: 161789, // 32767,

            socket: None.into(),

            dms: RwLock::new(Vec::new()),
        }))
    }
}

impl Debug for Discord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discord").finish()
    }
}

#[async_trait]
impl Messanger for Arc<Discord> {
    // === Unifi a bit ===
    fn name(&self) -> String {
        "Discord".into()
    }
    fn auth(&self) -> String {
        self.token.clone()
    }
    fn uuid(&self) -> Uuid {
        self.uuid
    }

    // ===
    fn query(&self) -> Option<&dyn MessangerQuery> {
        Some(&**self)
    }
    fn param_query(&self) -> Option<&dyn ParameterizedMessangerQuery> {
        Some(&**self)
    }

    async fn socket(&self) -> Option<Weak<dyn Socket + Send + Sync>> {
        let mut socket = self.socket.lock().await;

        if socket.is_none() {
            let gateway_url = "wss://gateway.discord.gg/?encoding=json&v=9";
            let (stream, response) = connect_async(gateway_url)
                .await
                .expect("Failed to connect to Discord gateway");

            println!("Response HTTP code: {}", response.status());

            *socket = Some(stream);
        };
        Some(Arc::<Discord>::downgrade(&self) as Weak<dyn Socket + Send + Sync>)
    }
}
