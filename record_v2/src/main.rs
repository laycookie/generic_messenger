use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use async_lock::Mutex as AsyncMutex;
use audio_controls::AudioControls;
use font_kit::{family_name::FamilyName, source::SystemSource};
use futures::{StreamExt, future::join_all, join, stream};
use futures_timer::Delay;
use iced::{Element, Subscription, Task, task::Handle, window};
use messenger_interface::auth::{Credential, InteractiveAuthSession};
use messenger_interface::{
    interface::{
        CallState, CallStatus, Messenger, MessengerError, QueryEvent, TextEvent, VideoCodec,
        VoiceEvent,
    },
    owner::Owner,
};
use messenger_unifier::{MessengerId, MessengerRegistry};
use pages::{
    AppMessage, CallOperationKey, CredentialFailure, ListenerKind, RemoteVideo, RetainedCredential,
    SelectedVideoInput, StartupSnapshot, StreamDirection, VideoInputKey, VideoOperationKey, login,
    messenger, settings,
};
use simple_audio_channels::AudioMixer;
use zeroize::Zeroizing;

mod audio_controls;
mod components;
mod credential_store_path;
mod events;
mod pages;
mod video_timing;

use tracing::{debug, error, trace, warn};
use tracing_subscriber::{
    EnvFilter, Layer as _, filter::filter_fn, fmt::time::Uptime, layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

use crate::video_timing::VideoTimingFormatter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Loading,
    Login,
    Messenger,
    Settings,
}

/// Serializes one backend control lane and lets queued obsolete operations
/// exit before touching backend state. Generation changes remain lock-free on
/// iced's update thread.
struct OperationGate {
    generation: AtomicU64,
    lock: Arc<AsyncMutex<()>>,
}

/// Socket mutations received while initial REST snapshots are in flight.
/// Replayed synchronously after snapshot assignment so live state wins.
enum BufferedStartupEvent {
    Query(QueryEvent),
    Text(TextEvent),
    Voice(VoiceEvent),
}

const MAX_STARTUP_EVENT_BUFFER: usize = 8192;
const MAX_STARTUP_REQUEST_ATTEMPTS: u32 = 3;

async fn retry_startup_request<T, E, F, Fut>(
    label: &'static str,
    mut request: F,
) -> Result<T, String>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut retry_delay = Duration::from_secs(1);
    for attempt in 1..=MAX_STARTUP_REQUEST_ATTEMPTS {
        match request().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                warn!(request = label, "Startup fetch failed: {error}");
                if attempt == MAX_STARTUP_REQUEST_ATTEMPTS {
                    return Err(format!(
                        "{label} failed after {MAX_STARTUP_REQUEST_ATTEMPTS} attempts: {error}"
                    ));
                }
                Delay::new(retry_delay).await;
                retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(30));
            }
        }
    }
    unreachable!("startup retry loop always returns")
}

fn closed_messenger_error() -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::new(std::io::ErrorKind::NotConnected, "messenger is closed").into()
}

fn listener_error_is_retryable(error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    !matches!(
        error.downcast_ref::<MessengerError>(),
        Some(MessengerError::NotImplemented)
    )
}

type ParsedStoredCredential = (
    &'static login::Backend,
    String,
    Credential,
    Zeroizing<String>,
);

fn parse_stored_credential(
    name: &str,
    blob: &str,
) -> Result<ParsedStoredCredential, RetainedCredential> {
    let backend = login::backend(name).ok_or_else(|| {
        RetainedCredential::new(name, blob, CredentialFailure::BackendUnavailable)
    })?;
    let keys: Vec<&'static str> = (backend.auth_methods)()
        .iter()
        .flat_map(|method| method.fields.iter().map(|field| field.key))
        .collect();
    let credential = login_store::from_line(blob, &keys).ok_or_else(|| {
        RetainedCredential::new(name, blob, CredentialFailure::CredentialParseFailed)
    })?;
    Ok((
        backend,
        name.to_owned(),
        credential,
        Zeroizing::new(blob.to_owned()),
    ))
}

async fn create_verified_messenger(
    backend_name: &'static str,
    credential: Credential,
) -> Result<Owner<dyn Messenger>, String> {
    let Some(backend) = login::backend(backend_name) else {
        return Err(format!("Unknown backend: {backend_name}"));
    };
    let owner = (backend.create)(credential)
        .await
        .map_err(|error| error.to_string())?;
    let query = owner.get().query().map_err(|error| error.to_string())?;
    query
        .client_user()
        .await
        .map_err(|error| error.to_string())?;
    Ok(owner)
}

fn partition_restore_results<T>(
    results: impl IntoIterator<Item = Result<T, RetainedCredential>>,
) -> (Vec<T>, Vec<RetainedCredential>) {
    let mut restored = Vec::new();
    let mut retained = Vec::new();
    for result in results {
        match result {
            Ok(messenger) => restored.push(messenger),
            Err(credential) => retained.push(credential),
        }
    }
    (restored, retained)
}

impl Default for OperationGate {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            lock: Arc::new(AsyncMutex::new(())),
        }
    }
}

impl OperationGate {
    fn with_lock(lock: Arc<AsyncMutex<()>>) -> Self {
        Self {
            generation: AtomicU64::new(0),
            lock,
        }
    }

    fn begin(&self) -> u64 {
        self.generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    fn is_current(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
    }

    fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    let standard_logs = tracing_subscriber::fmt::layer()
        .with_timer(Uptime::default())
        .with_line_number(true)
        .with_ansi(true)
        .with_ansi_sanitization(true)
        .with_filter(filter_fn(|metadata| metadata.target() != "video_timing"));
    let video_logs = tracing_subscriber::fmt::layer()
        .event_format(VideoTimingFormatter::new())
        .with_ansi(true)
        .with_ansi_sanitization(true)
        .with_filter(filter_fn(|metadata| metadata.target() == "video_timing"));
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // Broad Discord TRACE formats several events per RTP packet and can
        // saturate a core, distorting media timing.
        EnvFilter::new("record_v2=debug,discord=info,info")
    });
    tracing_subscriber::registry()
        .with(env_filter)
        .with(standard_logs)
        .with(video_logs)
        .try_init()?;

    // Reclaim any `.part` temp files orphaned by a previous hard kill. Safe to
    // run here since no downloads are in flight yet.
    messenger_interface::types::sweep_stale_temp_files();
    // Keep one process-local janitor alive for daemon lifetime. Cleanup state
    // persists access times across launches; failure is non-fatal because cache
    // retention must never prevent messenger startup.
    let _cache_janitor = match messenger_interface::types::CacheJanitor::start(Default::default()) {
        Ok(janitor) => Some(janitor),
        Err(err) => {
            warn!("Couldn't start cache janitor: {err}");
            None
        }
    };

    let fonts = SystemSource::new()
        .select_best_match(&[FamilyName::SansSerif], &Default::default())
        .ok()
        .and_then(|handle| handle.load().ok())
        .and_then(|font| font.copy_font_data())
        .map(|data| Cow::Owned(data.as_ref().to_vec()))
        .into_iter()
        .collect();

    iced::daemon(App::boot, App::update, App::view)
        .settings(iced::Settings {
            // Iced's built-in font remains available when system discovery
            // fails (common in stripped containers and fresh installs).
            fonts,
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
    /// Current login operation. Method/platform changes abort its network flow;
    /// attempt id prevents already-queued stale completions from taking effect.
    login_task: Option<(u64, Handle)>,
    messenger: messenger::Messenger,
    settings_category: settings::Category,
    // Subsystems
    audio: AudioMixer,
    /// Live voice-channel handles (per-user volume gains, effect racks).
    audio_controls: AudioControls,
    /// Advertised remote streams and debug still previews.
    remote_video: RemoteVideo,
    /// Selected local source for each backend's outgoing camera/screen stream.
    video_inputs: HashMap<VideoInputKey, SelectedVideoInput>,
    /// Session-local preferred encoder for outgoing video.
    preferred_video_codec: VideoCodec,
    /// Backend control lanes. Commands for one call/source execute in UI
    /// order; obsolete queued commands leave without touching backend state.
    call_operations: HashMap<CallOperationKey, Arc<OperationGate>>,
    voice_state_operations: HashMap<CallOperationKey, Arc<OperationGate>>,
    video_operations: HashMap<VideoOperationKey, Arc<OperationGate>>,
    messengers: MessengerRegistry,
    /// Backends whose initial REST snapshot was already scheduled. Login and
    /// listener recovery call `StartUp` again; snapshots remain single-shot.
    started_messengers: HashSet<MessengerId>,
    /// Active listener routes, independently recovered.
    active_listeners: HashSet<(MessengerId, ListenerKind)>,
    /// Unsupported routes skipped during sibling recovery.
    disabled_listeners: HashSet<(MessengerId, ListenerKind)>,
    /// Backends whose initial snapshot has committed and may apply live events.
    initialized_messengers: HashSet<MessengerId>,
    pending_startup_events: HashMap<MessengerId, VecDeque<BufferedStartupEvent>>,
    /// Journal overflow means some live mutations were omitted. A fresh REST
    /// snapshot is scheduled before this backend becomes initialized.
    startup_overflowed: HashSet<MessengerId>,
    /// Backends currently being reconstructed after journal overflow. A fresh
    /// instance gives recovery same authoritative baseline as normal launch.
    restarting_messengers: HashSet<MessengerId>,
    /// Raw `LoginInfo` entries that could not safely become live sessions.
    /// Unsupported backends, unknown schemas, and transient restore failures
    /// are written back unchanged.
    retained_credentials: Vec<RetainedCredential>,
    /// Resolved once at startup so later saves cannot follow a changed working
    /// directory or switch between legacy and per-user state locations.
    credential_store_path: PathBuf,
    /// False when startup could not read the complete credential file. Refuse
    /// partial rewrites in that session rather than destroy unread entries.
    credential_store_writable: bool,
    /// Fingerprint paired with the loaded store; prevents a second app process
    /// from being silently overwritten by stale in-memory credentials.
    credential_store_revision: Option<login_store::StoreRevision>,
    /// Active secret-storage backend and any OS-vault fallback reason.
    credential_storage: Option<login_store::StorageStatus>,
    /// Latest credential persistence failure shown in Settings.
    credential_error: Option<String>,
    /// Latest media failure. Settings exposes it with recovery actions.
    media_error: Option<String>,
    /// Invalidates close notifications from streams replaced through Settings.
    input_stream_generation: u64,
    output_stream_generation: u64,
}

impl App {
    fn track_login_task(&mut self, attempt: u64, task: Task<AppMessage>) -> Task<AppMessage> {
        self.abort_login_task();
        let (task, handle) = task.abortable();
        self.login_task = Some((attempt, handle));
        task
    }

    fn abort_login_task(&mut self) {
        if let Some((_, handle)) = self.login_task.take() {
            handle.abort();
        }
    }

    fn clear_login_task(&mut self, attempt: u64) {
        if self
            .login_task
            .as_ref()
            .is_some_and(|(current, _)| *current == attempt)
        {
            self.login_task = None;
        }
    }

    fn buffer_startup_event(&mut self, id: MessengerId, event: BufferedStartupEvent) -> bool {
        let pending = self.pending_startup_events.entry(id).or_default();
        if pending.len() < MAX_STARTUP_EVENT_BUFFER {
            pending.push_back(event);
            return false;
        }
        if self.startup_overflowed.insert(id) {
            warn!(
                ?id,
                limit = MAX_STARTUP_EVENT_BUFFER,
                "Startup event journal full; scheduling authoritative reconciliation"
            );
            return true;
        }
        false
    }

    fn restart_messenger(&mut self, id: MessengerId) -> Task<AppMessage> {
        if !self.restarting_messengers.insert(id) {
            return Task::none();
        }
        let Some(entry) = self.messengers.get(id) else {
            self.restarting_messengers.remove(&id);
            return Task::none();
        };
        let name = entry.interface.name().to_owned();
        let credential = entry.interface.auth();
        let Some(backend) = login::backend(&name) else {
            self.restarting_messengers.remove(&id);
            self.media_error = Some(format!(
                "Cannot reconcile {name}: backend unavailable in this build"
            ));
            return Task::none();
        };
        let create = backend.create;
        Task::future(async move {
            let result = create(credential).await.map_err(|error| {
                format!("Failed to rebuild {name} for state reconciliation: {error}")
            });
            AppMessage::MessengerRestarted {
                messenger: id,
                result,
            }
        })
    }

    fn persist_credentials(
        &mut self,
        excluded_messenger: Option<MessengerId>,
        excluded_retained: Option<u64>,
    ) -> std::io::Result<()> {
        self.persist_credentials_with_mode(excluded_messenger, excluded_retained, None)
    }

    fn persist_credentials_with_mode(
        &mut self,
        excluded_messenger: Option<MessengerId>,
        excluded_retained: Option<u64>,
        storage_mode: Option<login_store::StorageMode>,
    ) -> std::io::Result<()> {
        if !self.credential_store_writable {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "credential store is not writable because startup did not read it completely",
            ));
        }
        let expected = self.credential_store_revision.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "credential store has no loaded revision",
            )
        })?;
        let mut entries =
            Vec::with_capacity(self.retained_credentials.len() + self.messengers.len());
        for credential in &self.retained_credentials {
            if Some(credential.id) != excluded_retained {
                entries.push((credential.name.clone(), credential.auth.clone()));
            }
        }
        for (id, entry) in self.messengers.iter() {
            if Some(id) != excluded_messenger {
                entries.push((
                    entry.interface.name().to_owned(),
                    Zeroizing::new(login_store::to_line(&entry.interface.auth())),
                ));
            }
        }

        let saved = match storage_mode {
            Some(mode) => login_store::save_versioned_as(
                &self.credential_store_path,
                expected,
                entries
                    .iter()
                    .map(|(name, auth)| (name.as_str(), auth.as_str())),
                mode,
            ),
            None => login_store::save_versioned_with_status(
                &self.credential_store_path,
                expected,
                entries
                    .iter()
                    .map(|(name, auth)| (name.as_str(), auth.as_str())),
            ),
        };
        match saved {
            Ok(saved) => {
                self.credential_store_revision = Some(saved.revision);
                self.credential_storage = Some(saved.storage);
                self.credential_error = None;
                Ok(())
            }
            Err(error) => {
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    // Another process owns a newer account set. Never let this
                    // stale instance attempt another credential rewrite.
                    self.credential_store_writable = false;
                    self.credential_store_revision = None;
                }
                Err(error)
            }
        }
    }

    fn report_credential_error(&mut self, context: &str, error: &std::io::Error) {
        let message = format!("{context}: {error}");
        error!("{message}");
        self.credential_error = Some(message);
    }

    fn discard_messenger(&mut self, id: MessengerId) {
        self.messengers.remove(id);
        self.started_messengers.remove(&id);
        self.active_listeners
            .retain(|(messenger, _)| *messenger != id);
        self.disabled_listeners
            .retain(|(messenger, _)| *messenger != id);
        self.initialized_messengers.remove(&id);
        self.pending_startup_events.remove(&id);
        self.startup_overflowed.remove(&id);
        self.restarting_messengers.remove(&id);

        for ((messenger, _), gate) in &self.call_operations {
            if *messenger == id {
                gate.invalidate();
            }
        }
        for ((messenger, _), gate) in &self.voice_state_operations {
            if *messenger == id {
                gate.invalidate();
            }
        }
        for ((messenger, _, _), gate) in &self.video_operations {
            if *messenger == id {
                gate.invalidate();
            }
        }
        self.call_operations
            .retain(|(messenger, _), _| *messenger != id);
        self.voice_state_operations
            .retain(|(messenger, _), _| *messenger != id);
        self.video_operations
            .retain(|(messenger, _, _), _| *messenger != id);
        self.video_inputs
            .retain(|(messenger, _), _| *messenger != id);
        self.remote_video.remove_messenger(id);
        self.audio_controls.detach_messenger(id);
        self.messenger.remove_messenger(id);
    }

    fn boot() -> (Self, Task<AppMessage>) {
        // Read each saved `name:credential` line into a (backend, credential)
        // pair. Construction is async, so we only parse here and build the
        // messengers in the task below once the window is up.
        let mut retained_credentials = Vec::new();
        let resolved_path = credential_store_path::resolve();
        let (credential_store_path, stored) = match resolved_path {
            Ok(path) => {
                trace!(path = %path.display(), "Using credential-store path");
                let stored =
                    login_store::load_versioned(&path, |name, blob| match parse_stored_credential(
                        name, blob,
                    ) {
                        Ok(stored) => Some(stored),
                        Err(retained) => {
                            warn!(
                                error_code = retained.failure.code(),
                                "Unable to parse credentials for {name}; preserving saved entry"
                            );
                            retained_credentials.push(retained);
                            None
                        }
                    });
                (path, stored)
            }
            Err(error) => {
                // Path resolution failure makes this session read-only. Keep an
                // absolute placeholder so no later code can accidentally follow
                // a changed working directory if that invariant regresses.
                let placeholder = std::env::current_dir()
                    .unwrap_or_default()
                    .join("LoginInfo");
                (placeholder, Err(error))
            }
        };
        let (
            stored,
            credential_store_revision,
            credential_store_writable,
            credential_storage,
            credential_error,
        ) = match stored {
            Ok(stored) => (
                stored.values,
                Some(stored.revision),
                true,
                Some(stored.storage),
                None,
            ),
            Err(err) => {
                let message = format!("Failed to read complete credential store: {err}");
                error!("{message}");
                retained_credentials.clear();
                (Vec::new(), None, false, None, Some(message))
            }
        };

        if stored.is_empty() && retained_credentials.is_empty() {
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
            login_task: None,
            messenger: messenger::Messenger::new(),
            settings_category: settings::Category::default(),
            audio: AudioMixer::default(),
            audio_controls: AudioControls::default(),
            remote_video: RemoteVideo::default(),
            video_inputs: HashMap::new(),
            preferred_video_codec: VideoCodec::H264,
            call_operations: HashMap::new(),
            voice_state_operations: HashMap::new(),
            video_operations: HashMap::new(),
            messengers: MessengerRegistry::default(),
            started_messengers: HashSet::new(),
            active_listeners: HashSet::new(),
            disabled_listeners: HashSet::new(),
            initialized_messengers: HashSet::new(),
            pending_startup_events: HashMap::new(),
            startup_overflowed: HashSet::new(),
            restarting_messengers: HashSet::new(),
            retained_credentials,
            credential_store_path,
            credential_store_writable,
            credential_store_revision,
            credential_storage,
            credential_error,
            media_error: None,
            input_stream_generation: 0,
            output_stream_generation: 0,
        };

        let (_window_id, window_task) = window::open(window::Settings::default());
        let restore_task = if stored.is_empty() {
            Task::none()
        } else {
            Task::future(async move {
                let results = join_all(stored.into_iter().map(
                    |(backend, stored_name, credential, raw)| async move {
                        match create_verified_messenger(backend.name, credential).await {
                            Ok(owner) => Ok(owner),
                            Err(error) => {
                                let _error = Zeroizing::new(error);
                                let failure = CredentialFailure::SessionRestoreFailed;
                                error!(
                                    backend = backend.name,
                                    error_code = failure.code(),
                                    "Failed to restore saved session; preserving credential without logging backend error details"
                                );
                                Err(RetainedCredential::from_owned(stored_name, raw, failure))
                            }
                        }
                    },
                ))
                .await;
                let (messengers, retained) = partition_restore_results(results);
                AppMessage::CredentialsRestored {
                    messengers,
                    retained,
                }
            })
        };
        // `chain` waits for window creation without forcing the startup data
        // through iced's repeatable `FnMut` callback contract.
        (app, window_task.discard().chain(restore_task))
    }

    fn update(&mut self, message: AppMessage) -> Task<AppMessage> {
        match message {
            // === Navigation ===
            AppMessage::Navigate(screen) => {
                self.screen = screen;
                Task::none()
            }
            AppMessage::MediaError(error) => {
                error!("{error}");
                self.media_error = Some(error);
                Task::none()
            }
            // === Credentials ===
            AppMessage::CredentialsRestored {
                messengers,
                retained,
            } => {
                self.retained_credentials.extend(retained);
                if messengers.is_empty() {
                    self.screen = Screen::Login;
                    return Task::none();
                }
                for messenger in messengers {
                    self.messengers.add(messenger);
                }
                Task::done(AppMessage::StartUp)
            }
            AppMessage::SaveCredentials => {
                if let Err(error) = self.persist_credentials(None, None) {
                    self.report_credential_error("Failed to save credentials", &error);
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
                let mut tasks = Vec::new();
                let mut started_snapshot = false;
                for (id, entry) in self.messengers.iter() {
                    let interface = entry.interface.get();
                    let (has_query, has_text, has_voice, has_video) = (
                        interface.messenger().query().is_ok(),
                        interface.messenger().text().is_ok(),
                        interface.messenger().audio().is_ok(),
                        interface.messenger().video().is_ok(),
                    );
                    if !has_query {
                        error!("Query not impl");
                        continue;
                    }
                    if !has_text {
                        error!("Text not impl");
                        continue;
                    };

                    // Start and recover each capability stream independently.
                    if !self.disabled_listeners.contains(&(id, ListenerKind::Query))
                        && self.active_listeners.insert((id, ListenerKind::Query))
                    {
                        let Some(stream_interface) =
                            self.messengers.with_interface_async(id, async |interface| {
                                interface.listen_query().await
                            })
                        else {
                            continue;
                        };
                        tasks.push(
                            Task::future(async move {
                                stream_interface
                                    .await
                                    .unwrap_or_else(|| Err(closed_messenger_error()))
                            })
                            .then(move |result| match result {
                                Ok(socket) => Task::stream(
                                    socket
                                        .map(move |event| AppMessage::QueryEvent((id, event)))
                                        .chain(stream::once(async move {
                                            AppMessage::ListenerStopped {
                                                messenger: id,
                                                kind: ListenerKind::Query,
                                                error: None,
                                                retry: true,
                                            }
                                        })),
                                ),
                                Err(error) => {
                                    let retry = listener_error_is_retryable(error.as_ref());
                                    Task::done(AppMessage::ListenerStopped {
                                        messenger: id,
                                        kind: ListenerKind::Query,
                                        error: Some(error.to_string()),
                                        retry,
                                    })
                                }
                            }),
                        );
                    }
                    if !self.disabled_listeners.contains(&(id, ListenerKind::Text))
                        && self.active_listeners.insert((id, ListenerKind::Text))
                    {
                        let Some(stream_interface) =
                            self.messengers.with_interface_async(id, async |interface| {
                                interface.listen_text().await
                            })
                        else {
                            continue;
                        };
                        tasks.push(
                            Task::future(async move {
                                stream_interface
                                    .await
                                    .unwrap_or_else(|| Err(closed_messenger_error()))
                            })
                            .then(move |result| match result {
                                Ok(socket) => Task::stream(
                                    socket
                                        .map(move |event| AppMessage::TextEvent((id, event)))
                                        .chain(stream::once(async move {
                                            AppMessage::ListenerStopped {
                                                messenger: id,
                                                kind: ListenerKind::Text,
                                                error: None,
                                                retry: true,
                                            }
                                        })),
                                ),
                                Err(error) => {
                                    let retry = listener_error_is_retryable(error.as_ref());
                                    Task::done(AppMessage::ListenerStopped {
                                        messenger: id,
                                        kind: ListenerKind::Text,
                                        error: Some(error.to_string()),
                                        retry,
                                    })
                                }
                            }),
                        );
                    }

                    let preferred_video_codec = self.preferred_video_codec;
                    if has_voice
                        && !self.disabled_listeners.contains(&(id, ListenerKind::Voice))
                        && self.active_listeners.insert((id, ListenerKind::Voice))
                    {
                        let Some(stream_interface) =
                            self.messengers.with_interface_async(id, async |interface| {
                                interface.listen_voice().await
                            })
                        else {
                            continue;
                        };
                        tasks.push(
                            Task::future(async move {
                                stream_interface
                                    .await
                                    .unwrap_or_else(|| Err(closed_messenger_error()))
                            })
                            .then(move |result| match result {
                                Ok(socket) => Task::stream(
                                    socket
                                        .map(move |event| AppMessage::VoiceEvent((id, event)))
                                        .chain(stream::once(async move {
                                            AppMessage::ListenerStopped {
                                                messenger: id,
                                                kind: ListenerKind::Voice,
                                                error: None,
                                                retry: true,
                                            }
                                        })),
                                ),
                                Err(error) => {
                                    let retry = listener_error_is_retryable(error.as_ref());
                                    Task::done(AppMessage::ListenerStopped {
                                        messenger: id,
                                        kind: ListenerKind::Voice,
                                        error: Some(error.to_string()),
                                        retry,
                                    })
                                }
                            }),
                        );
                    }
                    if has_video
                        && !self.disabled_listeners.contains(&(id, ListenerKind::Video))
                        && self.active_listeners.insert((id, ListenerKind::Video))
                    {
                        let Some(stream_interface) =
                            self.messengers
                                .with_interface_async(id, async move |interface| {
                                    interface.listen_video(preferred_video_codec).await
                                })
                        else {
                            continue;
                        };
                        tasks.push(
                            Task::future(async move {
                                stream_interface
                                    .await
                                    .unwrap_or_else(|| Err(closed_messenger_error()))
                            })
                            .then(move |result| match result {
                                Ok(socket) => Task::stream(
                                    socket
                                        .map(move |event| AppMessage::VideoEvent((id, event)))
                                        .chain(stream::once(async move {
                                            AppMessage::ListenerStopped {
                                                messenger: id,
                                                kind: ListenerKind::Video,
                                                error: None,
                                                retry: true,
                                            }
                                        })),
                                ),
                                Err(error) => {
                                    let retry = listener_error_is_retryable(error.as_ref());
                                    Task::done(AppMessage::ListenerStopped {
                                        messenger: id,
                                        kind: ListenerKind::Video,
                                        error: Some(error.to_string()),
                                        retry,
                                    })
                                }
                            }),
                        );
                    }

                    if self.started_messengers.insert(id) {
                        started_snapshot = true;
                        let Some(profile) =
                            self.messengers.with_interface_async(id, async |interface| {
                                retry_startup_request("profile", || async {
                                    let query = interface
                                        .messenger()
                                        .query()
                                        .map_err(|error| error.to_string())?;
                                    query.client_user().await.map_err(|error| error.to_string())
                                })
                                .await
                            })
                        else {
                            continue;
                        };
                        let Some(contacts) =
                            self.messengers.with_interface_async(id, async |interface| {
                                retry_startup_request("contacts", || async {
                                    let query = interface
                                        .messenger()
                                        .query()
                                        .map_err(|error| error.to_string())?;
                                    query.contacts().await.map_err(|error| error.to_string())
                                })
                                .await
                            })
                        else {
                            continue;
                        };
                        let Some(conversations) =
                            self.messengers.with_interface_async(id, async |interface| {
                                retry_startup_request("rooms", || async {
                                    let query = interface
                                        .messenger()
                                        .query()
                                        .map_err(|error| error.to_string())?;
                                    query.rooms().await.map_err(|error| error.to_string())
                                })
                                .await
                            })
                        else {
                            continue;
                        };
                        let Some(servers) =
                            self.messengers.with_interface_async(id, async |interface| {
                                retry_startup_request("houses", || async {
                                    let query = interface
                                        .messenger()
                                        .query()
                                        .map_err(|error| error.to_string())?;
                                    query.houses().await.map_err(|error| error.to_string())
                                })
                                .await
                            })
                        else {
                            continue;
                        };
                        tasks.push(
                            Task::future(async move {
                                let (profile, contacts, conversations, servers) =
                                    join!(profile, contacts, conversations, servers);
                                (
                                    profile.unwrap_or_else(|| Err("messenger closed".to_owned())),
                                    contacts.unwrap_or_else(|| Err("messenger closed".to_owned())),
                                    conversations
                                        .unwrap_or_else(|| Err("messenger closed".to_owned())),
                                    servers.unwrap_or_else(|| Err("messenger closed".to_owned())),
                                )
                            })
                            .then(
                                move |(profile, contacts, conversations, servers)| {
                                    Task::done(AppMessage::StartupSnapshot {
                                        messenger: id,
                                        snapshot: Box::new(StartupSnapshot {
                                            profile,
                                            contacts,
                                            conversations,
                                            servers,
                                        }),
                                    })
                                },
                            ),
                        );
                    }
                }

                let tasks = Task::batch(tasks);
                if started_snapshot {
                    Task::done(AppMessage::Navigate(Screen::Messenger))
                        .chain(Task::done(AppMessage::SaveCredentials))
                        .chain(tasks)
                } else {
                    tasks
                }
            }
            AppMessage::StartupSnapshot {
                messenger: id,
                snapshot,
            } => {
                if self.messengers.get(id).is_none() {
                    return Task::none();
                }
                if self.startup_overflowed.contains(&id) {
                    self.media_error = Some(
                        "Startup traffic exceeded journal capacity; rebuilding messenger connection"
                            .to_owned(),
                    );
                    return self.restart_messenger(id);
                }

                let StartupSnapshot {
                    profile,
                    contacts,
                    conversations,
                    servers,
                } = *snapshot;
                let errors = [
                    profile.as_ref().err(),
                    contacts.as_ref().err(),
                    conversations.as_ref().err(),
                    servers.as_ref().err(),
                ]
                .into_iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>();
                if !errors.is_empty() {
                    // Do not consume dependent socket events against partial
                    // topology. Retry whole snapshot; journal remains intact.
                    self.started_messengers.remove(&id);
                    self.media_error = Some(format!(
                        "Messenger startup incomplete: {}. Retrying; Settings can retry now.",
                        errors.join("; ")
                    ));
                    return Task::future(async {
                        Delay::new(Duration::from_secs(5)).await;
                    })
                    .then(|()| Task::done(AppMessage::StartUp));
                }

                let data = self
                    .messengers
                    .data_mut(id)
                    .expect("messenger existence checked above");
                data.profile = profile.ok();
                data.contacts = contacts.unwrap_or_default();
                data.conversations = conversations.unwrap_or_default();
                data.guilds = servers.unwrap_or_default();
                data.rebuild_room_index();
                self.initialized_messengers.insert(id);
                let pending = self.pending_startup_events.remove(&id).unwrap_or_default();
                let mut tasks = Vec::with_capacity(pending.len());
                for event in pending {
                    tasks.push(match event {
                        BufferedStartupEvent::Query(event) => {
                            events::process_query_event(id, event, &mut self.messengers)
                        }
                        BufferedStartupEvent::Text(event) => {
                            events::process_text_event(id, event, &mut self.messengers)
                        }
                        BufferedStartupEvent::Voice(event) => events::process_voice_event(
                            id,
                            event,
                            &mut self.messengers,
                            &mut self.audio_controls,
                        ),
                    });
                }
                Task::batch(tasks)
            }
            AppMessage::RestartMessenger(id) => self.restart_messenger(id),
            AppMessage::MessengerRestarted {
                messenger: id,
                result,
            } => {
                self.restarting_messengers.remove(&id);
                match result {
                    Ok(api) => {
                        if self.messengers.get(id).is_none() {
                            return Task::none();
                        }
                        self.discard_messenger(id);
                        self.messenger = messenger::Messenger::new();
                        self.messengers.add(api);
                        self.media_error = None;
                        Task::done(AppMessage::StartUp)
                    }
                    Err(error) => {
                        self.media_error = Some(format!(
                            "{error}. Automatic retry scheduled; Settings can retry now."
                        ));
                        Task::future(async {
                            Delay::new(Duration::from_secs(5)).await;
                        })
                        .then(move |()| Task::done(AppMessage::RestartMessenger(id)))
                    }
                }
            }
            AppMessage::ListenerStopped {
                messenger,
                kind,
                error,
                retry,
            } => {
                self.active_listeners.remove(&(messenger, kind));
                if let Some(error) = error {
                    warn!(?kind, "Messenger listener stopped: {error}");
                } else {
                    warn!(?kind, "Messenger listener ended; scheduling restart");
                }
                if self.messengers.get(messenger).is_none() {
                    return Task::none();
                }
                if !retry {
                    self.disabled_listeners.insert((messenger, kind));
                    debug!(?kind, "Messenger listener unsupported; leaving disabled");
                    return Task::none();
                }
                Task::future(async {
                    Delay::new(Duration::from_secs(1)).await;
                })
                .then(|()| Task::done(AppMessage::StartUp))
            }
            // === Socket Events (delegated) ===
            AppMessage::QueryEvent((id, event)) => {
                if matches!(&event, QueryEvent::StateInvalidated) {
                    warn!(
                        ?id,
                        "Messenger realtime state invalidated; rebuilding snapshot"
                    );
                    return self.restart_messenger(id);
                }
                if self.messengers.get(id).is_none() {
                    return Task::none();
                }
                if self.initialized_messengers.contains(&id) {
                    events::process_query_event(id, event, &mut self.messengers)
                } else {
                    let overflowed =
                        self.buffer_startup_event(id, BufferedStartupEvent::Query(event));
                    if overflowed {
                        self.restart_messenger(id)
                    } else {
                        Task::none()
                    }
                }
            }
            AppMessage::TextEvent((id, event)) => {
                if self.messengers.get(id).is_none() {
                    return Task::none();
                }
                if self.initialized_messengers.contains(&id) {
                    events::process_text_event(id, event, &mut self.messengers)
                } else {
                    let overflowed =
                        self.buffer_startup_event(id, BufferedStartupEvent::Text(event));
                    if overflowed {
                        self.restart_messenger(id)
                    } else {
                        Task::none()
                    }
                }
            }
            AppMessage::VoiceEvent((id, event)) => {
                if self.messengers.get(id).is_none() {
                    return Task::none();
                }
                if self.initialized_messengers.contains(&id)
                    || matches!(&event, VoiceEvent::CallStreamReady { .. })
                {
                    events::process_voice_event(
                        id,
                        event,
                        &mut self.messengers,
                        &mut self.audio_controls,
                    )
                } else {
                    let overflowed =
                        self.buffer_startup_event(id, BufferedStartupEvent::Voice(event));
                    if overflowed {
                        self.restart_messenger(id)
                    } else {
                        Task::none()
                    }
                }
            }
            AppMessage::AudioEvent((id, event)) => {
                if self.messengers.get(id).is_none() {
                    return Task::none();
                }
                events::process_audio_event(id, event, &mut self.audio, &mut self.audio_controls)
            }
            AppMessage::VideoEvent((id, event)) => {
                if self.messengers.get(id).is_none() {
                    return Task::none();
                }
                // Frame/input handshakes carry oneshot senders and explicitly
                // require immediate response. They do not mutate REST snapshot
                // fields, so startup buffering would add latency without safety.
                events::process_video_event(id, event, &mut self.remote_video, &self.video_inputs)
            }
            AppMessage::VideoPreview {
                messenger,
                stream,
                generation,
                frame,
            } => {
                let timing_enabled =
                    tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
                let started = timing_enabled.then(Instant::now);
                let key = (messenger, stream);
                if self.remote_video.is_current(&key, generation) {
                    let (width, height) = (frame.width, frame.height);
                    let pixel_data = match frame.into_rgba8() {
                        Ok(pixel_data) => pixel_data,
                        Err(_) => return Task::none(),
                    };
                    if let Some(stream) = self.remote_video.streams.get_mut(&key) {
                        stream.preview = Some(iced::widget::image::Handle::from_rgba(
                            width, height, pixel_data,
                        ));
                        if let Some(started) = started {
                            trace!(
                                target: "video_timing",
                                stage = "ui.texture_handle",
                                width,
                                height,
                                elapsed_us = elapsed_micros(started),
                                "video stage complete"
                            );
                        }
                    }
                }
                Task::none()
            }
            AppMessage::VideoPreviewEnded {
                messenger,
                stream,
                generation,
            } => {
                let key = (messenger, stream);
                if self.remote_video.is_current(&key, generation) {
                    self.remote_video.remove(&key);
                }
                Task::none()
            }
            AppMessage::VideoSubscriptionResult {
                messenger,
                stream,
                generation,
                result,
            } => {
                let key = (messenger, stream);
                if !self.remote_video.is_current_subscription(&key, generation) {
                    if result.is_err() || self.remote_video.streams.contains_key(&key) {
                        return Task::none();
                    }
                    // Reset/removal won while subscribe was in flight. Issue a
                    // post-completion unsubscribe so request ordering cannot
                    // resurrect a native subscription with no UI consumer.
                    let Some(unsubscribe) =
                        self.messengers
                            .with_interface_async(messenger, async move |interface| {
                                let video = interface
                                    .messenger()
                                    .video()
                                    .map_err(|err| err.to_string())?;
                                video
                                    .unsubscribe_video(stream)
                                    .await
                                    .map_err(|err| err.to_string())
                            })
                    else {
                        return Task::none();
                    };
                    return Task::future(async move {
                        unsubscribe
                            .await
                            .unwrap_or_else(|| Err("messenger closed".to_owned()))
                    })
                    .then(|result| match result {
                        Ok(()) => Task::none(),
                        Err(error) => Task::done(AppMessage::MediaError(error)),
                    });
                }
                if let Err(err) = result {
                    let kind = self
                        .remote_video
                        .streams
                        .get(&key)
                        .map(|stream| stream.metadata.kind);
                    error!("Failed to subscribe to {kind:?} video stream {stream:?}: {err}");
                    self.media_error = Some(format!(
                        "Failed to subscribe to {kind:?} video stream {stream:?}: {err}"
                    ));
                    self.remote_video.subscription_failed(&key, generation);
                }
                Task::none()
            }
            AppMessage::CallConnectResult {
                key: (messenger, room),
                generation,
                result,
            } => {
                if !self
                    .call_operations
                    .get(&(messenger, room))
                    .is_some_and(|gate| gate.is_current(generation))
                {
                    return Task::none();
                }
                let Some(result) = result else {
                    return Task::none();
                };
                match result {
                    Ok(status) => {
                        if let Some(data) = self.messengers.data_mut(messenger)
                            && let Some(call) = data.calls.iter_mut().find(|call| call.id() == room)
                            && !call.is_connected()
                        {
                            call.set_state(CallState::Pending(status));
                        }
                        Task::none()
                    }
                    Err(error) => {
                        if let Some(data) = self.messengers.data_mut(messenger)
                            && let Some(call) = data.calls.iter_mut().find(|call| call.id() == room)
                            && !call.is_connected()
                        {
                            call.set_state(CallState::Pending(CallStatus::Failed));
                        }
                        self.media_error = Some(error.clone());
                        Task::none()
                    }
                }
            }
            AppMessage::CallDisconnectResult {
                key,
                generation,
                errors,
            } => {
                let call_is_current = self
                    .call_operations
                    .get(&key)
                    .is_some_and(|gate| gate.is_current(generation));
                if call_is_current {
                    self.call_operations.remove(&key);
                    self.voice_state_operations.remove(&key);
                    self.video_operations
                        .retain(|(messenger, room, _), _| (*messenger, *room) != key);
                }
                if !errors.is_empty() {
                    self.media_error = Some(errors.join("; "));
                }
                Task::none()
            }
            AppMessage::VoiceStateResult {
                key,
                generation,
                result,
            } => {
                if !self
                    .voice_state_operations
                    .get(&key)
                    .is_some_and(|gate| gate.is_current(generation))
                {
                    return Task::none();
                }
                if let Some(Err(error)) = result {
                    self.media_error = Some(error);
                }
                Task::none()
            }
            AppMessage::VideoToggleResult {
                operation,
                input: input_key,
                generation,
                previous,
                result,
            } => {
                if !self
                    .video_operations
                    .get(&operation)
                    .is_some_and(|gate| gate.is_current(generation))
                {
                    // A newer toggle or Settings reset owns this key now.
                    return Task::none();
                }
                if let Err(error) = result {
                    match previous {
                        Some(previous_input) => {
                            self.video_inputs.insert(input_key, previous_input);
                        }
                        None => {
                            self.video_inputs.remove(&input_key);
                        }
                    }
                    self.media_error = Some(error.clone());
                    error!("{error}");
                }
                Task::none()
            }
            AppMessage::VideoCaptureResult {
                key,
                generation,
                result,
            } => {
                if self
                    .video_inputs
                    .get(&key)
                    .is_none_or(|input| input.generation != generation)
                {
                    return Task::none();
                }
                if let Err(error) = result {
                    self.video_inputs.remove(&key);
                    self.media_error = Some(error);
                }
                Task::none()
            }
            // === Audio ===
            AppMessage::StartStream(dir) => {
                let already_running = match dir {
                    StreamDirection::Input => self.audio.is_streaming_input(),
                    StreamDirection::Output => self.audio.is_streaming_output(),
                };
                // One running generation owns exactly one close waiter. Several
                // AddAudio events can queue StartStream before first is handled.
                if already_running {
                    return Task::none();
                }
                let generation = match dir {
                    StreamDirection::Input => {
                        self.input_stream_generation = self.input_stream_generation.wrapping_add(1);
                        self.input_stream_generation
                    }
                    StreamDirection::Output => {
                        self.output_stream_generation =
                            self.output_stream_generation.wrapping_add(1);
                        self.output_stream_generation
                    }
                };
                let result = match dir {
                    StreamDirection::Input => self.audio.start_stream_input(),
                    StreamDirection::Output => self.audio.start_stream_output(),
                };
                match result {
                    Ok(Some(notify)) => {
                        Task::future(async move { notify.notified().await }).then(move |_| {
                            Task::done(AppMessage::StopStream {
                                direction: dir,
                                generation,
                            })
                        })
                    }
                    Ok(None) => {
                        let error = format!("No {dir} device available");
                        error!("{error}");
                        self.media_error = Some(error);
                        Task::none()
                    }
                    Err(err) => {
                        let error = format!("Failed to start {dir} stream: {err}");
                        error!("{error}");
                        self.media_error = Some(error);
                        Task::none()
                    }
                }
            }
            AppMessage::StopStream {
                direction: dir,
                generation,
            } => {
                let current_generation = match dir {
                    StreamDirection::Input => self.input_stream_generation,
                    StreamDirection::Output => self.output_stream_generation,
                };
                if generation != current_generation {
                    return Task::none();
                }
                let still_live = match dir {
                    StreamDirection::Input => self.audio.has_live_input_channels(),
                    StreamDirection::Output => self.audio.has_live_output_channels(),
                };
                if still_live {
                    // Channel appeared after callback announced an empty graph.
                    // Keep stream and re-arm same generation's sole waiter.
                    let notify = match dir {
                        StreamDirection::Input => self.audio.start_stream_input(),
                        StreamDirection::Output => self.audio.start_stream_output(),
                    };
                    return match notify {
                        Ok(Some(notify)) => Task::future(async move { notify.notified().await })
                            .then(move |_| {
                                Task::done(AppMessage::StopStream {
                                    direction: dir,
                                    generation,
                                })
                            }),
                        Ok(None) => Task::none(),
                        Err(error) => Task::done(AppMessage::MediaError(format!(
                            "Failed to re-arm {dir} stream: {error}"
                        ))),
                    };
                }
                // The stream's channels are gone with it — drop the control
                // handles that pointed into them (stored volumes survive).
                match dir {
                    StreamDirection::Input => {
                        self.audio.stop_stream_input();
                        self.audio_controls.detach_all_mics();
                    }
                    StreamDirection::Output => {
                        self.audio.stop_stream_output();
                        self.audio_controls.detach_all_users();
                    }
                };
                Task::none()
            }
            AppMessage::Settings(message) => match message {
                settings::Message::Back => {
                    self.screen = if self.messengers.is_empty() {
                        Screen::Login
                    } else {
                        Screen::Messenger
                    };
                    Task::none()
                }
                settings::Message::SelectCategory(category) => {
                    self.settings_category = category;
                    Task::none()
                }
                settings::Message::SignOut(id) => {
                    if self.messengers.get(id).is_none() {
                        return Task::none();
                    }
                    match self.persist_credentials(Some(id), None) {
                        Ok(()) => self.discard_messenger(id),
                        Err(error) => {
                            self.report_credential_error("Failed to sign out account", &error);
                        }
                    }
                    Task::none()
                }
                settings::Message::RemoveSavedAccount(id) => {
                    let Some(index) = self
                        .retained_credentials
                        .iter()
                        .position(|credential| credential.id == id)
                    else {
                        return Task::none();
                    };
                    match self.persist_credentials(None, Some(id)) {
                        Ok(()) => {
                            self.retained_credentials.remove(index);
                        }
                        Err(error) => {
                            self.report_credential_error(
                                "Failed to remove unavailable saved account",
                                &error,
                            );
                        }
                    }
                    Task::none()
                }
                settings::Message::RestartInput => {
                    self.input_stream_generation = self.input_stream_generation.wrapping_add(1);
                    self.audio.stop_stream_input();
                    self.media_error = None;
                    Task::done(AppMessage::StartStream(StreamDirection::Input))
                }
                settings::Message::RestartOutput => {
                    self.output_stream_generation = self.output_stream_generation.wrapping_add(1);
                    self.audio.stop_stream_output();
                    self.media_error = None;
                    Task::done(AppMessage::StartStream(StreamDirection::Output))
                }
                settings::Message::RedetectAudio => {
                    self.input_stream_generation = self.input_stream_generation.wrapping_add(1);
                    self.output_stream_generation = self.output_stream_generation.wrapping_add(1);
                    self.audio.stop_stream_input();
                    self.audio.stop_stream_output();
                    self.audio_controls.detach_all_mics();
                    self.audio_controls.detach_all_users();
                    self.audio = AudioMixer::default();
                    self.media_error = None;
                    Task::batch([
                        Task::done(AppMessage::StartStream(StreamDirection::Input)),
                        Task::done(AppMessage::StartStream(StreamDirection::Output)),
                    ])
                }
                settings::Message::SetVideoCodec(codec) => {
                    if self.preferred_video_codec == codec {
                        return Task::none();
                    }
                    self.preferred_video_codec = codec;
                    Task::batch(
                        self.messengers
                            .iter()
                            .filter(|(_, entry)| entry.interface.messenger().video().is_ok())
                            .filter_map(|(id, entry)| {
                                let name = entry.interface.name().to_owned();
                                let set_codec = self.messengers.with_interface_async(
                                    id,
                                    async move |interface| {
                                        let video = interface
                                            .messenger()
                                            .video()
                                            .map_err(|err| err.to_string())?;
                                        video.set_preferred_codec(codec).await.map_err(|err| {
                                            format!(
                                                "Failed to select {codec:?} video on {name}: {err}"
                                            )
                                        })
                                    },
                                )?;
                                Some(
                                    Task::future(async move {
                                        set_codec
                                            .await
                                            .unwrap_or_else(|| Err("messenger closed".to_owned()))
                                    })
                                    .then(|result| {
                                        match result {
                                            Ok(()) => Task::none(),
                                            Err(error) => Task::done(AppMessage::MediaError(error)),
                                        }
                                    }),
                                )
                            }),
                    )
                }
                settings::Message::ResetVideo => {
                    let subscriptions = self.remote_video.owned_subscriptions().collect::<Vec<_>>();
                    self.remote_video.clear();
                    self.video_inputs.clear();
                    for gate in self.video_operations.values() {
                        gate.invalidate();
                    }

                    let stop_requests = self
                        .messengers
                        .iter()
                        .flat_map(|(_, entry)| {
                            entry.data.calls.iter().flat_map(|call| {
                                [
                                    messenger_interface::interface::VideoBroadcastKind::Camera,
                                    messenger_interface::interface::VideoBroadcastKind::Screen,
                                ]
                                .into_iter()
                                .map(|kind| {
                                    (
                                        entry.interface.id,
                                        call.source().swap_data((**call.source()).clone()),
                                        (entry.interface.id, call.id(), kind),
                                        kind,
                                    )
                                })
                            })
                        })
                        .collect::<Vec<_>>();
                    let mut tasks = Vec::with_capacity(stop_requests.len() + subscriptions.len());
                    for (messenger, room, operation, kind) in stop_requests {
                        let call_operation = (operation.0, operation.1);
                        let call_gate = self
                            .call_operations
                            .entry(call_operation)
                            .or_insert_with(|| Arc::new(OperationGate::default()))
                            .clone();
                        let gate = self
                            .video_operations
                            .entry(operation)
                            .or_insert_with(|| {
                                Arc::new(OperationGate::with_lock(call_gate.lock.clone()))
                            })
                            .clone();
                        let generation = gate.begin();
                        let Some(stop) = self.messengers.with_interface_async(
                            messenger,
                            async move |interface| match interface.messenger().video() {
                                Ok(video) => video
                                    .stop_video(&room, kind)
                                    .await
                                    .map_err(|err| err.to_string()),
                                Err(err) => Err(err.to_string()),
                            },
                        ) else {
                            continue;
                        };
                        tasks.push(
                            Task::future(async move {
                                let _guard = gate.lock.lock().await;
                                if !gate.is_current(generation) {
                                    return None;
                                }
                                stop.await
                            })
                            .then(|result| match result {
                                Some(Err(error)) => Task::done(AppMessage::MediaError(error)),
                                Some(Ok(())) | None => Task::none(),
                            }),
                        );
                    }
                    tasks.extend(subscriptions.into_iter().filter_map(|(messenger, stream)| {
                        let unsubscribe = self.messengers.with_interface_async(
                            messenger,
                            async move |interface| {
                                let video = interface
                                    .messenger()
                                    .video()
                                    .map_err(|err| err.to_string())?;
                                video
                                    .unsubscribe_video(stream)
                                    .await
                                    .map_err(|err| err.to_string())
                            },
                        )?;
                        Some(
                            Task::future(async move {
                                unsubscribe
                                    .await
                                    .unwrap_or_else(|| Err("messenger closed".to_owned()))
                            })
                            .then(|result| match result {
                                Ok(()) => Task::none(),
                                Err(error) => Task::done(AppMessage::MediaError(error)),
                            }),
                        )
                    }));
                    self.media_error = None;
                    Task::batch(tasks)
                }
                settings::Message::RetryMessengerData => {
                    let ids = self.messengers.iter().map(|(id, _)| id).collect::<Vec<_>>();
                    let mut tasks = Vec::new();
                    for id in ids {
                        if self.startup_overflowed.contains(&id) {
                            tasks.push(self.restart_messenger(id));
                            continue;
                        }
                        let was_initialized = self.initialized_messengers.remove(&id);
                        // Uninitialized + started means fetch currently in
                        // flight. Do not create overlapping snapshot writes.
                        if !was_initialized && self.started_messengers.contains(&id) {
                            continue;
                        }
                        self.started_messengers.remove(&id);
                        if was_initialized {
                            self.pending_startup_events.remove(&id);
                        }
                    }
                    self.media_error = None;
                    tasks.push(Task::done(AppMessage::StartUp));
                    Task::batch(tasks)
                }
                settings::Message::SetCredentialStorage(mode) => {
                    let context = match mode {
                        login_store::StorageMode::SystemVault => {
                            "Failed to switch credentials to the OS secrets manager"
                        }
                        login_store::StorageMode::Plaintext => {
                            "Failed to switch credentials to plaintext storage"
                        }
                    };
                    if let Err(error) = self.persist_credentials_with_mode(None, None, Some(mode)) {
                        self.report_credential_error(context, &error);
                    }
                    Task::none()
                }
                settings::Message::ClearError => {
                    self.media_error = None;
                    Task::none()
                }
                settings::Message::ClearCredentialError => {
                    self.credential_error = None;
                    Task::none()
                }
            },
            // === Pages ===
            AppMessage::Login(message) => match self.login.update(message) {
                login::Action::None => Task::none(),
                login::Action::Cancel => {
                    self.abort_login_task();
                    Task::none()
                }
                login::Action::OpenSettings => {
                    self.screen = Screen::Settings;
                    Task::none()
                }
                login::Action::Login {
                    attempt,
                    backend,
                    credential,
                } => {
                    let task = Task::future(async move {
                        AppMessage::LoginFinished {
                            attempt,
                            result: create_verified_messenger(backend, credential).await,
                        }
                    });
                    self.track_login_task(attempt, task)
                }
                login::Action::InteractiveLogin {
                    attempt,
                    backend,
                    method,
                } => {
                    let task = Task::future(async move {
                        AppMessage::InteractiveAuthStarted {
                            attempt,
                            backend,
                            result: (method.start)().await.map_err(|error| error.to_string()),
                        }
                    });
                    self.track_login_task(attempt, task)
                }
            },
            AppMessage::InteractiveAuthStarted {
                attempt,
                backend,
                result,
            } => {
                if !self.login.is_pending(attempt) {
                    return Task::none();
                }
                self.clear_login_task(attempt);
                let session = match result {
                    Ok(session) => session,
                    Err(error) => {
                        error!("Interactive login failed: {error}");
                        self.login.fail(attempt, error);
                        return Task::none();
                    }
                };
                let InteractiveAuthSession { prompt, finish } = session;
                match self.login.show_interactive_prompt(attempt, prompt) {
                    Ok(true) => {}
                    Ok(false) => return Task::none(),
                    Err(error) => {
                        error!("Interactive login prompt failed: {error}");
                        self.login.fail(attempt, error);
                        return Task::none();
                    }
                }

                let task = Task::future(async move {
                    let result = match finish.await {
                        Ok(credential) => create_verified_messenger(backend, credential).await,
                        Err(error) => Err(error.to_string()),
                    };
                    AppMessage::LoginFinished { attempt, result }
                });
                self.track_login_task(attempt, task)
            }
            AppMessage::LoginFinished { attempt, result } => {
                if !self.login.is_pending(attempt) {
                    return Task::none();
                }
                self.clear_login_task(attempt);
                match result {
                    Ok(api) => {
                        self.login.finish(attempt);
                        Task::done(AppMessage::modify_messengers(move |messengers| {
                            messengers.add(api);
                        }))
                        .chain(Task::done(AppMessage::StartUp))
                    }
                    Err(error) => {
                        error!("Login failed: {error}");
                        self.login.fail(attempt, error);
                        Task::none()
                    }
                }
            }
            AppMessage::Chat(message) => {
                match self.messenger.update(message, &self.messengers) {
                    messenger::Action::None => Task::none(),
                    messenger::Action::OpenSettings => {
                        self.screen = Screen::Settings;
                        Task::none()
                    }
                    messenger::Action::Run(task) => task.map(AppMessage::Chat),
                    messenger::Action::ModifyMessengerData { id, modify } => {
                        Task::done(AppMessage::modify_data(id, modify))
                    }
                    messenger::Action::Call { messenger, channel } => {
                        let messenger_id = messenger;
                        if self.messengers.get(messenger_id).is_none() {
                            return Task::none();
                        }
                        let room_id = *channel.id();
                        let operation = (messenger_id, room_id);
                        let gate = self
                            .call_operations
                            .entry(operation)
                            .or_insert_with(|| Arc::new(OperationGate::default()))
                            .clone();
                        let generation = gate.begin();
                        // Register ownership before polling backend connect. Steam
                        // can emit CallStreamReady synchronously before `connect`
                        // returns; late insertion would discard that only stream.
                        if let Some(data) = self.messengers.data_mut(messenger_id) {
                            if let Some(call) =
                                data.calls.iter_mut().find(|call| call.id() == room_id)
                            {
                                call.set_state(CallState::Pending(CallStatus::Connecting(
                                    "Starting voice…",
                                )));
                            } else {
                                data.calls.push(messenger_unifier::Call::new(
                                    messenger_id,
                                    channel.clone(),
                                    CallState::Pending(CallStatus::Connecting("Starting voice…")),
                                ));
                            }
                        }
                        let room = channel.swap_data((*channel).clone());
                        let preferred_video_codec = self.preferred_video_codec;
                        let Some(connect) = self.messengers.with_interface_async(
                        messenger_id,
                        async move |interface| {
                            if let Ok(video) = interface.messenger().video() {
                                video
                                    .set_preferred_codec(preferred_video_codec)
                                    .await
                                    .map_err(|err| {
                                        format!(
                                            "Failed to select {preferred_video_codec:?} video: {err}"
                                        )
                                    })?;
                            }
                            let vc = interface
                                .messenger()
                                .audio()
                                .map_err(|err| format!("Voice not supported: {err:?}"))?;
                            vc.connect(&room)
                                .await
                                .map_err(|err| format!("Failed to connect voice: {err}"))
                        },
                    ) else {
                        return Task::none();
                    };
                        Task::future(async move {
                            let guard = gate.lock.lock().await;
                            let result = if gate.is_current(generation) {
                                Some(
                                    connect
                                        .await
                                        .unwrap_or_else(|| Err("messenger closed".to_owned())),
                                )
                            } else {
                                None
                            };
                            drop(guard);
                            AppMessage::CallConnectResult {
                                key: operation,
                                generation,
                                result,
                            }
                        })
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
                        let operation = (call.messenger_id(), call.id());
                        if self.messengers.get(operation.0).is_none() {
                            return Task::none();
                        }
                        let call_gate = self
                            .call_operations
                            .entry(operation)
                            .or_insert_with(|| Arc::new(OperationGate::default()))
                            .clone();
                        let gate = self
                            .voice_state_operations
                            .entry(operation)
                            .or_insert_with(|| {
                                Arc::new(OperationGate::with_lock(call_gate.lock.clone()))
                            })
                            .clone();
                        let generation = gate.begin();

                        // Apply optimistically so the toggle takes effect (and the
                        // buttons repaint) immediately — otherwise a press landing
                        // before the previous echo would compute from stale state
                        // (a double-click could never toggle off). The echo then
                        // confirms, or corrects if the server disagrees.
                        self.audio_controls.set_mic_muted(state.muted);
                        self.audio_controls.set_deafened(state.deafened);

                        let room = call.source().swap_data((**call.source()).clone());
                        let Some(set_state) = self.messengers.with_interface_async(
                            operation.0,
                            async move |interface| match interface
                                .messenger()
                                .audio()
                                .and_then(|voice| voice.self_state())
                            {
                                Ok(voice) => voice
                                    .set_self_voice_state(&room, state)
                                    .await
                                    .map_err(|err| format!("Failed to set voice state: {err}")),
                                Err(err) => {
                                    Err(format!("Setting call state not supported: {err:#?}"))
                                }
                            },
                        ) else {
                            return Task::none();
                        };
                        Task::future(async move {
                            let guard = gate.lock.lock().await;
                            let result = if gate.is_current(generation) {
                                Some(
                                    set_state
                                        .await
                                        .unwrap_or_else(|| Err("messenger closed".to_owned())),
                                )
                            } else {
                                None
                            };
                            drop(guard);
                            AppMessage::VoiceStateResult {
                                key: operation,
                                generation,
                                result,
                            }
                        })
                    }
                    messenger::Action::SetVideo {
                        call,
                        kind,
                        enabled,
                        input,
                    } => {
                        let input_key = (call.messenger_id(), kind);
                        let operation = (call.messenger_id(), call.id(), kind);
                        let call_operation = (call.messenger_id(), call.id());
                        let input = if enabled {
                            let Some(input) = input else {
                                warn!("Refusing to start {kind:?} without selected video input");
                                return Task::none();
                            };
                            Some(input)
                        } else {
                            None
                        };

                        let call_gate = self
                            .call_operations
                            .entry(call_operation)
                            .or_insert_with(|| Arc::new(OperationGate::default()))
                            .clone();
                        let gate = self
                            .video_operations
                            .entry(operation)
                            .or_insert_with(|| {
                                Arc::new(OperationGate::with_lock(call_gate.lock.clone()))
                            })
                            .clone();
                        let generation = gate.begin();
                        let input = input.map(|mut input| {
                            input.generation = generation;
                            input
                        });
                        let previous = match input {
                            Some(input) => self.video_inputs.insert(input_key, input),
                            None => self.video_inputs.remove(&input_key),
                        };

                        if self.messengers.get(call.messenger_id()).is_none() {
                            match previous {
                                Some(input) => {
                                    self.video_inputs.insert(input_key, input);
                                }
                                None => {
                                    self.video_inputs.remove(&input_key);
                                }
                            }
                            return Task::none();
                        }
                        let preferred_video_codec = self.preferred_video_codec;

                        let room = call.source().swap_data((**call.source()).clone());
                        let Some(toggle) = self.messengers.with_interface_async(
                            call.messenger_id(),
                            async move |interface| match interface.messenger().video() {
                                Ok(video) => {
                                    if enabled {
                                        video
                                            .set_preferred_codec(preferred_video_codec)
                                            .await
                                            .map_err(|err| {
                                                format!("Failed to toggle {kind:?} video: {err:#?}")
                                            })?;
                                        video.start_video(&room, kind).await.map_err(|err| {
                                            format!("Failed to toggle {kind:?} video: {err:#?}")
                                        })
                                    } else {
                                        video.stop_video(&room, kind).await.map_err(|err| {
                                            format!("Failed to toggle {kind:?} video: {err:#?}")
                                        })
                                    }
                                }
                                Err(err) => Err(format!("Video not supported: {err:#?}")),
                            },
                        ) else {
                            return Task::none();
                        };
                        Task::future(async move {
                            let guard = gate.lock.lock().await;
                            let result = if gate.is_current(generation) {
                                toggle
                                    .await
                                    .unwrap_or_else(|| Err("Messenger closed".to_owned()))
                            } else {
                                Ok(())
                            };
                            drop(guard);
                            AppMessage::VideoToggleResult {
                                operation,
                                input: input_key,
                                generation,
                                previous,
                                result,
                            }
                        })
                    }
                    messenger::Action::SubscribeVideo { messenger, stream } => {
                        let key = (messenger, stream);
                        let Some(subscribe) = self.messengers.with_interface_async(
                            messenger,
                            async move |interface| match interface.messenger().video() {
                                Ok(video) => video
                                    .subscribe_video(stream)
                                    .await
                                    .map_err(|err| err.to_string()),
                                Err(err) => Err(err.to_string()),
                            },
                        ) else {
                            return Task::none();
                        };
                        let Some(generation) = self.remote_video.begin_subscription(&key) else {
                            return Task::none();
                        };
                        Task::future(async move {
                            let result = subscribe
                                .await
                                .unwrap_or_else(|| Err("messenger closed".to_owned()));
                            AppMessage::VideoSubscriptionResult {
                                messenger,
                                stream,
                                generation,
                                result,
                            }
                        })
                    }
                    messenger::Action::DisconnectFromCall(call) => {
                        let messenger_id = call.messenger_id();
                        let room_id = call.id();
                        let operation = (messenger_id, room_id);
                        self.video_inputs.retain(|(id, _), _| *id != messenger_id);
                        if let Some(gate) = self.voice_state_operations.get(&operation) {
                            gate.invalidate();
                        }
                        let call_gate = self
                            .call_operations
                            .entry(operation)
                            .or_insert_with(|| Arc::new(OperationGate::default()))
                            .clone();
                        let call_generation = call_gate.begin();
                        if self.messengers.get(messenger_id).is_none() {
                            return Task::none();
                        }
                        let room = call.source().swap_data((**call.source()).clone());

                        // Remove immediately. A reconnect can be requested while
                        // disconnect waits; same operation lane then guarantees
                        // whichever user action came last owns backend state.
                        if let Some(data) = self.messengers.data_mut(messenger_id) {
                            data.calls.retain(|call| call.id() != room_id);
                        }

                        let video_operations = [
                            messenger_interface::interface::VideoBroadcastKind::Camera,
                            messenger_interface::interface::VideoBroadcastKind::Screen,
                        ]
                        .map(|kind| {
                            let video_operation = (messenger_id, room_id, kind);
                            let video_gate = self
                                .video_operations
                                .entry(video_operation)
                                .or_insert_with(|| {
                                    Arc::new(OperationGate::with_lock(call_gate.lock.clone()))
                                })
                                .clone();
                            let video_generation = video_gate.begin();
                            (video_operation, kind, video_gate, video_generation)
                        });
                        let call_gate_state = call_gate.clone();
                        let Some(disconnect) = self.messengers.with_interface_async(
                            messenger_id,
                            async move |interface| {
                                let mut errors = Vec::new();
                                for (_, kind, video_gate, video_generation) in video_operations {
                                    if video_gate.is_current(video_generation)
                                        && let Err(error) = match interface.messenger().video() {
                                            Ok(video) => video
                                                .stop_video(&room, kind)
                                                .await
                                                .map_err(|err| err.to_string()),
                                            Err(err) => Err(err.to_string()),
                                        }
                                    {
                                        errors.push(error);
                                    }
                                }
                                if call_gate_state.is_current(call_generation) {
                                    match interface.messenger().audio() {
                                        Ok(vc) => vc.disconnect(&room).await,
                                        Err(err) => {
                                            errors.push(format!("Voice not supported: {err:#?}"))
                                        }
                                    }
                                }
                                errors
                            },
                        ) else {
                            return Task::none();
                        };
                        Task::future(async move {
                            let guard = call_gate.lock.lock().await;
                            let errors = disconnect
                                .await
                                .unwrap_or_else(|| vec!["messenger closed".to_owned()]);
                            drop(guard);
                            AppMessage::CallDisconnectResult {
                                key: operation,
                                generation: call_generation,
                                errors,
                            }
                        })
                    }
                }
            }
        }
    }

    fn view(&self, _window: window::Id) -> Element<'_, AppMessage> {
        match self.screen {
            Screen::Loading => iced::widget::text("Loading").into(),
            Screen::Login => self.login.view().map(AppMessage::Login),
            Screen::Messenger => self
                .messenger
                .view(&self.messengers, &self.audio_controls, &self.remote_video)
                .map(AppMessage::Chat),
            Screen::Settings => {
                let logged_in_accounts = self
                    .messengers
                    .iter()
                    .map(|(id, entry)| settings::LoggedInAccount {
                        id,
                        backend: entry.interface.name().to_owned(),
                        display_name: entry
                            .data
                            .profile
                            .as_ref()
                            .map(|profile| profile.name.clone()),
                        tag: entry
                            .data
                            .profile
                            .as_ref()
                            .and_then(|profile| profile.tag.clone()),
                    })
                    .collect();
                let unavailable_accounts = self
                    .retained_credentials
                    .iter()
                    .map(|credential| settings::UnavailableAccount {
                        id: credential.id,
                        backend: credential.name.clone(),
                        error_code: credential.failure.code(),
                        error: credential.failure.detail().to_owned(),
                    })
                    .collect();
                settings::view(settings::Status {
                    selected_category: self.settings_category,
                    logged_in_accounts,
                    unavailable_accounts,
                    credential_store_writable: self.credential_store_writable
                        && self.credential_store_revision.is_some(),
                    credential_storage: self.credential_storage.as_ref(),
                    plaintext_storage_supported: login_store::plaintext_storage_supported(),
                    credential_error: self.credential_error.as_deref(),
                    input_streaming: self.audio.is_streaming_input(),
                    output_streaming: self.audio.is_streaming_output(),
                    active_remote_video: self.remote_video.streams.len(),
                    configured_video_inputs: self.video_inputs.len(),
                    preferred_video_codec: self.preferred_video_codec,
                    media_error: self.media_error.as_deref(),
                })
                .map(AppMessage::Settings)
            }
        }
    }

    fn subscription(&self) -> Subscription<AppMessage> {
        Subscription::none()
    }
}

#[cfg(test)]
mod operation_tests {
    use super::*;

    #[test]
    fn call_media_gates_share_one_serial_lane() {
        let call = Arc::new(OperationGate::default());
        let voice = OperationGate::with_lock(call.lock.clone());
        let video = OperationGate::with_lock(call.lock.clone());

        let guard = voice.lock.try_lock().expect("voice lane should start idle");
        assert!(call.lock.try_lock().is_none());
        assert!(video.lock.try_lock().is_none());
        drop(guard);
        assert!(call.lock.try_lock().is_some());
    }

    #[test]
    fn newer_generation_obsoletes_queued_operation() {
        let gate = OperationGate::default();
        let old = gate.begin();
        let current = gate.begin();

        assert!(!gate.is_current(old));
        assert!(gate.is_current(current));
    }

    #[test]
    fn failed_restore_keeps_exact_saved_credential() {
        let saved = RetainedCredential::new(
            "Discord",
            "token=secret",
            CredentialFailure::SessionRestoreFailed,
        );
        let saved_id = saved.id;
        let (restored, retained) = partition_restore_results(vec![Ok(7_u8), Err(saved)]);

        assert_eq!(restored, vec![7]);
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].id, saved_id);
        assert_eq!(retained[0].name, "Discord");
        assert_eq!(retained[0].auth.as_str(), "token=secret");
        assert_eq!(retained[0].failure.code(), "SESSION_RESTORE_FAILED");
        assert_eq!(
            retained[0].failure.detail(),
            "Saved credentials could not restore this session. Sign in again or remove the saved account."
        );
    }

    #[test]
    fn unknown_schema_or_backend_keeps_exact_saved_credential_with_error_code() {
        for (name, blob, error_code) in [
            (
                "Discord",
                "future-field=opaque%3Avalue",
                "CREDENTIAL_PARSE_FAILED",
            ),
            (
                "FutureBackend",
                "future-format:%not-parsed",
                "BACKEND_UNAVAILABLE",
            ),
        ] {
            let Err(retained) = parse_stored_credential(name, blob) else {
                panic!("unsupported saved credential unexpectedly parsed");
            };
            assert_eq!(retained.name, name);
            assert_eq!(retained.auth.as_str(), blob);
            assert_eq!(retained.failure.code(), error_code);
        }
    }
}
