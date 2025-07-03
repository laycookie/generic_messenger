pub mod chat;
pub mod login;

use std::fmt::Debug;

use crate::{messanger_unifier::MessangerHandle, SocketMesg};
use adaptors::types::{Chan, Identifier, Msg, Server, Usr};
use chat::Message as MessangerMessage;
pub use login::Login;
use login::Message as LoginMessage;

use crate::Screen;

#[derive(Debug)]
pub(crate) enum MessangerData {
    Everything {
        profile: Identifier<Usr>,
        contacts: Vec<Identifier<Usr>>,
        conversations: Vec<Identifier<Chan>>,
        servers: Vec<Identifier<Server>>,
        // chat: (Identifier<()>, Vec<Identifier<Msg>>),
    },
    Profile(Identifier<Usr>),
    Servers(Vec<Identifier<Server>>),
    Chat((Identifier<()>, Vec<Identifier<Msg>>)),
}

#[derive(Debug)]
pub(crate) enum MyAppMessage {
    OpenPage(Screen),
    SetMessangerData {
        messanger_handle: MessangerHandle,
        new_data: MessangerData,
    },
    SocketEvent(SocketMesg),
    StartUp,
    // === Pages ===
    Login(LoginMessage),
    Chat(MessangerMessage),
}
