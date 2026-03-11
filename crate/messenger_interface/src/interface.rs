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
use std::error::Error;
use std::fmt::Debug;

// QueryPlace is kept for reference in the commented-out legacy code below
use crate::types::{House, Identifier, Message, Place, Room, User};

pub use crate::stream::{ArcStream, WeakSocketStream};

use async_trait::async_trait;
use facet::Facet;
use futures::channel::oneshot;
use simple_audio_channels::{CHANNEL_BUFFER_SIZE, input::Input, output::Output};

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
    fn query(&self) -> Result<&dyn Query, MessengerError> {
        Err(MessengerError::NotImplemented)
    }
}
impl<T: Query> MessengerCasterQuery for T {
    fn query(&self) -> Result<&dyn Query, MessengerError> {
        Ok(self)
    }
}

pub trait MessengerCasterText {
    /// Capability: text chat integration.
    fn text(&self) -> Result<&dyn Text, MessengerError> {
        Err(MessengerError::NotImplemented)
    }
}
impl<T: Text> MessengerCasterText for T {
    fn text(&self) -> Result<&dyn Text, MessengerError> {
        Ok(self)
    }
}

pub trait MessengerCasterVoice {
    /// Capability: voice chat integration.
    fn voice(&self) -> Result<&dyn Voice, MessengerError> {
        Err(MessengerError::NotImplemented)
    }
}
impl<T: Voice> MessengerCasterVoice for T {
    fn voice(&self) -> Result<&dyn Voice, MessengerError> {
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
        room: Identifier<Place<Room>>,
    ) -> Result<Room, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch detailed information about a specific house/server/guild.
    async fn house_details(
        &self,
        house: Identifier<Place<House>>,
    ) -> Result<House, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    async fn listen(&self) -> Result<WeakSocketStream<QueryEvent>, Box<dyn Error + Sync + Send>> {
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

    /// Send a new message into `location`.
    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
    async fn listen(&self) -> Result<WeakSocketStream<TextEvent>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
}

/// Status of a voice call connection.
pub enum CallStatus {
    /// Successfully connected to the voice channel.
    Connected(WeakSocketStream<AudioEvent>),
    /// Currently attempting to connect to the voice channel.
    Connecting(&'static str), // String contains info of what stage in the "Connecting" pipeline we are at.
    Failed,
}
impl Debug for CallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected(_) => f.debug_tuple("Connected").finish(),
            Self::Connecting(arg0) => f.debug_tuple("Connecting").field(arg0).finish(),
            Self::Failed => write!(f, "Failed"),
        }
    }
}

pub enum VoiceEvent {
    CallStatusUpdate(CallStatus),
}

/// Minimal voice lifecycle API.
///
/// (More detailed voice controls likely live in `VC`.)
#[async_trait]
pub trait Voice: Send + Sync {
    /// Connect to voice in `location`.
    async fn connect<'a>(
        &'a self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>>;
    /// Disconnect from voice in `location`.
    async fn disconnect<'a>(&'a self, location: &Identifier<Place<Room>>);

    async fn listen(&self) -> Result<WeakSocketStream<VoiceEvent>, Box<dyn Error + Sync + Send>> {
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
    /// A channel/room was created (optionally within a server/place).
    ChannelCreated {
        r#where: Option<Identifier<()>>,
        room: Identifier<Room>,
    },
    CallStatusUpdate(CallStatus),
    /// Request to attach an audio source into the audio graph.
    ///
    /// The receiver receives a `SampleProducer` used to push samples into the system.
    AddAudioSource(oneshot::Sender<Output<CHANNEL_BUFFER_SIZE>>),
    /// Request to attach a local audio input (microphone) for sending to voice.
    ///
    /// The receiver receives a `SampleConsumer` used to pull samples from the input stream.
    AddAudioInput(oneshot::Sender<Input<CHANNEL_BUFFER_SIZE>>),
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
        }
    }
}
impl From<VoiceEvent> for SocketEvent {
    fn from(value: VoiceEvent) -> Self {
        match value {
            VoiceEvent::CallStatusUpdate(call_status) => SocketEvent::CallStatusUpdate(call_status),
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
        room: Identifier<Room>,
    },
}
pub enum TextEvent {
    /// A message was created in a channel.
    MessageCreated {
        room: Identifier<()>,
        // room: Identifier<Place<Room>>,
        message: Identifier<Message>,
    },
}

pub enum AudioEvent {
    /// Request to attach an audio source into the audio graph.
    ///
    /// The receiver receives a `SampleProducer` used to push samples into the system.
    AddAudioSource(oneshot::Sender<Output<CHANNEL_BUFFER_SIZE>>),
    /// Request to attach a local audio input (microphone) for sending to voice.
    ///
    /// The receiver receives a `SampleConsumer` used to pull samples from the input stream.
    AddAudioInput(oneshot::Sender<Input<CHANNEL_BUFFER_SIZE>>),
}
