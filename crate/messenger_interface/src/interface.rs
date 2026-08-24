//! Messenger backend traits, flattened from feature submodules.
//!
//! Specialization resolves optional whole features and operations from concrete
//! backend type without probing. Event polls own `Retained<Self>` access so
//! committed work may outlive caller cancellation. Backend errors stay erased.
use std::error::Error;
use std::fmt::Debug;

use crate::auth::{AuthMethod, Credential, InteractiveAuthMethod};
use crate::owner::Owner;

pub use crate::stream::{ArcStream, LeaseStream, StreamScope, WeakSocketStream, WeakStreamSource};

use async_trait::async_trait;
use facet::Facet;

mod audio;
mod caster;
mod query;
mod text;
mod video;

use caster::capability_caster;

pub use audio::{
    Audio, AudioCasterSelfState, AudioEvent, AudioSelfVoiceState, CallState, CallStatus, VoiceEvent,
};
pub use query::{Query, QueryEvent};
pub use text::{
    Ordering, Text, TextCancelSend, TextCasterCancel, TextCasterDelete, TextCasterEdit,
    TextCasterReactions, TextDelete, TextEdit, TextEvent, TextReactions,
};
pub use video::{
    FrameConsumer, FrameProducer, FrameScaler, PixelData, Video, VideoBroadcastKind, VideoCodec,
    VideoEvent, VideoFrame, VideoFrameTiming, VideoInput, VideoResolution, VideoStreamId,
    VideoStreamMetadata, frame_channel,
};

#[derive(Debug, Facet)]
#[facet(derive(Error))]
#[repr(u8)]
pub enum MessengerError {
    NotImplemented,
    Requires,
}

capability_caster!(
    /// Capability: query (fetch) state from the messenger.
    MessengerCasterQuery: Messenger => Query { query, retained_query }
    /// Capability: text chat integration.
    MessengerCasterText: Messenger => Text { text, retained_text }
    /// Capability: audio (voice call) integration.
    MessengerCasterAudio: Messenger => Audio { audio, retained_audio }
    /// Capability: video (webcam / screen share) integration.
    MessengerCasterVideo: Messenger => Video { video, retained_video }
);

/// Concrete backend exposing optional feature traits through generated casters.
#[async_trait]
pub trait Messenger: Send + Sync
where
    Self: MessengerCasterQuery + MessengerCasterText + MessengerCasterAudio + MessengerCasterVideo,
{
    /// Builds an authenticated backend. Coerce `Owner<Self>` to
    /// `Owner<dyn Messenger>` before creating stream leases.
    async fn create_messenger(
        cred: Credential,
    ) -> Result<Owner<dyn Messenger>, Box<dyn Error + Send + Sync>>
    where
        Self: Sized;
    /// Login methods exposed to generated UI.
    fn auth_methods() -> &'static [AuthMethod]
    where
        Self: Sized;
    /// Interactive login methods this backend offers, such as QR approval.
    fn interactive_auth_methods() -> &'static [InteractiveAuthMethod]
    where
        Self: Sized,
    {
        &[]
    }
    /// Human-readable backend name (e.g. `"discord"`).
    fn name(&self) -> &'static str;
    /// Stable unique id for this backend instance (e.g. includes account/server context).
    fn id(&self) -> String;
    /// Credential persisted and restored through [`Self::create_messenger`].
    fn auth(&self) -> Credential;
}

impl PartialEq for dyn Messenger {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}
