use std::{borrow::Borrow, fmt::Debug, sync::Arc};

use crate::messanger_unifier::{MessangerHandle, Messangers};

use adaptors::{
    Messanger as Auth,
    types::{Chan, Identifier, Msg},
};
use iced::{
    Alignment, ContentFit, Length, Padding, Task,
    widget::{
        Button, Column, Responsive, Scrollable, Text, TextInput, column, container, image, row,
        scrollable::{Direction, Scrollbar},
        text::LineHeight,
    },
};
use widgets::divider;

#[derive(Debug, Clone)]
pub(crate) enum Message {
    ChangeMain(Main),
    DividerChange(f32),
    MsgInput(String),
    MsgSend,
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
    Contacts {
        search_input: String,
    },
    Chat {
        interface: (MessangerHandle, Arc<dyn Auth>),
        meta_data: Identifier<Chan>,
        msg_box: String,
    },
}

#[derive(Debug)]
pub struct MessengingWindow {
    sidebar_width: f32,
    main: Main,
}

impl MessengingWindow {
    pub(crate) fn new() -> Self {
        MessengingWindow {
            main: Main::Contacts {
                search_input: String::new(),
            },
            sidebar_width: 168.0,
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

impl MessengingWindow {
    pub(crate) fn update(&mut self, message: Message, messengers: &Messangers) -> Action {
        match message {
            Message::DividerChange(val) => {
                if (self.sidebar_width + val > 300.0) | (self.sidebar_width + val < 100.0) {
                    return Action::None;
                }
                self.sidebar_width += val;
                Action::None
            }
            Message::ChangeMain(screen) => {
                self.main = screen;
                Action::None
            }
            Message::UpdateChat { handle, kv } => {
                // println!("{:?}", kv);
                Action::UpdateChat { handle, kv }
            }
            Message::LoadConversation {
                handle,
                conversation: chan,
            } => {
                let Some(interface) = messengers.interface_from_handle(handle) else {
                    return Action::None;
                };
                let interface = interface.to_owned();

                if let Some(messanger) = messengers.data_from_handle(handle)
                    && messanger.chats.contains_key(chan.borrow())
                {
                    return Action::Run(Task::done(Message::ChangeMain(Main::Chat {
                        interface,
                        meta_data: chan,
                        msg_box: String::new(),
                    })));
                }

                Action::Run(
                    Task::future(async move {
                        let msgs = {
                            let pq = interface.1.param_query().unwrap();
                            pq.get_messanges(&chan, None).await.unwrap()
                        };

                        (interface, chan, msgs)
                    })
                    .then(|(interface, chan, msgs)| {
                        let channel_id: &Identifier<()> = chan.borrow();
                        Task::done(Message::UpdateChat {
                            handle: interface.0,
                            kv: (channel_id.to_owned(), msgs),
                        })
                        .chain(Task::done(Message::ChangeMain(
                            Main::Chat {
                                interface,
                                meta_data: chan,
                                msg_box: String::new(),
                            },
                        )))
                    }),
                )
            }
            Message::MsgInput(change) => {
                match &mut self.main {
                    Main::Chat { msg_box: msg, .. } => {
                        *msg = change;
                    }
                    Main::Contacts { search_input } => {
                        *search_input = change;
                    }
                }
                Action::None
            }
            Message::MsgSend => {
                let Main::Chat {
                    interface,
                    // auth,
                    meta_data,
                    msg_box: msg,
                    ..
                } = &mut self.main
                else {
                    return Action::None;
                };

                let auth = interface.1.clone();
                let meta_data = meta_data.clone();
                let contents = msg.clone();

                Action::Run(
                    Task::future(async move {
                        let param = auth.param_query().unwrap();
                        param.send_message(&meta_data, contents).await.unwrap();
                    })
                    .then(|_| Task::done(Message::MsgInput(String::new()))),
                )
            }
        }
    }

    pub(crate) fn view<'a>(&'a self, messengers: &'a Messangers) -> iced::Element<'a, Message> {
        let options = row![Text::from(match messengers.data_iter().next() {
            Some(messanger_data) => {
                match &messanger_data.profile {
                    Some(p) => p.data.name.as_str(),
                    None => "No connection made?",
                }
            }
            None => "No messangers connected",
        })];

        let navbar = Scrollable::new({
            let guilds = messengers.data_iter().flat_map(|data| {
                data.guilds.iter().map(|i| {
                    let image = match &i.data.icon {
                        Some(icon) => image(icon),
                        None => image("./public/imgs/placeholder.jpg"),
                    };
                    Button::new(
                        image
                            .height(Length::Fixed(48.0))
                            .width(Length::Fixed(48.0))
                            .content_fit(ContentFit::Cover),
                    )
                    .into()
                })
            });

            Column::with_children(guilds)
        })
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
                    .on_press(Message::ChangeMain(Main::Contacts {
                        search_input: String::new()
                    }))
                    .width(Length::Fill),
                    // TODO: Make it read from all of them
                    messengers
                        .data_iter()
                        .zip(messengers.interface_iter())
                        .flat_map(|(data, (m_handle, _))| {
                            data.conversations.iter().map(|conversation| {
                                Button::new({
                                    let image = match &conversation.data.icon {
                                        Some(icon) => image(icon),
                                        None => image("./public/imgs/placeholder.jpg"),
                                    };
                                    row![
                                        container(image.height(Length::Fixed(28.0)))
                                            .padding(Padding::new(0.0).right(10.0)),
                                        conversation.data.name.as_str()
                                    ]
                                })
                                .width(Length::Fill)
                                .on_press(
                                    Message::LoadConversation {
                                        handle: *m_handle, // TODO
                                        conversation: conversation.to_owned(),
                                    },
                                )
                            })
                        })
                        .fold(Column::new(), |column, widget| column.push(widget))
                ]
                .width(self.sidebar_width),
            )
            .direction(Direction::Vertical(
                Scrollbar::default().width(7).scroller_width(7),
            ));

            let main = match &self.main {
                Main::Contacts { search_input } => {
                    let widget = Column::new();
                    let widget = widget
                        .push(TextInput::new("Search", search_input).on_input(Message::MsgInput));
                    widget.push(
                        messengers
                            .data_iter()
                            .flat_map(|messanger| {
                                messanger.contacts.iter().filter_map(|i| {
                                    if search_input.is_empty()
                                        || i.data
                                            .name
                                            .to_lowercase()
                                            .contains(search_input.to_lowercase().as_str())
                                    {
                                        return Some(Text::from(i.data.name.as_str()));
                                    }
                                    None
                                })
                            })
                            .fold(Column::new(), |column, widget| column.push(widget)),
                    )
                }
                Main::Chat {
                    interface,
                    meta_data,
                    msg_box: msg,
                } => {
                    let channel_info = row![Text::new(meta_data.data.name.clone())];

                    let messages = messengers
                        .data_from_handle(interface.0)
                        .unwrap()
                        .chats
                        .get(meta_data.borrow());

                    let chat = Scrollable::new(match messages {
                        Some(messages) => messages
                            .iter()
                            .map(|msg| Text::from(msg.data.text.as_str()))
                            .fold(Column::new(), |column, widget| column.push(widget)),
                        None => Column::new(),
                    })
                    .anchor_bottom()
                    .width(Length::Fill)
                    .height(Length::Fill);

                    let message_box = TextInput::new("New msg...", msg)
                        .on_input(Message::MsgInput)
                        .on_submit(Message::MsgSend)
                        .line_height(LineHeight::Absolute(20.into()));

                    column![channel_info, chat, message_box]
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
