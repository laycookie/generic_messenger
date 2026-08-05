//! Shared cache-backed HTTP download primitives for messenger adapters.
//!
//! Interface crates still own their typed CDN image enums, because URL and
//! cache-category rules are platform-specific. This crate owns the common body
//! lifecycle: HTTP fetches, Discord-style 429 retry, JSON decoding, cache hits,
//! and atomic download-then-rename writes.

use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    fmt, fs,
    future::Future,
    io::{self, Cursor},
    ops::Deref,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
    time::{Duration, SystemTime},
};

use async_lock::Mutex as AsyncMutex;
use facet::Facet;
use futures::{
    future::{Either, select},
    io::{AsyncReadExt as _, AsyncWriteExt as _},
};
use futures_timer::Delay;
use image::{ImageFormat, ImageReader, Limits};
use messenger_interface::types::{
    CacheCategory, TEMP_FILE_SUFFIX, cache_dir, prepare_cache_file_parent, record_cache_access,
    validate_cache_file_parent,
};
use surf::{Client, RequestBuilder, StatusCode};
use tracing::{error, warn};

pub type DownloadError = Box<dyn Error + Send + Sync>;
pub type DownloadResult<T> = Result<T, DownloadError>;

const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 4096;
const MAX_IMAGE_DECODE_BYTES: u64 = 96 * 1024 * 1024;
const MAX_VALIDATED_CACHE_FILES: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

static HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    surf::Config::new()
        .set_timeout(Some(REQUEST_TIMEOUT))
        .set_max_connections_per_host(16)
        .try_into()
        .expect("static Surf client configuration is valid")
});

static CACHE_GATES: LazyLock<Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Eq, PartialEq)]
struct CacheFingerprint {
    len: u64,
    modified: Option<SystemTime>,
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

static VALIDATED_CACHE_FILES: LazyLock<Mutex<ValidatedCacheFiles>> =
    LazyLock::new(|| Mutex::new(ValidatedCacheFiles::default()));

/// Typestate marker: a live, uncached response, read straight from the network.
pub struct Fresh(surf::Response);

/// Typestate marker: a body that lives on disk at this path.
pub struct Cached(PathBuf);

/// A typed HTTP fetch. The state `S` gates which operations are available:
/// [`Fetch<Fresh>`] can be decoded as a body or cached to disk, while
/// [`Fetch<Cached>`] can be read from disk or converted into its cache path.
pub struct Fetch<S>(S);

impl Fetch<Fresh> {
    /// Perform one HTTP request, accepting any 2xx response.
    ///
    /// Retries on 429 up to three attempts while honoring `Retry-After`. Failed
    /// non-2xx responses include the response body in the returned error so API
    /// call sites can surface useful server messages. Headers are held as a
    /// reusable slice, so retries re-iterate entries without cloning them.
    pub async fn fetch<H, K, V>(
        req: impl Fn() -> RequestBuilder,
        headers: H,
    ) -> DownloadResult<Self>
    where
        H: AsRef<[(K, V)]>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        const MAX_ATTEMPTS: u32 = 3;

        for attempt in 1..=MAX_ATTEMPTS {
            let mut request = req();
            for (key, value) in headers.as_ref() {
                request = request.header(key.as_ref(), value.as_ref());
            }
            let request = request.build();
            let method = request.method().to_string();
            let url = request.url().to_string();

            let mut response = with_timeout(HTTP_CLIENT.send(request), REQUEST_TIMEOUT)
                .await
                .map_err(|err| {
                    error!("HTTP request failed for {method} {url}: {err}");
                    err
                })?;
            let status = response.status();

            if status == StatusCode::TooManyRequests && attempt < MAX_ATTEMPTS {
                let retry_after = response
                    .header("Retry-After")
                    .map(|values| values.last().as_str())
                    .map(retry_after_delay)
                    .unwrap_or(DEFAULT_RETRY_DELAY);
                warn!(
                    retry_after_seconds = retry_after.as_secs_f64(),
                    "Rate limited {method} {url} (attempt {attempt}/{MAX_ATTEMPTS})"
                );
                Delay::new(retry_after).await;
                continue;
            }

            if !status.is_success() {
                let body = read_response_bytes(&mut response, MAX_ERROR_BODY_BYTES)
                    .await
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_else(|err| format!("<response body omitted: {err}>"));
                error!("HTTP request failed for {method} {url} with status {status:?}");
                return Err(surf::Error::from_str(status, format!("HTTP {status}: {body}")).into());
            }

            return Ok(Fetch(Fresh(response)));
        }

        unreachable!("the final attempt always returns")
    }

    /// Persist the response body to `file_path`, transitioning to
    /// [`Fetch<Cached>`].
    pub async fn cache(mut self, file_path: PathBuf) -> DownloadResult<Fetch<Cached>> {
        let bytes = read_response_bytes(&mut self.0.0, MAX_BODY_BYTES).await?;
        write_atomic(&file_path, bytes).await?;
        Ok(Fetch(Cached(file_path)))
    }
}

/// The body source behind a [`Fetch`] state, so callers can decode data
/// regardless of whether it came from the network or disk.
#[allow(async_fn_in_trait)]
pub trait Body {
    async fn bytes(self) -> DownloadResult<Vec<u8>>;

    async fn json<T: for<'a> Facet<'a>>(self) -> DownloadResult<T>
    where
        Self: Sized,
    {
        let bytes = self.bytes().await?;
        Ok(facet_json::from_str(std::str::from_utf8(&bytes)?)?)
    }
}

impl Body for Fresh {
    async fn bytes(mut self) -> DownloadResult<Vec<u8>> {
        read_response_bytes(&mut self.0, MAX_BODY_BYTES).await
    }
}

impl Body for Cached {
    async fn bytes(self) -> DownloadResult<Vec<u8>> {
        record_cache_access(&self.0);
        let metadata = async_fs::symlink_metadata(&self.0).await?;
        if !metadata.file_type().is_file() {
            return Err(invalid_cache_data("cache body is not a regular file"));
        }
        if metadata.len() > u64::try_from(MAX_BODY_BYTES).unwrap_or(u64::MAX) {
            return Err(body_too_large(MAX_BODY_BYTES));
        }
        let bytes = async_fs::read(&self.0).await?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(body_too_large(MAX_BODY_BYTES));
        }
        record_cache_access(&self.0);
        Ok(bytes)
    }
}

impl<S: Body> Body for Fetch<S> {
    async fn bytes(self) -> DownloadResult<Vec<u8>> {
        self.0.bytes().await
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
    ) -> DownloadResult<Self>
    where
        H: AsRef<[(K, V)]>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let file_path = safe_cache_path(&cache_dir(category, platform), file_name)?;
        if is_known_valid_cache_file(&file_path).await {
            record_cache_access(&file_path);
            return Ok(Fetch(Cached(file_path)));
        }

        let gate = cache_gate(&file_path);
        let _guard = gate.lock().await;
        if is_valid_cache_file(&file_path).await {
            record_cache_access(&file_path);
            return Ok(Fetch(Cached(file_path)));
        }
        remove_invalid_cache_entry(&file_path).await?;

        let mut fresh = Fetch::<Fresh>::fetch(req, headers).await?;
        let bytes = read_response_bytes(&mut fresh.0.0, MAX_BODY_BYTES).await?;
        let bytes = validate_image_body(bytes).await?;
        prepare_cache_file_parent(&file_path)?;
        write_atomic(&file_path, bytes).await?;
        remember_validated_cache_file(&file_path).await;
        let cached = Fetch(Cached(file_path));
        record_cache_access(&cached.0.0);
        Ok(cached)
    }

    /// The on-disk path of the cached body.
    pub fn into_path(self) -> PathBuf {
        self.0.0
    }
}

/// Write `bytes` to `path` atomically by writing into a sibling temp file and
/// renaming it into place.
pub async fn write_atomic(path: &Path, bytes: Vec<u8>) -> DownloadResult<()> {
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

fn retry_after_delay(value: &str) -> Duration {
    retry_after_delay_at(value, SystemTime::now())
}

fn retry_after_delay_at(value: &str, now: SystemTime) -> Duration {
    let delta_seconds = value
        .parse::<f64>()
        .ok()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        // `Duration::from_secs_f64` panics above its representable range.
        // Clamp untrusted Retry-After input while it is still a float.
        .map(|seconds| Duration::from_secs_f64(seconds.min(MAX_RETRY_DELAY.as_secs_f64())));
    let http_date = || {
        httpdate::parse_http_date(value)
            .ok()
            .map(|deadline| deadline.duration_since(now).unwrap_or(Duration::ZERO))
    };
    delta_seconds
        .or_else(http_date)
        .map(|delay| delay.min(MAX_RETRY_DELAY))
        .unwrap_or(DEFAULT_RETRY_DELAY)
}

async fn with_timeout<F, T, E>(future: F, timeout: Duration) -> DownloadResult<T>
where
    F: Future<Output = Result<T, E>>,
    E: fmt::Display,
{
    match select(Box::pin(future), Box::pin(Delay::new(timeout))).await {
        Either::Left((result, _)) => result
            .map_err(|err| io::Error::other(err.to_string()))
            .map_err(Into::into),
        Either::Right(((), _)) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("HTTP operation exceeded {} seconds", timeout.as_secs()),
        )
        .into()),
    }
}

async fn read_response_bytes(
    response: &mut surf::Response,
    max_bytes: usize,
) -> DownloadResult<Vec<u8>> {
    if response.len().is_some_and(|length| length > max_bytes) {
        return Err(body_too_large(max_bytes));
    }

    let capacity = response.len().unwrap_or(8 * 1024).min(max_bytes);
    let mut bytes = Vec::with_capacity(capacity);
    let limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut body = response.take_body().take(limit);
    let read = body.read_to_end(&mut bytes);
    match select(Box::pin(read), Box::pin(Delay::new(REQUEST_TIMEOUT))).await {
        Either::Left((result, pending_timeout)) => {
            drop(pending_timeout);
            result?;
        }
        Either::Right(((), pending_read)) => {
            drop(pending_read);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "HTTP response body exceeded {} second deadline",
                    REQUEST_TIMEOUT.as_secs()
                ),
            )
            .into());
        }
    }
    if bytes.len() > max_bytes {
        return Err(body_too_large(max_bytes));
    }
    Ok(bytes)
}

fn body_too_large(max_bytes: usize) -> DownloadError {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("HTTP response body exceeds {max_bytes} byte limit"),
    )
    .into()
}

fn safe_cache_path(base: &Path, relative: &str) -> DownloadResult<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache filename must be a non-empty relative path without traversal",
        )
        .into());
    }
    Ok(base.join(relative))
}

/// One strong lease on a per-path singleflight mutex. Final lease removes its
/// dead weak-map entry in O(1), avoiding a full-map retain on every cache miss.
struct CacheGate {
    path: PathBuf,
    gate: Arc<AsyncMutex<()>>,
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

fn cache_gate(path: &Path) -> CacheGate {
    let mut gates = CACHE_GATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(gate) = gates.get(path).and_then(Weak::upgrade) {
        return CacheGate {
            path: path.to_owned(),
            gate,
        };
    }
    let gate = Arc::new(AsyncMutex::new(()));
    gates.insert(path.to_owned(), Arc::downgrade(&gate));
    CacheGate {
        path: path.to_owned(),
        gate,
    }
}

fn cache_fingerprint(metadata: &fs::Metadata) -> Option<CacheFingerprint> {
    (metadata.file_type().is_file()
        && metadata.len() > 0
        && metadata.len() <= u64::try_from(MAX_BODY_BYTES).unwrap_or(u64::MAX))
    .then(|| CacheFingerprint {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

async fn is_known_valid_cache_file(path: &Path) -> bool {
    if validate_cache_file_parent(path).is_err() {
        return false;
    }
    let Ok(metadata) = async_fs::symlink_metadata(path).await else {
        return false;
    };
    let Some(fingerprint) = cache_fingerprint(&metadata) else {
        return false;
    };
    VALIDATED_CACHE_FILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(path, fingerprint)
}

async fn is_valid_cache_file(path: &Path) -> bool {
    if validate_cache_file_parent(path).is_err() {
        return false;
    }
    let metadata = match async_fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    let Some(fingerprint) = cache_fingerprint(&metadata) else {
        return false;
    };
    if VALIDATED_CACHE_FILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(path, fingerprint)
    {
        return true;
    }

    let bytes = match async_fs::read(path).await {
        Ok(bytes) if bytes.len() <= MAX_BODY_BYTES => bytes,
        _ => return false,
    };
    if !has_supported_image_signature(&bytes, bytes.len())
        || !smol::unblock(move || decode_supported_image(&bytes)).await
    {
        return false;
    }
    VALIDATED_CACHE_FILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(path.to_owned(), fingerprint);
    true
}

async fn remember_validated_cache_file(path: &Path) {
    let Ok(metadata) = async_fs::metadata(path).await else {
        return;
    };
    VALIDATED_CACHE_FILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            path.to_owned(),
            CacheFingerprint {
                len: metadata.len(),
                modified: metadata.modified().ok(),
            },
        );
}

async fn remove_invalid_cache_entry(path: &Path) -> DownloadResult<()> {
    VALIDATED_CACHE_FILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(path);
    match validate_cache_file_parent(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let metadata = match async_fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        async_fs::remove_file(path).await?;
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "cache entry exists but is not a regular file",
    )
    .into())
}

fn has_supported_image_signature(bytes: &[u8], total_len: usize) -> bool {
    let png = total_len >= 24
        && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        && bytes.get(12..16) == Some(b"IHDR");
    let jpeg = total_len >= 4 && bytes.starts_with(b"\xff\xd8\xff");
    let gif = total_len >= 13 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"));
    let webp = bytes.starts_with(b"RIFF")
        && bytes.get(8..12) == Some(b"WEBP")
        && bytes
            .get(4..8)
            .and_then(|length| <[u8; 4]>::try_from(length).ok())
            .map(u32::from_le_bytes)
            .and_then(|length| usize::try_from(length).ok())
            .and_then(|length| length.checked_add(8))
            == Some(total_len);
    png || jpeg || gif || webp
}

async fn validate_image_body(bytes: Vec<u8>) -> DownloadResult<Vec<u8>> {
    smol::unblock(move || {
        if decode_supported_image(&bytes) {
            Ok(bytes)
        } else {
            Err(invalid_cache_data(
                "downloaded cache body is not a valid supported image",
            ))
        }
    })
    .await
}

fn decode_supported_image(bytes: &[u8]) -> bool {
    let Ok(mut reader) = ImageReader::new(Cursor::new(bytes)).with_guessed_format() else {
        return false;
    };
    if !matches!(
        reader.format(),
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP)
    ) {
        return false;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
    reader.limits(limits);
    reader.decode().is_ok()
}

fn invalid_cache_data(message: &'static str) -> DownloadError {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_is_finite_and_clamped() {
        assert_eq!(retry_after_delay("NaN"), DEFAULT_RETRY_DELAY);
        assert_eq!(retry_after_delay("-1"), DEFAULT_RETRY_DELAY);
        assert_eq!(retry_after_delay("999999"), MAX_RETRY_DELAY);
        assert_eq!(retry_after_delay("1e300"), MAX_RETRY_DELAY);
        assert_eq!(retry_after_delay("0.25"), Duration::from_millis(250));
    }

    #[test]
    fn retry_after_accepts_http_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let future = httpdate::fmt_http_date(now + Duration::from_secs(12));
        let past = httpdate::fmt_http_date(now - Duration::from_secs(12));
        assert_eq!(retry_after_delay_at(&future, now), Duration::from_secs(12));
        assert_eq!(retry_after_delay_at(&past, now), Duration::ZERO);

        let distant = httpdate::fmt_http_date(now + Duration::from_secs(300));
        assert_eq!(retry_after_delay_at(&distant, now), MAX_RETRY_DELAY);
    }

    #[test]
    fn cache_paths_reject_escape_components() {
        let base = Path::new("cache/platform");
        assert!(safe_cache_path(base, "42/avatar.webp").is_ok());
        assert!(safe_cache_path(base, "../outside").is_err());
        assert!(safe_cache_path(base, "/outside").is_err());
        assert!(safe_cache_path(base, "").is_err());
    }

    #[test]
    fn concurrent_cache_misses_share_gate() {
        let path = Path::new("cache/platform/same.png");
        let first = cache_gate(path);
        let second = cache_gate(path);
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

    #[test]
    fn body_reader_rejects_oversized_declared_body() {
        let mut response: surf::Response = surf::http::Response::new(StatusCode::Ok).into();
        response.set_body(vec![0_u8; 9]);
        let result = futures::executor::block_on(read_response_bytes(&mut response, 8));
        assert!(result.is_err());
    }

    #[test]
    fn cache_validation_decodes_supported_images() {
        for format in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Gif,
            ImageFormat::WebP,
        ] {
            let mut encoded = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(1, 1)
                .write_to(&mut encoded, format)
                .unwrap();
            let encoded = encoded.into_inner();
            assert!(has_supported_image_signature(&encoded, encoded.len()));
            assert!(
                decode_supported_image(&encoded),
                "failed to decode {format:?}"
            );
        }
    }

    #[test]
    fn cache_validation_rejects_html_and_truncated_signatures() {
        assert!(!has_supported_image_signature(
            b"<html>not an image</html>",
            25
        ));
        for truncated in [
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR12345678".as_slice(),
            b"\xff\xd8\xff\xd9".as_slice(),
            b"GIF89a1234567".as_slice(),
            b"RIFF\x04\0\0\0WEBP".as_slice(),
        ] {
            assert!(has_supported_image_signature(truncated, truncated.len()));
            assert!(!decode_supported_image(truncated));
        }
    }

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
