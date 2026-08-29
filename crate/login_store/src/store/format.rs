//! Credential-store disk formats, parsing, validation, and serialization.

use std::{
    collections::HashSet,
    io::{self, BufRead, BufReader, Read, Write},
    path::Path,
};

use uuid::Uuid;
use zeroize::Zeroizing;

use crate::codec::{decode, encode};
use crate::filesystem::{ReplaceOutcome, atomic_replace, open_login_file};
use crate::{
    MAX_LOGIN_FILE_BYTES, PLAINTEXT_STORE_HEADER, PLAINTEXT_STORE_PREFIX, VAULT_MANIFEST_HEADER,
    VAULT_MANIFEST_PREFIX,
};

pub(super) struct StoredEntry {
    pub(super) name: String,
    pub(super) auth: Zeroizing<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ManifestEntry {
    pub(super) name: String,
    pub(super) slot: String,
}

pub(super) enum DiskStore {
    Missing,
    Legacy(Vec<StoredEntry>),
    ExplicitPlaintext(Vec<StoredEntry>),
    VaultManifest(Vec<ManifestEntry>),
}

pub(super) fn read_login_lines(path: &Path) -> io::Result<Option<Vec<Zeroizing<String>>>> {
    let Some(file) = open_login_file(path)? else {
        return Ok(None);
    };

    // Metadata validation races writers. Bound the reader too so a regular file
    // grown after `metadata()` cannot force an unbounded `read_line` allocation.
    let mut reader = BufReader::new(file.take(MAX_LOGIN_FILE_BYTES + 1));
    let mut consumed = 0_u64;
    let mut lines = Vec::new();
    let mut line = Zeroizing::new(String::new());
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        consumed = consumed.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if consumed > MAX_LOGIN_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("credential file exceeds {MAX_LOGIN_FILE_BYTES} byte limit"),
            ));
        }
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        lines.push(Zeroizing::new(std::mem::take(&mut *line)));
    }
    Ok(Some(lines))
}

pub(super) fn read_disk_store(path: &Path) -> io::Result<DiskStore> {
    let Some(lines) = read_login_lines(path)? else {
        return Ok(DiskStore::Missing);
    };
    if lines.first().map(|line| line.as_str()) == Some(VAULT_MANIFEST_HEADER) {
        let mut entries = Vec::with_capacity(lines.len().saturating_sub(1));
        let mut slots = HashSet::new();
        for line in lines.iter().skip(1) {
            let (encoded_name, slot) = line.split_once(':').ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed credential vault manifest entry",
                )
            })?;
            let name = decode(encoded_name).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "credential vault manifest contains an invalid name",
                )
            })?;
            if name.is_empty()
                || name.contains([':', '\r', '\n'])
                || Uuid::parse_str(slot).is_err()
                || !slots.insert(slot.to_owned())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "credential vault manifest contains an invalid or duplicate entry",
                ));
            }
            entries.push(ManifestEntry {
                name,
                slot: slot.to_owned(),
            });
        }
        return Ok(DiskStore::VaultManifest(entries));
    }
    if lines.first().is_some_and(|line| {
        line.starts_with(VAULT_MANIFEST_PREFIX) && line.as_str() != VAULT_MANIFEST_HEADER
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported credential vault manifest version",
        ));
    }
    if lines.first().is_some_and(|line| {
        line.starts_with(PLAINTEXT_STORE_PREFIX) && line.as_str() != PLAINTEXT_STORE_HEADER
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported plaintext credential-store version",
        ));
    }

    let explicit_plaintext =
        lines.first().map(|line| line.as_str()) == Some(PLAINTEXT_STORE_HEADER);
    let first_entry = if explicit_plaintext { 1 } else { 0 };
    let mut entries = Vec::new();
    for line in lines.into_iter().skip(first_entry) {
        if line.is_empty() {
            continue;
        }
        let (name, auth) = line.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed plaintext credential entry",
            )
        })?;
        if name.is_empty() || name.contains([':', '\r', '\n']) || auth.contains(['\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "plaintext credential store contains an invalid entry",
            ));
        }
        entries.push(StoredEntry {
            name: name.to_owned(),
            auth: Zeroizing::new(auth.to_owned()),
        });
    }
    Ok(if explicit_plaintext {
        DiskStore::ExplicitPlaintext(entries)
    } else {
        DiskStore::Legacy(entries)
    })
}

pub(super) fn checked_entry_bytes(name: &str, auth: &str) -> io::Result<u64> {
    name.len()
        .checked_add(auth.len())
        .and_then(|length| length.checked_add(2))
        .and_then(|length| u64::try_from(length).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "credential data is too large"))
}

pub(super) fn validate_entries(entries: &[StoredEntry]) -> io::Result<()> {
    let mut total = 0_u64;
    for entry in entries {
        if entry.name.is_empty()
            || entry.name.contains([':', '\r', '\n'])
            || entry.auth.contains(['\r', '\n'])
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "credential names and values must fit the line-oriented store format",
            ));
        }
        total = total
            .checked_add(checked_entry_bytes(&entry.name, &entry.auth)?)
            .filter(|total| *total <= MAX_LOGIN_FILE_BYTES)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("credential data exceeds {MAX_LOGIN_FILE_BYTES} byte limit"),
                )
            })?;
    }
    Ok(())
}

pub(super) fn collect_entries<I, K, V>(entries: I) -> io::Result<Vec<StoredEntry>>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let entries = entries
        .into_iter()
        .map(|(name, auth)| StoredEntry {
            name: name.as_ref().to_owned(),
            auth: Zeroizing::new(auth.as_ref().to_owned()),
        })
        .collect::<Vec<_>>();
    validate_entries(&entries)?;
    Ok(entries)
}

pub(super) fn write_plaintext(
    path: &Path,
    entries: &[StoredEntry],
    explicit_selection: bool,
) -> io::Result<ReplaceOutcome> {
    if explicit_selection {
        let entry_bytes = entries.iter().try_fold(0_u64, |total, entry| {
            total
                .checked_add(checked_entry_bytes(&entry.name, &entry.auth)?)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "credential data is too large")
                })
        })?;
        let header_bytes = u64::try_from(PLAINTEXT_STORE_HEADER.len() + 1).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "credential data is too large")
        })?;
        if header_bytes
            .checked_add(entry_bytes)
            .is_none_or(|total| total > MAX_LOGIN_FILE_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("credential data exceeds {MAX_LOGIN_FILE_BYTES} byte limit"),
            ));
        }
    }

    atomic_replace(path, |file| {
        if explicit_selection {
            writeln!(file, "{PLAINTEXT_STORE_HEADER}")?;
        }
        for entry in entries {
            writeln!(file, "{}:{}", entry.name, entry.auth.as_str())?;
        }
        Ok(())
    })
}

pub(super) fn validate_manifest_capacity(entries: &[StoredEntry]) -> io::Result<()> {
    let mut written = u64::try_from(VAULT_MANIFEST_HEADER.len() + 1).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "credential manifest is too large",
        )
    })?;
    for entry in entries {
        let encoded_name = encode(&entry.name);
        written = written
            .checked_add(checked_entry_bytes(
                &encoded_name,
                "00000000-0000-0000-0000-000000000000",
            )?)
            .filter(|total| *total <= MAX_LOGIN_FILE_BYTES)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("credential manifest exceeds {MAX_LOGIN_FILE_BYTES} byte limit"),
                )
            })?;
    }
    Ok(())
}

pub(super) fn write_manifest(path: &Path, entries: &[ManifestEntry]) -> io::Result<ReplaceOutcome> {
    atomic_replace(path, |file| {
        let mut written = u64::try_from(VAULT_MANIFEST_HEADER.len() + 1).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "credential manifest is too large",
            )
        })?;
        writeln!(file, "{VAULT_MANIFEST_HEADER}")?;
        for entry in entries {
            let name = encode(&entry.name);
            let line_bytes = checked_entry_bytes(&name, &entry.slot)?;
            written = written
                .checked_add(line_bytes)
                .filter(|total| *total <= MAX_LOGIN_FILE_BYTES)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("credential manifest exceeds {MAX_LOGIN_FILE_BYTES} byte limit"),
                    )
                })?;
            writeln!(file, "{name}:{}", entry.slot)?;
        }
        Ok(())
    })
}
