//! Credential load/save orchestration and public API.

mod format;
mod transaction;

use std::{io, path::Path};

use tracing::warn;

use self::format::*;
use self::transaction::*;
use crate::filesystem::{ReplaceOutcome, lock_store, store_revision};
use crate::vault::{CredentialVault, open_system_vault};
use crate::{
    EXISTING_PLAINTEXT_STORE_ALLOWED, Loaded, Saved, StorageMode, StorageStatus, StoreRevision,
};

enum VaultAccess<'a> {
    Available(&'a dyn CredentialVault),
    Unavailable(io::Error),
}

fn hydrate_manifest(
    manifest: Vec<ManifestEntry>,
    vault: &dyn CredentialVault,
) -> io::Result<Vec<StoredEntry>> {
    let mut entries = Vec::with_capacity(manifest.len());
    for entry in manifest {
        entries.push(StoredEntry {
            name: entry.name,
            auth: vault.get(&entry.slot)?,
        });
    }
    validate_entries(&entries).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("OS credential vault returned invalid credential data: {error}"),
        )
    })?;
    Ok(entries)
}

fn resolve_entries<T>(
    entries: Vec<StoredEntry>,
    mut resolve: impl FnMut(&str, &str) -> Option<T>,
) -> Vec<T> {
    entries
        .into_iter()
        .filter_map(|entry| resolve(&entry.name, &entry.auth))
        .collect()
}

fn load_with_vault_status<T>(
    path: &Path,
    vault: VaultAccess<'_>,
    resolve: impl FnMut(&str, &str) -> Option<T>,
) -> io::Result<(Vec<T>, StorageStatus)> {
    let disk = read_disk_store(path)?;
    let mut plaintext_warning = None;
    if let VaultAccess::Available(vault) = &vault {
        let current = match &disk {
            DiskStore::VaultManifest(entries) => Some(entries.as_slice()),
            DiskStore::Missing | DiskStore::Legacy(_) | DiskStore::ExplicitPlaintext(_) => None,
        };
        if let Err(error) = recover_vault_journal(path, current, *vault) {
            if EXISTING_PLAINTEXT_STORE_ALLOWED && matches!(&disk, DiskStore::ExplicitPlaintext(_))
            {
                warn!(
                    error = %error,
                    "Unable to remove previous OS-vault entries; loading committed plaintext store"
                );
                plaintext_warning = Some(format!(
                    "plaintext is active, but previous OS-vault entries could not be removed: {error}"
                ));
            } else {
                return Err(error);
            }
        }
    }
    match disk {
        DiskStore::Missing => {
            let storage = match vault {
                VaultAccess::Available(_) => StorageStatus::system_vault(None),
                VaultAccess::Unavailable(error) if EXISTING_PLAINTEXT_STORE_ALLOWED => {
                    StorageStatus::plaintext(true, Some(error.to_string()))
                }
                VaultAccess::Unavailable(error) => {
                    StorageStatus::system_vault(Some(error.to_string()))
                }
            };
            Ok((Vec::new(), storage))
        }
        DiskStore::VaultManifest(manifest) if manifest.is_empty() => {
            let storage = match vault {
                VaultAccess::Available(_) => StorageStatus::system_vault(None),
                VaultAccess::Unavailable(error) => {
                    StorageStatus::system_vault(Some(error.to_string()))
                }
            };
            Ok((Vec::new(), storage))
        }
        DiskStore::VaultManifest(manifest) => match vault {
            VaultAccess::Available(vault) => Ok((
                resolve_entries(hydrate_manifest(manifest, vault)?, resolve),
                StorageStatus::system_vault(None),
            )),
            VaultAccess::Unavailable(error) => Err(error),
        },
        DiskStore::ExplicitPlaintext(entries) => {
            if !EXISTING_PLAINTEXT_STORE_ALLOWED {
                return match vault {
                    VaultAccess::Available(vault) => {
                        // A selected plaintext file may arrive from Unix on a
                        // platform that cannot enforce its permissions. Upgrade
                        // it rather than making otherwise-readable accounts fail.
                        save_to_vault(path, &entries, &[], vault)?;
                        Ok((
                            resolve_entries(entries, resolve),
                            StorageStatus::system_vault(None),
                        ))
                    }
                    VaultAccess::Unavailable(error) => Err(error),
                };
            }
            let vault_error = match vault {
                VaultAccess::Available(_) => None,
                VaultAccess::Unavailable(error) => Some(error.to_string()),
            };
            let mut storage = StorageStatus::plaintext(false, vault_error);
            if let Some(warning) = plaintext_warning {
                storage = storage.with_warning(warning);
            }
            Ok((resolve_entries(entries, resolve), storage))
        }
        DiskStore::Legacy(entries) => {
            if entries.is_empty() {
                let storage = match vault {
                    VaultAccess::Available(_) => StorageStatus::system_vault(None),
                    VaultAccess::Unavailable(error) if EXISTING_PLAINTEXT_STORE_ALLOWED => {
                        StorageStatus::plaintext(true, Some(error.to_string()))
                    }
                    VaultAccess::Unavailable(error) => {
                        StorageStatus::system_vault(Some(error.to_string()))
                    }
                };
                return Ok((Vec::new(), storage));
            }

            let storage = match vault {
                VaultAccess::Available(vault) => {
                    // Migration is best-effort only where chmod can enforce
                    // the legacy fallback's owner-only guarantee.
                    match save_to_vault(path, &entries, &[], vault) {
                        Ok(()) => StorageStatus::system_vault(None),
                        Err(error) if EXISTING_PLAINTEXT_STORE_ALLOWED => {
                            warn!(
                                error = %error,
                                "OS credential vault migration failed; retaining owner-only plaintext"
                            );
                            StorageStatus::plaintext(true, Some(error.to_string()))
                        }
                        Err(error) => return Err(error),
                    }
                }
                VaultAccess::Unavailable(error) => {
                    if !EXISTING_PLAINTEXT_STORE_ALLOWED {
                        return Err(error);
                    }
                    warn!(
                        error = %error,
                        "OS credential vault unavailable; loading owner-only plaintext"
                    );
                    StorageStatus::plaintext(true, Some(error.to_string()))
                }
            };
            Ok((resolve_entries(entries, resolve), storage))
        }
    }
}

#[cfg(test)]
fn load_with_vault<T>(
    path: &Path,
    vault: VaultAccess<'_>,
    resolve: impl FnMut(&str, &str) -> Option<T>,
) -> io::Result<Vec<T>> {
    Ok(load_with_vault_status(path, vault, resolve)?.0)
}

/// Load credentials plus an opaque revision and storage status for
/// conflict-checked saving.
pub fn load_versioned<T>(
    path: impl AsRef<Path>,
    resolve: impl FnMut(&str, &str) -> Option<T>,
) -> io::Result<Loaded<T>> {
    let path = path.as_ref();
    let _lock = lock_store(path)?;
    let (values, storage) = match open_system_vault() {
        Ok(vault) => load_with_vault_status(path, VaultAccess::Available(&vault), resolve),
        Err(error) => load_with_vault_status(path, VaultAccess::Unavailable(error), resolve),
    }?;
    // Migration may have replaced the legacy file, so fingerprint only after
    // all load-time writes finish while the interprocess lock is still held.
    let revision = store_revision(path)?;
    Ok(Loaded {
        values,
        revision,
        storage,
    })
}

/// Reads credentials from the OS vault manifest or legacy owner-only file and
/// maps each complete `name:auth` pair through `resolve`.
///
/// A missing file yields an empty `Vec`. Values whose `resolve` returns `None`
/// are skipped, while malformed storage and incomplete vault reads are hard
/// errors so a later save cannot silently erase unrecognized data.
pub fn load<T>(
    path: impl AsRef<Path>,
    resolve: impl FnMut(&str, &str) -> Option<T>,
) -> io::Result<Vec<T>> {
    Ok(load_versioned(path, resolve)?.values)
}

fn finish_plaintext_commit(
    storage: StorageStatus,
    outcome: ReplaceOutcome,
) -> (bool, StorageStatus) {
    match outcome {
        ReplaceOutcome::Durable => (true, storage),
        ReplaceOutcome::CommittedNotSynced(error) => {
            warn!(
                error = %error,
                "Plaintext credential file committed but parent directory sync failed"
            );
            (
                false,
                storage.with_warning(format!(
                    "plaintext credential file was written, but its directory could not be synchronized: {error}"
                )),
            )
        }
    }
}

fn save_with_vault(
    path: &Path,
    entries: &[StoredEntry],
    vault: VaultAccess<'_>,
) -> io::Result<StorageStatus> {
    let disk = read_disk_store(path)?;
    match vault {
        VaultAccess::Available(vault) => {
            let current = match &disk {
                DiskStore::VaultManifest(entries) => Some(entries.as_slice()),
                DiskStore::Missing | DiskStore::Legacy(_) | DiskStore::ExplicitPlaintext(_) => None,
            };
            let mut plaintext_warning = None;
            if let Err(error) = recover_vault_journal(path, current, vault) {
                if EXISTING_PLAINTEXT_STORE_ALLOWED
                    && matches!(&disk, DiskStore::ExplicitPlaintext(_))
                {
                    warn!(
                        error = %error,
                        "Unable to remove previous OS-vault entries; updating committed plaintext store"
                    );
                    plaintext_warning = Some(format!(
                        "plaintext is active, but previous OS-vault entries could not be removed: {error}"
                    ));
                } else {
                    return Err(error);
                }
            }
            match disk {
                DiskStore::VaultManifest(previous) => {
                    // Once migrated, never downgrade the store if the vault
                    // becomes unavailable or rejects a write.
                    save_to_vault(path, entries, &previous, vault)?;
                    Ok(StorageStatus::system_vault(None))
                }
                DiskStore::Missing | DiskStore::Legacy(_) => {
                    // A failed vault transaction must never turn into a new
                    // plaintext write: the error may represent a locked vault,
                    // corrupt journal, permission failure, or incomplete cleanup.
                    save_to_vault(path, entries, &[], vault)?;
                    Ok(StorageStatus::system_vault(None))
                }
                DiskStore::ExplicitPlaintext(_) => {
                    let outcome = write_plaintext(path, entries, true)?;
                    let mut storage = StorageStatus::plaintext(false, None);
                    if let Some(warning) = plaintext_warning {
                        storage = storage.with_warning(warning);
                    }
                    let (_, storage) = finish_plaintext_commit(storage, outcome);
                    Ok(storage)
                }
            }
        }
        VaultAccess::Unavailable(error) => match disk {
            DiskStore::VaultManifest(_) => Err(error),
            DiskStore::ExplicitPlaintext(_) => {
                if !EXISTING_PLAINTEXT_STORE_ALLOWED {
                    return Err(error);
                }
                let reason = error.to_string();
                let outcome = write_plaintext(path, entries, true)?;
                let (_, storage) =
                    finish_plaintext_commit(StorageStatus::plaintext(false, Some(reason)), outcome);
                Ok(storage)
            }
            DiskStore::Missing | DiskStore::Legacy(_) => {
                if !EXISTING_PLAINTEXT_STORE_ALLOWED {
                    return Err(error);
                }
                // Never automatically downgrade a vault manifest. Missing and
                // legacy stores may use Unix's owner-only plaintext fallback.
                let reason = error.to_string();
                warn!(
                    error = %error,
                    "OS credential vault unavailable; using owner-only plaintext credential store"
                );
                let outcome = write_plaintext(path, entries, false)?;
                let (_, storage) =
                    finish_plaintext_commit(StorageStatus::plaintext(true, Some(reason)), outcome);
                Ok(storage)
            }
        },
    }
}

fn save_as_system_vault(
    path: &Path,
    entries: &[StoredEntry],
    vault: VaultAccess<'_>,
) -> io::Result<StorageStatus> {
    let vault = match vault {
        VaultAccess::Available(vault) => vault,
        VaultAccess::Unavailable(error) => return Err(error),
    };
    let disk = read_disk_store(path)?;
    let current = match &disk {
        DiskStore::VaultManifest(entries) => Some(entries.as_slice()),
        DiskStore::Missing | DiskStore::Legacy(_) | DiskStore::ExplicitPlaintext(_) => None,
    };
    recover_vault_journal(path, current, vault)?;
    let previous = match &disk {
        DiskStore::VaultManifest(entries) => entries.as_slice(),
        DiskStore::Missing | DiskStore::Legacy(_) | DiskStore::ExplicitPlaintext(_) => &[],
    };
    save_to_vault(path, entries, previous, vault)?;
    Ok(StorageStatus::system_vault(None))
}

fn save_as_plaintext(
    path: &Path,
    entries: &[StoredEntry],
    vault: VaultAccess<'_>,
) -> io::Result<StorageStatus> {
    if !EXISTING_PLAINTEXT_STORE_ALLOWED {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "plaintext credential storage is not supported on this platform",
        ));
    }

    let disk = read_disk_store(path)?;
    let current = match &disk {
        DiskStore::VaultManifest(entries) => Some(entries.as_slice()),
        DiskStore::Missing | DiskStore::Legacy(_) | DiskStore::ExplicitPlaintext(_) => None,
    };
    match &vault {
        VaultAccess::Available(vault) => recover_vault_journal(path, current, *vault)?,
        VaultAccess::Unavailable(_) => {
            if current.is_some() && read_vault_journal(path)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "cannot switch storage while an OS-vault transaction is pending",
                ));
            }
        }
    }

    if let DiskStore::VaultManifest(previous) = &disk
        && !previous.is_empty()
    {
        // Treat old manifest slots as the staged generation. Before plaintext
        // commits, recovery sees those slots in current manifest and keeps them;
        // afterward it sees no manifest and deletes them.
        write_vault_journal(path, &VaultJournal::new(previous.clone(), Vec::new()))?;
    }
    let outcome = write_plaintext(path, entries, true)?;

    let vault_error = match &vault {
        VaultAccess::Available(_) => None,
        VaultAccess::Unavailable(error) => Some(error.to_string()),
    };
    let (durable, mut storage) =
        finish_plaintext_commit(StorageStatus::plaintext(false, vault_error), outcome);
    if durable
        && let VaultAccess::Available(vault) = &vault
        && let Err(error) = recover_vault_journal(path, None, *vault)
    {
        // Plaintext is already committed. Keep the journal for a later cleanup
        // retry instead of reporting the mode switch as if it had rolled back.
        warn!(
            error = %error,
            "Plaintext credential store committed; unable to remove previous OS-vault entries"
        );
        storage = storage.with_warning(format!(
            "plaintext is active, but previous OS-vault entries could not be removed: {error}"
        ));
    }
    Ok(storage)
}

fn save_locked(path: &Path, entries: &[StoredEntry]) -> io::Result<StorageStatus> {
    match open_system_vault() {
        Ok(vault) => save_with_vault(path, entries, VaultAccess::Available(&vault)),
        Err(error) => save_with_vault(path, entries, VaultAccess::Unavailable(error)),
    }
}

fn save_locked_as(
    path: &Path,
    entries: &[StoredEntry],
    mode: StorageMode,
) -> io::Result<StorageStatus> {
    match (mode, open_system_vault()) {
        (StorageMode::SystemVault, Ok(vault)) => {
            save_as_system_vault(path, entries, VaultAccess::Available(&vault))
        }
        (StorageMode::SystemVault, Err(error)) => {
            save_as_system_vault(path, entries, VaultAccess::Unavailable(error))
        }
        (StorageMode::Plaintext, Ok(vault)) => {
            save_as_plaintext(path, entries, VaultAccess::Available(&vault))
        }
        (StorageMode::Plaintext, Err(error)) => {
            save_as_plaintext(path, entries, VaultAccess::Unavailable(error))
        }
    }
}

/// Conflict-checked save that also reports resulting secret-storage status.
pub fn save_versioned_with_status<I, K, V>(
    path: impl AsRef<Path>,
    expected: &StoreRevision,
    entries: I,
) -> io::Result<Saved>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let path = path.as_ref();
    let entries = collect_entries(entries)?;
    let _lock = lock_store(path)?;
    if &store_revision(path)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "credential store changed in another process; refusing stale overwrite",
        ));
    }
    let storage = save_locked(path, &entries)?;
    Ok(Saved {
        revision: store_revision(path)?,
        storage,
    })
}

/// Conflict-checked save that explicitly switches secret-storage mode.
pub fn save_versioned_as<I, K, V>(
    path: impl AsRef<Path>,
    expected: &StoreRevision,
    entries: I,
    mode: StorageMode,
) -> io::Result<Saved>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let path = path.as_ref();
    let entries = collect_entries(entries)?;
    let _lock = lock_store(path)?;
    if &store_revision(path)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "credential store changed in another process; refusing stale overwrite",
        ));
    }
    let storage = save_locked_as(path, &entries, mode)?;
    Ok(Saved {
        revision: store_revision(path)?,
        storage,
    })
}

/// Conflict-checked variant of [`save`]. The write fails with
/// [`io::ErrorKind::WouldBlock`] if another process changed the store after
/// [`load_versioned`] returned `expected`.
pub fn save_versioned<I, K, V>(
    path: impl AsRef<Path>,
    expected: &StoreRevision,
    entries: I,
) -> io::Result<StoreRevision>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    Ok(save_versioned_with_status(path, expected, entries)?.revision)
}

/// Save credentials transactionally to the OS vault and atomically replace
/// `path` with non-secret references. If the system vault is unavailable, Unix
/// falls back to an owner-only plaintext store. Other platforms fail closed, and
/// a migrated vault manifest is never silently downgraded. Application code
/// should prefer [`save_versioned_with_status`].
pub fn save<I, K, V>(path: impl AsRef<Path>, entries: I) -> io::Result<()>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let path = path.as_ref();
    let entries = collect_entries(entries)?;
    let _lock = lock_store(path)?;
    save_locked(path, &entries)?;
    Ok(())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
