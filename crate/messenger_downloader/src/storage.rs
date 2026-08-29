//! Atomic file replacement.

use std::path::Path;

use futures::io::AsyncWriteExt as _;
use messenger_interface::types::TEMP_FILE_SUFFIX;

use crate::DownloadError;

/// Write `bytes` to `path` atomically by writing into a sibling temp file and
/// renaming it into place.
pub async fn write_atomic(path: &Path, bytes: Vec<u8>) -> Result<(), DownloadError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    async_fs::create_dir_all(parent).await?;

    let temp = tempfile::Builder::new()
        .suffix(TEMP_FILE_SUFFIX)
        .tempfile_in(parent)?;
    let (file, temp_path) = temp.into_parts();
    let mut file = async_fs::File::from(file);
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    // Let tempfile perform and disarm one platform-correct atomic replace.
    // Renaming separately and then calling `TempPath::keep` reports a false
    // NotFound on Windows because the old temporary path no longer exists.
    temp_path.persist(path).map_err(|err| err.error)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_file_and_leaves_no_temp() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("avatar.png");
        std::fs::write(&path, b"old").unwrap();

        futures::executor::block_on(write_atomic(&path, b"new".to_vec())).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .path()
                .to_string_lossy()
                .ends_with(TEMP_FILE_SUFFIX)
        }));
    }

    #[test]
    fn failed_atomic_replace_cleans_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("occupied-directory");
        std::fs::create_dir(&target).unwrap();

        assert!(futures::executor::block_on(write_atomic(&target, b"new".to_vec())).is_err());
        assert!(target.is_dir());
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .path()
                .to_string_lossy()
                .ends_with(TEMP_FILE_SUFFIX)
        }));
    }
}
