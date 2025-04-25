pub mod chat;
pub mod login;

use chat::Message as MessangerMessage;
pub use login::Login;
use login::Message as LoginMessage;

use crate::Page;

#[derive(Debug)]
pub enum MyAppMessage {
    // Actions
    OpenPage(Page),
    AuthDiskSync,
    // Pages
    Login(LoginMessage),
    Chat(MessangerMessage),
}

