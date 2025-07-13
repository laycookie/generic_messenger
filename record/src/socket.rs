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

struct ActiveStream {
    handle: MessangerHandle,
    socket: Weak<dyn Socket + Send + Sync>,
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
                        self.active_streams.push(ActiveStream { handle, socket });
                        println!("Pushed as active");
                    }
                }
            }
        };
        // Prep some stuff pre-pulling events
        let mut open_streams = Vec::new();
        {
            let mut i = 0usize;
            self.active_streams.retain(|stream| {
                let mut retain = false;
                if let Some(socket) = stream.socket.upgrade() {
                    open_streams.push((i, socket));
                    retain = true;
                };
                i += 1;
                retain
            });
        }

        // Pull events
        for (i, stream) in open_streams.iter() {
            let polled_event = stream.clone().next().poll_unpin(cx);
            match polled_event {
                Poll::Ready(Some(event)) => {
                    let SocketEvent::Skip = event else {
                        return Poll::Ready(Some((self.active_streams[*i].handle, event)));
                    };
                    cx.waker().wake_by_ref();
                    continue;
                }
                Poll::Ready(None) => self.active_streams.remove(*i),
                Poll::Pending => continue,
            };
        }

        Poll::Pending
    }
}
