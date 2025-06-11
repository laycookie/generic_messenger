use std::{sync::Arc, task::Poll, thread::sleep, time::Duration};

use adaptors::{Messanger as Auth, TestStream};
use futures::{
    channel::mpsc::{self, Receiver, Sender},
    future::join_all,
    lock::Mutex,
    pending, poll,
    stream::FuturesUnordered,
    FutureExt, SinkExt, Stream, StreamExt,
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
    active_streams: Vec<Arc<dyn TestStream + Send + Sync>>,
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
                    let socket = auth.socket().unwrap();
                    let mut stream_fut = socket.get_stream();

                    let stream;
                    loop {
                        if let Poll::Ready(val) = stream_fut.poll_unpin(cx) {
                            stream = val;
                            break;
                        }
                    }

                    self.active_streams.push(stream);
                    println!("Pushed as active");
                }
            }
        };

        for stream in self.active_streams.iter() {
            if let Poll::Ready(val) = stream.next().poll_unpin(cx) {
                println!("{:?}", val);
            };
        }

        cx.waker().wake_by_ref();
        return Poll::Pending;
    }
}
