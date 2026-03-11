use std::{
    error::Error,
    iter,
    ops::Deref,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
    vec::IntoIter,
};

use async_trait::async_trait;
use async_tungstenite::{
    WebSocketReceiver, WebSocketSender, WebSocketStream,
    async_std::ConnectStream,
    tungstenite::{Bytes, Message as WebsocketMessage},
};
use facet::Facet;
use futures::{
    FutureExt as _, StreamExt, channel::oneshot, lock::Mutex as AsyncMutex, pending, select,
};
use futures_timer::Delay;
use messenger_interface::{
    interface::{AudioEvent, CallStatus, Voice, VoiceEvent},
    stream::WeakSocketStream,
    types::{Identifier, Place, Room},
};
use simple_audio_channels::{Consumer as _, Producer as _};
use smol::future::yield_now;
use surf::http::convert::json;
use tracing::{error, info, warn};

use crate::{
    Discord, InnerDiscord,
    gateaways::{
        general::{GatewayEvent, Opcode},
        voice::{InputChannel, VoiceOpcode, connection::VOICE_FRAME_SAMPLES},
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
    pub async fn poll_voice(&self) -> Option<()> {
        let load_gateaway = self.gateaway.load();
        let Some(gateaway) = load_gateaway.as_ref() else {
            warn!("Not connected to the gateaway");
            return None;
        };
        let Some(voice_gateaway) = gateaway.voice.full_load_gateaway() else {
            warn!("Not connected to the voice_gateaway");
            return None;
        };
        let mut connection = voice_gateaway.connection.lock().await;
        let Some(connection) = connection.as_mut() else {
            warn!("Not connected to the udp");
            return None;
        };
        let Some(description) = connection.description() else {
            warn!("Not description provided");
            return None;
        };

        let mut input_buffer = voice_gateaway.input_buffer.lock().await;
        // === Send audio, from the mic. ===
        let mut input_channel = voice_gateaway.input_channel.lock().await;
        match &mut *input_channel {
            InputChannel::None => {
                // Handle setting up a voice input channel if one doesn't exist.
                let (sender, receiver) = oneshot::channel();
                *input_channel = InputChannel::Initilizing(receiver);
                self.audio_events.push(AudioEvent::AddAudioInput(sender));
            }
            InputChannel::Initilizing(receiver) => {
                if let Some(input) = receiver.try_recv().unwrap() {
                    *input_channel = InputChannel::Connected(input);
                }
            }
            InputChannel::Connected(input) => {
                while let Some(sample) = input.try_pop() {
                    input_buffer.push_back(sample);
                }
            }
        };

        let mut frame = [0.0; VOICE_FRAME_SAMPLES];
        // Check if we should send stop speaking event
        // Only stop if buffer is low AND we haven't sent audio for 200ms
        if input_buffer.len() < VOICE_FRAME_SAMPLES {
            if voice_gateaway.is_speaking.load(Ordering::Relaxed) {
                let should_stop = connection
                    .last_send_time()
                    .map(|last_time| last_time.elapsed() > Duration::from_millis(200))
                    .unwrap_or(false);

                if should_stop {
                    let speaking_payload = json!({
                        "op": voice::VoiceOpcode::Speaking as u8,
                        "d": {
                            "speaking": 0,
                            "delay": 0,
                            "ssrc": connection.ssrc(),
                        }
                    })
                    .to_string();
                    if let Err(err) = voice_gateaway.websocket.send(speaking_payload.into()).await {
                        error!("Failed to send speaking update: {err}");
                    } else {
                        voice_gateaway.is_speaking.store(false, Ordering::Relaxed);
                    }
                }
            }
        } else {
            while input_buffer.len() >= VOICE_FRAME_SAMPLES {
                let mut frame_iter = input_buffer.drain(..VOICE_FRAME_SAMPLES).enumerate();

                let has_audio = frame_iter.any(|(i, sample)| {
                    frame[i] = sample;
                    sample != 0.0
                });

                if has_audio {
                    for (i, sample) in frame_iter {
                        frame[i] = sample;
                    }

                    // TODO: Temporary disable speaking
                    if !voice_gateaway.is_speaking.load(Ordering::Relaxed) {
                        let speaking_payload = json!({
                            "op": voice::VoiceOpcode::Speaking as u8,
                            "d": {
                                "speaking": 1,
                                "delay": 0,
                                "ssrc": connection.ssrc(),
                            }
                        })
                        .to_string();
                        if let Err(err) =
                            voice_gateaway.websocket.send(speaking_payload.into()).await
                        {
                            error!("Failed to send speaking update: {err}");
                        } else {
                            voice_gateaway.is_speaking.store(true, Ordering::Relaxed);
                        }
                    }

                    let mut dave_session = voice_gateaway.dave_session.lock().await;
                    if let Err(err) = connection
                        .send_audio_frame(&frame, dave_session.as_mut())
                        .await
                    {
                        warn!("Failed to send voice audio frame: {err}");
                    }
                }
            }
        }

        // === Receive and play audio ===
        // DEBUG: Generate sine wave instead of using received audio
        // TODO: Remove this and restore original recv_audio logic
        let mut dave_session = voice_gateaway.dave_session.lock().await;

        loop {
            let (ssrc, audio_frame) = match connection
                .recv_audio(dave_session.as_mut(), &voice_gateaway.ssrc_to_user_id)
                .await
            {
                Ok(ssrc_audio_frame) => ssrc_audio_frame,
                Err(err) => {
                    error!("{err}");
                    break;
                }
            };

            info!("playing");
            let mut channel_entery = match voice_gateaway.ssrc_to_audio_channel.entry(ssrc) {
                dashmap::Entry::Occupied(channel) => channel,
                dashmap::Entry::Vacant(e) => {
                    let (sender, reciver) = oneshot::channel();
                    e.insert(voice::AudioChannel::Initilizing(reciver).into());
                    self.audio_events.push(AudioEvent::AddAudioSource(sender));
                    return Some(());
                }
            };
            let mut channel = channel_entery.get_mut().lock().await;
            match &mut *channel {
                voice::AudioChannel::Initilizing(receiver) => {
                    let producer = match receiver.try_recv().unwrap() {
                        Some(producer) => producer,
                        None => continue,
                    };
                    drop(channel);
                    channel_entery.insert(voice::AudioChannel::Connected(producer).into());
                }
                voice::AudioChannel::Connected(producer) => {
                    let samples_to_push = audio_frame.len();
                    let samples_pushed = producer.push_iter(
                        audio_frame
                            .iter()
                            .map(|sample| *sample as f32 / i16::MAX as f32),
                    );

                    if samples_to_push != samples_pushed {
                        error!("Audio buffer overflow");
                    }
                }
            };
        }
        Some(())
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
impl Voice for Discord {
    async fn connect<'a>(
        &'a self,
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
        // Ok(WeakSocketStream::new(self.0.clone().audio().await))
    }
    async fn disconnect<'a>(&'a self, location: &Identifier<Place<Room>>) {
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

    async fn listen(&self) -> Result<WeakSocketStream<VoiceEvent>, Box<dyn Error + Sync + Send>> {
        Ok(WeakSocketStream::new(self.0.clone().voice().await))
    }
}
