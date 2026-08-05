use messenger_interface::{
    interface::{QueryEvent, VideoBroadcastKind, VideoEvent, VideoStreamId},
    types::UserStatus,
};
use surf::http::convert::json;
use tracing::{debug, error, warn};

use super::{EventHub, Opcode, payloads::PresencePayload};
use crate::{
    LocalMediaState, api,
    cache::DiscordCache,
    gateways::{
        Gateway,
        general::General,
        media::{ConnectProgress, StreamGateway},
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StreamRole {
    Broadcast,
    Watched,
}

impl StreamRole {
    fn label(self) -> &'static str {
        match self {
            Self::Broadcast => "broadcast",
            Self::Watched => "stream",
        }
    }
}

/// Borrowed dependencies for one main-gateway dispatch. Contains no owned
/// state and compiles down to three references.
pub(super) struct EventContext<'a> {
    cache: &'a DiscordCache,
    media: &'a LocalMediaState,
    gateway: &'a Gateway<General>,
}

impl<'a> EventContext<'a> {
    pub(super) fn new(
        cache: &'a DiscordCache,
        media: &'a LocalMediaState,
        gateway: &'a Gateway<General>,
    ) -> Self {
        Self {
            cache,
            media,
            gateway,
        }
    }

    pub(super) fn cache(&self) -> &'a DiscordCache {
        self.cache
    }

    pub(super) fn media(&self) -> &'a LocalMediaState {
        self.media
    }

    pub(super) fn events(&self) -> &EventHub {
        &self.gateway.events
    }

    pub(super) fn remove_participant_video(
        &self,
        user: api::SNOWFLAKE,
        location: api::SNOWFLAKE,
        kind: VideoBroadcastKind,
    ) -> Option<VideoStreamId> {
        let id = self.media.remove_participant_video(user, location, kind)?;
        self.gateway
            .events
            .submit_video(VideoEvent::RemoveVideoStream { id });
        Some(id)
    }

    pub(super) fn emit_presence(&self, presence: PresencePayload) {
        let user_id = presence.user.id;
        let status = match facet_value::from_value::<api::DiscordPresenceStatus>(
            presence.status.as_str().into(),
        ) {
            Ok(status) => UserStatus::from(status),
            Err(err) => {
                warn!(
                    "Unknown Discord presence status {:?}: {err}",
                    presence.status
                );
                return;
            }
        };

        self.gateway
            .events
            .submit_query(QueryEvent::UserStatusChanged { user_id, status });
    }

    pub(super) fn emit_presence_values(&self, presences: Vec<facet_value::Value>) {
        for presence in presences {
            match facet_value::from_value::<PresencePayload>(presence) {
                Ok(presence) => self.emit_presence(presence),
                Err(err) => warn!("Skipping unparseable presence: {err}"),
            }
        }
    }

    pub(super) async fn try_open_stream(&self, slot: &StreamGateway, role: StreamRole) {
        let Some(session_id) = self.gateway.voice.session_id().await else {
            return;
        };
        let Some(user_id) = self.cache.profile.load().as_ref().map(|profile| profile.id) else {
            return;
        };
        match slot.connect(session_id, user_id).await {
            Ok(ConnectProgress::Connected) => {
                debug!("Go Live {} connection opened", role.label());
                if let Some(stream_key) = slot.watching().await
                    && role == StreamRole::Broadcast
                {
                    let payload = json!({
                        "op": Opcode::StreamSetPaused as u8,
                        "d": { "stream_key": stream_key.as_str(), "paused": false }
                    });
                    if let Err(err) = self.gateway.send(payload.to_string().into()).await {
                        error!("Failed to unpause Go Live broadcast: {err}");
                    }
                }
            }
            Ok(
                ConnectProgress::WaitingForData
                | ConnectProgress::AlreadyConnected
                | ConnectProgress::Superseded,
            ) => {}
            Err(err) => error!("Failed to open Go Live {} connection: {err}", role.label()),
        }
    }
}
