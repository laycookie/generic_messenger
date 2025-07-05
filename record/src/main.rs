use adaptors::SocketEvent;
use auth::MessangersGenerator;
use futures::{channel::mpsc::Sender, future::{try_join_all, join_all}, stream, try_join, Stream, StreamExt};
use iced::{window, Element, Subscription, Task};
use messanger_unifier::Messangers;
use pages::{chat::MessengingWindow, Login, MyAppMessage};
use socket::{ReciverEvent, SocketsInterface};

use crate::messanger_unifier::MessangerHandle;
use crate::pages::login::Message;

mod auth;
mod cache;
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
                    App::new(Messangers::default(), Screen::Login(Login::new())),
                    false,
                )
            }
        }
        Err(_) => (
            // TODO: This will probably not handle the error well.
            App::new(Messangers::default(), Screen::Login(Login::new())),
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
        .inspect_err(|err| println!("{}", err))?;

    Ok(())
}

struct App {
    page: Screen,
    messangers: Messangers,
    socket_sender: Option<Sender<ReciverEvent>>,
}

impl App {
    fn new(messangers: Messangers, page: Screen) -> Self {
        Self {
            page,
            messangers,
            socket_sender: None,
        }
    }

    fn title() -> &'static str {
        "record"
    }
    fn update(&mut self, message: MyAppMessage) -> impl Into<Task<MyAppMessage>> {
        match message {
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
                let interfaces = self
                    .messangers
                    .interface_iter()
                    .cloned()
                    .collect::<Vec<_>>();
                let mut sender = self.socket_sender.clone().unwrap();
                Task::future(async move {
                    let qs = interfaces.into_iter().map(|interface| {
                        let (id, messanger) = interface.clone();
                        let mut sender = sender.clone();
                        async move {
                            let Some(q) = messanger.query() else {
                                return None;
                            };

                            let (profile, conversations, contacts, servers) = match try_join!(
                                q.get_profile(),
                                q.get_conversation(),
                                q.get_contacts(),
                                q.get_guilds()
                            ) {
                                Ok(t) => t,
                                Err(e) => return None,
                            };
                            sender
                                .try_send(ReciverEvent::Connection(interface))
                                .unwrap();
                            Some((id, profile, contacts, conversations, servers))
                        }
                    });
                    join_all(qs).await.into_iter().filter_map(|x| x).collect()
                })
                .then(|r: Vec<_>| {
                    let mut ta = Task::done(MyAppMessage::MessangerStructSync(r.clone()));
                    if r.len() == 0 {
                        return Task::done(MyAppMessage::RedirectToLogin);
                    }
                    for m in r {
                        let (handle, profile, contacts, conversations, servers) = m;
                        ta = ta.chain(Task::done(MyAppMessage::SetMessangerData {
                            messanger_handle: handle,
                            new_data: pages::MessangerData::Everything {
                                profile,
                                contacts,
                                conversations,
                                servers,
                            },
                        }));
                    }

                    ta.chain(Task::done(MyAppMessage::OpenPage(Screen::Chat(
                        MessengingWindow::new(),
                    ))))
                })
            }
            // Global Actions
            MyAppMessage::OpenPage(page) => {
                self.page = page;
                Task::none()
            }
            MyAppMessage::RedirectToLogin => {
                if let Screen::Login(_) = &self.page {
                    return Task::done(MyAppMessage::Login(Message::ToggleButtonState));
                }
                return Task::done(MyAppMessage::OpenPage(Screen::Login(Login::new())));
            }
            MyAppMessage::SocketEvent(event) => match event {
                SocketMesg::Connect(mut socket_connection) => {
                    let interfaces = self.messangers.interface_iter();
                    interfaces.into_iter().for_each(|auth| {
                        let _temp =
                            socket_connection.try_send(ReciverEvent::Connection(auth.clone()));
                    });
                    self.socket_sender = Some(socket_connection);
                    Task::none()
                }
                SocketMesg::Message((handle, socket_event)) => {
                    match socket_event {
                        SocketEvent::MessageCreated { channel, msg } => {
                            let d = self.messangers.mut_data_from_handle(handle).unwrap();
                            println!("{:#?}", d.chats);
                            println!("{:#?}", channel);
                            let msgs = d.chats.get_mut(&channel).unwrap();
                            println!("{:#?}", msgs);
                            msgs.push(msg);
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
                    pages::login::Action::Run(task) => task.map(MyAppMessage::Login),
                    pages::login::Action::Login(messenger) => {
                        let handle = self.messangers.add_messanger(messenger);
                        //redundant chain
                        Task::done(MyAppMessage::StartUp)
                    }
                }
            }
            MyAppMessage::MessangerStructSync(messangers) => {
                for (messanger, _, _, _, _) in messangers {
                    self.messangers.retain_by_handle(messanger);
                }
                Task::none()
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
        Subscription::run(spawn_sockets_interface).map(|t| MyAppMessage::SocketEvent(t))
    }
}

#[derive(Debug)]
enum SocketMesg {
    Connect(Sender<ReciverEvent>),
    Message((MessangerHandle, SocketEvent)),
}

fn spawn_sockets_interface() -> impl Stream<Item = SocketMesg> {
    iced::stream::channel(128, |mut output| async move {
        let (mut interface, sender) = SocketsInterface::new();
        output.try_send(SocketMesg::Connect(sender)).unwrap();

        while let Some(a) = interface.next().await {
            output.try_send(SocketMesg::Message(a)).unwrap();
        }
    })
}
