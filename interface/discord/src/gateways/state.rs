use std::{
    error::Error,
    io,
    ops::Deref,
    pin::{Pin, pin},
    sync::{OnceLock, atomic::AtomicUsize},
    time::Duration,
};

use async_tungstenite::tungstenite::Message as WebsocketMessage;
use facet::Facet;
use futures::{StreamExt, lock::Mutex as AsyncMutex};
use futures_timer::Delay;
use tracing::warn;

use super::{
    Gateway, GatewayFrame, GatewayFrameConversionError, GatewayPayload, GatewayStreamExt, Websocket,
};

pub(crate) struct Handshaking;
pub(crate) struct Connected;

pub(crate) trait GatewayState<T> {
    type Data;
}

pub(crate) struct ConnectedData<T> {
    pub(super) heartbeat_timer: AsyncMutex<HeartbeatTimer>,
    pub(super) last_sequence_number: OnceLock<AtomicUsize>,
    pub(super) type_specific_data: T,
}

impl<T> GatewayState<T> for Connected {
    type Data = ConnectedData<T>;
}

impl<T, S> Gateway<T, S>
where
    S: GatewayState<T>,
{
    pub(crate) async fn send(
        &self,
        msg: WebsocketMessage,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.websocket.send(msg).await
    }
}

impl Websocket {
    pub(super) fn into_gateway<T>(self, heartbeat_interval: Duration, data: T) -> Gateway<T> {
        Gateway {
            websocket: self,
            state: ConnectedData {
                heartbeat_timer: HeartbeatTimer::new(heartbeat_interval).into(),
                last_sequence_number: OnceLock::new(),
                type_specific_data: data,
            },
            _marker: std::marker::PhantomData,
        }
    }
}

pub(crate) async fn expect_hello<Op, H>(
    websocket: &Websocket,
    is_hello: impl FnOnce(&Op) -> bool,
    gateway_name: &str,
) -> Result<H, Box<dyn Error + Send + Sync>>
where
    Op: for<'facet> Facet<'facet> + TryFrom<u8>,
    H: Facet<'static>,
    GatewayFrame<Op>: TryFrom<WebsocketMessage, Error = GatewayFrameConversionError>,
{
    let mut receiver = websocket.receiver.lock().await;
    let mut frames = pin!(receiver.gateway_frames::<Op>());

    loop {
        let frame = frames.next().await.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{gateway_name} closed before receiving hello"),
            )
        })?;
        let payload: GatewayPayload<'_, Op> = match frame.payload() {
            Ok(payload) => payload,
            Err(err) => {
                warn!("Failed to parse {gateway_name} payload while waiting for Hello: {err}");
                continue;
            }
        };

        if !is_hello(payload.opcode()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{gateway_name} sent a non-Hello first event"),
            )
            .into());
        }

        return Ok(payload.decode()?);
    }
}

#[allow(async_fn_in_trait)]
pub(crate) trait Handshake: Sized {
    type Output;

    async fn handshake(self) -> Result<Self::Output, Box<dyn Error + Send + Sync>>;
}

/// Protocol-specific scheduling and sending of Discord heartbeats.
#[allow(async_fn_in_trait)]
pub(crate) trait Heartbeat {
    async fn heartbeat(&self) -> Result<(), async_tungstenite::tungstenite::Error>;
}

impl<T> Deref for Gateway<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.state.type_specific_data
    }
}

pub(super) struct HeartbeatTimer {
    duration: Duration,
    // Keep deadline across cancelled `heartbeat()` futures created by polling `select!` rounds.
    future: Pin<Box<dyn Future<Output = ()> + Send + Sync>>,
}

impl HeartbeatTimer {
    fn new(duration: Duration) -> Self {
        Self {
            duration,
            future: Box::pin(Delay::new(duration)),
        }
    }

    pub(super) async fn wait(&mut self) {
        (&mut self.future).await;
        self.future = Box::pin(Delay::new(self.duration));
    }
}
