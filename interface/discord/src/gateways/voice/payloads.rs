use std::io;

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

/// Binary payload for MLSProposals (opcode 27).
///
/// Wire format (opcode byte already stripped):
/// `[operation_type: u8][proposals_data: ...]`
pub(super) struct MlsProposalsPayload {
    pub(super) operation_type: davey::ProposalsOperationType,
    pub(super) data: Vec<u8>,
}

impl TryFrom<Vec<u8>> for MlsProposalsPayload {
    type Error = io::Error;
    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MLSProposals payload is empty",
            ));
        }
        let operation_type = if bytes[0] == 0 {
            davey::ProposalsOperationType::APPEND
        } else {
            davey::ProposalsOperationType::REVOKE
        };
        Ok(Self {
            operation_type,
            data: bytes[1..].to_vec(),
        })
    }
}

/// Binary payload with a transition ID prefix — used by
/// MLSAnnounceCommitTransition (opcode 29) and MLSWelcome (opcode 30).
///
/// Wire format (opcode byte already stripped):
/// `[transition_id: u16 BE][data: ...]`
pub(super) struct MlsTransitionPayload {
    pub(super) transition_id: u16,
    pub(super) data: Vec<u8>,
}

impl TryFrom<Vec<u8>> for MlsTransitionPayload {
    type Error = io::Error;
    fn try_from(bytes: Vec<u8>) -> Result<Self, Self::Error> {
        if bytes.len() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MLS transition payload too short for transition_id",
            ));
        }
        let transition_id = u16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Self {
            transition_id,
            data: bytes[2..].to_vec(),
        })
    }
}
