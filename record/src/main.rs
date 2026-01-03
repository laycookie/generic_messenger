use std::{
    borrow::Cow,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    task::{Context, Poll},
};

use crate::messanger_unifier::Call;
use audio::AudioMixer;
use auth::MessangersGenerator;
use font_kit::{family_name::FamilyName, source::SystemSource};
use futures::{Stream, StreamExt, future::join_all, join};
use iced::{Element, Subscription, Task, window};
use messaging_interface::interface::{Socket, SocketEvent};
use messanger_unifier::Messangers;
use pages::{AppMessage, Login, messenger::Messenger};
// use socket::{ReceiverEvent, SocketsInterface};

use crate::messanger_unifier::MessangerHandle;

mod auth;
mod components;
mod messanger_unifier;
mod pages;
// mod socket;

use tracing::{Level, error, info, trace};
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

/// A stream adapter that wraps a Weak<dyn Socket> and automatically stops
/// when the underlying Arc is dropped.
struct WeakSocketStream {
    socket: Weak<dyn Socket + Send + Sync>,
    handle: MessangerHandle,
    next_future: Option<Pin<Box<dyn std::future::Future<Output = Option<SocketEvent>> + Send>>>,
}

impl WeakSocketStream {
    fn new(socket: Weak<dyn Socket + Send + Sync>, handle: MessangerHandle) -> Self {
        Self {
            socket,
            handle,
            next_future: None,
        }
    }
}

impl Stream for WeakSocketStream {
    type Item = (MessangerHandle, SocketEvent);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Try to upgrade the Weak to Arc
        let socket_arc = match self.socket.upgrade() {
            Some(arc) => arc,
            None => {
                info!("Killed");
                // Arc was dropped, stream is finished
                return Poll::Ready(None);
            }
        };

        // Get or create the next future
        if self.next_future.is_none() {
            let socket_clone = socket_arc.clone();
            self.next_future = Some(Box::pin(async move { Socket::next(socket_clone).await }));
        }

        // Poll the future
        if let Some(ref mut fut) = self.next_future {
            match fut.as_mut().poll(cx) {
                Poll::Ready(Some(event)) => {
                    self.next_future = None;

                    // Skip SocketEvent::Skip events by immediately requesting the next one
                    if matches!(event, SocketEvent::Skip) {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    Poll::Ready(Some((self.handle, event)))
                }
                Poll::Ready(None) => {
                    // Underlying stream ended
                    Poll::Ready(None)
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Pending
        }
    }
}

struct App {
    audio: Arc<Mutex<AudioMixer>>,
    page: Screen,
    messangers: Messangers,
    // socket_sender: Option<Sender<ReceiverEvent>>,
}

impl App {
    fn new(messangers: Messangers, page: Screen) -> Self {
        Self {
            audio: Arc::new(Mutex::new(AudioMixer::new())),
            page,
            messangers,
            // socket_sender: None,
        }
    }

    fn boot() -> (Self, Task<AppMessage>) {
        let mut app = App::new(Messangers::default(), Screen::Loading);

        let messangers =
            MessangersGenerator::messengers_from_file("./LoginInfo".into(), &app.audio);

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
            match MessangersGenerator::messengers_from_file("./LoginInfo".into(), &app.audio) {
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
            Task::batch(vec![window_task.then(move |_| match loaded_messangers {
                true => Task::done(AppMessage::StartUp),
                false => Task::none(),
            })]),
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
                        .map(async |(handle, api)| {
                            let Ok(q) = api.query() else {
                                error!("Query not impl");
                                return None;
                            };

                            // let Ok(socket) = api.socket().await else {
                            //     error!("Problem with socket starting");
                            //     return None;
                            // };

                            let (profile, conversations, contacts, servers) = join!(
                                q.fetch_profile(),
                                q.fetch_conversation(),
                                q.fetch_contacts(),
                                q.fetch_guilds()
                            );

                            let profile = match profile {
                                Ok(profile) => profile,
                                Err(err) => {
                                    panic!("TODO: {err:#?}");
                                }
                            };

                            Some((
                                handle,
                                profile,
                                contacts,
                                conversations,
                                servers,
                                api.socket().await,
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
                        let (handle, profile, contacts, conversations, servers, socket) =
                            m.unwrap();

                        let mut task = Task::done(AppMessage::SetMessangerData {
                            messanger_handle: handle,
                            new_data: pages::MessangerData::Everything {
                                profile,
                                contacts: contacts.unwrap_or_default(),
                                conversations: conversations.unwrap_or_default(),
                                servers: servers.unwrap_or_default(),
                            },
                        });

                        if let Ok(socket) = socket {
                            let stream = WeakSocketStream::new(socket, handle);
                            task =
                                task.chain(Task::stream(stream.map(move |(handle, message)| {
                                    AppMessage::SocketEvent(SocketMesg::Message((handle, message)))
                                })));
                        };

                        task
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
            AppMessage::SocketEvent(event) => match event {
                // SocketMesg::Connect(socket_connection) => {
                //     self.socket_sender = Some(socket_connection.clone());
                //     Task::batch(self.messangers.interface_iter().map(|interface| {
                //         let interface = interface.to_owned();
                //         let mut socket_connection = socket_connection.clone();
                //         Task::future(async move {
                //             socket_connection
                //                 .try_send(ReceiverEvent::Connection((
                //                     interface.handle,
                //                     interface.api.socket().await,
                //                 )))
                //                 .unwrap();
                //         })
                //         .then(|_| Task::none())
                //     }))
                // }
                SocketMesg::Message((handle, socket_event)) => {
                    match socket_event {
                        SocketEvent::Skip => info!("Skipped"),
                        SocketEvent::MessageCreated { channel, msg } => {
                            let d = self.messangers.mut_data_from_handle(handle).unwrap();
                            match d.chats.get_mut(&channel) {
                                Some(msgs) => msgs.push(msg),
                                None => {
                                    d.chats.insert(channel, vec![msg]);
                                }
                            };
                        }
                        SocketEvent::ChannelCreated { .. } => {
                            todo!()
                        }
                        SocketEvent::Disconnected => info!("Disconnected"),
                    };
                    Task::none()
                }
            },
            // ====== Pages ======
            AppMessage::Login(message) => {
                let Screen::Login(login) = &mut self.page else {
                    return Task::none();
                };
                match login.update(message, &self.audio) {
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
        // Subscription::run(spawn_sockets_interface).map(AppMessage::SocketEvent)
    }
}

#[derive(Debug)]
enum SocketMesg {
    // Connect(Sender<ReceiverEvent>),
    Message((MessangerHandle, SocketEvent)),
}
//
// fn spawn_sockets_interface() -> impl Stream<Item = SocketMesg> {
//     iced::task::sipper(|mut output| async move {
//         let (mut interface, sender) = SocketsInterface::new();
//         output.send(SocketMesg::Connect(sender)).await;
//         loop {
//             let msg = interface.next().await.unwrap();
//             output.send(SocketMesg::Message(msg)).await;
//         }
//     })
// }
