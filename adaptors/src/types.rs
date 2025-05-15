use std::{
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
    thread,
    time::Duration,
};

use futures::{
    FutureExt, Stream, StreamExt,
    channel::mpsc::{self, Receiver, Sender},
    stream,
};
use uuid::Uuid;

// Legacy
#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub username: String,
}

// New
#[derive(Debug, Clone)]
pub struct Store {
    pub origin_uuid: Uuid,
    pub(crate) hash: Option<String>, // Used in cases where ID can change
    pub(crate) id: String,           // ID of a location
    pub name: String,
    pub icon: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub(crate) id: String,
    pub sender: Store,
    pub text: String,
}

// === Socket ===
// #[derive(Debug)]
// pub struct SocketConnection {
//     connection: Option<Receiver<usize>>,
//     count: usize,
// }
//
// #[derive(Debug)]
// pub enum SocketEvent {
//     Connect(Sender<usize>),
//     Echo(usize),
// }
//
// impl SocketConnection {
//     pub fn connect() -> impl Stream<Item = SocketEvent> {
//         let s = stream::unfold(0, |state| async move { Some((1, 1)) });
//
//         SocketConnection {
//             connection: None,
//             count: 0,
//         }
//     }
// }
//
// impl Stream for SocketConnection {
//     type Item = SocketEvent;
//
//     fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
//         let Some(rec) = &mut self.connection else {
//             let (sender, receiver) = mpsc::channel(128);
//             self.connection = Some(receiver);
//             return Poll::Ready(Some(SocketEvent::Connect(sender)));
//         };
//
//         println!("Run");
//
//         match rec.select_next_some().poll_unpin(cx) {
//             Poll::Ready(v) => Poll::Ready(Some(SocketEvent::Echo(v))),
//             Poll::Pending => {
//                 println!("Pending");
//                 cx.waker();
//                 Poll::Pending
//             }
//         }
//     }
// }
