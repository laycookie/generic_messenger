//! High-level query layer.
//!
//! This module implements the `Query` and `Text` traits from
//! `messenger_interface` on top of the raw REST calls in `rest_api.rs` and
//! the cached state populated by gateway events. It is also the place where
//! data returned to the UI gets reconciled against the cache so that
//! HTTP results and gateway events stay coherent.
use crate::{
    AudioDiscord, Discord, InnerDiscord, Owned, QueryDiscord, TextDiscord, VoiceDiscord,
    api_types::{self, SNOWFLAKE},
    downloaders::cache_cdn_image,
};
use async_trait::async_trait;
use futures::future::join_all;
use messenger_interface::{
    interface::{AudioEvent, Ordering, Query, QueryEvent, Text, TextEvent, VoiceEvent},
    stream::{ArcStream, WeakSocketStream},
    types::{
        CacheCategory, House, Identifier, Message, Place, Reaction, Room, RoomCapabilities, User,
    },
};
use tracing::{error, warn};

use std::{error::Error, sync::Arc};

impl InnerDiscord<Owned> {
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

    async fn process_guilds(&self, guilds: &[api_types::Guild]) -> Vec<Identifier<Place<House>>> {
        let house_producer = guilds.iter().map(async move |guild| {
            let icon = guild.icon.as_ref().map(async move |hash| {
                match cache_cdn_image("icons", CacheCategory::Servers, guild.id, hash).await {
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
                let identifier = Discord::identifier_generator(
                    channel.id,
                    Place::new(
                        channel_name,
                        None,
                        Room::new(
                            RoomCapabilities::from(channel.channel_type),
                            Some(Vec::new()),
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
            Some(hash) => cache_cdn_image("avatars", CacheCategory::Users, profile.id, hash)
                .await
                .ok(),
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
        let profile = self.rest_get_profile().await?;
        Ok(self.process_profile(&profile).await)
    }

    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        let friends = self.rest_get_contacts().await?;

        let contact_producer = friends
            .iter()
            .map(async move |friend| {
                let hash = match &friend.user.avatar {
                    Some(hash) => cache_cdn_image("avatars", CacheCategory::Users, friend.id, hash)
                        .await
                        .ok(),
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

        Ok(join_all(contact_producer).await)
    }

    async fn rooms(&self) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        if let Some(cached) = self.dm_channels.load_full() {
            return Ok(self.process_dm_channels(&cached).await);
        }
        let channels = self.rest_get_dms().await?;
        Ok(self.process_dm_channels(&channels).await)
    }

    async fn houses(&self) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        if let Some(cached) = self.guilds.load_full() {
            return Ok(self.process_guilds(&cached).await);
        }
        let guilds = self.rest_get_guilds().await?;
        Ok(self.process_guilds(&guilds).await)
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
impl Text for InnerDiscord<Owned> {
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
        ordering: Ordering,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
        let channel_location = *self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?;

        let before = match load_messages_before {
            Some(msg) => Some(
                *self
                    .message_id_mappings
                    .get(msg.id())
                    .ok_or("No discord message id mapping for before-pagination")?,
            ),
            None => None,
        };

        let messages = self
            .rest_get_messages(channel_location.channel_id(), before)
            .await?;

        // Drop any messages that a concurrent MessageDelete gateway event
        // has already tombstoned — Discord's REST view occasionally returns
        // deleted messages before catching up. See
        // `crate/messenger_interface/docs/races.md`.
        let messages = messages
            .into_iter()
            .filter(|m| !self.deleted_message_ids.contains(m.id));

        // Discord returns messages newest-first. For `Ordering::Time` we
        // reverse so callers get oldest-first; `Unordered` keeps the
        // newest-first arrival order.
        let messages_iter: Box<dyn Iterator<Item = api_types::Message> + Send> = match ordering {
            Ordering::Time => Box::new(messages.into_iter().rev()),
            Ordering::Unordered => Box::new(messages.into_iter()),
        };
        let identifiers = join_all(messages_iter.map(async |message| {
            let (content, history) = message.revisions();
            let reactions = message
                .reactions
                .unwrap_or(Vec::new())
                .iter()
                .map(|reaction| Reaction {
                    emoji: reaction.emoji.name.clone(),
                    count: reaction.count,
                    reacted: reaction.me,
                })
                .collect();

            let icon = match &message.author.avatar {
                Some(hash) => {
                    cache_cdn_image("avatars", CacheCategory::Users, message.author.id, hash)
                        .await
                        .ok()
                }
                None => None,
            };

            let author = messenger_interface::types::Identifier::new(
                message.author.id,
                messenger_interface::types::User {
                    name: message.author.username,
                    icon,
                },
            );
            let identifier = Discord::identifier_generator(
                message.id,
                Message {
                    content,
                    history,
                    reactions,
                    author: Some(author),
                },
            );

            (identifier, message.id)
        }))
        .await;

        Ok(identifiers
            .into_iter()
            .map(|(identifier, discord_msg_id)| {
                self.message_id_mappings
                    .insert(*identifier.id(), discord_msg_id);
                identifier
            })
            .collect())
    }

    async fn add_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let channel_location = *self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let msg_id = *self
            .message_id_mappings
            .get(message.id())
            .ok_or("No discord message id mapping")?;
        self.rest_add_reaction(channel_location.channel_id(), msg_id, emoji)
            .await
    }

    async fn remove_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let channel_location = *self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let msg_id = *self
            .message_id_mappings
            .get(message.id())
            .ok_or("No discord message id mapping")?;
        self.rest_remove_reaction(channel_location.channel_id(), msg_id, emoji)
            .await
    }

    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
        let channel_location = *self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?;

        let msg = self
            .rest_send_message(channel_location.channel_id(), contents.content.text)
            .await?;

        let (content, history) = msg.revisions();
        let icon = match &msg.author.avatar {
            Some(hash) => cache_cdn_image("avatars", CacheCategory::Users, msg.author.id, hash)
                .await
                .ok(),
            None => None,
        };
        let author = Identifier::new(
            msg.author.id,
            User {
                name: msg.author.username,
                icon,
            },
        );

        Ok(Identifier::new(
            msg.id,
            Message {
                content,
                history,
                reactions: Vec::new(),
                author: Some(author),
            },
        ))
    }
    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<TextEvent>, Box<dyn Error + Sync + Send>> {
        self.listen_as::<TextDiscord, _>().await
    }
}

#[async_trait]
impl ArcStream for InnerDiscord<QueryDiscord> {
    type Item = QueryEvent;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item> {
        loop {
            if let Some(event) = self.query_events.pop() {
                return Some(event);
            }
            self.poll_for_events().await;
        }
    }
}
#[async_trait]
impl ArcStream for InnerDiscord<TextDiscord> {
    type Item = TextEvent;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item> {
        loop {
            if let Some(event) = self.text_events.pop() {
                return Some(event);
            }
            self.poll_for_events().await;
        }
    }
}
#[async_trait]
impl ArcStream for InnerDiscord<VoiceDiscord> {
    type Item = VoiceEvent;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item> {
        loop {
            if let Some(event) = self.voice_events.pop() {
                return Some(event);
            }
            self.poll_for_events().await;
        }
    }
}

#[async_trait]
impl ArcStream for InnerDiscord<AudioDiscord> {
    type Item = AudioEvent;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item> {
        loop {
            if let Some(event) = self.audio_events.pop() {
                return Some(event);
            }
            self.poll_audio().await?;
        }
    }
}
