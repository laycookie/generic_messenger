use std::{
    collections::HashMap,
    error::Error,
    hash::{DefaultHasher, Hash, Hasher},
    sync::{Arc, Mutex, Weak},
};

use async_trait::async_trait;
use audio::AudioMixer;
use futures::lock::Mutex as AsyncMutex;
use futures_locks::RwLock as RwLockAwait;
use messaging_interface::{
    interface::{
        MessamgerError, Messanger, MessangerQuery, ParameterizedMessangerQuery, Socket, VC,
    },
    types::{ID, Identifier},
};
use secure_string::SecureString;

use crate::gateaways::{general::Gateaway, voice::VoiceGateawayState};

mod api_types;
mod downloaders;
mod gateaways;
mod query;

struct ChannelID {
    guild_id: Option<String>,
    id: String,
}

type GuildID = String;
type MessageID = String;

pub struct Discord {
    // Metadata
    token: SecureString,
    intents: u32,
    // gateaways
    gateaway: AsyncMutex<Option<Gateaway>>,
    voice_gateaway: AsyncMutex<VoiceGateawayState>,
    // Cache (External IDs, to internal)
    profile: RwLockAwait<Option<api_types::Profile>>,
    channel_id_mappings: RwLockAwait<HashMap<ID, ChannelID>>,
    guild_id_mappings: RwLockAwait<HashMap<ID, GuildID>>,
    msg_data: RwLockAwait<HashMap<ID, MessageID>>,
}
impl Discord {
    pub fn new(token: &str) -> Self {
        Discord {
            token: token.into(),
            intents: 194557,
            gateaway: None.into(),
            voice_gateaway: VoiceGateawayState::default().into(),
            profile: RwLockAwait::new(None),
            guild_id_mappings: RwLockAwait::new(HashMap::new()),
            channel_id_mappings: RwLockAwait::new(HashMap::new()),
            msg_data: RwLockAwait::new(HashMap::new()),
        }
    }
    fn discord_id_to_internal_id(id: &str) -> u32 {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        hasher.finish() as u32
    }
    fn identifier_generator<D>(id: &str, data: D) -> Identifier<D> {
        Identifier {
            id: Discord::discord_id_to_internal_id(id),
            data,
        }
    }
}

#[async_trait]
impl Messanger for Discord {
    fn id(&self) -> String {
        self.name().to_owned() + self.token.unsecure()
    }
    fn name(&self) -> &'static str {
        "Discord"
    }
    fn auth(&self) -> String {
        self.token.clone().into_unsecure()
    }

    fn query(&self) -> Result<&dyn MessangerQuery, MessamgerError> {
        Ok(self)
    }
    fn param_query(&self) -> Result<&dyn ParameterizedMessangerQuery, MessamgerError> {
        Ok(self)
    }
    async fn socket(
        self: Arc<Self>,
    ) -> Result<Weak<dyn Socket + Send + Sync>, Box<dyn Error + Sync + Send>> {
        let mut socket = self.gateaway.lock().await;

        // Check if already connected to discord gateaway
        if socket.is_some() {
            // return Ok(self.clone());
            return Ok(Arc::<Discord>::downgrade(&self));
        };

        // Connect to discord gateaway
        let gateaway = Gateaway::new(&self).await?;

        *socket = Some(gateaway);

        // Ok(self.clone())
        Ok(Arc::<Discord>::downgrade(&self))
    }

    fn vc(&self) -> Result<&dyn VC, Box<dyn Error + Sync + Send>> {
        Ok(self)
    }
}
