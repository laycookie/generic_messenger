//! Steam voice calls — the `Audio` capability.
//!
//! # Pipeline
//!
//! [`connect`](Audio::connect) joins the Steam voice session
//! (`ChatRoom.JoinVoiceChat`, group rooms only), brings up a media
//! [`VoiceTransport`](crate::gns::transport::VoiceTransport), then spawns the
//! per-call audio session ([`SteamAudioSession`]) and hands its `AudioEvent`
//! stream to the UI as [`VoiceEvent::CallStreamReady`] through the
//! [`listen`](Audio::listen) stream. The UI attaches the mic and per-speaker
//! playback channels; the session bridges them to Opus over the transport. The
//! full media plane — mic → Opus → transport → Opus → playback — is implemented
//! and runs for real.
//!
//! # Transports
//!
//! - **Loopback demo** (`STEAM_VOICE_LOOPBACK=1`): joining a voice channel echoes
//!   your own microphone back through the entire real pipeline. This is a working
//!   voice call against yourself — the way to hear the media plane end-to-end
//!   today.
//! - **GNS P2P** (default): the real Steam-peer transport. Its bring-up is
//!   blocked on the `gns-sys` FFI + the live rendezvous carrier (see
//!   [`crate::gns`] and `VOICE_PROTOCOL.md`), so it logs its gating reason and
//!   the call stays connected-without-audio until that lands. Everything above
//!   the transport is ready for it.
//!
//! # Scope
//!
//! Only **group** voice rooms are wired. 1:1 friend calls take a different path
//! (`CMsgClientVoiceCallPreAuthorize`) and are deferred.

use std::error::Error;
use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use futures::channel::mpsc;
use steam_vent::ConnectionTrait;
use steam_vent_proto::steammessages_chat_steamclient::{
    CChatRoom_JoinVoiceChat_Request, CChatRoom_LeaveVoiceChat_Request,
};
use tracing::{info, warn};

use messenger_interface::interface::{Audio, CallStatus, VoiceEvent, WeakSocketStream};
use messenger_interface::types::{Identifier, Place, Room};

use crate::SteamMessenger;
use crate::api_types::ChatRoomLocation;
use crate::gns;
use crate::gns::call::{SteamAudioSession, VoiceEventStream};
use crate::gns::transport::{LoopbackTransport, VoiceTransport};
use crate::session::Connected;

/// Resolve only room identifiers registered by this live Steam session.
/// Unknown ids must not be interpreted as user SteamIDs: a stale or colliding
/// group-room hash could otherwise target an unrelated direct conversation.
fn resolve_room_location(
    connected: &Connected,
    location: &Identifier<Place<Room>>,
) -> Option<ChatRoomLocation> {
    connected
        .chat_room_locations
        .get(location.id())
        .map(|location| *location)
}

impl SteamMessenger {
    /// Bring up the media transport for a joined voice session. Returns `None`
    /// when no audio path is available (the GNS transport is still blocked), in
    /// which case the call connects for signaling only.
    async fn build_voice_transport(
        &self,
        connected: &Connected,
        voice_chatid: u64,
    ) -> Option<Arc<dyn VoiceTransport>> {
        if std::env::var_os("STEAM_VOICE_LOOPBACK").is_some() {
            info!("Steam: voice loopback demo — your mic echoes back through the pipeline");
            return Some(Arc::new(LoopbackTransport::new(connected.client_steamid)));
        }

        // Real GNS P2P media. Runs the phase-1 cert for real, then hits the
        // blocked transport bring-up (see `gns`); logs the gating reason.
        let conn = connected.conn.clone();
        let session = gns::VoiceSession {
            client_steamid: connected.client_steamid,
            voice_chatid,
        };
        match self
            .run(async move { gns::establish_voice_media(&conn, session).await })
            .await
        {
            Ok(Ok(transport)) => Some(transport),
            Ok(Err(err)) => {
                // Surfaced at INFO so the "why is there no audio?" reason is
                // visible after "joined group voice" without needing debug logs.
                info!(
                    "Steam: voice media not established (stopped at phase {:?}): {err}",
                    err.phase()
                );
                None
            }
            Err(err) => {
                warn!("Steam: voice media setup failed: {err}");
                None
            }
        }
    }

    /// Start the per-call audio session over `transport` and hand its stream to
    /// the UI as `CallStreamReady`. The session is retained in `active_calls`
    /// (the app holds only a `Weak`); `disconnect` removes and kills it.
    fn start_call_audio(
        &self,
        location: &Identifier<Place<Room>>,
        transport: Arc<dyn VoiceTransport>,
    ) {
        let session = match SteamAudioSession::new(transport) {
            Ok(session) => session,
            Err(err) => {
                warn!("Steam: failed to start voice audio: {err}");
                return;
            }
        };
        self.active_calls.insert(*location.id(), session.clone());
        let stream = WeakSocketStream::from_arc(session);

        match self.voice_events.lock().unwrap().as_ref() {
            Some(sender) => {
                if sender
                    .unbounded_send(VoiceEvent::CallStreamReady {
                        room: Identifier::new(*location.id(), ()),
                        stream,
                    })
                    .is_err()
                {
                    warn!("Steam: no voice listener; call audio has no sink");
                    self.active_calls.remove(location.id());
                }
            }
            None => warn!("Steam: connect() before listen(); call audio stream dropped"),
        }
    }
}

#[async_trait]
impl Audio for SteamMessenger {
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>> {
        let connected = self.connected().await?;

        let room_location = resolve_room_location(&connected, location).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Steam: unknown room id {}", location.id()),
            )
        })?;

        match room_location {
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

                let Some(voice_chatid) = response.voice_chatid else {
                    warn!("Steam: JoinVoiceChat returned no voice_chatid");
                    return Ok(CallStatus::Failed);
                };
                info!(voice_chatid, "Steam: joined group voice");

                // Bring up media; on success the UI gets an audio stream via
                // `listen()` and the call transitions to Connected on its own.
                match self.build_voice_transport(&connected, voice_chatid).await {
                    Some(transport) => {
                        self.start_call_audio(location, transport);
                        Ok(CallStatus::Connecting("Connecting voice…"))
                    }
                    None => Ok(CallStatus::Connecting("Joined voice (no audio yet)")),
                }
            }
            ChatRoomLocation::Direct { steamid } => {
                // 1:1 friend calls use CMsgClientVoiceCallPreAuthorize, deferred
                // until that path is built. Fail loudly rather than pretend.
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
        // End the call's audio stream first (drops the mic/playback channels).
        if let Some((_, session)) = self.active_calls.remove(location.id()) {
            session.kill();
        }

        let connected = match self.connected().await {
            Ok(connected) => connected,
            Err(err) => {
                warn!("Steam: voice disconnect with no session: {err}");
                return;
            }
        };

        let Some(room_location) = resolve_room_location(&connected, location) else {
            warn!(
                room_id = *location.id(),
                "Steam: unknown room during voice disconnect"
            );
            return;
        };

        match room_location {
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

    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<VoiceEvent>, Box<dyn Error + Sync + Send>> {
        // Establish once so a hard failure (bad credentials) surfaces here,
        // mirroring `Text::listen`.
        self.connected().await?;

        let (sender, receiver) = mpsc::unbounded();
        *self.voice_events.lock().unwrap() = Some(sender);

        let stream = VoiceEventStream::new(receiver);
        // Retain the source: the returned stream holds only a `Weak`.
        *self.voice_stream.lock().unwrap() = Some(stream.clone());
        Ok(WeakSocketStream::from_arc(stream))
    }
}
