use std::borrow::Cow;

use auth::MessengersGenerator;
use font_kit::{family_name::FamilyName, source::SystemSource};
use futures::{StreamExt, future::join_all, join};
use iced::{Element, Subscription, Task, window};
use messenger_interface::interface::{CallState, CallStatus};
use pages::{AppMessage, StreamDirection, login, messenger};
use simple_audio_channels::AudioMixer;
use state::MessengerRegistry;

mod auth;
mod components;
mod events;
mod pages;
mod state;

use tracing::{error, trace, warn};
use tracing_subscriber::FmtSubscriber;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Loading,
    Login,
    Messenger,
}

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .without_time()
        .with_line_number(true)
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "record_v2=trace,discord=trace,info",
        ))
        .with_ansi(true)
        .with_ansi_sanitization(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let fonts_handle = SystemSource::new()
        .select_best_match(&[FamilyName::SansSerif], &Default::default())
        .unwrap();
    let font_data = fonts_handle.load().unwrap().copy_font_data().unwrap();
    let system_font = font_data.as_ref().clone().leak();

    iced::daemon(App::boot, App::update, App::view)
        .settings(iced::Settings {
            fonts: vec![Cow::Borrowed(system_font)],
            ..Default::default()
        })
        .subscription(App::subscription)
        .run()
        .inspect_err(|err| error!("{err}"))?;
    Ok(())
}

struct App {
    // Navigation state is separate from page state
    screen: Screen,
    // Page states persist independently of which screen is active
    login: login::Login,
    messenger: messenger::Messenger,
    // Subsystems
    audio: AudioMixer,
    messengers: MessengerRegistry,
}

impl App {
    fn boot() -> (Self, Task<AppMessage>) {
        let mut messengers = MessengerRegistry::default();
        let mut screen = Screen::Loading;

        let has_credentials = match MessengersGenerator::load("./LoginInfo".into(), &mut messengers)
        {
            Ok(loaded) if loaded => true,
            Ok(_) => {
                trace!("No messengers were found");
                screen = Screen::Login;
                false
            }
            Err(err) => {
                error!("{err}");
                screen = Screen::Login;
                false
            }
        };

        let app = Self {
            screen,
            login: login::Login::default(),
            messenger: messenger::Messenger::new(),
            audio: AudioMixer::default(),
            messengers,
        };

        let (_window_id, window_task) = window::open(window::Settings::default());
        (
            app,
            window_task.then(move |_| {
                if has_credentials {
                    Task::done(AppMessage::StartUp)
                } else {
                    Task::none()
                }
            }),
        )
    }

    fn update(&mut self, message: AppMessage) -> Task<AppMessage> {
        match message {
            // === Navigation ===
            AppMessage::Navigate(screen) => {
                self.screen = screen;
                Task::none()
            }
            // === Credentials ===
            AppMessage::SaveCredentials => {
                MessengersGenerator::save(&self.messengers, "./LoginInfo".into());
                Task::none()
            }
            // === Messenger Data ===
            AppMessage::ModifyMessengers(modify) => {
                modify(&mut self.messengers);
                Task::none()
            }
            // === Startup ===
            AppMessage::StartUp => {
                Task::future(join_all(self.messengers.iter().map(|(id, entry)| {
                    let api = entry.interface.api.clone();
                    async move {
                        let Ok(q) = api.clone().arc_query() else {
                            error!("Query not impl");
                            return None;
                        };
                        let Ok(t) = api.clone().arc_text() else {
                            error!("Text not impl");
                            return None;
                        };
                        let Ok(v) = api.clone().arc_voice() else {
                            error!("Voice not impl");
                            return None;
                        };

                        let (profile, contacts, conversations, servers) =
                            join!(q.client_user(), q.contacts(), q.rooms(), q.houses());

                        let profile = match profile {
                            Ok(p) => p,
                            Err(err) => {
                                error!("Failed to fetch profile: {err:#?}");
                                return None;
                            }
                        };

                        Some((
                            id,
                            profile,
                            contacts.unwrap_or_default(),
                            conversations.unwrap_or_default(),
                            servers.unwrap_or_default(),
                            q.listen().await,
                            t.listen().await,
                            v.listen().await,
                        ))
                    }
                })))
                .then(|outputs| {
                    let tasks_itr = outputs.into_iter().filter_map(|m| {
                        let (
                            id,
                            profile,
                            contacts,
                            conversations,
                            servers,
                            query_socket,
                            text_socket,
                            voice_socket,
                        ) = m?;

                        let task = Task::done(AppMessage::modify_data(id, move |data| {
                            data.profile = Some(profile);
                            data.contacts = contacts;
                            data.conversations = conversations;
                            data.guilds = servers;
                        }));

                        let mut streams = Vec::new();

                        if let Ok(socket) = query_socket {
                            streams.push(Task::stream(
                                socket.map(move |event| AppMessage::QueryEvent((id, event))),
                            ));
                        }
                        if let Ok(socket) = text_socket {
                            streams.push(Task::stream(
                                socket.map(move |event| AppMessage::TextEvent((id, event))),
                            ));
                        }
                        if let Ok(socket) = voice_socket {
                            streams.push(Task::stream(
                                socket.map(move |event| AppMessage::VoiceEvent((id, event))),
                            ));
                        }

                        Some(task.chain(Task::batch(streams)))
                    });

                    Task::done(AppMessage::Navigate(Screen::Messenger))
                        .chain(Task::done(AppMessage::SaveCredentials))
                        .chain(Task::batch(tasks_itr))
                })
            }
            // === Socket Events (delegated) ===
            AppMessage::QueryEvent((id, event)) => {
                events::process_query_event(id, event, &mut self.messengers)
            }
            AppMessage::TextEvent((id, event)) => {
                events::process_text_event(id, event, &mut self.messengers)
            }
            AppMessage::VoiceEvent((id, event)) => {
                events::process_voice_event(id, event, &mut self.messengers)
            }
            AppMessage::AudioEvent((id, event)) => {
                events::process_audio_event(id, event, &mut self.audio)
            }
            // === Audio ===
            AppMessage::StartStream(dir) => {
                let result = match dir {
                    StreamDirection::Input => self.audio.start_stream_input(),
                    StreamDirection::Output => self.audio.start_stream_output(),
                };
                match result {
                    Ok(Some(notify)) => Task::future(async move { notify.notified().await })
                        .then(move |_| Task::done(AppMessage::StopStream(dir))),
                    Ok(None) => {
                        error!("No {dir} device available");
                        Task::none()
                    }
                    Err(err) => {
                        error!("Failed to start {dir} stream: {err}");
                        Task::none()
                    }
                }
            }
            AppMessage::StopStream(dir) => {
                match dir {
                    StreamDirection::Input => self.audio.stop_stream_input(),
                    StreamDirection::Output => self.audio.stop_stream_output(),
                };
                Task::none()
            }
            // === Pages ===
            AppMessage::Login(message) => match self.login.update(message) {
                login::Action::None => Task::none(),
                login::Action::Login(api) => {
                    Task::done(AppMessage::modify_messengers(move |messengers| {
                        messengers.add(api);
                    }))
                    .chain(Task::done(AppMessage::StartUp))
                }
            },
            AppMessage::Chat(message) => match self.messenger.update(message, &self.messengers) {
                messenger::Action::None => Task::none(),
                messenger::Action::Run(task) => task.map(AppMessage::Chat),
                messenger::Action::ModifyMessengerData { id, modify } => {
                    Task::done(AppMessage::modify_data(id, modify))
                }
                messenger::Action::Call { interface, channel } => {
                    let api = interface.api.clone();
                    let messenger_id = interface.id;

                    Task::future(async move {
                        let vc = match api.voice() {
                            Ok(vc) => vc,
                            Err(err) => {
                                warn!("{err:?}");
                                return Task::none();
                            }
                        };
                        let room_id = channel.swap_data((*channel).clone());
                        let status = match vc.connect(&room_id).await {
                            Ok(status) => status,
                            Err(err) => {
                                // TODO: Remove UI after a little while maybe?
                                error!("{err}");
                                CallStatus::Failed
                            }
                        };

                        Task::done(AppMessage::modify_data(messenger_id, move |data| {
                            data.calls.push(state::Call::new(
                                messenger_id,
                                channel,
                                CallState::Pending(status),
                            ));
                        }))
                    })
                    .then(|task| task)
                }
                messenger::Action::DisconnectFromCall(call) => {
                    let Some(entry) = self.messengers.get(call.messenger_id()) else {
                        return Task::none();
                    };
                    let api = entry.interface.api.clone();

                    Task::future(async move {
                        match api.voice() {
                            Ok(vc) => {
                                let room_id = call.source().swap_data((**call.source()).clone());
                                vc.disconnect(&room_id).await;
                            }
                            Err(err) => warn!("Voice not supported: {err:#?}"),
                        }
                        call
                    })
                    .then(|call| {
                        let call_id = call.id();
                        Task::done(AppMessage::modify_data(call.messenger_id(), move |data| {
                            data.calls.retain(|c| c.id() != call_id);
                        }))
                    })
                }
            },
        }
    }

    fn view(&self, _window: window::Id) -> Element<'_, AppMessage> {
        match self.screen {
            Screen::Loading => iced::widget::text("Loading").into(),
            Screen::Login => self.login.view().map(AppMessage::Login),
            Screen::Messenger => self.messenger.view(&self.messengers).map(AppMessage::Chat),
        }
    }

    fn subscription(&self) -> Subscription<AppMessage> {
        Subscription::none()
    }
}
