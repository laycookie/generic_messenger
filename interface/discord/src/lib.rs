use std::{
    cell::Cell,
    marker::PhantomData,
    sync::{Arc, Weak},
};

use arc_swap::ArcSwapOption;
use asyncs_sync::Notify;
use bitflags::bitflags;
use crossbeam::queue::SegQueue;
use dashmap::DashMap;
use futures::{channel::oneshot, lock::Mutex as AsyncMutex};
use messenger_interface::{
    interface::{AudioEvent, Messenger, QueryEvent, TextEvent, VoiceEvent},
    stream::{ArcStream, WeakSocketStream},
    types::{ID, Identifier},
};
use secure_string::SecureString;
use simple_audio_channels::input::SampleConsumer;

use crate::{
    api_types::SNOWFLAKE,
    gateways::{Gateway, general::General, general::payloads::VoiceStatePayload},
    lazy_arc::LazyArc,
};

mod api_types;
mod downloaders;
mod gateways;
mod lazy_arc;
mod query;
mod rest_api;

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

bitflags! {
    /// <https://docs.discord.food/topics/gateway#gateway-capabilities>
    struct Capabilities: u32 {
        /// Splits each guild's static metadata into a `properties` sub-object
        /// in Ready/GuildCreate events. Without this, those fields are merged
        /// flat into the guild object.
        const CLIENT_STATE_V2 = 1 << 10;
    }
}

const DEFAULT_CAPABILITIES: Capabilities = Capabilities::CLIENT_STATE_V2;

/// Where a Discord channel lives. Splits guild vs. private so the opcode 4
/// payload and join flow can be picked statically instead of inferring from
/// an `Option<guild_id>` that has two meanings (DM vs "Discord didn't send").
#[derive(Clone, Copy, Debug)]
enum ChannelLocation {
    Guild {
        guild_id: SNOWFLAKE,
        channel_id: SNOWFLAKE,
    },
    /// DM or GroupDM. The two are protocol-identical for voice/messaging
    /// routing; the recipient/name distinction lives on `api_types::Channel`.
    Private { channel_id: SNOWFLAKE },
}

impl ChannelLocation {
    fn channel_id(&self) -> SNOWFLAKE {
        match self {
            Self::Guild { channel_id, .. } | Self::Private { channel_id } => *channel_id,
        }
    }

    fn guild_id(&self) -> Option<SNOWFLAKE> {
        match self {
            Self::Guild { guild_id, .. } => Some(*guild_id),
            Self::Private { .. } => None,
        }
    }

    /// Build from a raw `api_types::Channel`. For guild channels nested
    /// inside a Ready/GuildCreate payload, Discord omits `guild_id` — pass
    /// the parent guild's id via `parent_guild_id` to fill it in.
    /// Top-level events (e.g. ChannelCreate for a guild channel) include
    /// `guild_id` themselves and can pass `None`.
    fn from_api(channel: &api_types::Channel, parent_guild_id: Option<SNOWFLAKE>) -> Option<Self> {
        use api_types::ChannelTypes::*;
        match channel.channel_type {
            DM | GroupDM => Some(Self::Private {
                channel_id: channel.id,
            }),
            _ => {
                let guild_id = parent_guild_id.or(channel.guild_id)?;
                Some(Self::Guild {
                    guild_id,
                    channel_id: channel.id,
                })
            }
        }
    }
}
type GuildID = SNOWFLAKE;
type MessageID = SNOWFLAKE;

/// Public interface for creating the discord messenger
pub struct Discord;
impl Discord {
    pub fn new_messenger(token: &str) -> Arc<dyn Messenger> {
        InnerDiscord::create_messenger(token)
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
    // === Metadata ===
    token: SecureString,
    intents: Intents,
    capabilities: Capabilities,
    // Microphone
    audio_manager: AsyncMutex<AudioManager>,
    // === socket related ===
    gateway: LazyArc<Gateway<General>>,
    pulled_notification: Notify,
    // === event queues === (TODO: Submit them diractly to the UI)
    query_events: SegQueue<QueryEvent>,
    text_events: SegQueue<TextEvent>,
    voice_events: SegQueue<VoiceEvent>,
    audio_events: SegQueue<AudioEvent>,
    // === Cached data ===
    profile: ArcSwapOption<api_types::Profile>,
    voice_states: DashMap<SNOWFLAKE, VoiceStatePayload>,
    // Gateway-owned caches. The `Ready` dispatch handler in
    // `gateways::general::events` is currently the *sole* writer; the REST
    // fallback in `query.rs` reads through these for the cold-start path but
    // never writes back. Once `Ready` lands, REST is bypassed entirely.
    // Caveat: `*_UPDATE` / `*_DELETE` / `*_CREATE` handlers that would keep
    // these caches fresh over the connection's lifetime are not yet wired
    // up, so the cache reflects the `Ready` snapshot frozen in time. That's
    // a separate correctness gap, not a race-policy concern.
    // This is what keeps the gateway/REST race surface narrow for non-message
    // entities; see `crate/messenger_interface/docs/races.md` ("Cache-backed
    // queries: gateway is the only writer").
    dm_channels: ArcSwapOption<Vec<api_types::Channel>>,
    guilds: ArcSwapOption<Vec<api_types::Guild>>,
    guild_channels: DashMap<SNOWFLAKE, Vec<api_types::Channel>>,
    // External to internal ID mappings (TODO: Remove we can store discord IDs diractly in external
    // IDs)
    channel_id_mappings: DashMap<ID, ChannelLocation>,
    guild_id_mappings: DashMap<ID, GuildID>,
    message_id_mappings: DashMap<ID, MessageID>,
    _marker: PhantomData<T>,
}
impl<T: UnitStruct> InnerDiscord<T> {
    async fn ensure_gateway(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.gateway
            .get_or_try_init(async || Gateway::<General>::new(self).await)
            .await?;
        Ok(())
    }

    /// Creates a [`WeakSocketStream`] by reinterpreting this `InnerDiscord<T>`
    /// as `InnerDiscord<C>` to select the matching [`ArcStream`] implementation.
    ///
    /// # Safety
    /// `InnerDiscord<T>` and `InnerDiscord<C>` must have identical memory layouts.
    /// This holds as long as `T` and `C` are both zero-sized `UnitStruct` markers.
    async fn listen_as<C, E>(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<E>, Box<dyn std::error::Error + Send + Sync>>
    where
        C: UnitStruct + 'static,
        InnerDiscord<C>: ArcStream<Item = E> + Send + Sync,
        E: Send + 'static,
    {
        self.ensure_gateway().await?;
        let weak = Arc::downgrade(&self);
        let ptr = Weak::into_raw(weak) as *const InnerDiscord<C>;
        Ok(WeakSocketStream::new(unsafe { Weak::from_raw(ptr) }))
    }
}

impl Messenger for InnerDiscord<Owned> {
    /// Auth_obj is expected to be token for discord
    fn create_messenger(auth_obj: &str) -> Arc<dyn Messenger>
    where
        Self: Sized,
    {
        Arc::new(InnerDiscord {
            token: auth_obj.into(),
            intents: DEFAULT_INTENTS,
            capabilities: DEFAULT_CAPABILITIES,
            audio_manager: Default::default(),
            gateway: Default::default(),
            pulled_notification: Default::default(),
            query_events: SegQueue::new(),
            text_events: SegQueue::new(),
            voice_events: SegQueue::new(),
            audio_events: SegQueue::new(),
            profile: ArcSwapOption::empty(),
            voice_states: DashMap::new(),
            dm_channels: ArcSwapOption::empty(),
            guilds: ArcSwapOption::empty(),
            guild_channels: DashMap::new(),
            guild_id_mappings: DashMap::new(),
            channel_id_mappings: DashMap::new(),
            message_id_mappings: DashMap::new(),
            _marker: PhantomData,
        })
    }
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
