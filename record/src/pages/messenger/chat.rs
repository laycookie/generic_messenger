use std::borrow::Borrow;

use adaptors::types::{Chan, Identifier};
use iced::{
    Element, Length, Padding, Task,
    widget::{
        Button, Column, Scrollable, Text, TextInput, column, container, image, row,
        text::LineHeight,
    },
};

use crate::{
    components::message_text::message_text,
    messanger_unifier::{MessangerInterface, Messangers},
};

#[derive(Debug, Clone)]
pub struct Chat {
    interface: MessangerInterface,
    channel_data: Identifier<Chan>,
    msg_box: String,
}

#[derive(Debug, Clone)]
pub enum Action {
    Call {
        interface: MessangerInterface,
        channel: Identifier<Chan>,
    },
    Message(Message),
}

#[derive(Debug, Clone)]
pub enum Message {
    MsgInput(String),
    MsgSend,
}

impl Chat {
    pub fn new(interface: MessangerInterface, channel_data: Identifier<Chan>) -> Self {
        Self {
            interface,
            channel_data,
            msg_box: String::new(),
        }
    }

    pub fn get_element<'a>(&self, messengers: &'a Messangers) -> Element<'a, Action> {
        let channel_info = row![
            Text::new(self.channel_data.name.clone()),
            Button::new("CALL").on_press(Action::Call {
                interface: self.interface.clone(),
                channel: self.channel_data.clone()
            })
        ];

        let messages = messengers
            .data_from_handle(self.interface.handle)
            .unwrap()
            .chats
            .get(self.channel_data.borrow());

        let chat = Scrollable::new(match messages {
            Some(messages) => messages
                .iter()
                .map(|msg| {
                    let icon = msg.data.author.data.icon.clone();
                    let icon = icon.unwrap_or_else(|| "./public/imgs/placeholder.jpg".into());
                    let image_height = Length::Fixed(36.0);
                    row![
                        image(&icon).height(image_height),
                        container(message_text(msg)).padding(Padding::new(0.0).left(5.0))
                    ]
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

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::MsgInput(change) => {
                self.msg_box = change;
                Task::none()
            }
            Message::MsgSend => {
                let auth = self.interface.api.to_owned();
                let meta_data = self.channel_data.clone();
                let contents = self.msg_box.clone();

                Task::future(async move {
                    let param = auth.param_query().unwrap();
                    if let Err(e) = param.send_message(&meta_data, contents).await {
                        eprintln!("{e:#?}");
                    };
                })
                .then(|_| Task::done(Message::MsgInput(String::new())))
            }
        }
    }
}
