//! Thin wrappers around Discord REST API endpoints.
//!
//! Each function here performs a single HTTP request and returns the decoded
//! raw `api::*` payload (or `()` for endpoints that have no response
//! body). Anything stateful — cache lookups, mapping inserts, conversion to
//! `messenger_interface` types — belongs in `query.rs`.

use std::error::Error;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use secure_string::SecureString;

use crate::{
    DISCORD_API,
    api::{self, SNOWFLAKE},
    downloaders::{Body as _, Fetch, Fresh},
};

/// Cache-blind Discord HTTP transport. Callers decide whether returned data is
/// ephemeral, projected into interface values, or reconciled into an index.
pub(crate) struct DiscordApi {
    token: SecureString,
}

impl DiscordApi {
    pub(crate) fn new(token: SecureString) -> Self {
        Self { token }
    }

    pub(crate) fn token(&self) -> &SecureString {
        &self.token
    }

    pub(crate) async fn get_auth_header(
        &self,
    ) -> Result<Vec<(&'static str, String)>, Box<dyn Error + Sync + Send>> {
        // <https://docs.discord.food/reference#authentication>
        Ok(vec![(
            "Authorization",
            self.token.unsecure().trim().to_string(),
        )])
    }

    /// `GET /users/@me`
    /// <https://docs.discord.food/resources/user#get-current-user>
    pub(crate) async fn rest_get_profile(
        &self,
    ) -> Result<api::Profile, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/users/@me")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<api::Profile>()
        .await
    }

    /// `GET /users/@me/relationships`
    /// <https://docs.discord.food/resources/relationships#list-relationships>
    pub(crate) async fn rest_get_contacts(
        &self,
    ) -> Result<Vec<api::Friend>, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/users/@me/relationships")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<Vec<api::Friend>>()
        .await
    }

    /// `GET /users/@me/channels`
    /// <https://docs.discord.food/resources/channel#list-private-channels>
    pub(crate) async fn rest_get_dms(
        &self,
    ) -> Result<Vec<api::Channel>, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/users/@me/channels")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<Vec<api::Channel>>()
        .await
    }

    /// `GET /users/@me/guilds`
    /// <https://docs.discord.food/resources/guild#list-user-guilds>
    pub(crate) async fn rest_get_guilds(
        &self,
    ) -> Result<Vec<api::Guild>, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/users/@me/guilds")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<Vec<api::Guild>>()
        .await
    }

    /// `GET /guilds/{guild_id}/channels`
    /// <https://docs.discord.food/resources/channel#list-guild-channels>
    pub(crate) async fn rest_get_guild_channels(
        &self,
        guild_id: SNOWFLAKE,
    ) -> Result<Vec<api::Channel>, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/guilds/{guild_id}/channels")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<Vec<api::Channel>>()
        .await
    }

    /// `GET /channels/{channel_id}/messages`
    /// <https://docs.discord.food/resources/message#list-messages>
    pub(crate) async fn rest_get_messages(
        &self,
        channel_id: SNOWFLAKE,
        before: Option<SNOWFLAKE>,
    ) -> Result<Vec<api::Message>, Box<dyn Error + Sync + Send>> {
        let before_param = match before {
            Some(msg_id) => format!("?before={msg_id}"),
            None => String::new(),
        };
        let url = format!("{DISCORD_API}/channels/{channel_id}/messages{before_param}");
        Fetch::<Fresh>::fetch(|| surf::get(&url), self.get_auth_header().await?)
            .await?
            .json::<Vec<api::Message>>()
            .await
    }

    /// `GET /users/{user_id}`
    /// <https://docs.discord.food/resources/user#get-user>
    pub(crate) async fn rest_get_user(
        &self,
        user_id: SNOWFLAKE,
    ) -> Result<api::UserDetails, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/users/{user_id}")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<api::UserDetails>()
        .await
    }

    /// User-token endpoint; requires a mutual friend/guild with the target.
    /// <https://docs.discord.food/resources/user#get-user-profile>
    pub(crate) async fn rest_get_user_profile(
        &self,
        user_id: SNOWFLAKE,
    ) -> Result<api::UserProfile, Box<dyn Error + Sync + Send>> {
        let url = format!("{DISCORD_API}/users/{user_id}/profile?with_mutual_guilds=true");
        Fetch::<Fresh>::fetch(|| surf::get(&url), self.get_auth_header().await?)
            .await?
            .json::<api::UserProfile>()
            .await
    }

    /// `GET /guilds/{guild_id}?with_counts=true`
    /// <https://docs.discord.food/resources/guild#get-guild>
    pub(crate) async fn rest_get_guild(
        &self,
        guild_id: SNOWFLAKE,
    ) -> Result<api::GuildDetails, Box<dyn Error + Sync + Send>> {
        let url = format!("{DISCORD_API}/guilds/{guild_id}?with_counts=true");
        Fetch::<Fresh>::fetch(|| surf::get(&url), self.get_auth_header().await?)
            .await?
            .json::<api::GuildDetails>()
            .await
    }

    /// `PUT /channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me`
    /// <https://docs.discord.food/resources/message#create-reaction>
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
        Fetch::<Fresh>::fetch(|| surf::put(&url), self.get_auth_header().await?).await?;
        Ok(())
    }

    /// `DELETE /channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me`
    /// <https://docs.discord.food/resources/message#delete-own-reaction>
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
        Fetch::<Fresh>::fetch(|| surf::delete(&url), self.get_auth_header().await?).await?;
        Ok(())
    }

    /// `POST /channels/{channel_id}/messages`
    /// <https://docs.discord.food/resources/message#create-message>
    pub(crate) async fn rest_send_message(
        &self,
        channel_id: SNOWFLAKE,
        content: String,
        sticker_ids: Vec<SNOWFLAKE>,
    ) -> Result<api::Message, Box<dyn Error + Sync + Send>> {
        let message = api::CreateMessage {
            content: Some(content),
            nonce: None,
            enforce_nonce: None,
            tts: Some(false),
            sticker_ids: (!sticker_ids.is_empty()).then_some(sticker_ids),
            flags: Some(0),
        };
        let msg_string = facet_json::to_vec(&message)?;
        Fetch::<Fresh>::fetch(
            || {
                surf::post(format!("{DISCORD_API}/channels/{channel_id}/messages"))
                    .body(msg_string.clone())
                    .content_type("application/json")
            },
            self.get_auth_header().await?,
        )
        .await?
        .json::<api::Message>()
        .await
    }

    /// `PATCH /channels/{channel_id}/messages/{message_id}`
    /// <https://docs.discord.food/resources/message#edit-message>
    pub(crate) async fn rest_edit_message(
        &self,
        channel_id: SNOWFLAKE,
        message_id: SNOWFLAKE,
        content: String,
    ) -> Result<api::Message, Box<dyn Error + Sync + Send>> {
        let body = facet_json::to_vec(&api::MessageEdit { content })?;
        Fetch::<Fresh>::fetch(
            || {
                surf::patch(format!(
                    "{DISCORD_API}/channels/{channel_id}/messages/{message_id}"
                ))
                .body(body.clone())
                .content_type("application/json")
            },
            self.get_auth_header().await?,
        )
        .await?
        .json::<api::Message>()
        .await
    }

    /// `DELETE /channels/{channel_id}/messages/{message_id}`
    /// <https://docs.discord.food/resources/message#delete-message>
    pub(crate) async fn rest_delete_message(
        &self,
        channel_id: SNOWFLAKE,
        message_id: SNOWFLAKE,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let url = format!("{DISCORD_API}/channels/{channel_id}/messages/{message_id}");
        Fetch::<Fresh>::fetch(|| surf::delete(&url), self.get_auth_header().await?).await?;
        Ok(())
    }
}
