//! High-level query layer.
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
//! is seeded by the `Ready` gateway dispatch (see `gateways::general::events`),
//! which is currently the **sole** writer for these fields. Subsequent
//! `*_UPDATE` / `*_DELETE` / `*_CREATE` handlers that would mutate these
//! caches in place are not yet wired up, so the cache today reflects the
//! `Ready` snapshot frozen in time and goes progressively stale over the
//! connection's lifetime. Adding those handlers is a tracked correctness
//! task — see `crate/messenger_interface/docs/races.md` ("Audit").
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
//! Practical consequence: handlers for `GUILD_UPDATE`, `CHANNEL_UPDATE`,
//! etc. need to mutate the cache for correctness (otherwise it goes stale
//! over the connection's lifetime), but they do **not** need tombstone
//! rings or per-ID merge policies.
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
    Discord, InnerDiscord, Owned, QueryDiscord, StreamPollGuard,
    api_types::{self, SNOWFLAKE},
    downloaders::CdnImage,
};
use async_trait::async_trait;
use futures::future::{Either, join_all, select};
use futures_timer::Delay;
use messenger_interface::{
    interface::{Query, QueryEvent},
    stream::{ArcStream, WeakSocketStream},
    types::{House, Identifier, Place, Room, RoomCapabilities, User},
};
use tracing::{error, warn};

use std::{error::Error, sync::Arc, time::Duration};

const GATEWAY_CACHE_POLL_ATTEMPTS: usize = 8;
const GATEWAY_CACHE_POLL_TIMEOUT: Duration = Duration::from_millis(500);

impl InnerDiscord<Owned> {
    // TODO: Depricate
    async fn poll_gateway_until(&self, ready: impl Fn(&Self) -> bool) -> bool {
        if ready(self) {
            return true;
        }
        if let Err(err) = self.ensure_gateway().await {
            warn!("Discord: failed to open gateway for REST fallback: {err}");
            return false;
        }

        for _ in 0..GATEWAY_CACHE_POLL_ATTEMPTS {
            if ready(self) {
                return true;
            }

            let poll = self.poll_gateway_cache_event();
            futures::pin_mut!(poll);
            let timeout = Delay::new(GATEWAY_CACHE_POLL_TIMEOUT);
            futures::pin_mut!(timeout);
            match select(poll, timeout).await {
                Either::Left((Some(()), _)) => {}
                Either::Left((None, _)) => return false,
                Either::Right((_, _)) => {}
            }
        }

        ready(self)
    }

    async fn process_dm_channels(
        &self,
        channels: &[api_types::Channel],
    ) -> Vec<Identifier<Place<Room>>> {
        // Sort by last_message_id descending (most recent first).
        // Discord snowflake IDs encode timestamps, so higher = newer.
        let mut sorted: Vec<&api_types::Channel> = channels.iter().collect();
        sorted.sort_by_key(|c| std::cmp::Reverse(c.last_message_id));

        let rooms_producer = sorted.iter().map(async move |channel| {
            let place_room = channel.to_room_data().await;
            Discord::identifier_generator(channel.id, place_room)
        });

        let places = join_all(rooms_producer).await;

        for (identifier, channel) in places.iter().zip(sorted.iter()) {
            // DMs/GroupDMs have no guild; pass None for parent_guild_id.
            if let Some(location) = super::ChannelLocation::from_api(channel, None) {
                self.channel_id_mappings.insert(*identifier.id(), location);
            } else {
                warn!(
                    "DM channel {} produced no ChannelLocation (unexpected channel_type)",
                    channel.id
                );
            }
        }

        places
    }

    async fn process_contacts(&self, friends: &[api_types::Friend]) -> Vec<Identifier<User>> {
        let contact_producer = friends
            .iter()
            .map(async move |friend| {
                let hash = match &friend.user.avatar {
                    Some(hash) => CdnImage::avatar(friend.id, hash).fetch().await.ok(),
                    None => None,
                };

                Discord::identifier_generator(
                    friend.id,
                    User {
                        name: friend.user.username.clone(),
                        icon: hash,
                    },
                )
            })
            .collect::<Vec<_>>();

        join_all(contact_producer).await
    }

    async fn process_guilds(&self, guilds: &[api_types::Guild]) -> Vec<Identifier<Place<House>>> {
        let house_producer = guilds.iter().map(async move |guild| {
            let icon = guild.icon.as_ref().map(async move |hash| {
                match CdnImage::guild_icon(guild.id, hash).fetch().await {
                    Ok(path) => Some(path),
                    Err(e) => {
                        error!("Failed to download icon for guild: {}\n{}", guild.name, e);
                        None
                    }
                }
            });

            Discord::identifier_generator(
                guild.id,
                Place::new(
                    guild.name.clone(),
                    match icon {
                        Some(icon) => icon.await,
                        None => None,
                    },
                    House::new(None),
                ),
            )
        });

        let places = join_all(house_producer).await;

        for (identifier, guild) in places.iter().zip(guilds.iter()) {
            self.guild_id_mappings.insert(*identifier.id(), guild.id);
        }

        places
    }

    fn process_guild_channels(
        &self,
        guild_id: SNOWFLAKE,
        channels: &[api_types::Channel],
    ) -> Vec<Identifier<Place<Room>>> {
        // Build category position lookup: category_id → position
        let category_positions: std::collections::HashMap<SNOWFLAKE, i32> = channels
            .iter()
            .filter(|c| matches!(c.channel_type, api_types::ChannelTypes::GuildCategory))
            .map(|c| (c.id, c.position.unwrap_or(0)))
            .collect();

        // Sort: categories first by position, then children grouped under
        // their parent category and sorted by their own position.
        // Channels without a parent sort to the top.
        let mut sorted: Vec<&api_types::Channel> = channels.iter().collect();
        sorted.sort_by_key(|channel| {
            let own_pos = channel.position.unwrap_or(0);
            let is_category =
                matches!(channel.channel_type, api_types::ChannelTypes::GuildCategory);

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
                let participants = self
                    .voice_participants
                    .get(&channel.id)
                    .map(|entry| entry.clone())
                    .unwrap_or_default();
                let identifier = Discord::identifier_generator(
                    channel.id,
                    Place::new(
                        channel_name,
                        None,
                        Room::new(
                            RoomCapabilities::from(channel.channel_type),
                            Some(participants),
                            None,
                        ),
                    ),
                );
                // Discord omits guild_id on channels nested in Ready/
                // GuildCreate payloads, so pass the parent guild_id in.
                if let Some(location) = crate::ChannelLocation::from_api(channel, Some(guild_id)) {
                    self.channel_id_mappings.insert(*identifier.id(), location);
                }
                Some(identifier)
            })
            .collect::<Vec<_>>()
    }

    async fn process_profile(&self, profile: &api_types::Profile) -> Identifier<User> {
        let icon = match &profile.avatar {
            Some(hash) => CdnImage::avatar(profile.id, hash).fetch().await.ok(),
            None => None,
        };

        Discord::identifier_generator(
            profile.id,
            User {
                name: profile.username.clone(),
                icon,
            },
        )
    }
}

#[async_trait]
impl Query for InnerDiscord<Owned> {
    async fn client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        if let Some(profile) = self.profile.load_full() {
            return Ok(self.process_profile(&profile).await);
        }
        let rest_err = match self.rest_get_profile().await {
            Ok(profile) => return Ok(self.process_profile(&profile).await),
            Err(err) => err,
        };

        warn!("Discord: REST profile fetch failed; trying gateway Ready cache: {rest_err}");
        if self
            .poll_gateway_until(|discord| discord.profile.load().is_some())
            .await
            && let Some(profile) = self.profile.load_full()
        {
            return Ok(self.process_profile(&profile).await);
        }

        Err(rest_err)
    }

    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        if let Some(cached) = self.relationships.load_full() {
            return Ok(self.process_contacts(&cached).await);
        }
        let rest_err = match self.rest_get_contacts().await {
            Ok(friends) => return Ok(self.process_contacts(&friends).await),
            Err(err) => err,
        };

        warn!("Discord: REST contacts fetch failed; trying gateway Ready cache: {rest_err}");
        if self
            .poll_gateway_until(|discord| discord.relationships.load().is_some())
            .await
            && let Some(cached) = self.relationships.load_full()
        {
            return Ok(self.process_contacts(&cached).await);
        }

        Err(rest_err)
    }

    async fn rooms(&self) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        if let Some(cached) = self.dm_channels.load_full() {
            return Ok(self.process_dm_channels(&cached).await);
        }
        let rest_err = match self.rest_get_dms().await {
            Ok(channels) => return Ok(self.process_dm_channels(&channels).await),
            Err(err) => err,
        };

        warn!("Discord: REST DM fetch failed; trying gateway Ready cache: {rest_err}");
        if self
            .poll_gateway_until(|discord| discord.dm_channels.load().is_some())
            .await
            && let Some(cached) = self.dm_channels.load_full()
        {
            return Ok(self.process_dm_channels(&cached).await);
        }

        Err(rest_err)
    }

    async fn houses(&self) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        if let Some(cached) = self.guilds.load_full() {
            return Ok(self.process_guilds(&cached).await);
        }
        let rest_err = match self.rest_get_guilds().await {
            Ok(guilds) => return Ok(self.process_guilds(&guilds).await),
            Err(err) => err,
        };

        warn!("Discord: REST guild fetch failed; trying gateway Ready cache: {rest_err}");
        if self
            .poll_gateway_until(|discord| discord.guilds.load().is_some())
            .await
            && let Some(cached) = self.guilds.load_full()
        {
            return Ok(self.process_guilds(&cached).await);
        }

        Err(rest_err)
    }

    // TODO: Implement room_details and house_details if needed
    // These would fetch detailed information about a specific room/house

    async fn house_details(
        &self,
        house: Identifier<Place<House>>,
    ) -> Result<House, Box<dyn Error + Sync + Send>> {
        let guild_id = *self
            .guild_id_mappings
            .get(house.id())
            .ok_or("No discord guild id mapping for this house")?;

        let rooms = if let Some(cached) = self.guild_channels.get(&guild_id) {
            self.process_guild_channels(guild_id, &cached)
        } else {
            self.rest_get_guild_channels(guild_id)
                .await
                .map(|channels| self.process_guild_channels(guild_id, &channels))
                .unwrap_or_default()
        };

        Ok(House::new(Some(rooms)))
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
        let _guard = StreamPollGuard::new(&self.active_streams);
        loop {
            if self.killed.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            if self.owner_dropped() {
                self.kill();
                return None;
            }
            if let Some(event) = self.query_events.pop() {
                return Some(event);
            }
            self.poll_for_events().await?;
        }
    }
}
