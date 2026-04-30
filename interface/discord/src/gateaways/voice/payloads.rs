use facet::Facet;

use super::connection::{EncryptionMode, Ssrc};
use crate::api_types::SNOWFLAKE;

/// <https://docs.discord.food/topics/voice-connections#hello-structure>
#[derive(Facet)]
pub(super) struct HelloPayload {
    pub(super) v: u8,
    pub(super) heartbeat_interval: u64,
}

/// https://docs.discord.food/topics/voice-connections#ready-structure
#[derive(Facet)]
pub(super) struct ReadyPayload {
    pub(super) ssrc: Ssrc,
    pub(super) ip: String,
    pub(super) port: u16,
    pub(super) modes: Vec<EncryptionMode>,
    pub(super) experiments: Vec<String>,
    // streams: Vec<stream object>
}

/// <https://docs.discord.food/topics/voice-connections#speaking-structure>
#[derive(Facet)]
pub(super) struct SpeakingPayload {
    pub(super) speaking: u8,
    pub(super) ssrc: Ssrc,
    pub(super) user_id: SNOWFLAKE, // Only sent by the voice server.
    pub(super) delay: Option<u32>, // Not sent by the voice server.
}

#[derive(Facet)]
pub(super) struct DAVEPrepareEpoch {
    pub(super) transition_id: u16,
    pub(super) epoch: u64,
    pub(super) protocol_version: u16,
}
