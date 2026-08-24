//! Owner-bound root and nested event streams.
//!
//! [`LeaseStream`] erases owner type and retains it only inside in-flight root
//! events. [`ArcStream`] and [`WeakSocketStream`] handle independently shared
//! nested sources. `None` ends either stream; owner closure cancels pending polls.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Weak},
    task::{Context, Poll},
};

use async_trait::async_trait;
use futures::{FutureExt as _, Stream};
use tracing::debug;

use crate::owner;

/// Nested event source polled through shared [`Arc`] ownership.
#[async_trait]
pub trait ArcStream: Send + Sync {
    type Item;
    /// Returns next item; `None` permanently terminates the bound stream.
    /// Shield mandatory work before consuming cancellation-sensitive input.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item>;
}

/// Owned future producing one event or permanent source termination.
pub type EventFuture<Event> = Pin<Box<dyn Future<Output = Option<Event>> + Send + 'static>>;

type ErasedLeasePoll<Event> = Box<dyn Fn() -> Option<EventFuture<Event>> + Send + Sync + 'static>;

/// Type-erased root stream tied to one logical [`owner::Owner`] lifetime.
/// Each poll temporarily retains owner allocation.
#[must_use = "streams do nothing unless polled"]
pub struct LeaseStream<Event> {
    poll: Option<ErasedLeasePoll<Event>>,
    owner_closed: Option<owner::Closed>,
    next_future: Option<EventFuture<Event>>,
}

impl<Event> LeaseStream<Event>
where
    Event: Send + 'static,
{
    /// Creates a stream storing owner's lease behind erased poll closure.
    pub fn new<T, Poll>(owner: &owner::Owner<T>, poll: Poll) -> Self
    where
        T: ?Sized + Send + Sync + 'static,
        Poll: Fn(owner::Retained<T>) -> EventFuture<Event> + Send + Sync + 'static,
    {
        let lease = owner.downgrade();
        let owner_closed = lease.closed();
        let poll = Box::new(move || {
            let retained = lease.retain()?;
            Some(poll(retained))
        }) as ErasedLeasePoll<Event>;

        Self {
            poll: Some(poll),
            owner_closed: Some(owner_closed),
            next_future: None,
        }
    }

    /// Runs setup while owner is open, then creates stream.
    /// Closure wins races and prevents setup polling when already closed.
    pub async fn open<T, Error, Setup, Poll>(
        owner: &owner::Owner<T>,
        setup: Setup,
        poll: Poll,
    ) -> Result<Self, Error>
    where
        T: ?Sized + Send + Sync + 'static,
        Setup: Future<Output = Result<(), Error>> + Send,
        Poll: Fn(owner::Retained<T>) -> EventFuture<Event> + Send + Sync + 'static,
    {
        let closed = owner.downgrade().closed().fuse();
        let setup = setup.fuse();
        futures::pin_mut!(closed, setup);

        futures::select_biased! {
            _ = closed => Ok(Self::terminated()),
            result = setup => result.map(|()| Self::new(owner, poll)),
        }
    }

    fn terminated() -> Self {
        Self {
            poll: None,
            owner_closed: None,
            next_future: None,
        }
    }

    fn terminate(&mut self) {
        self.poll = None;
        self.owner_closed = None;
        self.next_future = None;
    }
}

impl<Event> std::fmt::Debug for LeaseStream<Event> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LeaseStream")
            .field("terminated", &self.poll.is_none())
            .finish()
    }
}

impl<Event> Stream for LeaseStream<Event>
where
    Event: Send + 'static,
{
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.poll.is_none() {
            return Poll::Ready(None);
        }

        if self
            .owner_closed
            .as_mut()
            .is_some_and(|closed| closed.poll_unpin(cx).is_ready())
        {
            self.terminate();
            debug!("Stream owner closed");
            return Poll::Ready(None);
        }

        if self.next_future.is_none() {
            let Some(next) = self.poll.as_ref().and_then(|poll| poll()) else {
                self.terminate();
                return Poll::Ready(None);
            };
            self.next_future = Some(next);
        }

        let result = self
            .next_future
            .as_mut()
            .expect("next future was initialized above")
            .poll_unpin(cx);

        match result {
            Poll::Ready(Some(event)) => {
                self.next_future = None;
                if self
                    .owner_closed
                    .as_mut()
                    .is_some_and(|closed| closed.poll_unpin(cx).is_ready())
                {
                    self.terminate();
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(event))
                }
            }
            Poll::Ready(None) => {
                self.terminate();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<Event> futures::stream::FusedStream for LeaseStream<Event>
where
    Event: Send + 'static,
{
    fn is_terminated(&self) -> bool {
        self.poll.is_none()
    }
}

/// Weak nested source descriptor, made pollable through [`StreamScope`].
pub struct WeakStreamSource<Event> {
    socket: Weak<dyn ArcStream<Item = Event> + Send + Sync>,
}

impl<Event> WeakStreamSource<Event> {
    /// Create a source descriptor from an erased weak stream source.
    pub fn new(socket: Weak<dyn ArcStream<Item = Event> + Send + Sync>) -> Self {
        Self { socket }
    }

    /// Downgrades an [`ArcStream`]; caller retains its strong [`Arc`].
    pub fn from_arc<T>(arc: &Arc<T>) -> Self
    where
        T: ArcStream<Item = Event> + 'static,
    {
        let weak = Arc::downgrade(arc);
        let erased: Weak<dyn ArcStream<Item = Event> + Send + Sync> = weak;
        Self::new(erased)
    }
}

impl<Event> std::fmt::Debug for WeakStreamSource<Event> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeakStreamSource")
            .field("source_alive", &(self.socket.strong_count() > 0))
            .finish()
    }
}

/// Cloneable owner lifetime used to bind nested sources.
pub struct StreamScope<O: ?Sized> {
    owner: owner::Lease<O>,
}

impl<O: ?Sized> Clone for StreamScope<O> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
        }
    }
}

impl<O> StreamScope<O>
where
    O: ?Sized + Send + Sync + 'static,
{
    /// Create a stream scope from an owner-lifetime lease.
    pub fn new(owner: owner::Lease<O>) -> Self {
        Self { owner }
    }

    /// Returns whether the logical owner remains active.
    pub fn is_active(&self) -> bool {
        self.owner.is_active()
    }

    /// Bind a backend source descriptor to this logical lifetime.
    pub fn bind<Event>(&self, source: WeakStreamSource<Event>) -> WeakSocketStream<Event> {
        WeakSocketStream::bound(source, self.owner.closed())
    }
}

impl<O> From<owner::Lease<O>> for StreamScope<O>
where
    O: ?Sized + Send + Sync + 'static,
{
    fn from(owner: owner::Lease<O>) -> Self {
        Self::new(owner)
    }
}

impl<O> std::fmt::Debug for StreamScope<O>
where
    O: ?Sized + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamScope")
            .field("active", &self.is_active())
            .finish()
    }
}

/// Nested stream terminated by owner closure, source loss, or source `None`.
/// Owner closure cancels any in-flight source poll.
pub struct WeakSocketStream<Event> {
    source: Option<WeakStreamSource<Event>>,
    owner_closed: Option<owner::Closed>,
    next_future: Option<Pin<Box<dyn Future<Output = Option<Event>> + Send>>>,
}
impl<Event> WeakSocketStream<Event> {
    fn bound(source: WeakStreamSource<Event>, owner_closed: owner::Closed) -> Self {
        Self {
            source: Some(source),
            owner_closed: Some(owner_closed),
            next_future: None,
        }
    }

    fn terminate(&mut self) {
        self.source = None;
        self.owner_closed = None;
        self.next_future = None;
    }
}

impl<Event> std::fmt::Debug for WeakSocketStream<Event> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeakSocketStream")
            .field(
                "source_alive",
                &self
                    .source
                    .as_ref()
                    .is_some_and(|source| source.socket.strong_count() > 0),
            )
            .field("terminated", &self.source.is_none())
            .finish()
    }
}

impl<Event> Stream for WeakSocketStream<Event>
where
    Event: Send + 'static,
{
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.source.is_none() {
            return Poll::Ready(None);
        }

        if self
            .owner_closed
            .as_mut()
            .is_some_and(|closed| closed.poll_unpin(cx).is_ready())
        {
            self.terminate();
            debug!("Stream owner closed");
            return Poll::Ready(None);
        }

        if self.next_future.is_none() {
            let socket = match self
                .source
                .as_ref()
                .and_then(|source| source.socket.upgrade())
            {
                Some(socket) => socket,
                None => {
                    self.terminate();
                    debug!("Stream source dropped");
                    return Poll::Ready(None);
                }
            };
            self.next_future = Some(ArcStream::next(socket));
        }

        if let Some(ref mut fut) = self.next_future {
            match fut.as_mut().poll(cx) {
                Poll::Ready(Some(event)) => {
                    self.next_future = None;
                    // Let owner closure win a ready-source race.
                    if self
                        .owner_closed
                        .as_mut()
                        .is_some_and(|closed| closed.poll_unpin(cx).is_ready())
                    {
                        self.terminate();
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(event))
                    }
                }
                Poll::Ready(None) => {
                    self.terminate();
                    Poll::Ready(None)
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Pending
        }
    }
}

impl<Event> futures::stream::FusedStream for WeakSocketStream<Event>
where
    Event: Send + 'static,
{
    fn is_terminated(&self) -> bool {
        self.source.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt, join, stream::FusedStream};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct Ended;
    #[async_trait]
    impl ArcStream for Ended {
        type Item = u8;
        async fn next(self: Arc<Self>) -> Option<u8> {
            None
        }
    }

    struct EndsThenProduces {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ArcStream for EndsThenProduces {
        type Item = u8;

        async fn next(self: Arc<Self>) -> Option<u8> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => None,
                _ => Some(1),
            }
        }
    }

    struct PendingSource {
        active: Arc<AtomicUsize>,
    }

    fn poll_retained(_: owner::Retained<()>) -> EventFuture<u8> {
        Box::pin(async { Some(7) })
    }

    fn poll_retained_arc(_: owner::Retained<Arc<()>>) -> EventFuture<u8> {
        Box::pin(async { Some(7) })
    }

    fn poll_pending(retained: owner::Retained<()>) -> EventFuture<u8> {
        Box::pin(async move {
            futures::future::pending::<()>().await;
            let _ = retained.get();
            None
        })
    }

    struct ActivePoll(Arc<AtomicUsize>);

    impl Drop for ActivePoll {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ArcStream for PendingSource {
        type Item = u8;

        async fn next(self: Arc<Self>) -> Option<u8> {
            self.active.fetch_add(1, Ordering::SeqCst);
            let _active = ActivePoll(self.active.clone());
            futures::future::pending().await
        }
    }

    struct ClosesThenProduces {
        owner: Mutex<Option<owner::Owner<()>>>,
    }

    #[async_trait]
    impl ArcStream for ClosesThenProduces {
        type Item = u8;

        async fn next(self: Arc<Self>) -> Option<u8> {
            drop(self.owner.lock().unwrap().take());
            Some(1)
        }
    }

    /// Ended streams stay fused instead of re-polling completed futures.
    #[test]
    fn poll_after_end_is_safe() {
        futures::executor::block_on(async {
            let owner = owner::Owner::new(());
            let scope = StreamScope::new(owner.downgrade());
            let source = Arc::new(Ended);
            let mut stream = scope.bind(WeakStreamSource::from_arc(&source));
            assert_eq!(stream.next().await, None);
            assert_eq!(stream.next().await, None);
        });
    }

    #[test]
    fn lease_stream_does_not_keep_owner_value_alive() {
        futures::executor::block_on(async {
            let value = Arc::new(());
            let allocation = Arc::downgrade(&value);
            let owner = owner::Owner::new(value);
            let mut stream = LeaseStream::new(&owner, poll_retained_arc);

            assert_eq!(stream.next().await, Some(7));
            drop(owner);

            assert!(allocation.upgrade().is_none());
            assert_eq!(stream.next().await, None);
        });
    }

    #[test]
    fn owner_close_cancels_pending_lease_stream_poll() {
        futures::executor::block_on(async {
            let owner = owner::Owner::new(());
            let lifetime = owner.downgrade();
            let mut stream = LeaseStream::new(&owner, poll_pending);

            let (event, ()) = join!(stream.next(), async { owner.close() });

            assert_eq!(event, None);
            assert!(!lifetime.is_active());
            assert!(stream.is_terminated());
        });
    }

    #[test]
    fn closed_owner_prevents_lease_stream_setup() {
        futures::executor::block_on(async {
            let owner = owner::Owner::new(());
            let calls = Arc::new(AtomicUsize::new(0));
            owner.close();

            let setup_calls = calls.clone();
            let mut stream = LeaseStream::open(
                &owner,
                async move {
                    setup_calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), ()>(())
                },
                poll_retained,
            )
            .await
            .unwrap();

            assert_eq!(stream.next().await, None);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn source_cannot_resurrect_after_returning_none() {
        futures::executor::block_on(async {
            let owner = owner::Owner::new(());
            let source = Arc::new(EndsThenProduces {
                calls: AtomicUsize::new(0),
            });
            let scope = StreamScope::new(owner.downgrade());
            let mut stream = scope.bind(WeakStreamSource::from_arc(&source));

            assert_eq!(stream.next().await, None);
            assert!(stream.is_terminated());
            assert_eq!(stream.next().await, None);
            assert_eq!(stream.next().await, None);
            assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn ends_when_source_dropped() {
        futures::executor::block_on(async {
            let owner = owner::Owner::new(());
            let scope = StreamScope::new(owner.downgrade());
            let source = Arc::new(Ended);
            let mut stream = scope.bind(WeakStreamSource::from_arc(&source));
            drop(source);
            assert_eq!(stream.next().await, None);
        });
    }

    #[test]
    fn owner_drop_cancels_every_pending_next() {
        futures::executor::block_on(async {
            let owner = owner::Owner::new(());
            let active = Arc::new(AtomicUsize::new(0));
            let source = Arc::new(PendingSource {
                active: active.clone(),
            });
            let scope = StreamScope::new(owner.downgrade());
            let mut first = scope.bind(WeakStreamSource::from_arc(&source));
            let mut second = scope.bind(WeakStreamSource::from_arc(&source));

            let (first, second, ()) =
                join!(first.next(), second.next(), async move { drop(owner) });

            assert_eq!(first, None);
            assert_eq!(second, None);
            assert_eq!(active.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn closed_owner_prevents_source_poll() {
        futures::executor::block_on(async {
            let owner = owner::Owner::new(());
            let calls = Arc::new(AtomicUsize::new(0));
            let source = Arc::new(PendingSource {
                active: calls.clone(),
            });
            let scope = StreamScope::new(owner.downgrade());
            let mut stream = scope.bind(WeakStreamSource::from_arc(&source));

            drop(owner);

            assert_eq!(stream.next().await, None);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn owner_close_wins_race_with_ready_event() {
        futures::executor::block_on(async {
            let owner = owner::Owner::new(());
            let lifetime = owner.downgrade();
            let source = Arc::new(ClosesThenProduces {
                owner: Mutex::new(Some(owner)),
            });
            let scope = StreamScope::new(lifetime);
            let mut stream = scope.bind(WeakStreamSource::from_arc(&source));

            assert_eq!(stream.next().await, None);
            assert!(stream.is_terminated());
        });
    }

    #[test]
    fn owner_close_cancels_pending_lease_stream_setup() {
        futures::executor::block_on(async {
            let owner = owner::Owner::new(());
            let active = Arc::new(AtomicUsize::new(0));
            let listener_active = active.clone();
            let listener = async move {
                listener_active.fetch_add(1, Ordering::SeqCst);
                let _active = ActivePoll(listener_active);
                futures::future::pending::<Result<(), ()>>().await
            };

            let (stream, ()) = join!(LeaseStream::open(&owner, listener, poll_retained), async {
                owner.close()
            },);
            let mut stream = stream.unwrap();

            assert_eq!(stream.next().await, None);
            assert_eq!(active.load(Ordering::SeqCst), 0);
        });
    }
}
