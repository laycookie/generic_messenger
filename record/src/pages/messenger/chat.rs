use std::borrow::Borrow;
use std::sync::Arc;

use adaptors::{
    Messanger as Auth,
    types::{Chan, Identifier},
};
use iced::{
    Element, Length, Padding, Task, advanced,
    widget::{
        Button, Column, Scrollable, Text, TextInput, column, container, image, row,
        text::LineHeight,
    },
};

use crate::messanger_unifier::{MessangerHandle, Messangers};

#[derive(Debug, Clone)]
pub struct Chat {
    interface: (MessangerHandle, Arc<dyn Auth>),
    channel_data: Identifier<Chan>,
    msg_box: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    Call,
    MsgInput(String),
    MsgSend,
}

impl Chat {
    pub fn new(
        interface: (MessangerHandle, Arc<dyn Auth>),
        channel_data: Identifier<Chan>,
    ) -> Self {
        Self {
            interface,
            channel_data,
            msg_box: String::new(),
        }
    }

    pub fn get_element<'a, Theme, Renderer>(
        &self,
        messengers: &'a Messangers,
    ) -> Element<'a, Message, Theme, Renderer>
    where
        Message: Clone,
        Renderer: iced::advanced::Renderer
            + iced::advanced::text::Renderer
            + iced::advanced::image::Renderer
            + 'a,
        <Renderer as advanced::image::Renderer>::Handle:
            for<'c> From<&'c std::path::PathBuf> + From<&'static str>,
        Theme: iced::widget::text::Catalog
            + iced::widget::button::Catalog
            + iced::widget::scrollable::Catalog
            + iced::widget::text_input::Catalog
            + iced::widget::container::Catalog
            + 'a,
    {
        let channel_info = row![
            Text::new(self.channel_data.name.clone()),
            Button::new("CALL").on_press(Message::Call)
        ];

        let messages = messengers
            .data_from_handle(self.interface.0)
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
                        column![
                            container(Text::from(msg.data.author.data.name.as_str()))
                                .center_y(image_height),
                            container(Text::from(msg.data.text.as_str()))
                        ]
                        .padding(Padding::new(0.0).left(5.0))
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
            .on_input(Message::MsgInput)
            .on_submit(Message::MsgSend)
            .line_height(LineHeight::Absolute(20.into()));

        column![channel_info, chat, message_box].into()
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Call => {
                let auth = self.interface.1.clone();
                let meta_data = self.channel_data.clone();

                Task::future(async move {
                    let vc = auth.vc().await;
                    vc.unwrap().connect(&meta_data).await;
                })
                .then(|_| Task::none())
            }
            Message::MsgInput(change) => {
                self.msg_box = change;
                Task::none()
            }
            Message::MsgSend => {
                let auth = self.interface.1.clone();
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
