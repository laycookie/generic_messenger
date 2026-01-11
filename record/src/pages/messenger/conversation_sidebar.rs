use iced::{
    Color, Element, Length, Padding,
    widget::{Button, Column, Scrollable, Text, button, column, container, image, row},
};
use messenger_interface::types::{Identifier, Room, RoomCapabilities};

use super::PLACEHOLDER_PFP;
use crate::messanger_unifier::{Call, MessangerHandle, Messangers};

#[derive(Debug, Clone)]
pub struct Server {
    pub handle: MessangerHandle,
    pub channels: Vec<Identifier<Room>>, // TODO(record-migration): server channels not yet supported
}

impl Server {
    pub fn new(handle: MessangerHandle, channels: Vec<Identifier<Room>>) -> Self {
        Self { handle, channels }
    }
}

#[derive(Debug)]
pub struct Sidebar {
    pub server_selected: Option<Server>,
    pub width: f32,
}

#[derive(Debug, Clone)]
pub enum Action {
    Call(Identifier<Room>),
    Disconnect(Call),
    OpenContacts,
    OpenChat {
        handle: crate::messanger_unifier::MessangerHandle,
        conversation: Identifier<Room>,
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
                // If the backend provides a "category/spacer" row, represent it as a room with
                // no capabilities (empty flags). Render it as a non-interactive label.
                if chan.room_capabilities.is_empty() {
                    return container(Text::new(chan.name.as_str()))
                        .padding(Padding::new(0.0).left(8.0).top(6.0).bottom(2.0))
                        .into();
                }

                if chan.room_capabilities.contains(RoomCapabilities::Voice)
                    && !chan.room_capabilities.contains(RoomCapabilities::Text)
                {
                    return Button::new(chan.name.as_str())
                        .on_press(Action::Call(chan.clone()))
                        .style(|_, _| button::Style {
                            background: Some(iced::Background::Color(Color::from_rgb(
                                0.0, 1.0, 0.2,
                            ))),
                            ..Default::default()
                        })
                        .into();
                }
                Button::new(chan.name.as_str())
                    .on_press(Action::OpenChat {
                        handle: server.handle,
                        conversation: chan.to_owned(),
                    })
                    .width(Length::Fill)
                    .into()
            })),
            None => Column::from_iter(messengers.data_iter().flat_map(|data| {
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
                        handle: data.handle(),
                        conversation: conversation.to_owned(),
                    })
                    .into()
                })
            })),
        };

        let mut active_calls = messengers.data_iter().flat_map(|m| &m.calls).peekable();

        let panel = if active_calls.peek().is_some() {
            let active_calls = Column::from_iter(active_calls.map(|call| {
                Element::from(row![
                    Text::from(call.status_str()),
                    Button::new("D").on_press(Action::Disconnect(call.clone()))
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
        .push(panel)
        .width(self.width)
        .into()
    }
}
