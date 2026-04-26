#![feature(never_type)]

use std::{
    cell::Cell,
    collections::HashMap,
    marker::PhantomData,
    sync::{Arc, Weak},
};

use arc_swap::ArcSwapOption;
use asyncs_sync::Notify;
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

/// Public interface for creating the discord messanger
pub struct Discord;
impl Discord {
    pub fn new_messenger(token: &str) -> Arc<dyn Messenger> {
        Arc::new(InnerDiscord {
            token: token.into(),
            intents: 194557,
            audio_manager: Default::default(),
            gateaway: Default::default(),
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
}

struct InnerDiscord<T: UnitStruct> {
    // Metadata
    token: SecureString,
    intents: u32,
    // Microphone
    audio_manager: AsyncMutex<AudioManager>,
    // socket related
    gateaway: ArcSwapOption<Gateaway<General>>,
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
        if self.gateaway.load().is_none() {
            self.gateaway.store(Some(Arc::new(
                Gateaway::<General>::new(&self).await.unwrap(),
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
