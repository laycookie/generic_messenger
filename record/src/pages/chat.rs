use std::{error::Error, fmt::Debug, sync::Arc};

use crate::{auth::Messanger, AuthStore};

use super::MyAppMessage;
use adaptors::types::{Message as ChatMessage, MsgsStore, User};
use futures::{future::try_join_all, try_join};
use iced::{
    widget::{
        column, container, image, row,
        scrollable::{Direction, Scrollbar},
        Button, Column, Scrollable, Text, TextInput,
    },
    Alignment, ContentFit, Length, Task,
};

#[derive(Debug, Clone)]
pub enum Message {
    OpenContacts,
    LoadConversation(MsgsStore, Vec<ChatMessage>),
    OpenConversation(MsgsStore),
}

// TODO: Automate
impl Into<MyAppMessage> for Message {
    fn into(self) -> MyAppMessage {
        MyAppMessage::Chat(self)
    }
}
//
#[derive(Clone)]
pub struct MessangerWindow {
    main: Main,
    messangers_data: Vec<MsngrData>,
}
impl Debug for MessangerWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessangerWindow")
            .field("auth_store", &"TODO: Find a way to print this")
            .field("main", &self.main)
            .field("messangers_data", &self.messangers_data)
            .finish()
    }
}

#[derive(Debug, Clone)]
struct MsngrData {
    profile: User,
    contacts: Vec<User>,
    conversations: Vec<MsgsStore>,
    guilds: Vec<MsgsStore>,
}

#[derive(Debug, Clone)]
enum Main {
    Contacts,
    Chat {
        _location: MsgsStore,
        messages: Vec<ChatMessage>,
    },
}

impl MessangerWindow {
    pub(crate) async fn new(m: Vec<Messanger>) -> Result<Self, Arc<dyn Error + Sync + Send>> {
        let reqs = m.iter().map(async move |m| {
            let q = m.auth.query().unwrap();
            try_join!(
                q.get_profile(),
                q.get_conversation(),
                q.get_contacts(),
                q.get_guilds(),
            )
        });

        let msngrs = try_join_all(reqs)
            .await?
            .into_iter()
            .map(|(profile, conversations, contacts, guilds)| MsngrData {
                profile,
                contacts,
                conversations,
                guilds,
            })
            .collect::<Vec<_>>();

        let window = MessangerWindow {
            main: Main::Contacts,
            messangers_data: msngrs,
        };

        Ok(window)
    }
}

pub enum Action {
    None,
    Run(Task<Message>),
}

impl MessangerWindow {
    // impl PageT<Message, Action> for MessangerWindow {
    pub(crate) fn update(&mut self, message: Message, auth_store: &AuthStore) -> Action {
        match message {
            Message::LoadConversation(msgs_store, mess) => {
                self.main = Main::Chat {
                    _location: msgs_store,
                    messages: mess,
                };
                return Action::None;
            }
            Message::OpenConversation(msgs_store) => {
                for messanger in auth_store.get_messangers() {
                    let auth = messanger.auth.clone();
                    let msgs_store = msgs_store.clone();
                    if msgs_store.origin_uuid == auth.uuid() {
                        return Action::Run(Task::perform(
                            async move {
                                let pq = auth.param_query().unwrap();
                                (
                                    msgs_store.clone(),
                                    pq.get_messanges(&msgs_store, None).await.unwrap(),
                                )
                            },
                            |(msgs_store, mess)| Message::LoadConversation(msgs_store, mess),
                        ));
                    }
                }
                return Action::None;
            }
            Message::OpenContacts => {
                self.main = Main::Contacts;
                Action::None
            }
        }
    }

    pub(crate) fn view(&self) -> iced::Element<Message> {
        let options = row![Text::new(&self.messangers_data[0].profile.username)];

        let navbar = Scrollable::new(
            self.messangers_data[0]
                .guilds
                .iter()
                .map(|i| {
                    let image = match &i.icon {
                        Some(icon) => image(icon),
                        None => image("./public/imgs/placeholder.jpg"),
                    };
                    Button::new(
                        image
                            .height(Length::Fixed(48.0))
                            .width(Length::Fixed(48.0))
                            .content_fit(ContentFit::Cover),
                    )
                })
                .fold(Column::new(), |column, widget| column.push(widget)),
        )
        .direction(Direction::Vertical(
            Scrollbar::default().width(0).scroller_width(0),
        ));

        let sidebar = Scrollable::new(
            column![
                Button::new(
                    container("Contacts")
                        .width(Length::Fill)
                        .align_x(Alignment::Center)
                )
                .on_press(Message::OpenContacts)
                .width(Length::Fill),
                self.messangers_data[0]
                    .conversations
                    .iter()
                    .map(|i| {
                        Button::new(i.name.as_str())
                            .width(Length::Fill)
                            .on_press(Message::OpenConversation(i.to_owned()).into())
                    })
                    .fold(Column::new(), |column, widget| column.push(widget))
            ]
            .width(168),
        )
        .direction(Direction::Vertical(
            Scrollbar::default().width(7).scroller_width(7),
        ));

        let main = match &self.main {
            Main::Contacts => {
                let widget = Column::new();
                let widget = widget.push(TextInput::new("Search", ""));
                widget.push(
                    self.messangers_data[0]
                        .contacts
                        .iter()
                        .map(|i| Text::from(i.username.as_str()))
                        .fold(Column::new(), |column, widget| column.push(widget)),
                )
            }
            Main::Chat { messages, .. } => {
                let chat = Column::new();
                let chat = chat.push(
                    Scrollable::new(
                        messages
                            .iter()
                            .map(|msg| Text::from(msg.text.as_str()))
                            .fold(Column::new(), |column, widget| column.push(widget)),
                    )
                    .height(Length::Shrink),
                );
                let chat = chat.push(TextInput::new("New msg...", ""));
                chat
            }
        };

        column![options, row![navbar, sidebar, main]].into()
    }
}

