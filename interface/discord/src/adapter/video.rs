//! Discord video adapter.
//!
//! Camera shares voice RTC and arrives automatically. Go Live uses a separate
//! voice connection: opcode 18 starts, 19 stops, and 20 watches; main-gateway
//! `STREAM_*` dispatches establish transport. Media coordinator handles frames.
//!
//! <https://docs.discord.food/gateway/gateway-events#update-voice-state>
//! <https://docs.discord.food/gateway/gateway-events#create-stream>
//! <https://docs.discord.food/gateway/gateway-events#watch-stream>
//! <https://docs.discord.food/gateway/gateway-events#delete-stream>

use std::{collections::HashMap, error::Error, io, pin::pin};

use async_trait::async_trait;
use futures::{FutureExt as _, future::Either};
use messenger_interface::{
    interface::{
        Audio as AudioTrait, CallStatus, Video as VideoTrait, VideoBroadcastKind, VideoEvent,
        VideoStreamId, VideoStreamMetadata,
    },
    owner::Retained,
    stream::EventFuture,
    types::{Identifier, Place, Room},
};
use simple_video::VideoCodec;
use surf::http::convert::json;
use tracing::debug;

use crate::{
    DiscordRuntimeState, InnerDiscord, LocalMediaState,
    api::SNOWFLAKE,
    cache::DiscordCache,
    gateways::{
        GatewaySupervisor,
        general::{Opcode, SelfVoiceFlags, VoiceCommand},
        media::{DiscordVideoCodec, StreamKey, coordinator::MediaDrive},
    },
};

/// App-facing stream registry; SSRC routing remains in voice gateway.
pub(crate) struct DiscordVideoStreams {
    next_id: u64,
    active: HashMap<VideoStreamId, VideoStreamMetadata>,
}

impl Default for DiscordVideoStreams {
    fn default() -> Self {
        Self {
            next_id: 1,
            active: HashMap::new(),
        }
    }
}

impl DiscordVideoStreams {
    fn metadata(&self, id: VideoStreamId) -> Option<VideoStreamMetadata> {
        self.active.get(&id).copied()
    }

    fn remove_metadata(&mut self, metadata: VideoStreamMetadata) -> Option<VideoStreamId> {
        let id = self
            .active
            .iter()
            .find_map(|(id, active)| (*active == metadata).then_some(*id))?;
        self.active.remove(&id);
        Some(id)
    }

    fn get_or_create(
        &mut self,
        metadata: VideoStreamMetadata,
    ) -> (VideoStreamId, VideoStreamMetadata) {
        if let Some((id, metadata)) = self.active.iter().find(|(_, active)| **active == metadata) {
            return (*id, *metadata);
        }

        let id = VideoStreamId::new(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("Discord video stream ID space exhausted");
        self.active.insert(id, metadata);
        (id, metadata)
    }
}

impl LocalMediaState {
    fn video_streams(&self) -> std::sync::MutexGuard<'_, DiscordVideoStreams> {
        self.video_streams
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn register_participant_video(
        &self,
        user: SNOWFLAKE,
        location: SNOWFLAKE,
        kind: VideoBroadcastKind,
    ) -> (VideoStreamId, VideoStreamMetadata) {
        self.video_streams().get_or_create(VideoStreamMetadata::new(
            Some(user),
            Some(location),
            kind,
        ))
    }

    pub(crate) fn video_stream_metadata(&self, id: VideoStreamId) -> Option<VideoStreamMetadata> {
        self.video_streams().metadata(id)
    }

    pub(crate) fn remove_participant_video(
        &self,
        user: SNOWFLAKE,
        location: SNOWFLAKE,
        kind: VideoBroadcastKind,
    ) -> Option<VideoStreamId> {
        self.video_streams()
            .remove_metadata(VideoStreamMetadata::new(Some(user), Some(location), kind))
    }
}

/// Borrowed video control-plane dependencies.
pub(crate) struct VideoControls<'a> {
    cache: &'a DiscordCache,
    gateway: &'a GatewaySupervisor,
    media: &'a LocalMediaState,
}

impl<'a> VideoControls<'a> {
    pub(crate) fn new(discord: &'a DiscordRuntimeState) -> Self {
        Self {
            cache: &discord.cache,
            gateway: &discord.gateway,
            media: &discord.media,
        }
    }

    pub(crate) fn emit_remove_participant_video(
        &self,
        user: SNOWFLAKE,
        location: SNOWFLAKE,
        kind: VideoBroadcastKind,
    ) -> Option<VideoStreamId> {
        let id = self.media.remove_participant_video(user, location, kind)?;
        if let Some(gateway) = self.gateway.current.load().as_ref() {
            gateway
                .events
                .submit_video(VideoEvent::RemoveVideoStream { id });
        }
        Some(id)
    }

    fn set_camera_send_enabled(&self, enabled: bool) -> bool {
        self.gateway
            .current
            .load()
            .as_ref()
            .map(|gateway| gateway.voice.set_camera_send_enabled(enabled))
            .unwrap_or(false)
    }

    /// Updates opcode-4 `self_video` without changing mute/deafen or channel.
    async fn set_self_video(&self, enabled: bool) -> Result<(), Box<dyn Error + Sync + Send>> {
        let gateway =
            self.gateway.current.load_full().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "gateway not connected")
            })?;
        let profile_id = self
            .cache
            .profile
            .load()
            .as_ref()
            .map(|profile| profile.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "own profile not loaded"))?;
        let voice_state =
            self.cache.voice.state(profile_id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "not in a voice call")
            })?;
        gateway
            .send_voice_command(VoiceCommand::SetFlags {
                guild_id: voice_state.guild_id,
                channel_id: voice_state.channel_id,
                flags: SelfVoiceFlags {
                    muted: voice_state.self_mute,
                    deafened: voice_state.self_deaf,
                    video: enabled,
                },
            })
            .await?;
        Ok(())
    }

    /// Starts Go Live on current call with opcode 18.
    async fn start_go_live(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        let gateway =
            self.gateway.current.load_full().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "gateway not connected")
            })?;
        let profile_id = self
            .cache
            .profile
            .load()
            .as_ref()
            .map(|profile| profile.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "own profile not loaded"))?;
        let voice_state =
            self.cache.voice.state(profile_id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "not in a voice call")
            })?;
        let guild_id = voice_state.guild_id;
        let channel_id = voice_state.channel_id;
        let was_enabled = gateway.voice.broadcast.send_enabled();
        if was_enabled && gateway.voice.broadcast.watching().await.is_some() {
            return Ok(());
        }

        // Prime deterministic key before opcode 18 dispatches arrive.
        // Clear stale connection before new handshake generation.
        gateway.voice.broadcast.disconnect().await;
        gateway.voice.broadcast.set_send_enabled(true);
        let stream_key = StreamKey::for_broadcaster(guild_id, channel_id, profile_id);
        gateway
            .voice
            .broadcast
            .begin_watch(stream_key, channel_id, None)
            .await;

        let data = match guild_id {
            Some(guild_id) => json!({
                "type": "guild",
                "guild_id": guild_id.to_string(),
                "channel_id": channel_id.to_string(),
            }),
            None => json!({
                "type": "call",
                "channel_id": channel_id.to_string(),
            }),
        };
        let payload = json!({ "op": Opcode::StreamCreate as u8, "d": data });
        debug!("Sending opcode 18 (StreamCreate): {payload}");
        match gateway.send(payload.to_string().into()).await {
            Ok(()) => Ok(()),
            Err(err) => {
                gateway.voice.broadcast.disconnect().await;
                Err(err.into())
            }
        }
    }

    /// Idempotently stops Go Live with opcode 19 and local teardown.
    pub(crate) async fn stop_go_live(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        let Some(gateway) = self.gateway.current.load_full() else {
            return Ok(());
        };
        let Some(stream_key) = gateway.voice.broadcast.watching().await else {
            gateway.voice.broadcast.disconnect().await;
            return Ok(());
        };

        let payload = json!({
            "op": Opcode::StreamDelete as u8,
            "d": { "stream_key": stream_key.as_str() }
        });
        debug!("Sending opcode 19 (StreamDelete): {payload}");
        let result = gateway.send(payload.to_string().into()).await;
        // Stop locally even if signaling or server echo fails.
        gateway.voice.broadcast.disconnect().await;
        result.map_err(Into::into)
    }

    /// Converges server state when capture ends implicitly.
    pub(crate) async fn video_input_ended(&self, kind: VideoBroadcastKind) {
        match kind {
            VideoBroadcastKind::Camera => {
                self.set_camera_send_enabled(false);
                if let Err(err) = self.set_self_video(false).await {
                    debug!("Failed to clear camera state after input ended: {err}");
                }
            }
            VideoBroadcastKind::Screen => {
                if let Err(err) = self.stop_go_live().await {
                    debug!("Failed to stop screen share after input ended: {err}");
                }
            }
        }
    }
}

#[async_trait]
impl VideoTrait for InnerDiscord {
    async fn set_preferred_codec(
        &self,
        codec: VideoCodec,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let codec = DiscordVideoCodec::try_from(codec).map_err(|_| {
            Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Discord does not support requested video codec",
            )) as Box<dyn Error + Sync + Send>
        })?;
        let runtime = self.runtime();
        runtime.media.set_preferred_video_codec(codec);

        let Some(gateway) = runtime.gateway.current.load_full() else {
            // Next Select Protocol consumes preference.
            return Ok(());
        };
        let media_gateways = [
            gateway.voice.load_gateway(),
            gateway.voice.broadcast.load_gateway(),
        ];
        let mut first_error = None;
        for media_gateway in media_gateways.into_iter().flatten() {
            if let Err(err) = media_gateway.update_video_codecs(codec).await {
                debug!("Failed to update Discord video codec preference: {err}");
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Reuses voice RTC; already-connected calls remain idempotent.
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>> {
        AudioTrait::connect(self, location).await
    }

    /// Ends video while preserving shared audio call.
    async fn disconnect(&self, _location: &Identifier<Place<Room>>) {
        let controls = VideoControls::new(self.runtime());
        controls.set_camera_send_enabled(false);
        if let Err(err) = controls.set_self_video(false).await {
            debug!("Video disconnect without an active broadcast: {err}");
        }
        // Also close separate Go Live connection.
        if let Err(err) = controls.stop_go_live().await {
            debug!("Video disconnect without an active screen share: {err}");
        }
    }

    async fn start_video(
        &self,
        _location: &Identifier<Place<Room>>,
        kind: VideoBroadcastKind,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let runtime = self.runtime();
        let controls = VideoControls::new(runtime);
        match kind {
            VideoBroadcastKind::Camera => {
                let gateway = runtime.gateway.current.load_full().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "gateway not connected")
                })?;
                if gateway.voice.set_camera_send_enabled(true) {
                    return Ok(());
                }
                // Enable sender before announcing camera to Discord.
                match controls.set_self_video(true).await {
                    Ok(()) => Ok(()),
                    Err(err) => {
                        gateway.voice.set_camera_send_enabled(false);
                        Err(err)
                    }
                }
            }
            VideoBroadcastKind::Screen => {
                // Opcode 18 opens separate Go Live connection.
                controls.start_go_live().await
            }
        }
    }

    async fn stop_video(
        &self,
        _location: &Identifier<Place<Room>>,
        kind: VideoBroadcastKind,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let controls = VideoControls::new(self.runtime());
        match kind {
            VideoBroadcastKind::Camera => {
                // Stop encoder before clearing gateway video flag.
                controls.set_camera_send_enabled(false);
                controls.set_self_video(false).await
            }
            VideoBroadcastKind::Screen => controls.stop_go_live().await,
        }
    }

    async fn subscribe_video(
        &self,
        stream: VideoStreamId,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let runtime = self.runtime();
        let metadata = runtime.media.video_stream_metadata(stream).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Discord video stream is no longer active",
            )
        })?;
        // Cameras arrive automatically and need no subscription.
        if metadata.kind == VideoBroadcastKind::Camera {
            return Ok(());
        }

        let user_id = metadata.user.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Discord video stream has no user",
            )
        })?;
        if runtime
            .cache
            .profile
            .load()
            .as_ref()
            .is_some_and(|profile| profile.id == user_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot subscribe to own Discord video stream",
            )
            .into());
        }

        let source_state = runtime.cache.voice.state(user_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "video stream is no longer in a call",
            )
        })?;
        let guild_id = source_state.guild_id;
        let channel_id = source_state.channel_id;
        let source_is_live = source_state.self_stream;
        if !source_is_live {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "video stream is no longer broadcasting",
            )
            .into());
        }
        if metadata
            .location
            .is_some_and(|location| location != channel_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "video stream call does not match current voice state",
            )
            .into());
        }

        let gateway_load = runtime.gateway.current.load();
        let gateway = gateway_load
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "gateway not connected"))?;
        let own_channel = runtime
            .cache
            .profile
            .load()
            .as_ref()
            .and_then(|profile| runtime.cache.voice.state(profile.id))
            .map(|state| state.channel_id);
        if own_channel != Some(channel_id) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "video stream is not in the current call",
            )
            .into());
        }

        let stream_key = StreamKey::for_broadcaster(guild_id, channel_id, user_id);

        if runtime.media.video_stream_metadata(stream) != Some(metadata) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Discord video stream ended while subscription was starting",
            )
            .into());
        }
        let (slot, inserted) =
            gateway
                .voice
                .begin_watching_stream(stream_key.clone(), channel_id, user_id);
        if !inserted {
            return Ok(());
        }
        let source_is_current = runtime
            .cache
            .voice
            .state(user_id)
            .is_some_and(|state| state.channel_id == channel_id && state.self_stream);
        let own_channel_is_current = runtime
            .cache
            .profile
            .load()
            .as_ref()
            .and_then(|profile| runtime.cache.voice.state(profile.id))
            .map(|state| state.channel_id)
            == Some(channel_id);
        if runtime.media.video_stream_metadata(stream) != Some(metadata)
            || !source_is_current
            || !own_channel_is_current
        {
            if let Some(slot) = gateway.voice.remove_watched_stream_if(&stream_key, &slot) {
                slot.disconnect().await;
            }
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Discord video stream ended while subscription was starting",
            )
            .into());
        }
        let payload = json!({
            "op": Opcode::StreamWatch as u8,
            "d": { "stream_key": stream_key.as_str() }
        });
        debug!("Requesting Go Live stream: {stream_key}");
        if let Err(err) = gateway.send(payload.to_string().into()).await {
            if let Some(slot) = gateway.voice.remove_watched_stream_if(&stream_key, &slot) {
                slot.disconnect().await;
            }
            return Err(err.into());
        }
        Ok(())
    }

    async fn unsubscribe_video(
        &self,
        stream: VideoStreamId,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let runtime = self.runtime();
        let Some(metadata) = runtime.media.video_stream_metadata(stream) else {
            return Ok(());
        };
        // Automatic cameras have no subscription.
        if metadata.kind == VideoBroadcastKind::Camera {
            return Ok(());
        }

        let Some(user_id) = metadata.user else {
            return Ok(());
        };
        let gateway_load = runtime.gateway.current.load();
        let Some(gateway) = gateway_load.as_ref() else {
            return Ok(());
        };

        if let Some(channel_id) = metadata.location {
            for slot in gateway
                .voice
                .remove_watched_source(user_id, channel_id)
                .await
            {
                slot.disconnect().await;
            }
        }
        Ok(())
    }

    async fn prepare_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        self.ensure_gateway().await
    }

    fn next_event(self: Retained<Self>) -> EventFuture<VideoEvent> {
        Box::pin(async move {
            let mut retained = self;
            loop {
                if !retained.is_active() {
                    return None;
                }
                let selected = {
                    let runtime = retained.get().runtime();
                    let gateway = runtime.gateway.current.load_full()?;
                    let mut event = pin!(gateway.events.next_video().fuse());
                    let mut drive = pin!(runtime.drive_media().fuse());
                    futures::select! {
                        event = event => Either::Left(event),
                        result = drive => Either::Right(result),
                    }
                };
                match selected {
                    Either::Left(event) => return Some(event),
                    Either::Right(MediaDrive::Complete(result)) => result?,
                    Either::Right(MediaDrive::Gateway(turn)) => {
                        retained = InnerDiscord::finish_gateway_turn(retained, turn).await?;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    fn participant_metadata(
        user: SNOWFLAKE,
        location: SNOWFLAKE,
        kind: VideoBroadcastKind,
    ) -> VideoStreamMetadata {
        VideoStreamMetadata::new(Some(user), Some(location), kind)
    }

    #[test]
    fn duplicate_metadata_reuses_active_stream_id() {
        let mut streams = DiscordVideoStreams::default();
        let metadata = participant_metadata(10, 20, VideoBroadcastKind::Screen);

        let first = streams.get_or_create(metadata).0;
        let duplicate = streams.get_or_create(metadata).0;

        assert_eq!(first, duplicate);
        assert_eq!(streams.active.len(), 1);
    }

    #[test]
    fn one_user_camera_and_screen_receive_distinct_ids() {
        let mut streams = DiscordVideoStreams::default();
        let camera_metadata = participant_metadata(10, 20, VideoBroadcastKind::Camera);
        let screen_metadata = participant_metadata(10, 20, VideoBroadcastKind::Screen);

        let camera = streams.get_or_create(camera_metadata).0;
        let screen = streams.get_or_create(screen_metadata).0;

        assert_ne!(camera, screen);
    }

    #[test]
    fn restarted_stream_receives_fresh_id() {
        let mut streams = DiscordVideoStreams::default();
        let metadata = participant_metadata(10, 20, VideoBroadcastKind::Screen);
        let first = streams.get_or_create(metadata).0;

        assert_eq!(streams.remove_metadata(metadata), Some(first));
        let restarted = streams.get_or_create(metadata).0;

        assert_ne!(first, restarted);
    }
}
