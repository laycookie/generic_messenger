use std::{borrow::Borrow, fmt::Debug};

use crate::{
    messanger_unifier::{MessangerHandle, Messangers},
    pages::messenger::{
        chat::{Chat, Message as ChatMessage},
        contacts::{Contacts, Message as ContactsMessage},
        conversation_sidebar::Sidebar,
        navbar::Navbar,
    },
};

use adaptors::types::{Chan, Identifier, Msg};
use iced::{
    Task,
    widget::{Responsive, Text, column, row},
};
use widgets::divider;

mod chat;
mod contacts;
mod conversation_sidebar;
mod navbar;

pub(super) const PLACEHOLDER_PFP: &str = "./public/imgs/placeholder.jpg";

#[derive(Debug, Clone)]
pub(crate) enum Message {
    Chat(ChatMessage),
    Contacts(ContactsMessage),
    // ===
    ChangeMain(Main),
    DividerChange(f32),
    // ===
    UpdateChat {
        handle: MessangerHandle,
        kv: (Identifier<()>, Vec<Identifier<Msg>>),
    },
    LoadConversation {
        handle: MessangerHandle,
        conversation: Identifier<Chan>,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum Main {
    Contacts(Contacts),
    Chat(Chat),
}

#[derive(Debug)]
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
        kv: (Identifier<()>, Vec<Identifier<Msg>>),
    },
    Run(Task<Message>),
}

impl Messenger {
    pub(crate) fn update(&mut self, message: Message, messengers: &Messangers) -> Action {
        match message {
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
            Message::Chat(msg) => {
                if let Main::Chat(chat) = &mut self.main {
                    return Action::Run(chat.update(msg).map(Message::Chat));
                };
                Action::None
            }
            Message::Contacts(msg) => {
                if let Main::Contacts(contacts) = &mut self.main {
                    return Action::Run(contacts.update(msg).map(Message::Contacts));
                };
                Action::None
            }
            Message::UpdateChat { handle, kv } => Action::UpdateChat { handle, kv },
            Message::LoadConversation {
                handle,
                conversation,
            } => {
                let Some(interface) = messengers.interface_from_handle(handle) else {
                    return Action::None;
                };
                let interface = interface.to_owned();

                if let Some(messanger) = messengers.data_from_handle(handle)
                    && messanger.chats.contains_key(conversation.borrow())
                {
                    return Action::Run(Task::done(Message::ChangeMain(Main::Chat(Chat::new(
                        interface,
                        conversation,
                    )))));
                }

                Action::Run(
                    Task::future(async move {
                        let msgs = {
                            let pq = interface.1.param_query().unwrap();
                            pq.get_messanges(&conversation, None).await.unwrap()
                        };

                        (interface, conversation, msgs)
                    })
                    .then(|(interface, conversation, msgs)| {
                        let channel_id: &Identifier<()> = conversation.borrow();
                        Task::done(Message::UpdateChat {
                            handle: interface.0,
                            kv: (channel_id.to_owned(), msgs),
                        })
                        .chain(Task::done(Message::ChangeMain(
                            Main::Chat(Chat::new(interface, conversation)),
                        )))
                    }),
                )
            }
        }
    }

    pub(crate) fn view<'a>(&'a self, messengers: &'a Messangers) -> iced::Element<'a, Message> {
        let profiles = row![Text::from(match messengers.data_iter().next() {
            Some(messanger_data) => {
                match &messanger_data.profile {
                    Some(p) => p.name.as_str(),
                    None => "No connection made?",
                }
            }
            None => "No messangers connected",
        })];

        let navbar = Navbar::get_element(messengers.data_iter().flat_map(|data| &data.guilds));

        let window = Responsive::new(move |size| {
            let sidebar = self.sidebar.get_element(messengers);

            let main = match &self.main {
                Main::Chat(chat) => chat.get_element(messengers).map(Message::Chat),
                Main::Contacts(contacts) => contacts.get_element(messengers).map(Message::Contacts),
            };
            row![
                sidebar,
                divider::Divider::new(10.0, size.height, Message::DividerChange),
                main
            ]
            .into()
        });

        column![profiles, row![navbar, window]].into()
    }
}
