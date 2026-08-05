//! Structured authentication credentials shared across messenger backends.
//!
//! This module defines *only* the interface through which the UI collects login
//! input and hands it to a backend — it carries no knowledge of how a credential
//! is written to disk (that is the `login_store` crate's job), nor of what the
//! individual fields *mean* (that is the backend's).
//!
//! A field is deliberately opaque: just a [`CredentialField`] `{ key, value }`
//! pair. The `key` is a backend-chosen identifier (used as the storage key and
//! to match values back to the right field); the `value` is the secret the user
//! supplied. There is no fixed `Username/Password/Token/...` vocabulary here — a
//! backend names its own fields.
//!
//! Backends declare their login shapes with `#[derive(CredentialSchema)]` on a
//! credential enum: the derive generates [`AuthMethod`]s (so the UI renders a
//! form) and a `TryFrom<Credential>` (so the backend consumes a typed,
//! exhaustive value rather than a loose list). The persisted subset is returned
//! from [`Messenger::auth`](crate::interface::Messenger::auth) as a
//! [`Credential`].
//!
//! Backends may also expose [`InteractiveAuthMethod`]s. Starting one returns an
//! [`InteractiveAuthSession`] containing a UI prompt and a future that resolves
//! to the same backend-neutral [`Credential`] type.

use std::{error::Error, fmt, future::Future, pin::Pin};

use secure_string::SecureString;

pub use credential_macro::CredentialSchema;

/// Error returned while starting or completing interactive authentication.
pub type AuthError = Box<dyn Error + Send + Sync>;

/// Owned, sendable authentication future stored behind method descriptors.
pub type AuthFuture<T> = Pin<Box<dyn Future<Output = Result<T, AuthError>> + Send>>;

/// One collected credential field: a schema-declared `key` (one of the backend's
/// compile-time field keys) and the value the user supplied. The value is always
/// held as a [`SecureString`]; whether it is masked on input is a UI concern
/// carried by [`CredentialFieldSpec::secret`], not by this type.
#[derive(Clone, Debug)]
pub struct CredentialField {
    pub key: &'static str,
    pub value: SecureString,
}

impl CredentialField {
    pub fn new(key: &'static str, value: impl Into<SecureString>) -> CredentialField {
        CredentialField {
            key,
            value: value.into(),
        }
    }
}

/// The set of fields collected for one login (or persisted for one account) —
/// the erased value that flows UI → backend and to/from storage. Backends parse
/// it into a typed credential via the generated `TryFrom`.
#[derive(Clone, Debug, Default)]
pub struct Credential(pub Vec<CredentialField>);

/// Prompt metadata for one field the UI should render.
#[derive(Clone, Copy, Debug)]
pub struct CredentialFieldSpec {
    /// The [`CredentialField::key`] this prompt produces.
    pub key: &'static str,
    /// Placeholder/label shown to the user (e.g. "Email or phone").
    pub label: &'static str,
    /// Mask the input (passwords).
    pub secret: bool,
    /// The field may be left blank (e.g. an optional one-time code).
    pub optional: bool,
}

/// A login method a backend exposes, e.g. "Token" or "Username & password". The
/// form renders [`fields`](AuthMethod::fields) and, on submit, collects them into
/// a [`Credential`]. A [`hidden`](AuthMethod::hidden) method is still a valid,
/// parseable shape (e.g. a session restored from storage) but is not rendered.
#[derive(Clone, Copy, Debug)]
pub struct AuthMethod {
    pub label: &'static str,
    /// Omit this method from the UI; it remains a valid shape for parsing a
    /// stored credential (its fields still contribute their keys).
    pub hidden: bool,
    pub fields: &'static [CredentialFieldSpec],
}

/// Runtime-generated text encoded into a QR code.
///
/// Payload remains opaque to the UI and deliberately omits `Display`; QR login
/// URLs can authorize a session and must not be written to logs accidentally.
#[derive(Clone, Eq, PartialEq)]
pub struct QrPayload(Box<str>);

impl QrPayload {
    #[must_use]
    pub fn new(payload: impl Into<Box<str>>) -> Self {
        Self(payload.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for QrPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QrPayload([redacted])")
    }
}

/// UI presentation produced after an interactive authentication method starts.
#[derive(Debug)]
pub enum InteractiveAuthPrompt {
    QrCode { payload: QrPayload },
}

/// Live interactive authentication session. Dropping `finish` cancels backend
/// work owned by its future.
pub struct InteractiveAuthSession {
    pub prompt: InteractiveAuthPrompt,
    pub finish: AuthFuture<Credential>,
}

/// Backend-provided interactive authentication method.
#[derive(Clone, Copy)]
pub struct InteractiveAuthMethod {
    pub label: &'static str,
    pub start: fn() -> AuthFuture<InteractiveAuthSession>,
}

/// Error from parsing a [`Credential`] into a backend's typed schema.
#[derive(Debug)]
pub enum CredentialError {
    /// The supplied fields matched none of the backend's declared shapes.
    Unrecognized,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialError::Unrecognized => {
                f.write_str("no known credential shape matched the supplied fields")
            }
        }
    }
}

impl Error for CredentialError {}

#[cfg(test)]
mod tests {
    use super::QrPayload;

    #[test]
    fn qr_payload_debug_is_redacted() {
        let debug = format!("{:?}", QrPayload::new("sensitive-payload"));

        assert_eq!(debug, "QrPayload([redacted])");
        assert!(!debug.contains("sensitive-payload"));
    }
}
