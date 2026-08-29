use std::{
    io, iter,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use async_tungstenite::{
    WebSocketReceiver, WebSocketSender, WebSocketStream,
    smol::ConnectStream,
    tungstenite::{Bytes, Message as WebsocketMessage},
};
use futures::{
    StreamExt,
    future::{Either, select},
    stream::FusedStream,
};
use futures_timer::Delay;
use tracing::warn;

use super::frame::{GatewayFrame, GatewayFrameConversionError};

const WEBSOCKET_SEND_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct Websocket {
    pub(super) sender: futures::lock::Mutex<WebSocketSender<ConnectStream>>,
    pub(super) receiver: futures::lock::Mutex<WebSocketReceiver<ConnectStream>>,
    send_failed: AtomicBool,
}

impl Websocket {
    pub(super) fn new(websocket: WebSocketStream<ConnectStream>) -> Self {
        let (sender, receiver) = websocket.split();
        Self {
            sender: sender.into(),
            receiver: receiver.into(),
            send_failed: AtomicBool::new(false),
        }
    }

    pub(super) fn send_failed(&self) -> bool {
        self.send_failed.load(Ordering::Acquire)
    }

    async fn send_with_timeout(
        &self,
        msg: WebsocketMessage,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        if self.send_failed.load(Ordering::Acquire) {
            return Err(async_tungstenite::tungstenite::Error::ConnectionClosed);
        }
        let send = async {
            let mut sender = self.sender.lock().await;
            // A concurrent send may have poisoned the split sink while this
            // caller waited for its lock.
            if self.send_failed.load(Ordering::Acquire) {
                return Err(async_tungstenite::tungstenite::Error::ConnectionClosed);
            }
            let result = sender.send(msg).await;
            if result.is_err() {
                // Publish poison before releasing the sender lock so a waiter
                // can never reuse a sink after its first transport error.
                self.send_failed.store(true, Ordering::Release);
            }
            result
        };
        match select(Box::pin(send), Box::pin(Delay::new(WEBSOCKET_SEND_TIMEOUT))).await {
            Either::Left((result, _timeout)) => result,
            Either::Right(((), pending_send)) => {
                // The pending future may own the sender lock. Poison first,
                // then cancel/drop it so the next waiter observes failure.
                self.send_failed.store(true, Ordering::Release);
                drop(pending_send);
                Err(async_tungstenite::tungstenite::Error::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "Discord websocket send exceeded {} second deadline",
                        WEBSOCKET_SEND_TIMEOUT.as_secs()
                    ),
                )))
            }
        }
    }

    pub(super) async fn send(
        &self,
        msg: WebsocketMessage,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.send_with_timeout(msg).await
    }

    pub(super) async fn send_binary(
        &self,
        op: u8,
        msg: impl Iterator<Item = u8>,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.send_with_timeout(WebsocketMessage::binary(Bytes::from_iter(
            iter::once(op).chain(msg),
        )))
        .await
    }
}

pub(crate) trait GatewayStreamExt: FusedStream {
    fn gateway_frames<Op>(&mut self) -> impl FusedStream<Item = GatewayFrame<Op>> + '_
    where
        GatewayFrame<Op>: TryFrom<WebsocketMessage, Error = GatewayFrameConversionError>;
}

impl GatewayStreamExt for WebSocketReceiver<ConnectStream> {
    fn gateway_frames<Op>(&mut self) -> impl FusedStream<Item = GatewayFrame<Op>> + '_
    where
        GatewayFrame<Op>: TryFrom<WebsocketMessage, Error = GatewayFrameConversionError>,
    {
        self.filter_map(async |msg| -> Option<GatewayFrame<Op>> {
            match msg {
                Ok(msg) => match GatewayFrame::try_from(msg) {
                    Ok(frame) => Some(frame),
                    Err(GatewayFrameConversionError::Control) => None,
                    Err(GatewayFrameConversionError::UnexpectedBinary) => {
                        warn!("Main gateway sent an unexpected binary websocket frame");
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
