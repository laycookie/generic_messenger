use std::{
    collections::hash_map,
    ops::{Deref, DerefMut},
    pin::{Pin, pin},
    sync::Arc,
    task::Poll,
    time::Duration,
};

use async_trait::async_trait;
use async_tungstenite::{
    WebSocketStream, async_std::ConnectStream, tungstenite::Message as WebsocketMessage,
};
use facet::Facet;
use futures::{FutureExt as _, Stream, StreamExt, channel::oneshot, pending, poll};
use futures_timer::Delay;
use messenger_interface::{
    interface::{Socket, SocketEvent, Voice},
    types::{Identifier, Place, Room},
};
use simple_audio_channels::{Consumer, Producer};
use surf::http::convert::json;
use tracing::{info, warn};

use crate::{
    Discord,
    gateaways::{
        general::{GatewayEvent, Opcode},
        voice::{
            InputChannel, Voice as VoiceGateawayData, VoiceGateawayState,
            connection::VOICE_FRAME_SAMPLES,
        },
    },
};

pub mod general;
pub mod voice;

pub struct Gateaway<T> {
    websocket: WebSocketStream<ConnectStream>,
    heart_beating: HeartBeatingData,
    last_sequence_number: Option<usize>,
    type_specific_data: T,
}
impl<T> Deref for Gateaway<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.type_specific_data
    }
}
impl<T> DerefMut for Gateaway<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.type_specific_data
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

fn deserialize_event<Op: for<'a> Facet<'a>>(
    event: &WebsocketMessage,
) -> Result<GatewayPayload<Op>, Box<dyn std::error::Error + Send + Sync>> {
    let json = match event {
        WebsocketMessage::Text(text) => match facet_json::from_str::<GatewayPayload<Op>>(text) {
            Ok(event) => event,
            Err(err) => {
                warn!("Failed to parse: {text}");
                return Err(Box::new(err));
            }
        },
        WebsocketMessage::Binary(_) => {
            // TODO(discord-migration): support compressed/binary gateway frames.
            return Err("Binary gateway frames are not supported yet".into());
        }
        WebsocketMessage::Frame(frame) => {
            return Err(format!("Frame: {frame:?}").into());
        }
        WebsocketMessage::Close(frame) => {
            return Err(format!("Close frame: {frame:?}").into());
        }
        WebsocketMessage::Ping(_) => return Err("Ping frame".into()),
        WebsocketMessage::Pong(_) => return Err("Pong frame".into()),
    };
    Ok(json)
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
    async fn is_beat_time(&mut self) -> bool {
        if poll!(&mut self.future).is_ready() {
            self.future = Box::pin(Delay::new(self.duration));
            return true;
        }
        false
    }
}

impl Stream for Discord {
    type Item = SocketEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.next().poll_unpin(cx)
    }
}

#[async_trait]
impl Socket for Discord {
    async fn next(self: Arc<Self>) -> Option<SocketEvent> {
        let (event, voice_event) = loop {
            let mut gateaway = self.gateaway.lock().await;
            let event = match gateaway.as_mut() {
                Some(gateaway) => {
                    if gateaway.heart_beating.is_beat_time().await {
                        gateaway
                            .websocket
                            .send(
                                json!({
                                        "op": Opcode::Heartbeat as u8,
                                        "d": gateaway.last_sequence_number,
                                })
                                .to_string()
                                .into(),
                            )
                            .await
                            .unwrap();
                    }

                    match poll!(pin!(gateaway.fetch_event())) {
                        Poll::Ready(Ok(Some(event))) => Some(event),

                        Poll::Ready(Ok(None)) => {
                            info!("Stream closed");
                            None
                        }
                        Poll::Ready(Err(err)) => {
                            warn!("Failed to parse event: {err}");
                            continue;
                        }
                        Poll::Pending => None,
                    }
                }
                None => None,
            };
            drop(gateaway);

            let mut voice_gateaway_state = self.voice_gateaway.lock().await;
            let voice_event = if let Some(voice_gateaway) = voice_gateaway_state.mut_gateaway() {
                // Heartbeat
                if voice_gateaway.heart_beating.is_beat_time().await {
                    voice_gateaway
                        .websocket
                        .send(
                            json!({
                                    "op": Opcode::Heartbeat as u8,
                                    "d": voice_gateaway.last_sequence_number,
                            })
                            .to_string()
                            .into(),
                        )
                        .await
                        .unwrap();
                }
                // Poll event
                match { poll!(pin!(voice_gateaway.fetch_event())) } {
                    Poll::Ready(Ok(Some(voice_event))) => Some(voice_event),
                    Poll::Ready(Ok(None)) => {
                        info!("Stream closed");
                        voice_gateaway_state.close_gateway();
                        None
                    }
                    Poll::Ready(Err(err)) => {
                        warn!("Failed to parse voice_event: {err}");
                        if event.is_none() {
                            continue;
                        }
                        None
                    }
                    Poll::Pending => None,
                }
            } else {
                None
            };

            match (event, voice_event) {
                (None, None) => {}
                (event, voice_event) => break (event, voice_event),
            };

            if let Some(voice_gateaway) = voice_gateaway_state.mut_gateaway() {
                let voice_gateaway_data = &mut voice_gateaway.type_specific_data;
                if let Some(connection) = voice_gateaway_data.connection.as_mut()
                    && let Some(description) = connection.description()
                {
                    // === Send audio, from the mic. ===
                    let VoiceGateawayData {
                        input_buffer,
                        input_channel,
                        ..
                    } = voice_gateaway_data;

                    match input_channel {
                        InputChannel::None => {
                            // Handle setting up a voice input channel if one doesn't exist.
                            let (sender, receiver) = oneshot::channel();
                            voice_gateaway.input_channel = InputChannel::Initilizing(receiver);
                            return Some(SocketEvent::AddAudioInput(sender));
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

                            if !voice_gateaway_data.is_speaking {
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
                                    warn!("Failed to send speaking update: {err}");
                                } else {
                                    info!("Successfuly sent a start speak event");
                                    voice_gateaway_data.is_speaking = true;
                                }
                            }

                            if let Err(err) = connection.send_audio_frame(&frame).await {
                                warn!("Failed to send voice audio frame: {err}");
                            }
                        }
                    }

                    // TODO
                    // if voice_gateaway_data.is_speaking {
                    //     voice_gateaway_data.is_speaking = false;
                    //     let speaking_payload = json!({
                    //         "op": voice::VoiceOpcode::Speaking as u8,
                    //         "d": {
                    //             "speaking": 0,
                    //             "delay": 0,
                    //             "ssrc": connection.ssrc(),
                    //         }
                    //     })
                    //     .to_string();
                    //     if let Err(err) =
                    //         voice_gateaway.websocket.send(speaking_payload.into()).await
                    //     {
                    //         warn!("Failed to send speaking update: {err}");
                    //     } else {
                    //         info!("Successfuly sent a stop speak event");
                    //     }
                    // }

                    // === Recive, and play audio ===
                    if let Poll::Ready(Some((ssrc, audio_frame))) =
                        poll!(pin!(connection.recv_audio()))
                    {
                        let mut channel =
                            match voice_gateaway_data.ssrc_to_audio_channel.entry(ssrc) {
                                hash_map::Entry::Occupied(channel) => channel,
                                hash_map::Entry::Vacant(e) => {
                                    let (sender, reciver) = oneshot::channel();
                                    e.insert(voice::AudioChannel::Initilizing(reciver));
                                    return Some(SocketEvent::AddAudioSource(sender));
                                }
                            };
                        match channel.get_mut() {
                            voice::AudioChannel::Initilizing(receiver) => {
                                let producer = match receiver.try_recv().unwrap() {
                                    Some(producer) => producer,
                                    None => continue,
                                };
                                channel.insert(voice::AudioChannel::Connected(producer));
                            }
                            voice::AudioChannel::Connected(producer) => {
                                // error!("{ssrc}: Talking: {audio_frame:?}");
                                producer.push_iter(
                                    audio_frame
                                        .iter()
                                        .map(|sample| *sample as f32 / i16::MAX as f32),
                                );
                            }
                        };
                    }
                };

                continue;
            };

            drop(voice_gateaway_state);
            pending!()
        };

        if let Some(event) = voice_event {
            match event.exec(&self).await {
                Ok(event) => {}
                Err(err) => {
                    warn!("Failed to execute voice_gateway event: {err}");
                }
            };
        };

        if let Some(event) = event {
            match event.exec(&self).await {
                Ok(event) => return Some(event),
                Err(err) => {
                    warn!("Failed to execute gateway event: {err}");
                    // return None;
                }
            };
        };

        Some(SocketEvent::Skip)
    }
}

#[async_trait]
impl Voice for Discord {
    async fn connect<'a>(&'a self, location: &Identifier<Place<Room>>) {
        let mut voice_gateaway = self.voice_gateaway.lock().await;
        *voice_gateaway = VoiceGateawayState::AwaitingData;

        let channels_map = self.channel_id_mappings.read().await;
        let channel = match channels_map.get(location.id()) {
            Some(c) => c,
            None => {
                // TODO(discord-migration): ensure all Rooms returned by Query have a mapping,
                // and support guild voice channels too.
                warn!("Tried to connect voice for a Room without a discord channel mapping");
                return;
            }
        };

        let payload = json!({
            "op": Opcode::VoiceStateUpdate as u8,
            "d": {
                "guild_id": channel.guild_id,
                "channel_id": channel.id,
                "self_mute": false,
                "self_deaf": false
              }
        });

        let mut gateaway = self.gateaway.lock().await;
        let gateaway = gateaway.as_mut().unwrap();

        gateaway
            .websocket
            .send(payload.to_string().into())
            .await
            .unwrap();
    }
    async fn disconnect<'a>(&'a self, location: &Identifier<Place<Room>>) {
        let mut voice_gateaway = self.voice_gateaway.lock().await;
        *voice_gateaway = VoiceGateawayState::Closed;

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
                "channel_id": None::<String>,
                "self_mute": false,
                "self_deaf": false
              }
        });

        let mut gateaway = self.gateaway.lock().await;
        let gateaway = gateaway.as_mut().unwrap();

        gateaway
            .websocket
            .send(payload.to_string().into())
            .await
            .unwrap();
    }
}
