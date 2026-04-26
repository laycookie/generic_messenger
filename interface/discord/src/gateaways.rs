use std::{
    iter,
    ops::Deref,
    pin::Pin,
    sync::{
        OnceLock,
        atomic::AtomicUsize,
    },
    time::Duration,
};

use async_tungstenite::{
    WebSocketReceiver, WebSocketSender, WebSocketStream,
    async_std::ConnectStream,
    tungstenite::{Bytes, Message as WebsocketMessage},
};
use facet::Facet;
use futures::{StreamExt, lock::Mutex as AsyncMutex};
use futures_timer::Delay;
use tracing::warn;

use crate::gateaways::general::GatewayEvent;

pub mod general;
pub mod voice;
mod polling;

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
        sender
            .send(WebsocketMessage::binary(Bytes::from_iter(
                iter::once(op).chain(msg),
            )))
            .await
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
impl<T> Gateaway<T> {}
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
