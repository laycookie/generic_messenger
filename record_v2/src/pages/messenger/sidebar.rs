use iced::{
    Color, ContentFit, Element, Length, Padding,
    widget::{
        Button, Column, Scrollable, Text, button, column, container, image, row, slider,
        text::Wrapping,
    },
};
use iced_palace::widget::ellipsized_text;
use messenger_interface::types::{
    House, ID, Identifier, Place, Room, RoomCapabilities, User, VoiceState,
};
use messenger_unifier::{Call, MessengerData, MessengerId, MessengerRegistry};

use super::PLACEHOLDER_PFP;
use crate::audio_controls::{AfxTarget, AudioControls};
use crate::components::user_status::{known_user_status, one_to_one_room_status, status_indicator};

/// "M"/"D" badges next to a muted/deafened voice participant.
fn voice_state_badges<'a>(participant: &User) -> Vec<Element<'a, Action>> {
    let Some(state) = participant.voice else {
        return Vec::new();
    };
    [(state.muted, "M"), (state.deafened, "D")]
        .into_iter()
        .filter(|(active, _)| *active)
        .map(|(_, label)| {
            container(Text::new(label).size(10))
                .padding(Padding::new(0.0).left(4.0))
                .into()
        })
        .collect()
}

/// Green tint marking a voice channel/button as joinable.
fn call_button_style(_theme: &iced::Theme, _status: button::Status) -> button::Style {
    button::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.0, 1.0, 0.2))),
        ..Default::default()
    }
}

/// Red tint marking an own-state toggle (mute/deafen) as engaged.
fn engaged_toggle_style(_theme: &iced::Theme, _status: button::Status) -> button::Style {
    button::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.9, 0.2, 0.2))),
        ..Default::default()
    }
}

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
    /// Broadcast the client's own mute/deafen flags for an active call.
    SetCallState(Call, VoiceState),
    OpenContacts,
    OpenChat {
        id: MessengerId,
        conversation: Identifier<Place<Room>>,
    },
    /// Open the inspect sheet for the selected server/guild.
    InspectGuild {
        id: MessengerId,
        guild: Identifier<Place<House>>,
    },
    /// A participant volume slider moved.
    SetUserVolume {
        messenger: MessengerId,
        user: ID,
        volume: f32,
    },
    /// Open the afx sheet for a live audio channel.
    OpenAfx {
        target: AfxTarget,
        title: String,
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
        let data = messengers.data(server.messenger_id);
        let guild = data.and_then(|d| d.guilds.iter().find(|g| *g.id() == server.guild_id));

        let server_name = guild.map(|g| g.name.as_str()).unwrap_or("");
        let channels = guild.and_then(|g| g.rooms.as_deref()).unwrap_or(&[]);

        let header = container(Text::new(server_name).size(18))
            .padding(Padding::new(0.0).left(8.0).top(6.0).bottom(6.0));
        let header = match guild {
            Some(guild) => row![
                header,
                Button::new(Text::new("Info").size(12)).on_press(Action::InspectGuild {
                    id: server.messenger_id,
                    guild: guild.swap_data(Place::new(
                        guild.name.clone(),
                        guild.icon.clone(),
                        House::new(None),
                    )),
                }),
            ],
            None => row![header],
        };

        let rooms_list = Column::from_iter(channels.iter().map(move |chan| {
            let caps = chan.room_capabilities;

            if caps.is_empty() {
                return container(Text::new(chan.name.as_str()))
                    .padding(Padding::new(0.0).left(8.0).top(6.0).bottom(2.0))
                    .into();
            }

            let has_text = caps.contains(RoomCapabilities::Text);
            let has_voice = caps.contains(RoomCapabilities::Voice);

            // The channel name opens the text chat when the room supports text;
            // a voice-only channel joins its call directly from the name.
            let name_button = Button::new(chan.name.as_str())
                .width(Length::Fill)
                .on_press(if has_text {
                    Action::OpenChat {
                        id: server.messenger_id,
                        conversation: chan.to_owned(),
                    }
                } else {
                    Action::Call(chan.clone())
                });

            if !has_voice {
                return name_button.into();
            }

            // Voice-capable. Steam group voice rooms are BOTH text and voice, so
            // keep the name as the text entry and add a dedicated join button;
            // a voice-only (Discord-style) room's name already calls, so just tint it.
            let header = if has_text {
                row![
                    name_button,
                    Button::new("Call")
                        .on_press(Action::Call(chan.clone()))
                        .style(call_button_style),
                ]
            } else {
                row![name_button.style(call_button_style)]
            };

            let participants =
                Column::from_iter(chan.participants.as_deref().unwrap_or(&[]).iter().map(
                    |participant| {
                        let avatar = match participant.icon.as_ref() {
                            Some(icon) => image(icon),
                            None => image(PLACEHOLDER_PFP),
                        };

                        let mut identity = row![
                            container(
                                avatar
                                    .height(Length::Fixed(18.0))
                                    .width(Length::Fixed(18.0))
                                    .content_fit(ContentFit::Cover),
                            )
                            .padding(Padding::new(0.0).right(6.0).left(18.0)),
                        ]
                        .spacing(4.0)
                        .width(Length::Fill);
                        if let Some(status) = status_indicator(known_user_status(data, participant))
                        {
                            identity = identity.push(status);
                        }
                        Element::from(
                            identity
                                .push(
                                    ellipsized_text(participant.name.as_str())
                                        .wrapping(Wrapping::None),
                                )
                                .extend(voice_state_badges(participant)),
                        )
                    },
                ));

            column![header, participants].into()
        }));

        column![header, rooms_list]
    }

    fn view_dm_panel<'a>(messengers: &'a MessengerRegistry) -> Column<'a, Action> {
        column![Button::new("Contacts").on_press(Action::OpenContacts)].extend(
            messengers.iter().flat_map(|(_, entry)| {
                let id = entry.interface.id;
                entry.data.conversations.iter().map(move |conversation| {
                    let image = match &conversation.icon {
                        Some(icon) => image(icon),
                        None => image(PLACEHOLDER_PFP),
                    };
                    let mut label = row![
                        container(image.height(Length::Fixed(28.0)))
                            .padding(Padding::new(0.0).right(10.0)),
                    ]
                    .spacing(4.0)
                    .align_y(iced::Alignment::Center);
                    if let Some(status) =
                        status_indicator(one_to_one_room_status(&entry.data, conversation))
                    {
                        label = label.push(status);
                    }
                    label = label
                        .push(ellipsized_text(conversation.name.as_str()).wrapping(Wrapping::None));

                    Button::new(label)
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

    /// One row of the active-call roster: avatar + name + "+afx", with the
    /// participant's volume slider underneath.
    fn view_call_participant<'a>(
        messenger_id: MessengerId,
        data: &'a MessengerData,
        participant: &'a Identifier<User>,
        audio: &AudioControls,
    ) -> Element<'a, Action> {
        let user_id = *participant.id();
        let avatar = match participant.icon.as_ref() {
            Some(icon) => image(icon),
            None => image(PLACEHOLDER_PFP),
        };
        let volume = audio.user_volume(messenger_id, user_id);

        let identity = row![
            container(
                avatar
                    .height(Length::Fixed(24.0))
                    .width(Length::Fixed(24.0))
                    .content_fit(ContentFit::Cover),
            )
            .padding(Padding::new(0.0).right(4.0)),
        ]
        .extend(status_indicator(known_user_status(Some(data), participant)))
        .push(
            ellipsized_text(participant.name.as_str())
                .wrapping(Wrapping::None)
                .width(Length::Fill),
        )
        .extend(voice_state_badges(participant))
        .push(
            Button::new(Text::new("+afx").size(10)).on_press(Action::OpenAfx {
                target: AfxTarget::User {
                    messenger: messenger_id,
                    user: user_id,
                },
                title: participant.name.clone(),
            }),
        )
        .align_y(iced::Alignment::Center);

        let volume_row = row![
            slider(0.0..=2.0, volume, move |volume| Action::SetUserVolume {
                messenger: messenger_id,
                user: user_id,
                volume,
            })
            .step(0.01_f32)
            .width(Length::Fill),
            container(Text::new(format!("{:.0}%", volume * 100.0)).size(11))
                .padding(Padding::new(0.0).left(4.0)),
        ]
        .align_y(iced::Alignment::Center);

        column![identity, volume_row]
            .padding(Padding::new(0.0).bottom(6.0))
            .into()
    }

    pub fn view<'a>(
        &'a self,
        messengers: &'a MessengerRegistry,
        audio: &'a AudioControls,
    ) -> Element<'a, Action> {
        let room_list = match &self.server_selected {
            Some(server) => Self::view_server_panel(*server, messengers),
            None => Self::view_dm_panel(messengers),
        };

        // === New ===
        let active_calls_panel = Element::from(Column::from_iter(messengers.iter().map(
            |(messenger_id, messenger)| {
                let active_call_panel = messenger.data.calls.iter().map(move |call| {
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
                    let mic_afx_button = Element::from(
                        Button::new(Text::new("+afx").size(10)).on_press(Action::OpenAfx {
                            target: AfxTarget::Mic,
                            title: "Microphone".to_owned(),
                        }),
                    );
                    let disconnect_button =
                        Element::from(Button::new("X").on_press(Action::Disconnect(call.clone())));
                    let call_participants =
                        Element::from(Column::from_iter(call_participents.map(|participant| {
                            Self::view_call_participant(
                                messenger_id,
                                &messenger.data,
                                participant,
                                audio,
                            )
                        })));

                    // Own mute/deafen toggles, only for backends that can
                    // broadcast them. The shown state is the app's desired
                    // state (set optimistically on press, reconciled by the
                    // backend's echo) — deriving it from the roster instead
                    // would lag a round-trip, so a quick second press would
                    // compute its toggle from stale flags.
                    let can_set_state = messenger
                        .interface
                        .api
                        .voice()
                        .is_ok_and(|voice| voice.self_state().is_ok());
                    let self_state = VoiceState {
                        muted: audio.mic_muted(),
                        deafened: audio.deafened(),
                    };
                    let toggle = |label, engaged: bool, target: VoiceState| {
                        let button = Button::new(Text::new(label).size(10))
                            .on_press(Action::SetCallState(call.clone(), target));
                        if engaged {
                            button.style(engaged_toggle_style)
                        } else {
                            button
                        }
                    };

                    let header = row![call_status]
                        .extend(
                            can_set_state
                                .then(|| {
                                    [
                                        toggle(
                                            "M",
                                            self_state.muted,
                                            VoiceState {
                                                muted: !self_state.muted,
                                                ..self_state
                                            },
                                        ),
                                        toggle(
                                            "D",
                                            self_state.deafened,
                                            VoiceState {
                                                deafened: !self_state.deafened,
                                                ..self_state
                                            },
                                        ),
                                    ]
                                    .map(Element::from)
                                })
                                .into_iter()
                                .flatten(),
                        )
                        .push(mic_afx_button)
                        .push(disconnect_button);

                    Element::from(column![header, call_participants])
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
