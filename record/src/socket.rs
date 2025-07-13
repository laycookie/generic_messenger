use std::{sync::Weak, task::Poll};

use adaptors::{Socket, SocketEvent};
use futures::{
    FutureExt, Stream, StreamExt,
    channel::mpsc::{self, Receiver, Sender},
};

use crate::messanger_unifier::MessangerHandle;

pub enum ReciverEvent {
    Connection((MessangerHandle, Option<Weak<dyn Socket + Send + Sync>>)),
}

#[derive(Debug, Clone)]
pub struct ActiveStream {
    pub(crate) handle: MessangerHandle,
    pub(crate) socket: Weak<dyn Socket + Send + Sync>,
}

pub struct SocketsInterface {
    receiver: Receiver<ReciverEvent>,
    active_streams: Vec<ActiveStream>,
}

impl Stream for ActiveStream {
    type Item = (MessangerHandle, SocketEvent);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let Some(stream) = self.socket.upgrade() else {
            return Poll::Ready(None);
        };

        match stream.next().poll_unpin(cx) {
            Poll::Ready(Some(socket_event)) => Poll::Ready(Some((self.handle, socket_event))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
