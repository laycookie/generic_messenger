//! High-level video adapter.
//!
//! Implements the `Video` trait from `messenger_interface`. Discord carries
//! camera video over the *same* RTC connection as the voice call, so the
//! connection methods resolve to ("merge with") the audio call's session
//! rather than opening anything of their own; `start_video`/`stop_video`
//! drive the opcode-4 `self_video` flag. The companion `ArcStream` impl
//! drains the buffered `VideoEvent` queue, which is fed from two sides: the
//! voice websocket (op-12 stream lifecycle, in `gateways::media::events`)
//! and shared media coordinator (per-frame delivery in `gateways::media::coordinator`).
//!
//! Remote camera receive is automatic on the call connection: its first
//! decoded frame emits `AddVideoStream` directly. Go Live is request-driven:
//! voice-state announcements emit `VideoStreamAvailable`, and
//! `subscribe_video` sends opcode 20 Stream Watch.
//!
//! Screen share ("Go Live") is a *separate* voice connection keyed by the
//! main-gateway Stream* opcodes: `start_video(Screen)` sends opcode 18
//! (StreamCreate) and `stop_video(Screen)` opcode 19 (StreamDelete); the
//! connection itself is opened from the STREAM_CREATE / STREAM_SERVER_UPDATE
//! dispatches in `gateways::general` and its frames are encoded and sent from
//! the shared media coordinator, exactly like the camera path but on the broadcast
//! connection.
//!
//! <https://docs.discord.food/gateway/gateway-events#update-voice-state>
//! <https://docs.discord.food/gateway/gateway-events#create-stream>
//! <https://docs.discord.food/gateway/gateway-events#watch-stream>
//! <https://docs.discord.food/gateway/gateway-events#delete-stream>

use std::{collections::HashMap, error::Error, io, pin::pin, sync::Arc};

use async_trait::async_trait;
use futures::FutureExt as _;
use messenger_interface::{
    interface::{
        Audio as AudioTrait, CallStatus, Video as VideoTrait, VideoBroadcastKind, VideoEvent,
        VideoStreamId, VideoStreamMetadata,
    },
    stream::{ArcStream, WeakSocketStream},
    types::{Identifier, Place, Room},
};
use simple_video::VideoCodec;
use surf::http::convert::json;
use tracing::debug;

use crate::{
    InnerDiscord, LocalMediaState, Owned, UnitStruct,
    api::SNOWFLAKE,
    cache::DiscordCache,
    gateways::{
        GatewaySupervisor,
        general::{Opcode, SelfVoiceFlags, VoiceCommand},
        media::{StreamKey, coordinator::MediaDrive},
    },
    stream_poll::StreamPollGuard,
};

#[unitstruct::unit_struct]
pub(crate) struct VideoDiscord;

/// Discord-private active-stream registry. Stream lifecycle traffic is tiny,
/// so one lock and linear metadata lookup keep allocation/removal atomic
/// without maintaining synchronized forward and reverse indexes. SSRC routing
/// remains in the voice gateway's opcode-12 maps; this registry contains only
/// app-facing logical streams.
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

/// Borrowed control-plane dependencies for logical video operations. Owns no
/// state and adds no allocation or synchronization.
pub(crate) struct VideoControls<'a> {
    cache: &'a DiscordCache,
    gateway: &'a GatewaySupervisor,
    media: &'a LocalMediaState,
}

impl<'a> VideoControls<'a> {
    pub(crate) fn new<T: UnitStruct>(discord: &'a InnerDiscord<T>) -> Self {
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

    /// Flip our own `self_video` flag via opcode 4, preserving the live
    /// mute/deafen flags. Targets the channel Discord already has us in (an
    /// opcode-4 naming another channel is a move request), so this requires
    /// an active call.
    ///
    /// <https://docs.discord.food/gateway/gateway-events#update-voice-state>
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

    /// Start a "Go Live" screen-share broadcast: send opcode 18 (StreamCreate)
    /// for the channel we are in. The server answers with the STREAM_CREATE /
    /// STREAM_SERVER_UPDATE dispatches that open the broadcast's own voice
    /// connection (see `gateways::general::events`); the media loop then
    /// encodes and sends frames on it once it is up. Requires an active call.
    ///
    /// <https://docs.discord.food/gateway/gateway-events#create-stream>
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

        // The stream key is deterministic (same shape the watch path builds),
        // so prime the broadcast slot with it now: the STREAM_* dispatches key
        // their connection details by it, and the media loop reads it back to
        // unpause/tear down. Raise the send flag before op-18 so the loop is
        // already watching for the connection when it appears.
        // Clear any stale local connection left by an interrupted previous
        // start before beginning a fresh handshake generation.
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

    /// Stop our Go Live broadcast: lower the send flag (so the media loop stops
    /// encoding and retracts its op-12 announce) and send opcode 19
    /// (StreamDelete). The echoed STREAM_DELETE tears the broadcast connection
    /// down. Best-effort and idempotent — a no-op when nothing is broadcasting.
    ///
    /// <https://docs.discord.food/gateway/gateway-events#delete-stream>
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
        // Server echo may arrive later; exact-key routing ignores it after this
        // local slot has gone idle. Local media must stop even if echo never
        // arrives or signaling failed.
        gateway.voice.broadcast.disconnect().await;
        result.map_err(Into::into)
    }

    /// Capture/input ended without an explicit UI stop. Converge desired and
    /// server state instead of immediately requesting another input forever.
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
impl VideoTrait for InnerDiscord<Owned> {
    async fn set_preferred_codec(
        &self,
        codec: VideoCodec,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        self.media.set_preferred_video_codec(codec).ok_or_else(|| {
            Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Discord does not support requested video codec",
            )) as Box<dyn Error + Sync + Send>
        })?;

        let Some(gateway) = self.gateway.current.load_full() else {
            // Preference is consumed by Select Protocol when next media
            // connection opens.
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

    /// Camera video rides the voice call's RTC connection, so connecting
    /// video *is* connecting the call. Idempotent when the call is already
    /// up: an opcode-4 for the channel we are in only refreshes flags and
    /// Discord does not restart the media handshake.
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>> {
        AudioTrait::connect(self, location).await
    }

    /// Ends only the video side of the shared connection: clears our own
    /// broadcast flag and leaves the audio call running. Incoming streams
    /// stop with their frame channels when the app drops its consumers.
    async fn disconnect(&self, _location: &Identifier<Place<Room>>) {
        let controls = VideoControls::new(self);
        controls.set_camera_send_enabled(false);
        if let Err(err) = controls.set_self_video(false).await {
            debug!("Video disconnect without an active broadcast: {err}");
        }
        // Also end a screen share if one is live, so leaving video doesn't
        // strand the Go Live broadcast on its own connection.
        if let Err(err) = controls.stop_go_live().await {
            debug!("Video disconnect without an active screen share: {err}");
        }
    }

    async fn start_video(
        &self,
        _location: &Identifier<Place<Room>>,
        kind: VideoBroadcastKind,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let controls = VideoControls::new(self);
        match kind {
            VideoBroadcastKind::Camera => {
                let gateway = self.gateway.current.load_full().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "gateway not connected")
                })?;
                if gateway.voice.set_camera_send_enabled(true) {
                    return Ok(());
                }
                // Order matters: raise the send flag first so the audio media
                // loop starts acquiring the frame source and encoding, then
                // tell Discord our camera is on. The loop sends the opcode-12
                // Video announce before the first RTP frame.
                match controls.set_self_video(true).await {
                    Ok(()) => Ok(()),
                    Err(err) => {
                        gateway.voice.set_camera_send_enabled(false);
                        Err(err)
                    }
                }
            }
            VideoBroadcastKind::Screen => {
                // Go Live: open a second voice connection via opcode 18. The
                // media loop starts sending once the STREAM_* dispatches bring
                // that connection up.
                controls.start_go_live().await
            }
        }
    }

    async fn stop_video(
        &self,
        _location: &Identifier<Place<Room>>,
        kind: VideoBroadcastKind,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let controls = VideoControls::new(self);
        match kind {
            VideoBroadcastKind::Camera => {
                // Lower the flag so the media loop retracts the opcode-12
                // announce and stops encoding, then clear the gateway flag.
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
        let metadata = self.media.video_stream_metadata(stream).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Discord video stream is no longer active",
            )
        })?;
        // <https://docs.discord.food/gateway/gateway-events#watch-stream>
        // Cameras arrive on the existing voice connection. They never produce
        // VideoStreamAvailable, so a conforming UI has nothing to subscribe.
        if metadata.kind == VideoBroadcastKind::Camera {
            return Ok(());
        }

        let user_id = metadata.user.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Discord video stream has no user",
            )
        })?;
        if self
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

        let source_state = self.cache.voice.state(user_id).ok_or_else(|| {
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

        let gateway_load = self.gateway.current.load();
        let gateway = gateway_load
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "gateway not connected"))?;
        let own_channel = self
            .cache
            .profile
            .load()
            .as_ref()
            .and_then(|profile| self.cache.voice.state(profile.id))
            .map(|state| state.channel_id);
        if own_channel != Some(channel_id) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "video stream is not in the current call",
            )
            .into());
        }

        let stream_key = StreamKey::for_broadcaster(guild_id, channel_id, user_id);

        if self.media.video_stream_metadata(stream) != Some(metadata) {
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
        let source_is_current = self
            .cache
            .voice
            .state(user_id)
            .is_some_and(|state| state.channel_id == channel_id && state.self_stream);
        let own_channel_is_current = self
            .cache
            .profile
            .load()
            .as_ref()
            .and_then(|profile| self.cache.voice.state(profile.id))
            .map(|state| state.channel_id)
            == Some(channel_id);
        if self.media.video_stream_metadata(stream) != Some(metadata)
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
        let Some(metadata) = self.media.video_stream_metadata(stream) else {
            return Ok(());
        };
        // Automatic cameras have no backend subscription to release.
        if metadata.kind == VideoBroadcastKind::Camera {
            return Ok(());
        }

        let Some(user_id) = metadata.user else {
            return Ok(());
        };
        let gateway_load = self.gateway.current.load();
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

    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<VideoEvent>, Box<dyn Error + Sync + Send>> {
        self.listen_as::<VideoDiscord, _>().await
    }
}

#[async_trait]
impl ArcStream for InnerDiscord<VideoDiscord> {
    type Item = VideoEvent;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item> {
        let _guard = StreamPollGuard::new(&self.gateway.active_streams);
        loop {
            if self
                .gateway
                .killed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return None;
            }
            if self.owner_dropped() {
                self.gateway.kill();
                return None;
            }
            let gateway = self.gateway.current.load_full()?;
            let mut event = pin!(gateway.events.next_video().fuse());
            let mut drive = pin!(self.drive_media().fuse());
            futures::select! {
                event = event => return Some(event),
                result = drive => match result {
                    MediaDrive::Complete(result) => result?,
                    MediaDrive::Gateway(turn) => self.execute_gateway_turn(turn).await?,
                },
            }
        }
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
