use std::error::Error;

use crate::{
    components::divider::Divider,
    state::{Call, MessengerData, MessengerId, MessengerInterface, MessengerRegistry, PendingSend},
};

use self::{
    chat::{Action as ChatAction, Chat, UpdateResult as ChatUpdateResult},
    contacts::{Action as ContactsAction, Contacts},
    navbar::{Action as NavbarAction, Navbar},
    sidebar::{Action as SidebarAction, Server, Sidebar},
};

use iced::{
    Task,
    widget::{Responsive, Text, column, row},
};
use messenger_interface::types::{
    ID, Identifier, Message as InterfaceMessage, Place, Revision, RichText, Room,
};
use tracing::error;

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
}

impl Messenger {
    pub(crate) fn new() -> Self {
        Messenger {
            sidebar: Sidebar::new(168.0),
            main: Main::Contacts(Contacts::default()),
            pending_counter: ID::MAX,
        }
    }

    fn next_pending_id(&mut self) -> ID {
        let id = self.pending_counter;
        self.pending_counter = self.pending_counter.wrapping_sub(1);
        id
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
}

impl Messenger {
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
                                            text: RichText::plain(contents.clone()),
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
                                                            text: RichText::plain(contents),
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
                            ChatUpdateResult::ToggleReaction {
                                interface,
                                room,
                                message_id,
                                emoji,
                                reacted,
                            } => Action::Run(
                                Task::future(async move {
                                    let text = match interface.api.text() {
                                        Ok(t) => t,
                                        Err(e) => {
                                            error!("Text not supported: {e:?}");
                                            return;
                                        }
                                    };
                                    let msg_ident =
                                        Identifier::new(message_id, InterfaceMessage::default());
                                    let result = if reacted {
                                        text.remove_reaction(&room, &msg_ident, &emoji).await
                                    } else {
                                        text.add_reaction(&room, &msg_ident, &emoji).await
                                    };
                                    if let Err(e) = result {
                                        error!("Failed to toggle reaction: {e:#?}");
                                    }
                                })
                                .then(|_| Task::none()),
                            ),
                        },
                    };
                }
                Action::None
            }
            Message::Contacts(msg) => {
                if let Main::Contacts(contacts) = &mut self.main {
                    return Action::Run(contacts.update(msg).map(Message::Contacts));
                }
                Action::None
            }
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
            let sidebar = self.sidebar.view(messengers).map(Message::Sidebar);

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

        column![profiles, row![navbar, window]].into()
    }
}
