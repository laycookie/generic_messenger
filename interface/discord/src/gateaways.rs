use std::{
    collections::VecDeque,
    error::Error,
    iter,
    mem::forget,
    ops::Deref,
    pin::{Pin, pin},
    ptr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use async_tungstenite::{
    WebSocketReceiver, WebSocketSender, WebSocketStream,
    async_std::ConnectStream,
    tungstenite::{Bytes, Message as WebsocketMessage},
};
use crossbeam::queue::SegQueue;
use facet::Facet;
use futures::{
    FutureExt as _, StreamExt,
    channel::oneshot,
    future::{Either, select},
    lock::Mutex as AsyncMutex,
    pending, select,
    sink::unfold,
    stream,
};
use futures_timer::Delay;
use messenger_interface::{
    interface::{AudioEvent, CallStatus, Voice as VoiceTrait, VoiceEvent},
    stream::WeakSocketStream,
    types::{Identifier, Place, Room},
};
use simple_audio_channels::input::SampleConsumer;
use smol::future::yield_now;
use surf::http::convert::json;
use tracing::{error, info, trace, warn};

use crate::{
    AudioManager, InnerDiscord, Owned, VoiceDiscord,
    gateaways::{
        general::{GatewayEvent, Opcode},
        voice::{
            InputChannel, Voice, VoiceOpcode,
            connection::{Connection, RecvAudioFuture, SendAudioFuture, VOICE_FRAME_SAMPLES},
        },
    },
};

pub mod general;
pub mod voice;

struct Websocket {
    sender: AsyncMutex<WebSocketSender<ConnectStream>>,
    reciver: AsyncMutex<WebSocketReceiver<ConnectStream>>,
}
impl Websocket {
    fn new(websocket: WebSocketStream<ConnectStream>) -> Self {
        let (sender, reciver) = websocket.split();
        Self {
            sender: sender.into(),
            reciver: reciver.into(),
        }
    }
    async fn send(
        &self,
        msg: WebsocketMessage,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        let mut sender = self.sender.lock().await;
        sender.send(msg).await
    }
    async fn send_binary(
        &self,
        op: u8,
        msg: impl Iterator<Item = u8>,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        let mut sender = self.sender.lock().await;

        let p = Bytes::from_iter(iter::once(op).chain(msg.into_iter()));
        sender.send(WebsocketMessage::binary(p)).await
    }
    async fn next(
        &self,
    ) -> Option<Result<WebsocketMessage, async_tungstenite::tungstenite::Error>> {
        if let Some(mut reciver) = self.reciver.try_lock() {
            return reciver.next().await;
        }
        let mut reciver = self.reciver.lock().await;
        reciver.next().await
    }
}

pub struct Gateaway<T> {
    websocket: Websocket,
    heart_beating: AsyncMutex<HeartBeatingData>,
    last_sequence_number: OnceLock<AtomicUsize>,
    type_specific_data: T,
}
impl<T> Deref for Gateaway<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.type_specific_data
    }
}

/// <https://discord.com/developers/docs/events/gateway-events#payload-structure>
#[derive(Debug, Facet)]
pub struct GatewayPayload<Op> {
    // Opcode
    op: Op,
    // Event type
    t: Option<GatewayEvent>,
    // Sequence numbers
    s: Option<usize>,
    // data
    d: facet_value::Value,
}

impl<Op> GatewayPayload<Op> {
    pub fn new_binary(op: Op, s: Option<usize>, d: Vec<u8>) -> Self {
        Self {
            op,
            t: None,
            s,
            d: facet_value::to_value(&d).unwrap(),
        }
    }
}

#[deprecated]
trait GateawayStream {
    async fn next_gateaway_payload<Op: Facet<'static>>(&mut self) -> GatewayPayload<Op>;
}
impl GateawayStream for WebSocketStream<ConnectStream> {
    async fn next_gateaway_payload<Op: Facet<'static>>(&mut self) -> GatewayPayload<Op> {
        // NOTE: this trait can't return a Result, so we "best-effort" skip frames until a valid
        // text payload arrives.
        //
        // TODO(discord-migration): change this API to return `Result<GatewayPayload<_>, _>` so
        // callers can handle socket closure/errors explicitly.
        while let Some(next) = self.next().await {
            match next {
                Ok(WebsocketMessage::Text(utf8)) => {
                    return facet_json::from_str::<GatewayPayload<Op>>(&utf8).unwrap();
                }
                Ok(WebsocketMessage::Ping(_)) | Ok(WebsocketMessage::Pong(_)) => {
                    // ignore
                }
                Ok(WebsocketMessage::Binary(_)) => {
                    // Discord can optionally send compressed/binary frames.
                    // TODO(discord-migration): support compressed/binary gateway frames.
                }
                Ok(WebsocketMessage::Close(_)) => break,
                Ok(WebsocketMessage::Frame(_)) => {
                    // ignore
                }
                Err(err) => {
                    warn!("Gateway websocket error while waiting for payload: {err:#?}");
                }
            }
        }
        panic!("Gateway websocket closed before receiving a payload");
    }
}

pub struct HeartBeatingData {
    duration: Duration,
    future: Pin<Box<dyn Future<Output = ()> + Send + Sync>>,
}
impl HeartBeatingData {
    fn new(duration: Duration) -> Self {
        Self {
            duration,
            future: Box::pin(Delay::new(duration)),
        }
    }
    async fn await_until_beat(&mut self) {
        (&mut self.future).await;
        self.future = Box::pin(Delay::new(self.duration));
    }
}

impl<T> InnerDiscord<T> {
    async fn heart_beating(&self) {
        if let Some(gateaway) = self.gateaway.load().as_ref() {
            let gateaway_heartbeat_fut = async {
                gateaway.heart_beating.lock().await.await_until_beat().await;
                gateaway
                    .websocket
                    .send(
                        json!({
                                "op": Opcode::Heartbeat as u8,
                                "d": gateaway.last_sequence_number.get(),
                        })
                        .to_string()
                        .into(),
                    )
                    .await
                    .unwrap();
            };
            let voice_gateaway_heartbeat_fut = async {
                if let Some(voice_gateaway) = gateaway.voice.full_load_gateaway() {
                    voice_gateaway
                        .heart_beating
                        .lock()
                        .await
                        .await_until_beat()
                        .await;
                    voice_gateaway
                        .websocket
                        .send(
                            json!({
                                    "op": Opcode::Heartbeat as u8,
                                    "d": voice_gateaway.last_sequence_number.get(),
                            })
                            .to_string()
                            .into(),
                        )
                        .await
                        .unwrap();
                }
            };
        }

        todo!()
    }

    // pub async fn send_audio(
    //     audio_events: &SegQueue<AudioEvent>,
    //     send_audio: &mut SendAudioFuture<'_>,
    //     input_channel: &mut InputChannel,
    // ) {
    //     // === Poll microphone Audio ===
    //     let mut frame_iter = {
    //         match input_channel {
    //             InputChannel::None => {
    //                 // Handle setting up a voice input channel if one doesn't exist.
    //                 let (sender, receiver) = oneshot::channel();
    //                 *input_channel = InputChannel::Initilizing(receiver);
    //                 audio_events.push(AudioEvent::AddAudioInput(sender));
    //                 return;
    //             }
    //             InputChannel::Initilizing(receiver) => {
    //                 if let Some(input) = receiver.try_recv().unwrap() {
    //                     *input_channel = InputChannel::Connected(input);
    //                 }
    //                 return;
    //             }
    //             InputChannel::Connected(input) => input.pop_iter().await,
    //         }
    //     };
    //     // ===
    //     // Check if we should send stop speaking event
    //     // Only stop if buffer is low AND we haven't sent audio for 150ms
    //     if frame_iter.len() < VOICE_FRAME_SAMPLES {
    //         let should_stop = send_audio
    //             .last_send_time()
    //             .map(|last_time| last_time.elapsed() > Duration::from_millis(150))
    //             .unwrap_or(false);
    //         if should_stop && voice_gateaway.is_speaking.swap(false, Ordering::Relaxed) {
    //             let speaking_payload = json!({
    //                 "op": voice::VoiceOpcode::Speaking as u8,
    //                 "d": {
    //                     "speaking": 0,
    //                     "delay": 0,
    //                     "ssrc": send_audio.ssrc(),
    //                 }
    //             })
    //             .to_string();
    //             if let Err(err) = voice_gateaway.websocket.send(speaking_payload.into()).await {
    //                 error!("Failed to send speaking update: {err}");
    //             }
    //         }
    //     } else {
    //         // TODO: Move this to somewhere static to avoid realocations
    //         let mut frame = [0.0; VOICE_FRAME_SAMPLES];
    //         while frame_iter.len() >= VOICE_FRAME_SAMPLES {
    //             let has_audio = frame_iter.any(|(i, sample)| {
    //                 frame[i] = sample;
    //                 sample != 0.0
    //             });
    //             if has_audio {
    //                 for (i, sample) in frame_iter {
    //                     frame[i] = sample;
    //                 }
    //                 // TODO: Temporary disable speaking
    //                 if !voice_gateaway.is_speaking.load(Ordering::Relaxed) {
    //                     let speaking_payload = json!({
    //                         "op": voice::VoiceOpcode::Speaking as u8,
    //                         "d": {
    //                             "speaking": 1,
    //                             "delay": 0,
    //                             "ssrc": send_audio.ssrc(),
    //                         }
    //                     })
    //                     .to_string();
    //                     if let Err(err) =
    //                         voice_gateaway.websocket.send(speaking_payload.into()).await
    //                     {
    //                         error!("Failed to send speaking update: {err}");
    //                     } else {
    //                         voice_gateaway.is_speaking.store(true, Ordering::Relaxed);
    //                     }
    //                 }
    //                 if let Err(err) = send_audio.send_audio_frame(&frame).await {
    //                     warn!("Failed to send voice audio frame: {err}");
    //                 }
    //             }
    //         }
    //     }
    // }
    // pub async fn recv_audio(audio_events: &SegQueue<AudioEvent>, audio_recv: &RecvAudioFuture<'_>) {
    //     // === Poll incoming audio ===
    //     let (ssrc, audio_frame) = match audio_recv.recv_audio().await {
    //         Ok(ssrc_audio_frame) => ssrc_audio_frame,
    //         Err(err) => {
    //             error!("{err}");
    //             return;
    //         }
    //     };
    //     // ===
    //     let mut channel_entery = match voice_gateaway.ssrc_to_audio_channel.entry(ssrc) {
    //         dashmap::Entry::Occupied(channel) => channel,
    //         dashmap::Entry::Vacant(e) => {
    //             let (sender, reciver) = oneshot::channel();
    //             e.insert(voice::AudioChannel::Initilizing(reciver).into());
    //             audio_events.push(AudioEvent::AddAudioSource(sender));
    //             return;
    //         }
    //     };
    //     loop {
    //         let mut channel = channel_entery.get_mut().lock().await;
    //         match &mut *channel {
    //             voice::AudioChannel::Initilizing(receiver) => {
    //                 let producer = match receiver.try_recv().unwrap() {
    //                     Some(producer) => producer,
    //                     None => continue,
    //                 };
    //                 drop(channel);
    //                 channel_entery.insert(voice::AudioChannel::Connected(producer).into());
    //             }
    //             voice::AudioChannel::Connected(producer) => {
    //                 let samples_pushed = producer.push_iter(
    //                     audio_frame
    //                         .iter()
    //                         .map(|sample| *sample as f32 / i16::MAX as f32),
    //                 );
    //                 if audio_frame.len() != samples_pushed {
    //                     error!(
    //                         "Audio buffer overflow. Pusehd: {}, Sucseeded: {}",
    //                         audio_frame.len(),
    //                         samples_pushed
    //                     );
    //                 }
    //             }
    //         };
    //         break;
    //     }
    // }

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
                                Err(err) => error!("Failed to send speaking update: {err}"),
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
                // Some(((), (microphone_input, audio_sender, frame)))
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

                // Some(((), audio_recver))
            }
        ));

        // loop {
        select(microphone_input_stream.next(), audio_recv_stream.next()).await;
        Some(())
        // }
    }
    pub async fn poll_for_events(self: &Arc<Self>) {
        if let Some(ref_gateaway) = self.gateaway.load().as_ref() {
            // If someone else is already pulling we just wait until they finish by looking at
            // the lock state. We also need to yield here, as try_lock isn't a future which means
            // that a stream polling at the moment might relock before ever yielding to us.
            yield_now().await;
            let Some(mut gateaway_reciver) = ref_gateaway.websocket.reciver.try_lock() else {
                ref_gateaway.websocket.reciver.lock().await;
                return;
            };

            let voice_gateaway = ref_gateaway.voice.full_load_gateaway();
            let voice_gateaway_event_fut = async {
                match voice_gateaway {
                    Some(voice_gateaway) => voice_gateaway.websocket.next().await,
                    None => futures::future::pending().await,
                }
            };

            select! {
                event = gateaway_reciver.next().fuse() => {
                    if let Some(deserilized_event) = match event {
                        Some(Ok(event)) => match event {
                            WebsocketMessage::Text(utf8_bytes) => match facet_json::from_str::<GatewayPayload<Opcode>>(&utf8_bytes) {
                                Ok(event) => Some(event),
                                Err(err) => {
                                    error!("Failed to parse: {err}");
                                    None
                                },
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
            };
        } else {
            warn!("Stream has not started, or has been killed");
            pending!()
        }
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
