use std::{
    collections::HashMap,
    fmt::Debug,
    hash::{DefaultHasher, Hash, Hasher},
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use async_tungstenite::async_std::connect_async;
use futures::lock::Mutex;
use futures_locks::RwLock as RwLockAwait;

use crate::{
    Messanger, MessangerQuery, ParameterizedMessangerQuery, Socket,
    discord::json_structs::{Channel, Guild, Message},
    types::{ID, Identifier},
};
use crate::{VC, discord::websocket::DiscordSockets};

pub mod json_structs;
pub mod main_socket;
pub mod rest_api;
pub mod vc_socket;
pub mod websocket;

pub struct Discord {
    // Metadata
    token: String, // TODO: Make it secure
    intents: u32,
    // Owned data
    socket: Mutex<Option<DiscordSockets>>,
    // Cache
    profile: RwLockAwait<Option<json_structs::Profile>>,
    guild_data: RwLockAwait<HashMap<ID, Guild>>,
    channels_map: RwLockAwait<HashMap<ID, Channel>>,
    msg_data: RwLockAwait<HashMap<ID, Message>>,
}

impl Discord {
    pub fn new(token: &str) -> Self {
        Discord {
            token: token.into(),
            intents: 161789, // 32767,
            socket: None.into(),
            profile: RwLockAwait::new(None),
            guild_data: RwLockAwait::new(HashMap::new()),
            channels_map: RwLockAwait::new(HashMap::new()),
            msg_data: RwLockAwait::new(HashMap::new()),
        }
    }
    fn id(&self) -> String {
        self.name().to_owned() + &self.token
    }
    fn name(&self) -> &'static str {
        "Discord"
    }
    fn discord_id_to_u32_id(id: &str) -> u32 {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        hasher.finish() as u32
    }
    fn identifier_generator<D>(id: &str, data: D) -> Identifier<D> {
        Identifier {
            neo_id: Discord::discord_id_to_u32_id(id),
            data,
        }
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

            *socket = Some(DiscordSockets::new(stream));
        };
        Some(Arc::<Discord>::downgrade(&self) as Weak<dyn Socket + Send + Sync>)
    }
    async fn vc(&self) -> Option<&dyn VC> {
        Some(self)
    }
}
