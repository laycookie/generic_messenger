use crate::{
    Discord,
    api_types::{self},
    downloaders::{cache_download, http_request},
};
use async_trait::async_trait;
use futures::future::join_all;
use messenger_interface::{
    interface::{MessengerError, Query, Text},
    types::{House, Identifier, Message, Place, Reaction, Room, RoomCapabilities, User},
};
use std::error::Error;
use tracing::error;

impl Discord {
    fn get_auth_header(&self) -> Vec<(&str, String)> {
        vec![("Authorization", self.token.unsecure().to_string())]
    }
}

impl Discord {
    async fn fetch_dms(
        &self,
    ) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        // DMs / group DMs
        let channels = http_request::<Vec<api_types::Channel>>(
            surf::get("https://discord.com/api/v10/users/@me/channels"),
            self.get_auth_header(),
        )
        .await?;

        let rooms_producer = channels
            .iter()
            .map(async move |channel| {
                let (name, icon, room_data) = channel.to_room_data().await;
                Discord::identifier_generator(
                    channel.id.as_str(),
                    Place::new(name, icon, room_data),
                )
            })
            .collect::<Vec<_>>();

        let places = join_all(rooms_producer).await;

        // Cache mapping internal room id -> discord channel id
        let mut channel_data = self.channel_id_mappings.write().await;
        for (identifier, channel) in places.iter().zip(channels) {
            channel_data.insert(
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
            surf::get("https://discord.com/api/v10/users/@me/guilds"),
            self.get_auth_header(),
        )
        .await?;

        let house_producer = guilds.iter().map(async move |g| {
            let icon = g.icon.as_ref().map(async move |hash| {
                let icon = cache_download(
                    format!(
                        "https://cdn.discordapp.com/icons/{}/{}.webp?size=80&quality=lossless",
                        g.id, hash
                    ),
                    format!("./cache/imgs/guilds/discord/{}", g.id).into(),
                    format!("{hash}.webp"),
                )
                .await;
                match icon {
                    Ok(path) => Some(path),
                    Err(e) => {
                        error!("Failed to download icon for guild: {}\n{}", g.name, e);
                        None
                    }
                }
            });

            // let rooms = self.fetch_guild_channels(&g.id).await.unwrap_or_default();

            Discord::identifier_generator(
                g.id.as_str(),
                Place::new(
                    g.name.clone(),
                    match icon {
                        Some(icon) => icon.await,
                        None => None,
                    },
                    House::new(None),
                ),
            )
        });

        let places = join_all(house_producer).await;

        let mut guild_map = self.guild_id_mappings.write().await;
        for (identifier, guild) in places.iter().zip(guilds) {
            guild_map.insert(*identifier.id(), guild.id);
        }

        Ok(places)
    }

    async fn fetch_guild_channels(
        &self,
        guild_id: &str,
    ) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        let channels = http_request::<Vec<api_types::Channel>>(
            surf::get(format!(
                "https://discord.com/api/v10/guilds/{}/channels",
                guild_id
            )),
            self.get_auth_header(),
        )
        .await?;

        let mut channel_data = self.channel_id_mappings.write().await;
        Ok(channels
            .into_iter()
            .filter_map(|channel| {
                if channel
                    .permission_overwrites
                    .as_ref()?
                    .iter()
                    // TODO: Rewrite
                    .any(|a| a.deny.parse::<u32>().unwrap() & (1 << 10) == (1 << 10))
                {
                    return None;
                };

                let channel_name = channel
                    .name
                    .clone()
                    .unwrap_or_else(|| "Unknown".to_string());
                let identifier = Discord::identifier_generator(
                    channel.id.as_str(),
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
                channel_data.insert(
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
impl Query for Discord {
    async fn client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        let profile = http_request::<api_types::Profile>(
            surf::get("https://discord.com/api/v9/users/@me"),
            self.get_auth_header(),
        )
        .await?;

        let prof = Discord::identifier_generator(
            profile.id.as_str(),
            User {
                name: profile.username.clone(),
                icon: None,
            },
        );

        let mut profile_cache = self.profile.write().await;
        *profile_cache = Some(profile);

        Ok(prof)
    }

    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        let friends = http_request::<Vec<api_types::Friend>>(
            surf::get("https://discord.com/api/v9/users/@me/relationships"),
            self.get_auth_header(),
        )
        .await?;

        let contact_producer = friends
            .iter()
            .map(async move |friend| {
                let hash = match &friend.user.avatar {
                    Some(hash) => {
                        let url = format!(
                            "https://cdn.discordapp.com/avatars/{}/{}.webp?size=80&quality=lossless",
                            friend.id, hash
                        );
                        let dir = format!("./cache/imgs/users/discord/{}", friend.id);
                        let filename = format!("{hash}.webp");

                        cache_download(url, dir.into(), filename).await.ok()
                    }
                    None => None,
                };

                Discord::identifier_generator(
                    friend.id.as_str(),
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
        let rooms = {
            let mapping = self.guild_id_mappings.read().await;
            let guild_id = mapping
                .get(house.id())
                .ok_or("No discord guild id mapping for this house")?;
            self.fetch_guild_channels(guild_id.as_str())
                .await
                .unwrap_or_default()
        };
        Ok(House::new(Some(rooms)))
    }
}

#[async_trait]
impl Text for Discord {
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
        let t = self.channel_id_mappings.read().await;
        let channel_id = t
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?;

        let before = match load_messages_before {
            Some(msg) => {
                let t2 = self.msg_data.read().await;
                let msg_id = t2
                    .get(msg.id())
                    .ok_or("No discord message id mapping for before-pagination")?;
                format!("?before={}", msg_id)
            }
            None => "".to_string(),
        };

        let messages = http_request::<Vec<api_types::Message>>(
            surf::get(format!(
                "https://discord.com/api/v10/channels/{}/messages{}",
                channel_id.id, before,
            )),
            self.get_auth_header(),
        )
        .await?;

        let mut msg_data = self.msg_data.write().await;
        Ok(messages
            .into_iter()
            .rev()
            .map(|message| {
                let reactions = message
                    .reactions
                    .unwrap_or(Vec::new())
                    .iter()
                    .map(|reaction| Reaction {
                        // NOTE: Discord reactions can be custom emojis (name may not be a unicode emoji).
                        // TODO(discord-migration): represent custom emojis (needs richer type than `char`).
                        emoji: reaction.emoji.name.chars().next().unwrap_or('�'),
                        count: reaction.count,
                    })
                    .collect();

                // TODO(discord-migration): messenger_interface::types::Message currently has no author field.
                // If UI needs author, we should reintroduce it in the interface types.
                let identifier = Discord::identifier_generator(
                    message.id.as_str(),
                    Message {
                        text: message.content,
                        reactions,
                    },
                );

                msg_data.insert(*identifier.id(), message.id);
                identifier
            })
            .collect())
    }

    // Docs: https://discord.com/developers/docs/resources/message#create-message
    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let channel_to_id = self.channel_id_mappings.read().await;
        let channel_id = channel_to_id
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?;

        let message = api_types::CreateMessage {
            content: Some(contents.text),
            nonce: None,
            enforce_nonce: None,
            tts: Some(false),
            flags: Some(0),
        };
        let msg_string = facet_json::to_vec(&message).unwrap();

        let _msg = http_request::<api_types::Message>(
            surf::post(format!(
                "https://discord.com/api/v9/channels/{}/messages",
                channel_id.id,
            ))
            .body(msg_string)
            .content_type("application/json"),
            self.get_auth_header(),
        )
        .await?;

        Ok(())
    }
}
