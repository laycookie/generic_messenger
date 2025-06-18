use std::{
    fmt::Debug,
    sync::{Arc, RwLock, Weak},
};

use async_trait::async_trait;
use async_tungstenite::async_std::connect_async;
use futures::lock::Mutex;

use crate::discord::websocket::DiscordSocket;
use crate::{Messanger, MessangerQuery, ParameterizedMessangerQuery, Socket};

pub mod json_structs;
pub mod rest_api;
pub mod websocket;

pub struct Discord {
    // Metadata
    token: String, // TODO: Make it secure
    intents: u32,
    // Owned data
    socket: Mutex<DiscordSocket>,
    // Cache
    dms: RwLock<Vec<json_structs::Channel>>,
    guilds: RwLock<Vec<json_structs::Guild>>,
}

impl Discord {
    pub fn new(token: &str) -> Arc<dyn Messanger> {
        Arc::new(Arc::new(Discord {
            token: token.into(),
            intents: 161789, // 32767,
            socket: Mutex::new(DiscordSocket::new()),
            dms: RwLock::new(Vec::new()),
            guilds: RwLock::new(Vec::new()),
        }))
    }
    fn id(&self) -> String {
        String::from(self.name().to_owned() + &self.token)
    }
    fn name(&self) -> &'static str {
        "Discord".into()
    }
}

impl Debug for Discord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discord").finish()
    }
}

#[async_trait]
impl Messanger for Arc<Discord> {
    fn id(&self) -> String {
        (**self).id()
    }
    // === Unify a bit ===
    fn name(&self) -> &'static str {
        (**self).name()
    }
    fn auth(&self) -> String {
        self.token.clone()
    }
    fn query(&self) -> Option<&dyn MessangerQuery> {
        Some(&**self)
    }
    fn param_query(&self) -> Option<&dyn ParameterizedMessangerQuery> {
        Some(&**self)
    }
    async fn socket(&self) -> Option<Weak<dyn Socket + Send + Sync>> {
        let mut socket = self.socket.lock().await;

        if socket.websocket.is_none() {
            let gateway_url = "wss://gateway.discord.gg/?encoding=json&v=9";
            let (stream, response) = connect_async(gateway_url)
                .await
                .expect("Failed to connect to Discord gateway");

            println!("Response HTTP code: {}", response.status());

            socket.websocket = Some(stream);
        };
        Some(Arc::<Discord>::downgrade(&self) as Weak<dyn Socket + Send + Sync>)
    }
}
