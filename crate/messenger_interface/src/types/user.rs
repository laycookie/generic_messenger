//! The [`User`] entity.

use std::path::PathBuf;

use bitflags::bitflags;

/// Represents a user in the messenger system.
#[derive(Debug, Clone)]
pub struct User {
    /// Display name of the user.
    pub name: String,
    /// Optional backend handle/tag used for lookup or friend invites. This may
    /// differ from the display name (e.g. Discord global display name vs.
    /// unique username).
    pub tag: Option<String>,
    /// Optional path to the user's avatar/icon image.
    pub icon: Option<PathBuf>,
    /// Presence/status, present only when a backend supports status
    /// indicators for this user.
    pub status: Option<UserStatus>,
    /// Voice-call state, present only on users delivered as voice-room
    /// participants (rosters seeded by queries or `VoiceEvent`s). `None` in
    /// every other context (message authors, contacts, profiles).
    pub voice: Option<VoiceState>,
}

/// Generic user presence/status. Backends map their native statuses into this
/// small shared set; unsupported backends leave [`User::status`] as `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserStatus {
    Online,
    Idle,
    DoNotDisturb,
    Offline,
}

impl UserStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Online => "Online",
            Self::Idle => "Idle",
            Self::DoNotDisturb => "Do not disturb",
            Self::Offline => "Offline",
        }
    }
}

bitflags! {
    /// Video streams a call participant is broadcasting. Flags rather than an
    /// enum because they are not mutually exclusive — some backends allow
    /// webcam and screen share at once.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct VideoBroadcast: u8 {
        /// Webcam.
        const CAMERA = 1 << 0;
        /// Screen/window capture (e.g. Discord "Go Live").
        const SCREENSHARE = 1 << 1;
    }
}

/// Mute/deafen/video flags of a user in a voice call. Backends that
/// distinguish self- and server-applied states fold both in.
#[derive(Debug, Clone, Copy, Default)]
pub struct VoiceState {
    /// Not transmitting microphone audio.
    pub muted: bool,
    /// Not receiving call audio (implies not transmitting on most platforms).
    pub deafened: bool,
    /// Video streams the user is broadcasting, if any.
    pub video: VideoBroadcast,
}
