use std::borrow::Cow;

use crate::messanger_unifier::Call;
use auth::MessangersGenerator;
use font_kit::{family_name::FamilyName, source::SystemSource};
use futures::{Stream, StreamExt, future::join_all, join};
use iced::{Element, Subscription, Task, window};
use messanger_unifier::Messangers;
use messenger_interface::interface::{SocketEvent, WeakSocketStream};
use pages::{AppMessage, Login, messenger::Messenger};
use simple_audio_channels::{AudioMixer, SampleFormat};

use crate::messanger_unifier::MessangerHandle;

mod auth;
mod components;
mod messanger_unifier;
mod pages;

use tracing::{Level, error, info, trace, warn};
use tracing_subscriber::FmtSubscriber;

pub enum Screen {
    Loading,
    Login(Login),
    Chat(Messenger),
}

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    // init logger
    let subscriber = FmtSubscriber::builder()
        .without_time()
        .with_line_number(true)
        .with_max_level(Level::INFO)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    // Load font
    let fonts_handle = SystemSource::new()
        .select_best_match(&[FamilyName::SansSerif], &Default::default())
        .unwrap();
    let font_data = fonts_handle.load().unwrap().copy_font_data().unwrap();
    let sytem_font = font_data.as_ref().clone().leak();

    // Start GUI
    iced::daemon(App::boot, App::update, App::view)
        .settings(iced::Settings {
            fonts: vec![Cow::Borrowed(sytem_font)],
            ..Default::default()
        })
        .subscription(App::subscription)
        .run()
        .inspect_err(|err| error!("{err}"))?;
    Ok(())
}

struct App {
    audio: AudioMixer,
    page: Screen,
    messangers: Messangers,
}

impl App {
    fn new(messangers: Messangers, page: Screen) -> Self {
        Self {
            audio: AudioMixer::default(),
            page,
            messangers,
            // socket_sender: None,
        }
    }

    fn boot() -> (Self, Task<AppMessage>) {
        let mut app = App::new(Messangers::default(), Screen::Loading);

        let messangers = MessangersGenerator::messengers_from_file("./LoginInfo".into());

        match messangers {
            Ok(messangers) => {
                if messangers.len() > 0 {
                    app.messangers = messangers;
                    true
                } else {
                    false
                };
            }
            Err(err) => {
                error!("{err:#?}");
            }
        };

        let loaded_messangers =
            match MessangersGenerator::messengers_from_file("./LoginInfo".into()) {
                Ok(messangers) => {
                    if messangers.len() > 0 {
                        app.messangers = messangers;
                        true
                    } else {
                        trace!("No massengers were found");
                        app.page = Screen::Login(Login::default());
                        false
                    }
                }
                Err(err) => {
                    error!("{err}");
                    app.page = Screen::Login(Login::default());
                    false
                }
            };

        let (_window_id, window_task) = window::open(window::Settings::default());
        (
            app,
            window_task.then(move |_| match loaded_messangers {
                true => Task::done(AppMessage::StartUp),
                false => Task::none(),
            }),
        )
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
                    pages::MessangerData::Chat((id, v)) => {
                        data.chats.insert(id, v);
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
                        .map(async |(handle, api)| {
                            // Query
                            let Ok(q) = api.query() else {
                                error!("Query not impl");
                                return None;
                            };

                            let (profile, contacts, conversations, servers) =
                                join!(q.client_user(), q.contacts(), q.rooms(), q.houses());

                            let profile = match profile {
                                Ok(profile) => profile,
                                Err(err) => {
                                    panic!("TODO: {err:#?}");
                                }
                            };

                            // let Ok(query_socket) = q.listen().await else {
                            //     error!("Problem with socket starting");
                            //     return None;
                            // };

                            // Text
                            let Ok(t) = api.text() else {
                                error!("Text not impl");
                                return None;
                            };
                            let Ok(v) = api.voice() else {
                                error!("Text not impl");
                                return None;
                            };

                            // let (conversations, servers) =
                            //     places.unwrap_or_default().into_iter().fold(
                            //         (Vec::new(), Vec::new()),
                            //         |(mut conversations, mut servers), place| {
                            //             match &*place {
                            //                 Place::Room(room) => {
                            //                     conversations.push(place.swap_data(room.to_owned()))
                            //                 }
                            //                 Place::House(house) => {
                            //                     servers.push(place.swap_data(house.to_owned()))
                            //                 }
                            //             };
                            //             (conversations, servers)
                            //         },
                            //     );

                            // let conversations: Vec<Identifier<Room>> = room_places
                            //     .unwrap_or_default()
                            //     .into_iter()
                            //     .filter_map(|place| match &*place {
                            //         Place::Room(room) => {
                            //             Some(Identifier::new(*place.id(), room.clone()))
                            //         }
                            //         _ => None,
                            //     })
                            //     .collect();

                            // let servers: Vec<Identifier<Place>> =
                            //     house_places.unwrap_or_default();

                            Some((
                                handle,
                                profile,
                                contacts.unwrap_or_default(),
                                conversations.unwrap_or_default(),
                                servers.unwrap_or_default(),
                                q.listen().await,
                                t.listen().await,
                                v.listen().await,
                            ))
                        }),
                ))
                .then(|outputs| {
                    // if !outputs.iter().any(|m| m.is_ok()) {
                    //     // In case we are running this from login screen. If
                    //     // we are not there this would be equivalent of Task::none()

                    //     // TODO: Make it also clear all messengers
                    //     // TODO: This might potentially get us stuck on loading screen
                    //     // return Task::done(AppMessage::Login(LoginMessage::ToggleButtonState));
                    // };

                    let tasks_itr = outputs.into_iter().map(|m| {
                        // let m = match m {
                        //     Ok(m) => m,
                        //     Err((handle, e)) => {
                        //         error!("Failed to fetch the data: {e}");
                        //         return Task::done(AppMessage::RemoveMessanger(handle));
                        //     }
                        // };
                        let (
                            handle,
                            profile,
                            contacts,
                            conversations,
                            servers,
                            query_socket,
                            text_socket,
                            voice_socket,
                        ) = m.unwrap();

                        let task = Task::done(AppMessage::SetMessangerData {
                            messanger_handle: handle,
                            new_data: pages::MessangerData::Everything {
                                profile,
                                contacts,
                                conversations,
                                servers,
                            },
                        });

                        let mut streams = Vec::new();

                        if let Ok(socket) = query_socket {
                            streams.push(Task::stream(socket.map(move |event| {
                                AppMessage::SocketEvent((handle, event.into()))
                            })));
                        };
                        if let Ok(socket) = text_socket {
                            streams.push(Task::stream(socket.map(move |event| {
                                AppMessage::SocketEvent((handle, event.into()))
                            })));
                        };
                        if let Ok(socket) = voice_socket {
                            streams.push(Task::stream(socket.map(move |event| {
                                AppMessage::SocketEvent((handle, event.into()))
                            })));
                        };

                        task.chain(Task::batch(streams))
                    });

                    Task::done(AppMessage::OpenPage(Screen::Chat(Messenger::new())))
                        .chain(Task::done(AppMessage::SaveMessengersCredentialToDisk))
                        .chain(Task::batch(tasks_itr))

                    // Task::batch(tasks_itr)
                    //     .chain(Task::done(AppMessage::OpenPage(Screen::Chat(
                    //         Messenger::new(),
                    //     ))))
                    //     .chain(Task::done(AppMessage::SaveMessengersCredentialToDisk))
                })
            }
            // Global Actions
            AppMessage::OpenPage(page) => {
                self.page = page;
                Task::none()
            }
            AppMessage::SocketEvent((handle, socket_event)) => {
                match socket_event {
                    SocketEvent::Skip => info!("Skipped"),
                    SocketEvent::MessageCreated {
                        room: channel,
                        message: msg,
                    } => {
                        let d = self.messangers.mut_data_from_handle(handle).unwrap();
                        match d.chats.get_mut(&channel.id()) {
                            Some(msgs) => msgs.push(msg),
                            None => {
                                d.chats.insert(*channel.id(), vec![msg]);
                            }
                        };
                    }
                    SocketEvent::ChannelCreated { r#where, room } => {
                        let d = self.messangers.mut_data_from_handle(handle).unwrap();

                        match r#where {
                            None => {
                                // It's a DM or group DM, add to conversations
                                // if !d.conversations.iter().any(|r| r.id() == room.id()) {
                                //     d.conversations.push(room);
                                // }
                            }
                            Some(server_id) => {
                                info!("Adding: {server_id:?}");
                                // It's a channel in a server, add to the server's rooms
                                // if let Some(server_identifier) =
                                //     d.guilds.iter_mut().find(|g| g.id() == server_id.id())
                                // {
                                //     let mut house = (**server_identifier).clone();
                                //     if !house.rooms.iter().any(|r| r.id() == room.id()) {
                                //         house.rooms.push(room);
                                //         *server_identifier = server_identifier.swap_data(house);
                                //     }
                                // }
                            }
                        }
                    }
                    SocketEvent::CallStatusUpdate(call_status) => match call_status {
                        messenger_interface::interface::CallStatus::Connected(
                            weak_socket_stream,
                        ) => {
                            return Task::stream(weak_socket_stream.map(move |event| {
                                AppMessage::SocketEvent((handle, event.into()))
                            }));
                        }
                        messenger_interface::interface::CallStatus::Connecting(msg) => {
                            info!("{msg}")
                        }
                        messenger_interface::interface::CallStatus::Failed => error!("TODO"),
                    },
                    SocketEvent::Disconnected => info!("Disconnected"),
                    SocketEvent::AddAudioSource(sender) => {
                        let producer = self
                            .audio
                            .create_output_channel(2, SampleFormat::I16, 48_000)
                            .unwrap();

                        if sender.send(producer).is_err() {
                            warn!("Couldn't send audio channel to the adapter");
                        };

                        if !self.audio.is_streaming_output() {
                            return Task::done(AppMessage::StartOutputStream);
                        }
                    }
                    SocketEvent::AddAudioInput(sender) => {
                        let input = self
                            .audio
                            .create_input_channel(2, SampleFormat::I16, 48_000)
                            .unwrap();

                        if sender.send(input).is_err() {
                            warn!("Couldn't send audio input channel to the adapter");
                        };

                        if !self.audio.is_streaming_input() {
                            return Task::done(AppMessage::StartInputStream);
                        }
                    }
                };
                Task::none()
            }
            AppMessage::StartOutputStream => {
                if let Some(notify) = self.audio.start_stream_output() {
                    return Task::future(async {
                        notify.await;
                    })
                    .then(|_| Task::done(AppMessage::StopOutputStream));
                } else {
                    // TODO: Remove this after making control flow simpler
                    error!("Stream is already running?");
                };

                Task::none()
            }
            AppMessage::StopOutputStream => {
                self.audio.stop_stream_output();
                Task::none()
            }
            AppMessage::StartInputStream => {
                if let Some(notify) = self.audio.start_stream_input() {
                    return Task::future(async {
                        notify.await;
                    })
                    .then(|_| Task::done(AppMessage::StopInputStream));
                } else {
                    // TODO: Remove this after making control flow simpler
                    error!("Input stream is already running?");
                };

                Task::none()
            }
            AppMessage::StopInputStream => {
                self.audio.stop_stream_input();
                Task::none()
            }
            // ====== Pages ======
            AppMessage::Login(message) => {
                let Screen::Login(login) = &mut self.page else {
                    return Task::none();
                };
                match login.update(message) {
                    pages::login::Action::None => Task::none(),
                    pages::login::Action::Login(messenger) => {
                        let handle = self.messangers.add_messanger(messenger);
                        let interface = self.messangers.interface_from_handle(handle).unwrap();
                        Task::done(AppMessage::StartUp)
                        // let mut sender = self.socket_sender.clone().unwrap();
                        // Task::perform(
                        //     async move {
                        //         sender
                        //             .try_send(ReceiverEvent::Connection((
                        //                 handle,
                        //                 api.socket().await,
                        //             )))
                        //             .unwrap();
                        //     },
                        //     |_| AppMessage::StartUp,
                        // )
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
                        let handle = interface.handle;

                        Task::future(async move {
                            let vc = api.voice();
                            let vc = match vc {
                                Ok(vc) => vc,
                                Err(err) => {
                                    warn!("{err:?}");
                                    return Task::none();
                                }
                            };
                            // Convert Place<Room> to Room for voice API
                            let room_id = channel.swap_data((*channel).clone());
                            vc.connect(&room_id).await;

                            // let vc_stream = match vc.listen().await {
                            //     Ok(stream) => Task::stream(stream.map(move |event| {
                            //         AppMessage::SocketEvent((handle, event.into()))
                            //     })),
                            //     Err(err) => {
                            //         error!("{err:?}");
                            //         Task::none()
                            //     }
                            // };

                            Task::done(AppMessage::SetMessangerData {
                                messanger_handle: interface.handle,
                                new_data: pages::MessangerData::Call(Call::new(
                                    interface.handle,
                                    channel,
                                )),
                            })
                            // .chain(vc_stream)
                        })
                        .then(|task| task)
                    }
                    pages::messenger::Action::DisconnectFromCall(call) => {
                        let interface = self
                            .messangers
                            .interface_from_handle(call.handle())
                            .unwrap();

                        let api = interface.api.to_owned();
                        Task::future(async move {
                            let vc = api.voice();
                            match vc {
                                Ok(vc) => {
                                    // Convert Place<Room> to Room for voice API
                                    let room_id =
                                        call.source().swap_data((**call.source()).clone());
                                    vc.disconnect(&room_id).await;
                                }
                                Err(err) => warn!("Voice not supported by adapter: {err:#?}"),
                            }
                            call
                        })
                        .then(move |call| {
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
        Subscription::none()
    }
}
