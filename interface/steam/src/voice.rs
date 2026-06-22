//! Steam voice calls — **signaling-only scaffold**.
//!
//! This implements the [`Voice`] capability's *control* surface: joining and
//! leaving a Steam voice session. It does **not** yet carry audio.
//!
//! # Why no audio yet
//!
//! Steam Chat (2018+) voice is a WebRTC-family stack — Opus over an ICE/STUN
//! path negotiated through Steam's GameNetworkingSockets (GNS) P2P rendezvous
//! (`CMsgSteamNetworkingP2PRendezvous` + `CMsgICECandidate`). `steam_vent`
//! gives us the Connection-Manager channel — enough to *signal* a call — but no
//! media transport. Building the GNS P2P media path is the next, larger phase;
//! see the `project_steam_voice_no_media_transport` design note.
//!
//! Until that lands, [`connect`](Voice::connect) performs the real
//! `ChatRoom.JoinVoiceChat` handshake and then stops: no
//! [`VoiceEvent::CallStreamReady`](messenger_interface::interface::VoiceEvent::CallStreamReady)
//! is emitted, so a connecting call stays in [`CallStatus::Connecting`] rather
//! than transitioning to connected. That is the honest state of a session whose
//! signaling is up but whose audio path does not exist yet.
//!
//! # Scope
//!
//! Only **group** voice rooms are wired (the `ChatRoom.JoinVoiceChat` /
//! `LeaveVoiceChat` service methods). 1:1 friend calls go through a different
//! path (`CMsgClientVoiceCallPreAuthorize`) and are deferred alongside the media
//! work.

use std::error::Error;
use std::io;

use async_trait::async_trait;
use steam_vent::ConnectionTrait;
use steam_vent_proto::steammessages_chat_steamclient::{
    CChatRoom_JoinVoiceChat_Request, CChatRoom_LeaveVoiceChat_Request,
};
use tracing::{debug, info, warn};

use messenger_interface::interface::{CallStatus, Voice};
use messenger_interface::types::{Identifier, Place, Room};

use crate::SteamMessenger;
use crate::api_types::ChatRoomLocation;
use crate::gns;
use crate::session::Connected;

/// Resolve a UI room identifier to its Steam location. Mirrors the fallback the
/// `Text` impl uses: an unmapped room id is a friend DM whose id *is* the
/// SteamID. (See `query.rs`'s `get_messages`/`send_message`.)
fn resolve_room_location(
    connected: &Connected,
    location: &Identifier<Place<Room>>,
) -> ChatRoomLocation {
    connected
        .chat_room_locations
        .get(location.id())
        .map(|location| *location)
        .unwrap_or(ChatRoomLocation::Direct {
            steamid: *location.id(),
        })
}

#[async_trait]
impl Voice for SteamMessenger {
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>> {
        let connected = self.connected().await?;

        match resolve_room_location(&connected, location) {
            ChatRoomLocation::Group {
                chat_group_id,
                chat_id,
            } => {
                let conn = connected.conn.clone();
                let response = self
                    .run(async move {
                        conn.service_method(CChatRoom_JoinVoiceChat_Request {
                            chat_group_id: Some(chat_group_id),
                            chat_id: Some(chat_id),
                            ..Default::default()
                        })
                        .await
                    })
                    .await??;

                match response.voice_chatid {
                    Some(voice_chatid) => {
                        info!(voice_chatid, "Steam: joined group voice (signaling only)");
                        // Try to bring up the GNS P2P media path. Phase 1 (the
                        // networking cert) runs for real now; later phases are
                        // still stubs, so this logs the gating reason and stays
                        // "connecting" without emitting CallStreamReady. Must run
                        // inside the tokio-compat context `self.run` provides —
                        // `conn.job` uses tokio timers and panics otherwise.
                        let conn = connected.conn.clone();
                        let session = gns::VoiceSession {
                            client_steamid: connected.client_steamid,
                            voice_chatid,
                        };
                        match self
                            .run(async move { gns::establish_voice_media(&conn, session).await })
                            .await
                        {
                            Ok(Ok(())) => info!("Steam: voice media established"),
                            Ok(Err(err)) => debug!(
                                "Steam: voice media unavailable ({:?}): {err}",
                                err.phase()
                            ),
                            Err(err) => warn!("Steam: voice media setup failed: {err}"),
                        }
                        Ok(CallStatus::Connecting("Joined voice (no audio yet)"))
                    }
                    None => {
                        warn!("Steam: JoinVoiceChat returned no voice_chatid");
                        Ok(CallStatus::Failed)
                    }
                }
            }
            ChatRoomLocation::Direct { steamid } => {
                // 1:1 friend calls use CMsgClientVoiceCallPreAuthorize, deferred
                // until the media path exists. Fail loudly rather than pretend.
                warn!(steamid, "Steam: 1:1 voice calls are not implemented yet");
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Steam 1:1 voice calls are not supported yet",
                )
                .into())
            }
        }
    }

    async fn disconnect(&self, location: &Identifier<Place<Room>>) {
        let connected = match self.connected().await {
            Ok(connected) => connected,
            Err(err) => {
                warn!("Steam: voice disconnect with no session: {err}");
                return;
            }
        };

        match resolve_room_location(&connected, location) {
            ChatRoomLocation::Group {
                chat_group_id,
                chat_id,
            } => {
                let conn = connected.conn.clone();
                let result = self
                    .run(async move {
                        conn.service_method(CChatRoom_LeaveVoiceChat_Request {
                            chat_group_id: Some(chat_group_id),
                            chat_id: Some(chat_id),
                            ..Default::default()
                        })
                        .await
                    })
                    .await;
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(err)) => warn!("Steam: LeaveVoiceChat RPC error: {err}"),
                    Err(err) => warn!("Steam: failed to leave group voice: {err}"),
                }
            }
            // No 1:1 session is ever established (connect rejects it), so there
            // is nothing to tear down here.
            ChatRoomLocation::Direct { .. } => {}
        }
    }

    // `listen` is intentionally left as the trait default (NotImplemented):
    // the incoming-call / participant event stream (CClientNotificationIncomingVoiceChat,
    // ParticipantJoined/Left) arrives with the media phase.
}
