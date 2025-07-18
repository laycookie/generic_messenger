use crate::pages::login::Message as LoginMessage;
use adaptors::{Socket, SocketEvent};
use auth::MessangersGenerator;
use futures::{Stream, StreamExt, channel::mpsc::Sender, future::join_all, try_join};
use iced::{Element, Subscription, Task, window};
use messanger_unifier::Messangers;
use pages::{Login, MyAppMessage, chat::MessengingWindow};
use std::sync::Weak;

use crate::messanger_unifier::MessangerHandle;
use crate::socket::ActiveStream;

mod auth;
mod messanger_unifier;
mod pages;
mod socket;

#[derive(Debug)]
pub enum Screen {
    Loading,
    Login(Login),
    Chat(MessengingWindow),
}

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (app, is_loading) = match MessangersGenerator::messengers_from_file("./LoginInfo".into()) {
        Ok(messangers) => {
            if messangers.len() > 0 {
                (App::new(messangers, Screen::Loading), true)
            } else {
                (
                    App::new(Messangers::default(), Screen::Login(Login::default())),
                    false,
                )
            }
        }
        Err(_) => (
            // TODO: This will probably not handle the error well.
            App::new(Messangers::default(), Screen::Login(Login::default())),
            false,
        ),
    };

    iced::daemon(App::title(), App::update, App::view)
        .subscription(App::subscription)
        .run_with(move || {
            let (_window_id, window_task) = window::open(window::Settings::default());
            (
                app,
                window_task.then(move |_| match is_loading {
                    true => Task::done(MyAppMessage::StartUp),
                    false => Task::none(),
                }),
            )
        })
        .inspect_err(|err| println!("{err}"))?;

    Ok(())
}

struct App {
    page: Screen,
    messangers: Messangers,
    active_streams: Vec<(MessangerHandle, Weak<dyn Socket + Send + Sync>)>,
}

impl App {
    fn new(messangers: Messangers, page: Screen) -> Self {
        Self {
            page,
            messangers,
            active_streams: Vec::new(),
        }
    }

    fn title() -> &'static str {
        "record"
    }
    fn update(&mut self, message: MyAppMessage) -> Task<MyAppMessage> {
        match message {
            MyAppMessage::SaveMessengers => {
                MessangersGenerator::messangers_to_file(&self.messangers, "./LoginInfo".into());
                Task::none()
            }
            MyAppMessage::RemoveMessanger(handle) => {
                self.messangers.remove_by_handle(handle);
                Task::none()
            }
            MyAppMessage::SetMessangerData {
                messanger_handle,
                new_data,
            } => {
                let d = self
                    .messangers
                    .mut_data_from_handle(messanger_handle)
                    .unwrap();
                match new_data {
                    pages::MessangerData::Everything {
                        profile,
                        contacts,
                        conversations,
                        servers,
                    } => {
                        d.profile = Some(profile);
                        d.contacts = contacts;
                        d.conversations = conversations;
                        d.guilds = servers;
                    }
                    pages::MessangerData::Profile(p) => {
                        d.profile = Some(p);
                    }
                    pages::MessangerData::Servers(s) => {
                        d.guilds = s;
                    }
                    pages::MessangerData::Chat((k, v)) => {
                        d.chats.insert(k.clone(), v);
                    }
                };
                Task::none()
            }
            MyAppMessage::StartUp => {
                Task::future(join_all(self.messangers.interface_iter().map(
                    |interface| {
                        let (handle, interface) = interface.clone();
                        async move {
                            let Some(q) = interface.query() else {
                                return Ok(None);
                            };

                            let (profile, conversations, contacts, servers) = match try_join!(
                                q.get_profile(),
                                q.get_conversation(),
                                q.get_contacts(),
                                q.get_guilds()
                            ) {
                                Ok(t) => t,
                                Err(e) => return Err((handle, e)),
                            };

                            Ok(Some((handle, profile, contacts, conversations, servers)))
                        }
                    },
                )))
                .then(|outputs| {
                    if !outputs.iter().any(|m| m.is_ok()) {
                        // In case we are running this from login screen. If
                        // we are not there this would be equivalent of Task::none()
                        return Task::done(MyAppMessage::Login(LoginMessage::ToggleButtonState));
                    };

                    let tasks_itr = outputs.into_iter().map(|m| {
                        let m = match m {
                            Ok(m) => m,
                            Err((handle, e)) => {
                                eprintln!("Failed to fetch the data: {e}");
                                return Task::done(MyAppMessage::RemoveMessanger(handle));
                            }
                        };
                        let (handle, profile, contacts, conversations, servers) = m.unwrap();

                        Task::done(MyAppMessage::SetMessangerData {
                            messanger_handle: handle,
                            new_data: pages::MessangerData::Everything {
                                profile,
                                contacts,
                                conversations,
                                servers,
                            },
                        })
                    });

                    Task::batch(tasks_itr)
                        .chain(Task::done(MyAppMessage::SocketEvent(SocketMesg::Connect)))
                        .chain(Task::done(MyAppMessage::OpenPage(Screen::Chat(
                            MessengingWindow::new(),
                        ))))
                        .chain(Task::done(MyAppMessage::SaveMessengers))
                })
            }
            // Global Actions
            MyAppMessage::OpenPage(page) => {
                self.page = page;
                Task::none()
            }
            MyAppMessage::SaveStreams(active_streams) => {
                self.active_streams = active_streams;
                Task::none()
            }
            MyAppMessage::SocketEvent(event) => match event {
                SocketMesg::Connect => Task::perform(
                    join_all(self.messangers.interface_iter().map(|interface| {
                        let (handle, messanger) = interface.to_owned();
                        async move {
                            let Some(socket) = messanger.socket().await else {
                                return None;
                            };
                            Some((handle, socket))
                        }
                    })),
                    |active_streams| {
                        let active_streams = active_streams.into_iter().filter_map(|x| x).collect();
                        MyAppMessage::SaveStreams(active_streams)
                    },
                ),
                SocketMesg::Message((handle, socket_event)) => {
                    match socket_event {
                        SocketEvent::MessageCreated { channel, msg } => {
                            let d = self.messangers.mut_data_from_handle(handle).unwrap();
                            // println!("{:#?}", d.chats);
                            // println!("{channel:#?}");
                            match d.chats.get_mut(&channel) {
                                Some(msgs) => msgs.push(msg),
                                None => {
                                    d.chats.insert(channel, vec![msg]);
                                }
                            };
                        }
                        SocketEvent::Disconected => println!("Disconected"),
                        SocketEvent::Skip => println!("Skiped"),
                    };
                    Task::none()
                }
            },
            // ====== Pages ======
            MyAppMessage::Login(message) => {
                let Screen::Login(login) = &mut self.page else {
                    return Task::none();
                };
                match login.update(message) {
                    pages::login::Action::None => Task::none(),
                    pages::login::Action::Login(messenger) => {
                        let handle = self.messangers.add_messanger(messenger);
                        Task::done(MyAppMessage::StartUp)
                    }
                }
            }
            MyAppMessage::Chat(message) => {
                let Screen::Chat(chat) = &mut self.page else {
                    return Task::none();
                };
                match chat.update(message, &self.messangers) {
                    pages::chat::Action::None => Task::none(),
                    pages::chat::Action::UpdateChat { handle, kv } => {
                        Task::done(MyAppMessage::SetMessangerData {
                            messanger_handle: handle,
                            new_data: pages::MessangerData::Chat(kv),
                        })
                    }
                    pages::chat::Action::Run(task) => task.map(MyAppMessage::Chat),
                }
            }
        }
    }
    fn view(&self, _window: window::Id) -> Element<MyAppMessage> {
        match &self.page {
            Screen::Login(login) => login.view().map(MyAppMessage::Login),
            Screen::Chat(chat) => chat.view(&self.messangers).map(MyAppMessage::Chat),
            Screen::Loading => iced::widget::text("Loading").into(),
        }
    }
    fn subscription(&self) -> Subscription<MyAppMessage> {
        if self.active_streams.is_empty() {
            return Subscription::none();
        }
        Subscription::batch(
            self.active_streams
                .clone()
                .into_iter()
                .map(|(handle, socket)| {
                    Subscription::run_with_id(handle.id(), ActiveStream::new(handle, socket))
                }),
        )
        .map(|msg| MyAppMessage::SocketEvent(SocketMesg::Message(msg)))
    }
}

#[derive(Debug)]
enum SocketMesg {
    Connect,
    Message((MessangerHandle, SocketEvent)),
}
