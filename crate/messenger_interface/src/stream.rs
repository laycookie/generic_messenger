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
use tracing::debug;

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
/// # Termination contract
///
/// While a `next()` future is in flight it holds a strong `Arc` to the
/// [`ArcStream`], so dropping every external `Arc` does *not* by itself
/// terminate the stream — the weak upgrade keeps succeeding until the
/// in-flight future completes. Implementors whose `next()` can pend
/// indefinitely (sockets, queues) must therefore provide cooperative
/// termination: detect that the owner is gone (e.g. count in-flight
/// `next()` futures and compare against `Arc::strong_count`, as the
/// discord backend does) and return `None` from `next()`. Once `next()`
/// resolves, the next poll re-upgrades the weak and ends the stream.
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

impl<Event> std::fmt::Debug for WeakSocketStream<Event> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeakSocketStream")
            .field("alive", &(self.socket.strong_count() > 0))
            .finish()
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
                debug!("Killed");
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
                Poll::Ready(None) => {
                    // Clear the completed future so a poll after the end
                    // (legal for non-fused consumers) doesn't re-poll a
                    // finished future and panic.
                    self.next_future = None;
                    Poll::Ready(None)
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    struct Ended;
    #[async_trait]
    impl ArcStream for Ended {
        type Item = u8;
        async fn next(self: Arc<Self>) -> Option<u8> {
            None
        }
    }

    /// Polling past the end must yield `None` again instead of re-polling
    /// the completed inner future (which would panic).
    #[test]
    fn poll_after_end_is_safe() {
        futures::executor::block_on(async {
            let source = Arc::new(Ended);
            let mut stream = WeakSocketStream::from_arc(source);
            assert_eq!(stream.next().await, None);
            assert_eq!(stream.next().await, None);
        });
    }

    #[test]
    fn ends_when_source_dropped() {
        futures::executor::block_on(async {
            let source = Arc::new(Ended);
            let mut stream = WeakSocketStream::from_arc(source.clone());
            drop(source);
            assert_eq!(stream.next().await, None);
        });
    }
}
