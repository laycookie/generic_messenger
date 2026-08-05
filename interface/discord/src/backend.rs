use std::{
    marker::PhantomData,
    sync::{
        Arc, Weak,
        atomic::{AtomicU8, Ordering, fence},
    },
};

use bitflags::bitflags;
use futures::lock::Mutex as AsyncMutex;
use messenger_interface::{
    interface::Messenger,
    stream::{ArcStream, WeakSocketStream},
};
use secure_string::SecureString;
use simple_video::VideoCodec;
use unitstruct::unit_struct;

use crate::{
    UnitStruct, adapter, cache::DiscordCache, gateways::GatewaySupervisor, rest_api::DiscordApi,
};

pub(crate) const INTERFACE_NAME: &str = "Discord";
/// <https://docs.discord.food/reference#api-versioning>
pub(crate) const DISCORD_API: &str = "https://discord.com/api/v10";

bitflags! {
    /// <https://docs.discord.food/gateway/using-gateway#gateway-intents>
    pub(crate) struct Intents: u32 {
        const GUILDS                    = 1 << 0;
        const GUILD_MODERATION          = 1 << 2;
        const GUILD_EXPRESSIONS         = 1 << 3;
        const GUILD_INTEGRATIONS        = 1 << 4;
        const GUILD_WEBHOOKS            = 1 << 5;
        const GUILD_INVITES             = 1 << 6;
        const GUILD_VOICE_STATES        = 1 << 7;
        const GUILD_PRESENCES           = 1 << 8;
        const GUILD_MESSAGES            = 1 << 9;
        const GUILD_MESSAGE_REACTIONS   = 1 << 10;
        const DIRECT_MESSAGES           = 1 << 12;
        const DIRECT_MESSAGE_REACTIONS  = 1 << 13;
        const DIRECT_MESSAGE_TYPING     = 1 << 14;
        const MESSAGE_CONTENT           = 1 << 15;
        const AUTO_MODERATION_CONFIGURATION = 1 << 17;
    }
}
const DEFAULT_INTENTS: Intents = Intents::all();

bitflags! {
    /// <https://docs.discord.food/gateway/using-gateway#gateway-capabilities>
    pub(crate) struct Capabilities: u32 {
        const LAZY_USER_NOTES                        = 1 << 0;
        const NO_AFFINE_USER_IDS                     = 1 << 1;
        const VERSIONED_READ_STATES                  = 1 << 2;
        const VERSIONED_USER_GUILD_SETTINGS          = 1 << 3;
        const DEDUPE_USER_OBJECTS                    = 1 << 4;
        const PRIORITIZED_READY_PAYLOAD               = 1 << 5;
        const MULTIPLE_GUILD_EXPERIMENT_POPULATIONS  = 1 << 6;
        const NON_CHANNEL_READ_STATES                 = 1 << 7;
        const AUTH_TOKEN_REFRESH                      = 1 << 8;
        const USER_SETTINGS_PROTO                     = 1 << 9;
        const CLIENT_STATE_V2                         = 1 << 10;
        const PASSIVE_GUILD_UPDATE                    = 1 << 11;
        const AUTO_CALL_CONNECT                       = 1 << 12;
        const DEBOUNCE_MESSAGE_REACTIONS              = 1 << 13;
        const PASSIVE_GUILD_UPDATE_V2                 = 1 << 14;
        const CHANNEL_OBFUSCATION                     = 1 << 15;
        const AUTO_LOBBY_CONNECT                      = 1 << 16;
    }
}
const DEFAULT_CAPABILITIES: Capabilities = Capabilities::CLIENT_STATE_V2;

#[unit_struct]
pub(crate) struct Owned;

/// Backend-local media resources. Stored inline, not behind another `Arc`, so
/// grouping them adds no allocation or indirection.
#[derive(Default)]
pub(crate) struct LocalMediaState {
    pub(crate) audio_manager: AsyncMutex<adapter::audio::AudioManager>,
    pub(crate) video_streams: std::sync::Mutex<adapter::video::DiscordVideoStreams>,
    /// User preference sent in codec priority order. Zero intentionally maps
    /// to H264 so derived/default application state remains compatible.
    preferred_video_codec: AtomicU8,
}

impl LocalMediaState {
    const VIDEO_CODEC_H264: u8 = 0;
    const VIDEO_CODEC_AV1: u8 = 1;

    pub(crate) fn preferred_video_codec(&self) -> VideoCodec {
        match self.preferred_video_codec.load(Ordering::Acquire) {
            Self::VIDEO_CODEC_AV1 => VideoCodec::Av1,
            Self::VIDEO_CODEC_H264 => VideoCodec::H264,
            _ => VideoCodec::H264,
        }
    }

    /// Store known codec preference, returning whether value changed. Future
    /// `VideoCodec` variants remain rejected until Discord support is defined.
    pub(crate) fn set_preferred_video_codec(&self, codec: VideoCodec) -> Option<bool> {
        let codec = match codec {
            VideoCodec::H264 => Self::VIDEO_CODEC_H264,
            VideoCodec::Av1 => Self::VIDEO_CODEC_AV1,
            _ => return None,
        };
        Some(self.preferred_video_codec.swap(codec, Ordering::AcqRel) != codec)
    }
}

/// Discord state plus capability marker. Each subsystem owns persistent state
/// exactly once; borrowed operation contexts coordinate fields without copying
/// them.
#[repr(C)]
pub(crate) struct InnerDiscord<T: UnitStruct> {
    /// HTTP transport and credential only. Never owns or mutates cache state.
    pub(crate) api: DiscordApi,
    /// Canonical gateway-derived state plus compact routing indexes.
    pub(crate) cache: DiscordCache,
    /// Main gateway connection and stream lifecycle.
    pub(crate) gateway: GatewaySupervisor,
    /// Microphone and logical video resources shared by media control paths.
    pub(crate) media: LocalMediaState,
    _marker: PhantomData<T>,
}

impl<T: UnitStruct> InnerDiscord<T> {
    /// True when every remaining strong reference is held by in-flight
    /// `next()` futures themselves — i.e. the app dropped its last
    /// `Arc<dyn Messenger>` / capability handle and the streams are the
    /// only thing keeping this alive.
    ///
    /// Must be called from inside `next()` (which contributes one counted
    /// reference) while being polled through `WeakSocketStream` (whose
    /// transient upgrade contributes the `+ 1`). Counts are maintained so
    /// they never over-state in-flight futures (see
    /// [`crate::stream_poll::StreamPollGuard`]),
    /// so a race can only delay detection to a later poll, never fire it
    /// while an external owner exists.
    pub(crate) fn owner_dropped(self: &Arc<Self>) -> bool {
        let active = self.gateway.active_streams.load(Ordering::SeqCst);
        // Pairs with the fence in `StreamPollGuard::new`: if `active`
        // includes a freshly registered future, the strong count read
        // below includes that future's Arc as well.
        fence(Ordering::SeqCst);
        Arc::strong_count(self) == active + 1
    }

    /// Creates a [`WeakSocketStream`] by reinterpreting this `InnerDiscord<T>`
    /// as `InnerDiscord<C>` to select the matching [`ArcStream`] implementation.
    ///
    /// # Safety
    /// `InnerDiscord<T>` and `InnerDiscord<C>` have identical `repr(C)` field
    /// layouts: `T` appears only in `PhantomData<T>`, whose layout is always
    /// zero-sized with alignment one.
    pub(crate) async fn listen_as<C, E>(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<E>, Box<dyn std::error::Error + Send + Sync>>
    where
        C: UnitStruct + 'static,
        InnerDiscord<C>: ArcStream<Item = E> + Send + Sync,
        E: Send + 'static,
    {
        self.gateway.ensure(&self.api).await?;
        let weak = Arc::downgrade(&self);
        let ptr = Weak::into_raw(weak) as *const InnerDiscord<C>;
        Ok(WeakSocketStream::new(unsafe { Weak::from_raw(ptr) }))
    }
}

impl InnerDiscord<Owned> {
    /// Shared constructor for both login paths. The token is always known up
    /// front: a token login passes it straight through, a username/password
    /// login fetches it from Discord first.
    pub(crate) fn build(token: SecureString) -> Arc<dyn Messenger> {
        Arc::new(InnerDiscord {
            api: DiscordApi::new(token),
            cache: DiscordCache::default(),
            gateway: GatewaySupervisor::new(DEFAULT_INTENTS, DEFAULT_CAPABILITIES),
            media: LocalMediaState::default(),
            _marker: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[unitstruct::unit_struct]
    struct TestMarker;

    #[test]
    fn marker_variants_share_layout() {
        assert_eq!(
            std::mem::size_of::<InnerDiscord<Owned>>(),
            std::mem::size_of::<InnerDiscord<TestMarker>>()
        );
        assert_eq!(
            std::mem::align_of::<InnerDiscord<Owned>>(),
            std::mem::align_of::<InnerDiscord<TestMarker>>()
        );
    }

    #[test]
    fn media_codec_preference_defaults_to_h264_and_switches() {
        let media = LocalMediaState::default();

        assert_eq!(media.preferred_video_codec(), VideoCodec::H264);
        assert_eq!(media.set_preferred_video_codec(VideoCodec::Av1), Some(true));
        assert_eq!(media.preferred_video_codec(), VideoCodec::Av1);
        assert_eq!(
            media.set_preferred_video_codec(VideoCodec::Av1),
            Some(false)
        );
    }
}
