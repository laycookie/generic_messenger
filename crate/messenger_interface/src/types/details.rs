//! The [`Details`] sheet: backend-specific metadata for inspection UIs.

/// An ordered list of human-readable `(label, value)` pairs describing an
/// entity (a user, a house, ...). Backends fill in whatever they know about
/// the entity; the UI renders the pairs as-is and stays platform-agnostic.
#[derive(Debug, Clone, Default)]
pub struct Details {
    pub fields: Vec<(String, String)>,
}

impl Details {
    pub fn push(&mut self, label: impl Into<String>, value: impl Into<String>) {
        self.fields.push((label.into(), value.into()));
    }

    /// Push only when a value is present.
    pub fn push_opt(&mut self, label: impl Into<String>, value: Option<impl Into<String>>) {
        if let Some(value) = value {
            self.push(label, value);
        }
    }
}
