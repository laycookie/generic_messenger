//! Typed outbound commands for the main gateway.
//!
//! Adapter methods describe intent here instead of rebuilding opcode payloads
//! and independently deciding which self-state flags must be preserved.

use async_tungstenite::tungstenite;
use surf::http::convert::json;
use tracing::debug;

use super::{General, Opcode};
use crate::{api::SNOWFLAKE, gateways::Gateway};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SelfVoiceFlags {
    pub(crate) muted: bool,
    pub(crate) deafened: bool,
    pub(crate) video: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VoiceCommand {
    Join {
        guild_id: Option<SNOWFLAKE>,
        channel_id: SNOWFLAKE,
    },
    Leave {
        guild_id: Option<SNOWFLAKE>,
    },
    SetFlags {
        guild_id: Option<SNOWFLAKE>,
        channel_id: SNOWFLAKE,
        flags: SelfVoiceFlags,
    },
}

impl Gateway<General> {
    pub(crate) async fn send_voice_command(
        &self,
        command: VoiceCommand,
    ) -> Result<(), tungstenite::Error> {
        let payload = match command {
            VoiceCommand::Join {
                guild_id,
                channel_id,
            } => json!({
                "op": Opcode::VoiceStateUpdate as u8,
                "d": {
                    "guild_id": guild_id,
                    "channel_id": channel_id,
                    "self_mute": false,
                    "self_deaf": false,
                    "self_video": false,
                }
            }),
            VoiceCommand::Leave { guild_id } => json!({
                "op": Opcode::VoiceStateUpdate as u8,
                "d": {
                    "guild_id": guild_id,
                    "channel_id": null,
                    "self_mute": false,
                    "self_deaf": false,
                }
            }),
            VoiceCommand::SetFlags {
                guild_id,
                channel_id,
                flags,
            } => json!({
                "op": Opcode::VoiceStateUpdate as u8,
                "d": {
                    "guild_id": guild_id,
                    "channel_id": channel_id,
                    "self_mute": flags.muted,
                    "self_deaf": flags.deafened,
                    "self_video": flags.video,
                }
            }),
        };
        debug!(?command, "Sending Discord voice command: {payload}");
        self.send(payload.to_string().into()).await
    }
}
