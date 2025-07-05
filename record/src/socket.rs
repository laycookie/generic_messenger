use std::{
    sync::{Arc, Weak},
    task::Poll,
};

use adaptors::{Messanger as Auth, Socket, SocketEvent};
use futures::{
    FutureExt, Stream, StreamExt,
    channel::mpsc::{self, Receiver, Sender},
};

use crate::messanger_unifier::MessangerHandle;

pub enum ReciverEvent {
    Connection((MessangerHandle, Arc<dyn Auth>)),
}

pub struct SocketsInterface {
    receiver: Receiver<ReciverEvent>,
    active_streams: Vec<(MessangerHandle, Weak<dyn Socket + Send + Sync>)>,
    ready_events: Vec<(MessangerHandle, SocketEvent)>,
}
impl SocketsInterface {
    pub fn new() -> (Self, Sender<ReciverEvent>) {
        let (sender, receiver) = mpsc::channel::<ReciverEvent>(128);
        (
            SocketsInterface {
                receiver,
                active_streams: Vec::new(),
                ready_events: Vec::new(),
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
        if let Poll::Ready(m) = self.receiver.select_next_some().poll_unpin(cx) {
            match m {
                ReciverEvent::Connection((handle, auth)) => {
                    let mut stream_fut = auth.socket();

                    let stream;
                    // TODO: Make this none blocking
                    loop {
                        if let Poll::Ready(val) = stream_fut.poll_unpin(cx) {
                            stream = val;
                            break;
                        }
                    }
                    if let Some(stream) = stream {
                        self.active_streams.push((handle, stream));
                        println!("Pushed as active");
                    }
                }
            }
        };

        if let Some(e) = self.ready_events.pop() {
            return Poll::Ready(Some(e));
        }

        let mut new_events = Vec::with_capacity(self.active_streams.len());
        self.active_streams.retain(|(handle, stream)| {
            let Some(stream) = stream.upgrade() else {
                println!("Got dropped");
                return false;
            };

            let mut next = stream.next();
            match next.poll_unpin(cx) {
                Poll::Ready(Some(update)) => {
                    new_events.push((handle.to_owned(), update));
                    true
                }
                Poll::Ready(None) => false, // The stream got closed
                Poll::Pending => true,
            }
        });
        if new_events.len() > 0 {
            self.ready_events.extend(new_events);
            cx.waker().wake_by_ref();
        }

        return Poll::Pending;
    }
}
