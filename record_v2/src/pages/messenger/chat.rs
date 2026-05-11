use iced::{
    Element, Length, Task,
    widget::{Button, Column, Scrollable, Text, TextInput, column, row, text::LineHeight},
};
use messenger_interface::types::{ID, Identifier, Message as InterfaceMessage, Place, Room};

use crate::{
    components::message_text::message_text,
    state::{MessengerInterface, MessengerRegistry},
};

#[derive(Clone)]
pub struct Chat {
    pub interface: MessengerInterface,
    pub room: Identifier<Place<Room>>,
    msg_box: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    MsgInput(String),
    MsgSend,
    ToggleReaction {
        message_id: ID,
        emoji: String,
        reacted: bool,
    },
}

#[derive(Clone)]
pub enum Action {
    Call {
        interface: MessengerInterface,
        room: Identifier<Place<Room>>,
    },
    Message(Message),
}

pub enum UpdateResult {
    Task(Task<Message>),
    /// User wants to send a message. The parent handles the optimistic insert + async send.
    Send {
        interface: MessengerInterface,
        room: Identifier<Place<Room>>,
        contents: String,
    },
    /// User clicked a reaction button — toggle it on the server.
    ToggleReaction {
        interface: MessengerInterface,
        room: Identifier<Place<Room>>,
        message_id: ID,
        emoji: String,
        reacted: bool,
    },
}

impl Chat {
    pub fn new(interface: MessengerInterface, room: Identifier<Place<Room>>) -> Self {
        Self {
            interface,
            room,
            msg_box: String::new(),
        }
    }

    pub fn get_element<'a>(&self, messengers: &'a MessengerRegistry) -> Element<'a, Action> {
        let channel_info = row![
            Text::new(self.room.name.clone()),
            Button::new("CALL").on_press(Action::Call {
                interface: self.interface.clone(),
                room: self.room.clone()
            })
        ];

        let messages = messengers
            .data(self.interface.id)
            .and_then(|d| d.chats.get(self.room.id()));

        let chat = Scrollable::new(match messages {
            Some(messages) => messages
                .iter()
                .map(|msg| {
                    message_text(msg, |msg, emoji, reacted| {
                        Action::Message(Message::ToggleReaction {
                            message_id: *msg.id(),
                            emoji: emoji.to_owned(),
                            reacted,
                        })
                    })
                })
                .fold(Column::new().spacing(15.0), |column, widget| {
                    column.push(widget)
                }),
            None => Column::new(),
        })
        .anchor_bottom()
        .width(Length::Fill)
        .height(Length::Fill);

        let message_box = TextInput::new("New msg...", &self.msg_box)
            .on_input(|s| Action::Message(Message::MsgInput(s)))
            .on_submit(Action::Message(Message::MsgSend))
            .line_height(LineHeight::Absolute(20.into()));

        column![channel_info, chat, message_box].into()
    }

    pub fn update(&mut self, message: Message) -> UpdateResult {
        match message {
            Message::MsgInput(change) => {
                self.msg_box = change;
                UpdateResult::Task(Task::none())
            }
            Message::MsgSend => {
                let contents = std::mem::take(&mut self.msg_box);
                if contents.is_empty() {
                    return UpdateResult::Task(Task::none());
                }
                UpdateResult::Send {
                    interface: self.interface.clone(),
                    room: self.room.clone(),
                    contents,
                }
            }
            Message::ToggleReaction {
                message_id,
                emoji,
                reacted,
            } => UpdateResult::ToggleReaction {
                interface: self.interface.clone(),
                room: self.room.clone(),
                message_id,
                emoji,
                reacted,
            },
        }
    }
}
