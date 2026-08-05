//! JSON request bodies sent to Discord REST endpoints.
//!
//! <https://docs.discord.food/reference>

use facet::Facet;

/// `POST /channels/{channel_id}/messages` JSON body.
/// <https://docs.discord.food/resources/message#create-message>
#[derive(Facet)]
pub(crate) struct CreateMessage {
    pub nonce: Option<String>,
    pub enforce_nonce: Option<bool>,
    pub tts: Option<bool>,
    pub content: Option<String>,
    pub sticker_ids: Option<Vec<super::resources::SNOWFLAKE>>,
    pub flags: Option<u32>,
}

/// `PATCH /channels/{channel_id}/messages/{message_id}` JSON body.
/// <https://docs.discord.food/resources/message#edit-message>
#[derive(Facet)]
pub(crate) struct MessageEdit {
    pub content: String,
}
