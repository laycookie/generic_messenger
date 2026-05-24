use iced::{
    Color, ContentFit, Element, Length, Padding,
    widget::{
        Button, Column, Row, Scrollable, Text, button, column, container, image, row,
        text::Wrapping,
    },
};
use iced_palace::widget::ellipsized_text;
use messenger_interface::types::{Identifier, Place, Room, RoomCapabilities};

use super::PLACEHOLDER_PFP;
use crate::messenger_unifier::{Call, MessengerHandle, Messengers};

#[derive(Debug, Clone)]
pub struct Server {
    pub handle: MessengerHandle,
    pub channels: Vec<Identifier<Place<Room>>>,
}
impl Server {
    pub fn new(handle: MessengerHandle, channels: Vec<Identifier<Place<Room>>>) -> Self {
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
    Call(Identifier<Place<Room>>),
    Disconnect(Call),
    OpenContacts,
    OpenChat {
        handle: crate::messenger_unifier::MessengerHandle,
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

    pub fn view<'a>(&'a self, messengers: &'a Messengers) -> Element<'a, Action> {
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
                    let channel_button = Button::new(chan.name.as_str())
                        .on_press(Action::Call(chan.clone()))
                        .width(Length::Fill)
                        .style(|_, _| button::Style {
                            background: Some(iced::Background::Color(Color::from_rgb(
                                0.0, 1.0, 0.2,
                            ))),
                            ..Default::default()
                        });

                    let participants =
                        Column::from_iter(chan.participants.as_deref().unwrap_or(&[]).iter().map(
                            |participant| {
                                let avatar = match participant.icon.as_ref() {
                                    Some(icon) => image(icon),
                                    None => image(PLACEHOLDER_PFP),
                                };

                                Element::from(
                                    row![
                                        container(
                                            avatar
                                                .height(Length::Fixed(18.0))
                                                .width(Length::Fixed(18.0))
                                                .content_fit(ContentFit::Cover),
                                        )
                                        .padding(Padding::new(0.0).right(6.0).left(18.0)),
                                        ellipsized_text(participant.name.as_str())
                                            .wrapping(Wrapping::None),
                                    ]
                                    .width(Length::Fill),
                                )
                            },
                        ));

                    return column![channel_button, participants].into();
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
                            ellipsized_text(conversation.name.as_str()).wrapping(Wrapping::None)
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

        let mut active_calls = messengers
            .data_iter()
            .flat_map(|data| data.calls.iter().map(move |call| (call, data)))
            .peekable();

        let panel = if active_calls.peek().is_some() {
            let active_calls = Column::from_iter(active_calls.map(|(call, data)| {
                let participant_icons = Row::from_iter(
                    call.source()
                        .participants
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .filter(|participant| {
                            data.profile
                                .as_ref()
                                .is_none_or(|profile| profile.id() != participant.id())
                        })
                        .map(|participant| {
                            let avatar = match participant.icon.as_ref() {
                                Some(icon) => image(icon),
                                None => image(PLACEHOLDER_PFP),
                            };

                            Element::from(
                                container(
                                    avatar
                                        .height(Length::Fixed(24.0))
                                        .width(Length::Fixed(24.0))
                                        .content_fit(ContentFit::Cover),
                                )
                                .padding(Padding::new(0.0).right(4.0)),
                            )
                        }),
                );

                Element::from(column![
                    row![
                        Text::from(call.state_str()).width(Length::Fill),
                        Button::new("D").on_press(Action::Disconnect(call.clone()))
                    ],
                    participant_icons,
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
