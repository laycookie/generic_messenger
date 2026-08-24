//! Video lifecycle, frame channels, and realtime events.
//!
//! Video remains independent from audio, though backends may share transport.
//! [`VideoBroadcastKind`] distinguishes camera and screen streams.

use std::error::Error;

use async_trait::async_trait;
use futures::channel::oneshot;
pub use simple_video::{
    FrameConsumer, FrameProducer, FrameScaler, PixelData, VideoCodec, VideoFrame, VideoFrameTiming,
    frame_channel,
};

use super::MessengerError;
use super::audio::CallStatus;
use crate::types::{ID, Identifier, Place, Room, VideoBroadcast};

/// Video broadcast content type.
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

/// Backend-scoped remote stream identity.
/// Concurrent and restarted streams require distinct IDs.
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

/// Immutable stream metadata, separate from [`VideoStreamId`] identity.
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

/// Application source and encoding limits for outgoing video.
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

/// Video connection, broadcast, and subscription lifecycle.
/// App owns capture; backend requests frames through [`VideoEvent::AddVideoInput`].
#[async_trait]
pub trait Video: Send + Sync {
    /// Prefers codec for future and active streams when supported.
    async fn set_preferred_codec(
        &self,
        _codec: VideoCodec,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        Ok(())
    }

    /// Connects video, reusing shared audio transport when applicable.
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>>;

    /// Disconnects video without ending shared audio transport.
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

    /// Starts receiving an advertised stream.
    /// Automatic-delivery backends may skip availability and this method.
    async fn subscribe_video(
        &self,
        _stream: VideoStreamId,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Releases a subscription before remote broadcast ends.
    async fn unsubscribe_video(
        &self,
        _stream: VideoStreamId,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Prepare this backend's video event source.
    async fn prepare_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Polls next prepared event.
    fn next_event(self: crate::owner::Retained<Self>) -> crate::stream::EventFuture<VideoEvent> {
        Box::pin(async { None })
    }
}

/// Updates about video streams in connected calls.
pub enum VideoEvent {
    /// Remote stream awaiting explicit subscription; repeats are idempotent.
    VideoStreamAvailable {
        id: VideoStreamId,
        metadata: VideoStreamMetadata,
    },
    /// Remote frames started; reply with [`frame_channel`] producer.
    AddVideoStream {
        id: VideoStreamId,
        metadata: VideoStreamMetadata,
        channel: oneshot::Sender<FrameProducer>,
    },
    /// Removes advertised or active remote stream.
    RemoveVideoStream { id: VideoStreamId },
    /// Requests local input for own broadcast; dropping channel aborts it.
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
    use crate::owner::Owner;

    struct NoVideo;
    struct WithVideo;

    macro_rules! impl_messenger_core {
        ($ty:ty) => {
            #[async_trait]
            impl Messenger for $ty {
                async fn create_messenger(
                    _: Credential,
                ) -> Result<Owner<dyn Messenger>, Box<dyn Error + Send + Sync>> {
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

    #[test]
    fn video_caster_resolves_by_concrete_type() {
        let plain: Owner<dyn Messenger> = Owner::new(NoVideo);
        assert!(plain.video().is_err());
        let plain_retained = plain
            .downgrade()
            .retain()
            .expect("owner must remain active");
        assert!(plain_retained.retained_video().is_err());

        let video: Owner<dyn Messenger> = Owner::new(WithVideo);
        assert!(video.video().is_ok());
        let video_retained = video
            .downgrade()
            .retain()
            .expect("owner must remain active");
        assert!(video_retained.retained_video().is_ok());
    }
}
