//! Discord Voice Gateway JSON and binary payloads.
//!
//! <https://docs.discord.food/topics/voice-connections>

mod inbound;
mod outbound;

pub(crate) use inbound::*;
pub(crate) use outbound::*;
