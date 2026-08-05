//! Zero-sized marker support.

pub use credential_macro::unit_struct;

/// Marks a type as safe to use where a zero-sized marker is required.
///
/// # Safety
/// Implementors must have size zero.
pub unsafe trait UnitStruct {}
