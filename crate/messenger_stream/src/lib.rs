//! Generic owner-lifetime and owner-bound stream primitives.

// Owner and retained guards preserve allocation while coercing or dynamically
// dispatching from concrete values into trait objects.
#![feature(coerce_unsized, dispatch_from_dyn, unsize)]

pub mod owner;
pub mod stream;
