use crate::{
    Discord,
    api_types::{self},
    downloaders::{cache_download, http_request},
};
use async_trait::async_trait;
use futures::future::join_all;
use messaging_interface::{
    interface::{MessangerQuery, ParameterizedMessangerQuery},
    types::{Chan, ChanType, Identifier, Message, MessageContents, Reaction, Server, Usr},
};
use std::{error::Error, path::PathBuf};
use tracing::error;

impl Discord {
    fn get_auth_header(&self) -> Vec<(&str, String)> {
        vec![("Authorization", self.token.unsecure().to_string())]
    }
}

#[async_trait]
impl MessangerQuery for Discord {
    async fn fetch_profile(&self) -> Result<Identifier<Usr>, Box<dyn Error + Sync + Send>> {
        let profile = http_request::<api_types::Profile>(
            surf::get("https://discord.com/api/v9/users/@me"),
            self.get_auth_header(),
        )
        .await?;
        let name = profile.username.clone();

        let prof = Discord::identifier_generator(profile.id.as_str(), Usr { name, icon: None });

        let mut profile_cache = self.profile.write().await;
        *profile_cache = Some(profile);

        Ok(prof)
    }
    async fn fetch_contacts(&self) -> Result<Vec<Identifier<Usr>>, Box<dyn Error + Sync + Send>> {
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
                    None => None
                };
                Discord::identifier_generator(friend.id.as_str(), Usr {
                        name: friend.user.username.clone(),
                        icon: hash,
                    })
            })
            .collect::<Vec<_>>();
        let contacts = join_all(contact_producer).await;
        Ok(contacts)
    }
    async fn fetch_conversation(
        &self,
    ) -> Result<Vec<Identifier<Chan>>, Box<dyn Error + Sync + Send>> {
        let channels = http_request::<Vec<api_types::Channel>>(
            surf::get("https://discord.com/api/v10/users/@me/channels"),
            self.get_auth_header(),
        )
        .await?;

        let conversation_producer = channels
            .iter()
            .map(async move |channel| {
                let mut id = Discord::identifier_generator(channel.id.as_str(), Chan {
                        name: channel
                              .name
                              .to_owned()
                              .unwrap_or(match channel.recipients.as_ref().unwrap_or(&Vec::new()).first() {
                                    Some(test) => test.username.clone(),
                                    None => "Fix later".to_string(),
                        }),
                        icon: None,
                        participants: Vec::new(),
                        chan_type: ChanType::TextAndVoice,
                    });

                // If channel has icon, insert that, and return it
                if let Some(hash) = &channel.icon {
                    let icon = cache_download(
                        format!(
                            "https://cdn.discordapp.com/channel-icons/{}/{}.webp?size=80&quality=lossless",
                            channel.id, hash
                        ),
                        format!("./cache/imgs/channels/discord/{}", channel.id).into(),
                        format!("{hash}.webp"),
                    )
                    .await;
                    match icon {
                        Ok(path) => {
                            id.data.icon = Some(path);
                            return id;
                        }
                        Err(e) => {
                            error!("Failed to download icon for channel: {}\n{}", id.data.name, e);
                        }
                    };
                }

                // If first recipient has a profile picture, insert that, and return
                match channel.recipients.as_ref().unwrap_or(&Vec::new()).first() {
                    Some(first_recipients) => {
                        if let Some(hash) = &first_recipients.avatar {
                            let icon = cache_download(
                                format!(
                                    "https://cdn.discordapp.com/avatars/{}/{}.webp?size=80&quality=lossless",
                                    first_recipients.id, hash
                                ),
                                format!("./cache/imgs/channels/discord/{}", channel.id).into(),
                                format!("{hash}.webp"),
                            )
                            .await;
                            match icon {
                                Ok(path) => {
                                    id.data.icon = Some(path);
                                    return id;
                                }
                                Err(e) => {
                                    error!("Failed to download icon for channel: {}\n{}", id.data.name, e);
                                }
                            };
                        }
                        id
                    },
                    None => id,
                }
            })
            .collect::<Vec<_>>();
        let conversations = join_all(conversation_producer).await;

        let mut channel_data = self.channel_id_mappings.write().await;
        for (identifier, channel) in conversations.iter().zip(channels) {
            channel_data.insert(
                identifier.id,
                super::ChannelID {
                    guild_id: channel.guild_id,
                    id: channel.id,
                },
            );
        }

        Ok(conversations)
    }
    async fn fetch_guilds(&self) -> Result<Vec<Identifier<Server>>, Box<dyn Error + Sync + Send>> {
        let guilds = http_request::<Vec<api_types::Guild>>(
            surf::get("https://discord.com/api/v10/users/@me/guilds"),
            self.get_auth_header(),
        )
        .await?;

        let server_producer = guilds.iter().map(async move |g| {
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

            Discord::identifier_generator(
                g.id.as_str(),
                Server {
                    name: g.name.clone(),
                    icon: match icon {
                        Some(icon) => icon.await,
                        None => None,
                    },
                },
            )
        });
        let servers = join_all(server_producer).await;

        let mut channel_data = self.guild_id_mappings.write().await;
        for (identifier, guild) in servers.iter().zip(guilds) {
            channel_data.insert(identifier.id, guild.id);
        }

        Ok(servers)
    }
}

#[async_trait]
impl ParameterizedMessangerQuery for Discord {
    // Docs: https://discord.com/developers/docs/resources/guild#get-guild
    async fn get_server_conversations(
        &self,
        location: &Identifier<Server>,
    ) -> Vec<Identifier<Chan>> {
        let t = self.guild_id_mappings.read().await;
        let guild_id = t.get(&location.id).unwrap();

        let channels = http_request::<Vec<api_types::Channel>>(
            surf::get(format!(
                "https://discord.com/api/v10/guilds/{}/channels",
                guild_id
            )),
            self.get_auth_header(),
        )
        .await
        .unwrap();

        let mut channel_data = self.channel_id_mappings.write().await;
        channels
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

                let identifier = Discord::identifier_generator(
                    channel.id.as_str(),
                    Chan {
                        name: channel.name.clone().unwrap(),
                        icon: None,
                        participants: Vec::new(),
                        chan_type: match channel.channel_type {
                            api_types::ChannelTypes::GuildCategory => ChanType::Spacer,
                            api_types::ChannelTypes::GuildText => ChanType::Text,
                            api_types::ChannelTypes::GuildAnnouncement => ChanType::Text,
                            api_types::ChannelTypes::GuildVoice => ChanType::Voice,
                            api_types::ChannelTypes::GuildStageVoice => ChanType::Voice,
                            _ => ChanType::Spacer,
                        },
                    },
                );
                channel_data.insert(
                    identifier.id,
                    crate::ChannelID {
                        guild_id: channel.guild_id,
                        id: channel.id,
                    },
                );
                Some(identifier)
            })
            .collect::<Vec<_>>()
    }
    // Docs: https://discord.com/developers/docs/resources/channel#get-channel
    // https://discord.com/developers/docs/resources/message#get-channel-message
    async fn get_messages(
        &self,
        msgs_location: &Identifier<Chan>,
        load_from_msg: Option<Identifier<Message>>,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
        let t = self.channel_id_mappings.read().await;
        let channel_id = t.get(&msgs_location.id).unwrap();

        let before = match load_from_msg {
            Some(msg) => {
                let t2 = self.msg_data.read().await;
                let msg_id = t2.get(&msg.id).unwrap();
                format!("?{}", msg_id)
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

        Ok(messages
            .into_iter()
            .rev()
            .map(|message| {
                //NOTE: I have no idea if this will ever be desynchronized
                //For the time being, I dont want to turn this into asyncrhonous map and use cache_download()
                let icon = message.author.avatar.and_then(|hash| {
                    let path = PathBuf::from(format!(
                        "./cache/imgs/users/discord/{}/{}.webp",
                        message.author.id, hash
                    ));
                    path.exists().then_some(path)
                });

                let reactions = message
                    .reactions
                    .unwrap_or(Vec::new())
                    .iter()
                    .map(|reaction| {
                        Reaction {
                            emoji: reaction.emoji.name.chars().next().unwrap(), // TODO: Will break
                            count: reaction.count,
                        }
                    })
                    .collect();

                Discord::identifier_generator(
                    message.id.as_str(),
                    Message {
                        author: Discord::identifier_generator(
                            message.author.id.as_str(),
                            Usr {
                                name: message.author.username,
                                icon,
                            },
                        ),
                        contents: MessageContents {
                            text: message.content,
                            reactions,
                        },
                    },
                )
            })
            .collect())
    }

    // Docs: https://discord.com/developers/docs/resources/message#create-message
    async fn send_message(
        &self,
        location: &Identifier<Chan>,
        contents: MessageContents,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let channel_to_id = self.channel_id_mappings.read().await;
        let channel_id = channel_to_id.get(&location.id).unwrap();

        let message = api_types::CreateMessage {
            content: Some(contents.text),
            nonce: None,
            enforce_nonce: None,
            tts: Some(false),
            flags: Some(0),
        };
        let msg_string = facet_format_json::to_vec(&message).unwrap();

        let _msgs = http_request::<api_types::Message>(
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
