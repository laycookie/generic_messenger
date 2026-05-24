use crate::api_types::{SNOWFLAKE, User};
use facet::Facet;

#[derive(Facet)]
pub(super) struct HelloPayload {
    pub(super) heartbeat_interval: u64,
    pub(super) _trace: Vec<String>,
}

/// <https://docs.discord.food/gateway/gateway-events#ready-structure>
#[derive(Facet)]
pub(super) struct ReadyPayload {
    pub(super) _trace: Option<Vec<String>>,
    pub(super) v: Option<u64>,
    pub(super) user: Option<User>,
    pub(super) user_settings: Option<facet_value::Value>,
    pub(super) user_settings_proto: Option<String>,
    pub(super) notification_settings: Option<facet_value::Value>,
    pub(super) user_guild_settings: Option<facet_value::Value>,
    pub(super) read_state: Option<facet_value::Value>,
    pub(super) guilds: Option<Vec<ReadyGuildPayload>>,
    pub(super) guild_join_requests: Option<Vec<facet_value::Value>>,
    pub(super) relationships: Option<Vec<facet_value::Value>>,
    pub(super) game_relationships: Option<Vec<facet_value::Value>>,
    pub(super) friend_suggestion_count: Option<u64>,
    pub(super) private_channels: Option<Vec<facet_value::Value>>,
    pub(super) connected_accounts: Option<Vec<facet_value::Value>>,
    pub(super) notes: Option<facet_value::Value>,
    pub(super) presences: Option<Vec<facet_value::Value>>,
    pub(super) merged_presences: Option<facet_value::Value>,
    pub(super) merged_members: Option<Vec<Vec<VoiceStateMemberPayload>>>,
    pub(super) users: Option<Vec<facet_value::Value>>,
    pub(super) linked_users: Option<Vec<facet_value::Value>>,
    pub(super) application: Option<facet_value::Value>,
    pub(super) scopes: Option<Vec<String>>,
    pub(super) session_id: Option<String>,
    pub(super) session_type: Option<String>,
    pub(super) sessions: Option<Vec<SessionObjectPayload>>,
    pub(super) static_client_session_id: Option<String>,
    pub(super) auth_session_id_hash: Option<String>,
    pub(super) auth_token: Option<String>,
    pub(super) analytics_token: Option<String>,
    pub(super) auth: Option<facet_value::Value>,
    pub(super) required_action: Option<String>,
    pub(super) country_code: Option<String>,
    pub(super) geo_ordered_rtc_regions: Option<Vec<String>>,
    pub(super) consents: Option<facet_value::Value>,
    pub(super) tutorial: Option<facet_value::Value>,
    pub(super) shard: Option<Vec<u64>>,
    pub(super) resume_gateway_url: Option<String>,
    pub(super) api_code_version: Option<u64>,
    pub(super) experiments: Option<Vec<facet_value::Value>>,
    pub(super) guild_experiments: Option<Vec<facet_value::Value>>,
    pub(super) apex_experiments: Option<facet_value::Value>,
    pub(super) explicit_content_scan_version: Option<u64>,
    pub(super) pending_payments: Option<Vec<facet_value::Value>>,
    pub(super) av_sf_protocol_floor: Option<u64>,
    pub(super) feature_flags: Option<facet_value::Value>,
    pub(super) lobbies: Option<Vec<facet_value::Value>>,
    pub(super) user_application_profiles: Option<facet_value::Value>,
    pub(super) connection_request_data: Option<facet_value::Value>,
    pub(super) ad_personalization_toggles_disabled: Option<bool>,
    pub(super) broadcaster_user_ids: Option<Vec<facet_value::Value>>,
    pub(super) regional_feature_config: Option<facet_value::Value>,
}

/// <https://docs.discord.food/gateway/gateway-events#gateway-guild-object>
#[derive(Facet)]
pub(super) struct ReadyGuildPayload {
    pub(super) id: Option<SNOWFLAKE>,
    pub(super) joined_at: Option<String>,
    pub(super) large: Option<bool>,
    pub(super) unavailable: Option<bool>,
    pub(super) geo_restricted: Option<bool>,
    pub(super) member_count: Option<u64>,
    pub(super) members: Option<Vec<VoiceStateMemberPayload>>,
    pub(super) channels: Option<Vec<facet_value::Value>>,
    pub(super) threads: Option<Vec<facet_value::Value>>,
    pub(super) presences: Option<Vec<facet_value::Value>>,
    pub(super) voice_states: Option<Vec<VoiceStatePayload>>,
    pub(super) activity_instances: Option<Vec<facet_value::Value>>,
    pub(super) stage_instances: Option<Vec<facet_value::Value>>,
    pub(super) guild_scheduled_events: Option<Vec<facet_value::Value>>,
    pub(super) data_mode: Option<String>,
    pub(super) properties: Option<facet_value::Value>,
    pub(super) stickers: Option<Vec<facet_value::Value>>,
    pub(super) roles: Option<Vec<facet_value::Value>>,
    pub(super) emojis: Option<Vec<facet_value::Value>>,
    pub(super) soundboard_sounds: Option<Vec<facet_value::Value>>,
    pub(super) premium_subscription_count: Option<u64>,
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
    pub(super) channel_id: Option<SNOWFLAKE>,
    pub(super) lobby_id: Option<String>,
    pub(super) user_id: SNOWFLAKE,
    pub(super) member: Option<VoiceStateMemberPayload>,
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

#[derive(Facet)]
pub(super) struct VoiceStateMemberPayload {
    pub(super) user: User,
}

/// https://docs.discord.food/resources/presence#session-object
#[derive(Debug, Facet)]
pub(super) struct SessionObjectPayload {
    pub(super) session_id: String,
    // client_info: ?
    pub(super) status: String,
    // activities: Vec<?>,
    // hidden_activities: Vec<?>,
    pub(super) active: Option<bool>,
}
