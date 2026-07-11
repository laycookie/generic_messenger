use std::{
    marker::PhantomData,
    pin::pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering, fence},
    },
    task::Poll,
};

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use asyncs_sync::Notify;
use bitflags::bitflags;
use crossbeam::queue::ArrayQueue;
use dashmap::DashMap;
use futures::{channel::oneshot, future::poll_fn, lock::Mutex as AsyncMutex};
use messenger_interface::{
    auth::{AuthMethod, Credential, CredentialField, CredentialSchema},
    interface::{AudioEvent, Messenger, QueryEvent, TextEvent, VoiceEvent},
    stream::{ArcStream, WeakSocketStream},
    types::{ID, Identifier, User as GlobalUser, UserStatus},
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
mod rich;
mod text;
mod voice;

pub(crate) const INTERFACE_NAME: &str = "Discord";
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
    /// Construct a Discord messenger from a [`Credential`]; see
    /// [`Messenger::create_messenger`]. A `Token` builds immediately; a
    /// `Password` (email/phone + password, optional MFA) fetches the token from
    /// Discord up front, so a rejected login surfaces here rather than on first use.
    pub async fn create_messenger(
        cred: Credential,
    ) -> Result<Arc<dyn Messenger>, Box<dyn std::error::Error + Send + Sync>> {
        <InnerDiscord<Owned> as Messenger>::create_messenger(cred).await
    }
    /// The login methods Discord offers (a token, or email/phone + password).
    pub fn auth_methods() -> &'static [AuthMethod] {
        <InnerDiscord<Owned> as Messenger>::auth_methods()
    }
    fn identifier_generator<D>(id: SNOWFLAKE, data: D) -> Identifier<D> {
        Identifier::new(id, data)
    }
}

/// Maximum number of buffered events per queue. Queues are only drained by
/// their corresponding `listen()` stream; if the app never listens for an
/// event type (e.g. voice), the queue would otherwise grow without bound. The
/// queues are [`ArrayQueue`]s of this capacity, so producers use
/// `force_push` to drop the oldest entry when full.
const EVENT_QUEUE_CAP: usize = 4096;

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
    microphone_recv: Option<oneshot::Receiver<SampleConsumer>>,
    microphone: Option<SampleConsumer>,
    microphone_retries: u8,
}

struct InnerDiscord<T: UnitStruct> {
    // === Metadata ===
    /// The Discord user token used for every authenticated request. Known at
    /// construction for both login paths: a token login receives it directly,
    /// a username/password login ([`Discord::login`]) fetches it up front.
    token: SecureString,
    intents: Intents,
    capabilities: Capabilities,
    // Microphone
    audio_manager: AsyncMutex<AudioManager>,
    // === socket related ===
    gateway: LazyArc<Gateway<General>>,
    pulled_notification: Notify,
    /// Set by [`InnerDiscord::kill`] once a stream detects the app dropped
    /// its last handle (see [`InnerDiscord::owner_dropped`]). In-flight
    /// `next()` futures hold strong `Arc`s to this struct, so `Drop` on
    /// `InnerDiscord` can never be the teardown signal by itself; every
    /// poll loop checks this flag and returns `None`.
    killed: AtomicBool,
    /// Wakes pollers parked on long awaits (e.g. the audio loop) so they
    /// observe `killed` promptly instead of on their next natural wake.
    kill_notify: Notify,
    /// Number of in-flight `ArcStream::next` futures, maintained by
    /// [`StreamPollGuard`]. Input to [`InnerDiscord::owner_dropped`].
    active_streams: AtomicUsize,
    // === event queues === (TODO: Submit them diractly to the UI)
    query_events: ArrayQueue<QueryEvent>,
    text_events: ArrayQueue<TextEvent>,
    voice_events: ArrayQueue<VoiceEvent>,
    audio_events: ArrayQueue<AudioEvent>,
    // === Cached data ===
    profile: ArcSwapOption<api_types::Profile>,
    voice_states: DashMap<SNOWFLAKE, VoiceStatePayload>,
    /// Per-channel voice roster, keyed by Discord channel SNOWFLAKE. The
    /// authoritative source of "who is in which VC" so that lazily-loaded
    /// guilds get correct `Room.participants` on first `house_details`
    /// fetch — see `process_guild_channels`. Mutated alongside `voice_states`
    /// in `emit_voice_state_participant`.
    voice_participants: DashMap<SNOWFLAKE, Vec<Identifier<GlobalUser>>>,
    /// Latest known user presence/status, keyed by Discord user snowflake.
    presences: DashMap<SNOWFLAKE, UserStatus>,
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
    relationships: ArcSwapOption<Vec<api_types::Friend>>,
    dm_channels: ArcSwapOption<Vec<api_types::Channel>>,
    guilds: ArcSwapOption<Vec<api_types::Guild>>,
    guild_channels: DashMap<SNOWFLAKE, Vec<api_types::Channel>>,
    /// Per-guild custom emoji / stickers from the `Ready` snapshot, feeding
    /// the picker via `pickable_emojis`/`pickable_stickers` in `text.rs`.
    guild_emojis: DashMap<SNOWFLAKE, Vec<api_types::GuildEmoji>>,
    guild_stickers: DashMap<SNOWFLAKE, Vec<api_types::GuildSticker>>,
    // External to internal ID mappings (TODO: Remove we can store discord IDs diractly in external
    // IDs)
    channel_id_mappings: DashMap<ID, ChannelLocation>,
    guild_id_mappings: DashMap<ID, GuildID>,
    message_id_mappings: DashMap<ID, MessageID>,
    _marker: PhantomData<T>,
}
/// `listen_as` reinterprets `InnerDiscord<T>` across marker types, which is
/// only sound while every instantiation has an identical layout. The markers
/// are all ZSTs so this holds; assert it so a future non-ZST marker fails to
/// compile instead of becoming UB.
const _: () = {
    use std::mem::{align_of, size_of};
    assert!(size_of::<InnerDiscord<Owned>>() == size_of::<InnerDiscord<QueryDiscord>>());
    assert!(size_of::<InnerDiscord<Owned>>() == size_of::<InnerDiscord<TextDiscord>>());
    assert!(size_of::<InnerDiscord<Owned>>() == size_of::<InnerDiscord<VoiceDiscord>>());
    assert!(size_of::<InnerDiscord<Owned>>() == size_of::<InnerDiscord<AudioDiscord>>());
    assert!(align_of::<InnerDiscord<Owned>>() == align_of::<InnerDiscord<QueryDiscord>>());
    assert!(align_of::<InnerDiscord<Owned>>() == align_of::<InnerDiscord<TextDiscord>>());
    assert!(align_of::<InnerDiscord<Owned>>() == align_of::<InnerDiscord<VoiceDiscord>>());
    assert!(align_of::<InnerDiscord<Owned>>() == align_of::<InnerDiscord<AudioDiscord>>());
};

/// RAII registration of one in-flight `ArcStream::next` future in
/// `active_streams`. Created as the first statement of `next()`, so its
/// drop — running before the future's own `self: Arc` is released — keeps
/// the invariant "`active_streams` never exceeds the number of `Arc`s held
/// by in-flight `next()` futures", which is what makes
/// [`InnerDiscord::owner_dropped`] free of false positives.
struct StreamPollGuard<'a>(&'a AtomicUsize);
impl<'a> StreamPollGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        // The future's self-Arc clone is sequenced before this on the same
        // thread; the fence pairs with the one in `owner_dropped` so an
        // observer that sees this increment also sees that Arc increment.
        fence(Ordering::SeqCst);
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}
impl Drop for StreamPollGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<T: UnitStruct> InnerDiscord<T> {
    pub(crate) fn cached_user_status(&self, user_id: SNOWFLAKE) -> Option<UserStatus> {
        self.presences.get(&user_id).map(|status| *status)
    }

    pub(crate) fn interface_user_status(&self, user_id: SNOWFLAKE) -> Option<UserStatus> {
        Some(
            self.cached_user_status(user_id)
                .unwrap_or(UserStatus::Offline),
        )
    }

    async fn ensure_gateway(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.killed.load(Ordering::Acquire) {
            return Err("messenger was dropped".into());
        }
        self.gateway
            .get_or_try_init(async || Gateway::<General>::new(self).await)
            .await?;
        Ok(())
    }

    /// True when every remaining strong reference is held by in-flight
    /// `next()` futures themselves — i.e. the app dropped its last
    /// `Arc<dyn Messenger>` / capability handle and the streams are the
    /// only thing keeping this alive.
    ///
    /// Must be called from inside `next()` (which contributes one counted
    /// reference) while being polled through `WeakSocketStream` (whose
    /// transient upgrade contributes the `+ 1`). Counts are maintained so
    /// they never over-state in-flight futures (see [`StreamPollGuard`]),
    /// so a race can only delay detection to a later poll, never fire it
    /// while an external owner exists.
    fn owner_dropped(self: &Arc<Self>) -> bool {
        let active = self.active_streams.load(Ordering::SeqCst);
        // Pairs with the fence in `StreamPollGuard::new`: if `active`
        // includes a freshly registered future, the strong count read
        // below includes that future's Arc as well.
        fence(Ordering::SeqCst);
        Arc::strong_count(self) == active + 1
    }

    /// Tear down cooperatively: flag every poll loop to exit, drop the
    /// gateway (closing the websocket once in-flight loads release their
    /// guards), and wake parked pollers so they observe the flag.
    fn kill(&self) {
        self.killed.store(true, Ordering::Release);
        self.gateway.store(None);
        self.kill_notify.notify_all();
        self.pulled_notification.notify_all();
    }

    /// Resolves once [`InnerDiscord::kill`] has run. Check → register →
    /// re-check, so a kill landing between the flag check and the waiter
    /// registration cannot be missed. Used as a select arm by the audio loop
    /// so a long await wakes promptly on teardown.
    async fn killed_signal(&self) {
        if self.killed.load(Ordering::Acquire) {
            return;
        }
        let mut notified = pin!(self.kill_notify.notified());
        poll_fn(|cx| {
            if self.killed.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            if notified.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
            // `kill()` may have stored the flag after the check above but
            // before `notified` registered its waker; re-check.
            if self.killed.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await;
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

impl InnerDiscord<Owned> {
    /// Shared constructor for both login paths. The token is always known up
    /// front: a token login passes it straight through, a username/password
    /// login ([`Discord::login`]) fetches it from Discord first.
    fn build(token: SecureString) -> Arc<dyn Messenger> {
        Arc::new(InnerDiscord {
            token,
            intents: DEFAULT_INTENTS,
            capabilities: DEFAULT_CAPABILITIES,
            audio_manager: Default::default(),
            gateway: Default::default(),
            pulled_notification: Default::default(),
            killed: AtomicBool::new(false),
            kill_notify: Default::default(),
            active_streams: AtomicUsize::new(0),
            query_events: ArrayQueue::new(EVENT_QUEUE_CAP),
            text_events: ArrayQueue::new(EVENT_QUEUE_CAP),
            voice_events: ArrayQueue::new(EVENT_QUEUE_CAP),
            audio_events: ArrayQueue::new(EVENT_QUEUE_CAP),
            profile: ArcSwapOption::empty(),
            voice_states: DashMap::new(),
            voice_participants: DashMap::new(),
            presences: DashMap::new(),
            relationships: ArcSwapOption::empty(),
            dm_channels: ArcSwapOption::empty(),
            guilds: ArcSwapOption::empty(),
            guild_channels: DashMap::new(),
            guild_emojis: DashMap::new(),
            guild_stickers: DashMap::new(),
            guild_id_mappings: DashMap::new(),
            channel_id_mappings: DashMap::new(),
            message_id_mappings: DashMap::new(),
            _marker: PhantomData,
        })
    }
}

/// Discord's login shapes. `Token` builds immediately; `Password` performs the
/// username/password (+ optional MFA) exchange first. Discord has no restored
/// session — its persisted credential is just a `Token`.
#[derive(CredentialSchema)]
enum DiscordAuth {
    #[credential(method = "Token")]
    Token {
        #[credential(label = "Token")]
        token: SecureString,
    },
    #[credential(method = "Email & password")]
    Password {
        #[credential(label = "Email or phone")]
        username: SecureString,
        #[credential(label = "Password", secret)]
        password: SecureString,
        #[credential(label = "Two-factor code (if enabled)")]
        otp: Option<SecureString>,
    },
}

#[async_trait]
impl Messenger for InnerDiscord<Owned> {
    /// A `Token` builds immediately; a `Password` performs the username/password
    /// (+ optional MFA) exchange and builds from the resolved token.
    async fn create_messenger(
        cred: Credential,
    ) -> Result<Arc<dyn Messenger>, Box<dyn std::error::Error + Send + Sync>> {
        match DiscordAuth::try_from(cred)? {
            DiscordAuth::Token { token } => Ok(Self::build(token)),
            DiscordAuth::Password {
                username,
                password,
                otp,
            } => {
                let token = rest_api::login_with_credentials(
                    username.unsecure(),
                    password.unsecure(),
                    otp.as_ref().map(SecureString::unsecure),
                )
                .await?;
                Ok(Self::build(SecureString::from(token)))
            }
        }
    }

    fn auth_methods() -> &'static [AuthMethod] {
        DiscordAuth::auth_methods()
    }

    /// NOTE: the id is meant to be used only on the client, for identification
    /// purposes, and is never supposed to be sent anywhere — that's why it is
    /// safe to embed the token in it.
    fn id(&self) -> String {
        INTERFACE_NAME.to_string() + self.token.unsecure()
    }
    fn name(&self) -> &'static str {
        INTERFACE_NAME
    }
    /// The resolved user token, persisted so a credential login becomes a token
    /// login on the next start.
    fn auth(&self) -> Credential {
        Credential(vec![CredentialField::new("token", self.token.clone())])
    }
}
