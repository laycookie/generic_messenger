//! The ID primitive and the [`Identifier`] wrapper that pairs an ID with data.

use std::ops::{Deref, DerefMut};

/// Unique identifier type used throughout the messenger interface.
pub type ID = u64;

/// A type-safe identifier that pairs a unique ID with associated data.
///
/// This allows comparing identifiers by ID while maintaining type safety
/// and carrying additional data alongside the identifier.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct Identifier<D> {
    id: ID,
    data: D,
}
impl<D> Identifier<D> {
    /// Create a new identifier with the given ID and data.
    pub fn new(id: ID, data: D) -> Self {
        Self { id, data }
    }

    /// Get a reference to the unique ID.
    pub fn id(&self) -> &ID {
        &self.id
    }

    /// Create a new identifier with the same ID but different data type.
    ///
    /// This is useful for converting between identifier types while preserving
    /// the underlying ID.
    pub fn swap_data<T>(&self, new_data: T) -> Identifier<T> {
        Identifier {
            id: self.id,
            data: new_data,
        }
    }
}
impl<D> Deref for Identifier<D> {
    type Target = D;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}
impl<D> DerefMut for Identifier<D> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}
impl<D, E> PartialEq<Identifier<E>> for Identifier<D> {
    fn eq(&self, other: &Identifier<E>) -> bool {
        self.id == other.id
    }
}
