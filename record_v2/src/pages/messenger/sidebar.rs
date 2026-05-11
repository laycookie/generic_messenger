use iced::{
    Color, Element, Length, Padding,
    widget::{
        Button, Column, Scrollable, Text, button, column, container, image, row, text::Wrapping,
    },
};
use iced_palace::widget::ellipsized_text;
use messenger_interface::types::{ID, Identifier, Place, Room, RoomCapabilities};

use super::PLACEHOLDER_PFP;
use crate::state::{Call, MessengerId, MessengerRegistry};

#[derive(Debug, Clone, Copy)]
pub struct Server {
    pub messenger_id: MessengerId,
    pub guild_id: ID,
}
impl Server {
    pub fn new(messenger_id: MessengerId, guild_id: ID) -> Self {
        Self {
            messenger_id,
            guild_id,
        }
    }
}

#[derive(Debug)]
pub struct Sidebar {
    pub server_selected: Option<Server>,
    pub width: f32,
}

#[derive(Debug, Clone)]
pub enum Action {
    Call(Identifier<Place<Room>>),
    Disconnect(Call),
    OpenContacts,
    OpenChat {
        id: MessengerId,
        conversation: Identifier<Place<Room>>,
    },
}

impl Sidebar {
    pub fn new(width: f32) -> Self {
        Self {
            server_selected: None,
            width,
        }
    }

    pub fn view<'a>(&'a self, messengers: &'a MessengerRegistry) -> Element<'a, Action> {
        let elements = match &self.server_selected {
            Some(server) => {
                let channels = messengers
                    .data(server.messenger_id)
                    .and_then(|d| d.guilds.iter().find(|g| *g.id() == server.guild_id))
                    .and_then(|g| g.rooms.as_deref())
                    .unwrap_or(&[]);

                Column::from_iter(channels.iter().map(|chan| {
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
                            id: server.messenger_id,
                            conversation: chan.to_owned(),
                        })
                        .width(Length::Fill)
                        .into()
                }))
            }
            None => Column::from_iter(
                messengers
                    .iter()
                    .flat_map(|(_, entry)| {
                        let id = entry.interface.id;
                        entry.data.conversations.iter().map(move |conversation| {
                            Button::new({
                                let image = match &conversation.icon {
                                    Some(icon) => image(icon),
                                    None => image(PLACEHOLDER_PFP),
                                };
                                row![
                                    container(image.height(Length::Fixed(28.0)))
                                        .padding(Padding::new(0.0).right(10.0)),
                                    ellipsized_text(conversation.name.as_str())
                                        .wrapping(Wrapping::None)
                                ]
                            })
                            .width(Length::Fill)
                            .on_press(Action::OpenChat {
                                id,
                                conversation: conversation.to_owned(),
                            })
                            .into()
                        })
                    }),
            ),
        };

        let mut active_calls = messengers
            .iter()
            .flat_map(|(_, entry)| &entry.data.calls)
            .peekable();

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
