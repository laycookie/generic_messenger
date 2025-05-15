use std::sync::Arc;

use adaptors::Messanger as Auth;
use futures::{
    channel::mpsc::{self, Sender},
    SinkExt, Stream, StreamExt,
};

pub enum ReciverEvent {
    Connection(Arc<dyn Auth>),
}

#[derive(Debug)]
pub enum SocketEvent {
    Connect(Sender<ReciverEvent>),
    Echo(usize),
}

#[derive(Debug)]
pub struct SocketConnection;

impl SocketConnection {
    pub fn connect() -> impl Stream<Item = SocketEvent> {
        iced::stream::channel(128, |mut output| async move {
            let (sender, mut receiver) = mpsc::channel::<ReciverEvent>(128);
            output.send(SocketEvent::Connect(sender)).await.unwrap();

            loop {
                futures::select! {
                    message = receiver.select_next_some() => {
                        match message {
                            ReciverEvent::Connection(auth) => {
                                let socket = auth.socket().unwrap();
                                let stream = socket.get_stream().await;
                                loop {
                                    output.send(SocketEvent::Echo(stream.next().await)).await.unwrap();
                                }
                            },
                        };
                    }
                }
            }
        })
    }
}
