use bitflags::bitflags;
use std::{hash::Hash, ops::Deref, path::PathBuf};

/// Unique identifier type used throughout the messenger interface.
pub type ID = u64;

/// A type-safe identifier that pairs a unique ID with associated data.
///
/// This allows comparing identifiers by ID while maintaining type safety
/// and carrying additional data alongside the identifier.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct Identifier<D> {
    id: ID,
    data: D,
}
impl<D> Identifier<D> {
    /// Create a new identifier with the given ID and data.
    pub fn new(id: ID, data: D) -> Self {
        Self { id, data }
    }

    /// Get a reference to the unique ID.
    pub fn id(&self) -> &ID {
        &self.id
    }

    /// Create a new identifier with the same ID but different data type.
    ///
    /// This is useful for converting between identifier types while preserving
    /// the underlying ID.
    pub fn swap_data<T>(&self, new_data: T) -> Identifier<T> {
        Identifier {
            id: self.id,
            data: new_data,
        }
    }
}
impl<D> Deref for Identifier<D> {
    type Target = D;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}
impl<D, E> PartialEq<Identifier<E>> for Identifier<D> {
    fn eq(&self, other: &Identifier<E>) -> bool {
        self.id == other.id
    }
}

/// Represents a user in the messenger system.
#[derive(Debug, Clone)]
pub struct User {
    /// Display name of the user.
    pub name: String,
    /// Optional path to the user's avatar/icon image.
    pub icon: Option<PathBuf>,
}

/// Represents a reaction (emoji) on a message.
#[derive(Debug, Clone)]
pub struct Reaction {
    /// The emoji character used for this reaction.
    pub emoji: char,
    /// Number of users who added this reaction.
    pub count: u32,
}

/// Represents a message in a chat room/channel.
#[derive(Debug, Clone)]
pub struct Message {
    /// The text content of the message.
    pub text: String,
    /// List of reactions on this message.
    pub reactions: Vec<Reaction>,
}

/// Bitflags representing the capabilities supported by a room/channel.
///
/// Rooms can support text chat, voice chat, or both.
bitflags! {
    #[derive(Debug, Clone)]
    pub struct RoomCapabilities: u8 {
        /// Room supports text messaging.
        const Text = 0b0000_0001;
        /// Room supports voice chat.
        const Voice = 0b0000_0010;
    }
}

// === Legacy Interface (Deprecated) ===
//
// The following types were part of the old interface design and have been
// replaced by the new type-safe generic approach below. Kept for reference
// during migration.
//
// #[derive(Debug, Clone)]
// pub enum QueryPlace {
//     Room,
//     House,
//     All,
// }
// #[derive(Debug, Clone)]
// pub struct Room {
//     pub name: String,
//     pub icon: Option<PathBuf>,
//     pub room_capabilities: RoomCapabilities,
//     pub participants: Vec<Identifier<User>>,
// }
// #[derive(Debug, Clone)]
// pub struct House {
//     pub name: String,
//     pub icon: Option<PathBuf>,
//     pub rooms: Vec<Identifier<Room>>,
// }
// #[derive(Debug, Clone)]
// pub enum Place {
//     Room(Room),
//     House(House),
// }

// === New Interface ===

/// Type alias for optionally fetched data.
///
/// Used to indicate that certain fields may not be loaded immediately
/// and can be fetched on demand.
type Fetched<T> = Option<T>;

/// Represents a room/channel in the messenger system.
///
/// Rooms contain capabilities (text/voice) and optionally loaded data
/// about participants and messages.
#[derive(Debug, Clone)]
pub struct Room {
    /// Capabilities supported by this room (text, voice, etc.).
    pub room_capabilities: RoomCapabilities,
    /// List of participants, if fetched.
    pub participants: Fetched<Vec<Identifier<User>>>,
    /// List of messages, if fetched.
    pub messages: Fetched<Vec<Identifier<Message>>>,
}
impl Room {
    /// Create a new Room with the given capabilities and optional data.
    pub fn new(
        room_capabilities: RoomCapabilities,
        participants: Fetched<Vec<Identifier<User>>>,
        messages: Fetched<Vec<Identifier<Message>>>,
    ) -> Self {
        Self {
            room_capabilities,
            participants,
            messages,
        }
    }
}

/// Represents a house/server/guild that contains multiple rooms.
///
/// Houses are containers for rooms and may have their own metadata.
#[derive(Debug, Clone)]
pub struct House {
    /// List of rooms in this house, if fetched.
    pub rooms: Fetched<Vec<Identifier<Place<Room>>>>,
}
impl House {
    /// Create a new House with optional rooms.
    pub fn new(rooms: Fetched<Vec<Identifier<Place<Room>>>>) -> Self {
        Self { rooms }
    }
}

/// A generic place that can contain either room or house data.
///
/// This provides a unified way to represent locations in the messenger
/// hierarchy while maintaining type safety through the generic parameter.
#[derive(Debug, Clone)]
pub struct Place<PD> {
    /// Display name of the place.
    pub name: String,
    /// Optional path to the place's icon/avatar image.
    pub icon: Option<PathBuf>,
    /// Type-specific data (Room or House).
    place_data: PD,
}
impl<PD> Place<PD> {
    /// Create a new Place with the given name, icon, and place data.
    pub fn new(name: String, icon: Option<PathBuf>, place_data: PD) -> Self {
        Self {
            name,
            icon,
            place_data,
        }
    }
}
impl<PD> Deref for Place<PD> {
    type Target = PD;

    fn deref(&self) -> &Self::Target {
        &self.place_data
    }
}

/// Status of a voice call connection.
#[derive(Debug, Clone)]
pub enum CallStatus {
    /// Successfully connected to the voice channel.
    Connected,
    /// Currently attempting to connect to the voice channel.
    Connecting,
    /// Not connected to the voice channel.
    Disconnected,
}
