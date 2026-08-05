//! Shared lifetime accounting for capability event streams.

use std::sync::atomic::{AtomicUsize, Ordering, fence};

/// RAII registration of one in-flight `ArcStream::next` future in
/// `active_streams`. Created as the first statement of `next()`, so its
/// drop — running before the future's own `self: Arc` is released — keeps
/// the invariant "`active_streams` never exceeds the number of `Arc`s held
/// by in-flight `next()` futures", which makes `owner_dropped` free of false
/// positives.
pub(crate) struct StreamPollGuard<'a>(&'a AtomicUsize);

impl<'a> StreamPollGuard<'a> {
    pub(crate) fn new(counter: &'a AtomicUsize) -> Self {
        // The future's self-Arc clone is sequenced before this on the same
        // thread; this fence pairs with `owner_dropped`'s fence.
        fence(Ordering::SeqCst);
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}

impl Drop for StreamPollGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
