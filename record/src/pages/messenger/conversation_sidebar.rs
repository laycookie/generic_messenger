use adaptors::types::{Chan, ChanType, Identifier};
use iced::{
    Color, Element, Length, Padding,
    widget::{Button, Column, Row, Scrollable, Text, button, column, container, image, row},
};

use super::PLACEHOLDER_PFP;
use crate::{
    messanger_unifier::{Call, Messangers},
    pages::messenger::{self, server::Server},
};

#[derive(Debug)]
pub struct Sidebar {
    pub server_selected: Option<Server>,
    pub width: f32,
}

#[derive(Debug, Clone)]
pub enum Action {
    Call(Identifier<Chan>),
    Disconect(Call),
    OpenContacts,
    OpenChat {
        handle: crate::messanger_unifier::MessangerHandle,
        conversation: Identifier<Chan>,
    },
}

impl Sidebar {
    pub fn new(width: f32) -> Self {
        Self {
            server_selected: None,
            width,
        }
    }

    pub fn view<'a>(&'a self, messengers: &'a Messangers) -> Element<'a, Action> {
        let elements = match &self.server_selected {
            Some(server) => Column::from_iter(server.channels.iter().map(|chan| {
                match chan.chan_type {
                    ChanType::Spacer => Text::new(chan.name.as_str()).into(),
                    ChanType::Voice => Button::new(chan.name.as_str())
                        .on_press(Action::Call(chan.clone()))
                        .style(|_, _| button::Style {
                            background: Some(iced::Background::Color(Color::from_rgb(
                                0.0, 1.0, 0.2,
                            ))),
                            ..Default::default()
                        })
                        .into(),
                    ChanType::Text => Button::new(chan.name.as_str())
                        .on_press(Action::OpenChat {
                            handle: server.handle,
                            conversation: chan.to_owned(),
                        })
                        .width(Length::Fill)
                        .into(),
                    ChanType::TextAndVoice => Text::new(chan.name.as_str()).into(),
                }
            })),
            None => Column::from_iter(
                messengers
                    .data_iter()
                    .zip(messengers.interface_iter())
                    .flat_map(|(data, interface)| {
                        data.conversations.iter().map(|conversation| {
                            Button::new({
                                let image = match &conversation.icon {
                                    Some(icon) => image(icon),
                                    None => image(PLACEHOLDER_PFP),
                                };
                                row![
                                    container(image.height(Length::Fixed(28.0)))
                                        .padding(Padding::new(0.0).right(10.0)),
                                    conversation.name.as_str()
                                ]
                            })
                            .width(Length::Fill)
                            .on_press(Action::OpenChat {
                                handle: interface.handle,
                                conversation: conversation.to_owned(),
                            })
                            .into()
                        })
                    }),
            ),
        };

        let mut active_calls = messengers.data_iter().flat_map(|m| &m.calls).peekable();

        let panel = if active_calls.peek().is_some() {
            let active_calls = Column::from_iter(active_calls.map(|call| {
                Element::from(row![
                    Text::from(call.status_str()),
                    Button::new("D").on_press(Action::Disconect(call.clone()))
                ])
            }));
            Some(column![
                active_calls,
                Button::new("Mute"),
                Button::new("Deafen"),
            ])
        } else {
            None
        };

        column![
            Scrollable::new(column![
                Button::new("Contacts").on_press(Action::OpenContacts),
                elements,
            ])
            .height(Length::Fill),
        ]
        .push_maybe(panel)
        .width(self.width)
        .into()
    }
}
