//! Contains all UI related state data

pub mod login;
pub mod messenger;

use crate::messanger_unifier::{Call, MessangerHandle};
pub use login::Login;
use login::Message as LoginMessage;
use messaging_interface::{
    interface::SocketEvent,
    types::{Chan, ID, Identifier, Message, Server, Usr},
};
use messenger::Message as MessangerMessage;
use std::fmt::Debug;

use crate::Screen;

#[derive(Debug)]
pub(crate) enum MessangerData {
    Everything {
        profile: Identifier<Usr>,
        contacts: Vec<Identifier<Usr>>,
        conversations: Vec<Identifier<Chan>>,
        servers: Vec<Identifier<Server>>,
    },
    Chat((Identifier<()>, Vec<Identifier<Message>>)),
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
