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
    interface::{CallStatus, Voice as VoiceTrait, VoiceEvent},
    stream::{ArcStream, WeakSocketStream},
    types::{Identifier, Place, Room},
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
