use std::{
    error::Error,
    pin::pin,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use async_trait::async_trait;
use async_tungstenite::tungstenite::Message as WebsocketMessage;
use futures::{
    FutureExt as _, StreamExt, channel::oneshot, future::select, pending, select, stream,
};
use messenger_interface::{
    interface::{AudioEvent, CallStatus, Voice as VoiceTrait, VoiceEvent},
    stream::WeakSocketStream,
    types::{Identifier, Place, Room},
};
use smol::future::yield_now;
use surf::http::convert::json;
use tracing::{error, info, warn};

use super::{
    GatewayPayload,
    general::Opcode,
    voice::{self, VoiceOpcode, connection::VOICE_FRAME_SAMPLES},
};
use crate::{AudioManager, InnerDiscord, Owned, UnitStruct, VoiceDiscord};

impl<T: UnitStruct> InnerDiscord<T> {
    pub async fn poll_voice(&self) -> Option<()> {
        let AudioManager {
            ref mut microphone,
            ref microphone_recv,
        } = *self.audio_manager.lock().await;
        if microphone.is_none() {
            let Some(reciver) = microphone_recv.take() else {
                let (sender, receiver) = oneshot::channel();
                self.audio_events.push(AudioEvent::AddAudioInput(sender));
                microphone_recv.set(Some(receiver));
                return Some(());
            };
            *microphone = Some(reciver.await.unwrap());
        };
        let microphone_input = microphone.as_mut().unwrap();

        let gateaway = self.gateaway.load();
        let Some(gateaway) = gateaway.as_ref() else {
            warn!("Not connected to the gateaway");
            return None;
        };
        info!("gateaway inited.");
        let Some(voice_gateaway) = gateaway.voice.full_load_gateaway() else {
            warn!("Not connected to the voice_gateaway");
            return None;
        };
        info!("voice gateaway inited.");
        let connection = voice_gateaway.connection.load();
        let Some(ref connection) = *connection else {
            warn!("Not connected to the udp");
            return None;
        };
        info!("udp inited.");

        let (mut audio_recver, mut audio_sender) = connection
            .init_audio(
                &voice_gateaway.dave_session,
                &voice_gateaway.ssrc_to_user_id,
            )
            .ok()?;
        info!("inited audio reciver, and sender.");

        let mut frame = [0.0; VOICE_FRAME_SAMPLES];
        let mut microphone_input_stream = pin!(stream::unfold::<_, _, _, ()>(
            (microphone_input, &mut audio_sender, &mut frame),
            async |(microphone_input, audio_sender, frame)| {
                loop {
                    let recived_audio = {
                        let microphone_sample_iter = microphone_input.pop_iter().await;
                        if microphone_sample_iter.len() >= VOICE_FRAME_SAMPLES {
                            for (i, s) in
                                microphone_sample_iter.take(VOICE_FRAME_SAMPLES).enumerate()
                            {
                                frame[i] = s;
                            }
                            true
                        } else {
                            false
                        }
                    };

                    if recived_audio {
                        if !voice_gateaway.is_speaking.load(Ordering::Relaxed) {
                            info!("Send speaking packet");
                            let speaking_payload = json!({
                                "op": voice::VoiceOpcode::Speaking as u8,
                                "d": {
                                    "speaking": 1,
                                    "delay": 0,
                                    "ssrc": audio_sender.ssrc(),
                                }
                            })
                            .to_string();

                            match voice_gateaway
                                .websocket
                                .send(speaking_payload.clone().into())
                                .await
                            {
                                Ok(_) => {
                                    info!("Speaking: {}", speaking_payload);
                                    voice_gateaway.is_speaking.store(true, Ordering::Relaxed)
                                }
                                Err(err) => {
                                    error!("Failed to send speaking update: {err}");
                                    return None;
                                }
                            };
                        }
                        if let Err(err) = audio_sender.send_audio_frame(frame.as_slice()).await {
                            warn!("Failed to send voice audio frame: {err}");
                        }
                    } else {
                        let should_stop = audio_sender
                            .last_send_time()
                            .map(|last_time| last_time.elapsed() > Duration::from_millis(200))
                            .unwrap_or(false);

                        if should_stop && voice_gateaway.is_speaking.swap(false, Ordering::Relaxed)
                        {
                            info!("Send stop speaking packet");
                            let speaking_payload = json!({
                                "op": voice::VoiceOpcode::Speaking as u8,
                                "d": {
                                    "speaking": 0,
                                    "delay": 0,
                                    "ssrc": audio_sender.ssrc(),
                                }
                            })
                            .to_string();

                            if let Err(err) =
                                voice_gateaway.websocket.send(speaking_payload.into()).await
                            {
                                error!("Failed to send speaking update: {err}");
                            }
                        }
                    }
                }
            }
        ));

        let mut audio_recv_stream = pin!(stream::unfold::<_, _, _, ()>(
            &mut audio_recver,
            async |audio_recver| {
                loop {
                    match audio_recver.recv_audio().await {
                        Ok((ssrc, incoming_audio_frame)) => {
                            let mut channel_entery =
                                match voice_gateaway.ssrc_to_audio_channel.entry(ssrc) {
                                    dashmap::Entry::Occupied(channel) => channel,
                                    dashmap::Entry::Vacant(vacant_entry) => {
                                        let (sender, reciver) = oneshot::channel();
                                        vacant_entry.insert(
                                            voice::AudioChannel::Initilizing(reciver).into(),
                                        );
                                        self.audio_events.push(AudioEvent::AddAudioSource(sender));
                                        // TODO: Save incoming_audio_frame
                                        return Some(((), audio_recver));
                                    }
                                };
                            loop {
                                let mut channel = channel_entery.get().lock().unwrap();
                                match &mut *channel {
                                    voice::AudioChannel::Initilizing(receiver) => {
                                        let producer = match receiver.try_recv().unwrap() {
                                            Some(producer) => producer,
                                            None => continue,
                                        };
                                        drop(channel);
                                        channel_entery.insert(
                                            voice::AudioChannel::Connected(producer).into(),
                                        );
                                    }
                                    voice::AudioChannel::Connected(producer) => {
                                        let samples_pushed = producer.push_iter(
                                            incoming_audio_frame
                                                .iter()
                                                .map(|sample| *sample as f32 / i16::MAX as f32),
                                        );
                                        if incoming_audio_frame.len() != samples_pushed {
                                            error!(
                                                "Audio buffer overflow. Pusehd: {}, Sucseeded: {}",
                                                incoming_audio_frame.len(),
                                                samples_pushed
                                            );
                                        }
                                    }
                                };
                                break;
                            }
                        }
                        Err(err) => {
                            error!("{err}");
                        }
                    };
                }
            }
        ));

        select(microphone_input_stream.next(), audio_recv_stream.next()).await;
        Some(())
    }

    pub async fn poll_for_events(self: &Arc<Self>) {
        let gateaway = self.gateaway.load();
        let Some(ref_gateaway) = gateaway.as_ref() else {
            warn!("Stream has not started, or has been killed");
            pending!();
            return;
        };
        // If someone else is already pulling we just wait until they finish by looking at
        // the lock state. We also need to yield here, as try_lock isn't a future which means
        // that a stream polling at the moment might relock before ever yielding to us.
        yield_now().await;
        let Some(mut gateaway_reciver) = ref_gateaway.websocket.reciver.try_lock() else {
            self.pulled_notification.notified().await;
            return;
        };

        let voice_gateaway = ref_gateaway.voice.full_load_gateaway();
        let voice_gateaway_event_fut = {
            let voice_gateaway = voice_gateaway.clone();
            async {
                match voice_gateaway {
                    Some(voice_gateaway) => voice_gateaway.websocket.next().await,
                    None => futures::future::pending().await,
                }
            }
        };

        select! {
        event = gateaway_reciver.next() => {
            if let Some(deserilized_event) = match event {
                Some(Ok(event)) => match event {
                    WebsocketMessage::Text(utf8_bytes) => {
                        info!("Gateway raw text: {utf8_bytes}");
                        match facet_json::from_str::<GatewayPayload<Opcode>>(&utf8_bytes) {
                            Ok(event) => {
                                info!("Parsed gateway event OK: op={:?} t={:?}", event.op, event.t);
                                Some(event)
                            },
                            Err(err) => {
                                error!("Failed to parse gateway payload: {err}");
                                None
                            },
                        }
                    }
                    msg => {
                        error!("{msg:?}");
                        None
                    },
                },
                Some(Err(err)) => {
                    error!("Stream error: {err}");
                    None
                }
                None => None,
            } && let Err(err) = deserilized_event.exec(self).await
            {
                warn!("Failed to execute gateway event: {err}")
            }
        }
        event = voice_gateaway_event_fut.fuse() => {
            if let Some(deserilized_event) = match event {
                Some(Ok(event)) => match event {
                    WebsocketMessage::Text(utf8_bytes) => match facet_json::from_str::<GatewayPayload<VoiceOpcode>>(&utf8_bytes) {
                        Ok(event) => Some(event),
                        Err(err) => {
                            error!("Failed to parse: {err}");
                            None
                        },
                    }
                    WebsocketMessage::Binary(bytes) =>  {
                        let opcode_sample = VoiceOpcode::try_from(bytes[0]).unwrap();
                        Some(GatewayPayload::<VoiceOpcode>::new_binary(opcode_sample, None, bytes[0..].to_vec()))
                    }
                    msg => {
                        error!("Failed: {msg:?}");
                        None
                    },
                },
                Some(Err(err)) => {
                    error!("Stream error: {err}");
                    None
                }
                None => {
                    warn!("Stream closed");
                    ref_gateaway.voice.disconnect().await;
                    None
                }
            } && let Err(err) = deserilized_event.exec(self).await
            {
                warn!("Failed to execute gateway event: {err}")
            }
        }
        _ = ref_gateaway.heartbeat().fuse() => {}
        _ = async {
                match voice_gateaway {
                    Some(voice_gateaway) => voice_gateaway.heartbeat().await,
                    None => futures::future::pending().await,
                }
            }.fuse() => {}
        };
        self.pulled_notification.notify_all();
    }
}

#[async_trait]
impl VoiceTrait for InnerDiscord<Owned> {
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>> {
        let load_gateaway = self.gateaway.load();
        let Some(gateaway) = load_gateaway.as_ref() else {
            return Err("Not connected to the socket".into());
        };

        let channels_map = self.channel_id_mappings.read().await;
        let channel = match channels_map.get(location.id()) {
            Some(c) => c,
            None => {
                // TODO(discord-migration): ensure all Rooms returned by Query have a mapping,
                // and support guild voice channels too.
                warn!("Tried to connect voice for a Room without a discord channel mapping");
                return Err(
                    "Tried to connect voice for a Room without a discord channel mapping".into(),
                );
            }
        };

        gateaway.voice.initiate_connection(channel.to_owned()).await;

        let payload = json!({
            "op": Opcode::VoiceStateUpdate as u8,
            "d": {
                "guild_id": channel.guild_id,
                "channel_id": channel.id,
                "self_mute": false,
                "self_deaf": false
              }
        });

        if let Err(err) = gateaway.websocket.send(payload.to_string().into()).await {
            gateaway.voice.disconnect().await;
            return Err(err.into());
        };
        Ok(CallStatus::Connecting("Awaiting call start"))
    }

    async fn disconnect(&self, location: &Identifier<Place<Room>>) {
        let load_gateaway = self.gateaway.load();
        let Some(gateaway) = load_gateaway.as_ref() else {
            error!("Not connected to the socket");
            return;
        };
        gateaway.voice.disconnect().await;

        let channels_map = self.channel_id_mappings.read().await;
        let channel = match channels_map.get(location.id()) {
            Some(c) => c,
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
                "guild_id": channel.guild_id,
                "self_mute": false,
                "self_deaf": false
              }
        });

        gateaway
            .websocket
            .send(payload.to_string().into())
            .await
            .unwrap();
    }

    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<VoiceEvent>, Box<dyn Error + Sync + Send>> {
        Ok(WeakSocketStream::new(unsafe {
            self.cast_and_downgrade::<VoiceDiscord>().await
        }))
    }
}
