//! High-level voice (call) layer.
//!
//! Implements the `Voice` trait from `messenger_interface`: joining and
//! leaving a call drives the opcode-4 `VoiceStateUpdate` handshake on the main
//! gateway and the per-call media setup in `gateways::voice`. The companion
//! `ArcStream` impl drains the buffered `VoiceEvent` queue (the audio media
//! loop itself lives in `gateways::voice::audio`).
use std::{error::Error, io, sync::Arc};

use async_trait::async_trait;
use messenger_interface::{
    interface::{CallStatus, SelfVoiceState, Voice as VoiceTrait, VoiceEvent},
    stream::{ArcStream, WeakSocketStream},
    types::{Identifier, Place, Room, VoiceState},
};
use surf::http::convert::json;
use tracing::{debug, error, warn};

use crate::{InnerDiscord, Owned, StreamPollGuard, VoiceDiscord, gateways::general::Opcode};

#[async_trait]
impl VoiceTrait for InnerDiscord<Owned> {
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>> {
        let load_gateway = self.gateway.load();
        let Some(gateway) = load_gateway.as_ref() else {
            return Err(
                io::Error::new(io::ErrorKind::NotConnected, "gateway not connected").into(),
            );
        };

        let channel = match self.channel_id_mappings.get(location.id()) {
            Some(c) => c.clone(),
            None => {
                // TODO(discord-migration): ensure all Rooms returned by Query have a mapping,
                // and support guild voice channels too.
                warn!(
                    "Tried to connect voice for a Room without a discord channel mapping: room_id={:?}, cache_size={}",
                    location.id(),
                    self.channel_id_mappings.len()
                );
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no channel mapping for this room",
                )
                .into());
            }
        };

        debug!("Voice connect: room_id={:?} → {channel:?}", location.id());

        gateway.voice.initiate_connection(channel).await;

        let payload = json!({
            "op": Opcode::VoiceStateUpdate as u8,
            "d": {
                "guild_id": channel.guild_id(),
                "channel_id": channel.channel_id(),
                "self_mute": false,
                "self_deaf": false
              }
        });
        debug!("Sending opcode 4 (VoiceStateUpdate): {}", payload);

        if let Err(err) = gateway.send(payload.to_string().into()).await {
            gateway.voice.disconnect().await;
            return Err(err.into());
        };
        Ok(CallStatus::Connecting("Awaiting call start"))
    }

    async fn disconnect(&self, location: &Identifier<Place<Room>>) {
        let load_gateway = self.gateway.load();
        let Some(gateway) = load_gateway.as_ref() else {
            error!("Not connected to the socket");
            return;
        };
        gateway.voice.disconnect().await;

        let channel = match self.channel_id_mappings.get(location.id()) {
            Some(c) => c.clone(),
            None => {
                // TODO(discord-migration): ensure all Rooms returned by Query have a mapping,
                // and support guild voice channels too.
                warn!("Tried to disconnect voice for a Room without a discord channel mapping");
                return;
            }
        };

        let payload = json!({
            "op": Opcode::VoiceStateUpdate as u8,
            "d": {
                "guild_id": channel.guild_id(),
                "channel_id": null,
                "self_mute": false,
                "self_deaf": false
              }
        });

        if let Err(err) = gateway.send(payload.to_string().into()).await {
            error!("Failed to send voice disconnect: {err}");
        }
    }

    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<VoiceEvent>, Box<dyn Error + Sync + Send>> {
        self.listen_as::<VoiceDiscord, _>().await
    }
}

#[async_trait]
impl SelfVoiceState for InnerDiscord<Owned> {
    async fn set_self_voice_state(
        &self,
        _location: &Identifier<Place<Room>>,
        state: VoiceState,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let load_gateway = self.gateway.load();
        let Some(gateway) = load_gateway.as_ref() else {
            return Err(
                io::Error::new(io::ErrorKind::NotConnected, "gateway not connected").into(),
            );
        };

        // Target the channel Discord already has us in (our own live voice
        // state), not `_location`'s mapping: an opcode-4 naming any *other*
        // channel is a move request, and a failed move disconnects the call.
        // Discord only allows one voice session per account anyway, so the
        // live state is the only call the flags could apply to.
        let Some(profile_id) = self.profile.load().as_ref().map(|profile| profile.id) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "own profile not loaded").into());
        };
        let (guild_id, channel_id) = match self.voice_states.get(&profile_id) {
            Some(voice_state) if voice_state.channel_id.is_some() => {
                (voice_state.guild_id, voice_state.channel_id)
            }
            _ => {
                return Err(
                    io::Error::new(io::ErrorKind::NotConnected, "not in a voice call").into(),
                );
            }
        };

        // Same opcode as joining: an opcode-4 for the channel we are already
        // in only updates the flags — Discord does not restart the media
        // handshake unless the channel changes. The confirmed state comes
        // back as our own VOICE_STATE_UPDATE and flows to the app through
        // the usual roster path.
        let payload = json!({
            "op": Opcode::VoiceStateUpdate as u8,
            "d": {
                "guild_id": guild_id,
                "channel_id": channel_id,
                "self_mute": state.muted,
                "self_deaf": state.deafened,
              }
        });
        debug!("Sending opcode 4 (self voice state): {}", payload);

        gateway.send(payload.to_string().into()).await?;
        Ok(())
    }
}

#[async_trait]
impl ArcStream for InnerDiscord<VoiceDiscord> {
    type Item = VoiceEvent;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item> {
        let _guard = StreamPollGuard::new(&self.active_streams);
        loop {
            if self.killed.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            if self.owner_dropped() {
                self.kill();
                return None;
            }
            if let Some(event) = self.voice_events.pop() {
                return Some(event);
            }
            self.poll_for_events().await?;
        }
    }
}
