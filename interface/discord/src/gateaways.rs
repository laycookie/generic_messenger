use std::{
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
use futures::{FutureExt as _, Stream, StreamExt, future::poll_fn, pending, poll};
use futures_timer::Delay;
use messaging_interface::interface::{Socket, SocketEvent};
use surf::http::convert::json;
use tracing::{error, info, warn};

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
        match self.next().await.unwrap().unwrap() {
            WebsocketMessage::Text(utf8_bytes) => {
                facet_format_json::from_str::<GatewayPayload<Op>>(&utf8_bytes).unwrap()
            }
            _ => todo!(),
        }
    }
}

fn deserialize_event<Op: for<'a> Facet<'a>>(
    event: &WebsocketMessage,
) -> Result<GatewayPayload<Op>, Box<dyn std::error::Error + Send + Sync>> {
    let json = match event {
        WebsocketMessage::Text(text) => {
            facet_format_json::from_str::<GatewayPayload<Op>>(text).unwrap()
        }
        WebsocketMessage::Binary(_) => todo!(),
        WebsocketMessage::Frame(frame) => {
            return Err(format!("Frame: {frame:?}").into());
        }
        WebsocketMessage::Close(frame) => {
            return Err(format!("Close frame: {frame:?}").into());
        }
        WebsocketMessage::Ping(_) => todo!(),
        WebsocketMessage::Pong(_) => todo!(),
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
                connection.next().await;
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
