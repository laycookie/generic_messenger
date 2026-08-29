//! Transaction journal, crash recovery, and OS-vault generation replacement.

use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::Path,
};

use tracing::warn;
use uuid::Uuid;

use super::format::{
    ManifestEntry, StoredEntry, checked_entry_bytes, read_login_lines, validate_manifest_capacity,
    write_manifest,
};
use crate::codec::{decode, encode};
use crate::filesystem::{
    ReplaceOutcome, atomic_replace, sync_parent_directory, vault_journal_path,
};
use crate::vault::CredentialVault;
use crate::{
    MAX_LOGIN_FILE_BYTES, VAULT_JOURNAL_HEADER, VAULT_JOURNAL_HEADER_V1, VAULT_JOURNAL_PREFIX,
};

pub(super) enum StagedGeneration {
    /// Version 2 journals bind account names and slots to exact manifest order.
    ExactManifest(Vec<ManifestEntry>),
    /// Version 1 compatibility. Old journals recorded slots only.
    LegacySlots(Vec<String>),
}

pub(super) struct VaultJournal {
    pub(super) staged: StagedGeneration,
    pub(super) previous: Vec<String>,
}

impl VaultJournal {
    pub(super) fn new(staged: Vec<ManifestEntry>, previous: Vec<String>) -> Self {
        Self {
            staged: StagedGeneration::ExactManifest(staged),
            previous,
        }
    }

    fn staged_slots(&self) -> Vec<String> {
        match &self.staged {
            StagedGeneration::ExactManifest(entries) => {
                entries.iter().map(|entry| entry.slot.clone()).collect()
            }
            StagedGeneration::LegacySlots(slots) => slots.clone(),
        }
    }
}

pub(super) fn read_vault_journal(path: &Path) -> io::Result<Option<VaultJournal>> {
    let journal_path = vault_journal_path(path)?;
    let Some(lines) = read_login_lines(&journal_path)? else {
        return Ok(None);
    };
    let exact_manifest = match lines.first().map(|line| line.as_str()) {
        Some(VAULT_JOURNAL_HEADER) => true,
        Some(VAULT_JOURNAL_HEADER_V1) => false,
        Some(header) if header.starts_with(VAULT_JOURNAL_PREFIX) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported credential vault transaction journal version",
            ));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed credential vault transaction journal",
            ));
        }
    };

    let mut staged_manifest = Vec::new();
    let mut staged_slots = Vec::new();
    let mut previous = Vec::new();
    let mut seen = HashSet::new();
    for line in lines.iter().skip(1) {
        if let Some(staged) = line.strip_prefix("staged:") {
            let (name, slot) = if exact_manifest {
                let (encoded_name, slot) = staged.split_once(':').ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "malformed staged credential vault journal entry",
                    )
                })?;
                let name = decode(encoded_name).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "credential vault journal contains an invalid account name",
                    )
                })?;
                if name.is_empty() || name.contains([':', '\r', '\n']) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "credential vault journal contains an invalid account name",
                    ));
                }
                (Some(name), slot)
            } else {
                (None, staged)
            };
            if Uuid::parse_str(slot).is_err() || !seen.insert(slot.to_owned()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "credential vault transaction journal has an invalid or duplicate slot",
                ));
            }
            if let Some(name) = name {
                staged_manifest.push(ManifestEntry {
                    name,
                    slot: slot.to_owned(),
                });
            } else {
                staged_slots.push(slot.to_owned());
            }
        } else if let Some(slot) = line.strip_prefix("previous:") {
            if Uuid::parse_str(slot).is_err() || !seen.insert(slot.to_owned()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "credential vault transaction journal has an invalid or duplicate slot",
                ));
            }
            previous.push(slot.to_owned());
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "credential vault transaction journal has an unknown entry kind",
            ));
        }
    }

    Ok(Some(VaultJournal {
        staged: if exact_manifest {
            StagedGeneration::ExactManifest(staged_manifest)
        } else {
            StagedGeneration::LegacySlots(staged_slots)
        },
        previous,
    }))
}

pub(super) fn write_vault_journal(path: &Path, journal: &VaultJournal) -> io::Result<()> {
    let journal_path = vault_journal_path(path)?;
    let outcome = atomic_replace(&journal_path, |file| {
        let mut written = u64::try_from(VAULT_JOURNAL_HEADER.len() + 1).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "vault journal is too large")
        })?;
        writeln!(file, "{VAULT_JOURNAL_HEADER}")?;
        let StagedGeneration::ExactManifest(staged) = &journal.staged else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "legacy slot-only vault journals cannot be written",
            ));
        };
        for entry in staged {
            let name = encode(&entry.name);
            let line_length = "staged:"
                .len()
                .checked_add(name.len())
                .and_then(|length| length.checked_add(1))
                .and_then(|length| length.checked_add(entry.slot.len()))
                .and_then(|length| length.checked_add(1))
                .and_then(|length| u64::try_from(length).ok())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "vault journal is too large")
                })?;
            written = written
                .checked_add(line_length)
                .filter(|total| *total <= MAX_LOGIN_FILE_BYTES)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("vault journal exceeds {MAX_LOGIN_FILE_BYTES} byte limit"),
                    )
                })?;
            writeln!(file, "staged:{name}:{}", entry.slot)?;
        }
        for slot in &journal.previous {
            written = written
                .checked_add(checked_entry_bytes("previous", slot)?)
                .filter(|total| *total <= MAX_LOGIN_FILE_BYTES)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("vault journal exceeds {MAX_LOGIN_FILE_BYTES} byte limit"),
                    )
                })?;
            writeln!(file, "previous:{slot}")?;
        }
        Ok(())
    })?;
    match outcome {
        ReplaceOutcome::Durable => Ok(()),
        ReplaceOutcome::CommittedNotSynced(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "credential vault journal was renamed but its parent directory was not synchronized: {error}"
            ),
        )),
    }
}

fn remove_vault_journal(path: &Path) -> io::Result<()> {
    let journal_path = vault_journal_path(path)?;
    match fs::remove_file(&journal_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let parent = journal_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if let Err(error) = sync_parent_directory(parent) {
        // Recovery is idempotent if an unsynced deletion reappears.
        warn!(
            error = %error,
            "Credential vault journal deletion could not be synchronized"
        );
    }
    Ok(())
}

fn remove_vault_journal_best_effort(path: &Path) {
    if let Err(error) = remove_vault_journal(path) {
        warn!(
            error = %error,
            "Unable to remove completed credential vault transaction journal"
        );
    }
}

fn cleanup_vault_slots(vault: &dyn CredentialVault, slots: &[String]) -> io::Result<()> {
    let mut first_error = None;
    for slot in slots {
        if let Err(error) = vault.delete(slot) {
            warn!(
                error = %error,
                %slot,
                "Unable to remove unreachable OS credential vault entry"
            );
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(super) fn recover_vault_journal(
    path: &Path,
    current: Option<&[ManifestEntry]>,
    vault: &dyn CredentialVault,
) -> io::Result<()> {
    let Some(journal) = read_vault_journal(path)? else {
        return Ok(());
    };
    let current_slots = current.map(|entries| {
        entries
            .iter()
            .map(|entry| entry.slot.clone())
            .collect::<Vec<_>>()
    });
    // Version 2 binds names, slots, and order. Version 1 remains readable but
    // can only prove slot order because old journals never stored names.
    let committed = match &journal.staged {
        StagedGeneration::ExactManifest(staged) => current == Some(staged.as_slice()),
        StagedGeneration::LegacySlots(staged) => {
            current_slots.as_deref() == Some(staged.as_slice())
        }
    };
    let staged_slots = journal.staged_slots();
    let staged_set = staged_slots.iter().cloned().collect::<HashSet<_>>();
    if !committed
        && current_slots
            .as_ref()
            .is_some_and(|slots| slots.iter().any(|slot| staged_set.contains(slot)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "credential manifest only partially matches its transaction journal",
        ));
    }
    let cleanup = if committed {
        &journal.previous
    } else {
        &staged_slots
    };
    cleanup_vault_slots(vault, cleanup)?;
    remove_vault_journal(path)
}

pub(super) fn save_to_vault(
    path: &Path,
    entries: &[StoredEntry],
    previous: &[ManifestEntry],
    vault: &dyn CredentialVault,
) -> io::Result<()> {
    validate_manifest_capacity(entries)?;
    let staged = entries
        .iter()
        .map(|entry| ManifestEntry {
            name: entry.name.clone(),
            slot: Uuid::new_v4().to_string(),
        })
        .collect::<Vec<_>>();
    let staged_slots = staged
        .iter()
        .map(|entry| entry.slot.clone())
        .collect::<Vec<_>>();
    let journal = VaultJournal::new(
        staged.clone(),
        previous.iter().map(|entry| entry.slot.clone()).collect(),
    );
    // Persist exact staged manifest identity before creating any secret. A crash
    // can then distinguish old, new, and malformed mixed generations.
    write_vault_journal(path, &journal)?;

    for (entry, manifest) in entries.iter().zip(&staged) {
        if let Err(error) = vault.set(&manifest.slot, &entry.auth) {
            if cleanup_vault_slots(vault, &staged_slots).is_ok() {
                remove_vault_journal_best_effort(path);
            }
            return Err(error);
        }
    }
    match write_manifest(path, &staged) {
        Err(error) => {
            if cleanup_vault_slots(vault, &staged_slots).is_ok() {
                remove_vault_journal_best_effort(path);
            }
            Err(error)
        }
        Ok(ReplaceOutcome::CommittedNotSynced(error)) => {
            // Rename already committed, so deleting either generation could
            // corrupt recovery after power loss. The journal retries cleanup on
            // the next locked operation once the surviving manifest is known.
            warn!(
                error = %error,
                "Credential manifest committed but parent directory sync failed; retaining both vault generations"
            );
            Ok(())
        }
        Ok(ReplaceOutcome::Durable) => {
            if cleanup_vault_slots(vault, &journal.previous).is_ok() {
                remove_vault_journal_best_effort(path);
            }
            Ok(())
        }
    }
}
