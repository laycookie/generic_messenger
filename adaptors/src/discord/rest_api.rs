use async_trait::async_trait;
use futures::future::join_all;
use std::error::Error;

use crate::{
    MessangerQuery, ParameterizedMessangerQuery,
    network::{cache_download, http_request},
    types::{Message as GlobalMessage, MsgsStore, User},
};

use super::{
    Discord,
    json_structs::{Channel, Friend, Guild, Message, Profile},
};

impl Discord {
    fn get_auth_header(&self) -> Vec<(&str, String)> {
        vec![("Authorization", self.token.clone())]
    }
}

#[async_trait]
impl MessangerQuery for Discord {
    async fn get_profile(&self) -> Result<User, Box<dyn Error + Sync + Send>> {
        let profile = http_request::<Profile>(
            surf::get("https://discord.com/api/v9/users/@me"),
            self.get_auth_header(),
        )
        .await?;

        Ok(profile.into())
    }
    async fn get_contacts(&self) -> Result<Vec<User>, Box<dyn Error + Sync + Send>> {
        let friends = http_request::<Vec<Friend>>(
            surf::get("https://discord.com/api/v9/users/@me/relationships"),
            self.get_auth_header(),
        )
        .await?;
        Ok(friends.iter().map(|friend| friend.clone().into()).collect())
    }
    async fn get_conversation(&self) -> Result<Vec<MsgsStore>, Box<dyn Error + Sync + Send>> {
        let channels = http_request::<Vec<Channel>>(
            surf::get("https://discord.com/api/v10/users/@me/channels"),
            self.get_auth_header(),
        )
        .await?;

        let conversations = channels
            .iter()
            .map(|channel| MsgsStore {
                origin_uuid: self.uuid,
                id: channel.id.clone(),
                name: channel
                    .clone()
                    .name
                    .unwrap_or(match channel.recipients.get(0) {
                        Some(test) => test.username.clone(),
                        None => "Fix later".to_string(),
                    }),
                icon: None,
            })
            .collect::<Vec<_>>();

        *self.dms.write().unwrap() = channels;
        // self.dms.set(channels);

        Ok(conversations)
    }
    async fn get_guilds(&self) -> Result<Vec<MsgsStore>, Box<dyn Error + Sync + Send>> {
        let guilds = http_request::<Vec<Guild>>(
            surf::get("https://discord.com/api/v10/users/@me/guilds"),
            self.get_auth_header(),
        )
        .await?;

        let a = guilds.iter().map(async move |g| {
            let Some(hash) = &g.icon else {
                return MsgsStore {
                    origin_uuid: self.uuid,
                    id: g.id.clone(),
                    name: g.name.clone(),
                    icon: None,
                };
            };

            // TODO: Deal with this possibly failing
            let icon = cache_download(
                format!(
                    "https://cdn.discordapp.com/icons/{}/{}.webp?size=80&quality=lossless",
                    g.id, hash
                ),
                format!("./cache/discord/guilds/{}/imgs/", g.id).into(),
                format!("{}.webp", hash),
            )
            .await;

            MsgsStore {
                origin_uuid: self.uuid,
                // hash: None,
                id: g.id.clone(),
                name: g.name.clone(),
                icon: match icon {
                    Ok(path) => Some(path),
                    Err(e) => {
                        eprintln!("Failed to download icon for guild: {}\n{}", g.name, e);
                        None
                    }
                },
            }
        });

        Ok(join_all(a).await)
    }
}

#[async_trait]
impl ParameterizedMessangerQuery for Discord {
    async fn get_messanges(
        &self,
        msgs_location: &MsgsStore,
        load_from_msg: Option<GlobalMessage>,
    ) -> Result<Vec<GlobalMessage>, Box<dyn Error + Sync + Send>> {
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

        Ok(messages.iter().map(|message| message.into()).collect())
    }
}
