use std::{sync::Weak, task::Poll};

use adaptors::{Socket, SocketEvent};
use futures::{
    FutureExt, Stream
};

use crate::messanger_unifier::MessangerHandle;


pub struct ActiveStream {
    handle: MessangerHandle,
    socket: Weak<dyn Socket + Send + Sync>,
}
impl ActiveStream {
    pub fn new(handle: MessangerHandle, socket: Weak<dyn Socket + Send + Sync>) -> Self {
        ActiveStream {
            handle,
            socket
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

        match stream.next().poll_unpin(cx) {
            Poll::Ready(Some(socket_event)) => {
                if let SocketEvent::Skip = socket_event {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Ready(Some((self.handle, socket_event)))
            },
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
