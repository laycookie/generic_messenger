//! OS credential-vault abstraction and keyring adapter.

use std::io;

use zeroize::{Zeroize as _, Zeroizing};

use crate::VAULT_SERVICE;

pub(crate) trait CredentialVault {
    fn get(&self, slot: &str) -> io::Result<Zeroizing<String>>;
    fn set(&self, slot: &str, secret: &str) -> io::Result<()>;
    fn delete(&self, slot: &str) -> io::Result<()>;
}

pub(crate) struct SystemVault;

#[cfg(not(test))]
fn vault_error(context: &str, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{context}: {error}"))
}

fn owned_vault_error(context: &str, mut error: keyring::Error) -> io::Error {
    // Some keyring decoding errors own the raw secret returned by the OS.
    // Their Display output is intentionally non-secret; wipe those attached
    // bytes before releasing the error allocation.
    let message = format!("{context}: {error}");
    match &mut error {
        keyring::Error::BadEncoding(bytes) | keyring::Error::BadDataFormat(bytes, _) => {
            bytes.zeroize();
        }
        _ => {}
    }
    io::Error::other(message)
}

impl SystemVault {
    fn entry(slot: &str) -> io::Result<keyring::Entry> {
        keyring::Entry::new(VAULT_SERVICE, slot)
            .map_err(|error| owned_vault_error("failed to open OS credential vault entry", error))
    }
}

impl CredentialVault for SystemVault {
    fn get(&self, slot: &str) -> io::Result<Zeroizing<String>> {
        Self::entry(slot)?
            .get_password()
            .map(Zeroizing::new)
            .map_err(|error| owned_vault_error("failed to read OS credential vault entry", error))
    }

    fn set(&self, slot: &str, secret: &str) -> io::Result<()> {
        Self::entry(slot)?
            .set_password(secret)
            .map_err(|error| owned_vault_error("failed to write OS credential vault entry", error))
    }

    fn delete(&self, slot: &str) -> io::Result<()> {
        match Self::entry(slot)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(owned_vault_error(
                "failed to delete OS credential vault entry",
                error,
            )),
        }
    }
}

#[cfg(not(test))]
pub(crate) fn open_system_vault() -> io::Result<SystemVault> {
    match keyring::Entry::store_status() {
        Ok(()) => Ok(SystemVault),
        Err(error) => Err(vault_error(
            "operating-system credential vault is unavailable",
            error,
        )),
    }
}

// Unit tests must never read or mutate the developer's real desktop keyring.
#[cfg(test)]
pub(crate) fn open_system_vault() -> io::Result<SystemVault> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "OS credential vault disabled in unit tests",
    ))
}
