//! Messenger abstraction layer.
//!
//! This module defines the traits that each messenger backend (Discord, Matrix, etc.)
//! must implement so the rest of the application can interact with it in a uniform way.
//!
//! Design notes:
//! - Most trait methods provide a default implementation that returns
//!   [`MessengerError::NotImplemented`]. This allows backends to opt into only the
//!   features they support.
//! - Errors are intentionally trait-object based (`Box<dyn Error + Send + Sync>`) to
//!   allow each backend to return its own error types without exposing them here.
use std::fmt::Debug;
use std::{error::Error, sync::Arc};

// QueryPlace is kept for reference in the commented-out legacy code below
use crate::types::{House, ID, Identifier, Message, Place, Room, User};

pub use crate::stream::{ArcStream, WeakSocketStream};

use async_trait::async_trait;
use facet::Facet;
use futures::channel::oneshot;
use simple_audio_channels::input::SampleConsumer;
use simple_audio_channels::output::SampleProducer;

#[derive(Debug, Facet)]
#[facet(derive(Error))]
#[repr(u8)]
pub enum MessengerError {
    // #[facet(diagnostic::help = "Feature not implemented on this messenger")]
    NotImplemented,
    // #[facet(diagnostic::help = "Has some prerequest that wasnt fullfiled")]
    Requires,
}

pub trait MessengerCasterQuery {
    /// Capability: query (fetch) state from the messenger.
    fn query(&self) -> Result<&dyn Query, MessengerError>;
    fn arc_query(self: Arc<Self>) -> Result<Arc<dyn Query>, MessengerError>;
}
impl<T: Messenger> MessengerCasterQuery for T {
    default fn query(&self) -> Result<&dyn Query, MessengerError> {
        Err(MessengerError::NotImplemented)
    }
    default fn arc_query(self: Arc<Self>) -> Result<Arc<dyn Query>, MessengerError> {
        Err(MessengerError::NotImplemented)
    }
}
impl<T: Messenger + Query + 'static> MessengerCasterQuery for T {
    fn query(&self) -> Result<&dyn Query, MessengerError> {
        Ok(self)
    }
    fn arc_query(self: Arc<Self>) -> Result<Arc<dyn Query>, MessengerError> {
        Ok(self)
    }
}

pub trait MessengerCasterText {
    /// Capability: text chat integration.
    fn text(&self) -> Result<&dyn Text, MessengerError>;
    fn arc_text(self: Arc<Self>) -> Result<Arc<dyn Text>, MessengerError>;
}
impl<T: Messenger> MessengerCasterText for T {
    default fn text(&self) -> Result<&dyn Text, MessengerError> {
        Err(MessengerError::NotImplemented)
    }
    default fn arc_text(self: Arc<Self>) -> Result<Arc<dyn Text>, MessengerError> {
        Err(MessengerError::NotImplemented)
    }
}
impl<T: Messenger + Text + 'static> MessengerCasterText for T {
    fn text(&self) -> Result<&dyn Text, MessengerError> {
        Ok(self)
    }
    fn arc_text(self: Arc<Self>) -> Result<Arc<dyn Text>, MessengerError> {
        Ok(self)
    }
}

pub trait MessengerCasterVoice {
    /// Capability: voice chat integration.
    fn voice(&self) -> Result<&dyn Voice, MessengerError>;
    fn arc_voice(self: Arc<Self>) -> Result<Arc<dyn Voice>, MessengerError>;
}
impl<T: Messenger> MessengerCasterVoice for T {
    default fn voice(&self) -> Result<&dyn Voice, MessengerError> {
        Err(MessengerError::NotImplemented)
    }
    default fn arc_voice(self: Arc<Self>) -> Result<Arc<dyn Voice>, MessengerError> {
        Err(MessengerError::NotImplemented)
    }
}
impl<T: Messenger + Voice + 'static> MessengerCasterVoice for T {
    fn voice(&self) -> Result<&dyn Voice, MessengerError> {
        Ok(self)
    }
    fn arc_voice(self: Arc<Self>) -> Result<Arc<dyn Voice>, MessengerError> {
        Ok(self)
    }
}

/// A concrete messenger backend.
///
/// Implement this for each integration (e.g. Discord, Matrix, etc).
///
/// Feature areas are split into smaller traits (`Query`, `Socket`, `VC`, ...). Backends
/// can expose a sub-API by returning `Ok(&impl Trait)` from the corresponding method,
/// or return [`MessengerError::NotImplemented`] if they don't support it.
pub trait Messenger: Send + Sync
where
    Self: MessengerCasterQuery + MessengerCasterText + MessengerCasterVoice,
{
    // TODO: Replace auth_obj with a better representation then str
    fn create_messenger(auth_obj: &str) -> Arc<dyn Messenger>
    where
        Self: Sized;
    /// Stable unique id for this backend instance (e.g. includes account/server context).
    fn id(&self) -> String;
    /// Human-readable backend name (e.g. `"discord"`).
    fn name(&self) -> &'static str;
    /// Backend-provided auth identifier (token, session id, etc).
    ///
    /// Note: the exact format is backend-defined.
    fn auth(&self) -> String;
}

impl PartialEq for dyn Messenger {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

/// Query-only API for fetching state from a messenger.
///
/// This is intentionally read-only; mutations belong in more specialized traits.
#[async_trait]
pub trait Query: Send + Sync {
    /// Fetch the current "client user" (the authenticated account / profile).
    async fn client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch users that the messenger considers "contacts" (friends, following, etc).
    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Fetch all rooms/channels available to the client.
    async fn rooms(&self) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Fetch all houses/servers/guilds available to the client.
    async fn houses(&self) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch detailed information about a specific room/channel.
    async fn room_details(
        &self,
        _room: Identifier<Place<Room>>,
    ) -> Result<Room, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch detailed information about a specific house/server/guild.
    async fn house_details(
        &self,
        _house: Identifier<Place<House>>,
    ) -> Result<House, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<QueryEvent>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
}

/// Text chat API for reading/sending messages in a room/channel.
#[async_trait]
pub trait Text: Send + Sync {
    /// Load messages in `location`, optionally paginating older messages by passing
    /// `load_messages_before`.
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>>;

    /// Add a reaction to a message.
    async fn add_reaction(
        &self,
        _location: &Identifier<Place<Room>>,
        _message: &Identifier<Message>,
        _emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Remove own reaction from a message.
    async fn remove_reaction(
        &self,
        _location: &Identifier<Place<Room>>,
        _message: &Identifier<Message>,
        _emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Send a new message into `location`.
    ///
    /// Returns the confirmed message (with server-assigned ID) on success.
    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>>;
    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<TextEvent>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
}

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
/// (More detailed voice controls likely live in `VC`.)
#[async_trait]
pub trait Voice: Send + Sync {
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

/// Realtime events emitted by a [`Socket`].
#[derive(Debug)]
pub enum SocketEvent {
    /// A message was created in a channel.
    MessageCreated {
        room: Identifier<()>,
        message: Identifier<Message>,
    },
    /// A message was edited/updated in a channel.
    MessageUpdated {
        room: Identifier<()>,
        message: Identifier<Message>,
    },
    /// A message was deleted in a channel.
    MessageDeleted {
        room: Identifier<()>,
        message_id: ID,
    },
    /// A reaction was added to a message.
    ReactionAdded {
        room: Identifier<()>,
        message_id: ID,
        user_id: ID,
        emoji: String,
    },
    /// A reaction was removed from a message.
    ReactionRemoved {
        room: Identifier<()>,
        message_id: ID,
        user_id: ID,
        emoji: String,
    },
    /// A channel/room was created (optionally within a server/place).
    ChannelCreated {
        r#where: Option<Identifier<()>>,
        room: Identifier<Place<Room>>,
    },
    CallStatusUpdate(CallStatus),
    /// Audio stream for a newly-connected call. Receiving this implies the call
    /// is connected — no separate status update is emitted for that transition.
    CallStreamReady(WeakSocketStream<AudioEvent>),
    /// Request to attach an audio source into the audio graph.
    AddAudioSource(oneshot::Sender<SampleProducer>),
    /// Request to attach a local audio input (microphone) for sending to voice.
    AddAudioInput(oneshot::Sender<SampleConsumer>),
    /// Socket disconnected (cleanly or due to error).
    Disconnected,
    /// No-op / placeholder event (used by some backends to "tick" the stream).
    Skip,
}

impl From<QueryEvent> for SocketEvent {
    fn from(value: QueryEvent) -> Self {
        match value {
            QueryEvent::ChannelCreated { r#where, room } => {
                SocketEvent::ChannelCreated { r#where, room }
            }
        }
    }
}
impl From<TextEvent> for SocketEvent {
    fn from(value: TextEvent) -> Self {
        match value {
            TextEvent::MessageCreated { room, message } => {
                SocketEvent::MessageCreated { room, message }
            }
            TextEvent::MessageUpdated { room, message } => {
                SocketEvent::MessageUpdated { room, message }
            }
            TextEvent::MessageDeleted { room, message_id } => {
                SocketEvent::MessageDeleted { room, message_id }
            }
            TextEvent::ReactionAdded {
                room,
                message_id,
                user_id,
                emoji,
            } => SocketEvent::ReactionAdded {
                room,
                message_id,
                user_id,
                emoji,
            },
            TextEvent::ReactionRemoved {
                room,
                message_id,
                user_id,
                emoji,
            } => SocketEvent::ReactionRemoved {
                room,
                message_id,
                user_id,
                emoji,
            },
        }
    }
}
impl From<VoiceEvent> for SocketEvent {
    fn from(value: VoiceEvent) -> Self {
        match value {
            VoiceEvent::CallStatusUpdate(call_status) => SocketEvent::CallStatusUpdate(call_status),
            VoiceEvent::CallStreamReady(stream) => SocketEvent::CallStreamReady(stream),
            VoiceEvent::ParticipantJoined { .. } | VoiceEvent::ParticipantLeft { .. } => {
                SocketEvent::Skip
            }
        }
    }
}
impl From<AudioEvent> for SocketEvent {
    fn from(value: AudioEvent) -> Self {
        match value {
            AudioEvent::AddAudioSource(sender) => SocketEvent::AddAudioSource(sender),
            AudioEvent::AddAudioInput(sender) => SocketEvent::AddAudioInput(sender),
        }
    }
}

pub enum QueryEvent {
    /// A channel/room was created (optionally within a server/place).
    ChannelCreated {
        r#where: Option<Identifier<()>>,
        // r#where: Option<Identifier<Place<House>>>,
        room: Identifier<Place<Room>>,
    },
}
pub enum TextEvent {
    /// A message was created in a channel.
    MessageCreated {
        room: Identifier<()>,
        message: Identifier<Message>,
    },
    /// A message was edited/updated in a channel.
    MessageUpdated {
        room: Identifier<()>,
        message: Identifier<Message>,
    },
    /// A message was deleted in a channel.
    MessageDeleted {
        room: Identifier<()>,
        message_id: ID,
    },
    /// A reaction was added to a message.
    ReactionAdded {
        room: Identifier<()>,
        message_id: ID,
        user_id: ID,
        emoji: String,
    },
    /// A reaction was removed from a message.
    ReactionRemoved {
        room: Identifier<()>,
        message_id: ID,
        user_id: ID,
        emoji: String,
    },
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
    AddAudioSource(oneshot::Sender<SampleProducer>),
    /// Request to attach a local audio input (microphone) for sending to voice.
    ///
    /// The receiver receives a `SampleConsumer` used to pull samples from the input stream.
    AddAudioInput(oneshot::Sender<SampleConsumer>),
}
