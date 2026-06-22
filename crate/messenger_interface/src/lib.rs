// Full `specialization` (not `min_specialization`) is required: the caster
// impls in `interface.rs` specialize blanket impls by adding trait bounds
// (`T: Messenger` → `T: Messenger + Query`), which `min_specialization`
// rejects. The known soundness holes involve lifetime-dependent
// specialization, which these impls don't do.
#![feature(specialization)]
#![allow(incomplete_features)]

pub mod interface;
pub mod stream;
pub mod types;
