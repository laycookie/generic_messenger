// Casters add trait bounds, requiring full specialization; they do not
// specialize lifetimes. Arbitrary receivers carry `Retained<Self>` polls.
#![feature(arbitrary_self_types, specialization)]
#![allow(incomplete_features)]

pub mod auth;
pub mod interface;
pub mod owner;
pub mod stream;
pub mod types;
