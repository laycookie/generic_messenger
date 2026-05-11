pub mod login;
pub mod messenger;

use crate::Screen;
use crate::state::{MessengerId, MessengerRegistry};
use messenger_interface::interface::{AudioEvent, QueryEvent, TextEvent, VoiceEvent};

#[derive(Debug, Clone, Copy)]
pub enum StreamDirection {
    Input,
    Output,
}

impl std::fmt::Display for StreamDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input => f.write_str("input"),
            Self::Output => f.write_str("output"),
        }
    }
}

pub(crate) enum AppMessage {
    // === Navigation ===
    Navigate(Screen),
    // === Messenger Data ===
    ModifyMessengers(Box<dyn FnOnce(&mut MessengerRegistry) + Send>),
    // === Lifecycle ===
    StartUp,
    SaveCredentials,
    // === Audio ===
    StartStream(StreamDirection),
    StopStream(StreamDirection),
    // === Pages ===
    Login(login::Message),
    Chat(messenger::Message),
    // === Socket ===
    QueryEvent((MessengerId, QueryEvent)),
    TextEvent((MessengerId, TextEvent)),
    VoiceEvent((MessengerId, VoiceEvent)),
    AudioEvent((MessengerId, AudioEvent)),
}

impl AppMessage {
    pub fn modify_messengers(f: impl FnOnce(&mut MessengerRegistry) + Send + 'static) -> Self {
        Self::ModifyMessengers(Box::new(f))
    }

    pub fn modify_data(
        id: MessengerId,
        f: impl FnOnce(&mut crate::state::MessengerData) + Send + 'static,
    ) -> Self {
        Self::modify_messengers(move |messengers| {
            if let Some(entry) = messengers.get_mut(id) {
                f(&mut entry.data);
            }
        })
    }
}
