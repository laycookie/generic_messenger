//! Thin wrappers around Discord REST API endpoints.
//!
//! Each function here performs a single HTTP request and returns the decoded
//! raw `api_types::*` payload (or `()` for endpoints that have no response
//! body). Anything stateful — cache lookups, mapping inserts, conversion to
//! `messenger_interface` types — belongs in `query.rs`.

use std::error::Error;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

use crate::{
    DISCORD_API, InnerDiscord, Owned,
    api_types::{self, SNOWFLAKE},
    downloaders::http_request,
};

impl InnerDiscord<Owned> {
    pub(crate) fn get_auth_header(&self) -> Vec<(&str, String)> {
        vec![("Authorization", self.token.unsecure().to_string())]
    }

    pub(crate) async fn rest_get_profile(
        &self,
    ) -> Result<api_types::Profile, Box<dyn Error + Sync + Send>> {
        http_request::<api_types::Profile>(
            surf::get(format!("{DISCORD_API}/users/@me")),
            self.get_auth_header(),
        )
        .await
    }

    pub(crate) async fn rest_get_contacts(
        &self,
    ) -> Result<Vec<api_types::Friend>, Box<dyn Error + Sync + Send>> {
        http_request::<Vec<api_types::Friend>>(
            surf::get(format!("{DISCORD_API}/users/@me/relationships")),
            self.get_auth_header(),
        )
        .await
    }

    pub(crate) async fn rest_get_dms(
        &self,
    ) -> Result<Vec<api_types::Channel>, Box<dyn Error + Sync + Send>> {
        http_request::<Vec<api_types::Channel>>(
            surf::get(format!("{DISCORD_API}/users/@me/channels")),
            self.get_auth_header(),
        )
        .await
    }

    pub(crate) async fn rest_get_guilds(
        &self,
    ) -> Result<Vec<api_types::Guild>, Box<dyn Error + Sync + Send>> {
        http_request::<Vec<api_types::Guild>>(
            surf::get(format!("{DISCORD_API}/users/@me/guilds")),
            self.get_auth_header(),
        )
        .await
    }

    pub(crate) async fn rest_get_guild_channels(
        &self,
        guild_id: SNOWFLAKE,
    ) -> Result<Vec<api_types::Channel>, Box<dyn Error + Sync + Send>> {
        http_request::<Vec<api_types::Channel>>(
            surf::get(format!("{DISCORD_API}/guilds/{guild_id}/channels")),
            self.get_auth_header(),
        )
        .await
    }

    pub(crate) async fn rest_get_messages(
        &self,
        channel_id: SNOWFLAKE,
        before: Option<SNOWFLAKE>,
    ) -> Result<Vec<api_types::Message>, Box<dyn Error + Sync + Send>> {
        let before_param = match before {
            Some(msg_id) => format!("?before={msg_id}"),
            None => String::new(),
        };
        http_request::<Vec<api_types::Message>>(
            surf::get(format!(
                "{DISCORD_API}/channels/{channel_id}/messages{before_param}"
            )),
            self.get_auth_header(),
        )
        .await
    }

    // Docs: https://discord.com/developers/docs/resources/message#create-reaction
    pub(crate) async fn rest_add_reaction(
        &self,
        channel_id: SNOWFLAKE,
        message_id: SNOWFLAKE,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let encoded_emoji = utf8_percent_encode(emoji, NON_ALPHANUMERIC);
        let url = format!(
            "{DISCORD_API}/channels/{channel_id}/messages/{message_id}/reactions/{encoded_emoji}/@me"
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
    pub(crate) async fn rest_remove_reaction(
        &self,
        channel_id: SNOWFLAKE,
        message_id: SNOWFLAKE,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let encoded_emoji = utf8_percent_encode(emoji, NON_ALPHANUMERIC);
        let url = format!(
            "{DISCORD_API}/channels/{channel_id}/messages/{message_id}/reactions/{encoded_emoji}/@me"
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
    pub(crate) async fn rest_send_message(
        &self,
        channel_id: SNOWFLAKE,
        content: String,
    ) -> Result<api_types::Message, Box<dyn Error + Sync + Send>> {
        let message = api_types::CreateMessage {
            content: Some(content),
            nonce: None,
            enforce_nonce: None,
            tts: Some(false),
            flags: Some(0),
        };
        let msg_string = facet_json::to_vec(&message)?;
        http_request::<api_types::Message>(
            surf::post(format!("{DISCORD_API}/channels/{channel_id}/messages"))
                .body(msg_string)
                .content_type("application/json"),
            self.get_auth_header(),
        )
        .await
    }
}
