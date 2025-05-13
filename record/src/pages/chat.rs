use std::{error::Error, fmt::Debug, sync::Arc};

use crate::AuthStore;

use adaptors::{
    types::{Message as ChatMessage, Store, User},
    Messanger as Auth,
};
use futures::{future::try_join_all, try_join};
use iced::{
    widget::{
        column, container, image, row,
        scrollable::{Direction, Scrollbar},
        text::LineHeight,
        Button, Column, Responsive, Scrollable, Text, TextInput,
    },
    Alignment, ContentFit, Length, Task,
};
use widgets::divider;

#[derive(Debug, Clone)]
struct MessangerData {
    profile: User,
    contacts: Vec<User>,
    conversations: Vec<Store>,
    guilds: Vec<Store>,
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    DividerChange(f32),
    OpenScreen(Screen),
    LoadConversation(Store),
    MessageInput(String),
    MessageSend,
}

#[derive(Debug, Clone)]
enum Screen {
    Contacts {
        search_input: String,
    },
    Chat {
        auth: Arc<dyn Auth>,
        meta_data: Store,
        messages: Vec<ChatMessage>,
        msg: String,
    },
}

pub struct MessangerWindow {
    screen: Screen,
    messangers_data: Vec<MessangerData>,
    sidebar_width: f32,
}
impl Debug for MessangerWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessangerWindow")
            .field("auth_store", &"TODO: Find a way to print this")
            .field("main", &self.screen)
            .field("messangers_data", &self.messangers_data)
            .finish()
    }
}

impl MessangerWindow {
    pub(crate) async fn new(
        auths: Vec<Arc<dyn Auth>>,
    ) -> Result<Self, Arc<dyn Error + Sync + Send>> {
        let reqs = auths.iter().map(async move |auth| {
            let q = auth.query().unwrap();
            try_join!(
                q.get_profile(),
                q.get_conversation(),
                q.get_contacts(),
                q.get_guilds(),
            )
        });

        let messangers_data = try_join_all(reqs)
            .await?
            .into_iter()
            .map(|(profile, conversations, contacts, guilds)| MessangerData {
                profile,
                contacts,
                conversations,
                guilds,
            })
            .collect::<Vec<_>>();

        let window = MessangerWindow {
            screen: Screen::Contacts {
                search_input: String::new(),
            },
            messangers_data,
            sidebar_width: 168.0,
        };

        Ok(window)
    }
}

pub enum Action {
    None,
    Run(Task<Message>),
}

impl MessangerWindow {
    pub(crate) fn update(&mut self, message: Message, auth_store: &AuthStore) -> Action {
        match message {
            Message::DividerChange(val) => {
                if (self.sidebar_width + val > 300.0) | (self.sidebar_width + val < 100.0) {
                    return Action::None;
                }
                self.sidebar_width += val;
                Action::None
            }
            Message::OpenScreen(screen) => {
                self.screen = screen;
                Action::None
            }
            Message::LoadConversation(msgs_store) => {
                let auth = auth_store
                    .get_auths()
                    .into_iter()
                    .find(|auth| msgs_store.origin_uuid == auth.uuid())
                    .clone();

                if let Some(auth) = auth {
                    let future = async move {
                        let msgs = {
                            let pq = auth.param_query().unwrap();
                            pq.get_messanges(&msgs_store, None).await.unwrap()
                        };

                        (auth, msgs_store, msgs)
                    };

                    return Action::Run(Task::perform(future, |(auth, msgs_store, mess)| {
                        Message::OpenScreen(Screen::Chat {
                            auth,
                            meta_data: msgs_store,
                            messages: mess,
                            msg: String::new(),
                        })
                    }));
                };

                Action::None
            }
            Message::MessageInput(change) => {
                match &mut self.screen {
                    Screen::Chat { msg, .. } => {
                        *msg = change;
                    }
                    Screen::Contacts { search_input } => {
                        *search_input = change;
                    }
                }
                Action::None
            }
            Message::MessageSend => {
                let Screen::Chat {
                    auth,
                    meta_data,
                    msg,
                    ..
                } = &mut self.screen
                else {
                    return Action::None;
                };

                let auth = auth.clone();
                let meta_data = meta_data.clone();
                let contents = msg.clone();
                let future = async move {
                    let b = auth.param_query().unwrap();
                    b.send_message(&meta_data, contents).await.unwrap();
                    ()
                };

                // TODO: Make this better (Probably reverse the order)
                Action::Run(Task::perform(future, |_| {
                    Message::MessageInput(String::new())
                }))
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
        let window = Responsive::new(move |size| {
            let sidebar = Scrollable::new(
                column![
                    Button::new(
                        container("Contacts")
                            .width(Length::Fill)
                            .align_x(Alignment::Center)
                    )
                    .on_press(Message::OpenScreen(Screen::Contacts {
                        search_input: String::new()
                    }))
                    .width(Length::Fill),
                    self.messangers_data[0]
                        .conversations
                        .iter()
                        .map(|i| {
                            Button::new(i.name.as_str())
                                .width(Length::Fill)
                                .on_press(Message::LoadConversation(i.to_owned()).into())
                        })
                        .fold(Column::new(), |column, widget| column.push(widget))
                ]
                .width(self.sidebar_width),
            )
            .direction(Direction::Vertical(
                Scrollbar::default().width(7).scroller_width(7),
            ));

            let main = match &self.screen {
                Screen::Contacts { search_input } => {
                    let widget = Column::new();
                    let widget = widget.push(
                        TextInput::new("Search", search_input).on_input(Message::MessageInput),
                    );
                    widget.push(
                        self.messangers_data[0]
                            .contacts
                            .iter()
                            .filter_map(|i| {
                                if search_input.is_empty() || i.username.to_lowercase().contains(search_input.to_lowercase().as_str()) {
                                    return Some(Text::from(i.username.as_str()))
                                } 
                                None
                            })
                            .fold(Column::new(), |column, widget| column.push(widget)),
                    )
                }
                Screen::Chat {
                    messages,
                    msg,
                    meta_data,
                    ..
                } => {
                    let meta_data = row![Text::new(meta_data.name.clone())];

                    let chat = Scrollable::new(
                        messages
                            .iter()
                            .rev()
                            .map(|msg| Text::from(msg.text.as_str()))
                            .fold(Column::new(), |column, widget| column.push(widget)),
                    )
                    .anchor_bottom()
                    .width(Length::Fill)
                    .height(Length::Fill);

                    let message_box = TextInput::new("New msg...", msg)
                        .on_input(Message::MessageInput)
                        .on_submit(Message::MessageSend)
                        .line_height(LineHeight::Absolute(20.into()));

                    column![meta_data, chat, message_box].into()
                }
            };
            row![
                sidebar,
                divider::Divider::new(10.0, size.height, Message::DividerChange),
                main
            ]
            .into()
        });

        column![options, row![navbar, window]].into()
    }
}
