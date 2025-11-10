//! Contains all UI related state data

pub mod login;
pub mod messenger;

use crate::{
    SocketMesg,
    messanger_unifier::{Call, MessangerHandle},
};
use adaptors::types::{Chan, Identifier, Msg, Server, Usr};
pub use login::Login;
use login::Message as LoginMessage;
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
    Chat((Identifier<()>, Vec<Identifier<Msg>>)),
    Call(Call),
}

#[derive(Debug)]
pub(crate) enum MyAppMessage {
    OpenPage(Screen),
    SetMessangerData {
        messanger_handle: MessangerHandle,
        new_data: MessangerData,
    },
    RemoveMessanger(MessangerHandle),
    SocketEvent(SocketMesg),
    SaveMessengers,
    StartUp,
    // === Pages ===
    Login(LoginMessage),
    Chat(MessangerMessage),
}
