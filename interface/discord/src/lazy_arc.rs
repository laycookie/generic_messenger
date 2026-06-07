use std::{ops::Deref, sync::Arc};

use arc_swap::ArcSwapOption;
use futures::lock::Mutex as AsyncMutex;

/// `ArcSwapOption<T>` paired with an async init lock. Reads stay lock-free
/// via `Deref` to the inner swap; `get_or_try_init` serializes construction
/// so concurrent callers don't each build a `T` and race the final `store`.
pub(crate) struct LazyArc<T> {
    value: ArcSwapOption<T>,
    init: AsyncMutex<()>,
}

impl<T> Default for LazyArc<T> {
    fn default() -> Self {
        Self {
            value: ArcSwapOption::empty(),
            init: AsyncMutex::new(()),
        }
    }
}

impl<T> Deref for LazyArc<T> {
    type Target = ArcSwapOption<T>;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> LazyArc<T> {
    pub async fn get_or_try_init<F, E>(&self, init: F) -> Result<Arc<T>, E>
    where
        F: AsyncFnOnce() -> Result<T, E>,
    {
        if let Some(v) = self.value.load_full() {
            return Ok(v);
        }
        let _guard = self.init.lock().await;
        if let Some(v) = self.value.load_full() {
            return Ok(v);
        }
        let v = Arc::new(init().await?);
        self.value.store(Some(v.clone()));
        Ok(v)
    }
}
