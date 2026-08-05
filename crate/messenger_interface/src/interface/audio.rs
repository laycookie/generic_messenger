//! Audio call lifecycle ([`Audio`]), the optional per-operation capabilities
//! ([`AudioSelfVoiceState`]) with their casters, call state types, and the realtime
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
    AudioCasterSelfState: Audio => AudioSelfVoiceState { self_state, arc_self_state }
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

/// Minimal audio-call lifecycle API.
///
/// Optional per-operation capabilities (setting the own mute/deafen state)
/// live in their own capability traits ([`AudioSelfVoiceState`]); whether a
/// backend supports them is a property of its *type*, surfaced to the UI
/// through the `AudioCaster*` supertraits. Video is *not* a sub-capability
/// of audio — it is its own feature area ([`super::video::Video`]), so a
/// backend can offer video streams without audio calls; backends that carry
/// both over one connection merge them internally.
#[async_trait]
pub trait Audio: Send + Sync + AudioCasterSelfState {
    /// Connect to voice in `location`.
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>>;
    /// Disconnect from voice in `location`.
    async fn disconnect(&self, location: &Identifier<Place<Room>>);

    /// Subscribe to this backend's voice control events.
    ///
    /// This is the long-lived control-plane stream for every call owned by the
    /// backend instance; it does not contain PCM samples. Call it once when the
    /// backend is registered, independently of [`Self::connect`]. A later
    /// `connect` request reports progress through
    /// [`VoiceEvent::CallStatusUpdate`] and reports a usable media connection
    /// through [`VoiceEvent::CallStreamReady`]. Participant joins, leaves, and
    /// confirmed voice-state changes arrive on the same stream.
    ///
    /// [`VoiceEvent::CallStreamReady`] contains a second, per-call
    /// [`WeakSocketStream`] of [`AudioEvent`] values. That inner stream performs
    /// audio-graph handshakes such as requesting microphone input or a playback
    /// source; encoded packets and raw samples never cross this outer stream.
    ///
    /// `self: Arc<Self>` lets an implementation build an owned asynchronous
    /// subscription without borrowing the backend. The returned
    /// [`WeakSocketStream`] holds only a weak reference to its event source, so
    /// it does not keep a discarded backend alive and ends when that source
    /// shuts down. The interface expects one active consumer per backend
    /// feature; implementations may use a draining queue rather than broadcast
    /// fan-out.
    ///
    /// The default reports [`MessengerError::NotImplemented`].
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
pub trait AudioSelfVoiceState: Audio {
    /// Publish the client's desired mute/deafen state for `location`.
    ///
    /// This signals the backend only; local capture/playback muting remains
    /// application responsibility.
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
    CallStatusUpdate {
        /// Zero-data room identity. Backend instance comes from outer event
        /// subscription, so no room graph data is cloned into lifecycle events.
        room: Identifier<()>,
        status: CallStatus,
    },
    /// Audio stream for a newly-connected call. Receiving this implies the call
    /// is connected.
    CallStreamReady {
        /// Room/call this media stream belongs to.
        room: Identifier<()>,
        stream: WeakSocketStream<AudioEvent>,
    },
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

    /// Implements only the required [`Audio`] core.
    struct CoreVoice;
    /// Implements the core plus the self-state capability.
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

    /// The `AudioCaster*` specialization resolves per concrete type —
    /// implementors answer `Ok`, everyone else `NotImplemented` — and the
    /// resolution survives erasure to `dyn Audio` / `Arc<dyn Audio>`.
    #[test]
    fn audio_casters_resolve_by_concrete_type() {
        let core: Arc<dyn Audio> = Arc::new(CoreVoice);
        assert!(core.self_state().is_err());
        assert!(core.arc_self_state().is_err());

        let stateful: Arc<dyn Audio> = Arc::new(StatefulVoice);
        assert!(stateful.self_state().is_ok());
        assert!(stateful.arc_self_state().is_ok());
    }
}
