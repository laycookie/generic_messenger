use std::{
    future::Future,
    pin::Pin,
    sync::atomic::Ordering,
    task::{Context, Poll},
};

use bitflags::bitflags;
use futures::lock::Mutex as AsyncMutex;
use messenger_interface::{
    interface::Messenger,
    owner::{Owner, Retained},
};
use secure_string::SecureString;

use crate::{
    adapter,
    cache::DiscordCache,
    gateways::{
        GatewaySupervisor, GatewayTurn, finish_committed_gateway_turn,
        media::{AtomicDiscordVideoCodec, DiscordVideoCodec},
    },
    rest_api::DiscordApi,
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
        const PRIORITIZED_READY_PAYLOAD              = 1 << 5;
        const MULTIPLE_GUILD_EXPERIMENT_POPULATIONS  = 1 << 6;
        const NON_CHANNEL_READ_STATES                = 1 << 7;
        const AUTH_TOKEN_REFRESH                     = 1 << 8;
        const USER_SETTINGS_PROTO                    = 1 << 9;
        const CLIENT_STATE_V2                        = 1 << 10;
        const PASSIVE_GUILD_UPDATE                   = 1 << 11;
        const AUTO_CALL_CONNECT                      = 1 << 12;
        const DEBOUNCE_MESSAGE_REACTIONS             = 1 << 13;
        const PASSIVE_GUILD_UPDATE_V2                = 1 << 14;
        const CHANNEL_OBFUSCATION                    = 1 << 15;
        const AUTO_LOBBY_CONNECT                     = 1 << 16;
    }
}
const DEFAULT_CAPABILITIES: Capabilities = Capabilities::CLIENT_STATE_V2;

/// Media resources stored inline in owner allocation.
#[derive(Default)]
pub(crate) struct LocalMediaState {
    pub(crate) audio_manager: AsyncMutex<adapter::audio::AudioManager>,
    pub(crate) video_streams: std::sync::Mutex<adapter::video::DiscordVideoStreams>,
    /// User preference sent in codec priority order.
    preferred_video_codec: AtomicDiscordVideoCodec,
}

impl LocalMediaState {
    pub(crate) fn preferred_video_codec(&self) -> DiscordVideoCodec {
        self.preferred_video_codec.load(Ordering::Acquire)
    }

    /// Stores codec preference and reports change.
    pub(crate) fn set_preferred_video_codec(&self, codec: DiscordVideoCodec) -> bool {
        self.preferred_video_codec.swap(codec, Ordering::AcqRel) != codec
    }
}

/// Runtime state stored inline in [`InnerDiscord`].
pub(crate) struct DiscordRuntimeState {
    pub(crate) cache: DiscordCache,
    pub(crate) gateway: GatewaySupervisor,
    pub(crate) media: LocalMediaState,
}

impl DiscordRuntimeState {
    fn new() -> Self {
        Self {
            cache: DiscordCache::default(),
            gateway: GatewaySupervisor::new(DEFAULT_INTENTS, DEFAULT_CAPABILITIES),
            media: LocalMediaState::default(),
        }
    }
}

/// Mandatory work paired with its non-cloneable retained owner access.
pub(crate) struct Committed<T> {
    discord: Retained<InnerDiscord>,
    work: T,
}

impl<T> Committed<T> {
    fn new(discord: Retained<InnerDiscord>, work: T) -> Self {
        Self { discord, work }
    }

    pub(crate) fn into_parts(self) -> (Retained<InnerDiscord>, T) {
        (self.discord, self.work)
    }
}

/// Detaches unfinished committed work on cancellation.
struct CompleteOnDrop<T: Send + 'static> {
    future: Option<Pin<Box<dyn Future<Output = T> + Send + 'static>>>,
}

impl<T: Send + 'static> CompleteOnDrop<T> {
    fn new(future: impl Future<Output = T> + Send + 'static) -> Self {
        Self {
            future: Some(Box::pin(future)),
        }
    }
}

impl<T: Send + 'static> Future for CompleteOnDrop<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut future = self
            .future
            .take()
            .expect("completed future must not be polled again");
        match future.as_mut().poll(cx) {
            Poll::Ready(output) => Poll::Ready(output),
            Poll::Pending => {
                self.future = Some(future);
                Poll::Pending
            }
        }
    }
}

impl<T: Send + 'static> Drop for CompleteOnDrop<T> {
    fn drop(&mut self) {
        if let Some(future) = self.future.take() {
            smol::spawn(future).detach();
        }
    }
}

/// Discord backend. Only selected gateway turns retain it past poll cancellation.
pub(crate) struct InnerDiscord {
    /// Stateless HTTP transport and credential.
    pub(crate) api: DiscordApi,
    runtime: DiscordRuntimeState,
}

impl InnerDiscord {
    pub(crate) fn runtime(&self) -> &DiscordRuntimeState {
        &self.runtime
    }

    /// Ensure a usable gateway exists.
    pub(crate) async fn ensure_gateway(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.runtime().gateway.ensure(&self.api).await
    }

    fn new(token: SecureString) -> Owner<Self> {
        Owner::new(Self {
            api: DiscordApi::new(token),
            runtime: DiscordRuntimeState::new(),
        })
    }

    /// Builds erased owner after either login path resolves token.
    pub(crate) fn build(token: SecureString) -> Owner<dyn Messenger> {
        let owner: Owner<dyn Messenger> = Self::new(token);
        owner
    }

    /// Shields selected turn; completion returns guard for same root poll.
    pub(crate) fn finish_gateway_turn(
        retained: Retained<Self>,
        turn: GatewayTurn,
    ) -> impl Future<Output = Option<Retained<Self>>> + Send + 'static {
        CompleteOnDrop::new(finish_committed_gateway_turn(Committed::new(
            retained, turn,
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::task::Poll;

    use futures::{Future as _, channel::oneshot, future::poll_fn};

    use super::*;

    #[test]
    fn detached_completion_retains_closed_owner_until_work_finishes() {
        smol::block_on(async {
            let owner = InnerDiscord::new(SecureString::from("test-token"));
            let lifetime = owner.downgrade();
            let retained = lifetime.retain().expect("owner must be active");
            let (release, released) = oneshot::channel();
            let (complete, completed) = oneshot::channel();
            let mut shield = Box::pin(CompleteOnDrop::new(async move {
                released.await.expect("test must release committed work");
                assert!(retained.get().runtime().gateway.current.load().is_none());
                complete.send(()).expect("test must await completion");
            }));

            poll_fn(|cx| {
                assert!(shield.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(shield);
            drop(owner);
            assert!(!lifetime.is_active());

            release.send(()).expect("committed work must be detached");
            completed.await.expect("detached work must complete");
        });
    }

    #[test]
    fn media_codec_preference_defaults_to_h264_and_switches() {
        let media = LocalMediaState::default();

        assert_eq!(media.preferred_video_codec(), DiscordVideoCodec::H264);
        assert!(media.set_preferred_video_codec(DiscordVideoCodec::Av1));
        assert_eq!(media.preferred_video_codec(), DiscordVideoCodec::Av1);
        assert!(!media.set_preferred_video_codec(DiscordVideoCodec::Av1));
    }
}
