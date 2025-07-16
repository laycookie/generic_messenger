use std::{fmt::Debug, pin::Pin, sync::Weak, task::Poll};

use adaptors::{Socket, SocketEvent};
use futures::{
    FutureExt, Stream, StreamExt,
    channel::mpsc::{self, Receiver, Sender},
};

use crate::messanger_unifier::MessangerHandle;

pub enum ReciverEvent {
    Connection((MessangerHandle, Option<Weak<dyn Socket + Send + Sync>>)),
}

struct ActiveStream {
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
    fn new(handle: MessangerHandle, socket: Weak<dyn Socket + Send + Sync>) -> Self {
        Self {
            handle,
            socket,
            fut: None,
        }
    }
}

pub struct SocketsInterface {
    receiver: Receiver<ReciverEvent>,
    active_streams: Vec<ActiveStream>,
}
impl SocketsInterface {
    pub fn new() -> (Self, Sender<ReciverEvent>) {
        let (sender, receiver) = mpsc::channel::<ReciverEvent>(128);
        (
            SocketsInterface {
                receiver,
                active_streams: Vec::new(),
            },
            sender,
        )
    }
}

impl Stream for SocketsInterface {
    type Item = (MessangerHandle, SocketEvent);

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        // Check if we got anything new from the outside
        if let Poll::Ready(event) = self.receiver.select_next_some().poll_unpin(cx) {
            match event {
                ReciverEvent::Connection((handle, socket)) => {
                    if let Some(socket) = socket {
                        self.active_streams.push(ActiveStream::new(handle, socket));
                        println!("Pushed as active");
                    }
                }
            }
        };
        // Prep some stuff pre-pulling events
        let mut open_streams = Vec::new();
        self.active_streams.retain(|stream| {
            if let Some(socket) = stream.socket.upgrade() {
                open_streams.push(socket);
                return true;
            };
            false
        });

        // Pull events
        for (i, stream) in open_streams.iter().enumerate() {
            if self.active_streams[i].fut.is_none() {
                self.active_streams[i].fut = Some(stream.clone().next());
            }
            match self.active_streams[i].fut.as_mut().unwrap().poll_unpin(cx) {
                Poll::Ready(Some(event)) => {
                    self.active_streams[i].fut = None;
                    let SocketEvent::Skip = event else {
                        return Poll::Ready(Some((self.active_streams[i].handle, event)));
                    };
                    cx.waker().wake_by_ref();
                    continue;
                }
                Poll::Ready(None) => self.active_streams.remove(i),
                Poll::Pending => continue,
            };
        }

        Poll::Pending
    }
}
