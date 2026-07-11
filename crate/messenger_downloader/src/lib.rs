//! Shared cache-backed HTTP download primitives for messenger adapters.
//!
//! Interface crates still own their typed CDN image enums, because URL and
//! cache-category rules are platform-specific. This crate owns the common body
//! lifecycle: HTTP fetches, Discord-style 429 retry, JSON decoding, cache hits,
//! and atomic download-then-rename writes.

use std::{
    error::Error,
    path::{Path, PathBuf},
    time::Duration,
};

use facet::Facet;
use futures_timer::Delay;
use messenger_interface::types::{CacheCategory, TEMP_FILE_SUFFIX, cache_dir};
use surf::{RequestBuilder, StatusCode};
use tracing::{error, warn};

pub type DownloadError = Box<dyn Error + Send + Sync>;
pub type DownloadResult<T> = std::result::Result<T, DownloadError>;

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
    /// call sites can surface useful server messages.
    pub async fn fetch(
        req: impl Fn() -> RequestBuilder,
        headers: Vec<(&str, String)>,
    ) -> DownloadResult<Self> {
        const MAX_ATTEMPTS: u32 = 3;

        for attempt in 1..=MAX_ATTEMPTS {
            let mut request = req();
            for (key, value) in &headers {
                request = request.header(*key, value.clone());
            }
            let request = request.build();
            let method = request.method().to_string();
            let url = request.url().to_string();

            let mut response = surf::client().send(request).await.map_err(|err| {
                error!("HTTP request failed for {method} {url}: {err}");
                err
            })?;
            let status = response.status();

            if status == StatusCode::TooManyRequests && attempt < MAX_ATTEMPTS {
                let retry_after = response
                    .header("Retry-After")
                    .and_then(|values| values.last().as_str().parse::<f64>().ok())
                    .unwrap_or(1.0);
                warn!(
                    "Rate limited {method} {url} (attempt {attempt}/{MAX_ATTEMPTS}); retrying in {retry_after}s"
                );
                Delay::new(Duration::from_secs_f64(retry_after)).await;
                continue;
            }

            if !status.is_success() {
                let body = response.body_string().await.unwrap_or_default();
                error!("HTTP request failed for {method} {url} with status {status:?}: {body}");
                return Err(surf::Error::from_str(status, format!("HTTP {status}: {body}")).into());
            }

            return Ok(Fetch(Fresh(response)));
        }

        unreachable!("the final attempt always returns")
    }

    /// Persist the response body to `file_path`, transitioning to
    /// [`Fetch<Cached>`].
    pub async fn cache(self, file_path: PathBuf) -> DownloadResult<Fetch<Cached>> {
        let bytes = self.0.bytes().await?;
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
        Ok(self.0.body_bytes().await?)
    }
}

impl Body for Cached {
    async fn bytes(self) -> DownloadResult<Vec<u8>> {
        Ok(async_fs::read(self.0).await?)
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
    /// performs the request and persists the body atomically.
    pub async fn cached_fetch(
        platform: &str,
        req: impl Fn() -> RequestBuilder,
        headers: Vec<(&str, String)>,
        category: CacheCategory,
        file_name: &str,
    ) -> DownloadResult<Self> {
        let file_path = cache_dir(category, platform).join(file_name);
        if async_fs::metadata(&file_path).await.is_ok() {
            return Ok(Fetch(Cached(file_path)));
        }

        Fetch::<Fresh>::fetch(req, headers)
            .await?
            .cache(file_path)
            .await
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

    let tmp = tempfile::Builder::new()
        .suffix(TEMP_FILE_SUFFIX)
        .tempfile_in(parent)?
        .into_temp_path();

    async_fs::write(&tmp, &bytes).await?;
    async_fs::rename(&tmp, path).await?;
    tmp.keep()?;

    Ok(())
}
