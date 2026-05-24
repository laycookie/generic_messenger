use iced::{
    Color, ContentFit, Element, Length, Padding,
    widget::{
        Button, Column, Row, Scrollable, Text, button, column, container, image, row,
        text::Wrapping,
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

    fn view_server_panel<'a>(
        server: Server,
        messengers: &'a MessengerRegistry,
    ) -> Column<'a, Action> {
        let guild = messengers
            .data(server.messenger_id)
            .and_then(|d| d.guilds.iter().find(|g| *g.id() == server.guild_id));

        let server_name = guild.map(|g| g.name.as_str()).unwrap_or("");
        let channels = guild.and_then(|g| g.rooms.as_deref()).unwrap_or(&[]);

        let header = container(Text::new(server_name).size(18))
            .padding(Padding::new(0.0).left(8.0).top(6.0).bottom(6.0));

        let rooms_list = Column::from_iter(channels.iter().map(move |chan| {
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
                        background: Some(iced::Background::Color(Color::from_rgb(0.0, 1.0, 0.2))),
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
                    id: server.messenger_id,
                    conversation: chan.to_owned(),
                })
                .width(Length::Fill)
                .into()
        }));

        column![header, rooms_list]
    }

    fn view_dm_panel<'a>(messengers: &'a MessengerRegistry) -> Column<'a, Action> {
        column![Button::new("Contacts").on_press(Action::OpenContacts)].extend(
            messengers.iter().flat_map(|(_, entry)| {
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
                            ellipsized_text(conversation.name.as_str()).wrapping(Wrapping::None)
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
        )
    }

    pub fn view<'a>(&'a self, messengers: &'a MessengerRegistry) -> Element<'a, Action> {
        let room_list = match &self.server_selected {
            Some(server) => Self::view_server_panel(*server, messengers),
            None => Self::view_dm_panel(messengers),
        };

        // === New ===
        let active_calls_panel = Element::from(Column::from_iter(messengers.iter().map(
            |(_, messenger)| {
                let active_call_panel = messenger.data.calls.iter().map(|call| {
                    let client_user_id =
                        messenger.data.profile.as_ref().map(|profile| profile.id());

                    // Excluding client
                    let call_participents = call
                        .source()
                        .participants
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .filter(|participant| {
                            client_user_id
                                .is_none_or(|client_user_id| client_user_id != participant.id())
                        });

                    let call_status =
                        Element::from(Text::from(call.state_str()).width(Length::Fill));
                    let disconnect_button =
                        Element::from(Button::new("D").on_press(Action::Disconnect(call.clone())));
                    let call_participents_icons =
                        Element::from(Row::from_iter(call_participents.map(|participant| {
                            let pfp_image = match participant.icon.as_ref() {
                                Some(icon) => image(icon),
                                None => image(PLACEHOLDER_PFP),
                            };
                            Element::from(
                                container(
                                    pfp_image
                                        .height(Length::Fixed(24.0))
                                        .width(Length::Fixed(24.0))
                                        .content_fit(ContentFit::Cover),
                                )
                                .padding(Padding::new(0.0).right(4.0)),
                            )
                        })));

                    Element::from(column![
                        row![call_status, disconnect_button],
                        call_participents_icons,
                    ])
                });

                Element::from(Column::from_iter(active_call_panel))
            },
        )));

        column![Scrollable::new(room_list).height(Length::Fill)]
            .push(active_calls_panel)
            .width(self.width)
            .into()
    }
}
