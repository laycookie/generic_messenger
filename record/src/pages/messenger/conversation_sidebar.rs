use adaptors::types::{Chan, ChanType, Identifier};
use iced::{
    Alignment, Color, Element, Length, Padding,
    advanced::{self, renderer},
    widget::{
        Button, Column, Scrollable, Text, button, column, container, image, row,
        scrollable::{self, Direction, Scrollbar},
        text,
    },
};

use super::PLACEHOLDER_PFP;
use crate::{messanger_unifier::Messangers, pages::messenger::server::Server};

#[derive(Debug)]
pub struct Sidebar {
    pub server: Option<Server>,
    pub width: f32,
}

#[derive(Debug, Clone)]
pub enum Action {
    OpenContacts,
    OpenChat {
        handle: crate::messanger_unifier::MessangerHandle,
        conversation: Identifier<Chan>,
    },
}

impl Sidebar {
    pub fn new(width: f32) -> Self {
        Self {
            server: None,
            width,
        }
    }

    pub fn get_bar<'a>(&'a self, messengers: &'a Messangers) -> Element<'a, Action> {
        let elements = match &self.server {
            Some(server) => Column::from_iter(server.channels.iter().map(|chan| {
                match chan.chan_type {
                    ChanType::Spacer => Text::new(chan.name.as_str()).into(),
                    ChanType::Voice => Button::new(chan.name.as_str())
                        // .on_press(Action::)
                        .style(|_, _| button::Style {
                            // TODO: TEMP
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
                    .flat_map(|(data, (m_handle, _))| {
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
                                handle: *m_handle,
                                conversation: conversation.to_owned(),
                            })
                            .into()
                        })
                    }),
            ),
        };

        Scrollable::new(elements).width(self.width).into()
    }
}
