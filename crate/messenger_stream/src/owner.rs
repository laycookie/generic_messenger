//! Unique ownership with observable logical closure.
//!
//! [`Owner`] owns allocation, [`Lease`] observes weakly, and [`Retained`] is a
//! non-cloneable strong guard usable as owning stream receiver. Closure blocks
//! new access but never revokes an existing guard.

use std::{
    future::Future,
    marker::Unsize,
    ops::{CoerceUnsized, Deref, DispatchFromDyn},
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use futures::{
    FutureExt as _,
    channel::oneshot,
    future::{FusedFuture, Shared},
};

/// Unique owner of a value and its closure signal.
///
/// Coerces to unsized owners without reallocating.
#[must_use = "dropping the owner closes the value"]
pub struct Owner<T: ?Sized> {
    inner: Arc<Inner<T>>,
}

/// Cloneable weak access tied to an [`Owner`].
pub struct Lease<T: ?Sized> {
    inner: Weak<Inner<T>>,
}

/// Non-cloneable strong access acquired from a [`Lease`].
/// Unsizes between trait objects without reallocating.
#[must_use = "dropping retained access may release the owned value"]
pub struct Retained<T: ?Sized> {
    inner: Arc<Inner<T>>,
}

/// Future completing when an [`Owner`] is explicitly closed or dropped.
#[must_use = "futures do nothing unless polled or awaited"]
pub struct Closed {
    receiver: Option<Shared<oneshot::Receiver<()>>>,
}

struct Inner<T: ?Sized> {
    signal: CloseSignal,
    value: T,
}

impl<T, U> CoerceUnsized<Owner<U>> for Owner<T>
where
    T: ?Sized + Unsize<U>,
    U: ?Sized,
{
}

impl<T, U> CoerceUnsized<Retained<U>> for Retained<T>
where
    T: ?Sized + Unsize<U>,
    U: ?Sized,
{
}

impl<T, U> DispatchFromDyn<Retained<U>> for Retained<T>
where
    T: ?Sized + Unsize<U>,
    U: ?Sized,
{
}

struct CloseSignal {
    closed: AtomicBool,
    sender: Mutex<Option<oneshot::Sender<()>>>,
    receiver: Shared<oneshot::Receiver<()>>,
}

impl Default for CloseSignal {
    fn default() -> Self {
        let (sender, receiver) = oneshot::channel();
        Self {
            closed: AtomicBool::new(false),
            sender: Mutex::new(Some(sender)),
            receiver: receiver.shared(),
        }
    }
}

fn discard_panic_payload(payload: Box<dyn std::any::Any + Send>) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(payload);
    }
}

impl CloseSignal {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }

        let sender = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();

        // Wake outside lock to allow reentrant wakers.
        if let Some(sender) = sender
            && let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
                let _ = sender.send(());
            }))
        {
            // Never unwind through `Owner::drop`.
            discard_panic_payload(payload);
        }
    }

    fn subscribe(&self) -> Closed {
        if self.is_closed() {
            Closed::terminated()
        } else {
            // `Shared` preserves closure racing this clone.
            Closed::waiting(self.receiver.clone())
        }
    }
}

impl<T> Owner<T> {
    /// Creates a new owner.
    pub fn new(value: T) -> Self {
        Self {
            inner: Arc::new(Inner {
                signal: CloseSignal::default(),
                value,
            }),
        }
    }
}

impl<T: ?Sized> Owner<T> {
    /// Idempotently closes logical lifetime without dropping value.
    /// Existing [`Owner`] and [`Retained`] access remains valid.
    pub fn close(&self) {
        self.inner.signal.close();
    }

    /// Returns a lease tied to this owner's lifetime.
    pub fn downgrade(&self) -> Lease<T> {
        Lease {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Returns the owned value by shared reference.
    pub fn get(&self) -> &T {
        &self.inner.value
    }
}

impl<T: ?Sized> Drop for Owner<T> {
    fn drop(&mut self) {
        self.close();
    }
}

impl<T: ?Sized> Deref for Owner<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.get()
    }
}

impl<T: ?Sized> Retained<T> {
    /// Returns retained value by shared reference.
    pub fn get(&self) -> &T {
        &self.inner.value
    }

    /// Reports whether logical lifetime remains open for new work.
    pub fn is_active(&self) -> bool {
        !self.inner.signal.is_closed()
    }
}

impl<T: ?Sized> Deref for Retained<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.get()
    }
}

impl<T: ?Sized> Clone for Lease<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: ?Sized> Lease<T> {
    /// Acquires strong access when owner is open.
    /// A close race may succeed; acquired access is never revoked.
    pub fn retain(&self) -> Option<Retained<T>> {
        let inner = self.inner.upgrade()?;
        (!inner.signal.is_closed()).then_some(Retained { inner })
    }

    /// Reports whether new access can currently be retained.
    pub fn is_active(&self) -> bool {
        self.retain().is_some()
    }

    /// Runs an operation with strong access when owner is open.
    pub fn with<R>(&self, operation: impl FnOnce(&T) -> R) -> Option<R> {
        let retained = self.retain()?;
        Some(operation(retained.get()))
    }

    /// Runs an async operation with strong access held through completion.
    pub async fn with_async<F, R>(&self, operation: F) -> Option<R>
    where
        F: for<'a> AsyncFnOnce(&'a T) -> R,
    {
        let retained = self.retain()?;
        Some(operation(retained.get()).await)
    }

    /// Returns a future completing once the owner is explicitly closed or dropped.
    pub fn closed(&self) -> Closed {
        let Some(inner) = self.inner.upgrade() else {
            return Closed::terminated();
        };
        inner.signal.subscribe()
    }
}

impl Future for Closed {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let Some(receiver) = this.receiver.as_mut() else {
            return Poll::Ready(());
        };

        match Pin::new(receiver).poll(cx) {
            Poll::Ready(_) => {
                this.receiver = None;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl FusedFuture for Closed {
    fn is_terminated(&self) -> bool {
        self.receiver.is_none()
    }
}

impl Closed {
    fn terminated() -> Self {
        Self { receiver: None }
    }

    fn waiting(receiver: Shared<oneshot::Receiver<()>>) -> Self {
        Self {
            receiver: Some(receiver),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{executor::block_on, future::FusedFuture, join};
    use std::{
        sync::atomic::AtomicUsize,
        task::{Wake, Waker},
        thread,
    };

    struct ReentrantWake {
        lease: Lease<u32>,
        calls: AtomicUsize,
        observed_inactive: AtomicBool,
    }

    impl Wake for ReentrantWake {
        fn wake(self: Arc<Self>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.observed_inactive
                .store(!self.lease.is_active(), Ordering::SeqCst);
            block_on(self.lease.closed());
        }
    }

    #[test]
    fn owner_types_have_expected_positive_auto_traits() {
        fn assert_send<T: Send>() {}
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<Owner<()>>();
        assert_send_sync::<Lease<()>>();
        assert_send_sync::<Retained<()>>();
        assert_send::<Closed>();
    }

    #[test]
    fn lease_does_not_keep_value_alive() {
        let value = Arc::new(());
        let allocation = Arc::downgrade(&value);
        let owner = Owner::new(value);
        let lease = owner.downgrade();

        assert!(lease.is_active());
        drop(owner);

        assert!(!lease.is_active());
        assert!(allocation.upgrade().is_none());
    }

    #[test]
    fn retained_unsizes_without_reallocating() {
        let owner = Owner::new(7_u32);
        let retained = owner.downgrade().retain().expect("owner must be active");
        let allocation = Arc::as_ptr(&retained.inner) as *const ();
        let retained: Retained<dyn std::fmt::Display> = retained;

        assert_eq!(Arc::as_ptr(&retained.inner) as *const (), allocation);
        assert_eq!(retained.to_string(), "7");
    }

    #[test]
    fn retained_access_outlives_owner_but_releases_value() {
        let value = Arc::new(());
        let allocation = Arc::downgrade(&value);
        let owner = Owner::new(value);
        let lease = owner.downgrade();
        let retained = lease.retain().expect("owner must be active");

        drop(owner);

        assert!(!lease.is_active());
        assert!(!retained.is_active());
        assert!(allocation.upgrade().is_some());
        assert_eq!(Arc::strong_count(retained.get()), 1);

        drop(retained);
        assert!(allocation.upgrade().is_none());
    }

    #[test]
    fn callbacks_provide_temporary_access() {
        block_on(async {
            let owner = Owner::new(7);
            let lease = owner.downgrade();

            assert_eq!(lease.with(|value| *value), Some(7));
            assert_eq!(lease.with_async(async |value| *value).await, Some(7));

            owner.close();

            assert_eq!(lease.with(|value| *value), None);
            assert_eq!(lease.with_async(async |value| *value).await, None);
            assert_eq!(*owner, 7);
        });
    }

    #[test]
    fn async_callback_keeps_value_alive_until_completion() {
        block_on(async {
            let value = Arc::new(());
            let allocation = Arc::downgrade(&value);
            let owner = Owner::new(value);
            let lease = owner.downgrade();
            let (entered, started) = oneshot::channel();
            let (release, released) = oneshot::channel();

            let access = lease.with_async(async move |value| {
                entered.send(()).expect("owner task must still be waiting");
                released.await.expect("owner task must release access");
                Arc::strong_count(value)
            });
            let live_allocation = allocation.clone();
            let retire = async move {
                started.await.expect("access must start");
                drop(owner);
                assert!(live_allocation.upgrade().is_some());
                release.send(()).expect("access task must still be waiting");
            };

            let (strong_count, ()) = join!(access, retire);

            assert_eq!(strong_count, Some(1));
            assert!(allocation.upgrade().is_none());
        });
    }

    #[test]
    fn owner_unsizes_without_reallocating() {
        let owner = Owner::new(7_u32);
        let allocation = Arc::as_ptr(&owner.inner) as *const ();
        let owner: Owner<dyn std::fmt::Display> = owner;

        assert_eq!(Arc::as_ptr(&owner.inner) as *const (), allocation);
        assert_eq!(owner.to_string(), "7");
    }

    #[test]
    fn owner_drop_wakes_every_closed_future() {
        block_on(async {
            let owner = Owner::new(7);
            let first = owner.downgrade().closed();
            let second = owner.downgrade().closed();

            let waiters = async { join!(first, second) };
            let close = async { drop(owner) };
            let (_, ()) = join!(waiters, close);
        });
    }

    #[test]
    fn registered_closed_can_be_closed_from_another_thread() {
        let owner = Owner::new(7);
        let lease = owner.downgrade();
        let mut closed = Box::pin(lease.closed());
        let waker = futures::task::noop_waker();
        let mut context = Context::from_waker(&waker);

        assert!(closed.as_mut().poll(&mut context).is_pending());
        let owner = thread::spawn(move || {
            owner.close();
            owner
        })
        .join()
        .expect("close thread must not panic");

        assert!(closed.as_mut().poll(&mut context).is_ready());
        assert!(!lease.is_active());
        assert_eq!(*owner, 7);
    }

    #[test]
    fn close_allows_reentrant_waker() {
        let owner = Owner::new(7_u32);
        let lease = owner.downgrade();
        let state = Arc::new(ReentrantWake {
            lease: lease.clone(),
            calls: AtomicUsize::new(0),
            observed_inactive: AtomicBool::new(false),
        });
        let waker = Waker::from(state.clone());
        let mut context = Context::from_waker(&waker);
        let mut closed = Box::pin(lease.closed());

        assert!(closed.as_mut().poll(&mut context).is_pending());
        owner.close();

        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        assert!(state.observed_inactive.load(Ordering::SeqCst));
        assert!(closed.as_mut().poll(&mut context).is_ready());
    }

    #[test]
    fn closed_is_fused_after_owner_drop() {
        let owner = Owner::new(7);
        let lease = owner.downgrade();
        drop(owner);

        let mut closed = Box::pin(lease.closed());
        assert!(closed.is_terminated());
        block_on(closed.as_mut());
        assert!(closed.is_terminated());
        block_on(closed.as_mut());
    }

    #[test]
    fn cancelled_closed_future_does_not_block_retirement() {
        let owner = Owner::new(7);
        let lease = owner.downgrade();

        block_on(async {
            let mut cancelled = Box::pin(lease.closed());
            futures::future::poll_fn(|cx| {
                assert!(cancelled.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
        });

        drop(owner);
        block_on(lease.closed());
    }

    #[test]
    fn registered_closed_does_not_keep_inner_alive() {
        let owner = Owner::new(7);
        let allocation = Arc::downgrade(&owner.inner);
        let mut closed = Box::pin(owner.downgrade().closed());
        let waker = futures::task::noop_waker();
        let mut context = Context::from_waker(&waker);

        assert!(closed.as_mut().poll(&mut context).is_pending());
        drop(owner);

        assert!(allocation.upgrade().is_none());
        drop(closed);
    }

    #[test]
    fn closed_future_does_not_lease_value() {
        let value = Arc::new(());
        let allocation = Arc::downgrade(&value);
        let owner = Owner::new(value);
        let closed = owner.downgrade().closed();

        drop(owner);

        assert!(allocation.upgrade().is_none());
        block_on(closed);
    }
}
