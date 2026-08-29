//! Persistent login credential storage.
//!
//! [`to_line`]/[`from_line`] encode a backend [`Credential`] as a delimiter-safe
//! percent-encoded `key=value&...` blob. A secret containing `:` `&` `=` or a
//! newline therefore round-trips intact, and each decoded key is resolved back
//! to one of the backend's known static field keys.
//!
//! [`load`]/[`save`] keep those blobs in the operating system's credential vault
//! when it is usable. `LoginInfo` then contains only a versioned list of backend
//! names and opaque vault identifiers. Existing legacy `name:auth` plaintext
//! files remain readable on Unix for compatibility and are migrated
//! transactionally: the file is replaced only after every secret has been
//! written successfully. When the OS vault is unavailable, Unix falls back to
//! an owner-only plaintext store; status-aware APIs report that downgrade and
//! its sanitized cause. Explicit plaintext selection uses a tagged format so it
//! persists without being auto-migrated. Vault manifests are downgraded only by
//! an explicit [`StorageMode::Plaintext`] request.

mod codec;
mod filesystem;
mod store;
mod vault;

const MAX_LOGIN_FILE_BYTES: u64 = 1024 * 1024;
const VAULT_MANIFEST_PREFIX: &str = "# generic_messenger credential vault ";
const VAULT_MANIFEST_HEADER: &str = "# generic_messenger credential vault v1";
const PLAINTEXT_STORE_PREFIX: &str = "# generic_messenger plaintext credentials ";
const PLAINTEXT_STORE_HEADER: &str = "# generic_messenger plaintext credentials v1";
const VAULT_JOURNAL_PREFIX: &str = "# generic_messenger credential vault transaction ";
const VAULT_JOURNAL_HEADER_V1: &str = "# generic_messenger credential vault transaction v1";
const VAULT_JOURNAL_HEADER: &str = "# generic_messenger credential vault transaction v2";
const VAULT_SERVICE: &str = "io.github.mroute.generic_messenger";

const EXISTING_PLAINTEXT_STORE_ALLOWED: bool = cfg!(unix);

/// Opaque fingerprint used to reject stale cross-process credential writes.
#[derive(Clone, Eq, PartialEq)]
pub struct StoreRevision([u8; 32]);

impl std::fmt::Debug for StoreRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StoreRevision(..)")
    }
}

/// Durable location used for saved login secrets.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum StorageMode {
    /// Native OS credential manager. `LoginInfo` contains opaque references only.
    SystemVault,
    /// Owner-only plaintext `LoginInfo` file. Supported only where permissions
    /// can be enforced by this crate.
    Plaintext,
}

/// Current credential-storage mode plus any OS-vault failure that affected it.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct StorageStatus {
    mode: StorageMode,
    automatic_fallback: bool,
    vault_error: Option<String>,
    warning: Option<String>,
}

impl StorageStatus {
    fn system_vault(vault_error: Option<String>) -> Self {
        Self {
            mode: StorageMode::SystemVault,
            automatic_fallback: false,
            vault_error,
            warning: None,
        }
    }

    fn plaintext(automatic_fallback: bool, vault_error: Option<String>) -> Self {
        Self {
            mode: StorageMode::Plaintext,
            automatic_fallback,
            vault_error,
            warning: None,
        }
    }

    /// Durable location currently selected for saved secrets.
    pub const fn mode(&self) -> StorageMode {
        self.mode
    }

    /// True when plaintext was selected automatically after an OS-vault failure.
    pub const fn is_automatic_fallback(&self) -> bool {
        self.automatic_fallback
    }

    /// Sanitized OS-vault failure associated with current status, if any.
    pub fn vault_error(&self) -> Option<&str> {
        self.vault_error.as_deref()
    }

    fn with_warning(mut self, warning: impl Into<String>) -> Self {
        let warning = warning.into();
        self.warning = Some(match self.warning {
            Some(existing) => format!("{existing}; {warning}"),
            None => warning,
        });
        self
    }

    /// Recoverable storage warning associated with current state, if any.
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }
}

/// Whether explicit plaintext storage can enforce owner-only file permissions.
pub const fn plaintext_storage_supported() -> bool {
    EXISTING_PLAINTEXT_STORE_ALLOWED
}

pub struct Loaded<T> {
    pub values: Vec<T>,
    pub revision: StoreRevision,
    pub storage: StorageStatus,
}

/// Result of a status-aware credential save.
pub struct Saved {
    pub revision: StoreRevision,
    pub storage: StorageStatus,
}

pub use codec::{from_line, to_line};
pub use store::{
    load, load_versioned, save, save_versioned, save_versioned_as, save_versioned_with_status,
};
