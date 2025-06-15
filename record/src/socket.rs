use std::{
    sync::{Arc, Weak},
    task::Poll,
};

use adaptors::{Messanger as Auth, Socket, SocketUpdate};
use futures::{
    channel::mpsc::{self, Receiver, Sender},
    FutureExt, Stream, StreamExt,
};

pub enum ReciverEvent {
    Connection(Arc<dyn Auth>),
}

#[derive(Debug)]
pub enum SocketEvent {
    Connect(Sender<ReciverEvent>),
    Message(SocketUpdate),
}

pub struct SocketConnection {
    receiver: Option<Receiver<ReciverEvent>>,
    active_streams: Vec<Weak<dyn Socket + Send + Sync>>,
    ready_events: Vec<SocketUpdate>,
}
impl SocketConnection {
    pub fn new() -> Self {
        SocketConnection {
            receiver: None,
            active_streams: Vec::new(),
            ready_events: Vec::new(),
        }
    }
}

impl Stream for SocketConnection {
    type Item = SocketEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let Some(ref mut reciever) = self.receiver else {
            let (sender, receiver) = mpsc::channel::<ReciverEvent>(128);
            self.receiver = Some(receiver);
            return Poll::Ready(Some(SocketEvent::Connect(sender)));
        };

        if let Poll::Ready(m) = reciever.select_next_some().poll_unpin(cx) {
            match m {
                ReciverEvent::Connection(auth) => {
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
                        self.active_streams.push(stream);
                        println!("Pushed as active");
                    }
                }
            }
        };

        // Return events, before fetching new ones
        if let Some(e) = self.ready_events.pop() {
            return Poll::Ready(Some(SocketEvent::Message(e)));
        }

        let mut new_events = Vec::with_capacity(self.active_streams.len());
        self.active_streams.retain(|stream| {
            // Underlying messenger got dropped.
            let Some(stream) = stream.upgrade() else {
                println!("Got dropped");
                return false;
            };

            let mut next = stream.next();
            match next.poll_unpin(cx) {
                Poll::Ready(Some(update)) => {
                    new_events.push(update);
                    true
                }
                Poll::Ready(None) => false, // The stream got closed
                Poll::Pending => true,
            }
        });
        self.ready_events.extend(new_events);

        cx.waker().wake_by_ref();
        return Poll::Pending;
    }
}
