use std::{
    iter,
    ops::Deref,
    pin::{Pin, pin},
    sync::{OnceLock, atomic::AtomicUsize},
    time::Duration,
};

use async_tungstenite::{
    WebSocketReceiver, WebSocketSender, WebSocketStream,
    async_std::ConnectStream,
    tungstenite::{Bytes, Message as WebsocketMessage},
};
use facet::Facet;
use futures::{Stream, StreamExt, lock::Mutex as AsyncMutex, stream::FusedStream};
use futures_timer::Delay;
use tracing::warn;

use crate::gateways::general::GatewayEvent;

pub mod general;
mod polling;
pub mod voice;

struct Websocket {
    sender: AsyncMutex<WebSocketSender<ConnectStream>>,
    receiver: AsyncMutex<WebSocketReceiver<ConnectStream>>,
}
impl Websocket {
    fn new(websocket: WebSocketStream<ConnectStream>) -> Self {
        let (sender, receiver) = websocket.split();
        Self {
            sender: sender.into(),
            receiver: receiver.into(),
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
        sender
            .send(WebsocketMessage::binary(Bytes::from_iter(
                iter::once(op).chain(msg),
            )))
            .await
    }
}
trait GatewayStreamReciver: FusedStream {
    fn filter_payload<Op: Facet<'static> + TryFrom<u8>>(
        &mut self,
    ) -> impl FusedStream<Item = GatewayPayload<Op>> + '_;
}
impl GatewayStreamReciver for WebSocketReceiver<ConnectStream> {
    fn filter_payload<Op: Facet<'static> + TryFrom<u8>>(
        &mut self,
    ) -> impl FusedStream<Item = GatewayPayload<Op>> + '_ {
        self.filter_map(async |msg| -> Option<GatewayPayload<Op>> {
            match msg {
                Ok(msg) => match parse_gateway_event::<Op>(msg) {
                    Some(payload) => Some(payload),
                    None => {
                        warn!("Something went wrong parsing the event payload");
                        None
                    }
                },
                Err(err) => {
                    warn!("Gateway websocket error while waiting for payload: {err:#?}");
                    None
                }
            }
        })
    }
}

/// Parse a text websocket message into a `GatewayPayload`.
/// Returns `None` for non-text frames (binary, ping, close, etc.).
fn parse_gateway_event<Op: Facet<'static> + TryFrom<u8>>(
    msg: WebsocketMessage,
) -> Option<GatewayPayload<Op>> {
    match msg {
        WebsocketMessage::Text(utf8) => match facet_json::from_str::<GatewayPayload<Op>>(&utf8) {
            Ok(payload) => Some(payload),
            Err(err) => {
                warn!("Failed to parse gateway payload: {err}");
                None
            }
        },
        WebsocketMessage::Binary(bytes) => match voice::VoiceBinaryFrame::parse(&bytes) {
            Ok(frame) => {
                let op = Op::try_from(frame.opcode as u8).ok()?;
                Some(GatewayPayload::new_binary(
                    op,
                    frame.sequence.map(|s| s as usize),
                    frame.payload,
                ))
            }
            Err(err) => {
                warn!("Failed to parse binary frame: {err}");
                None
            }
        },
        _ => None,
    }
}

pub struct Gateway<T> {
    websocket: Websocket,
    heart_beating: AsyncMutex<HeartBeatingData>,
    last_sequence_number: OnceLock<AtomicUsize>,
    type_specific_data: T,
}
impl<T> Gateway<T> {}
impl<T> Deref for Gateway<T> {
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
    /// Construct a `GatewayPayload` from a binary websocket frame.
    ///
    /// `d` must be the payload bytes **without** the leading opcode byte
    /// (the opcode is already captured in `op`).
    pub fn new_binary(op: Op, s: Option<usize>, d: Vec<u8>) -> Self {
        Self {
            op,
            t: None,
            s,
            d: facet_value::to_value(&d).unwrap(),
        }
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
