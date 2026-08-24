//! Steam group-voice adapter.
//!
//! Connect joins signaling, starts transport/session, then emits
//! [`VoiceEvent::CallStreamReady`]. Session bridges UI mixer channels through
//! Opus. `STEAM_VOICE_LOOPBACK=1` runs complete loopback; GNS P2P remains blocked
//! on native transport/rendezvous. Direct friend calls remain unsupported.

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

use messenger_interface::interface::{ArcStream, Audio, CallStatus, VoiceEvent};
use messenger_interface::owner::Retained;
use messenger_interface::stream::{EventFuture, WeakStreamSource};
use messenger_interface::types::{Identifier, Place, Room};

use crate::SteamMessenger;
use crate::api_types::ChatRoomLocation;
use crate::gns;
use crate::gns::call::{SteamAudioSession, VoiceEventStream};
use crate::gns::transport::{LoopbackTransport, VoiceTransport};
use crate::session::Connected;

/// Resolves registered room IDs; never interprets unknown hashes as users.
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
    /// Opens media transport or returns signaling-only `None`.
    async fn build_voice_transport(
        &self,
        connected: &Connected,
        voice_chatid: u64,
    ) -> Option<Arc<dyn VoiceTransport>> {
        if std::env::var_os("STEAM_VOICE_LOOPBACK").is_some() {
            info!("Steam: voice loopback demo — your mic echoes back through the pipeline");
            return Some(Arc::new(LoopbackTransport::new(connected.client_steamid)));
        }

        // GNS currently stops after certificate phase with logged reason.
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
                // Keep missing-audio reason visible at default log level.
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

    /// Starts retained call session and emits weak `CallStreamReady` source.
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
        let source = WeakStreamSource::from_arc(&session);

        match self.voice_events.lock().unwrap().as_ref() {
            Some(sender) => {
                if sender
                    .unbounded_send(VoiceEvent::CallStreamReady {
                        room: Identifier::new(*location.id(), ()),
                        source,
                    })
                    .is_err()
                {
                    warn!("Steam: no voice listener; call audio has no sink");
                    self.active_calls.remove(location.id());
                }
            }
            None => {
                warn!("Steam: connect() before listener preparation; call audio stream dropped")
            }
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

                // Media success emits stream and connected transition.
                match self.build_voice_transport(&connected, voice_chatid).await {
                    Some(transport) => {
                        self.start_call_audio(location, transport);
                        Ok(CallStatus::Connecting("Connecting voice…"))
                    }
                    None => Ok(CallStatus::Connecting("Joined voice (no audio yet)")),
                }
            }
            ChatRoomLocation::Direct { steamid } => {
                // Direct calls need unimplemented preauthorization path.
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
        // End media before signaling leave.
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
            // Direct calls never establish sessions.
            ChatRoomLocation::Direct { .. } => {}
        }
    }

    async fn prepare_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        self.prepare_voice_listener().await
    }

    fn next_event(self: Retained<Self>) -> EventFuture<VoiceEvent> {
        Box::pin(async move {
            let source = self.voice_stream.lock().unwrap().clone()?;
            source.next().await
        })
    }
}

impl SteamMessenger {
    async fn prepare_voice_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        // Surface authentication failure during listener setup.
        self.connected().await?;

        let (sender, receiver) = mpsc::unbounded();
        *self.voice_events.lock().unwrap() = Some(sender);

        let source = VoiceEventStream::new(receiver);
        *self.voice_stream.lock().unwrap() = Some(source.clone());
        Ok(())
    }
}
