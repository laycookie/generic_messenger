use std::{collections::HashMap, error::Error, fmt::Debug, sync::Arc};

use crate::auth::Messanger;

use super::{MyAppMessage, Page};
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
    OpenConversation(Vec<ChatMessage>),
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
    chat: HashMap<String, String>,
}

#[derive(Debug, Clone)]
enum Main {
    Contacts,
    Chat { messages: Vec<ChatMessage> },
}

impl MessangerWindow {
    pub async fn new(m: Vec<Messanger>) -> Result<Self, Arc<dyn Error + Sync + Send>> {
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
                chat: HashMap::new(),
            })
            .collect::<Vec<_>>();

        let window = MessangerWindow {
            main: Main::Contacts,
            messangers_data: msngrs,
        };

        Ok(window)
    }
}

impl Page for MessangerWindow {
    fn update(&mut self, message: MyAppMessage) -> Task<MyAppMessage> {
        if let MyAppMessage::Chat(message) = message {
            match message {
                Message::OpenConversation(mess) => {
                    self.main = Main::Chat { messages: mess };
                }
                Message::OpenContacts => self.main = Main::Contacts,
            }
        }

        Task::none()
    }

    fn view(&self) -> iced::Element<MyAppMessage> {
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
                .on_press(MyAppMessage::Chat(Message::OpenContacts))
                .width(Length::Fill),
                self.messangers_data[0]
                    .conversations
                    .iter()
                    .map(|i| {
                        Button::new(i.name.as_str())
                            .width(Length::Fill)
                            .on_press(MyAppMessage::LoadConversation(i.to_owned()).into())
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
            Main::Chat { messages } => {
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
