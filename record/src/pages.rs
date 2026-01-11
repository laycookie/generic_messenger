//! Contains all UI related state data

pub mod login;
pub mod messenger;

use crate::messanger_unifier::{Call, MessangerHandle};
pub use login::Login;
use login::Message as LoginMessage;
use messenger::Message as MessangerMessage;
use messenger_interface::{
    interface::SocketEvent,
    types::{House, ID, Identifier, Message, Place, Room, User},
};
use std::fmt::Debug;

use crate::Screen;

#[derive(Debug)]
pub(crate) enum MessangerData {
    Everything {
        profile: Identifier<User>,
        contacts: Vec<Identifier<User>>,
        conversations: Vec<Identifier<Room>>,
        servers: Vec<Identifier<House>>,
    },
    Chat((ID, Vec<Identifier<Message>>)),
    Call(Call),
}

#[derive(Debug)]
pub(crate) enum MessangerDataType {
    Call,
}

pub(crate) enum AppMessage {
    // === UI ===
    OpenPage(Screen),
    // === Control Messenger Obj Data ===
    SetMessangerData {
        messanger_handle: MessangerHandle,
        new_data: MessangerData,
    },
    RemoveMessangerData {
        messanger_handle: MessangerHandle,
        data_type: MessangerDataType,
        data_id: ID,
    },
    RemoveMessanger(MessangerHandle),
    // === State Managers ===
    StartUp,
    SaveMessengersCredentialToDisk,
    // === Pages ===
    Login(LoginMessage),
    Chat(MessangerMessage),
    // === Socket ===
    SocketEvent((MessangerHandle, SocketEvent)),
}
