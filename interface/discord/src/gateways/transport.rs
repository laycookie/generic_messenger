use std::iter;

use async_tungstenite::{
    WebSocketReceiver, WebSocketSender, WebSocketStream,
    smol::ConnectStream,
    tungstenite::{Bytes, Message as WebsocketMessage},
};
use futures::{StreamExt, stream::FusedStream};
use tracing::warn;

use super::frame::{GatewayFrame, GatewayFrameConversionError};

pub(crate) struct Websocket {
    pub(super) sender: futures::lock::Mutex<WebSocketSender<ConnectStream>>,
    pub(super) receiver: futures::lock::Mutex<WebSocketReceiver<ConnectStream>>,
}

impl Websocket {
    pub(super) fn new(websocket: WebSocketStream<ConnectStream>) -> Self {
        let (sender, receiver) = websocket.split();
        Self {
            sender: sender.into(),
            receiver: receiver.into(),
        }
    }

    pub(super) async fn send(
        &self,
        msg: WebsocketMessage,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        let mut sender = self.sender.lock().await;
        sender.send(msg).await
    }

    pub(super) async fn send_binary(
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
