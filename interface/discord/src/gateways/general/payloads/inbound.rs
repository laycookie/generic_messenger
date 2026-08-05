//! Main Discord Gateway JSON payloads.
//!
//! <https://docs.discord.food/gateway/gateway-events>

use crate::api::{self, SNOWFLAKE, User};
use facet::Facet;

use crate::api::Emoji;

/// `MESSAGE_DELETE` dispatch payload.
/// <https://docs.discord.food/gateway/gateway-events#message-delete>
#[derive(Facet)]
pub(crate) struct MessageDelete {
    pub(crate) id: SNOWFLAKE,
    pub(crate) channel_id: SNOWFLAKE,
}

/// `MESSAGE_DELETE_BULK` dispatch payload.
/// <https://docs.discord.food/gateway/gateway-events#message-delete-bulk>
#[derive(Facet)]
pub(crate) struct MessageDeleteBulk {
    pub(crate) ids: Vec<SNOWFLAKE>,
    pub(crate) channel_id: SNOWFLAKE,
}

/// Minimal `CHANNEL_DELETE` shape. Private-channel deletes can be partial, so
/// decoding the full channel object would reject valid dispatches.
#[derive(Facet)]
pub(crate) struct ChannelDeletePayload {
    pub(crate) id: SNOWFLAKE,
    pub(crate) guild_id: Option<SNOWFLAKE>,
}

/// `GUILD_DELETE` unavailable-guild payload.
#[derive(Facet)]
pub(crate) struct GuildDeletePayload {
    pub(crate) id: SNOWFLAKE,
    pub(crate) unavailable: Option<bool>,
}

#[derive(Facet)]
pub(crate) struct GuildEmojisUpdatePayload {
    pub(crate) guild_id: SNOWFLAKE,
    pub(crate) emojis: Vec<api::GuildEmoji>,
}

#[derive(Facet)]
pub(crate) struct GuildStickersUpdatePayload {
    pub(crate) guild_id: SNOWFLAKE,
    pub(crate) stickers: Vec<api::GuildSticker>,
}

/// Relationship Update/Remove objects are partial. Only fields represented by
/// the adapter cache are decoded; nickname/type-only updates need no mutation.
#[derive(Facet)]
pub(crate) struct RelationshipPayload {
    pub(crate) id: SNOWFLAKE,
    pub(crate) user: Option<User>,
}

/// Shared subset of `MESSAGE_REACTION_ADD` and `MESSAGE_REACTION_REMOVE`.
/// <https://docs.discord.food/gateway/gateway-events#message-reaction-add>
#[derive(Facet)]
pub(crate) struct MessageReactionChange {
    pub(crate) user_id: SNOWFLAKE,
    pub(crate) channel_id: SNOWFLAKE,
    pub(crate) message_id: SNOWFLAKE,
    pub(crate) emoji: Emoji,
}

/// Main Gateway opcode 10 Hello payload.
/// <https://docs.discord.food/gateway/using-gateway#hello-event>
#[derive(Facet)]
pub(crate) struct HelloPayload {
    pub(crate) heartbeat_interval: u64,
    pub(crate) _trace: Vec<String>,
}

/// <https://docs.discord.food/gateway/gateway-events#ready-structure>
#[derive(Facet)]
pub(crate) struct ReadyPayload {
    pub(crate) _trace: Option<Vec<String>>,
    pub(crate) v: Option<u64>,
    pub(crate) user: Option<User>,
    pub(crate) user_settings: Option<facet_value::Value>,
    pub(crate) user_settings_proto: Option<String>,
    pub(crate) notification_settings: Option<facet_value::Value>,
    pub(crate) user_guild_settings: Option<facet_value::Value>,
    pub(crate) read_state: Option<facet_value::Value>,
    pub(crate) guilds: Option<Vec<ReadyGuildPayload>>,
    pub(crate) guild_join_requests: Option<Vec<facet_value::Value>>,
    pub(crate) relationships: Option<Vec<facet_value::Value>>,
    pub(crate) game_relationships: Option<Vec<facet_value::Value>>,
    pub(crate) friend_suggestion_count: Option<u64>,
    pub(crate) private_channels: Option<Vec<api::Channel>>,
    pub(crate) connected_accounts: Option<Vec<facet_value::Value>>,
    pub(crate) notes: Option<facet_value::Value>,
    pub(crate) presences: Option<Vec<facet_value::Value>>,
    pub(crate) merged_presences: Option<facet_value::Value>,
    pub(crate) merged_members: Option<Vec<Vec<VoiceStateMemberPayload>>>,
    pub(crate) users: Option<Vec<facet_value::Value>>,
    pub(crate) linked_users: Option<Vec<facet_value::Value>>,
    pub(crate) application: Option<facet_value::Value>,
    pub(crate) scopes: Option<Vec<String>>,
    pub(crate) session_id: Option<String>,
    pub(crate) session_type: Option<String>,
    pub(crate) sessions: Option<Vec<SessionObjectPayload>>,
    pub(crate) static_client_session_id: Option<String>,
    pub(crate) auth_session_id_hash: Option<String>,
    pub(crate) auth_token: Option<String>,
    pub(crate) analytics_token: Option<String>,
    pub(crate) auth: Option<facet_value::Value>,
    pub(crate) required_action: Option<String>,
    pub(crate) country_code: Option<String>,
    pub(crate) geo_ordered_rtc_regions: Option<Vec<String>>,
    pub(crate) consents: Option<facet_value::Value>,
    pub(crate) tutorial: Option<facet_value::Value>,
    pub(crate) shard: Option<Vec<u64>>,
    pub(crate) resume_gateway_url: Option<String>,
    pub(crate) api_code_version: Option<u64>,
    pub(crate) experiments: Option<Vec<facet_value::Value>>,
    pub(crate) guild_experiments: Option<Vec<facet_value::Value>>,
    pub(crate) apex_experiments: Option<facet_value::Value>,
    pub(crate) explicit_content_scan_version: Option<u64>,
    pub(crate) pending_payments: Option<Vec<facet_value::Value>>,
    pub(crate) av_sf_protocol_floor: Option<u64>,
    pub(crate) feature_flags: Option<facet_value::Value>,
    pub(crate) lobbies: Option<Vec<facet_value::Value>>,
    pub(crate) user_application_profiles: Option<facet_value::Value>,
    pub(crate) connection_request_data: Option<facet_value::Value>,
    pub(crate) ad_personalization_toggles_disabled: Option<bool>,
    pub(crate) broadcaster_user_ids: Option<Vec<facet_value::Value>>,
    pub(crate) regional_feature_config: Option<facet_value::Value>,
}

/// <https://docs.discord.food/gateway/gateway-events#gateway-guild-object>
#[derive(Facet)]
pub(crate) struct ReadyGuildPayload {
    pub(crate) id: Option<SNOWFLAKE>,
    pub(crate) joined_at: Option<String>,
    pub(crate) large: Option<bool>,
    pub(crate) unavailable: Option<bool>,
    pub(crate) geo_restricted: Option<bool>,
    pub(crate) member_count: Option<u64>,
    pub(crate) members: Option<Vec<VoiceStateMemberPayload>>,
    pub(crate) channels: Option<Vec<api::Channel>>,
    pub(crate) threads: Option<Vec<facet_value::Value>>,
    pub(crate) presences: Option<Vec<facet_value::Value>>,
    pub(crate) voice_states: Option<Vec<VoiceStatePayload>>,
    pub(crate) activity_instances: Option<Vec<facet_value::Value>>,
    pub(crate) stage_instances: Option<Vec<facet_value::Value>>,
    pub(crate) guild_scheduled_events: Option<Vec<facet_value::Value>>,
    pub(crate) data_mode: Option<String>,
    pub(crate) properties: Option<api::Guild>,
    pub(crate) stickers: Option<Vec<facet_value::Value>>,
    pub(crate) roles: Option<Vec<facet_value::Value>>,
    pub(crate) emojis: Option<Vec<facet_value::Value>>,
    pub(crate) soundboard_sounds: Option<Vec<facet_value::Value>>,
    pub(crate) premium_subscription_count: Option<u64>,
}

/// <https://docs.discord.food/gateway/gateway-events#voice-server-update>
#[derive(Debug, Facet)]
pub struct VoiceServerUpdatePayload {
    pub token: String,
    pub guild_id: Option<SNOWFLAKE>,
    pub endpoint: Option<String>,
}

/// STREAM_CREATE / STREAM_UPDATE dispatch. Carries the RTC server hosting a Go
/// Live stream's media. Lenient because the two dispatches (CREATE/UPDATE)
/// share a shape and the field set is only partly documented — logged so real
/// traffic can confirm it.
/// <https://docs.discord.food/gateway/gateway-events#stream-create>
#[derive(Debug, Facet)]
pub(crate) struct StreamCreatePayload {
    pub(crate) stream_key: String,
    pub(crate) rtc_server_id: Option<SNOWFLAKE>,
    pub(crate) viewer_ids: Option<Vec<SNOWFLAKE>>,
    pub(crate) region: Option<String>,
    pub(crate) paused: Option<bool>,
}

/// STREAM_SERVER_UPDATE dispatch: the media endpoint + token for a watched
/// stream's separate voice connection (the stream analog of
/// VOICE_SERVER_UPDATE).
/// <https://docs.discord.food/gateway/gateway-events#stream-server-update>
#[derive(Debug, Facet)]
pub(crate) struct StreamServerUpdatePayload {
    pub(crate) stream_key: String,
    pub(crate) token: String,
    pub(crate) guild_id: Option<SNOWFLAKE>,
    pub(crate) endpoint: Option<String>,
}

/// STREAM_DELETE dispatch: a watched stream ended or we were dropped from it.
/// <https://docs.discord.food/gateway/gateway-events#stream-delete>
#[derive(Debug, Facet)]
pub(crate) struct StreamDeletePayload {
    pub(crate) stream_key: String,
    pub(crate) reason: Option<String>,
    pub(crate) unavailable: Option<bool>,
}

/// <https://docs.discord.food/resources/voice#voice-state-structure>
#[derive(Facet)]
pub(crate) struct VoiceStatePayload {
    pub(crate) guild_id: Option<SNOWFLAKE>,
    pub(crate) channel_id: Option<SNOWFLAKE>,
    pub(crate) lobby_id: Option<String>,
    pub(crate) user_id: SNOWFLAKE,
    pub(crate) member: Option<VoiceStateMemberPayload>,
    pub(crate) session_id: String,
    pub(crate) deaf: bool,
    pub(crate) mute: bool,
    pub(crate) self_deaf: bool,
    pub(crate) self_mute: bool,
    pub(crate) self_stream: Option<bool>,
    pub(crate) self_video: bool,
    pub(crate) suppress: bool,
    // request_to_speak_timestamp: ?
    pub(crate) discoverable: Option<bool>,
    pub(crate) user_volume: Option<f32>,
}

/// Guild member subset embedded in voice-state payloads.
/// <https://docs.discord.food/resources/guild#guild-member-object>
#[derive(Facet)]
pub(crate) struct VoiceStateMemberPayload {
    pub(crate) user: User,
}

/// <https://docs.discord.food/gateway/gateway-events#presence-update>
#[derive(Facet)]
pub(crate) struct PresencePayload {
    pub(crate) user: PresenceUserPayload,
    pub(crate) status: String,
}

/// User identity embedded in a Presence Update dispatch.
/// <https://docs.discord.food/gateway/gateway-events#presence-update>
#[derive(Facet)]
pub(crate) struct PresenceUserPayload {
    pub(crate) id: SNOWFLAKE,
}

/// <https://docs.discord.food/resources/presence#session-object>
#[derive(Debug, Facet)]
pub(crate) struct SessionObjectPayload {
    pub(crate) session_id: String,
    // client_info: ?
    pub(crate) status: String,
    // activities: Vec<?>,
    // hidden_activities: Vec<?>,
    pub(crate) active: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_delete_decodes_all_ids() {
        let payload = facet_json::from_str::<MessageDeleteBulk>(
            r#"{"ids":[1,2,3],"channel_id":9,"guild_id":8}"#,
        )
        .unwrap();

        assert_eq!(payload.ids, [1, 2, 3]);
        assert_eq!(payload.channel_id, 9);
    }

    #[test]
    fn partial_delete_and_relationship_payloads_decode() {
        let channel = facet_json::from_str::<ChannelDeletePayload>(r#"{"id":7}"#).unwrap();
        assert_eq!(channel.id, 7);
        assert_eq!(channel.guild_id, None);

        let relationship =
            facet_json::from_str::<RelationshipPayload>(r#"{"id":11,"type":0}"#).unwrap();
        assert_eq!(relationship.id, 11);
        assert!(relationship.user.is_none());
    }
}
