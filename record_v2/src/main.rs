use std::borrow::Cow;

use audio_controls::AudioControls;
use font_kit::{family_name::FamilyName, source::SystemSource};
use futures::{StreamExt, future::join_all, join};
use iced::{Element, Subscription, Task, window};
use messenger_interface::auth::Credential;
use messenger_interface::interface::{CallState, CallStatus};
use messenger_unifier::MessengerRegistry;
use pages::{AppMessage, StreamDirection, login, messenger};
use simple_audio_channels::AudioMixer;

mod audio_controls;
mod components;
mod events;
mod pages;

use tracing::{debug, error, trace, warn};
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
        .with_env_filter(
            // Honor RUST_LOG when set (e.g. `steam=debug,steam_vent=debug` for
            // voice-signaling capture); otherwise keep the default verbosity.
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("record_v2=trace,discord=trace,info")
            }),
        )
        .with_ansi(true)
        .with_ansi_sanitization(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    // Reclaim any `.part` temp files orphaned by a previous hard kill. Safe to
    // run here since no downloads are in flight yet.
    messenger_interface::types::sweep_stale_temp_files();

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
    /// Live voice-channel handles (per-user volume gains, effect racks).
    audio_controls: AudioControls,
    messengers: MessengerRegistry,
}

impl App {
    fn boot() -> (Self, Task<AppMessage>) {
        // Read each saved `name:credential` line into a (backend, credential)
        // pair. Construction is async, so we only parse here and build the
        // messengers in the task below once the window is up.
        let stored: Vec<(String, Credential)> = login_store::load("./LoginInfo", |name, blob| {
            let backend = login::backend(name)?;
            let keys: Vec<&'static str> = (backend.auth_methods)()
                .iter()
                .flat_map(|method| method.fields.iter().map(|field| field.key))
                .collect();
            match login_store::from_line(blob, &keys) {
                Some(cred) => Some((backend.name.to_owned(), cred)),
                None => {
                    error!("Ignoring unparsable saved credentials for {name}");
                    None
                }
            }
        })
        .unwrap_or_else(|err| {
            error!("{err}");
            Vec::new()
        });

        if stored.is_empty() {
            trace!("No saved credentials were found");
        }
        let has_credentials = !stored.is_empty();

        let app = Self {
            screen: if has_credentials {
                Screen::Loading
            } else {
                Screen::Login
            },
            login: login::Login::default(),
            messenger: messenger::Messenger::new(),
            audio: AudioMixer::default(),
            audio_controls: AudioControls::default(),
            messengers: MessengerRegistry::default(),
        };

        let (_window_id, window_task) = window::open(window::Settings::default());
        (
            app,
            window_task.then(move |_| {
                if stored.is_empty() {
                    return Task::none();
                }
                let stored = stored.clone();
                Task::future(async move {
                    let mut built = Vec::new();
                    for (name, cred) in stored {
                        match login::backend(&name) {
                            Some(backend) => match (backend.create)(cred).await {
                                Ok(api) => built.push(api),
                                Err(err) => error!("Failed to restore {name} session: {err}"),
                            },
                            None => warn!("Unknown backend in LoginInfo: {name}"),
                        }
                    }
                    built
                })
                .then(|built| {
                    Task::done(AppMessage::modify_messengers(move |messengers| {
                        for api in built {
                            messengers.add(api);
                        }
                    }))
                    .chain(Task::done(AppMessage::StartUp))
                })
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
                if let Err(err) = login_store::save(
                    "./LoginInfo",
                    self.messengers.iter().map(|(_, entry)| {
                        (
                            entry.interface.name(),
                            login_store::to_line(&entry.interface.auth()),
                        )
                    }),
                ) {
                    error!("{err}");
                }
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
                    let user_statuses = entry.interface.user_statuses.clone();
                    async move {
                        let Ok(q) = api.clone().arc_query() else {
                            error!("Query not impl");
                            return None;
                        };
                        let Ok(t) = api.clone().arc_text() else {
                            error!("Text not impl");
                            return None;
                        };
                        // Voice is optional: messengers without it (e.g. Steam)
                        // must still load. Query/Text above remain required.
                        let voice = api.clone().arc_voice().ok();

                        let (profile, contacts, conversations, servers) =
                            join!(q.client_user(), q.contacts(), q.rooms(), q.houses());

                        let profile = match profile {
                            Ok(p) => Some(p),
                            Err(err) => {
                                error!("Failed to fetch profile: {err:#?}");
                                None
                            }
                        };

                        let contacts = contacts.unwrap_or_default();
                        let conversations = conversations.unwrap_or_default();
                        let servers = servers.unwrap_or_default();

                        let status_updates = match user_statuses {
                            Some(statuses) => statuses
                                .user_statuses(&contacts)
                                .await
                                .unwrap_or_else(|err| {
                                    debug!("Failed to fetch user statuses: {err}");
                                    Vec::new()
                                }),
                            None => Vec::new(),
                        };

                        Some((
                            id,
                            profile,
                            contacts,
                            conversations,
                            servers,
                            status_updates,
                            q.listen().await,
                            t.listen().await,
                            match voice {
                                Some(v) => v.listen().await,
                                None => Err("Voice not supported".into()),
                            },
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
                            status_updates,
                            query_socket,
                            text_socket,
                            voice_socket,
                        ) = m?;

                        let task = Task::done(AppMessage::modify_data(id, move |data| {
                            data.profile = profile;
                            data.contacts = contacts;
                            data.conversations = conversations;
                            data.guilds = servers;
                            for (user_id, status) in status_updates {
                                events::apply_user_status(data, user_id, status);
                            }
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
            AppMessage::VoiceEvent((id, event)) => events::process_voice_event(
                id,
                event,
                &mut self.messengers,
                &mut self.audio_controls,
            ),
            AppMessage::AudioEvent((id, event)) => {
                events::process_audio_event(id, event, &mut self.audio, &mut self.audio_controls)
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
                // The stream's channels are gone with it — drop the control
                // handles that pointed into them (stored volumes survive).
                match dir {
                    StreamDirection::Input => {
                        self.audio.stop_stream_input();
                        self.audio_controls.detach_mic();
                    }
                    StreamDirection::Output => {
                        self.audio.stop_stream_output();
                        self.audio_controls.detach_all_users();
                    }
                };
                Task::none()
            }
            // === Pages ===
            AppMessage::Login(message) => match self.login.update(message) {
                login::Action::None => Task::none(),
                login::Action::Login {
                    backend,
                    credential,
                } => {
                    // Build, then verify, before committing. create_messenger may
                    // perform an eager credential->token exchange that can fail;
                    // client_user then confirms the token/session actually works.
                    Task::future(async move {
                        let Some(backend) = login::backend(backend) else {
                            return Err(format!("Unknown backend: {backend}"));
                        };
                        let api = (backend.create)(credential)
                            .await
                            .map_err(|err| err.to_string())?;
                        if let Ok(query) = api.clone().arc_query()
                            && let Err(err) = query.client_user().await
                        {
                            return Err(err.to_string());
                        }
                        Ok(api)
                    })
                    .then(|result| match result {
                        Ok(api) => Task::done(AppMessage::modify_messengers(move |messengers| {
                            messengers.add(api);
                        }))
                        .chain(Task::done(AppMessage::StartUp)),
                        Err(err) => {
                            error!("Login failed: {err}");
                            Task::done(AppMessage::Login(login::Message::LoginFailed(err)))
                        }
                    })
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
                            data.calls.push(messenger_unifier::Call::new(
                                messenger_id,
                                channel,
                                CallState::Pending(status),
                            ));
                        }))
                    })
                    .then(|task| task)
                }
                messenger::Action::Audio(op) => {
                    self.audio_controls.apply(op);
                    Task::none()
                }
                messenger::Action::SetCallState { call, state } => {
                    debug!(
                        "Requesting call state: muted={} deafened={}",
                        state.muted, state.deafened
                    );
                    let Some(entry) = self.messengers.get(call.messenger_id()) else {
                        return Task::none();
                    };
                    let api = entry.interface.api.clone();

                    // Apply optimistically so the toggle takes effect (and the
                    // buttons repaint) immediately — otherwise a press landing
                    // before the previous echo would compute from stale state
                    // (a double-click could never toggle off). The echo then
                    // confirms, or corrects if the server disagrees.
                    self.audio_controls.set_mic_muted(state.muted);
                    self.audio_controls.set_deafened(state.deafened);

                    Task::future(async move {
                        match api.voice().and_then(|voice| voice.self_state()) {
                            Ok(voice) => {
                                let room_id = call.source().swap_data((**call.source()).clone());
                                if let Err(err) = voice.set_self_voice_state(&room_id, state).await
                                {
                                    error!("Failed to set voice state: {err}");
                                }
                            }
                            Err(err) => warn!("Setting call state not supported: {err:#?}"),
                        }
                    })
                    .discard()
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
            Screen::Messenger => self
                .messenger
                .view(&self.messengers, &self.audio_controls)
                .map(AppMessage::Chat),
        }
    }

    fn subscription(&self) -> Subscription<AppMessage> {
        Subscription::none()
    }
}
