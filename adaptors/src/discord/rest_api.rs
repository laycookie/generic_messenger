use async_trait::async_trait;
use futures::future::join_all;
use std::error::Error;

use crate::{
    MessangerQuery, ParameterizedMessangerQuery,
    network::{cache_download, http_request},
    types::{Chan, Identifier, Msg, Server, Usr},
};

use super::{
    Discord,
    json_structs::{Channel, CreateMessage, Friend, Guild, Message, Profile},
};

impl Discord {
    fn get_auth_header(&self) -> Vec<(&str, String)> {
        vec![("Authorization", self.token.clone())]
    }
}

#[async_trait]
impl MessangerQuery for Discord {
    async fn get_profile(&self) -> Result<Identifier<Usr>, Box<dyn Error + Sync + Send>> {
        let profile = http_request::<Profile>(
            surf::get("https://discord.com/api/v9/users/@me"),
            self.get_auth_header(),
        )
        .await?;

        Ok(Identifier {
            id: profile.id.clone(),
            hash: None,
            data: Usr {
                name: profile.username.clone(),
                icon: None,
            },
        })
    }
    async fn get_contacts(&self) -> Result<Vec<Identifier<Usr>>, Box<dyn Error + Sync + Send>> {
        let friends = http_request::<Vec<Friend>>(
            surf::get("https://discord.com/api/v9/users/@me/relationships"),
            self.get_auth_header(),
        )
        .await?;
        println!("Friends: {:?}", friends);
        Ok(friends
            .iter()
            .map(|friend| Identifier {
                id: friend.id.clone(),
                hash: None,
                data: Usr {
                    name: friend.user.username.clone(),
                    icon: None,
                },
            })
            .collect())
    }
    async fn get_conversation(
        &self,
    ) -> Result<Vec<Identifier<Chan>>, Box<dyn Error + Sync + Send>> {
        let channels = http_request::<Vec<Channel>>(
            surf::get("https://discord.com/api/v10/users/@me/channels"),
            self.get_auth_header(),
        )
        .await?;

        let conversations = channels
            .iter()
            .map(async move |channel| {
                let mut id = Identifier {
                    id: channel.id.clone(),
                    hash: None,
                    data: Chan {
                        name: channel
                              .clone()
                              .name
                              .unwrap_or(match channel.recipients.get(0) {
                                    Some(test) => test.username.clone(),
                                    None => "Fix later".to_string(),
                        }),
                        icon: None,
                        particepents: Vec::new(),
                    }
                };

                // If channel has icon, insert that, and return it
                if let Some(hash) = &channel.icon {
                    let icon = cache_download(
                        format!(
                            "https://cdn.discordapp.com/channel-icons/{}/{}.webp?size=80&quality=lossless",
                            channel.id, hash
                        ),
                        format!("./cache/imgs/channels/discord/{}", channel.id).into(),
                        format!("{}.webp", hash),
                    )
                    .await;
                    match icon {
                        Ok(path) => {
                            id.data.icon = Some(path);
                            return id;
                        }
                        Err(e) => {
                            eprintln!("Failed to download icon for channel: {}\n{}", id.data.name, e);
                        }
                    };
                }

                // If first recipient has a profile picture, insert that, and return
                let first_recipients = &channel.recipients[0];
                if let Some(hash) = &first_recipients.avatar {
                    let icon = cache_download(
                        format!(
                            "https://cdn.discordapp.com/avatars/{}/{}.webp?size=80&quality=lossless",
                            first_recipients.id, hash
                        ),
                        format!("./cache/imgs/users/discord/{}", channel.id).into(),
                        format!("{}.webp", hash),
                    )
                    .await;
                    match icon {
                        Ok(path) => {
                            id.data.icon = Some(path);
                            return id;
                        }
                        Err(e) => {
                            eprintln!("Failed to download icon for channel: {}\n{}", id.data.name, e);
                        }
                    };
                };

                id
            })
            .collect::<Vec<_>>();
        let b = join_all(conversations).await;

        *self.dms.write().unwrap() = channels;

        Ok(b)
    }
    async fn get_guilds(&self) -> Result<Vec<Identifier<Server>>, Box<dyn Error + Sync + Send>> {
        let guilds = http_request::<Vec<Guild>>(
            surf::get("https://discord.com/api/v10/users/@me/guilds"),
            self.get_auth_header(),
        )
        .await?;

        let g = guilds.iter().map(async move |g| {
            let icon = g.icon.as_ref().map(async move |hash| {
                let icon = cache_download(
                    format!(
                        "https://cdn.discordapp.com/icons/{}/{}.webp?size=80&quality=lossless",
                        g.id, hash
                    ),
                    format!("./cache/imgs/guilds/discord/{}", g.id).into(),
                    format!("{}.webp", hash),
                )
                .await;
                match icon {
                    Ok(path) => Some(path),
                    Err(e) => {
                        eprintln!("Failed to download icon for guild: {}\n{}", g.name, e);
                        None
                    }
                }
            });

            Identifier {
                id: g.id.clone(),
                hash: None,
                data: Server {
                    name: g.name.clone(),
                    icon: match icon {
                        Some(icon) => icon.await,
                        None => None,
                    },
                },
            }
        });
        let b = join_all(g).await;
        *self.guilds.write().unwrap() = guilds;

        Ok(b)
    }
}

#[async_trait]
impl ParameterizedMessangerQuery for Discord {
    // Docs: https://discord.com/developers/docs/resources/channel#get-channel
    // https://discord.com/developers/docs/resources/message#get-channel-message
    async fn get_messanges(
        &self,
        msgs_location: &Identifier<Chan>,
        load_from_msg: Option<Identifier<Msg>>,
    ) -> Result<Vec<Identifier<Msg>>, Box<dyn Error + Sync + Send>> {
        let before = match load_from_msg {
            Some(msg) => format!("?{}", msg.id),
            None => "".to_string(),
        };

        let messages = http_request::<Vec<Message>>(
            surf::get(format!(
                "https://discord.com/api/v10/channels/{}/messages{}",
                msgs_location.id, before,
            )),
            self.get_auth_header(),
        )
        .await?;

        Ok(messages
            .iter()
            .rev()
            .map(|message| Identifier {
                id: message.id.clone(),
                hash: None,
                data: Msg {
                    author: Identifier {
                        id: message.author.id.clone(),
                        hash: None,
                        data: Usr {
                            name: message.author.username.clone(),
                            icon: None, // TODO
                        },
                    },
                    text: message.content.clone(),
                },
            })
            .collect())
    }

    // Docs: https://discord.com/developers/docs/resources/message#create-message
    async fn send_message(
        &self,
        location: &Identifier<Chan>,
        contents: String,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let message = CreateMessage {
            content: Some(contents),
            nonce: None,
            enforce_nonce: None,
            tts: Some(false),
            flags: Some(0),
            mobile_network_type: None,
        };

        let req = http_request::<Vec<Message>>(
            surf::post(format!(
                "https://discord.com/api/v9/channels/{}/messages",
                location.id,
            ))
            .body_json(&message)
            .unwrap(),
            self.get_auth_header(),
        )
        .await;

        Ok(())
    }
}
