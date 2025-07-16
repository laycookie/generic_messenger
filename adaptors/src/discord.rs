use std::{
    fmt::Debug,
    pin::Pin,
    sync::{Arc, RwLock, Weak},
    time::Duration,
};

use async_trait::async_trait;
use async_tungstenite::async_std::connect_async;
use futures::lock::Mutex;
use futures_locks::RwLock as RwLockAwait;

use crate::{Messanger, MessangerQuery, ParameterizedMessangerQuery, Socket};
use crate::{VC, discord::websocket::DiscordSocket};

pub mod json_structs;
pub mod rest_api;
pub mod websocket;

type HeartBeatFuture = Pin<Box<dyn Future<Output = Option<()>> + Send>>;
pub struct Discord {
    // Metadata
    token: String, // TODO: Make it secure
    intents: u32,
    // Owned data
    socket: Mutex<Option<DiscordSocket>>,
    heart_beat_future: Arc<Mutex<Option<HeartBeatFuture>>>,
    heart_beat_interval: RwLockAwait<Option<Duration>>,
    // Cache
    profile: RwLockAwait<Option<json_structs::Profile>>,
    dms: RwLock<Vec<json_structs::Channel>>,
    guilds: RwLock<Vec<json_structs::Guild>>,
}

impl Discord {
    pub fn new(token: &str) -> Self {
        Discord {
            token: token.into(),
            intents: 161789, // 32767,
            socket: None.into(),
            heart_beat_interval: RwLockAwait::new(None),
            heart_beat_future: Mutex::new(None).into(),
            profile: RwLockAwait::new(None),
            dms: RwLock::new(Vec::new()),
            guilds: RwLock::new(Vec::new()),
        }
    }
    fn id(&self) -> String {
        self.name().to_owned() + &self.token
    }
    fn name(&self) -> &'static str {
        "Discord"
    }
}

impl Debug for Discord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discord").finish()
    }
}

#[async_trait]
impl Messanger for Discord {
    fn id(&self) -> String {
        self.id()
    }
    // === Unify a bit ===
    fn name(&self) -> &'static str {
        self.name()
    }
    fn auth(&self) -> String {
        self.token.clone()
    }
    fn query(&self) -> Option<&dyn MessangerQuery> {
        Some(self)
    }
    fn param_query(&self) -> Option<&dyn ParameterizedMessangerQuery> {
        Some(self)
    }
    async fn socket(self: Arc<Self>) -> Option<Weak<dyn Socket + Send + Sync>> {
        let mut socket = self.socket.lock().await;

        if socket.is_none() {
            let gateway_url = "wss://gateway.discord.gg/?encoding=json&v=9";
            let (stream, response) = connect_async(gateway_url)
                .await
                .expect("Failed to connect to Discord gateway");

            println!("Response HTTP code: {}", response.status());

            *socket = Some(DiscordSocket::new(stream));
        };
        Some(Arc::<Discord>::downgrade(&self) as Weak<dyn Socket + Send + Sync>)
    }
    async fn vc(&self) -> Option<&dyn VC> {
        Some(self)
    }
}
