use std::{
    collections::hash_map,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::Duration,
};

use async_trait::async_trait;
use async_tungstenite::{
    WebSocketStream, async_std::ConnectStream, tungstenite::Message as WebsocketMessage,
};
use facet::Facet;
use futures::{
    FutureExt as _, Stream, StreamExt, channel::oneshot, future::poll_fn, pending, poll,
};
use futures_timer::Delay;
use messenger_interface::interface::{Socket, SocketEvent};
use simple_audio_channels::Producer;
use surf::http::convert::json;
use tracing::warn;

use crate::{Discord, gateaways::general::Opcode};

pub mod general;
pub mod voice;

/// <https://discord.com/developers/docs/events/gateway-events#payload-structure>
#[derive(Debug, Facet)]
pub struct GatewayPayload<Op> {
    // Opcode
    op: Op,
    // Event type
    t: Option<String>,
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
                    return facet_format_json::from_str::<GatewayPayload<Op>>(&utf8).unwrap()
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
        WebsocketMessage::Text(text) => {
            facet_format_json::from_str::<GatewayPayload<Op>>(text).unwrap()
        }
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

                    match poll!(poll_fn(|cx| gateaway.fetch_event(cx))) {
                        Poll::Ready(events) => Some(events),
                        _ => None,
                    }
                }
                None => None,
            };
            drop(gateaway);

            let mut voice_gateaway = self.voice_gateaway.lock().await;
            let voice_event = match voice_gateaway.mut_gateaway() {
                Some(voice_gateaway) => {
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

                    match poll!(poll_fn(|cx| voice_gateaway.fetch_event(cx))) {
                        Poll::Ready(events) => Some(events),
                        _ => None,
                    }
                }
                None => None,
            };

            match (event, voice_event) {
                (None, None) => {}
                (event, voice_event) => break (event, voice_event),
            };

            if let Some(voice_gateaway) = voice_gateaway.mut_gateaway()
                && let Some(connection) = voice_gateaway.connection.as_mut()
                && let Some(description) = connection.description()
                && description.mode().is_some()
            {
                if let Some((ssrc, audio_frame)) = connection.recv_audio().await {
                    let mut channel = match voice_gateaway.ssrc_to_audio_channel.entry(ssrc) {
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
                            producer.push_iter(
                                audio_frame
                                    .iter()
                                    .map(|sample| *sample as f32 / i16::MAX as f32),
                            );
                        }
                    };
                }
                continue;
            };

            drop(voice_gateaway);
            pending!()
        };

        if let Some(Ok(event)) = voice_event {
            match event.exec(&self).await {
                Ok(event) => {}
                Err(err) => {
                    warn!("Failed to execute voice_gateway event: {err:#?}");
                }
            };
        };

        if let Some(Ok(event)) = event {
            match event.exec(&self).await {
                Ok(event) => return Some(event),
                Err(err) => {
                    warn!("Failed to execute gateway event: {err:#?}");
                    // return None;
                }
            };
        };

        Some(SocketEvent::Skip)
    }
}
