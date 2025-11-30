use std::{borrow::Cow, fs, rc::Rc};

use crate::{audio::AudioControl, messanger_unifier::Call, pages::login::Message as LoginMessage};
use adaptors::SocketEvent;
use auth::MessangersGenerator;
use font_kit::{family_name::FamilyName, source::SystemSource};
// use fontconfig::Fontconfig;
use futures::{Stream, StreamExt, channel::mpsc::Sender, future::join_all, try_join};
use iced::{Element, Font, Subscription, Task, window};
use messanger_unifier::Messangers;
use pages::{AppMessage, Login, messenger::Messenger};
use socket::{ReceiverEvent, SocketsInterface};

use crate::messanger_unifier::MessangerHandle;

mod audio;
mod auth;
mod components;
mod messanger_unifier;
mod pages;
mod socket;

use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

#[derive(Debug)]
pub enum Screen {
    Loading,
    Login(Login),
    Chat(Messenger),
}

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    // init logger
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
    // ETC
    let audio = AudioControl::new();

    let mut app = App::new(Messangers::default(), Screen::Loading, audio);

    let is_loading =
        match MessangersGenerator::messengers_from_file("./LoginInfo".into(), &app.audio) {
            Ok(messangers) => {
                if messangers.len() > 0 {
                    app.messangers = messangers;
                    true
                } else {
                    false
                }
            }
            Err(err) => {
                // TODO: This will just freeze aplication on loading screen.
                eprintln!("{err}");
                app.page = Screen::Login(Login::default());
                false
            }
        };

    let fonts_handle = SystemSource::new()
        .select_best_match(&[FamilyName::SansSerif], &Default::default())
        .unwrap();
    let font_data = fonts_handle.load().unwrap().copy_font_data().unwrap();
    let sytem_font = font_data.as_ref().clone().leak();

    iced::daemon(App::title(), App::update, App::view)
        .settings(iced::Settings {
            fonts: vec![Cow::Borrowed(sytem_font)],
            ..Default::default()
        })
        .subscription(App::subscription)
        .run_with(move || {
            let (_window_id, window_task) = window::open(window::Settings::default());

            (
                app,
                Task::batch(vec![window_task.then(move |_| match is_loading {
                    true => Task::done(AppMessage::StartUp),
                    false => Task::none(),
                })]),
            )
        })
        .inspect_err(|err| println!("{err}"))?;

    Ok(())
}

struct App {
    audio: AudioControl,
    page: Screen,
    messangers: Messangers,
    socket_sender: Option<Sender<ReceiverEvent>>,
}

impl App {
    fn new(messangers: Messangers, page: Screen, audio: AudioControl) -> Self {
        Self {
            audio,
            page,
            messangers,
            socket_sender: None,
        }
    }

    fn title() -> &'static str {
        "record"
    }
    fn update(&mut self, message: AppMessage) -> Task<AppMessage> {
        match message {
            AppMessage::SaveMessengersCredentialToDisk => {
                MessangersGenerator::messangers_to_file(&self.messangers, "./LoginInfo".into());
                Task::none()
            }
            AppMessage::RemoveMessanger(handle) => {
                self.messangers.remove_by_handle(handle);
                Task::none()
            }
            AppMessage::SetMessangerData {
                messanger_handle,
                new_data,
            } => {
                let data = self
                    .messangers
                    .mut_data_from_handle(messanger_handle)
                    .unwrap();
                match new_data {
                    pages::MessangerData::Call(call_status) => data.calls.push(call_status),
                    pages::MessangerData::Everything {
                        profile,
                        contacts,
                        conversations,
                        servers,
                    } => {
                        data.profile = Some(profile);
                        data.contacts = contacts;
                        data.conversations = conversations;
                        data.guilds = servers;
                    }
                    pages::MessangerData::Chat((k, v)) => {
                        data.chats.insert(k.clone(), v);
                    }
                };
                Task::none()
            }
            AppMessage::RemoveMessangerData {
                messanger_handle,
                data_type,
                data_id,
            } => {
                let data = self
                    .messangers
                    .mut_data_from_handle(messanger_handle)
                    .unwrap();
                match data_type {
                    pages::MessangerDataType::Call => {
                        data.calls.retain(|call| call.id() != data_id);
                    }
                };

                Task::none()
            }
            AppMessage::StartUp => {
                Task::future(join_all(
                    self.messangers
                        .interface_iter()
                        .map(|interface| (interface.handle, interface.api.to_owned()))
                        .map(|(handle, api)| async move {
                            let Some(q) = api.query() else {
                                return Ok(None);
                            };

                            let (profile, conversations, contacts, servers) = match try_join!(
                                q.fetch_profile(),
                                q.fetch_conversation(),
                                q.fetch_contacts(),
                                q.fetch_guilds()
                            ) {
                                Ok(t) => t,
                                Err(e) => {
                                    return Err((handle, e));
                                }
                            };

                            Ok(Some((handle, profile, contacts, conversations, servers)))
                        }),
                ))
                .then(|outputs| {
                    if !outputs.iter().any(|m| m.is_ok()) {
                        // In case we are running this from login screen. If
                        // we are not there this would be equivalent of Task::none()

                        // TODO: Make it also clear all messengers
                        // TODO: This might potentially get us stuck on loading screen
                        return Task::done(AppMessage::Login(LoginMessage::ToggleButtonState));
                    };

                    let tasks_itr = outputs.into_iter().map(|m| {
                        let m = match m {
                            Ok(m) => m,
                            Err((handle, e)) => {
                                eprintln!("Failed to fetch the data: {e}");
                                return Task::done(AppMessage::RemoveMessanger(handle));
                            }
                        };
                        let (handle, profile, contacts, conversations, servers) = m.unwrap();

                        Task::done(AppMessage::SetMessangerData {
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
                        .chain(Task::done(AppMessage::OpenPage(Screen::Chat(
                            Messenger::new(),
                        ))))
                        .chain(Task::done(AppMessage::SaveMessengersCredentialToDisk))
                })
            }
            // Global Actions
            AppMessage::OpenPage(page) => {
                self.page = page;
                Task::none()
            }
            AppMessage::SocketEvent(event) => match event {
                SocketMesg::Connect(socket_connection) => {
                    self.socket_sender = Some(socket_connection.clone());
                    Task::batch(self.messangers.interface_iter().map(|interface| {
                        let interface = interface.to_owned();
                        let mut socket_connection = socket_connection.clone();
                        Task::future(async move {
                            socket_connection
                                .try_send(ReceiverEvent::Connection((
                                    interface.handle,
                                    interface.api.socket().await,
                                )))
                                .unwrap();
                        })
                        .then(|_| Task::none())
                    }))
                }
                SocketMesg::Message((handle, socket_event)) => {
                    match socket_event {
                        SocketEvent::Skip => println!("Skipped"),
                        SocketEvent::MessageCreated { channel, msg } => {
                            let d = self.messangers.mut_data_from_handle(handle).unwrap();
                            match d.chats.get_mut(&channel) {
                                Some(msgs) => msgs.push(msg),
                                None => {
                                    d.chats.insert(channel, vec![msg]);
                                }
                            };
                        }
                        SocketEvent::ChannelCreated { server, channel } => {
                            todo!()
                        }
                        SocketEvent::Disconnected => println!("Disconnected"),
                    };
                    Task::none()
                }
            },
            // ====== Pages ======
            AppMessage::Login(message) => {
                let Screen::Login(login) = &mut self.page else {
                    return Task::none();
                };
                match login.update(message, self.audio.get_sender()) {
                    pages::login::Action::None => Task::none(),
                    pages::login::Action::Login(messenger) => {
                        let handle = self.messangers.add_messanger(messenger);
                        let interface = self.messangers.interface_from_handle(handle).unwrap();
                        let api = interface.api.clone();
                        let mut sender = self.socket_sender.clone().unwrap();
                        Task::perform(
                            async move {
                                sender
                                    .try_send(ReceiverEvent::Connection((
                                        handle,
                                        api.socket().await,
                                    )))
                                    .unwrap();
                            },
                            |_| AppMessage::StartUp,
                        )
                    }
                }
            }
            AppMessage::Chat(message) => {
                let Screen::Chat(chat) = &mut self.page else {
                    return Task::none();
                };
                match chat.update(message, &self.messangers) {
                    pages::messenger::Action::None => Task::none(),
                    pages::messenger::Action::UpdateChat { handle, kv } => {
                        Task::done(AppMessage::SetMessangerData {
                            messanger_handle: handle,
                            new_data: pages::MessangerData::Chat(kv),
                        })
                    }
                    pages::messenger::Action::Call { interface, channel } => {
                        let api = interface.api.to_owned();

                        Task::future(async move {
                            let vc = api.vc();
                            vc.unwrap().connect(&channel).await;
                            channel
                        })
                        .then(move |channel| {
                            Task::done(AppMessage::SetMessangerData {
                                messanger_handle: interface.handle,
                                new_data: pages::MessangerData::Call(Call::new(
                                    interface.handle,
                                    channel,
                                )),
                            })
                        })
                    }
                    pages::messenger::Action::DisconnectFromCall(call) => {
                        let interface = self
                            .messangers
                            .interface_from_handle(call.handle())
                            .unwrap();

                        let api = interface.api.to_owned();
                        Task::future(async move {
                            let vc = api.vc();
                            vc.unwrap().disconnect(call.source()).await;
                            call
                        })
                        .then(move |call| {
                            println!("TODO: DISCONNECT CALL");
                            Task::done(AppMessage::RemoveMessangerData {
                                messanger_handle: call.handle(),
                                data_type: pages::MessangerDataType::Call,
                                data_id: call.id(),
                            })
                        })
                    }
                    pages::messenger::Action::Run(task) => task.map(AppMessage::Chat),
                }
            }
        }
    }
    fn view<'a>(&'a self, _window: window::Id) -> Element<'a, AppMessage> {
        match &self.page {
            Screen::Login(login) => login.view().map(AppMessage::Login),
            Screen::Chat(chat) => chat.view(&self.messangers).map(AppMessage::Chat),
            Screen::Loading => iced::widget::text("Loading").into(),
        }
    }
    fn subscription(&self) -> Subscription<AppMessage> {
        Subscription::run(spawn_sockets_interface).map(AppMessage::SocketEvent)
    }
}

#[derive(Debug)]
enum SocketMesg {
    Connect(Sender<ReceiverEvent>),
    Message((MessangerHandle, SocketEvent)),
}

fn spawn_sockets_interface() -> impl Stream<Item = SocketMesg> {
    iced::stream::channel(128, |mut output| async move {
        let (mut interface, sender) = SocketsInterface::new();
        output.try_send(SocketMesg::Connect(sender)).unwrap();
        loop {
            let msg = interface.next().await.unwrap();
            output.try_send(SocketMesg::Message(msg)).unwrap();
        }
    })
}
