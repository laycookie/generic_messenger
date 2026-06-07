//! Lock-free bounded ring buffer with overwrite-on-push.
//!
//! Designed for dedup rings, tombstones, and recency-window use cases:
//!
//! - `push` always succeeds and overwrites the oldest slot when full,
//!   driven by modulo arithmetic on an atomic head index — no conditional
//!   eviction branch.
//! - `contains` scans every slot atomically without consuming entries,
//!   so multiple concurrent readers are fine.
//!
//! For SPSC streaming hand-off (producer pushes, consumer drains), use
//! the `ringbuf` crate instead — that's a different pattern.

use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_utils::atomic::AtomicCell;

/// A bounded ring buffer with overwrite-on-push and non-consuming search.
///
/// The implementation is genuinely lock-free for types that fit in a
/// hardware atomic word (u8/u16/u32/u64/usize/pointers). Larger types
/// fall back to a global `SeqLock` inside `AtomicCell` — still correct,
/// just no longer lock-free.
///
/// Slot value `T::default()` marks "never written," so `contains(default)`
/// will return `true` once the ring has any unfilled slot. Choose `T` so
/// that no legitimate entry equals the default (e.g. for `u64`, the
/// default `0` is fine unless `0` is a valid ID in your domain).
pub struct Ring<T, const CAP: usize>
where
    T: Copy + Eq + Send + Default + 'static,
{
    slots: [AtomicCell<T>; CAP],
    head: AtomicUsize,
}

impl<T, const CAP: usize> Ring<T, CAP>
where
    T: Copy + Eq + Send + Default + 'static,
{
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicCell::new(T::default())),
            head: AtomicUsize::new(0),
        }
    }

    /// Push `item`, overwriting the oldest slot when the ring is full.
    pub fn push(&self, item: T) {
        let idx = self.head.fetch_add(1, Ordering::Relaxed) % CAP;
        self.slots[idx].store(item);
    }

    /// Returns `true` if any slot currently holds `item`.
    pub fn contains(&self, item: T) -> bool {
        self.slots.iter().any(|slot| slot.load() == item)
    }
}

impl<T, const CAP: usize> Default for Ring<T, CAP>
where
    T: Copy + Eq + Send + Default + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_contains_within_cap() {
        let ring: Ring<u64, 4> = Ring::new();
        ring.push(10);
        ring.push(20);
        ring.push(30);
        assert!(ring.contains(10));
        assert!(ring.contains(20));
        assert!(ring.contains(30));
        assert!(!ring.contains(40));
    }

    #[test]
    fn overwrites_oldest_on_overflow() {
        let ring: Ring<u64, 3> = Ring::new();
        ring.push(1);
        ring.push(2);
        ring.push(3);
        ring.push(4); // evicts 1
        ring.push(5); // evicts 2
        assert!(!ring.contains(1));
        assert!(!ring.contains(2));
        assert!(ring.contains(3));
        assert!(ring.contains(4));
        assert!(ring.contains(5));
    }

    #[test]
    fn default_value_appears_in_unfilled_ring() {
        // Documented behavior: default() marks unwritten slots.
        let ring: Ring<u64, 4> = Ring::new();
        assert!(ring.contains(0));
        ring.push(7);
        assert!(ring.contains(0)); // three slots still default
        ring.push(8);
        ring.push(9);
        ring.push(10);
        assert!(!ring.contains(0)); // all slots written now
    }
}
