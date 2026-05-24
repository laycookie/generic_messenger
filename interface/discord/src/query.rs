use crate::{
    AudioDiscord, DISCORD_API, Discord, InnerDiscord, Owned, QueryDiscord, TextDiscord,
    VoiceDiscord,
    api_types::{self, SNOWFLAKE},
    downloaders::{cache_cdn_image, http_request},
};
use async_trait::async_trait;
use futures::future::join_all;
use messenger_interface::{
    interface::{AudioEvent, Query, QueryEvent, Text, TextEvent, VoiceEvent},
    stream::{ArcStream, WeakSocketStream},
    types::{
        CacheCategory, House, Identifier, Message, Place, Reaction, Room, RoomCapabilities, User,
    },
};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use tracing::error;

use std::{error::Error, sync::Arc};

impl InnerDiscord<Owned> {
    fn get_auth_header(&self) -> Vec<(&str, String)> {
        vec![("Authorization", self.token.unsecure().to_string())]
    }

    async fn fetch_dms(
        &self,
    ) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        // DMs / group DMs
        let mut channels = http_request::<Vec<api_types::Channel>>(
            surf::get(format!("{DISCORD_API}/users/@me/channels")),
            self.get_auth_header(),
        )
        .await?;

        // Sort by last_message_id descending (most recent first).
        // Discord snowflake IDs encode timestamps, so higher = newer.
        channels.sort_by(|a, b| b.last_message_id.cmp(&a.last_message_id));

        let rooms_producer = channels
            .iter()
            .map(async move |channel| {
                let place_room = channel.to_room_data().await;
                Discord::identifier_generator(channel.id, place_room)
            })
            .collect::<Vec<_>>();

        let places = join_all(rooms_producer).await;

        // Cache mapping internal room id -> discord channel id
        for (identifier, channel) in places.iter().zip(channels.iter()) {
            self.channel_id_mappings.insert(
                *identifier.id(),
                super::ChannelID {
                    guild_id: channel.guild_id,
                    id: channel.id,
                },
            );
        }

        Ok(places)
    }

    async fn fetch_guilds(
        &self,
    ) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        // Guilds / servers
        let guilds = http_request::<Vec<api_types::Guild>>(
            surf::get(format!("{DISCORD_API}/users/@me/guilds")),
            self.get_auth_header(),
        )
        .await?;

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

            // let rooms = self.fetch_guild_channels(&g.id).await.unwrap_or_default();

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

        for (identifier, guild) in places.iter().zip(guilds) {
            self.guild_id_mappings.insert(*identifier.id(), guild.id);
        }

        Ok(places)
    }

    async fn fetch_guild_channels(
        &self,
        guild_id: SNOWFLAKE,
    ) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        let mut channels = http_request::<Vec<api_types::Channel>>(
            surf::get(format!("{DISCORD_API}/guilds/{guild_id}/channels")),
            self.get_auth_header(),
        )
        .await?;

        // Build category position lookup: category_id → position
        let category_positions: std::collections::HashMap<SNOWFLAKE, i32> = channels
            .iter()
            .filter(|c| matches!(c.channel_type, api_types::ChannelTypes::GuildCategory))
            .map(|c| (c.id, c.position.unwrap_or(0)))
            .collect();

        // Sort: categories first by position, then children grouped under
        // their parent category and sorted by their own position.
        // Channels without a parent sort to the top.
        channels.sort_by_key(|channel| {
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

        Ok(channels
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
                self.channel_id_mappings.insert(
                    *identifier.id(),
                    crate::ChannelID {
                        guild_id: channel.guild_id,
                        id: channel.id,
                    },
                );
                Some(identifier)
            })
            .collect::<Vec<_>>())
    }
}

#[async_trait]
impl Query for InnerDiscord<Owned> {
    async fn client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        let profile = http_request::<api_types::Profile>(
            surf::get(format!("{DISCORD_API}/users/@me")),
            self.get_auth_header(),
        )
        .await?;

        let icon = match &profile.avatar {
            Some(hash) => cache_cdn_image("avatars", CacheCategory::Users, profile.id, hash)
                .await
                .ok(),
            None => None,
        };

        let prof = Discord::identifier_generator(
            profile.id,
            User {
                name: profile.username.clone(),
                icon,
            },
        );

        self.profile.store(Some(Arc::new(profile)));

        Ok(prof)
    }

    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        let friends = http_request::<Vec<api_types::Friend>>(
            surf::get(format!("{DISCORD_API}/users/@me/relationships")),
            self.get_auth_header(),
        )
        .await?;

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
        self.fetch_dms().await
    }

    async fn houses(&self) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        self.fetch_guilds().await
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
        let rooms = self.fetch_guild_channels(guild_id).await.unwrap_or_default();
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
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
        let channel_id = self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?
            .clone();

        let before = match load_messages_before {
            Some(msg) => {
                let msg_id = *self
                    .message_id_mappings
                    .get(msg.id())
                    .ok_or("No discord message id mapping for before-pagination")?;
                format!("?before={msg_id}")
            }
            None => String::new(),
        };

        let messages = http_request::<Vec<api_types::Message>>(
            surf::get(format!(
                "{DISCORD_API}/channels/{}/messages{before}",
                channel_id.id,
            )),
            self.get_auth_header(),
        )
        .await?;

        let identifiers = join_all(messages.into_iter().rev().map(async |message| {
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
                    text: message.content,
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

    // Docs: https://discord.com/developers/docs/resources/message#create-reaction
    async fn add_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let channel_id = self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?
            .clone();
        let msg_id = *self
            .message_id_mappings
            .get(message.id())
            .ok_or("No discord message id mapping")?;
        let encoded_emoji = utf8_percent_encode(emoji, NON_ALPHANUMERIC);
        let url = format!(
            "{DISCORD_API}/channels/{}/messages/{msg_id}/reactions/{encoded_emoji}/@me",
            channel_id.id,
        );
        let mut req = surf::put(&url);
        for (key, value) in self.get_auth_header() {
            req = req.header(key, value);
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            return Err(surf::Error::from_str(res.status(), "Failed to add reaction").into());
        }
        Ok(())
    }

    // Docs: https://discord.com/developers/docs/resources/message#delete-own-reaction
    async fn remove_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let channel_id = self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?
            .clone();
        let msg_id = *self
            .message_id_mappings
            .get(message.id())
            .ok_or("No discord message id mapping")?;
        let encoded_emoji = utf8_percent_encode(emoji, NON_ALPHANUMERIC);
        let url = format!(
            "{DISCORD_API}/channels/{}/messages/{msg_id}/reactions/{encoded_emoji}/@me",
            channel_id.id,
        );
        let mut req = surf::delete(&url);
        for (key, value) in self.get_auth_header() {
            req = req.header(key, value);
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            return Err(surf::Error::from_str(res.status(), "Failed to remove reaction").into());
        }
        Ok(())
    }

    // Docs: https://discord.com/developers/docs/resources/message#create-message
    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
        let channel_id = self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?
            .clone();

        let message = api_types::CreateMessage {
            content: Some(contents.text),
            nonce: None,
            enforce_nonce: None,
            tts: Some(false),
            flags: Some(0),
        };
        let msg_string = facet_json::to_vec(&message)?;

        let msg = http_request::<api_types::Message>(
            surf::post(format!("{DISCORD_API}/channels/{}/messages", channel_id.id,))
                .body(msg_string)
                .content_type("application/json"),
            self.get_auth_header(),
        )
        .await?;

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
                text: msg.content,
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
