//! High-level query adapter.
//!
//! This module implements the `Query` trait from `messenger_interface` on top
//! of the raw REST calls in `rest_api.rs` and the cached state populated by
//! gateway events. It is also the place where data returned to the UI gets
//! reconciled against the cache so that HTTP results and gateway events stay
//! coherent. The sibling `Text` (messaging) layer lives in `text.rs` and
//! shares the same cache/REST reconciliation model described below.
//!
//! # Two query shapes
//!
//! Methods on this module fall into one of two shapes, and the distinction
//! drives the entire race story for the Discord backend.
//!
//! ## Cache-backed (cache-first, REST cold-start fallback)
//!
//! `houses`, `rooms`, `house_details`, `client_user`: read the relevant
//! `InnerDiscord` cache field (`guilds`, `dm_channels`, `guild_channels`,
//! `profile`) and only fall back to REST if the cache is empty. The cache
//! is seeded by the `Ready` gateway dispatch and maintained by the matching
//! guild/channel/relationship/expression create, update, and delete reducers
//! in `gateways::general::dispatcher`.
//!
//! **The REST fallback returns data directly to the caller without writing
//! it back to the cache.** This is load-bearing: once `Ready` lands, the
//! cache is populated forever (or until the gateway reconnects, at which
//! point a fresh `Ready` overwrites it), and REST is never consulted again
//! for these queries. The race window for cache-backed queries is therefore
//! confined to the gateway-up-but-pre-`Ready` cold-start window — typically
//! 1–3 seconds — and even within that window there is no cache-clobber,
//! only a potential UI-level phantom if the event stream is consumed before
//! the in-flight REST future resolves.
//!
//! These gateway-owned caches do **not** need REST tombstone rings or per-ID
//! merge policies: REST never writes them after a Ready snapshot is published.
//!
//! ## Every-call HTTP (no cache, REST on every invocation)
//!
//! `get_messages`, `contacts`: have no cache-first path. Every call hits
//! REST, and the result is returned to the UI without ever being stored.
//! These are the queries where the gateway/REST reconciliation policy
//! actually applies: a gateway event landing during the in-flight HTTP
//! call can race with the response, and a tombstone ring (for deletes) or
//! per-field merge (for updates) is required to keep the UI consistent.
//! See `crate/messenger_interface/docs/races.md` for the full reconciliation
//! policy and the per-fetch audit table.
use crate::{
    Discord, InnerDiscord, Owned,
    api::{self, SNOWFLAKE},
    cache::{ChannelRoute, DiscordCache},
    downloaders::CdnImage,
    rest_api::DiscordApi,
    stream_poll::StreamPollGuard,
};

#[unitstruct::unit_struct]
pub(crate) struct QueryDiscord;
use async_trait::async_trait;
use futures::{
    FutureExt as _, StreamExt as _,
    future::{Either, select},
    stream,
};
use futures_timer::Delay;
use messenger_interface::{
    interface::{Query, QueryEvent},
    stream::{ArcStream, WeakSocketStream},
    types::{Details, House, Identifier, Place, Room, RoomCapabilities, User},
};
use tracing::{error, warn};

use std::{error::Error, pin::pin, sync::Arc, time::Duration};

const GATEWAY_CACHE_POLL_ATTEMPTS: usize = 8;
const GATEWAY_CACHE_POLL_TIMEOUT: Duration = Duration::from_millis(500);
const ASSET_FETCH_CONCURRENCY: usize = 16;

/// Creation time encoded in a snowflake: `id >> 22` is milliseconds since
/// the Discord epoch (2015-01-01T00:00:00Z).
fn snowflake_created(id: SNOWFLAKE) -> Option<chrono::DateTime<chrono::Utc>> {
    const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
    chrono::DateTime::from_timestamp_millis(((id >> 22) + DISCORD_EPOCH_MS) as i64)
}

// TODO: Deprecate once queries no longer pump gateway events themselves.
async fn poll_gateway_until(
    discord: &InnerDiscord<Owned>,
    ready: impl Fn(&InnerDiscord<Owned>) -> bool,
) -> bool {
    if ready(discord) {
        return true;
    }
    if let Err(err) = discord.gateway.ensure(&discord.api).await {
        warn!("Discord: failed to open gateway for REST fallback: {err}");
        return false;
    }

    for _ in 0..GATEWAY_CACHE_POLL_ATTEMPTS {
        if ready(discord) {
            return true;
        }

        let turn = discord.wait_for_main_gateway_turn();
        futures::pin_mut!(turn);
        let timeout = Delay::new(GATEWAY_CACHE_POLL_TIMEOUT);
        futures::pin_mut!(timeout);
        match select(turn, timeout).await {
            Either::Left((Some(turn), _)) => {
                if discord.execute_main_gateway_turn(turn).await.is_none() {
                    return false;
                }
            }
            Either::Left((None, _)) => return false,
            Either::Right((_, _)) => {}
        }
    }

    ready(discord)
}

/// Borrowed query dependencies. Owns no state; REST/cache interaction remains
/// explicit at each operation instead of making HTTP transport cache-aware.
struct QueryContext<'a> {
    api: &'a DiscordApi,
    cache: &'a DiscordCache,
}

impl<'a> QueryContext<'a> {
    fn new(api: &'a DiscordApi, cache: &'a DiscordCache) -> Self {
        Self { api, cache }
    }

    async fn process_dm_channels(&self, channels: &[api::Channel]) -> Vec<Identifier<Place<Room>>> {
        // Sort by last_message_id descending (most recent first).
        // Discord snowflake IDs encode timestamps, so higher = newer.
        let mut sorted = channels.to_vec();
        sorted.sort_by_key(|channel| std::cmp::Reverse(self.cache.last_message_id(channel)));

        let rooms_producer = sorted.into_iter().map(|channel| async move {
            let place_room = channel.to_room_data().await;
            (
                Discord::identifier_generator(channel.id, place_room),
                channel.guild_id,
            )
        });

        let rooms = stream::iter(rooms_producer)
            .buffered(ASSET_FETCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        for (identifier, guild_id) in &rooms {
            self.cache
                .channel_routes
                .insert(*identifier.id(), ChannelRoute::new(*guild_id));
        }

        rooms
            .into_iter()
            .map(|(identifier, _)| identifier)
            .collect()
    }

    async fn process_contacts(&self, friends: &[api::Friend]) -> Vec<Identifier<User>> {
        let contact_producer = friends.iter().cloned().map(|friend| async move {
            let hash = match &friend.user.avatar {
                Some(hash) => CdnImage::avatar(friend.id, hash).fetch().await.ok(),
                None => None,
            };

            Discord::identifier_generator(
                friend.id,
                User {
                    name: friend
                        .user
                        .global_name
                        .clone()
                        .unwrap_or_else(|| friend.user.username.clone()),
                    tag: Some(friend.user.username.clone()),
                    icon: hash,
                    status: None,
                    voice: None,
                },
            )
        });

        stream::iter(contact_producer)
            .buffered(ASSET_FETCH_CONCURRENCY)
            .collect()
            .await
    }

    async fn process_guilds(&self, guilds: &[api::Guild]) -> Vec<Identifier<Place<House>>> {
        let house_producer = guilds.iter().cloned().map(|guild| async move {
            let guild_name = guild.name.clone();
            let icon = guild.icon.as_ref().map(|hash| async move {
                match CdnImage::guild_icon(guild.id, hash).fetch().await {
                    Ok(path) => Some(path),
                    Err(e) => {
                        error!("Failed to download icon for guild: {}\n{}", guild_name, e);
                        None
                    }
                }
            });

            Discord::identifier_generator(
                guild.id,
                Place::new(
                    guild.name,
                    match icon {
                        Some(icon) => icon.await,
                        None => None,
                    },
                    House::new(None),
                ),
            )
        });

        stream::iter(house_producer)
            .buffered(ASSET_FETCH_CONCURRENCY)
            .collect()
            .await
    }

    fn process_guild_channels(
        &self,
        guild_id: SNOWFLAKE,
        channels: &[api::Channel],
    ) -> Vec<Identifier<Place<Room>>> {
        // Build category position lookup: category_id → position
        let category_positions: std::collections::HashMap<SNOWFLAKE, i32> = channels
            .iter()
            .filter(|c| matches!(c.channel_type, api::ChannelTypes::GuildCategory))
            .map(|c| (c.id, c.position.unwrap_or(0)))
            .collect();

        // Sort: categories first by position, then children grouped under
        // their parent category and sorted by their own position.
        // Channels without a parent sort to the top.
        let mut sorted: Vec<&api::Channel> = channels.iter().collect();
        sorted.sort_by_key(|channel| {
            let own_pos = channel.position.unwrap_or(0);
            let is_category = matches!(channel.channel_type, api::ChannelTypes::GuildCategory);

            match channel
                .parent_id
                .as_ref()
                .and_then(|pid| pid.parse::<SNOWFLAKE>().ok())
            {
                // Child channel: sort after its parent category
                Some(parent_id) => {
                    let parent_pos = category_positions.get(&parent_id).copied().unwrap_or(0);
                    (parent_pos, 1, own_pos)
                }
                // Category or top-level channel
                None => {
                    if is_category {
                        (own_pos, 0, 0) // Category header comes first
                    } else {
                        (own_pos, 1, 0) // Uncategorized channel
                    }
                }
            }
        });

        sorted
            .into_iter()
            .filter_map(|channel| {
                if channel
                    .permission_overwrites
                    .as_ref()
                    .is_some_and(|overwrites| {
                        overwrites
                            .iter()
                            // TODO: Rewrite
                            .any(|a| a.deny.parse::<u64>().unwrap_or(0) & (1 << 10) != 0)
                    })
                {
                    return None;
                };

                let channel_name = channel
                    .name
                    .clone()
                    .unwrap_or_else(|| "Unknown".to_string());
                // Seed participants from the live voice roster so a guild
                // loaded after Ready already contains the users currently
                // in voice — no extra fetch, no race with gateway events.
                let participants = self.cache.voice.participants(channel.id);
                let identifier = Discord::identifier_generator(
                    channel.id,
                    Place::new(
                        channel_name,
                        None,
                        Room::new(
                            RoomCapabilities::from(channel.channel_type),
                            None,
                            Some(participants),
                            None,
                        ),
                    ),
                );
                // Discord omits guild_id on channels nested in Ready/
                // GuildCreate payloads, so preserve their parent context.
                self.cache
                    .channel_routes
                    .insert(*identifier.id(), ChannelRoute::new(Some(guild_id)));
                Some(identifier)
            })
            .collect::<Vec<_>>()
    }

    async fn process_profile(&self, profile: &api::Profile) -> Identifier<User> {
        let icon = match &profile.avatar {
            Some(hash) => CdnImage::avatar(profile.id, hash).fetch().await.ok(),
            None => None,
        };

        Discord::identifier_generator(
            profile.id,
            User {
                name: profile
                    .global_name
                    .clone()
                    .unwrap_or_else(|| profile.username.clone()),
                tag: Some(profile.username.clone()),
                icon,
                status: None,
                voice: None,
            },
        )
    }
}

#[async_trait]
impl Query for InnerDiscord<Owned> {
    async fn client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        let query = QueryContext::new(&self.api, &self.cache);
        if let Some(profile) = query.cache.profile.load_full() {
            return Ok(query.process_profile(&profile).await);
        }
        let rest_err = match query.api.rest_get_profile().await {
            Ok(profile) => return Ok(query.process_profile(&profile).await),
            Err(err) => err,
        };

        warn!("Discord: REST profile fetch failed; trying gateway Ready cache: {rest_err}");
        if poll_gateway_until(self, |discord| discord.cache.profile.load().is_some()).await
            && let Some(profile) = self.cache.profile.load_full()
        {
            return Ok(query.process_profile(&profile).await);
        }

        Err(rest_err)
    }

    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        let query = QueryContext::new(&self.api, &self.cache);
        if let Some(cached) = query.cache.relationships.load_full() {
            return Ok(query.process_contacts(&cached).await);
        }
        let rest_err = match query.api.rest_get_contacts().await {
            Ok(friends) => return Ok(query.process_contacts(&friends).await),
            Err(err) => err,
        };

        warn!("Discord: REST contacts fetch failed; trying gateway Ready cache: {rest_err}");
        if poll_gateway_until(self, |discord| discord.cache.relationships.load().is_some()).await
            && let Some(cached) = self.cache.relationships.load_full()
        {
            return Ok(query.process_contacts(&cached).await);
        }

        Err(rest_err)
    }

    async fn rooms(&self) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        let query = QueryContext::new(&self.api, &self.cache);
        if let Some(cached) = query.cache.dm_channels.load_full() {
            return Ok(query.process_dm_channels(&cached).await);
        }
        let rest_err = match query.api.rest_get_dms().await {
            Ok(channels) => return Ok(query.process_dm_channels(&channels).await),
            Err(err) => err,
        };

        warn!("Discord: REST DM fetch failed; trying gateway Ready cache: {rest_err}");
        if poll_gateway_until(self, |discord| discord.cache.dm_channels.load().is_some()).await
            && let Some(cached) = self.cache.dm_channels.load_full()
        {
            return Ok(query.process_dm_channels(&cached).await);
        }

        Err(rest_err)
    }

    async fn houses(&self) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        let query = QueryContext::new(&self.api, &self.cache);
        if let Some(cached) = query.cache.guilds.load_full() {
            return Ok(query.process_guilds(&cached).await);
        }
        let rest_err = match query.api.rest_get_guilds().await {
            Ok(guilds) => return Ok(query.process_guilds(&guilds).await),
            Err(err) => err,
        };

        warn!("Discord: REST guild fetch failed; trying gateway Ready cache: {rest_err}");
        if poll_gateway_until(self, |discord| discord.cache.guilds.load().is_some()).await
            && let Some(cached) = self.cache.guilds.load_full()
        {
            return Ok(query.process_guilds(&cached).await);
        }

        Err(rest_err)
    }

    // TODO: Implement room_details and house_details if needed
    // These would fetch detailed information about a specific room/house

    async fn house_details(
        &self,
        house: Identifier<Place<House>>,
    ) -> Result<House, Box<dyn Error + Sync + Send>> {
        let guild_id = *house.id();
        let query = QueryContext::new(&self.api, &self.cache);

        let rooms = if let Some(cached) = query.cache.guild_channels.get(&guild_id) {
            query.process_guild_channels(guild_id, &cached)
        } else {
            query
                .api
                .rest_get_guild_channels(guild_id)
                .await
                .map(|channels| query.process_guild_channels(guild_id, &channels))
                .unwrap_or_default()
        };

        Ok(House::new(Some(rooms)))
    }

    async fn inspect_user(
        &self,
        user: &Identifier<User>,
    ) -> Result<Details, Box<dyn Error + Sync + Send>> {
        // Interface user ids are the raw Discord snowflakes (see the contact
        // and message-author construction paths), so no mapping table here.
        let user_id = *user.id();
        let mut details = Details::default();
        let query = QueryContext::new(&self.api, &self.cache);

        // The rich profile endpoint needs a mutual friend/guild with the
        // target; fall back to the public user object when it rejects us.
        match query.api.rest_get_user_profile(user_id).await {
            Ok(profile) => {
                details.push("Username", profile.user.username);
                details.push_opt("Display name", profile.user.global_name);
                if profile.user.bot.unwrap_or(false) {
                    details.push("Bot", "Yes");
                }
                if let Some(meta) = profile.user_profile {
                    details.push_opt("Bio", meta.bio.filter(|bio| !bio.is_empty()));
                    details.push_opt(
                        "Pronouns",
                        meta.pronouns.filter(|pronouns| !pronouns.is_empty()),
                    );
                }
                details.push_opt("Premium since", profile.premium_since);
                if let Some(mutual) = profile.mutual_guilds {
                    details.push("Mutual servers", mutual.len().to_string());
                }
            }
            Err(err) => {
                warn!(
                    "Discord: profile endpoint failed for {user_id}, \
                     falling back to the public user object: {err}"
                );
                let user = query.api.rest_get_user(user_id).await?;
                details.push("Username", user.username);
                details.push_opt("Display name", user.global_name);
                if user.bot.unwrap_or(false) {
                    details.push("Bot", "Yes");
                }
            }
        }

        details.push("User ID", user_id.to_string());
        details.push_opt(
            "Created",
            snowflake_created(user_id).map(|at| at.format("%Y-%m-%d").to_string()),
        );
        Ok(details)
    }

    async fn inspect_house(
        &self,
        house: &Identifier<Place<House>>,
    ) -> Result<Details, Box<dyn Error + Sync + Send>> {
        let guild_id = *house.id();
        let query = QueryContext::new(&self.api, &self.cache);

        let guild = query.api.rest_get_guild(guild_id).await?;

        let mut details = Details::default();
        details.push("Name", guild.name);
        details.push_opt(
            "Description",
            guild
                .description
                .filter(|description| !description.is_empty()),
        );
        details.push_opt(
            "Members",
            guild
                .approximate_member_count
                .map(|count| count.to_string()),
        );
        details.push_opt(
            "Online",
            guild
                .approximate_presence_count
                .map(|count| count.to_string()),
        );
        details.push_opt("Owner ID", guild.owner_id.map(|id| id.to_string()));
        details.push("Server ID", guild_id.to_string());
        details.push_opt(
            "Created",
            snowflake_created(guild_id).map(|at| at.format("%Y-%m-%d").to_string()),
        );
        Ok(details)
    }

    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<QueryEvent>, Box<dyn Error + Sync + Send>> {
        self.listen_as::<QueryDiscord, _>().await
    }
}

#[async_trait]
impl ArcStream for InnerDiscord<QueryDiscord> {
    type Item = QueryEvent;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item> {
        let _guard = StreamPollGuard::new(&self.gateway.active_streams);
        loop {
            if self
                .gateway
                .killed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return None;
            }
            if self.owner_dropped() {
                self.gateway.kill();
                return None;
            }
            let gateway = self.gateway.current.load_full()?;
            let mut event = pin!(gateway.events.next_query().fuse());
            let mut turn = pin!(self.wait_for_gateway_turn().fuse());
            futures::select! {
                event = event => return Some(event),
                turn = turn => {
                    let turn = turn?;
                    self.execute_gateway_turn(turn).await?;
                },
            }
        }
    }
}
