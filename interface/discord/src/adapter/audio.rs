//! High-level audio call and voice-state adapter.

use std::{error::Error, io, pin::pin};

use async_trait::async_trait;
use futures::{FutureExt as _, channel::oneshot, future::Either};
use messenger_interface::{
    interface::{
        Audio as AudioTrait, AudioSelfVoiceState, CallStatus, VideoBroadcastKind, VoiceEvent,
    },
    owner::Retained,
    stream::EventFuture,
    types::{Identifier, Place, Room, VoiceState},
};
use tracing::{debug, error, warn};

use simple_audio_channels::input::SampleConsumer;

use crate::{
    InnerDiscord,
    adapter::video::VideoControls,
    gateways::{
        general::{SelfVoiceFlags, VoiceCommand},
        media::{VoiceTarget, coordinator::MediaDrive},
    },
};

/// Pending and active microphone handles shared with media coordination.
#[derive(Default)]
pub(crate) struct AudioManager {
    pub(crate) microphone_recv: Option<oneshot::Receiver<SampleConsumer>>,
    pub(crate) microphone: Option<SampleConsumer>,
    pub(crate) microphone_retries: u8,
}

#[async_trait]
impl AudioTrait for InnerDiscord {
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>> {
        let runtime = self.runtime();
        let load_gateway = runtime.gateway.current.load();
        let Some(gateway) = load_gateway.as_ref() else {
            return Err(
                io::Error::new(io::ErrorKind::NotConnected, "gateway not connected").into(),
            );
        };
        let channel_id = *location.id();
        let guild_id = {
            let Some(route) = runtime.cache.channel_routes.get(&channel_id) else {
                warn!(
                    "Tried to connect voice for a Room without a discord channel mapping: room_id={:?}, cache_size={}",
                    location.id(),
                    runtime.cache.channel_routes.len()
                );
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no channel mapping for this room",
                )
                .into());
            };
            route.guild_id
        };

        debug!(
            "Voice connect: room_id={:?}, channel_id={}",
            location.id(),
            channel_id
        );
        gateway
            .voice
            .initiate_connection(VoiceTarget {
                guild_id,
                channel_id,
            })
            .await;
        if let Err(err) = gateway
            .send_voice_command(VoiceCommand::Join {
                guild_id,
                channel_id,
            })
            .await
        {
            gateway.voice.disconnect().await;
            return Err(err.into());
        };
        Ok(CallStatus::Connecting("Awaiting call start"))
    }

    async fn disconnect(&self, location: &Identifier<Place<Room>>) {
        let runtime = self.runtime();
        let load_gateway = runtime.gateway.current.load();
        let Some(gateway) = load_gateway.as_ref() else {
            error!("Not connected to the socket");
            return;
        };
        let video = VideoControls::new(runtime);
        if let Err(err) = video.stop_go_live().await {
            warn!("Failed to stop Go Live while leaving call: {err}");
        }
        let watched_sources = gateway.voice.watched_sources().await;
        gateway.voice.disconnect().await;
        for (user, channel) in watched_sources {
            video.emit_remove_participant_video(user, channel, VideoBroadcastKind::Screen);
        }

        let Some(route) = runtime.cache.channel_routes.get(location.id()) else {
            warn!("Tried to disconnect voice for a Room without a discord channel mapping");
            return;
        };
        let guild_id = route.guild_id;
        drop(route);
        if let Err(err) = gateway
            .send_voice_command(VoiceCommand::Leave { guild_id })
            .await
        {
            error!("Failed to send voice disconnect: {err}");
        }
    }

    async fn prepare_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        self.ensure_gateway().await
    }

    fn next_event(self: Retained<Self>) -> EventFuture<VoiceEvent> {
        Box::pin(async move {
            let mut retained = self;
            loop {
                if !retained.is_active() {
                    return None;
                }
                let selected = {
                    let runtime = retained.get().runtime();
                    let gateway = runtime.gateway.current.load_full()?;
                    let mut event = pin!(gateway.events.next_voice().fuse());
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

#[async_trait]
impl AudioSelfVoiceState for InnerDiscord {
    async fn set_self_voice_state(
        &self,
        _location: &Identifier<Place<Room>>,
        state: VoiceState,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let runtime = self.runtime();
        let load_gateway = runtime.gateway.current.load();
        let Some(gateway) = load_gateway.as_ref() else {
            return Err(
                io::Error::new(io::ErrorKind::NotConnected, "gateway not connected").into(),
            );
        };
        let Some(profile_id) = runtime
            .cache
            .profile
            .load()
            .as_ref()
            .map(|profile| profile.id)
        else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "own profile not loaded").into());
        };
        let voice_state =
            runtime.cache.voice.state(profile_id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "not in a voice call")
            })?;

        // Preserve camera state: video capability exclusively owns this flag.
        gateway
            .send_voice_command(VoiceCommand::SetFlags {
                guild_id: voice_state.guild_id,
                channel_id: voice_state.channel_id,
                flags: SelfVoiceFlags {
                    muted: state.muted,
                    deafened: state.deafened,
                    video: voice_state.self_video,
                },
            })
            .await?;
        Ok(())
    }
}
