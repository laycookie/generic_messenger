use std::{error::Error, path::PathBuf};

use crate::audio_controls::{AfxKind, AfxTarget, AudioControls, AudioOp};
use crate::components::divider::Divider;
use messenger_unifier::{
    Call, MessengerData, MessengerId, MessengerInterface, MessengerRegistry, PendingSend,
};
use simple_audio_channels::{
    effects_rack::SlotKey,
    params::{ParamKey, ParamSpec, ParamUnit},
};

use self::{
    chat::{Action as ChatAction, Chat, UpdateResult as ChatUpdateResult},
    contacts::{Action as ContactsAction, Contacts},
    navbar::{Action as NavbarAction, Navbar},
    sidebar::{Action as SidebarAction, Server, Sidebar},
};

use iced::{
    Alignment, Color, ContentFit, Element, Length, Padding, Task,
    widget::{
        Button, Column, Responsive, Row, Text, center, column, container, image, mouse_area,
        opaque, row, slider, stack,
    },
};
use messenger_interface::types::{
    Details, House, ID, Identifier, Message as InterfaceMessage, Place, Revision, Room, User,
    VoiceState,
};
use tracing::{error, warn};

mod chat;
mod contacts;
mod navbar;
mod sidebar;

pub(super) const PLACEHOLDER_PFP: &str = "./public/imgs/placeholder.jpg";

#[derive(Clone)]
pub(crate) enum Main {
    Contacts(Contacts),
    Chat(Chat),
}

pub struct Messenger {
    sidebar: Sidebar,
    main: Main,
    pending_counter: ID,
    /// Inspect sheet shown as a modal over the page, if any.
    inspect: Option<Inspect>,
    /// Afx sheet (a live channel's effect chain) shown as a modal, if any.
    afx: Option<AfxSheet>,
}

impl Messenger {
    pub(crate) fn new() -> Self {
        Messenger {
            sidebar: Sidebar::new(168.0),
            main: Main::Contacts(Contacts::default()),
            pending_counter: ID::MAX,
            inspect: None,
            afx: None,
        }
    }

    fn next_pending_id(&mut self) -> ID {
        let id = self.pending_counter;
        self.pending_counter = self.pending_counter.wrapping_sub(1);
        id
    }
}

/// The inspect modal: an entity's name/icon plus backend detail fields.
#[derive(Clone)]
struct Inspect {
    title: String,
    icon: Option<PathBuf>,
    state: InspectState,
}

#[derive(Clone)]
enum InspectState {
    Loading,
    Loaded(Details),
    Failed(String),
}

impl Inspect {
    fn view(&self) -> Element<'_, Message> {
        let mut header = Row::new().spacing(10.0).align_y(Alignment::Center);
        if let Some(icon) = &self.icon {
            header = header.push(
                image(icon)
                    .height(Length::Fixed(48.0))
                    .width(Length::Fixed(48.0))
                    .content_fit(ContentFit::Cover),
            );
        }
        header = header.push(Text::new(self.title.as_str()).size(22));

        let body: Element<'_, Message> = match &self.state {
            InspectState::Loading => Text::new("Loading...").into(),
            InspectState::Failed(err) => Text::new(err.as_str())
                .color(Color::from_rgb(0.8, 0.2, 0.2))
                .into(),
            InspectState::Loaded(details) if details.fields.is_empty() => {
                Text::new("No details available").into()
            }
            InspectState::Loaded(details) => {
                Column::from_iter(details.fields.iter().map(|(label, value)| {
                    row![
                        Text::new(label.as_str())
                            .size(13)
                            .width(Length::Fixed(110.0)),
                        Text::new(value.as_str()),
                    ]
                    .spacing(8.0)
                    .into()
                }))
                .spacing(6.0)
                .into()
            }
        };

        container(
            column![
                header,
                body,
                Button::new("Close").on_press(Message::CloseInspect)
            ]
            .spacing(14.0),
        )
        .padding(18.0)
        .width(Length::Fixed(380.0))
        .style(container::rounded_box)
        .into()
    }
}

/// The afx modal: a live channel's effect chain — listed with per-effect
/// remove and one generic control per parameter the effect declares
/// (rendered from its [`ParamSpec`] table), plus the effects that can be added.
struct AfxSheet {
    target: AfxTarget,
    /// Whose channel this is, for the header (a participant's name, or "Microphone").
    title: String,
}

/// Display a parameter value the way its spec says it should be read.
fn format_param(spec: &ParamSpec, value: f32) -> String {
    if spec.min_is_off && value <= spec.min {
        return "off".to_owned();
    }
    match spec.unit {
        ParamUnit::Amplitude => format!("{:.0}%", value * 100.0),
        ParamUnit::Decibels => format!("{value:.1} dB"),
        ParamUnit::Milliseconds => format!("{value:.0} ms"),
        ParamUnit::Hertz => format!("{value:.0} Hz"),
    }
}

impl AfxSheet {
    fn view(&self, audio: &AudioControls) -> Element<'_, Message> {
        let header = Text::new(format!("Audio effects — {}", self.title)).size(20);

        let body: Element<'_, Message> = match audio.effects(self.target) {
            // Racks only exist while their channel does (during a call).
            None => Text::new("No live audio channel to edit.").into(),
            Some(effects) => {
                let listing: Element<'_, Message> = if effects.is_empty() {
                    Text::new("No effects").size(13).into()
                } else {
                    Column::from_iter(effects.iter().map(|afx| {
                        let effect = afx.id;
                        let title = row![
                            Text::new(afx.name).width(Length::Fill),
                            Button::new(Text::new("x").size(11))
                                .on_press(Message::AfxRemove(effect)),
                        ]
                        .align_y(Alignment::Center);

                        let knobs = Column::from_iter(afx.params().map(|state| {
                            let (spec, param) = (state.spec, state.spec.key);
                            row![
                                Text::new(spec.name).size(11).width(Length::Fixed(72.0)),
                                slider(spec.min..=spec.max, state.value, move |value| {
                                    Message::AfxSetParam {
                                        effect,
                                        param,
                                        value,
                                    }
                                })
                                .step((spec.max - spec.min) / 200.0)
                                .width(Length::Fill),
                                container(Text::new(format_param(spec, state.value)).size(11))
                                    .width(Length::Fixed(56.0))
                                    .padding(Padding::new(0.0).left(6.0)),
                            ]
                            .align_y(Alignment::Center)
                            .into()
                        }))
                        .spacing(2.0);

                        column![title, knobs].spacing(2.0).into()
                    }))
                    .spacing(10.0)
                    .into()
                };

                let add_row = Row::from_iter(
                    std::iter::once(Element::from(Text::new("Add:").size(13))).chain(
                        AfxKind::ALL.iter().map(|kind| {
                            Button::new(Text::new(format!("+ {}", kind.name())).size(13))
                                .on_press(Message::AfxAdd(*kind))
                                .into()
                        }),
                    ),
                )
                .spacing(8.0)
                .align_y(Alignment::Center);

                column![listing, add_row].spacing(12.0).into()
            }
        };

        container(
            column![
                header,
                body,
                Button::new("Close").on_press(Message::CloseAfx)
            ]
            .spacing(14.0),
        )
        .padding(18.0)
        .width(Length::Fixed(400.0))
        .style(container::rounded_box)
        .into()
    }
}

#[derive(Clone)]
pub(crate) enum Message {
    Chat(ChatAction),
    Contacts(ContactsAction),
    Navbar(NavbarAction),
    Sidebar(SidebarAction),
    // ===
    ChangeMain(Main),
    SetSidebarServer(Option<Server>),
    DividerChange(f32),
    GuildRoomsLoaded {
        id: MessengerId,
        guild_id: ID,
        rooms: Vec<Identifier<Place<Room>>>,
    },
    UpdateChat {
        id: MessengerId,
        kv: (ID, Vec<Identifier<InterfaceMessage>>),
    },
    /// A message the user just sent, shown optimistically before server confirmation.
    #[allow(clippy::enum_variant_names)]
    PendingMessage {
        id: MessengerId,
        room_id: ID,
        message: Identifier<InterfaceMessage>,
    },
    /// A pending message was confirmed by the server — replace the temp entry.
    ConfirmPending {
        id: MessengerId,
        room_id: ID,
        pending_id: ID,
        confirmed: Identifier<InterfaceMessage>,
    },
    /// A pending message failed to send — remove it.
    RemovePending {
        id: MessengerId,
        room_id: ID,
        pending_id: ID,
    },
    /// The inspect sheet fetch resolved.
    InspectLoaded(Result<Details, String>),
    CloseInspect,
    /// Add the picked effect to the open afx sheet's channel.
    AfxAdd(AfxKind),
    /// Remove an effect from the open afx sheet's channel.
    AfxRemove(SlotKey),
    /// A parameter slider in the afx sheet moved.
    AfxSetParam {
        effect: SlotKey,
        param: ParamKey,
        value: f32,
    },
    CloseAfx,
    /// An edit was confirmed by the server; replace the stored message.
    ConfirmEdit {
        id: MessengerId,
        room_id: ID,
        updated: Identifier<InterfaceMessage>,
    },
    /// A delete was confirmed by the server; drop the stored message.
    ConfirmDelete {
        id: MessengerId,
        room_id: ID,
        message_id: ID,
    },
}

pub enum Action {
    None,
    Run(Task<Message>),
    ModifyMessengerData {
        id: MessengerId,
        modify: Box<dyn FnOnce(&mut MessengerData) + Send>,
    },
    Call {
        interface: MessengerInterface,
        channel: Identifier<Place<Room>>,
    },
    DisconnectFromCall(Call),
    /// Broadcast the client's own mute/deafen flags for an active call.
    SetCallState {
        call: Call,
        state: VoiceState,
    },
    /// An audio edit for the app-owned `AudioControls` (volume, afx chain).
    Audio(AudioOp),
}

impl Messenger {
    /// Open the inspect modal for `user` and fetch their detail sheet.
    fn start_inspect_user(
        &mut self,
        interface: MessengerInterface,
        user: Identifier<User>,
    ) -> Action {
        self.inspect = Some(Inspect {
            title: user.name.clone(),
            icon: user.icon.clone(),
            state: InspectState::Loading,
        });
        Action::Run(
            Task::future(async move {
                let query = interface.query()?;
                query.inspect_user(&user).await
            })
            .then(|result: Result<Details, Box<dyn Error + Send + Sync>>| {
                Task::done(Message::InspectLoaded(result.map_err(|e| e.to_string())))
            }),
        )
    }

    /// Open the inspect modal for `house` and fetch its detail sheet.
    fn start_inspect_house(
        &mut self,
        interface: MessengerInterface,
        house: Identifier<Place<House>>,
    ) -> Action {
        self.inspect = Some(Inspect {
            title: house.name.clone(),
            icon: house.icon.clone(),
            state: InspectState::Loading,
        });
        Action::Run(
            Task::future(async move {
                let query = interface.query()?;
                query.inspect_house(&house).await
            })
            .then(|result: Result<Details, Box<dyn Error + Send + Sync>>| {
                Task::done(Message::InspectLoaded(result.map_err(|e| e.to_string())))
            }),
        )
    }

    pub(crate) fn update(&mut self, message: Message, messengers: &MessengerRegistry) -> Action {
        match message {
            Message::SetSidebarServer(server) => {
                self.sidebar.server_selected = server;
                Action::None
            }
            Message::ChangeMain(main) => {
                self.main = main;
                Action::None
            }
            Message::DividerChange(val) => {
                if (self.sidebar.width + val > 300.0) | (self.sidebar.width + val < 100.0) {
                    return Action::None;
                }
                self.sidebar.width += val;
                Action::None
            }
            Message::UpdateChat { id, kv } => Action::ModifyMessengerData {
                id,
                modify: Box::new(move |data| {
                    if let Some(room) = data.room_mut(kv.0) {
                        room.messages = Some(kv.1);
                    }
                }),
            },
            Message::GuildRoomsLoaded {
                id,
                guild_id,
                rooms,
            } => {
                self.sidebar.server_selected = Some(Server::new(id, guild_id));
                Action::ModifyMessengerData {
                    id,
                    modify: Box::new(move |data| {
                        if let Some(guild) = data.guilds.iter_mut().find(|g| *g.id() == guild_id) {
                            guild.rooms = Some(rooms);
                        }
                    }),
                }
            }
            Message::PendingMessage {
                id,
                room_id,
                message,
            } => {
                let pending_id = *message.id();
                Action::ModifyMessengerData {
                    id,
                    modify: Box::new(move |data| {
                        data.pending_sends.push(PendingSend {
                            pending_id,
                            room_id,
                        });
                        if let Some(room) = data.room_mut(room_id) {
                            room.messages.get_or_insert_with(Vec::new).push(message);
                        }
                        data.move_conversation_to_front(room_id);
                    }),
                }
            }
            Message::ConfirmPending {
                id,
                room_id,
                pending_id,
                confirmed,
            } => Action::ModifyMessengerData {
                id,
                modify: Box::new(move |data| {
                    data.pending_sends.retain(|p| p.pending_id != pending_id);

                    if let Some(room) = data.room_mut(room_id)
                        && let Some(msgs) = room.messages.as_mut()
                        && let Some(pending) = msgs.iter_mut().find(|m| *m.id() == pending_id)
                    {
                        // Pending message still exists — replace in-place with confirmed
                        *pending = confirmed;
                    }
                    // If pending_id wasn't found, the socket already replaced it — nothing to do
                }),
            },
            Message::RemovePending {
                id,
                room_id,
                pending_id,
            } => Action::ModifyMessengerData {
                id,
                modify: Box::new(move |data| {
                    data.pending_sends.retain(|p| p.pending_id != pending_id);
                    if let Some(room) = data.room_mut(room_id)
                        && let Some(msgs) = room.messages.as_mut()
                    {
                        msgs.retain(|m| *m.id() != pending_id);
                    }
                }),
            },
            Message::InspectLoaded(result) => {
                if let Some(inspect) = &mut self.inspect {
                    inspect.state = match result {
                        Ok(details) => InspectState::Loaded(details),
                        Err(err) => InspectState::Failed(err),
                    };
                }
                Action::None
            }
            Message::CloseInspect => {
                self.inspect = None;
                Action::None
            }
            Message::AfxAdd(kind) => match &self.afx {
                Some(sheet) => Action::Audio(AudioOp::AddEffect {
                    target: sheet.target,
                    kind,
                }),
                None => Action::None,
            },
            Message::AfxRemove(id) => match &self.afx {
                Some(sheet) => Action::Audio(AudioOp::RemoveEffect {
                    target: sheet.target,
                    id,
                }),
                None => Action::None,
            },
            Message::AfxSetParam {
                effect,
                param,
                value,
            } => match &self.afx {
                Some(sheet) => Action::Audio(AudioOp::SetEffectParam {
                    target: sheet.target,
                    effect,
                    param,
                    value,
                }),
                None => Action::None,
            },
            Message::CloseAfx => {
                self.afx = None;
                Action::None
            }
            Message::ConfirmEdit {
                id,
                room_id,
                updated,
            } => Action::ModifyMessengerData {
                id,
                modify: Box::new(move |data| {
                    if let Some(room) = data.room_mut(room_id)
                        && let Some(msgs) = room.messages.as_mut()
                        && let Some(existing) = msgs.iter_mut().find(|m| m.id() == updated.id())
                    {
                        *existing = updated;
                    }
                }),
            },
            Message::ConfirmDelete {
                id,
                room_id,
                message_id,
            } => Action::ModifyMessengerData {
                id,
                modify: Box::new(move |data| {
                    if let Some(room) = data.room_mut(room_id)
                        && let Some(msgs) = room.messages.as_mut()
                    {
                        msgs.retain(|m| *m.id() != message_id);
                    }
                }),
            },
            Message::Chat(msg) => {
                if let Main::Chat(chat) = &mut self.main {
                    return match msg {
                        ChatAction::Call { interface, room } => {
                            let Some(interface) = messengers.interface(interface.id) else {
                                return Action::None;
                            };
                            Action::Call {
                                interface: interface.clone(),
                                channel: room,
                            }
                        }
                        ChatAction::Message(message) => match chat.update(message) {
                            ChatUpdateResult::Task(task) => {
                                Action::Run(task.map(|m| Message::Chat(ChatAction::Message(m))))
                            }
                            ChatUpdateResult::Send {
                                interface,
                                room,
                                contents,
                            } => {
                                let id = interface.id;
                                let room_id = *room.id();
                                let pending_id = self.next_pending_id();
                                let author =
                                    messengers.data(id).and_then(|data| data.profile.clone());

                                // TODO: Verify if we actually need to create an InterfaceMessage here,
                                // as we are already in the UI and all data in here is in the unified format.
                                let pending_msg = Identifier::new(
                                    pending_id,
                                    InterfaceMessage {
                                        content: Revision {
                                            at: None,
                                            text: contents.clone(),
                                        },
                                        history: Vec::new(),
                                        reactions: Vec::new(),
                                        author,
                                    },
                                );

                                Action::Run(Task::batch([
                                    // Add pending message to the chat store immediately
                                    Task::done(Message::PendingMessage {
                                        id,
                                        room_id,
                                        message: pending_msg,
                                    }),
                                    // Fire the async send and handle result
                                    Task::future({
                                        let api = interface.api.clone();
                                        async move {
                                            let text = api.text().map_err(|e| {
                                                Box::new(e) as Box<dyn Error + Send + Sync>
                                            })?;
                                            let confirmed = text
                                                .send_message(
                                                    &room,
                                                    InterfaceMessage {
                                                        content: Revision {
                                                            at: None,
                                                            text: contents,
                                                        },
                                                        history: Vec::new(),
                                                        reactions: Vec::new(),
                                                        author: None,
                                                    },
                                                )
                                                .await?;
                                            Ok(confirmed)
                                        }
                                    })
                                    .then(
                                        move |result: Result<_, Box<dyn Error + Send + Sync>>| {
                                            match result {
                                                Ok(confirmed) => {
                                                    Task::done(Message::ConfirmPending {
                                                        id,
                                                        room_id,
                                                        pending_id,
                                                        confirmed,
                                                    })
                                                }
                                                Err(e) => {
                                                    error!("Failed to send message: {e:#?}");
                                                    Task::done(Message::RemovePending {
                                                        id,
                                                        room_id,
                                                        pending_id,
                                                    })
                                                }
                                            }
                                        },
                                    ),
                                ]))
                            }
                            ChatUpdateResult::Edit {
                                interface,
                                room,
                                message_id,
                                contents,
                            } => {
                                let id = interface.id;
                                let room_id = *room.id();
                                let Some(edit) = interface.edit else {
                                    error!("Edit not supported by this messenger");
                                    return Action::None;
                                };
                                Action::Run(
                                    Task::future(async move {
                                        edit.edit_message(
                                            &room,
                                            &Identifier::new(
                                                message_id,
                                                InterfaceMessage::default(),
                                            ),
                                            InterfaceMessage {
                                                content: Revision {
                                                    at: None,
                                                    text: contents,
                                                },
                                                ..Default::default()
                                            },
                                        )
                                        .await
                                    })
                                    .then(
                                        move |result: Result<_, Box<dyn Error + Send + Sync>>| {
                                            match result {
                                                Ok(updated) => Task::done(Message::ConfirmEdit {
                                                    id,
                                                    room_id,
                                                    updated,
                                                }),
                                                Err(e) => {
                                                    error!("Failed to edit message: {e:#?}");
                                                    Task::none()
                                                }
                                            }
                                        },
                                    ),
                                )
                            }
                            ChatUpdateResult::Delete {
                                interface,
                                room,
                                message_id,
                            } => {
                                let id = interface.id;
                                let room_id = *room.id();
                                let Some(delete) = interface.delete else {
                                    error!("Delete not supported by this messenger");
                                    return Action::None;
                                };
                                Action::Run(
                                    Task::future(async move {
                                        delete
                                            .delete_message(
                                                &room,
                                                &Identifier::new(
                                                    message_id,
                                                    InterfaceMessage::default(),
                                                ),
                                            )
                                            .await
                                    })
                                    .then(
                                        move |result: Result<(), Box<dyn Error + Send + Sync>>| {
                                            match result {
                                                Ok(()) => Task::done(Message::ConfirmDelete {
                                                    id,
                                                    room_id,
                                                    message_id,
                                                }),
                                                Err(e) => {
                                                    error!("Failed to delete message: {e:#?}");
                                                    Task::none()
                                                }
                                            }
                                        },
                                    ),
                                )
                            }
                            ChatUpdateResult::InspectUser { interface, user } => {
                                self.start_inspect_user(interface, user)
                            }
                            ChatUpdateResult::ToggleReaction {
                                interface,
                                room,
                                message_id,
                                emoji,
                                reacted,
                            } => {
                                let Some(reactions) = interface.reactions else {
                                    error!("Reactions not supported by this messenger");
                                    return Action::None;
                                };
                                Action::Run(
                                    Task::future(async move {
                                        let msg_ident = Identifier::new(
                                            message_id,
                                            InterfaceMessage::default(),
                                        );
                                        let result = if reacted {
                                            reactions
                                                .remove_reaction(&room, &msg_ident, &emoji)
                                                .await
                                        } else {
                                            reactions.add_reaction(&room, &msg_ident, &emoji).await
                                        };
                                        if let Err(e) = result {
                                            error!("Failed to toggle reaction: {e:#?}");
                                        }
                                    })
                                    .then(|_| Task::none()),
                                )
                            }
                        },
                    };
                }
                Action::None
            }
            Message::Contacts(msg) => match msg {
                ContactsAction::OpenDm { id, user } => {
                    let conversation = messengers.data(id).and_then(|data| {
                        data.conversations.iter().find(|room| {
                            room.members
                                .as_deref()
                                .is_some_and(|members| matches!(members, [m] if m == &user))
                        })
                    });
                    let Some(conversation) = conversation else {
                        warn!("No existing DM conversation with {}", user.name);
                        return Action::None;
                    };

                    let conversation = conversation.clone();
                    self.update(
                        Message::Sidebar(SidebarAction::OpenChat { id, conversation }),
                        messengers,
                    )
                }
                msg => {
                    if let Main::Contacts(contacts) = &mut self.main {
                        return Action::Run(contacts.update(msg).map(Message::Contacts));
                    }
                    Action::None
                }
            },
            Message::Navbar(action) => match action {
                NavbarAction::GetDMs => Action::Run(Task::done(Message::SetSidebarServer(None))),
                NavbarAction::GetGuild { id, server } => {
                    let Some(interface) = messengers.interface(id) else {
                        return Action::None;
                    };

                    let guild_id = *server.id();

                    // Check if rooms are already loaded in the registry
                    let rooms_loaded = messengers
                        .data(id)
                        .and_then(|d| d.guilds.iter().find(|g| *g.id() == guild_id))
                        .and_then(|g| g.rooms.as_ref())
                        .is_some();

                    if rooms_loaded {
                        return Action::Run(Task::done(Message::SetSidebarServer(Some(
                            Server::new(id, guild_id),
                        ))));
                    }

                    // Fetch rooms and store in registry
                    let interface = interface.clone();
                    Action::Run(
                        Task::future({
                            let interface = interface.clone();
                            let server = server.clone();
                            async move {
                                let query = interface.query()?;
                                let house = query.house_details(server).await?;
                                Ok((interface.id, guild_id, house.rooms))
                            }
                        })
                        .then(
                            |result: Result<_, Box<dyn Error + Send + Sync>>| match result {
                                Ok((id, guild_id, rooms)) => {
                                    Task::done(Message::GuildRoomsLoaded {
                                        id,
                                        guild_id,
                                        rooms: rooms.unwrap_or_default(),
                                    })
                                }
                                Err(e) => {
                                    error!("{e:#?}");
                                    Task::none()
                                }
                            },
                        ),
                    )
                }
            },
            Message::Sidebar(action) => match action {
                SidebarAction::Disconnect(call) => Action::DisconnectFromCall(call),
                SidebarAction::SetCallState(call, state) => Action::SetCallState { call, state },
                SidebarAction::SetUserVolume {
                    messenger,
                    user,
                    volume,
                } => Action::Audio(AudioOp::SetUserVolume {
                    messenger,
                    user,
                    volume,
                }),
                SidebarAction::OpenAfx { target, title } => {
                    self.afx = Some(AfxSheet { target, title });
                    Action::None
                }
                SidebarAction::Call(channel) => {
                    let server = self.sidebar.server_selected.as_ref().unwrap();
                    let Some(interface) = messengers.interface(server.messenger_id) else {
                        return Action::None;
                    };

                    Action::Call {
                        interface: interface.clone(),
                        channel,
                    }
                }
                SidebarAction::OpenContacts => {
                    self.main = Main::Contacts(Contacts::default());
                    Action::None
                }
                SidebarAction::InspectGuild { id, guild } => {
                    let Some(interface) = messengers.interface(id) else {
                        return Action::None;
                    };
                    self.start_inspect_house(interface.clone(), guild)
                }
                SidebarAction::OpenChat { id, conversation } => {
                    let Some(interface) = messengers.interface(id) else {
                        return Action::None;
                    };

                    // Check cache
                    if let Some(data) = messengers.data(id)
                        && data
                            .room(*conversation.id())
                            .is_some_and(|room| room.messages.is_some())
                    {
                        return Action::Run(Task::done(Message::ChangeMain(Main::Chat(
                            Chat::new(interface.clone(), conversation),
                        ))));
                    }

                    // Otherwise fetch
                    Action::Run(Task::batch([
                        Task::done(Message::ChangeMain(Main::Chat(Chat::new(
                            interface.clone(),
                            conversation.clone(),
                        )))),
                        Task::future({
                            let interface = interface.clone();
                            async move {
                                let text = interface
                                    .api
                                    .text()
                                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                                let msgs = text
                                    .get_messages(
                                        &conversation,
                                        None,
                                        messenger_interface::interface::Ordering::Time,
                                    )
                                    .await?;
                                let id = interface.id;

                                Ok((id, conversation, msgs))
                            }
                        })
                        .then(
                            |t: Result<_, Box<dyn Error + Send + Sync>>| match t {
                                Ok((id, conversation, msgs)) => Task::done(Message::UpdateChat {
                                    id,
                                    kv: (*conversation.id(), msgs),
                                }),
                                Err(err) => {
                                    error!("{err}");
                                    Task::none()
                                }
                            },
                        ),
                    ]))
                }
            },
        }
    }

    pub(crate) fn view<'a>(
        &'a self,
        messengers: &'a MessengerRegistry,
        audio: &'a AudioControls,
    ) -> iced::Element<'a, Message> {
        let profiles = row![Text::from(
            messengers
                .iter()
                .next()
                .map(|(_, entry)| {
                    entry
                        .data
                        .profile
                        .as_ref()
                        .map(|p| p.name.as_str())
                        .unwrap_or("No connection made?")
                })
                .unwrap_or("No messengers connected")
        )];

        let navbar = Navbar::get_element(messengers).map(Message::Navbar);

        let window = Responsive::new(move |size| {
            let sidebar = self.sidebar.view(messengers, audio).map(Message::Sidebar);

            let main = match &self.main {
                Main::Contacts(contacts) => contacts.get_element(messengers).map(Message::Contacts),
                Main::Chat(chat) => chat.get_element(messengers).map(Message::Chat),
            };
            row![
                sidebar,
                Divider::new(10.0, size.height, Message::DividerChange),
                main
            ]
            .into()
        });

        let base = Element::from(column![profiles, row![navbar, window]]);

        // Modals: dimmed backdrop closes on click, the card itself swallows
        // clicks. Inspect wins if both are somehow open.
        let (card, close): (Element<'a, Message>, Message) = if let Some(inspect) = &self.inspect {
            (inspect.view(), Message::CloseInspect)
        } else if let Some(afx) = &self.afx {
            (afx.view(audio), Message::CloseAfx)
        } else {
            return base;
        };
        stack![
            base,
            opaque(
                mouse_area(center(opaque(card)).style(|_theme| {
                    container::Style {
                        background: Some(iced::Background::Color(Color {
                            a: 0.6,
                            ..Color::BLACK
                        })),
                        ..Default::default()
                    }
                }))
                .on_press(close)
            )
        ]
        .into()
    }
}
