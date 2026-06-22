//! The location hierarchy: [`Room`]s and [`House`]s wrapped by a generic
//! [`Place`], plus the [`RoomCapabilities`] flags.

use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use bitflags::bitflags;

use super::identifier::Identifier;
use super::message::Message;
use super::user::User;

bitflags! {
    /// Bitflags representing the capabilities supported by a room/channel.
    ///
    /// Rooms can support text chat, voice chat, or both.
    #[derive(Debug, Clone, Copy)]
    pub struct RoomCapabilities: u8 {
        /// Room supports text messaging.
        const Text = 0b0000_0001;
        /// Room supports voice chat.
        const Voice = 0b0000_0010;
    }
}

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
    /// List of messages, if fetched.
    pub messages: Fetched<Vec<Identifier<Message>>>,
    /// List of participants, if fetched.
    pub participants: Fetched<Vec<Identifier<User>>>,
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
    pub place_data: PD,
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
impl<PD> DerefMut for Place<PD> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.place_data
    }
}
