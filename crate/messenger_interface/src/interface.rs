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
use std::sync::{Arc, Weak};

// QueryPlace is kept for reference in the commented-out legacy code below
use crate::types::{House, Identifier, Message, Place, Room, User};
use async_trait::async_trait;
use futures::Stream;
use futures::channel::oneshot;
use simple_audio_channels::output::Output;

#[derive(Debug, thiserror::Error)]
pub enum MessengerError {
    #[error("Feature not implemented on this messenger")]
    NotImplemented,
}

/// A concrete messenger backend.
///
/// Implement this for each integration (e.g. Discord, Matrix, etc).
///
/// Feature areas are split into smaller traits (`Query`, `Socket`, `VC`, ...). Backends
/// can expose a sub-API by returning `Ok(&impl Trait)` from the corresponding method,
/// or return [`MessengerError::NotImplemented`] if they don't support it.
#[async_trait]
pub trait Messenger: Send + Sync {
    /// Stable unique id for this backend instance (e.g. includes account/server context).
    fn id(&self) -> String;
    /// Human-readable backend name (e.g. `"discord"`).
    fn name(&self) -> &'static str;
    /// Backend-provided auth identifier (token, session id, etc).
    ///
    /// Note: the exact format is backend-defined.
    fn auth(&self) -> String;

    /// Capability: query (fetch) state from the messenger.
    fn query(&self) -> Result<&dyn Query, MessengerError> {
        Err(MessengerError::NotImplemented)
    }

    /// Capability: text chat integration.
    fn text(&self) -> Result<&dyn Text, MessengerError> {
        Err(MessengerError::NotImplemented)
    }

    /// Capability: voice chat integration.
    fn voice(&self) -> Result<&dyn Voice, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Capability: open a realtime socket/stream for events (messages, channel changes, etc).
    ///
    /// Returns a `Weak` reference because sockets are owned/managed elsewhere and
    /// can outlive or be dropped independent of the caller.
    async fn socket(
        self: Arc<Self>,
    ) -> Result<Weak<dyn Socket + Send + Sync>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
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

    /// Legacy: Fetch "places" (servers/guilds/spaces/etc) containing rooms/channels.
    ///
    /// This method has been replaced by `rooms()` and `houses()` for better type safety.
    /// The `query_place` parameter described how to filter/locate places for the backend.
    ///
    /// Kept for reference during migration.
    // async fn places(
    //     &self,
    //     query_place: QueryPlace,
    // ) -> Result<Vec<Identifier<PlaceVariant>>, Box<dyn Error + Sync + Send>> {
    //     Err(Box::new(MessengerError::NotImplemented))
    // }

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
}

/// Minimal voice lifecycle API.
///
/// (More detailed voice controls likely live in `VC`.)
#[async_trait]
pub trait Voice: Send + Sync {
    /// Connect to voice in `location`.
    async fn connect<'a>(&'a self, location: &Identifier<Place<Room>>);
    /// Disconnect from voice in `location`.
    async fn disconnect<'a>(&'a self, location: &Identifier<Place<Room>>);
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
    /// Request to attach an audio source into the audio graph.
    ///
    /// The receiver receives a `SampleProducer` used to push samples into the system.
    AddAudioSource(oneshot::Sender<Output<5120>>),
    /// Socket disconnected (cleanly or due to error).
    Disconnected,
    /// No-op / placeholder event (used by some backends to "tick" the stream).
    Skip,
}

/// Realtime socket/stream of messenger events.
///
/// This is both a `Stream` (for poll-based consumers) and provides an async `next`
/// helper for backends that prefer an explicit method.
#[async_trait]
pub trait Socket: Stream<Item = SocketEvent> {
    /// Await the next socket event.
    async fn next(self: Arc<Self>) -> Option<SocketEvent>;
}
