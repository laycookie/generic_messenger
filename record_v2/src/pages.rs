pub mod login;
pub mod messenger;
pub mod settings;

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::Screen;
use futures::channel::oneshot;
use messenger_interface::auth::InteractiveAuthSession;
use messenger_interface::interface::{
    AudioEvent, CallStatus, Messenger, QueryEvent, TextEvent, VideoBroadcastKind, VideoEvent,
    VideoFrame, VideoResolution, VideoStreamId, VideoStreamMetadata, VoiceEvent,
};
use messenger_interface::owner::Owner;
use messenger_interface::types::{House, ID, Identifier, Place, Room, User};
use messenger_unifier::{MessengerId, MessengerRegistry};
use simple_video::CaptureSource;
use zeroize::Zeroizing;

static NEXT_RETAINED_CREDENTIAL_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) struct RetainedCredential {
    pub id: u64,
    pub name: String,
    pub auth: Zeroizing<String>,
    pub failure: CredentialFailure,
}

pub(crate) enum CredentialFailure {
    BackendUnavailable,
    CredentialParseFailed,
    SessionRestoreFailed,
}

impl CredentialFailure {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::BackendUnavailable => "BACKEND_UNAVAILABLE",
            Self::CredentialParseFailed => "CREDENTIAL_PARSE_FAILED",
            Self::SessionRestoreFailed => "SESSION_RESTORE_FAILED",
        }
    }

    pub const fn detail(&self) -> &'static str {
        match self {
            Self::BackendUnavailable => "Backend is unknown or not enabled in this build.",
            Self::CredentialParseFailed => {
                "Stored credential does not match the backend authentication schema."
            }
            Self::SessionRestoreFailed => {
                "Saved credentials could not restore this session. Sign in again or remove the saved account."
            }
        }
    }
}

impl RetainedCredential {
    pub fn new(name: &str, auth: &str, failure: CredentialFailure) -> Self {
        Self::from_owned(name.to_owned(), Zeroizing::new(auth.to_owned()), failure)
    }

    pub fn from_owned(name: String, auth: Zeroizing<String>, failure: CredentialFailure) -> Self {
        Self {
            id: NEXT_RETAINED_CREDENTIAL_ID.fetch_add(1, Ordering::Relaxed),
            name,
            auth,
            failure,
        }
    }
}

pub type RemoteVideoKey = (MessengerId, VideoStreamId);
pub type VideoInputKey = (MessengerId, VideoBroadcastKind);
pub type CallOperationKey = (MessengerId, ID);
pub type VideoOperationKey = (MessengerId, ID, VideoBroadcastKind);

/// Independently supervised long-lived backend event route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ListenerKind {
    Query,
    Text,
    Voice,
    Video,
}

/// One independently retried initial state snapshot. App commits it only when
/// every domain succeeded and no socket-journal overflow invalidated attempt.
pub(crate) struct StartupSnapshot {
    pub profile: Result<Identifier<User>, String>,
    pub contacts: Result<Vec<Identifier<User>>, String>,
    pub conversations: Result<Vec<Identifier<Place<Room>>>, String>,
    pub servers: Result<Vec<Identifier<Place<House>>>, String>,
}

/// Application-owned source selected for one outgoing video kind.
#[derive(Clone, Debug)]
pub enum VideoInputSource {
    CyanTestPattern,
    Capture(CaptureSource),
}

impl VideoInputSource {
    pub fn label(&self) -> String {
        match self {
            Self::CyanTestPattern => "Test: cyan screen".to_owned(),
            Self::Capture(CaptureSource::Webcam(source)) => {
                format!("Webcam: {}", source.name())
            }
            Self::Capture(CaptureSource::Screen(source)) => {
                format!("Screen: {}", source.title())
            }
            Self::Capture(source) => source.name().to_owned(),
        }
    }
}

/// Local source and requested capture cadence selected before broadcasting.
#[derive(Clone, Debug)]
pub struct SelectedVideoInput {
    pub source: VideoInputSource,
    pub frames_per_second: u32,
    pub resolution: VideoResolution,
    pub bitrate_bps: u32,
    /// Backend toggle generation which installed this source. Capture-thread
    /// failures carry it back so an old device cannot clear its replacement.
    pub(crate) generation: u64,
}

/// UI-side lifecycle for one advertised/active remote video source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteVideoStatus {
    /// Backend requires a user action before media is requested.
    Available,
    /// User pressed Watch; backend subscription is being established.
    Requested,
    /// Backend emitted `AddVideoStream`; frames may now arrive.
    Active,
}

/// Application-owned state for one advertised or active remote stream.
pub struct RemoteVideoStream {
    pub metadata: VideoStreamMetadata,
    pub status: RemoteVideoStatus,
    pub preview: Option<iced::widget::image::Handle>,
    /// This app explicitly asked the backend to receive this stream. Directly
    /// delivered streams must not be passed to `unsubscribe_video`, whose
    /// default implementation reports unsupported.
    subscription_owned: bool,
    /// Generation of the explicit subscribe request which owns this entry.
    /// Separate from frame-channel generation: either future can complete
    /// after this key has been removed and recreated.
    subscription_generation: Option<u64>,
    generation: Option<u64>,
    cancel_preview: Option<oneshot::Sender<()>>,
}

/// Remote video registry. Metadata, lifecycle, preview, and generation share
/// one entry so parallel maps cannot drift out of sync.
#[derive(Default)]
pub struct RemoteVideo {
    pub streams: HashMap<RemoteVideoKey, RemoteVideoStream>,
    next_generation: u64,
}

impl RemoteVideo {
    pub fn available(&mut self, key: RemoteVideoKey, metadata: VideoStreamMetadata) {
        self.streams
            .entry(key)
            .and_modify(|stream| stream.metadata = metadata)
            .or_insert(RemoteVideoStream {
                metadata,
                status: RemoteVideoStatus::Available,
                preview: None,
                subscription_owned: false,
                subscription_generation: None,
                generation: None,
                cancel_preview: None,
            });
    }

    /// Transition an advertised stream into Requested and return its request
    /// generation. The completion must carry this generation back.
    pub fn begin_subscription(&mut self, key: &RemoteVideoKey) -> Option<u64> {
        if self.streams.get(key)?.status != RemoteVideoStatus::Available {
            return None;
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let stream = self.streams.get_mut(key)?;
        stream.status = RemoteVideoStatus::Requested;
        stream.subscription_generation = Some(generation);
        Some(generation)
    }

    pub fn is_current_subscription(&self, key: &RemoteVideoKey, generation: u64) -> bool {
        self.streams
            .get(key)
            .is_some_and(|stream| stream.subscription_generation == Some(generation))
    }

    pub fn subscription_failed(&mut self, key: &RemoteVideoKey, generation: u64) {
        let Some(stream) = self.streams.get_mut(key) else {
            return;
        };
        if stream.subscription_generation != Some(generation) {
            return;
        }
        stream.subscription_generation = None;
        if stream.status == RemoteVideoStatus::Requested {
            stream.status = RemoteVideoStatus::Available;
        }
    }

    /// Mark a fresh frame channel active and return its UI generation. Late
    /// frames/end messages from a replaced channel can then be ignored.
    pub fn start(
        &mut self,
        key: RemoteVideoKey,
        metadata: VideoStreamMetadata,
    ) -> (u64, oneshot::Receiver<()>) {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let previous = self.streams.remove(&key);
        let subscription_owned = previous.as_ref().is_some_and(|stream| {
            stream.subscription_owned || stream.status == RemoteVideoStatus::Requested
        });
        let subscription_generation = previous
            .as_ref()
            .and_then(|stream| stream.subscription_generation);
        if let Some(cancel) = previous.and_then(|stream| stream.cancel_preview) {
            let _ = cancel.send(());
        }
        let (cancel_preview, cancelled) = oneshot::channel();
        self.streams.insert(
            key,
            RemoteVideoStream {
                metadata,
                status: RemoteVideoStatus::Active,
                preview: None,
                subscription_owned,
                subscription_generation,
                generation: Some(generation),
                cancel_preview: Some(cancel_preview),
            },
        );
        (generation, cancelled)
    }

    pub fn is_current(&self, key: &RemoteVideoKey, generation: u64) -> bool {
        self.streams
            .get(key)
            .is_some_and(|stream| stream.generation == Some(generation))
    }

    pub fn remove(&mut self, key: &RemoteVideoKey) {
        if let Some(stream) = self.streams.remove(key)
            && let Some(cancel) = stream.cancel_preview
        {
            let _ = cancel.send(());
        }
    }

    pub fn remove_messenger(&mut self, messenger: MessengerId) {
        let keys = self
            .streams
            .keys()
            .filter(|(owner, _)| *owner == messenger)
            .copied()
            .collect::<Vec<_>>();
        for key in keys {
            self.remove(&key);
        }
    }

    /// Clear UI state without reusing generations. Reusing zero after reset
    /// lets late preview events from an old stream mutate its replacement.
    pub fn clear(&mut self) {
        for (_, stream) in self.streams.drain() {
            if let Some(cancel) = stream.cancel_preview {
                let _ = cancel.send(());
            }
        }
    }

    pub fn owned_subscriptions(&self) -> impl Iterator<Item = RemoteVideoKey> + '_ {
        self.streams.iter().filter_map(|(key, stream)| {
            (stream.subscription_owned || stream.status == RemoteVideoStatus::Requested)
                .then_some(*key)
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub enum StreamDirection {
    Input,
    Output,
}

impl std::fmt::Display for StreamDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input => f.write_str("input"),
            Self::Output => f.write_str("output"),
        }
    }
}

pub(crate) enum AppMessage {
    // === Navigation ===
    Navigate(Screen),
    // === Messenger Data ===
    ModifyMessengers(Box<dyn FnOnce(&mut MessengerRegistry) + Send>),
    // === Lifecycle ===
    StartUp,
    StartupSnapshot {
        messenger: MessengerId,
        snapshot: Box<StartupSnapshot>,
    },
    RestartMessenger(MessengerId),
    MessengerRestarted {
        messenger: MessengerId,
        result: Result<Owner<dyn Messenger>, String>,
    },
    ListenerStopped {
        messenger: MessengerId,
        kind: ListenerKind,
        error: Option<String>,
        retry: bool,
    },
    CredentialsRestored {
        messengers: Vec<Owner<dyn Messenger>>,
        retained: Vec<RetainedCredential>,
    },
    SaveCredentials,
    // === Audio ===
    StartStream(StreamDirection),
    StopStream {
        direction: StreamDirection,
        generation: u64,
    },
    /// Last recoverable media failure shown in Settings.
    MediaError(String),
    // === Pages ===
    Login(login::Message),
    InteractiveAuthStarted {
        attempt: u64,
        backend: &'static str,
        result: Result<InteractiveAuthSession, String>,
    },
    LoginFinished {
        attempt: u64,
        result: Result<Owner<dyn Messenger>, String>,
    },
    Chat(messenger::Message),
    Settings(settings::Message),
    // === Socket ===
    QueryEvent((MessengerId, QueryEvent)),
    TextEvent((MessengerId, TextEvent)),
    VoiceEvent((MessengerId, VoiceEvent)),
    AudioEvent((MessengerId, AudioEvent)),
    VideoEvent((MessengerId, VideoEvent)),
    // === Video debug previews ===
    /// A fresh frame for the preview of one remote video stream.
    VideoPreview {
        messenger: MessengerId,
        stream: VideoStreamId,
        generation: u64,
        frame: VideoFrame,
    },
    /// The stream's frame channel closed — drop its preview.
    VideoPreviewEnded {
        messenger: MessengerId,
        stream: VideoStreamId,
        generation: u64,
    },
    /// Result of an explicit remote-video subscription request.
    VideoSubscriptionResult {
        messenger: MessengerId,
        stream: VideoStreamId,
        generation: u64,
        result: Result<(), String>,
    },
    CallConnectResult {
        key: CallOperationKey,
        generation: u64,
        result: Option<Result<CallStatus, String>>,
    },
    CallDisconnectResult {
        key: CallOperationKey,
        generation: u64,
        errors: Vec<String>,
    },
    VoiceStateResult {
        key: CallOperationKey,
        generation: u64,
        result: Option<Result<(), String>>,
    },
    /// Completion of one outgoing-video operation. Generation prevents a slow
    /// earlier request from rolling back a newer user choice.
    VideoToggleResult {
        operation: VideoOperationKey,
        input: VideoInputKey,
        generation: u64,
        previous: Option<SelectedVideoInput>,
        result: Result<(), String>,
    },
    VideoCaptureResult {
        key: VideoInputKey,
        generation: u64,
        result: Result<(), String>,
    },
}

impl AppMessage {
    pub fn modify_messengers(f: impl FnOnce(&mut MessengerRegistry) + Send + 'static) -> Self {
        Self::ModifyMessengers(Box::new(f))
    }

    pub fn modify_data(
        id: MessengerId,
        f: impl FnOnce(&mut messenger_unifier::MessengerData) + Send + 'static,
    ) -> Self {
        Self::modify_messengers(move |messengers| {
            if let Some(entry) = messengers.get_mut(id) {
                f(&mut entry.data);
            }
        })
    }
}
