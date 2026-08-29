//! Discord cache-first query adapter.
//!
//! Ready-owned caches are authoritative; cold-start REST responses go only to
//! caller and never overwrite them. Query methods never consume gateway frames:
//! root event streams drive cache. Failed REST briefly waits for externally
//! driven Ready state before returning original error. Message-history races are
//! handled in `text.rs`; see `crate/messenger_interface/docs/races.md`.
use crate::{
    DiscordRuntimeState, InnerDiscord,
    api::{self, SNOWFLAKE},
    cache::{ChannelRoute, DiscordCache},
    downloaders::CdnImage,
    rest_api::DiscordApi,
};

use async_trait::async_trait;
use futures::{FutureExt as _, StreamExt as _, future::Either, stream};
use futures_timer::Delay;
use messenger_interface::{
    interface::{Query, QueryEvent},
    owner::Retained,
    stream::EventFuture,
    types::{Details, House, Identifier, Place, Room, RoomCapabilities, User},
};
use tracing::{error, warn};

use std::{error::Error, pin::pin, time::Duration};

const GATEWAY_CACHE_POLL_ATTEMPTS: usize = 8;
const GATEWAY_CACHE_POLL_TIMEOUT: Duration = Duration::from_millis(500);
const ASSET_FETCH_CONCURRENCY: usize = 16;

/// Waits briefly for externally-driven gateway cache.
async fn wait_for_gateway_cache(
    discord: &InnerDiscord,
    ready: impl Fn(&DiscordRuntimeState) -> bool,
) -> bool {
    let runtime = discord.runtime();
    if ready(runtime) {
        return true;
    }
    if let Err(err) = discord.ensure_gateway().await {
        warn!("Discord: failed to open gateway while waiting for cache: {err}");
        return false;
    }

    for _ in 0..GATEWAY_CACHE_POLL_ATTEMPTS {
        if ready(runtime) {
            return true;
        }
        Delay::new(GATEWAY_CACHE_POLL_TIMEOUT).await;
    }

    ready(runtime)
}

/// Extracts creation time encoded after Discord epoch.
fn snowflake_created(id: SNOWFLAKE) -> Option<chrono::DateTime<chrono::Utc>> {
    const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
    chrono::DateTime::from_timestamp_millis(((id >> 22) + DISCORD_EPOCH_MS) as i64)
}

/// Borrowed REST and cache dependencies.
struct QueryContext<'a> {
    api: &'a DiscordApi,
    cache: &'a DiscordCache,
}

impl<'a> QueryContext<'a> {
    fn new(api: &'a DiscordApi, cache: &'a DiscordCache) -> Self {
        Self { api, cache }
    }

    async fn process_dm_channels(&self, channels: &[api::Channel]) -> Vec<Identifier<Place<Room>>> {
        // Higher snowflake means newer message.
        let mut sorted = channels.to_vec();
        sorted.sort_by_key(|channel| std::cmp::Reverse(self.cache.last_message_id(channel)));

        let rooms_producer = sorted.into_iter().map(|channel| async move {
            let place_room = channel.to_room_data().await;
            (Identifier::new(channel.id, place_room), channel.guild_id)
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

            Identifier::new(
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

            Identifier::new(
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

        // Sort categories, grouped children, then top-level channels.
        let mut sorted: Vec<&api::Channel> = channels.iter().collect();
        sorted.sort_by_key(|channel| {
            let own_pos = channel.position.unwrap_or(0);
            let is_category = matches!(channel.channel_type, api::ChannelTypes::GuildCategory);

            match channel
                .parent_id
                .as_ref()
                .and_then(|pid| pid.parse::<SNOWFLAKE>().ok())
            {
                Some(parent_id) => {
                    let parent_pos = category_positions.get(&parent_id).copied().unwrap_or(0);
                    (parent_pos, 1, own_pos)
                }
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
            .map(|channel| {
                // The gateway/REST response is already scoped to this
                // authenticated account. Do not treat an arbitrary deny
                // overwrite as authoritative: Discord permissions require
                // combining @everyone, every role, member-specific allows and
                // administrator/owner state. The old any-deny check hid
                // channels even when a later allow made them visible.
                let channel_name = channel
                    .name
                    .clone()
                    .unwrap_or_else(|| "Unknown".to_string());
                // Seed current voice roster from authoritative cache.
                let participants = self.cache.voice.participants(channel.id);
                let identifier = Identifier::new(
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
                // Nested Ready channels omit guild_id; preserve parent.
                self.cache
                    .channel_routes
                    .insert(*identifier.id(), ChannelRoute::new(Some(guild_id)));
                identifier
            })
            .collect::<Vec<_>>()
    }

    async fn process_profile(&self, profile: &api::Profile) -> Identifier<User> {
        let icon = match &profile.avatar {
            Some(hash) => CdnImage::avatar(profile.id, hash).fetch().await.ok(),
            None => None,
        };

        Identifier::new(
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
impl Query for InnerDiscord {
    async fn client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        let runtime = self.runtime();
        let query = QueryContext::new(&self.api, &runtime.cache);
        if let Some(profile) = query.cache.profile.load_full() {
            return Ok(query.process_profile(&profile).await);
        }
        let rest_err = match query.api.rest_get_profile().await {
            Ok(profile) => return Ok(query.process_profile(&profile).await),
            Err(err) => err,
        };

        warn!("Discord: REST profile fetch failed; waiting for gateway Ready cache: {rest_err}");
        if wait_for_gateway_cache(self, |runtime| runtime.cache.profile.load().is_some()).await
            && let Some(profile) = runtime.cache.profile.load_full()
        {
            return Ok(query.process_profile(&profile).await);
        }

        Err(rest_err)
    }

    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        let runtime = self.runtime();
        let query = QueryContext::new(&self.api, &runtime.cache);
        if let Some(cached) = query.cache.relationships.load_full() {
            return Ok(query.process_contacts(&cached).await);
        }
        let rest_err = match query.api.rest_get_contacts().await {
            Ok(friends) => return Ok(query.process_contacts(&friends).await),
            Err(err) => err,
        };

        warn!("Discord: REST contacts fetch failed; waiting for gateway Ready cache: {rest_err}");
        if wait_for_gateway_cache(self, |runtime| runtime.cache.relationships.load().is_some())
            .await
            && let Some(cached) = runtime.cache.relationships.load_full()
        {
            return Ok(query.process_contacts(&cached).await);
        }

        Err(rest_err)
    }

    async fn rooms(&self) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        let runtime = self.runtime();
        let query = QueryContext::new(&self.api, &runtime.cache);
        if let Some(cached) = query.cache.dm_channels.load_full() {
            return Ok(query.process_dm_channels(&cached).await);
        }
        let rest_err = match query.api.rest_get_dms().await {
            Ok(channels) => return Ok(query.process_dm_channels(&channels).await),
            Err(err) => err,
        };

        warn!("Discord: REST DM fetch failed; waiting for gateway Ready cache: {rest_err}");
        if wait_for_gateway_cache(self, |runtime| runtime.cache.dm_channels.load().is_some()).await
            && let Some(cached) = runtime.cache.dm_channels.load_full()
        {
            return Ok(query.process_dm_channels(&cached).await);
        }

        Err(rest_err)
    }

    async fn houses(&self) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        let runtime = self.runtime();
        let query = QueryContext::new(&self.api, &runtime.cache);
        if let Some(cached) = query.cache.guilds.load_full() {
            return Ok(query.process_guilds(&cached).await);
        }
        let rest_err = match query.api.rest_get_guilds().await {
            Ok(guilds) => return Ok(query.process_guilds(&guilds).await),
            Err(err) => err,
        };

        warn!("Discord: REST guild fetch failed; waiting for gateway Ready cache: {rest_err}");
        if wait_for_gateway_cache(self, |runtime| runtime.cache.guilds.load().is_some()).await
            && let Some(cached) = runtime.cache.guilds.load_full()
        {
            return Ok(query.process_guilds(&cached).await);
        }

        Err(rest_err)
    }

    async fn house_details(
        &self,
        house: Identifier<Place<House>>,
    ) -> Result<House, Box<dyn Error + Sync + Send>> {
        let guild_id = *house.id();
        let query = QueryContext::new(&self.api, &self.runtime().cache);

        let rooms = if let Some(cached) = query.cache.guild_channels.get(&guild_id) {
            query.process_guild_channels(guild_id, &cached)
        } else {
            let channels = query.api.rest_get_guild_channels(guild_id).await?;
            query.process_guild_channels(guild_id, &channels)
        };

        Ok(House::new(Some(rooms)))
    }

    async fn inspect_user(
        &self,
        user: &Identifier<User>,
    ) -> Result<Details, Box<dyn Error + Sync + Send>> {
        // Interface user IDs are raw Discord snowflakes.
        let user_id = *user.id();
        let mut details = Details::default();
        let query = QueryContext::new(&self.api, &self.runtime().cache);

        // Rich profile requires mutual context; fall back to public user.
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
        let query = QueryContext::new(&self.api, &self.runtime().cache);

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

    async fn prepare_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        self.ensure_gateway().await
    }

    fn next_event(self: Retained<Self>) -> EventFuture<QueryEvent> {
        Box::pin(async move {
            let mut retained = self;
            loop {
                if !retained.is_active() {
                    return None;
                }
                if retained.get().runtime().take_realtime_state_invalidation() {
                    return Some(QueryEvent::StateInvalidated);
                }
                if retained
                    .get()
                    .runtime()
                    .gateway
                    .current
                    .load_full()
                    .is_none()
                {
                    retained.get().retry_gateway_after_loss().await;
                    continue;
                }
                let selected = {
                    let runtime = retained.get().runtime();
                    let Some(gateway) = runtime.gateway.current.load_full() else {
                        continue;
                    };
                    let mut event = pin!(gateway.events.next_query().fuse());
                    let mut turn = pin!(runtime.wait_for_gateway_turn().fuse());
                    futures::select! {
                        event = event => Either::Left(event),
                        turn = turn => Either::Right(turn),
                    }
                };
                match selected {
                    Either::Left(event) => return event,
                    Either::Right(turn) => {
                        let Some(turn) = turn else {
                            continue;
                        };
                        retained = InnerDiscord::finish_gateway_turn(retained, turn).await?;
                    }
                }
            }
        })
    }
}
