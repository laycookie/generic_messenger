//! Video feature area ([`Video`]) — webcam and screen-share streams — the
//! frame channel types, and the realtime [`VideoEvent`]s.
//!
//! Video is its own feature area, deliberately *not* a sub-capability of
//! [`Audio`](super::audio::Audio): a backend can offer video streams without
//! audio calls. Backends that carry both over one connection (e.g. Discord,
//! where camera video rides the voice call's RTC connection) merge them
//! internally — `connect` on either trait resolves to the shared session, and
//! connecting the second feature in the same `location` is a cheap no-op.
//!
//! One trait covers every kind of stream a participant can broadcast;
//! [`VideoBroadcastKind`] says which one a given stream is, so webcam and screen
//! share don't need parallel APIs.

use std::{error::Error, sync::Arc};

use async_trait::async_trait;
use futures::channel::oneshot;
pub use simple_video::{
    FrameConsumer, FrameProducer, FrameScaler, PixelData, VideoCodec, VideoFrame, VideoFrameTiming,
    frame_channel,
};

use super::MessengerError;
use super::audio::CallStatus;
use crate::stream::WeakSocketStream;
use crate::types::{ID, Identifier, Place, Room, VideoBroadcast};

/// What a video stream shows. Backends that support only one kind simply
/// never emit or accept the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoBroadcastKind {
    /// Webcam.
    Camera,
    /// Screen/window capture (e.g. Discord "Go Live").
    Screen,
}

impl From<VideoBroadcastKind> for VideoBroadcast {
    fn from(kind: VideoBroadcastKind) -> Self {
        match kind {
            VideoBroadcastKind::Camera => VideoBroadcast::CAMERA,
            VideoBroadcastKind::Screen => VideoBroadcast::SCREENSHARE,
        }
    }
}

/// Opaque identity of one remote video stream.
///
/// IDs are scoped to one backend instance. Every simultaneously active stream
/// must have a distinct ID, including multiple streams owned by the same user.
/// A stopped and restarted broadcast should receive a fresh ID so delayed
/// lifecycle events cannot affect its replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoStreamId(ID);

impl VideoStreamId {
    pub const fn new(id: ID) -> Self {
        Self(id)
    }

    pub const fn get(self) -> ID {
        self.0
    }
}

/// Immutable display and routing context announced for a video stream.
///
/// This information does not participate in stream identity. Applications
/// store it beside [`VideoStreamId`] while the stream is available; backend
/// subscription methods receive only the ID and resolve native connection
/// state from their private active-stream registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoStreamMetadata {
    pub user: Option<ID>,
    pub location: Option<ID>,
    pub kind: VideoBroadcastKind,
}

impl VideoStreamMetadata {
    pub const fn new(user: Option<ID>, location: Option<ID>, kind: VideoBroadcastKind) -> Self {
        Self {
            user,
            location,
            kind,
        }
    }
}

/// UI-selected capture and encode bound for one outgoing video broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoResolution {
    pub width: u32,
    pub height: u32,
}

impl VideoResolution {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// Application-provided source for one outgoing video broadcast.
///
/// `frames_per_second` is both capture cadence and backend encoding target;
/// `resolution` and `bitrate_bps` are UI-selected encode limits.
#[non_exhaustive]
pub struct VideoInput {
    pub frames: FrameConsumer,
    pub frames_per_second: u32,
    pub resolution: VideoResolution,
    pub bitrate_bps: u32,
}

impl VideoInput {
    pub fn new(
        frames: FrameConsumer,
        frames_per_second: u32,
        resolution: VideoResolution,
        bitrate_bps: u32,
    ) -> Self {
        Self {
            frames,
            frames_per_second,
            resolution,
            bitrate_bps,
        }
    }
}

/// Video stream lifecycle API.
///
/// Mirrors [`Audio`](super::audio::Audio)'s connection shape (`connect` /
/// `disconnect` / `listen`) so the two feature areas stay symmetric; a
/// backend sharing one transport implements both on the same session.
///
/// Broadcasting is signal-first: `start_video` announces the stream to the
/// call, and the frames themselves are requested out-of-band via
/// [`VideoEvent::AddVideoInput`] — the app owns the capture devices, the
/// backend owns encoding and transport (the same split as microphone audio).
///
/// The own mute/deafen flags stay on [`super::AudioSelfVoiceState`]; toggling them
/// must not affect an active video broadcast, and vice versa.
#[async_trait]
pub trait Video: Send + Sync {
    /// Prefer `codec` for future outgoing streams and request a negotiated
    /// switch for active streams when the backend supports one.
    ///
    /// Backends without selectable encoders may ignore this hint.
    async fn set_preferred_codec(
        &self,
        _codec: VideoCodec,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        Ok(())
    }

    /// Connect to video in `location`. On backends where video shares the
    /// audio call's connection, this resolves to that session (and is a
    /// no-op when [`Audio::connect`](super::audio::Audio::connect) already
    /// established it).
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>>;

    /// Disconnect from video in `location`. On a shared connection this ends
    /// only the video streams, not the audio call.
    async fn disconnect(&self, location: &Identifier<Place<Room>>);

    /// Start broadcasting own video of `kind` in `location`.
    async fn start_video(
        &self,
        location: &Identifier<Place<Room>>,
        kind: VideoBroadcastKind,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;

    /// Stop broadcasting own video of `kind` in `location`.
    async fn stop_video(
        &self,
        location: &Identifier<Place<Room>>,
        kind: VideoBroadcastKind,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;

    /// Ask the backend to begin receiving an advertised remote stream.
    ///
    /// Backends call this only indirectly: they first emit
    /// [`VideoEvent::VideoStreamAvailable`], the UI presents an explicit watch
    /// action, and that action calls this method. Protocol details such as a
    /// Discord Stream Watch or equivalent native subscription stay inside
    /// the backend. Once delivery begins, the backend emits
    /// [`VideoEvent::AddVideoStream`].
    ///
    /// Backends whose media arrives automatically skip the availability event,
    /// emit `AddVideoStream` directly, and may keep this default implementation.
    async fn subscribe_video(
        &self,
        _stream: VideoStreamId,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Stop receiving a stream previously started by [`Self::subscribe_video`].
    /// An adapter may also stop delivery when the app drops the frame consumer,
    /// but this explicit operation lets UIs release native subscriptions before
    /// the remote broadcast itself ends.
    async fn unsubscribe_video(
        &self,
        _stream: VideoStreamId,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Subscribe to video lifecycle and frame-channel handshake events.
    ///
    /// This stream carries stream availability plus requests to attach frame
    /// channels; frame bytes travel through [`frame_channel`], not as
    /// `VideoEvent` values. Intended for one long-lived consumer per backend.
    /// The returned weak stream does not keep the backend alive. The default
    /// reports [`MessengerError::NotImplemented`].
    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<VideoEvent>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
}

/// Updates about video streams in connected calls.
pub enum VideoEvent {
    /// A remote stream exists, but this backend will not deliver its media
    /// until the app calls [`Video::subscribe_video`] in response to a UI
    /// action. Repeated advertisements for the same stream are idempotent.
    ///
    /// Backends that receive streams automatically do not emit this event;
    /// they emit [`VideoEvent::AddVideoStream`] when delivery starts.
    VideoStreamAvailable {
        id: VideoStreamId,
        metadata: VideoStreamMetadata,
    },
    /// A remote stream is now delivering frames. Reply with the producer half
    /// of a [`frame_channel`] to display it on the consumer half. Apps should
    /// respond immediately: any user choice already happened at
    /// [`VideoEvent::VideoStreamAvailable`], when one was required. Metadata
    /// is included because automatic-delivery backends may skip availability.
    AddVideoStream {
        id: VideoStreamId,
        metadata: VideoStreamMetadata,
        channel: oneshot::Sender<FrameProducer>,
    },
    /// A remote stream stopped being available. Removes both an unaccepted
    /// advertisement and an active stream; its frame channel also closes.
    RemoveVideoStream { id: VideoStreamId },
    /// Request local video input (camera / screen capture) for an own
    /// broadcast started via [`Video::start_video`]. Reply with the consumer
    /// half of a [`frame_channel`] plus its capture frame rate and push
    /// captured frames into the producer; drop the sender to abort the
    /// broadcast.
    AddVideoInput {
        kind: VideoBroadcastKind,
        channel: oneshot::Sender<VideoInput>,
    },
}

#[cfg(test)]
mod tests {
    use super::super::Messenger;
    use super::*;
    use crate::auth::{AuthMethod, Credential};

    /// Implements only the [`Messenger`] core.
    struct NoVideo;
    /// Implements the core plus the video feature area.
    struct WithVideo;

    macro_rules! impl_messenger_core {
        ($ty:ty) => {
            #[async_trait]
            impl Messenger for $ty {
                async fn create_messenger(
                    _: Credential,
                ) -> Result<Arc<dyn Messenger>, Box<dyn Error + Send + Sync>> {
                    unimplemented!()
                }
                fn auth_methods() -> &'static [AuthMethod] {
                    &[]
                }
                fn name(&self) -> &'static str {
                    "mock"
                }
                fn id(&self) -> String {
                    String::new()
                }
                fn auth(&self) -> Credential {
                    Credential(Vec::new())
                }
            }
        };
    }
    impl_messenger_core!(NoVideo);
    impl_messenger_core!(WithVideo);

    #[async_trait]
    impl Video for WithVideo {
        async fn connect(
            &self,
            _: &Identifier<Place<Room>>,
        ) -> Result<CallStatus, Box<dyn Error + Sync + Send>> {
            unimplemented!()
        }
        async fn disconnect(&self, _: &Identifier<Place<Room>>) {
            unimplemented!()
        }
        async fn start_video(
            &self,
            _: &Identifier<Place<Room>>,
            _: VideoBroadcastKind,
        ) -> Result<(), Box<dyn Error + Sync + Send>> {
            unimplemented!()
        }
        async fn stop_video(
            &self,
            _: &Identifier<Place<Room>>,
            _: VideoBroadcastKind,
        ) -> Result<(), Box<dyn Error + Sync + Send>> {
            unimplemented!()
        }
    }

    /// Same contract as the other feature-area casters: resolution is per
    /// concrete type and survives erasure to `dyn Messenger`.
    #[test]
    fn video_caster_resolves_by_concrete_type() {
        let plain: Arc<dyn Messenger> = Arc::new(NoVideo);
        assert!(plain.video().is_err());
        assert!(plain.arc_video().is_err());

        let video: Arc<dyn Messenger> = Arc::new(WithVideo);
        assert!(video.video().is_ok());
        assert!(video.arc_video().is_ok());
    }
}
