use std::{
    collections::HashMap,
    marker::PhantomData,
    ops::Deref,
    sync::{Arc, Weak},
};

use arc_swap::ArcSwapOption;
use crossbeam::queue::SegQueue;
use futures_locks::RwLock as RwLockAwait;
use messenger_interface::{
    interface::{AudioEvent, Messenger, QueryEvent, TextEvent, VoiceEvent},
    types::{ID, Identifier},
};
use secure_string::SecureString;

use crate::{
    api_types::SNOWFLAKE,
    gateaways::{Gateaway, general::General},
};

mod api_types;
mod downloaders;
mod gateaways;
mod query;

#[derive(Clone)]
struct ChannelID {
    guild_id: Option<SNOWFLAKE>,
    id: SNOWFLAKE,
}
type GuildID = SNOWFLAKE;
type MessageID = SNOWFLAKE;

pub struct Owned;
struct QueryDiscord;
struct TextDiscord;
struct VoiceDiscord;
struct AudioDiscord;

pub struct InnerDiscord<T> {
    // Metadata
    token: SecureString,
    intents: u32,
    // socket related
    gateaway: ArcSwapOption<Gateaway<General>>,
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
impl<T> InnerDiscord<T> {
    async fn query(self: Arc<Self>) -> Weak<InnerDiscord<QueryDiscord>> {
        if self.gateaway.load().is_none() {
            self.gateaway.store(Some(Arc::new(
                Gateaway::<General>::new(&self).await.unwrap(),
            )));
        }

        let weak = Arc::downgrade(&self);
        let ptr = Weak::into_raw(weak) as *const InnerDiscord<QueryDiscord>;
        unsafe { Weak::from_raw(ptr) }
    }
    async fn text(self: Arc<Self>) -> Weak<InnerDiscord<TextDiscord>> {
        if self.gateaway.load().is_none() {
            self.gateaway.store(Some(Arc::new(
                Gateaway::<General>::new(&self).await.unwrap(),
            )));
        }

        let weak = Arc::downgrade(&self);
        let ptr = Weak::into_raw(weak) as *const InnerDiscord<TextDiscord>;
        unsafe { Weak::from_raw(ptr) }
    }
    async fn voice(self: Arc<Self>) -> Weak<InnerDiscord<VoiceDiscord>> {
        if self.gateaway.load().is_none() {
            self.gateaway.store(Some(Arc::new(
                Gateaway::<General>::new(&self).await.unwrap(),
            )));
        }

        let weak = Arc::downgrade(&self);
        let ptr = Weak::into_raw(weak) as *const InnerDiscord<VoiceDiscord>;
        unsafe { Weak::from_raw(ptr) }
    }
    async fn audio(self: Arc<Self>) -> Weak<InnerDiscord<AudioDiscord>> {
        if self.gateaway.load().is_none() {
            self.gateaway.store(Some(Arc::new(
                Gateaway::<General>::new(&self).await.unwrap(),
            )));
        }

        let weak = Arc::downgrade(&self);
        let ptr = Weak::into_raw(weak) as *const InnerDiscord<AudioDiscord>;
        unsafe { Weak::from_raw(ptr) }
    }
}

pub struct Discord(Arc<InnerDiscord<Owned>>);
impl Deref for Discord {
    type Target = InnerDiscord<Owned>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Discord {
    pub fn new(token: &str) -> Self {
        Discord(
            InnerDiscord {
                token: token.into(),
                intents: 194557,
                gateaway: None.into(),
                query_events: SegQueue::new(),
                text_events: SegQueue::new(),
                voice_events: SegQueue::new(),
                audio_events: SegQueue::new(),
                profile: RwLockAwait::new(None),
                guild_id_mappings: RwLockAwait::new(HashMap::new()),
                channel_id_mappings: RwLockAwait::new(HashMap::new()),
                msg_data: RwLockAwait::new(HashMap::new()),
                _marker: PhantomData,
            }
            .into(),
        )
    }
    fn identifier_generator<D>(id: SNOWFLAKE, data: D) -> Identifier<D> {
        Identifier::new(id, data)
    }
}

impl Messenger for Discord {
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
