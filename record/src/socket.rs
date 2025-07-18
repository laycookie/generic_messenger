use std::{fmt::Debug, pin::Pin, sync::Weak, task::Poll};

use adaptors::{Socket, SocketEvent};
use futures::{FutureExt, Stream, poll};

use crate::messanger_unifier::MessangerHandle;

pub struct ActiveStream {
    handle: MessangerHandle,
    socket: Weak<dyn Socket + Send + Sync>,
    fut: Option<Pin<Box<dyn Future<Output = Option<SocketEvent>> + Send>>>,
}
impl Debug for ActiveStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveStream")
            .field("handle", &self.handle)
            .field("socket", &self.socket)
            .field("fut", &self.fut.is_some())
            .finish()
    }
}

impl ActiveStream {
    pub fn new(handle: MessangerHandle, socket: Weak<dyn Socket + Send + Sync>) -> Self {
        ActiveStream {
            handle,
            socket,
            fut: None,
        }
    }
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
        if self.fut.is_none() {
            self.fut = Some(stream.clone().next());
        }
        match self.fut.as_mut().unwrap().poll_unpin(cx) {
            Poll::Ready(Some(socket_event)) => {
                self.fut = None;
                if let SocketEvent::Skip = socket_event {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Ready(Some((self.handle, socket_event)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
