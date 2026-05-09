use std::{
    cell::Cell,
    collections::HashMap,
    marker::PhantomData,
    sync::{Arc, Weak},
};

use arc_swap::ArcSwapOption;
use asyncs_sync::Notify;
use bitflags::bitflags;
use crossbeam::queue::SegQueue;
use futures::{channel::oneshot, lock::Mutex as AsyncMutex};
use futures_locks::RwLock as RwLockAwait;
use messenger_interface::{
    interface::{AudioEvent, Messenger, QueryEvent, TextEvent, VoiceEvent},
    types::{ID, Identifier},
};
use secure_string::SecureString;
use simple_audio_channels::input::SampleConsumer;

use crate::{
    api_types::SNOWFLAKE,
    gateways::{Gateway, general::General},
};

mod api_types;
mod downloaders;
mod gateways;
mod query;

pub(crate) const DISCORD_API: &str = "https://discord.com/api/v10";

bitflags! {
    /// <https://discord.com/developers/docs/events/gateway#list-of-intents>
    struct Intents: u32 {
        const GUILDS                    = 1 << 0;
        const GUILD_MODERATION          = 1 << 2;
        const GUILD_EXPRESSIONS         = 1 << 3;
        const GUILD_INTEGRATIONS        = 1 << 4;
        const GUILD_WEBHOOKS            = 1 << 5;
        const GUILD_INVITES             = 1 << 6;
        const GUILD_VOICE_STATES        = 1 << 7;
        const GUILD_PRESENCES           = 1 << 8;
        const GUILD_MESSAGES            = 1 << 9;
        const GUILD_MESSAGE_REACTIONS   = 1 << 10;
        const DIRECT_MESSAGES           = 1 << 12;
        const DIRECT_MESSAGE_REACTIONS  = 1 << 13;
        const DIRECT_MESSAGE_TYPING     = 1 << 14;
        const MESSAGE_CONTENT           = 1 << 15;
        const AUTO_MODERATION_CONFIGURATION = 1 << 17;
    }
}

const DEFAULT_INTENTS: Intents = Intents::all();

#[derive(Clone)]
struct ChannelID {
    guild_id: Option<SNOWFLAKE>,
    id: SNOWFLAKE,
}
type GuildID = SNOWFLAKE;
type MessageID = SNOWFLAKE;

/// Public interface for creating the discord messenger
pub struct Discord;
impl Discord {
    pub fn new_messenger(token: &str) -> Arc<dyn Messenger> {
        Arc::new(InnerDiscord {
            token: token.into(),
            intents: DEFAULT_INTENTS,
            audio_manager: Default::default(),
            gateway: Default::default(),
            pulled_notification: Default::default(),
            query_events: SegQueue::new(),
            text_events: SegQueue::new(),
            voice_events: SegQueue::new(),
            audio_events: SegQueue::new(),
            profile: RwLockAwait::new(None),
            guild_id_mappings: RwLockAwait::new(HashMap::new()),
            channel_id_mappings: RwLockAwait::new(HashMap::new()),
            msg_data: RwLockAwait::new(HashMap::new()),
            _marker: PhantomData,
        })
    }
    fn identifier_generator<D>(id: SNOWFLAKE, data: D) -> Identifier<D> {
        Identifier::new(id, data)
    }
}

trait UnitStruct {}

struct Owned;
impl UnitStruct for Owned {}
struct QueryDiscord;
impl UnitStruct for QueryDiscord {}
struct TextDiscord;
impl UnitStruct for TextDiscord {}
struct VoiceDiscord;
impl UnitStruct for VoiceDiscord {}
struct AudioDiscord;
impl UnitStruct for AudioDiscord {}

#[derive(Default)]
struct AudioManager {
    microphone_recv: Cell<Option<oneshot::Receiver<SampleConsumer>>>,
    microphone: Option<SampleConsumer>,
    microphone_retries: u8,
}

struct InnerDiscord<T: UnitStruct> {
    // Metadata
    token: SecureString,
    intents: Intents,
    // Microphone
    audio_manager: AsyncMutex<AudioManager>,
    // socket related
    gateway: ArcSwapOption<Gateway<General>>,
    pulled_notification: Notify,
    // event queues
    query_events: SegQueue<QueryEvent>,
    text_events: SegQueue<TextEvent>,
    voice_events: SegQueue<VoiceEvent>,
    audio_events: SegQueue<AudioEvent>,
    // Cached data
    profile: RwLockAwait<Option<api_types::Profile>>,
    channel_id_mappings: RwLockAwait<HashMap<ID, ChannelID>>,
    guild_id_mappings: RwLockAwait<HashMap<ID, GuildID>>,
    msg_data: RwLockAwait<HashMap<ID, MessageID>>,
    // etc
    _marker: PhantomData<T>,
}
impl<T: UnitStruct> InnerDiscord<T> {
    async unsafe fn cast_and_downgrade<C: UnitStruct>(self: Arc<Self>) -> Weak<InnerDiscord<C>> {
        if self.gateway.load().is_none() {
            self.gateway.store(Some(Arc::new(
                Gateway::<General>::new(&self).await.unwrap(),
            )));
        }

        let weak = Arc::downgrade(&self);
        let ptr = Weak::into_raw(weak) as *const InnerDiscord<C>;
        unsafe { Weak::from_raw(ptr) }
    }
}

impl Messenger for InnerDiscord<Owned> {
    fn id(&self) -> String {
        self.name().to_owned() + self.token.unsecure()
    }
    fn name(&self) -> &'static str {
        "Discord"
    }
    fn auth(&self) -> String {
        self.token.clone().into_unsecure()
    }
}
