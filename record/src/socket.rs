use std::{
    sync::{Arc, Weak},
    task::Poll,
};

use adaptors::{Messanger as Auth, Socket};
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
    Echo(usize),
}

pub struct SocketConnection {
    receiver: Option<Receiver<ReciverEvent>>,
    active_streams: Vec<Weak<dyn Socket + Send + Sync>>,
}
impl SocketConnection {
    pub fn new() -> Self {
        SocketConnection {
            receiver: None,
            active_streams: Vec::new(),
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

        self.active_streams.retain(|stream| {
            // Underlying messenger got dropped.
            let Some(stream) = stream.upgrade() else {
                println!("Got dropped");
                return false;
            };

            // The stream got closed
            if let Poll::Ready(val) = stream.next().poll_unpin(cx) {
                println!("PULLING: {:?}", val.is_some());
                return val.is_some();
            };

            true
        });

        cx.waker().wake_by_ref();
        return Poll::Pending;
    }
}
