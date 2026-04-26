use crate::api_types::SNOWFLAKE;
use facet::Facet;

#[derive(Facet)]
pub(super) struct ReadyPayload {
    pub(super) ssrc: u64,
    pub(super) ip: String,
    pub(super) port: u64,
    pub(super) modes: Vec<String>,
    pub(super) experiments: Vec<String>,
    // streams: Vec<?>
}

#[derive(Facet)]
pub(super) struct HelloPayload {
    pub(super) heartbeat_interval: u64,
    pub(super) _trace: Vec<String>,
}

/// <https://docs.discord.com/developers/events/gateway-events#voice-server-update>
#[derive(Debug, Facet)]
pub struct VoiceServerUpdatePayload {
    pub token: String,
    pub guild_id: Option<SNOWFLAKE>,
    pub endpoint: Option<String>,
}

/// <https://docs.discord.food/resources/voice#voice-state-structure>
#[derive(Facet)]
pub(super) struct VoiceStatePayload {
    pub(super) guild_id: Option<String>,
    pub(super) channel_id: String,
    pub(super) lobby_id: Option<String>,
    pub(super) user_id: String,
    //member: Vec<?>
    pub(super) session_id: String,
    pub(super) deaf: bool,
    pub(super) mute: bool,
    pub(super) self_deaf: bool,
    pub(super) self_mute: bool,
    pub(super) self_stream: Option<bool>,
    pub(super) self_video: bool,
    pub(super) suppress: bool,
    // request_to_speak_timestamp: ?
    pub(super) discoverable: Option<bool>,
    pub(super) user_volume: Option<f32>,
}

/// https://docs.discord.food/resources/presence#session-object
#[derive(Facet)]
pub(super) struct SessionObjectPayload {
    pub(super) session_id: String,
    // client_info: ?
    pub(super) status: String,
    // activities: Vec<?>,
    // hidden_activities: Vec<?>,
    pub(super) active: Option<bool>,
}
