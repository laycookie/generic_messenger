pub mod chat;
pub mod login;

use std::{fmt::Debug, sync::Arc};

use adaptors::{types::MsgsStore, Messanger};
use chat::{Message as MessangerMessage, MessangerWindow};
use iced::Task;
pub use login::Login;
use login::Message as LoginMessage;

#[derive(Debug, Clone)]
pub enum MyAppMessage {
    // Actions
    AddAuth(Arc<dyn Messanger>),
    LoadConversation(MsgsStore),
    OpenChat(MessangerWindow),
    // Pages
    Login(LoginMessage),
    Chat(MessangerMessage),
}

pub enum UpdateResult<M> {
    Page(Box<dyn Page>),
    Task(Task<M>),
    None,
}

pub trait Page {
    fn update(&mut self, message: MyAppMessage) -> Task<MyAppMessage>;
    fn view(&self) -> iced::Element<MyAppMessage>;
}
