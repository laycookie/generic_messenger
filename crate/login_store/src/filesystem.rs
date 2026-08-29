//! Secure paths, interprocess locking, revisions, and atomic replacement.

use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Read, Write},
    path::Path,
};

use sha2::{Digest as _, Sha256};
use tracing::warn;
use zeroize::Zeroize as _;

use crate::{MAX_LOGIN_FILE_BYTES, StoreRevision};

const ATOMIC_TEMP_SUFFIX: &str = ".tmp";
const ATOMIC_TEMP_RANDOM_BYTES: usize = 12;
const LEGACY_ATOMIC_TEMP_RANDOM_BYTES: usize = 6;

pub(crate) struct StoreLock(File);

impl Drop for StoreLock {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            warn!(error = %error, "Unable to release credential-store lock");
        }
    }
}

pub(crate) fn store_sidecar_path(path: &Path, suffix: &str) -> io::Result<std::path::PathBuf> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "credential store path has no file name",
        )
    })?;
    let mut sidecar_name = OsString::from(file_name);
    sidecar_name.push(suffix);
    Ok(path.with_file_name(sidecar_name))
}

pub(crate) fn store_lock_path(path: &Path) -> io::Result<std::path::PathBuf> {
    store_sidecar_path(path, ".lock")
}

pub(crate) fn vault_journal_path(path: &Path) -> io::Result<std::path::PathBuf> {
    store_sidecar_path(path, ".vault-journal")
}

fn atomic_temp_prefix(path: &Path) -> io::Result<OsString> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "credential store path has no file name",
        )
    })?;
    let mut prefix = OsString::from(".");
    prefix.push(file_name);
    prefix.push("-");
    Ok(prefix)
}

fn is_atomic_temp_name(name: &OsStr, prefix: &OsStr) -> bool {
    let (Some(name), Some(prefix)) = (name.to_str(), prefix.to_str()) else {
        return false;
    };
    let Some(random) = name
        .strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(ATOMIC_TEMP_SUFFIX))
    else {
        return false;
    };
    matches!(
        random.len(),
        LEGACY_ATOMIC_TEMP_RANDOM_BYTES | ATOMIC_TEMP_RANDOM_BYTES
    ) && random.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[cfg(unix)]
fn safe_stale_temp(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    metadata.file_type().is_file()
        && metadata.uid() == rustix::process::geteuid().as_raw()
        && metadata.mode() & 0o077 == 0
        && metadata.len() <= MAX_LOGIN_FILE_BYTES
}

#[cfg(not(unix))]
fn safe_stale_temp(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_file() && metadata.len() <= MAX_LOGIN_FILE_BYTES
}

fn cleanup_stale_atomic_temps(path: &Path, parent: &Path) -> io::Result<()> {
    let prefixes = [
        atomic_temp_prefix(path)?,
        atomic_temp_prefix(&vault_journal_path(path)?)?,
    ];
    let mut removed = false;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if !prefixes
            .iter()
            .any(|prefix| is_atomic_temp_name(&entry.file_name(), prefix))
        {
            continue;
        }
        let temp_path = entry.path();
        let metadata = fs::symlink_metadata(&temp_path)?;
        if !safe_stale_temp(&metadata) {
            warn!(
                path = %temp_path.display(),
                "Ignoring unsafe credential-store temporary-file candidate"
            );
            continue;
        }
        fs::remove_file(&temp_path)?;
        removed = true;
    }
    if removed {
        sync_parent_directory(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn validate_store_parent(parent: &Path) -> io::Result<()> {
    use std::{
        os::unix::fs::MetadataExt as _,
        path::{Component, PathBuf},
    };

    let effective_uid = rustix::process::geteuid().as_raw();
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != effective_uid
        || metadata.mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "credential-store directory must be owned by this user and not group/world writable",
        ));
    }

    // A protected leaf is insufficient if another user can rename one of its
    // ancestors. Walk the original path rather than only its canonical target
    // so an attacker-owned symlink inside a sticky directory is rejected.
    let absolute_parent = if parent.is_absolute() {
        parent.to_owned()
    } else {
        std::env::current_dir()?.join(parent)
    };
    let mut traversed = PathBuf::new();
    for component in absolute_parent.components() {
        match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "credential-store path may not contain parent traversal",
                ));
            }
            _ => traversed.push(component.as_os_str()),
        }
        let metadata = fs::symlink_metadata(&traversed)?;
        let mode = metadata.mode();
        let writable_by_others = mode & 0o022 != 0;
        let sticky = mode & 0o1000 != 0;
        let trusted_owner = metadata.uid() == effective_uid || metadata.uid() == 0;
        if !metadata.file_type().is_dir()
            || metadata.file_type().is_symlink()
            || !trusted_owner
            || writable_by_others && !sticky
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "credential-store path traverses an untrusted writable or symlinked directory",
            ));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn validate_store_parent(parent: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "credential-store parent must be a real directory",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn open_store_lock(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, open};

    let file = File::from(open(
        path,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?);
    validate_login_file(&file)?;
    Ok(file)
}

#[cfg(windows)]
pub(crate) fn open_store_lock(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Keep the stable lock identity from being unlinked/replaced while any
        // process holds it. Other instances may still open and lock the file.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    validate_login_file(&file)?;
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_store_lock(path: &Path) -> io::Result<File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    validate_login_file(&file)?;
    Ok(file)
}

pub(crate) fn lock_store(path: &Path) -> io::Result<StoreLock> {
    let lock_path = store_lock_path(path)?;
    let parent = lock_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    validate_store_parent(parent)?;
    let file = open_store_lock(&lock_path)?;
    file.lock()?;
    if let Err(error) = cleanup_stale_atomic_temps(path, parent) {
        warn!(
            error = %error,
            "Unable to clean stale credential-store temporary files"
        );
    }
    Ok(StoreLock(file))
}

#[cfg(unix)]
pub(crate) fn open_login_file(path: &Path) -> io::Result<Option<File>> {
    use rustix::fs::{Mode, OFlags, open};
    use rustix::io::Errno;

    let file = match open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    validate_login_file(&file)?;
    Ok(Some(file))
}

#[cfg(windows)]
pub(crate) fn open_login_file(path: &Path) -> io::Result<Option<File>> {
    use std::os::windows::fs::OpenOptionsExt as _;

    // Open the reparse point itself so validation applies to the acquired
    // handle, not to a target swapped between metadata and File::open.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    validate_login_file(&file)?;
    Ok(Some(file))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn open_login_file(path: &Path) -> io::Result<Option<File>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "credential path is not a regular file",
        ));
    }
    let file = File::open(path)?;
    validate_login_file(&file)?;
    Ok(Some(file))
}

pub(crate) fn validate_login_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_LOGIN_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "credential file must be a regular file no larger than {MAX_LOGIN_FILE_BYTES} bytes"
            ),
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if metadata.permissions().mode() & 0o077 != 0 {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

pub(crate) fn store_revision(path: &Path) -> io::Result<StoreRevision> {
    let mut hasher = Sha256::new();
    let Some(file) = open_login_file(path)? else {
        hasher.update(b"missing credential store");
        return Ok(StoreRevision(hasher.finalize().into()));
    };
    hasher.update(b"present credential store\0");
    let mut reader = file.take(MAX_LOGIN_FILE_BYTES + 1);
    let mut total = 0_u64;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if total > MAX_LOGIN_FILE_BYTES {
            buffer[..read].zeroize();
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("credential file exceeds {MAX_LOGIN_FILE_BYTES} byte limit"),
            ));
        }
        hasher.update(&buffer[..read]);
        buffer[..read].zeroize();
    }
    Ok(StoreRevision(hasher.finalize().into()))
}

pub(crate) enum ReplaceOutcome {
    Durable,
    CommittedNotSynced(io::Error),
}

pub(crate) fn atomic_replace(
    path: &Path,
    write_contents: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<ReplaceOutcome> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    validate_store_parent(parent)?;
    let temp_prefix = atomic_temp_prefix(path)?;
    let mut temp = tempfile::Builder::new()
        .prefix(&temp_prefix)
        .suffix(ATOMIC_TEMP_SUFFIX)
        .rand_bytes(ATOMIC_TEMP_RANDOM_BYTES)
        .tempfile_in(parent)?;
    set_private_permissions(temp.as_file())?;
    write_contents(temp.as_file_mut())?;
    temp.as_file_mut().flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(match sync_parent_directory(parent) {
        Ok(()) => ReplaceOutcome::Durable,
        Err(error) => ReplaceOutcome::CommittedNotSynced(error),
    })
}

#[cfg(unix)]
pub(crate) fn set_private_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
pub(crate) fn set_private_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(parent)?
        .sync_all()
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "parent directory synchronization is unavailable on this platform",
    ))
}
