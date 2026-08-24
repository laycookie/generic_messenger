//! DAVE/MLS media event helpers.
//!
//! <https://docs.discord.food/topics/voice-connections#end-to-end-encryption>

use std::{io, num::NonZeroU16};

use async_tungstenite::tungstenite::Message;
use dashmap::DashMap;
use davey::DaveSession;
use surf::http::convert::json;
use tracing::{error, warn};

use super::VoiceOpcode;
use crate::api::SNOWFLAKE;

pub(super) fn execute_pending_transition(
    dave_session: &mut DaveSession,
    dave_pending_transitions: &DashMap<u16, NonZeroU16>,
    transition_id: u16,
) {
    let Some((_, new_version)) = dave_pending_transitions.remove(&transition_id) else {
        tracing::trace!(
            target: "video_timing",
            stage = "outgoing.control",
            kind = "DAVE",
            action = "dave_execute_missing",
            transition_id,
            failed = true,
            "DAVE transition execute missing"
        );
        warn!(
            "Received execute transition, but we don't have a pending transition for {transition_id}"
        );
        return;
    };

    let old_version = dave_session.protocol_version();
    if old_version != new_version {
        error!(
            "DAVE protocol version mismatch: old={old_version:?}, new={new_version:?}. Transition not applied"
        );
    }
    tracing::trace!(
        target: "video_timing",
        stage = "outgoing.control",
        kind = "DAVE",
        action = "dave_transition_executed",
        transition_id,
        previous_protocol_version = u64::from(old_version.get()),
        protocol_version = u64::from(new_version.get()),
        pending_transitions = dave_pending_transitions.len(),
        failed = old_version != new_version,
        "DAVE transition executed"
    );
}

pub(super) async fn respond_to_mls_transition<E: std::fmt::Debug>(
    websocket: &crate::gateways::Websocket,
    dave_pending_transitions: &DashMap<u16, NonZeroU16>,
    protocol_version: NonZeroU16,
    transition_id: u16,
    process_result: Result<(), E>,
) -> Result<(), Box<dyn std::error::Error>> {
    match process_result {
        Err(err) => {
            dave_pending_transitions.remove(&transition_id);
            error!("{err:?}");
            websocket
                .send(Message::Text(
                    json!({
                        "op": VoiceOpcode::MLSInvalidCommitWelcome as u8,
                        "d": { "transition_id": transition_id }
                    })
                    .to_string()
                    .into(),
                ))
                .await?;
        }
        Ok(()) if transition_id != 0 => {
            dave_pending_transitions.insert(transition_id, protocol_version);
            websocket
                .send(Message::Text(
                    json!({
                        "op": VoiceOpcode::DAVEProtocolTransitionReady as u8,
                        "d": { "transition_id": transition_id }
                    })
                    .to_string()
                    .into(),
                ))
                .await?;
        }
        Ok(()) => {}
    }
    Ok(())
}

/// Reinitialize DAVE and return MLS key package for announcement.
pub(super) fn reinit_dave_session(
    dave_session: &mut Option<DaveSession>,
    dave_protocol_version: u16,
    group_id: SNOWFLAKE,
    user_id: SNOWFLAKE,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let dave_ver = NonZeroU16::new(dave_protocol_version).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "DAVE protocol version is zero")
    })?;

    let key_package = if let Some(dave_session) = dave_session {
        dave_session.reinit(dave_ver, user_id, group_id, None)?;
        dave_session.create_key_package()
    } else {
        let mut new_dave_session = DaveSession::new(dave_ver, user_id, group_id, None)?;
        let key_package = new_dave_session.create_key_package();
        *dave_session = Some(new_dave_session);
        key_package
    };

    Ok(key_package?)
}
