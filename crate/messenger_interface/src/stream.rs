//! Traits and types for event sources compatible with [`WeakSocketStream`].
//!
//! Implement [`ArcStream`] for your type to use it with [`WeakSocketStream`].

use std::{
    pin::Pin,
    sync::{Arc, Weak},
    task::{Context, Poll},
};

use async_trait::async_trait;
use futures::Stream;
use tracing::info;

/// Trait for event sources that can be polled via `Arc` (shared ownership).
///
/// Implement this for your type to use it with [`WeakSocketStream`].
/// The `next(Arc<Self>)` method works with shared ownership, unlike [`Stream`] which requires `Pin<&mut Self>`.
#[async_trait]
pub trait ArcStream: Send + Sync {
    type Item;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item>;
}

/// A stream adapter that wraps a Weak reference to an [`ArcStream`] and
/// automatically stops when the underlying Arc is dropped.
///
/// Yields `Event` items directly. Implements [`Stream`] for use with `StreamExt`, iced, etc.
pub struct WeakSocketStream<Event> {
    socket: Weak<dyn ArcStream<Item = Event> + Send + Sync>,
    next_future: Option<Pin<Box<dyn std::future::Future<Output = Option<Event>> + Send>>>,
}
impl<Event> WeakSocketStream<Event> {
    /// Create a stream from a weak reference to an [`ArcStream`].
    pub fn new(socket: Weak<dyn ArcStream<Item = Event> + Send + Sync>) -> Self {
        Self {
            socket,
            next_future: None,
        }
    }

    /// Create a stream from an [`Arc`] to an [`ArcStream`] implementor.
    pub fn from_arc<T>(arc: Arc<T>) -> Self
    where
        T: ArcStream<Item = Event> + 'static,
    {
        let erased: Arc<dyn ArcStream<Item = Event> + Send + Sync> = arc;
        Self::new(Arc::downgrade(&erased))
    }
}
impl<Event> Stream for WeakSocketStream<Event>
where
    Event: Send + 'static,
{
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let socket_arc = match self.socket.upgrade() {
            Some(arc) => arc,
            None => {
                info!("Killed");
                return Poll::Ready(None);
            }
        };

        if self.next_future.is_none() {
            let socket_clone = socket_arc.clone();
            self.next_future = Some(Box::pin(async move { ArcStream::next(socket_clone).await }));
        }

        if let Some(ref mut fut) = self.next_future {
            match fut.as_mut().poll(cx) {
                Poll::Ready(Some(event)) => {
                    self.next_future = None;
                    Poll::Ready(Some(event))
                }
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Pending
        }
    }
}
