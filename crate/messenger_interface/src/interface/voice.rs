//! Voice call lifecycle ([`Voice`]), the optional per-operation capabilities
//! ([`SelfVoiceState`]) with their casters, call state types, and the realtime
//! [`VoiceEvent`]/[`AudioEvent`]s.

use std::{error::Error, sync::Arc};

use async_trait::async_trait;
use futures::channel::oneshot;
use simple_audio_channels::input::SampleConsumer;
use simple_audio_channels::output::SampleProducer;

use super::MessengerError;
use super::caster::capability_caster;
use crate::stream::WeakSocketStream;
use crate::types::{ID, Identifier, Place, Room, User, VoiceState};

capability_caster!(
    /// Capability: broadcasting the client's own mute/deafen state.
    VoiceCasterSelfState: Voice => SelfVoiceState { self_state, arc_self_state }
);

/// Non-connected sub-states of a voice call. The connected state is represented
/// by the *absence* of a `CallStatus` — see [`CallState`].
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

/// Full state of a voice call as seen by the UI. The audio stream is delivered
/// out-of-band via [`VoiceEvent::CallStreamReady`]; receiving that event is the
/// signal to transition into [`CallState::Connected`].
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

/// Minimal voice lifecycle API.
///
/// Optional voice operations (setting the own mute/deafen state) live in
/// their own capability traits ([`SelfVoiceState`]); whether a backend
/// supports them is a property of its *type*, surfaced to the UI through the
/// `VoiceCaster*` supertraits.
#[async_trait]
pub trait Voice: Send + Sync + VoiceCasterSelfState {
    /// Connect to voice in `location`.
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>>;
    /// Disconnect from voice in `location`.
    async fn disconnect(&self, location: &Identifier<Place<Room>>);

    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<VoiceEvent>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
}

/// Capability: broadcast the client's own mute/deafen flags for the call at
/// `location`, so other participants see them.
///
/// Signaling only — actually silencing the microphone/playback is the
/// application's job (it owns the audio channels). The backend's state echo
/// comes back through [`VoiceEvent::ParticipantJoined`] for the own user,
/// carrying the confirmed flags in [`crate::types::User::voice`].
#[async_trait]
pub trait SelfVoiceState: Voice {
    async fn set_self_voice_state(
        &self,
        location: &Identifier<Place<Room>>,
        state: VoiceState,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
}

/// Updates in the status of the voice call
pub enum VoiceEvent {
    /// A non-connected sub-state update (Connecting/Failed). The connected
    /// transition is signalled by [`VoiceEvent::CallStreamReady`] instead.
    CallStatusUpdate(CallStatus),
    /// Audio stream for a newly-connected call. Receiving this implies the call
    /// is connected.
    CallStreamReady(WeakSocketStream<AudioEvent>),
    ParticipantJoined {
        room: Identifier<()>,
        user: Identifier<User>,
    },
    ParticipantLeft {
        user_id: ID,
    },
}
/// Audio events that are meant to be processed with the mixer
pub enum AudioEvent {
    /// Request to attach an audio source into the audio graph.
    ///
    /// The receiver receives a `SampleProducer` used to push samples into the system.
    AddAudioSource {
        /// The user whose voice this source plays, when the backend can tell —
        /// the same [`ID`] the roster events ([`VoiceEvent::ParticipantJoined`])
        /// use, so the app can bind per-user audio controls to it.
        user: Option<ID>,
        channel: oneshot::Sender<SampleProducer>,
    },
    /// Request to attach a local audio input (microphone) for sending to voice.
    ///
    /// The receiver receives a `SampleConsumer` used to pull samples from the input stream.
    AddAudioInput(oneshot::Sender<SampleConsumer>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Implements only the required [`Voice`] core.
    struct CoreVoice;
    /// Implements the core plus the self-state capability.
    struct StatefulVoice;

    macro_rules! impl_voice_core {
        ($ty:ty) => {
            #[async_trait]
            impl Voice for $ty {
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
    impl SelfVoiceState for StatefulVoice {
        async fn set_self_voice_state(
            &self,
            _: &Identifier<Place<Room>>,
            _: VoiceState,
        ) -> Result<(), Box<dyn Error + Sync + Send>> {
            unimplemented!()
        }
    }

    /// The `VoiceCaster*` specialization resolves per concrete type —
    /// implementors answer `Ok`, everyone else `NotImplemented` — and the
    /// resolution survives erasure to `dyn Voice` / `Arc<dyn Voice>`.
    #[test]
    fn voice_casters_resolve_by_concrete_type() {
        let core: Arc<dyn Voice> = Arc::new(CoreVoice);
        assert!(core.self_state().is_err());
        assert!(core.arc_self_state().is_err());

        let stateful: Arc<dyn Voice> = Arc::new(StatefulVoice);
        assert!(stateful.self_state().is_ok());
        assert!(stateful.arc_self_state().is_ok());
    }
}
