//! The [`User`] entity.

use std::path::PathBuf;

/// Represents a user in the messenger system.
#[derive(Debug, Clone)]
pub struct User {
    /// Display name of the user.
    pub name: String,
    /// Optional path to the user's avatar/icon image.
    pub icon: Option<PathBuf>,
}
