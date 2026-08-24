//! Audio calls, optional controls, and realtime voice/media events.

use std::error::Error;

use async_trait::async_trait;
use futures::channel::oneshot;
use simple_audio_channels::input::SampleConsumer;
use simple_audio_channels::output::SampleProducer;

use super::MessengerError;
use super::caster::capability_caster;
use crate::stream::WeakStreamSource;
use crate::types::{ID, Identifier, Place, Room, User, VoiceState};

capability_caster!(
    /// Capability: broadcasting the client's own mute/deafen state.
    AudioCasterSelfState: Audio => AudioSelfVoiceState { self_state }
);

/// Non-connected call states; connected is represented by [`CallState`].
#[derive(Debug, Clone, Copy)]
pub enum CallStatus {
    /// Currently attempting to connect; the string carries the pipeline stage.
    Connecting(&'static str),
    Failed,
    // TODO: Add Retrying.
}

impl CallStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connecting(msg) => msg,
            Self::Failed => "Failed",
        }
    }
}

/// UI call state; [`VoiceEvent::CallStreamReady`] transitions to connected.
#[derive(Debug, Clone, Copy)]
pub enum CallState {
    Connected,
    Pending(CallStatus),
}

impl CallState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Connected => "Connected",
            Self::Pending(status) => status.as_str(),
        }
    }
}

/// Audio-call lifecycle API.
/// Optional controls use capability traits; video remains independent.
#[async_trait]
pub trait Audio: Send + Sync + AudioCasterSelfState {
    /// Connect to voice in `location`.
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>>;
    /// Disconnect from voice in `location`.
    async fn disconnect(&self, location: &Identifier<Place<Room>>);

    /// Prepare this backend's voice event source.
    async fn prepare_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Polls backend-wide control events.
    /// Per-call media handshakes arrive through [`WeakStreamSource<AudioEvent>`].
    fn next_event(self: crate::owner::Retained<Self>) -> crate::stream::EventFuture<VoiceEvent> {
        Box::pin(async { None })
    }
}

/// Capability for signaling own mute/deafen state.
/// Application still controls local audio; confirmation arrives in user state.
#[async_trait]
pub trait AudioSelfVoiceState: Audio {
    /// Signals desired mute/deafen state for `location`.
    async fn set_self_voice_state(
        &self,
        location: &Identifier<Place<Room>>,
        state: VoiceState,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
}

/// Backend-wide voice control events.
pub enum VoiceEvent {
    /// Non-connected state; readiness arrives through `CallStreamReady`.
    CallStatusUpdate {
        /// ID-only room identity scoped by outer backend subscription.
        room: Identifier<()>,
        status: CallStatus,
    },
    /// Nested audio stream proving call is connected.
    CallStreamReady {
        /// Room/call this media stream belongs to.
        room: Identifier<()>,
        source: WeakStreamSource<AudioEvent>,
    },
    ParticipantJoined {
        room: Identifier<()>,
        user: Identifier<User>,
    },
    ParticipantLeft {
        user_id: ID,
    },
}
/// Per-call audio graph handshakes.
pub enum AudioEvent {
    /// Requests playback producer attachment.
    AddAudioSource {
        /// Optional roster user ID for per-user controls.
        user: Option<ID>,
        channel: oneshot::Sender<SampleProducer>,
    },
    /// Requests microphone consumer attachment.
    AddAudioInput(oneshot::Sender<SampleConsumer>),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CoreVoice;
    struct StatefulVoice;

    macro_rules! impl_voice_core {
        ($ty:ty) => {
            #[async_trait]
            impl Audio for $ty {
                async fn connect(
                    &self,
                    _: &Identifier<Place<Room>>,
                ) -> Result<CallStatus, Box<dyn Error + Sync + Send>> {
                    unimplemented!()
                }
                async fn disconnect(&self, _: &Identifier<Place<Room>>) {
                    unimplemented!()
                }
            }
        };
    }
    impl_voice_core!(CoreVoice);
    impl_voice_core!(StatefulVoice);

    #[async_trait]
    impl AudioSelfVoiceState for StatefulVoice {
        async fn set_self_voice_state(
            &self,
            _: &Identifier<Place<Room>>,
            _: VoiceState,
        ) -> Result<(), Box<dyn Error + Sync + Send>> {
            unimplemented!()
        }
    }

    #[test]
    fn audio_casters_resolve_by_concrete_type() {
        let core: &dyn Audio = &CoreVoice;
        assert!(core.self_state().is_err());

        let stateful: &dyn Audio = &StatefulVoice;
        assert!(stateful.self_state().is_ok());
    }
}
