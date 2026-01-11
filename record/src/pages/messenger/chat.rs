use iced::{
    Element, Length, Task,
    widget::{Button, Column, Scrollable, Text, TextInput, column, row, text::LineHeight},
};
use messenger_interface::types::{Identifier, Message as InterfaceMessage, Room};
use tracing::error;

use crate::{
    components::message_text::message_text,
    messanger_unifier::{MessangerInterface, Messangers},
};

#[derive(Clone)]
pub struct Chat {
    interface: MessangerInterface,
    room: Identifier<Room>,
    msg_box: String,
}

#[derive(Clone)]
pub enum Action {
    Call {
        interface: MessangerInterface,
        room: Identifier<Room>,
    },
    Message(Message),
}

#[derive(Debug, Clone)]
pub enum Message {
    MsgInput(String),
    MsgSend,
}

impl Chat {
    pub fn new(interface: MessangerInterface, room: Identifier<Room>) -> Self {
        Self {
            interface,
            room,
            msg_box: String::new(),
        }
    }

    pub fn get_element<'a>(&self, messengers: &'a Messangers) -> Element<'a, Action> {
        let channel_info = row![
            Text::new(self.room.name.clone()),
            Button::new("CALL").on_press(Action::Call {
                interface: self.interface.clone(),
                room: self.room.clone()
            })
        ];

        let messages = messengers
            .data_from_handle(self.interface.handle)
            .unwrap()
            .chats
            .get(self.room.id());

        let chat = Scrollable::new(match messages {
            Some(messages) => messages
                .iter()
                .map(|msg| message_text(msg))
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

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::MsgInput(change) => {
                self.msg_box = change;
                Task::none()
            }
            Message::MsgSend => {
                let auth = self.interface.api.to_owned();
                let meta_data = self.room.clone();
                let contents = self.msg_box.clone();

                Task::future(async move {
                    let t = auth.text();
                    let text = match t {
                        Ok(t) => t,
                        Err(e) => {
                            error!("Text not supported by adapter: {e:?}");
                            return;
                        }
                    };
                    if let Err(e) = text
                        .send_message(
                            &meta_data,
                            InterfaceMessage {
                                text: contents,
                                reactions: Vec::new(),
                            },
                        )
                        .await
                    {
                        error!("{e:#?}");
                    };
                })
                .then(|_| Task::done(Message::MsgInput(String::new())))
            }
        }
    }
}
