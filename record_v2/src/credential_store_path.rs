//! Platform-standard credential-store path selection.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

const STORE_FILE_NAME: &str = "LoginInfo";
const JOURNAL_SUFFIX: &str = ".vault-journal";

fn sidecar_path(path: &Path, suffix: &str) -> io::Result<PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "credential store path has no file name",
        )
    })?;
    let mut sidecar = OsString::from(file_name);
    sidecar.push(suffix);
    Ok(path.with_file_name(sidecar))
}

fn has_committed_or_recoverable_state(path: &Path) -> io::Result<bool> {
    Ok(path.try_exists()? || sidecar_path(path, JOURNAL_SUFFIX)?.try_exists()?)
}

fn select_path(preferred: PathBuf, legacy: PathBuf) -> io::Result<PathBuf> {
    if preferred == legacy {
        return Ok(preferred);
    }
    let preferred_exists = has_committed_or_recoverable_state(&preferred)?;
    let legacy_exists = has_committed_or_recoverable_state(&legacy)?;
    match (preferred_exists, legacy_exists) {
        (true, true) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "credential stores exist at both {} and {}; refusing ambiguous startup",
                preferred.display(),
                legacy.display()
            ),
        )),
        (true, false) | (false, false) => Ok(preferred),
        (false, true) => Ok(legacy),
    }
}

/// New installs use persistent per-user state. Existing working-directory stores
/// remain in place until an explicit migration can move them without guessing
/// between two potentially divergent copies.
pub(crate) fn resolve() -> io::Result<PathBuf> {
    let project =
        ProjectDirs::from("io.github", "mroute", "generic_messenger").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "operating system did not provide a per-user application directory",
            )
        })?;
    let preferred_dir = project
        .state_dir()
        .unwrap_or_else(|| project.data_local_dir());
    let preferred = preferred_dir.join(STORE_FILE_NAME);
    let legacy = std::env::current_dir()?.join(STORE_FILE_NAME);
    select_path(preferred, legacy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_store_uses_preferred_user_state_path() {
        let root = tempfile::tempdir().unwrap();
        let preferred = root.path().join("state/LoginInfo");
        let legacy = root.path().join("work/LoginInfo");

        assert_eq!(select_path(preferred.clone(), legacy).unwrap(), preferred);
    }

    #[test]
    fn existing_legacy_store_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let preferred = root.path().join("state/LoginInfo");
        let legacy = root.path().join("work/LoginInfo");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "legacy").unwrap();

        assert_eq!(select_path(preferred, legacy.clone()).unwrap(), legacy);
    }

    #[test]
    fn interrupted_legacy_journal_counts_as_recoverable_state() {
        let root = tempfile::tempdir().unwrap();
        let preferred = root.path().join("state/LoginInfo");
        let legacy = root.path().join("work/LoginInfo");
        let journal = sidecar_path(&legacy, JOURNAL_SUFFIX).unwrap();
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(journal, "pending").unwrap();

        assert_eq!(select_path(preferred, legacy.clone()).unwrap(), legacy);
    }

    #[test]
    fn two_store_locations_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let preferred = root.path().join("state/LoginInfo");
        let legacy = root.path().join("work/LoginInfo");
        std::fs::create_dir_all(preferred.parent().unwrap()).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&preferred, "new").unwrap();
        std::fs::write(&legacy, "old").unwrap();

        let error = select_path(preferred, legacy).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }
}
