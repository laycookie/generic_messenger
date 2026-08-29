//! Cache coordination, validation index, and typestate transitions.

use std::{
    collections::{HashMap, VecDeque},
    fs, io,
    ops::Deref,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
    time::SystemTime,
};

use async_lock::Mutex as AsyncMutex;
use messenger_interface::types::{
    CacheCategory, cache_dir, prepare_cache_file_parent, record_cache_access,
    validate_cache_file_parent,
};
use surf::RequestBuilder;

use crate::{
    Body, Cached, DownloadError, Fetch, Fresh, http::BodyLimit,
    image_validation::UnvalidatedImageBody, storage::write_atomic,
};

const MAX_VALIDATED_CACHE_FILES: usize = 4096;

static CACHE_GATES: LazyLock<Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Eq, PartialEq)]
struct CacheFingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

impl CacheFingerprint {
    fn from_metadata(metadata: &fs::Metadata) -> Option<Self> {
        let len = metadata.len();
        (metadata.file_type().is_file() && len > 0 && BodyLimit::DOWNLOAD.allows_file(len)).then(
            || Self {
                len,
                modified: metadata.modified().ok(),
            },
        )
    }
}

#[derive(Default)]
struct ValidatedCacheFiles {
    entries: HashMap<PathBuf, CacheFingerprint>,
    order: VecDeque<PathBuf>,
}

impl ValidatedCacheFiles {
    fn contains(&self, path: &Path, fingerprint: CacheFingerprint) -> bool {
        self.entries.get(path) == Some(&fingerprint)
    }

    fn insert(&mut self, path: PathBuf, fingerprint: CacheFingerprint) {
        if let Some(existing) = self.entries.get_mut(&path) {
            *existing = fingerprint;
            return;
        }
        while self.entries.len() >= MAX_VALIDATED_CACHE_FILES {
            let Some(expired) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&expired);
        }
        self.order.push_back(path.clone());
        self.entries.insert(path, fingerprint);
    }

    fn remove(&mut self, path: &Path) {
        self.entries.remove(path);
    }
}

#[derive(Default)]
struct ValidatedCacheIndex(Mutex<ValidatedCacheFiles>);

impl ValidatedCacheIndex {
    fn contains(&self, path: &Path, fingerprint: CacheFingerprint) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(path, fingerprint)
    }

    fn insert(&self, path: PathBuf, fingerprint: CacheFingerprint) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path, fingerprint);
    }

    fn remove(&self, path: &Path) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path);
    }
}

static VALIDATED_CACHE_FILES: LazyLock<ValidatedCacheIndex> =
    LazyLock::new(ValidatedCacheIndex::default);

struct CacheEntry(PathBuf);

impl CacheEntry {
    fn new(base: &Path, relative: &str) -> Result<Self, DownloadError> {
        let relative = Path::new(relative);
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(DownloadError::InvalidCacheFilename);
        }
        Ok(Self(base.join(relative)))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn into_fetch(self) -> Fetch<Cached> {
        record_cache_access(&self.0);
        Fetch(Cached(self.0))
    }

    async fn fingerprint(&self) -> Option<CacheFingerprint> {
        validate_cache_file_parent(self.path()).ok()?;
        let metadata = async_fs::symlink_metadata(self.path()).await.ok()?;
        CacheFingerprint::from_metadata(&metadata)
    }

    async fn is_known_valid(&self) -> bool {
        let Some(fingerprint) = self.fingerprint().await else {
            return false;
        };
        VALIDATED_CACHE_FILES.contains(self.path(), fingerprint)
    }

    async fn is_valid(&self) -> bool {
        let Some(fingerprint) = self.fingerprint().await else {
            return false;
        };
        if VALIDATED_CACHE_FILES.contains(self.path(), fingerprint) {
            return true;
        }

        let bytes = match async_fs::read(self.path()).await {
            Ok(bytes) if BodyLimit::DOWNLOAD.allows_bytes(bytes.len()) => bytes,
            _ => return false,
        };
        if UnvalidatedImageBody::new(bytes).validate().await.is_err() {
            return false;
        }
        VALIDATED_CACHE_FILES.insert(self.0.clone(), fingerprint);
        true
    }

    async fn remember_valid(&self) {
        let Some(fingerprint) = self.fingerprint().await else {
            return;
        };
        VALIDATED_CACHE_FILES.insert(self.0.clone(), fingerprint);
    }

    async fn remove_invalid(&self) -> Result<(), DownloadError> {
        VALIDATED_CACHE_FILES.remove(self.path());
        match validate_cache_file_parent(self.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let metadata = match async_fs::symlink_metadata(self.path()).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            async_fs::remove_file(self.path()).await?;
            return Ok(());
        }
        Err(DownloadError::CacheEntryNotRegularFile)
    }
}

impl Fetch<Fresh> {
    /// Persist the response body to `file_path`, transitioning to
    /// [`Fetch<Cached>`].
    pub async fn cache(mut self, file_path: PathBuf) -> Result<Fetch<Cached>, DownloadError> {
        let bytes = BodyLimit::DOWNLOAD.read_response(&mut self.0.0).await?;
        write_atomic(&file_path, bytes).await?;
        Ok(Fetch(Cached(file_path)))
    }
}

impl Body for Cached {
    async fn bytes(self) -> Result<Vec<u8>, DownloadError> {
        record_cache_access(&self.0);
        let metadata = async_fs::symlink_metadata(&self.0).await?;
        if !metadata.file_type().is_file() {
            return Err(DownloadError::CacheEntryNotRegularFile);
        }
        if !BodyLimit::DOWNLOAD.allows_file(metadata.len()) {
            return Err(BodyLimit::DOWNLOAD.too_large_error());
        }
        let bytes = async_fs::read(&self.0).await?;
        if !BodyLimit::DOWNLOAD.allows_bytes(bytes.len()) {
            return Err(BodyLimit::DOWNLOAD.too_large_error());
        }
        record_cache_access(&self.0);
        Ok(bytes)
    }
}

impl Fetch<Cached> {
    /// Resolve a request against the standard image cache directory.
    ///
    /// On a cache hit this returns without touching the network. On a miss it
    /// performs the request and persists the body atomically. Header inputs
    /// follow [`Fetch::<Fresh>::fetch`]'s reusable slice contract.
    pub async fn cached_fetch<H, K, V>(
        platform: &str,
        req: impl Fn() -> RequestBuilder,
        headers: H,
        category: CacheCategory,
        file_name: &str,
    ) -> Result<Self, DownloadError>
    where
        H: AsRef<[(K, V)]>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let entry = CacheEntry::new(&cache_dir(category, platform), file_name)?;
        if entry.is_known_valid().await {
            return Ok(entry.into_fetch());
        }

        let gate = CacheGate::for_path(entry.path());
        let _guard = gate.lock().await;
        if entry.is_valid().await {
            return Ok(entry.into_fetch());
        }
        entry.remove_invalid().await?;

        let mut fresh = Fetch::<Fresh>::fetch(req, headers).await?;
        let bytes = BodyLimit::DOWNLOAD.read_response(&mut fresh.0.0).await?;
        let bytes = UnvalidatedImageBody::new(bytes)
            .validate()
            .await?
            .into_bytes();
        prepare_cache_file_parent(entry.path())?;
        write_atomic(entry.path(), bytes).await?;
        entry.remember_valid().await;
        Ok(entry.into_fetch())
    }

    /// The on-disk path of the cached body.
    pub fn into_path(self) -> PathBuf {
        self.0.0
    }
}

/// One strong lease on a per-path singleflight mutex. Final lease removes its
/// dead weak-map entry in O(1), avoiding a full-map retain on every cache miss.
struct CacheGate {
    path: PathBuf,
    gate: Arc<AsyncMutex<()>>,
}

impl CacheGate {
    fn for_path(path: &Path) -> Self {
        let path = path.to_owned();
        let mut gates = CACHE_GATES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gate = if let Some(gate) = gates.get(&path).and_then(Weak::upgrade) {
            gate
        } else {
            let gate = Arc::new(AsyncMutex::new(()));
            gates.insert(path.clone(), Arc::downgrade(&gate));
            gate
        };
        Self { path, gate }
    }
}

impl Deref for CacheGate {
    type Target = AsyncMutex<()>;

    fn deref(&self) -> &Self::Target {
        &self.gate
    }
}

impl Drop for CacheGate {
    fn drop(&mut self) {
        let mut gates = CACHE_GATES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::strong_count(&self.gate) == 1
            && gates
                .get(&self.path)
                .is_some_and(|weak| Weak::ptr_eq(weak, &Arc::downgrade(&self.gate)))
        {
            gates.remove(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_entries_reject_escape_components() {
        let base = Path::new("cache/platform");
        assert!(CacheEntry::new(base, "42/avatar.webp").is_ok());
        for invalid in ["../outside", "/outside", ""] {
            assert!(matches!(
                CacheEntry::new(base, invalid),
                Err(DownloadError::InvalidCacheFilename)
            ));
        }
    }

    #[test]
    fn concurrent_cache_misses_share_gate() {
        let path = Path::new("cache/platform/same.png");
        let first = CacheGate::for_path(path);
        let second = CacheGate::for_path(path);
        assert!(Arc::ptr_eq(&first.gate, &second.gate));
        drop(first);
        assert!(CACHE_GATES.lock().unwrap().contains_key(path));
        drop(second);
        assert!(!CACHE_GATES.lock().unwrap().contains_key(path));
    }

    #[test]
    fn validated_cache_index_is_bounded() {
        let mut cache = ValidatedCacheFiles::default();
        let fingerprint = CacheFingerprint {
            len: 1,
            modified: None,
        };
        for index in 0..=MAX_VALIDATED_CACHE_FILES {
            cache.insert(PathBuf::from(format!("image-{index}")), fingerprint);
        }

        assert_eq!(cache.entries.len(), MAX_VALIDATED_CACHE_FILES);
        assert!(!cache.entries.contains_key(Path::new("image-0")));
    }
}
