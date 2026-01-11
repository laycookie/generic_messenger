//! Messenger abstraction layer.
//!
//! This module defines the traits that each messenger backend (Discord, Matrix, etc.)
//! must implement so the rest of the application can interact with it in a uniform way.
//!
//! Design notes:
//! - Most trait methods provide a default implementation that returns
//!   [`MessamgerError::NotImplimented`]. This allows backends to opt into only the
//!   features they support.
//! - Errors are intentionally trait-object based (`Box<dyn Error + Send + Sync>`) to
//!   allow each backend to return its own error types without exposing them here.
use std::error::Error;
use std::fmt::Debug;
use std::sync::{Arc, Weak};

use crate::types::{Identifier, Message, Place, QueryPlace, Room, User};
use async_trait::async_trait;
use futures::Stream;
use futures::channel::oneshot;
use simple_audio_channels::SampleProducer;

#[derive(Debug, thiserror::Error)]
pub enum MessamgerError {
    #[error("Feature not implimented on this messanger")]
    NotImplimented,
}

/// A concrete messenger backend.
///
/// Implement this for each integration (e.g. Discord, Matrix, etc).
///
/// Feature areas are split into smaller traits (`Query`, `Socket`, `VC`, ...). Backends
/// can expose a sub-API by returning `Ok(&impl Trait)` from the corresponding method,
/// or return [`MessamgerError::NotImplimented`] if they don't support it.
#[async_trait]
pub trait Messanger: Send + Sync {
    /// Stable unique id for this backend instance (e.g. includes account/server context).
    fn id(&self) -> String;
    /// Human-readable backend name (e.g. `"discord"`).
    fn name(&self) -> &'static str;
    /// Backend-provided auth identifier (token, session id, etc).
    ///
    /// Note: the exact format is backend-defined.
    fn auth(&self) -> String;

    /// Capability: query (fetch) state from the messenger.
    fn query(&self) -> Result<&dyn Query, MessamgerError> {
        Err(MessamgerError::NotImplimented)
    }

    /// Capability: text chat integration.
    fn text(&self) -> Result<&dyn Text, MessamgerError> {
        Err(MessamgerError::NotImplimented)
    }

    /// Capability: voice chat integration.
    fn voice(&self) -> Result<&dyn Voice, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }

    /// Capability: open a realtime socket/stream for events (messages, channel changes, etc).
    ///
    /// Returns a `Weak` reference because sockets are owned/managed elsewhere and
    /// can outlive or be droped independent of the caller.
    async fn socket(
        self: Arc<Self>,
    ) -> Result<Weak<dyn Socket + Send + Sync>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }
}
impl PartialEq for dyn Messanger {
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
    async fn query_client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }
    /// Fetch users that the messenger considers "contacts" (friends, following, etc).
    async fn query_contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }

    /// Fetch "places" (servers/guilds/spaces/etc) containing rooms/channels.
    ///
    /// `query_place` describes how to filter/locate places for the backend.
    async fn query_place(
        &self,
        query_place: QueryPlace,
    ) -> Result<Vec<Identifier<Place>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }
}

/// Text chat API for reading/sending messages in a room/channel.
#[async_trait]
pub trait Text: Send + Sync {
    /// Load messages in `location`, optionally paginating older messages by passing
    /// `load_messages_before`.
    async fn get_messages(
        &self,
        location: &Identifier<Room>,
        load_messages_before: Option<Identifier<Message>>,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>>;

    /// Send a new message into `location`.
    async fn send_message(
        &self,
        location: &Identifier<Room>,
        contents: Message,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
}

/// Minimal voice lifecycle API.
///
/// (More detailed voice controls likely live in `VC`.)
#[async_trait]
pub trait Voice: Send + Sync {
    /// Connect to voice in `location`.
    async fn connect<'a>(&'a self, location: &Identifier<Room>);
    /// Disconnect from voice in `location`.
    async fn disconnect<'a>(&'a self, location: &Identifier<Room>);
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
        place: Option<Identifier<()>>,
        room: Identifier<Room>,
    },
    /// Request to attach an audio source into the audio graph.
    ///
    /// The receiver gets a `SampleProducer` used to push samples into the system.
    AddAudioSource(oneshot::Sender<SampleProducer<5120>>),
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
