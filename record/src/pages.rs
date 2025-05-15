pub mod chat;
pub mod login;

use crate::socket::SocketEvent;
use chat::Message as MessangerMessage;
pub use login::Login;
use login::Message as LoginMessage;

use crate::Page;

#[derive(Debug)]
pub(crate) enum MyAppMessage {
    // Actions
    OpenPage(Page),
    AuthDiskSync,
    SocketConnect,
    SocketEvent(SocketEvent),
    // Pages
    Login(LoginMessage),
    Chat(MessangerMessage),
}
