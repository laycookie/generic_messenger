use std::{
    io,
    num::NonZeroU16,
    pin::pin,
    sync::{OnceLock, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwapOption;
use async_tungstenite::async_std::connect_async;
use crossbeam::queue::ArrayQueue;
use dashmap::DashMap;
use davey::{DAVE_PROTOCOL_VERSION, DaveSession};
use facet_pretty::FacetPretty;
use futures::{
    StreamExt as _,
    channel::oneshot,
    lock::{Mutex as AsyncMutex, MutexGuard},
};
use messenger_interface::interface::AudioEvent;
use surf::http::convert::json;
use tracing::{debug, error, trace, warn};

use super::{
    AudioChannel, Endpoint, SessionId, VoiceOpcode,
    connection::{Connection, Ssrc},
    payloads::HelloPayload,
};
use crate::gateways::{Gateway, HeartBeatingData, Websocket};
use crate::{api_types::SNOWFLAKE, gateways::GatewayStreamReciver as _};

pub struct Voice {
    heartbeat_version: u8,
    pub channel_id: SNOWFLAKE,
    pub dave_pending_transitions: DashMap<u16, NonZeroU16>, // transition_id, dave_protocol_version
    pub dave_session: AsyncMutex<Option<DaveSession>>,
    pub connection: ArcSwapOption<Connection>,
    pub ssrc_to_audio_channel: DashMap<Ssrc, std::sync::Mutex<super::AudioChannel>>,
    pub ssrc_to_user_id: DashMap<Ssrc, SNOWFLAKE>,
    pub is_speaking: AtomicBool,
}

pub(crate) struct DaveSessionGuard<'a>(MutexGuard<'a, Option<DaveSession>>);

impl std::ops::Deref for DaveSessionGuard<'_> {
    type Target = DaveSession;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for DaveSessionGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}

impl Voice {
    pub(crate) async fn require_dave_session(&self) -> Result<DaveSessionGuard<'_>, io::Error> {
        // Freeze suspect: the audio loop holds this lock across its UDP
        // send, and the SessionDescription handler across DAVE reinit.
        trace!("require_dave_session: waiting for dave_session lock");
        let guard = self.dave_session.lock().await;
        trace!("require_dave_session: dave_session lock acquired");
        if guard.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DAVE session not initialized",
            ));
        }
        Ok(DaveSessionGuard(guard))
    }

    /// Dispatch a decoded audio frame to the appropriate playback channel.
    ///
    /// Returns `false` if a new channel is being initialized and the caller should yield.
    pub(crate) fn dispatch_incoming_audio(
        &self,
        audio_events: &ArrayQueue<AudioEvent>,
        ssrc: Ssrc,
        samples: &[i16],
    ) -> bool {
        let mut channel_entry = match self.ssrc_to_audio_channel.entry(ssrc) {
            dashmap::Entry::Occupied(channel) => channel,
            dashmap::Entry::Vacant(vacant_entry) => {
                let (sender, receiver) = oneshot::channel();
                vacant_entry.insert(AudioChannel::Initializing(receiver).into());
                let _ = audio_events.force_push(AudioEvent::AddAudioSource(sender));
                // TODO: Save samples for when channel is ready
                return false;
            }
        };
        loop {
            let mut channel = channel_entry.get().lock().unwrap();
            match &mut *channel {
                AudioChannel::Initializing(receiver) => match receiver.try_recv() {
                    Ok(Some(producer)) => {
                        drop(channel);
                        channel_entry.insert(AudioChannel::Connected(producer).into());
                        // Re-enter the loop so this frame is delivered to
                        // the freshly connected channel.
                        continue;
                    }
                    // Producer not ready yet: drop this frame and keep
                    // receiving. Spinning here (the previous behavior)
                    // blocks the executor thread until the UI task — which
                    // may need this very thread — sends the producer.
                    Ok(None) => break,
                    Err(_) => {
                        drop(channel);
                        channel_entry.remove();
                        warn!("Audio source sender dropped for SSRC {ssrc}, will re-request");
                        break;
                    }
                },
                AudioChannel::Connected(producer) => {
                    // The channel is declared as i16; the mixer converts to
                    // the device format itself.
                    let samples_pushed = producer.push_iter(samples);
                    if samples.len() != samples_pushed {
                        error!(
                            "Audio buffer overflow. Pushed: {}, Succeeded: {}",
                            samples.len(),
                            samples_pushed
                        );
                    }
                    break;
                }
            };
        }
        true
    }
}

impl Gateway<Voice> {
    const VERSION: usize = super::VOICE_GATEWAY_VERSION;
    pub async fn new(
        endpoint: &Endpoint,
        session_id: &SessionId,
        guild_id: Option<SNOWFLAKE>,
        channel_id: SNOWFLAKE,
        user_id: u64,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // No timeout on any of the steps below (TLS connect, identify
        // send, hello wait) — each is a freeze suspect during VC entry.
        trace!("voice gateway: connecting websocket to {}", endpoint.wss);
        let (voice_websocket, _) = connect_async(
            "wss://".to_string() + &endpoint.wss + "/?v=" + &Self::VERSION.to_string(),
        )
        .await?;
        trace!("voice gateway: websocket connected, sending identify");
        let websocket = Websocket::new(voice_websocket);

        // <https://discord.com/developers/docs/topics/voice-connections#establishing-a-voice-websocket-connection>
        let identify_payload = json!({
          "op": VoiceOpcode::Identify as u8,
          "d": {
            // The ID of the guild, private channel, stream, or lobby being connected to
            "server_id": guild_id.unwrap_or(channel_id),
            "channel_id": channel_id,
            "user_id": user_id,
            "session_id": session_id,
            "token": endpoint.token,
            "max_dave_protocol_version": DAVE_PROTOCOL_VERSION,
          }
        });
        debug!("{identify_payload:#?}");
        websocket.send(identify_payload.to_string().into()).await?;

        trace!("voice gateway: identify sent, waiting for hello");
        let hello_event = {
            let mut receiver = websocket.receiver.lock().await;
            pin!(receiver.filter_payload()).next().await
        }
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "voice gateway closed before receiving hello",
            )
        })?;

        let VoiceOpcode::Hello = hello_event.op else {
            return Err(io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Expected to receive hello event as the first event",
            )
            .into());
        };

        let hello_d = facet_value::from_value::<HelloPayload>(hello_event.d)?;
        debug!("{}", hello_d.pretty());
        let heart_beating_duration = Duration::from_millis(hello_d.heartbeat_interval);

        Ok(Self {
            websocket,
            heart_beating: HeartBeatingData::new(heart_beating_duration).into(),
            last_sequence_number: OnceLock::new(),
            type_specific_data: Voice {
                heartbeat_version: hello_d.v,
                channel_id,
                dave_pending_transitions: DashMap::new(),
                dave_session: AsyncMutex::new(None),
                connection: Default::default(),
                ssrc_to_audio_channel: DashMap::new(),
                ssrc_to_user_id: Default::default(),
                is_speaking: false.into(),
            },
        })
    }
    /// https://docs.discord.food/topics/voice-connections#heartbeating
    pub async fn heartbeat(&self) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.heart_beating.lock().await.await_until_beat().await;

        let payload = match self.heartbeat_version {
            ver if 7 < ver => json!({
                    "op": VoiceOpcode::Heartbeat as u8,
                    "d": {
                        "t": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
                        "seq_ack": self.last_sequence_number.get()
                    },
            })
            .to_string(),
            _ => json!({
                    "op": VoiceOpcode::Heartbeat as u8,
                    "d": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            })
            .to_string(),
        };

        self.websocket.send(payload.into()).await
    }
}
