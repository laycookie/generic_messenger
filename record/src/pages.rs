//! Contains all UI related state data

pub mod login;
pub mod messenger;

use crate::messenger_unifier::{Call, MessengerHandle};
pub use login::Login;
use login::Message as LoginMessage;
use messenger::Message as MessengerMessage;
use messenger_interface::{
    interface::SocketEvent,
    types::{House, ID, Identifier, Message, Place, Room, User},
};
use std::fmt::Debug;

use crate::Screen;

#[derive(Debug, Clone)]
pub(crate) enum MessengerData {
    Everything {
        profile: Identifier<User>,
        contacts: Vec<Identifier<User>>,
        conversations: Vec<Identifier<Place<Room>>>,
        servers: Vec<Identifier<Place<House>>>,
    },
    Servers(Vec<Identifier<Place<House>>>),
    Chat((ID, Vec<Identifier<Message>>)),
    Call(Call),
}

#[derive(Debug)]
pub(crate) enum MessengerDataType {
    Call,
}

pub(crate) enum AppMessage {
    // === UI ===
    OpenPage(Screen),
    // === Control Messenger Obj Data ===
    SetMessengerData {
        messenger_handle: MessengerHandle,
        new_data: MessengerData,
    },
    RemoveMessengerData {
        messenger_handle: MessengerHandle,
        data_type: MessengerDataType,
        data_id: ID,
    },
    RemoveMessenger(MessengerHandle),
    // === State Managers ===
    StartUp,
    SaveMessengersCredentialToDisk,
    // === Audio ===
    StartOutputStream,
    StopOutputStream,
    StartInputStream,
    StopInputStream,
    // === Pages ===
    Login(LoginMessage),
    Chat(MessengerMessage),
    // === Socket ===
    SocketEvent((MessengerHandle, SocketEvent)),
}
