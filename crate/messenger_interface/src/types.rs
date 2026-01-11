use bitflags::bitflags;
use std::{hash::Hash, ops::Deref, path::PathBuf};

pub type ID = u64;
#[derive(Debug, Clone)]
#[repr(C)]
pub struct Identifier<D> {
    id: ID,
    data: D,
}
impl<D> Identifier<D> {
    pub fn new(id: ID, data: D) -> Self {
        Self { id, data }
    }
    pub fn id(&self) -> &ID {
        &self.id
    }
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

#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub icon: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Reaction {
    pub emoji: char,
    pub count: u32,
}
#[derive(Debug, Clone)]
pub struct Message {
    pub text: String,
    pub reactions: Vec<Reaction>,
}

bitflags! {
    #[derive(Debug, Clone)]
    pub struct RoomCapabilities: u8 {
        const Text = 0b0000_0001;
        const Voice = 0b0000_0010;
    }
}
#[derive(Debug, Clone)]
pub struct Room {
    pub room_capabilities: RoomCapabilities,
    pub name: String,
    pub icon: Option<PathBuf>,
    pub participants: Vec<Identifier<User>>,
}
#[derive(Debug, Clone)]
pub struct House {
    pub name: String,
    pub icon: Option<PathBuf>,
    pub rooms: Vec<Identifier<Room>>,
}

pub enum QueryPlace {
    Room,
    House,
    All,
}
#[derive(Debug, Clone)]
pub enum Place {
    Room(Room),
    House(House),
}

#[derive(Debug, Clone)]
pub enum CallStatus {
    Connected,
    Connecting,
    Disconnected,
}
