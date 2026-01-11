use std::error::Error;

use crate::{
    components::divider::Divider,
    messanger_unifier::{Call, MessangerHandle, MessangerInterface, Messangers},
    pages::messenger::{
        chat::{Action as ChatAction, Chat},
        contacts::{Contacts, Message as ContactsMessage},
        conversation_sidebar::{Action as SidebarAction, Server, Sidebar},
        navbar::{Action as NavbarAction, Navbar},
    },
};

use iced::{
    Task,
    widget::{Responsive, Text, column, row},
};
use messenger_interface::types::{ID, Identifier, Message as InterfaceMessage, Room};
use tracing::error;

mod chat;
mod contacts;
mod conversation_sidebar;
mod navbar;

pub(super) const PLACEHOLDER_PFP: &str = "./public/imgs/placeholder.jpg";

#[derive(Clone)]
pub(crate) enum Message {
    Chat(ChatAction),
    Contacts(ContactsMessage),
    Navbar(NavbarAction),
    Sidebar(SidebarAction),
    // ===
    ChangeMain(Main),
    SetSidebarServer(Option<Server>), // TODO: Make it just a SetSidebar
    DividerChange(f32),
    UpdateChat {
        handle: MessangerHandle,
        kv: (ID, Vec<Identifier<InterfaceMessage>>),
    },
}

#[derive(Clone)]
pub(crate) enum Main {
    Contacts(Contacts),
    Chat(Chat),
}

pub struct Messenger {
    sidebar: Sidebar,
    main: Main,
}

impl Messenger {
    pub(crate) fn new() -> Self {
        Messenger {
            sidebar: Sidebar::new(168.0),
            main: Main::Contacts(Contacts::default()),
        }
    }
}

pub enum Action {
    None,
    UpdateChat {
        handle: MessangerHandle,
        kv: (ID, Vec<Identifier<InterfaceMessage>>),
    },
    Run(Task<Message>),
    Call {
        interface: MessangerInterface,
        channel: Identifier<Room>,
    },
    DisconnectFromCall(Call),
}

impl Messenger {
    pub(crate) fn update(&mut self, message: Message, messengers: &Messangers) -> Action {
        match message {
            Message::SetSidebarServer(server) => {
                self.sidebar.server_selected = server;
                Action::None
            }
            Message::ChangeMain(screen) => {
                self.main = screen;
                Action::None
            }
            Message::DividerChange(val) => {
                if (self.sidebar.width + val > 300.0) | (self.sidebar.width + val < 100.0) {
                    return Action::None;
                }
                self.sidebar.width += val;
                Action::None
            }
            Message::UpdateChat { handle, kv } => Action::UpdateChat { handle, kv },
            Message::Chat(msg) => {
                if let Main::Chat(chat) = &mut self.main {
                    return match msg {
                        ChatAction::Call { interface, room } => {
                            let Some(interface) =
                                messengers.interface_from_handle(interface.handle)
                            else {
                                return Action::None;
                            };
                            Action::Call {
                                interface: interface.clone(),
                                channel: room,
                            }
                        }
                        ChatAction::Message(message) => Action::Run(
                            chat.update(message)
                                .map(|message| Message::Chat(ChatAction::Message(message))),
                        ),
                    };
                };
                Action::None
            }
            Message::Contacts(msg) => {
                if let Main::Contacts(contacts) = &mut self.main {
                    return Action::Run(contacts.update(msg).map(Message::Contacts));
                };
                Action::None
            }

            Message::Navbar(action) => match action {
                NavbarAction::GetDMs => Action::Run(Task::done(Message::SetSidebarServer(None))),
                NavbarAction::GetGuild { handle, server } => {
                    let Some(interface) = messengers.interface_from_handle(handle) else {
                        return Action::None;
                    };
                    let interface = interface.to_owned();

                    let channels = server.rooms.clone();
                    Action::Run(Task::done(Message::SetSidebarServer(Some(Server::new(
                        interface.handle,
                        channels,
                    )))))
                }
            },
            Message::Sidebar(action) => match action {
                SidebarAction::Disconnect(call) => Action::DisconnectFromCall(call),
                // TODO: Only calls server check if can be simplified
                SidebarAction::Call(channel) => {
                    let server = self.sidebar.server_selected.as_ref().unwrap();
                    let Some(interface) = messengers.interface_from_handle(server.handle) else {
                        return Action::None;
                    };

                    Action::Call {
                        interface: interface.to_owned(),
                        channel,
                    }
                }
                SidebarAction::OpenContacts => {
                    self.main = Main::Contacts(Contacts::default());
                    Action::None
                }
                SidebarAction::OpenChat {
                    handle,
                    conversation,
                } => {
                    let Some(interface) = messengers.interface_from_handle(handle) else {
                        return Action::None;
                    };

                    // Check cache
                    if let Some(messanger) = messengers.data_from_handle(handle)
                        && messanger.chats.contains_key(conversation.id())
                    {
                        return Action::Run(Task::done(Message::ChangeMain(Main::Chat(
                            Chat::new(interface.to_owned(), conversation),
                        ))));
                    }

                    // Otherwise fetch
                    Action::Run(Task::batch([
                        Task::done(Message::ChangeMain(Main::Chat(Chat::new(
                            interface.to_owned(),
                            conversation.clone(),
                        )))),
                        Task::future({
                            let interface = interface.to_owned();
                            async move {
                                let text = interface
                                    .api
                                    .text()
                                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                                let msgs = text.get_messages(&conversation, None).await?;
                                let handle = interface.handle;

                                Ok::<
                                    (
                                        MessangerHandle,
                                        Identifier<Room>,
                                        Vec<Identifier<InterfaceMessage>>,
                                    ),
                                    Box<dyn Error + Send + Sync>,
                                >((handle, conversation, msgs))
                            }
                        })
                        .then(
                            |t: Result<
                                (
                                    MessangerHandle,
                                    Identifier<Room>,
                                    Vec<Identifier<InterfaceMessage>>,
                                ),
                                Box<dyn Error + Send + Sync>,
                            >| match t {
                                Ok((handle, conversation, msgs)) => {
                                    Task::done(Message::UpdateChat {
                                        handle,
                                        kv: (*conversation.id(), msgs),
                                    })
                                }
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

    pub(crate) fn view<'a>(&'a self, messengers: &'a Messangers) -> iced::Element<'a, Message> {
        let profiles = row![Text::from(match messengers.data_iter().next() {
            Some(data) => {
                match &data.profile {
                    Some(p) => p.name.as_str(),
                    None => "No connection made?",
                }
            }
            None => "No messangers connected",
        })];

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
