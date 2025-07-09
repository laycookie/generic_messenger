use std::{
    pin::Pin,
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

struct ActiveStream {
    handle: MessangerHandle,
    socket: Weak<dyn Socket + Send + Sync>,
    future: Option<Pin<Box<dyn Future<Output = Option<SocketEvent>> + Send>>>,
    silent_future: Option<Pin<Box<dyn Future<Output = Option<()>> + Send>>>,
}

pub struct SocketsInterface {
    receiver: Receiver<ReciverEvent>,
    active_streams: Vec<ActiveStream>,
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
                    loop {
                        if let Poll::Ready(val) = stream_fut.poll_unpin(cx) {
                            stream = val;
                            break;
                        }
                    }
                    if let Some(stream) = stream {
                        self.active_streams.push(ActiveStream {
                            handle,
                            socket: stream,
                            future: None,
                            silent_future: None,
                        });
                        println!("Pushed as active");
                    }
                }
            }
        };

        if let Some(e) = self.ready_events.pop() {
            return Poll::Ready(Some(e));
        }

        // Pull streams, and collect inactive ones
        let mut new_events = Vec::with_capacity(self.active_streams.len());
        let open_status = self
            .active_streams
            .iter_mut()
            .map(|stream| {
                let Some(socket) = stream.socket.upgrade() else {
                    return false;
                };

                {
                    if stream.future.is_none() {
                        stream.future = Some(socket.clone().next());
                    }
                    match stream.future.as_mut().unwrap().poll_unpin(cx) {
                        Poll::Ready(Some(update)) => {
                            new_events.push((stream.handle.to_owned(), update));
                            stream.future = None;
                        }
                        Poll::Ready(None) => return false, // The stream got closed
                        Poll::Pending => {
                            // TODO: Work around, when pending that's usually,
                            // due to it waiting on new events from socket
                            // causing a block preventing others from borrowing.
                            // the socket.
                            stream.future = None;
                        }
                    }
                }
                {
                    if stream.silent_future.is_none() {
                        stream.silent_future = Some(socket.clone().background_next());
                    }
                    match stream.silent_future.as_mut().unwrap().poll_unpin(cx) {
                        Poll::Ready(Some(_)) => {
                            stream.silent_future = None;
                        }
                        Poll::Ready(None) => return false, // The stream got closed
                        Poll::Pending => {}
                    }
                }
                true
            })
            .collect::<Vec<_>>();
        let mut open_status_itr = open_status.iter();

        // Drop closed streams
        self.active_streams
            .retain(|_| *open_status_itr.next().unwrap());

        // In case any new events are pending
        if !new_events.is_empty() {
            self.ready_events.extend(
                new_events
                    .into_iter()
                    .filter(|(_, e)| *e != SocketEvent::Skip),
            );
            cx.waker().wake_by_ref();
        }

        Poll::Pending
    }
}
