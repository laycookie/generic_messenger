use std::{
    collections::HashMap,
    fs::{self, File},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use messenger_interface::auth::{Credential, CredentialField};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::*;
use crate::filesystem::{lock_store, set_private_permissions, store_lock_path, vault_journal_path};
use crate::{
    MAX_LOGIN_FILE_BYTES, PLAINTEXT_STORE_HEADER, VAULT_JOURNAL_HEADER_V1, VAULT_MANIFEST_HEADER,
    from_line, to_line,
};

#[derive(Default)]
struct MemoryVault {
    entries: Mutex<HashMap<String, String>>,
    fail_reads: AtomicBool,
    fail_writes: AtomicBool,
    fail_deletes: AtomicBool,
}

impl CredentialVault for MemoryVault {
    fn get(&self, slot: &str) -> io::Result<Zeroizing<String>> {
        if self.fail_reads.load(Ordering::Acquire) {
            return Err(io::Error::other("injected vault read failure"));
        }
        self.entries
            .lock()
            .unwrap()
            .get(slot)
            .cloned()
            .map(Zeroizing::new)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing vault entry"))
    }

    fn set(&self, slot: &str, secret: &str) -> io::Result<()> {
        if self.fail_writes.load(Ordering::Acquire) {
            return Err(io::Error::other("injected vault write failure"));
        }
        self.entries
            .lock()
            .unwrap()
            .insert(slot.to_owned(), secret.to_owned());
        Ok(())
    }

    fn delete(&self, slot: &str) -> io::Result<()> {
        if self.fail_deletes.load(Ordering::Acquire) {
            return Err(io::Error::other("injected vault delete failure"));
        }
        self.entries.lock().unwrap().remove(slot);
        Ok(())
    }
}

/// Look up a field's value by key in a parsed credential.
fn value<'a>(cred: &'a Credential, key: &str) -> Option<&'a str> {
    cred.0
        .iter()
        .find(|field| field.key == key)
        .map(|field| field.value.unsecure())
}

#[test]
fn value_round_trips_through_special_chars() {
    let cred = Credential(vec![CredentialField::new("token", "abc:123&x=%y zπ")]);
    let line = to_line(&cred);
    // The blob must be free of the delimiters the file format uses.
    assert!(!line.contains(['\n', ':', ' ']));
    let restored = from_line(&line, &["token"]).unwrap();
    assert_eq!(value(&restored, "token"), Some("abc:123&x=%y zπ"));
}

#[test]
fn multiple_fields_round_trip_including_keys() {
    let cred = Credential(vec![
        // Keys are encoded too, so a key with a delimiter survives.
        CredentialField::new("user:name&", "alice"),
        CredentialField::new("token", "t=%ok"),
    ]);
    let keys = ["user:name&", "token"];
    let restored = from_line(&to_line(&cred), &keys).unwrap();
    assert_eq!(value(&restored, "user:name&"), Some("alice"));
    assert_eq!(value(&restored, "token"), Some("t=%ok"));
    assert_eq!(value(&restored, "missing"), None);
}

#[test]
fn fully_encoded_blob_is_accepted_for_migration() {
    let restored = from_line("token%3Dabc.def%253Aghi", &["token"]).unwrap();
    assert_eq!(value(&restored, "token"), Some("abc.def:ghi"));
}

#[test]
fn malformed_pairs_are_rejected() {
    assert!(from_line("no-equals", &["token"]).is_none());
    assert!(from_line("token=%ZZ", &["token"]).is_none());
}

#[test]
fn unknown_keys_are_rejected() {
    // A stored key the schema doesn't declare rejects the whole credential.
    assert!(from_line("ghost=value", &["token"]).is_none());
}

#[test]
fn missing_login_file_loads_empty_without_creating_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let loaded = load::<()>(&path, |_, _| None).unwrap();
    assert!(loaded.is_empty());
    assert!(!path.exists());
}

#[test]
fn legacy_load_migrates_plaintext_into_vault_transactionally() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    fs::write(
        &path,
        "discord:token=secret-value\nfuture:opaque=preserved\n",
    )
    .unwrap();
    let vault = MemoryVault::default();

    let loaded = load_with_vault(&path, VaultAccess::Available(&vault), |name, auth| {
        Some((name.to_owned(), auth.to_owned()))
    })
    .unwrap();

    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0], ("discord".into(), "token=secret-value".into()));
    let manifest = fs::read_to_string(&path).unwrap();
    assert!(manifest.starts_with(VAULT_MANIFEST_HEADER));
    assert!(!manifest.contains("secret-value"));
    assert!(!manifest.contains("preserved"));

    let restored = load_with_vault(&path, VaultAccess::Available(&vault), |name, auth| {
        Some((name.to_owned(), auth.to_owned()))
    })
    .unwrap();
    assert_eq!(restored, loaded);
}

#[test]
fn failed_migration_keeps_legacy_plaintext_readable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let plaintext = "discord:token=still-readable\n";
    fs::write(&path, plaintext).unwrap();
    let vault = MemoryVault::default();
    vault.fail_writes.store(true, Ordering::Release);

    let (loaded, storage) =
        load_with_vault_status(&path, VaultAccess::Available(&vault), |name, auth| {
            Some((name.to_owned(), auth.to_owned()))
        })
        .unwrap();

    assert_eq!(loaded.len(), 1);
    assert_eq!(storage.mode(), StorageMode::Plaintext);
    assert!(storage.is_automatic_fallback());
    assert!(
        storage
            .vault_error()
            .is_some_and(|error| error.contains("injected vault write failure"))
    );
    assert_eq!(fs::read_to_string(path).unwrap(), plaintext);
    assert!(vault.entries.lock().unwrap().is_empty());
}

#[test]
fn committed_unsynced_plaintext_is_success_with_warning() {
    let storage = StorageStatus::plaintext(true, Some("injected vault outage".to_owned()));

    let (durable, storage) = finish_plaintext_commit(
        storage,
        ReplaceOutcome::CommittedNotSynced(io::Error::other("injected directory sync failure")),
    );

    assert!(!durable);
    assert_eq!(storage.mode(), StorageMode::Plaintext);
    assert!(storage.is_automatic_fallback());
    assert_eq!(storage.vault_error(), Some("injected vault outage"));
    assert!(
        storage
            .warning()
            .is_some_and(|warning| warning.contains("directory sync failure"))
    );
}

#[cfg(unix)]
#[test]
fn automatic_plaintext_save_reports_why_the_vault_failed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let entries = collect_entries([("discord", "token=secret")]).unwrap();

    let storage = save_with_vault(
        &path,
        &entries,
        VaultAccess::Unavailable(io::Error::new(
            io::ErrorKind::NotConnected,
            "injected vault outage",
        )),
    )
    .unwrap();

    assert_eq!(storage.mode(), StorageMode::Plaintext);
    assert!(storage.is_automatic_fallback());
    assert_eq!(storage.vault_error(), Some("injected vault outage"));
    assert_eq!(fs::read_to_string(path).unwrap(), "discord:token=secret\n");
}

#[cfg(unix)]
#[test]
fn explicit_storage_choice_round_trips_between_vault_and_plaintext() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let entries = collect_entries([("discord", "token=secret")]).unwrap();

    let storage = save_with_vault(&path, &entries, VaultAccess::Available(&vault)).unwrap();
    assert_eq!(storage.mode(), StorageMode::SystemVault);
    assert_eq!(vault.entries.lock().unwrap().len(), 1);

    let storage = save_as_plaintext(&path, &entries, VaultAccess::Available(&vault)).unwrap();
    assert_eq!(storage.mode(), StorageMode::Plaintext);
    assert!(!storage.is_automatic_fallback());
    let plaintext = fs::read_to_string(&path).unwrap();
    assert!(plaintext.starts_with(PLAINTEXT_STORE_HEADER));
    assert!(plaintext.contains("token=secret"));
    assert!(vault.entries.lock().unwrap().is_empty());
    assert!(!vault_journal_path(&path).unwrap().exists());

    let (loaded, storage) =
        load_with_vault_status(&path, VaultAccess::Available(&vault), |name, auth| {
            Some((name.to_owned(), auth.to_owned()))
        })
        .unwrap();
    assert_eq!(loaded, vec![("discord".into(), "token=secret".into())]);
    assert_eq!(storage.mode(), StorageMode::Plaintext);
    assert!(!storage.is_automatic_fallback());
    assert!(vault.entries.lock().unwrap().is_empty());

    let storage = save_as_system_vault(&path, &entries, VaultAccess::Available(&vault)).unwrap();
    assert_eq!(storage.mode(), StorageMode::SystemVault);
    let manifest = fs::read_to_string(&path).unwrap();
    assert!(manifest.starts_with(VAULT_MANIFEST_HEADER));
    assert!(!manifest.contains("token=secret"));
    assert_eq!(vault.entries.lock().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn committed_plaintext_switch_retries_failed_vault_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let entries = collect_entries([("discord", "token=secret")]).unwrap();
    save_with_vault(&path, &entries, VaultAccess::Available(&vault)).unwrap();
    vault.fail_deletes.store(true, Ordering::Release);

    let storage = save_as_plaintext(&path, &entries, VaultAccess::Available(&vault)).unwrap();

    assert_eq!(storage.mode(), StorageMode::Plaintext);
    assert!(storage.vault_error().is_none());
    assert!(
        storage
            .warning()
            .is_some_and(|warning| warning.contains("could not be removed"))
    );
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .starts_with(PLAINTEXT_STORE_HEADER)
    );
    assert_eq!(vault.entries.lock().unwrap().len(), 1);
    assert!(vault_journal_path(&path).unwrap().exists());

    let (_, storage) =
        load_with_vault_status::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap();
    assert_eq!(storage.mode(), StorageMode::Plaintext);
    assert!(storage.vault_error().is_none());
    assert!(
        storage
            .warning()
            .is_some_and(|warning| warning.contains("could not be removed"))
    );

    let replacement = collect_entries([("discord", "token=new")]).unwrap();
    let storage = save_with_vault(&path, &replacement, VaultAccess::Available(&vault)).unwrap();
    assert_eq!(storage.mode(), StorageMode::Plaintext);
    assert!(storage.vault_error().is_none());
    assert!(storage.warning().is_some());
    assert!(fs::read_to_string(&path).unwrap().contains("token=new"));

    vault.fail_deletes.store(false, Ordering::Release);
    let (_, storage) =
        load_with_vault_status::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap();
    assert_eq!(storage.mode(), StorageMode::Plaintext);
    assert!(storage.vault_error().is_none());
    assert!(storage.warning().is_none());
    assert!(vault.entries.lock().unwrap().is_empty());
    assert!(!vault_journal_path(&path).unwrap().exists());
}

#[test]
fn malformed_legacy_file_is_a_hard_error_and_is_not_migrated() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let plaintext = "discord:token=valid\nmalformed\n";
    fs::write(&path, plaintext).unwrap();
    let vault = MemoryVault::default();

    let error =
        load_with_vault::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read_to_string(path).unwrap(), plaintext);
    assert!(vault.entries.lock().unwrap().is_empty());
}

#[test]
fn unknown_vault_manifest_version_is_a_hard_error() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    fs::write(
        &path,
        "# generic_messenger credential vault v2\ndiscord:opaque\n",
    )
    .unwrap();
    let vault = MemoryVault::default();

    let error =
        load_with_vault::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn manifest_capacity_is_checked_before_any_vault_write() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    // UTF-8 name fits the legacy byte bound but expands past the manifest
    // bound when percent-encoded.
    let name = "π".repeat(300_000);
    let entries = collect_entries([(name, "")]).unwrap();
    let vault = MemoryVault::default();

    assert!(save_to_vault(&path, &entries, &[], &vault).is_err());
    assert!(vault.entries.lock().unwrap().is_empty());
    assert!(!path.exists());
}

#[test]
fn vault_manifest_never_degrades_to_partial_or_plaintext_load() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let entries = collect_entries([("discord", "token=secret")]).unwrap();
    save_with_vault(&path, &entries, VaultAccess::Available(&vault)).unwrap();
    vault.fail_reads.store(true, Ordering::Release);

    let error =
        load_with_vault::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap_err();
    assert!(error.to_string().contains("injected vault read failure"));
    let unavailable = load_with_vault::<()>(
        &path,
        VaultAccess::Unavailable(io::Error::new(io::ErrorKind::NotConnected, "vault offline")),
        |_, _| None,
    )
    .unwrap_err();
    assert_eq!(unavailable.kind(), io::ErrorKind::NotConnected);
    assert!(!fs::read_to_string(path).unwrap().contains("secret"));
}

#[test]
fn vault_save_replaces_references_then_removes_old_secrets() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let first = collect_entries([("discord", "token=old")]).unwrap();
    save_with_vault(&path, &first, VaultAccess::Available(&vault)).unwrap();
    let old_slots = match read_disk_store(&path).unwrap() {
        DiskStore::VaultManifest(entries) => entries,
        _ => panic!("save did not write a vault manifest"),
    };

    let second = collect_entries([("discord", "token=new"), ("future", "opaque=value")]).unwrap();
    save_with_vault(&path, &second, VaultAccess::Available(&vault)).unwrap();

    let stored = vault.entries.lock().unwrap();
    assert_eq!(stored.len(), 2);
    for old in old_slots {
        assert!(!stored.contains_key(&old.slot));
    }
    drop(stored);
    let restored = load_with_vault(&path, VaultAccess::Available(&vault), |name, auth| {
        Some((name.to_owned(), auth.to_owned()))
    })
    .unwrap();
    assert_eq!(
        restored,
        vec![
            ("discord".into(), "token=new".into()),
            ("future".into(), "opaque=value".into())
        ]
    );
}

#[test]
fn migrated_store_is_never_downgraded_when_vault_write_fails() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let initial = collect_entries([("discord", "token=old")]).unwrap();
    save_with_vault(&path, &initial, VaultAccess::Available(&vault)).unwrap();
    let manifest_before = fs::read_to_string(&path).unwrap();
    vault.fail_writes.store(true, Ordering::Release);
    let replacement = collect_entries([("discord", "token=new")]).unwrap();

    assert!(save_with_vault(&path, &replacement, VaultAccess::Available(&vault)).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), manifest_before);
    assert!(!manifest_before.contains("token="));
}

#[test]
fn failed_manifest_commit_rolls_back_new_vault_entries() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    fs::create_dir(&path).unwrap();
    let vault = MemoryVault::default();
    let entries = collect_entries([("discord", "token=secret")]).unwrap();

    assert!(save_to_vault(&path, &entries, &[], &vault).is_err());
    assert!(vault.entries.lock().unwrap().is_empty());
}

#[test]
fn unusable_new_vault_fails_closed_without_plaintext() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    vault.fail_writes.store(true, Ordering::Release);
    let entries = collect_entries([("discord", "token=secret")]).unwrap();

    assert!(save_with_vault(&path, &entries, VaultAccess::Available(&vault)).is_err());

    assert!(!path.exists());
    assert!(vault.entries.lock().unwrap().is_empty());
    assert!(!vault_journal_path(&path).unwrap().exists());
}

#[test]
fn unavailable_vault_updates_existing_legacy_store() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    fs::write(&path, "discord:token=old\n").unwrap();
    let entries = collect_entries([("discord", "token=new")]).unwrap();

    save_with_vault(
        &path,
        &entries,
        VaultAccess::Unavailable(io::Error::new(
            io::ErrorKind::NotConnected,
            "vault unavailable",
        )),
    )
    .unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "discord:token=new\n");
}

#[cfg(unix)]
#[test]
fn versioned_save_creates_owner_only_plaintext_when_vault_is_unavailable() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let loaded = load_versioned::<()>(&path, |_, _| None).unwrap();
    assert_eq!(loaded.storage.mode(), StorageMode::Plaintext);
    assert!(loaded.storage.is_automatic_fallback());

    let saved =
        save_versioned_with_status(&path, &loaded.revision, [("discord", "token=secret")]).unwrap();

    assert_eq!(saved.storage.mode(), StorageMode::Plaintext);
    assert!(saved.storage.is_automatic_fallback());
    assert!(saved.storage.vault_error().is_some());
    assert_eq!(fs::read_to_string(&path).unwrap(), "discord:token=secret\n");
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn versioned_explicit_plaintext_choice_persists_across_loads() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let loaded = load_versioned::<()>(&path, |_, _| None).unwrap();

    let saved = save_versioned_as(
        &path,
        &loaded.revision,
        [("discord", "token=secret")],
        StorageMode::Plaintext,
    )
    .unwrap();

    assert_eq!(saved.storage.mode(), StorageMode::Plaintext);
    assert!(!saved.storage.is_automatic_fallback());
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .starts_with(PLAINTEXT_STORE_HEADER)
    );
    let reloaded =
        load_versioned(&path, |name, auth| Some((name.to_owned(), auth.to_owned()))).unwrap();
    assert_eq!(reloaded.storage.mode(), StorageMode::Plaintext);
    assert!(!reloaded.storage.is_automatic_fallback());
    assert_eq!(
        reloaded.values,
        vec![("discord".to_owned(), "token=secret".to_owned())]
    );
}

#[cfg(not(unix))]
#[test]
fn unavailable_vault_does_not_create_a_plaintext_store() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let entries = collect_entries([("discord", "token=secret")]).unwrap();

    let error = save_with_vault(
        &path,
        &entries,
        VaultAccess::Unavailable(io::Error::new(
            io::ErrorKind::NotConnected,
            "vault unavailable",
        )),
    )
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    assert!(!path.exists());
}

#[test]
fn crash_after_journal_before_first_vault_write_recovers_empty_store() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    write_vault_journal(
        &path,
        &VaultJournal::new(
            vec![ManifestEntry {
                name: "discord".into(),
                slot: Uuid::new_v4().to_string(),
            }],
            Vec::new(),
        ),
    )
    .unwrap();

    let loaded = load_with_vault::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap();

    assert!(loaded.is_empty());
    assert!(vault.entries.lock().unwrap().is_empty());
    assert!(!vault_journal_path(&path).unwrap().exists());
}

#[test]
fn uncommitted_vault_journal_sweeps_staged_secret_on_next_load() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let staged = Uuid::new_v4().to_string();
    vault.set(&staged, "orphaned-secret").unwrap();
    write_vault_journal(
        &path,
        &VaultJournal::new(
            vec![ManifestEntry {
                name: "discord".into(),
                slot: staged,
            }],
            Vec::new(),
        ),
    )
    .unwrap();

    let loaded = load_with_vault::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap();

    assert!(loaded.is_empty());
    assert!(vault.entries.lock().unwrap().is_empty());
    assert!(!vault_journal_path(&path).unwrap().exists());
}

#[test]
fn committed_vault_journal_sweeps_previous_generation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let previous = Uuid::new_v4().to_string();
    let staged = Uuid::new_v4().to_string();
    vault.set(&previous, "old-secret").unwrap();
    vault.set(&staged, "new-secret").unwrap();
    write_vault_journal(
        &path,
        &VaultJournal::new(
            vec![ManifestEntry {
                name: "discord".into(),
                slot: staged.clone(),
            }],
            vec![previous.clone()],
        ),
    )
    .unwrap();
    assert!(matches!(
        write_manifest(
            &path,
            &[ManifestEntry {
                name: "discord".into(),
                slot: staged,
            }]
        )
        .unwrap(),
        ReplaceOutcome::Durable
    ));

    let loaded = load_with_vault(&path, VaultAccess::Available(&vault), |name, auth| {
        Some((name.to_owned(), auth.to_owned()))
    })
    .unwrap();

    assert_eq!(loaded, vec![("discord".into(), "new-secret".into())]);
    assert!(!vault.entries.lock().unwrap().contains_key(&previous));
    assert!(!vault_journal_path(&path).unwrap().exists());
}

#[test]
fn version_one_slot_only_journal_remains_recoverable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let previous = Uuid::new_v4().to_string();
    let staged = Uuid::new_v4().to_string();
    vault.set(&previous, "old-secret").unwrap();
    vault.set(&staged, "new-secret").unwrap();
    fs::write(
        vault_journal_path(&path).unwrap(),
        format!("{VAULT_JOURNAL_HEADER_V1}\nstaged:{staged}\nprevious:{previous}\n"),
    )
    .unwrap();
    write_manifest(
        &path,
        &[ManifestEntry {
            name: "discord".into(),
            slot: staged,
        }],
    )
    .unwrap();

    let loaded = load_with_vault(&path, VaultAccess::Available(&vault), |name, auth| {
        Some((name.to_owned(), auth.to_owned()))
    })
    .unwrap();

    assert_eq!(loaded, vec![("discord".into(), "new-secret".into())]);
    assert!(!vault.entries.lock().unwrap().contains_key(&previous));
    assert!(!vault_journal_path(&path).unwrap().exists());
}

#[test]
fn reordered_manifest_is_not_accepted_as_a_committed_journal() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let first = Uuid::new_v4().to_string();
    let second = Uuid::new_v4().to_string();
    vault.set(&first, "first-secret").unwrap();
    vault.set(&second, "second-secret").unwrap();
    write_vault_journal(
        &path,
        &VaultJournal::new(
            vec![
                ManifestEntry {
                    name: "discord".into(),
                    slot: first.clone(),
                },
                ManifestEntry {
                    name: "future".into(),
                    slot: second.clone(),
                },
            ],
            Vec::new(),
        ),
    )
    .unwrap();
    write_manifest(
        &path,
        &[
            ManifestEntry {
                name: "discord".into(),
                slot: second,
            },
            ManifestEntry {
                name: "future".into(),
                slot: first,
            },
        ],
    )
    .unwrap();

    let error =
        load_with_vault::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(vault.entries.lock().unwrap().len(), 2);
    assert!(vault_journal_path(&path).unwrap().exists());
}

#[test]
fn renamed_manifest_is_not_accepted_as_a_committed_v2_journal() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let first = Uuid::new_v4().to_string();
    let second = Uuid::new_v4().to_string();
    vault.set(&first, "first-secret").unwrap();
    vault.set(&second, "second-secret").unwrap();
    write_vault_journal(
        &path,
        &VaultJournal::new(
            vec![
                ManifestEntry {
                    name: "discord".into(),
                    slot: first.clone(),
                },
                ManifestEntry {
                    name: "future".into(),
                    slot: second.clone(),
                },
            ],
            Vec::new(),
        ),
    )
    .unwrap();
    write_manifest(
        &path,
        &[
            ManifestEntry {
                name: "future".into(),
                slot: first,
            },
            ManifestEntry {
                name: "discord".into(),
                slot: second,
            },
        ],
    )
    .unwrap();

    let error =
        load_with_vault::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(vault.entries.lock().unwrap().len(), 2);
    assert!(vault_journal_path(&path).unwrap().exists());
}

#[test]
fn failed_journal_cleanup_is_retained_and_retried() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let vault = MemoryVault::default();
    let staged = Uuid::new_v4().to_string();
    vault.set(&staged, "orphaned-secret").unwrap();
    write_vault_journal(
        &path,
        &VaultJournal::new(
            vec![ManifestEntry {
                name: "discord".into(),
                slot: staged.clone(),
            }],
            Vec::new(),
        ),
    )
    .unwrap();
    vault.fail_deletes.store(true, Ordering::Release);

    let error =
        load_with_vault::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap_err();
    assert!(error.to_string().contains("injected vault delete failure"));
    assert!(vault.entries.lock().unwrap().contains_key(&staged));
    assert!(vault_journal_path(&path).unwrap().exists());

    vault.fail_deletes.store(false, Ordering::Release);
    let loaded = load_with_vault::<()>(&path, VaultAccess::Available(&vault), |_, _| None).unwrap();
    assert!(loaded.is_empty());
    assert!(!vault.entries.lock().unwrap().contains_key(&staged));
    assert!(!vault_journal_path(&path).unwrap().exists());
}

#[test]
fn stale_atomic_write_files_are_removed_under_store_lock() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let store_temp = root.path().join(".LoginInfo-Ab12Cd.tmp");
    let journal_temp = root.path().join(".LoginInfo.vault-journal-Zx98Yw.tmp");
    let unrelated = root.path().join(".LoginInfo-not-ours.tmp");
    fs::write(&path, "discord:token=committed\n").unwrap();
    for candidate in [&store_temp, &journal_temp, &unrelated] {
        fs::write(candidate, "discord:token=interrupted\n").unwrap();
        set_private_permissions(&File::open(candidate).unwrap()).unwrap();
    }

    drop(lock_store(&path).unwrap());
    let vault = MemoryVault::default();
    let loaded = load_with_vault(&path, VaultAccess::Available(&vault), |name, auth| {
        Some((name.to_owned(), auth.to_owned()))
    })
    .unwrap();

    assert_eq!(
        loaded,
        vec![("discord".to_owned(), "token=committed".to_owned())]
    );
    assert!(!store_temp.exists());
    assert!(!journal_temp.exists());
    assert!(unrelated.exists());
}

#[cfg(unix)]
#[test]
fn unsafe_symlink_temp_candidate_is_not_removed() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let target = root.path().join("keep-me");
    let candidate = root.path().join(".LoginInfo-Ab12Cd.tmp");
    fs::write(&target, "unrelated").unwrap();
    symlink(&target, &candidate).unwrap();

    drop(lock_store(&path).unwrap());

    assert!(candidate.exists());
    assert_eq!(fs::read_to_string(target).unwrap(), "unrelated");
}

#[test]
fn lock_holder_child_process() {
    let Some(path) = std::env::var_os("LOGIN_STORE_LOCK_TEST_PATH") else {
        return;
    };
    let ready = std::env::var_os("LOGIN_STORE_LOCK_TEST_READY").unwrap();
    let _lock = lock_store(std::path::Path::new(&path)).unwrap();
    fs::write(ready, b"locked").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn killing_process_releases_kernel_store_lock() {
    use std::{process::Command, time::Duration};

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    let ready = root.path().join("ready");
    fs::write(&path, "discord:token=committed\n").unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("store::tests::lock_holder_child_process")
        .arg("--exact")
        .arg("--nocapture")
        .env("LOGIN_STORE_LOCK_TEST_PATH", &path)
        .env("LOGIN_STORE_LOCK_TEST_READY", &ready)
        .spawn()
        .unwrap();

    let mut locked = false;
    for _ in 0..500 {
        if ready.exists() {
            locked = true;
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("lock-holder child exited before locking: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !locked {
        let _ = child.kill();
        let _ = child.wait();
        panic!("lock-holder child did not acquire lock");
    }

    child.kill().unwrap();
    child.wait().unwrap();
    drop(lock_store(&path).unwrap());
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "discord:token=committed\n"
    );
}

#[test]
fn stale_versioned_save_cannot_overwrite_newer_process() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    fs::write(&path, "discord:token=initial\n").unwrap();
    let first = load_versioned::<()>(&path, |_, _| None).unwrap();
    let stale = load_versioned::<()>(&path, |_, _| None).unwrap();

    let current = save_versioned(&path, &first.revision, [("discord", "token=new")]).unwrap();
    let error = save_versioned(&path, &stale.revision, [("discord", "token=stale")]).unwrap_err();

    assert_ne!(current, stale.revision);
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(fs::read_to_string(path).unwrap(), "discord:token=new\n");
}

#[test]
fn save_atomically_replaces_previous_contents() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    fs::write(&path, "old:value\nold:stale\n").unwrap();

    save(&path, [("discord", "token=new")]).unwrap();

    assert_eq!(fs::read_to_string(path).unwrap(), "discord:token=new\n");
}

#[test]
fn invalid_or_oversized_credentials_do_not_replace_existing_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    fs::write(&path, "discord:token=old\n").unwrap();

    assert!(save(&path, [("discord\nother", "token=new")]).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "discord:token=old\n");

    let oversized = "x".repeat(MAX_LOGIN_FILE_BYTES as usize);
    assert!(save(&path, [("discord", oversized)]).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "discord:token=old\n");
}

#[test]
fn oversized_login_file_is_rejected_before_parsing() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    File::create(&path)
        .unwrap()
        .set_len(MAX_LOGIN_FILE_BYTES + 1)
        .unwrap();

    assert!(load::<()>(&path, |_, _| None).is_err());
}

#[cfg(unix)]
#[test]
fn writable_store_parent_is_rejected() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("unsafe");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
    let path = parent.join("LoginInfo");

    let error = load::<()>(&path, |_, _| None).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(!store_lock_path(&path).unwrap().exists());
}

#[cfg(unix)]
#[test]
fn writable_store_ancestor_is_rejected() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let unsafe_ancestor = root.path().join("unsafe");
    let protected_parent = unsafe_ancestor.join("private");
    fs::create_dir_all(&protected_parent).unwrap();
    fs::set_permissions(&unsafe_ancestor, fs::Permissions::from_mode(0o777)).unwrap();
    fs::set_permissions(&protected_parent, fs::Permissions::from_mode(0o700)).unwrap();

    let error = load::<()>(&protected_parent.join("LoginInfo"), |_, _| None).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[cfg(unix)]
#[test]
fn symlinked_store_parent_is_rejected() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    let link = root.path().join("linked");
    fs::create_dir(&target).unwrap();
    symlink(target, &link).unwrap();

    let error = load::<()>(&link.join("LoginInfo"), |_, _| None).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[cfg(unix)]
#[test]
fn symlinked_store_ancestor_is_rejected() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    let nested = target.join("private");
    let link = root.path().join("linked");
    fs::create_dir_all(&nested).unwrap();
    symlink(target, &link).unwrap();

    let error = load::<()>(&link.join("private/LoginInfo"), |_, _| None).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[cfg(unix)]
#[test]
fn login_file_symlink_is_rejected() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    let link = root.path().join("LoginInfo");
    fs::write(&target, "discord:token=secret\n").unwrap();
    symlink(target, &link).unwrap();

    assert!(load::<()>(&link, |_, _| None).is_err());
}

#[cfg(unix)]
#[test]
fn loaded_credentials_are_restricted_to_owner() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    fs::write(&path, "discord:token=secret\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    load::<()>(&path, |_, _| None).unwrap();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn saved_credentials_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("LoginInfo");
    fs::write(&path, "discord:token=old\n").unwrap();
    save(&path, [("discord", "token=secret")]).unwrap();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
