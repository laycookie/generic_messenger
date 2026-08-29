//! HTTP request execution, retry policy, and bounded response handling.

use std::{
    fmt,
    future::Future,
    sync::LazyLock,
    time::{Duration, SystemTime},
};

use futures::{
    future::{Either, select},
    io::AsyncReadExt as _,
};
use futures_timer::Delay;
use surf::{Client, RequestBuilder, StatusCode};
use tracing::{error, warn};

use crate::{Body, DownloadError, Fetch, Fresh};

const REQUEST_TIMEOUT: HttpTimeout = HttpTimeout::from_secs(30);

#[derive(Clone, Copy)]
struct HttpTimeout(Duration);

impl HttpTimeout {
    const fn from_secs(seconds: u64) -> Self {
        Self(Duration::from_secs(seconds))
    }

    const fn duration(self) -> Duration {
        self.0
    }

    async fn run<F, T, E>(self, future: F) -> Result<T, DownloadError>
    where
        F: Future<Output = Result<T, E>>,
        E: Into<DownloadError>,
    {
        match select(Box::pin(future), Box::pin(Delay::new(self.0))).await {
            Either::Left((result, _)) => result.map_err(Into::into),
            Either::Right(((), _)) => Err(DownloadError::RequestTimedOut {
                seconds: self.0.as_secs(),
            }),
        }
    }
}

static HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    surf::Config::new()
        .set_timeout(Some(REQUEST_TIMEOUT.duration()))
        .set_max_connections_per_host(16)
        .try_into()
        .expect("static Surf client configuration is valid")
});

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BodyLimit(usize);

impl BodyLimit {
    pub(super) const DOWNLOAD: Self = Self(16 * 1024 * 1024);
    const ERROR: Self = Self(64 * 1024);

    #[cfg(test)]
    const fn new(max_bytes: usize) -> Self {
        Self(max_bytes)
    }

    pub(super) fn allows_bytes(self, length: usize) -> bool {
        length <= self.0
    }

    pub(super) fn allows_file(self, length: u64) -> bool {
        length <= u64::try_from(self.0).unwrap_or(u64::MAX)
    }

    pub(super) fn too_large_error(self) -> DownloadError {
        DownloadError::ResponseBodyTooLarge { limit: self.0 }
    }

    pub(super) async fn read_response(
        self,
        response: &mut surf::Response,
    ) -> Result<Vec<u8>, DownloadError> {
        if response
            .len()
            .is_some_and(|length| !self.allows_bytes(length))
        {
            return Err(self.too_large_error());
        }

        let capacity = response.len().unwrap_or(8 * 1024).min(self.0);
        let mut bytes = Vec::with_capacity(capacity);
        let read_limit = u64::try_from(self.0).unwrap_or(u64::MAX).saturating_add(1);
        let mut body = response.take_body().take(read_limit);
        let read = body.read_to_end(&mut bytes);
        match select(
            Box::pin(read),
            Box::pin(Delay::new(REQUEST_TIMEOUT.duration())),
        )
        .await
        {
            Either::Left((result, pending_timeout)) => {
                drop(pending_timeout);
                result?;
            }
            Either::Right(((), pending_read)) => {
                drop(pending_read);
                return Err(DownloadError::ResponseBodyTimedOut {
                    seconds: REQUEST_TIMEOUT.duration().as_secs(),
                });
            }
        }
        if !self.allows_bytes(bytes.len()) {
            return Err(self.too_large_error());
        }
        Ok(bytes)
    }
}

impl Fetch<Fresh> {
    /// Perform one HTTP request, accepting any 2xx response.
    ///
    /// Retries on 429 up to three attempts while honoring `Retry-After` and
    /// adding bounded jitter. Failed non-2xx responses include a bounded body
    /// preview in the returned error so API call sites can surface useful server
    /// messages. Headers are held as a reusable slice, so retries re-iterate
    /// entries without cloning them.
    pub async fn fetch<H, K, V>(
        req: impl Fn() -> RequestBuilder,
        headers: H,
    ) -> Result<Self, DownloadError>
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
            let safe_url = DiagnosticUrl::from(request.url());

            let mut response = REQUEST_TIMEOUT
                .run(HTTP_CLIENT.send(request))
                .await
                .map_err(|err| {
                    error!("HTTP request failed for {method} {safe_url}: {err}");
                    err
                })?;
            let status = response.status();

            if status == StatusCode::TooManyRequests && attempt < MAX_ATTEMPTS {
                let retry_delay: Duration = response
                    .header("Retry-After")
                    .map_or_else(RetryDelay::default, |values| {
                        RetryDelay::from_retry_after_at(values.last().as_str(), SystemTime::now())
                    })
                    .with_random_jitter()
                    .into();
                warn!(
                    retry_after_seconds = retry_delay.as_secs_f64(),
                    "Rate limited {method} {safe_url} (attempt {attempt}/{MAX_ATTEMPTS})"
                );
                Delay::new(retry_delay).await;
                continue;
            }

            if !status.is_success() {
                let body = ErrorBodyPreview::read(&mut response).await;
                error!("HTTP request failed for {method} {safe_url} with status {status:?}");
                return Err(surf::Error::from_str(status, format!("HTTP {status}: {body}")).into());
            }

            return Ok(Fetch(Fresh(response)));
        }

        unreachable!("the final attempt always returns")
    }
}

impl Body for Fresh {
    async fn bytes(mut self) -> Result<Vec<u8>, DownloadError> {
        BodyLimit::DOWNLOAD.read_response(&mut self.0).await
    }
}

/// URL representation safe for diagnostics: scheme, host, and port only.
#[derive(Clone, Debug, Eq, PartialEq)]
struct DiagnosticUrl(String);

impl From<&surf::Url> for DiagnosticUrl {
    fn from(url: &surf::Url) -> Self {
        let value = match url.host_str() {
            Some(host) => match url.port() {
                Some(port) => format!("{}://{host}:{port}/", url.scheme()),
                None => format!("{}://{host}/", url.scheme()),
            },
            // Non-hierarchical URLs cannot expose a host independently from
            // potentially sensitive path-like data. Retain only their scheme.
            None => format!("{}:<redacted>", url.scheme()),
        };
        Self(value)
    }
}

impl fmt::Display for DiagnosticUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Bounded delay derived from a `Retry-After` header.
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetryDelay(Duration);

impl RetryDelay {
    const DEFAULT: Self = Self(Duration::from_secs(1));
    const MAX: Self = Self(Duration::from_secs(30));
    const MAX_JITTER: Duration = Duration::from_millis(250);

    fn from_retry_after_at(value: &str, now: SystemTime) -> Self {
        let delta_seconds = value
            .parse::<f64>()
            .ok()
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            // `Duration::from_secs_f64` panics above its representable range.
            // Clamp untrusted Retry-After input while it is still a float.
            .map(|seconds| Duration::from_secs_f64(seconds.min(Self::MAX.0.as_secs_f64())));
        let http_date = || {
            httpdate::parse_http_date(value)
                .ok()
                .map(|deadline| deadline.duration_since(now).unwrap_or(Duration::ZERO))
        };
        delta_seconds
            .or_else(http_date)
            .map(|delay| Self(delay.min(Self::MAX.0)))
            .unwrap_or(Self::DEFAULT)
    }

    fn with_random_jitter(self) -> Self {
        let max_jitter_nanos = u64::try_from(Self::MAX_JITTER.as_nanos())
            .expect("retry jitter bound fits in u64 nanoseconds");
        let range_width = max_jitter_nanos
            .checked_add(1)
            .expect("inclusive retry jitter range fits in u64");
        let rejection_threshold = range_width.wrapping_neg() % range_width;
        let jitter_nanos = loop {
            let candidate = std::random::random::<u64>(..);
            if candidate >= rejection_threshold {
                break candidate % range_width;
            }
        };
        self.with_jitter(Duration::from_nanos(jitter_nanos))
    }

    fn with_jitter(self, jitter: Duration) -> Self {
        Self(
            self.0
                .saturating_add(jitter.min(Self::MAX_JITTER))
                .min(Self::MAX.0),
        )
    }
}

impl Default for RetryDelay {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl From<RetryDelay> for Duration {
    fn from(delay: RetryDelay) -> Self {
        delay.0
    }
}

struct ErrorBodyPreview(String);

impl ErrorBodyPreview {
    const MAX_CHARS: usize = 1024;

    async fn read(response: &mut surf::Response) -> Self {
        match BodyLimit::ERROR.read_response(response).await {
            Ok(bytes) => Self::from_bytes(&bytes),
            Err(error) => Self(format!("<response body omitted: {error}>")),
        }
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let body = String::from_utf8_lossy(bytes);
        let mut chars = body.chars();
        let mut display = String::new();
        for character in chars.by_ref().take(Self::MAX_CHARS) {
            display.extend(character.escape_debug());
        }
        if chars.next().is_some() {
            display.push('…');
        }
        Self(display)
    }
}

impl fmt::Display for ErrorBodyPreview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_url_removes_credentials_path_query_and_fragment() {
        let url = surf::Url::parse(
            "https://api-user:api-password@example.com/image.png?token=secret#fragment",
        )
        .unwrap();

        assert_eq!(
            DiagnosticUrl::from(&url).to_string(),
            "https://example.com/"
        );
    }

    #[test]
    fn diagnostic_url_redacts_non_hierarchical_urls() {
        let url = surf::Url::parse("mailto:bearer@example.com?token=secret").unwrap();

        assert_eq!(DiagnosticUrl::from(&url).to_string(), "mailto:<redacted>");
    }

    #[test]
    fn retry_after_is_finite_and_clamped() {
        let now = SystemTime::UNIX_EPOCH;
        assert_eq!(
            RetryDelay::from_retry_after_at("NaN", now),
            RetryDelay::DEFAULT
        );
        assert_eq!(
            RetryDelay::from_retry_after_at("-1", now),
            RetryDelay::DEFAULT
        );
        assert_eq!(
            RetryDelay::from_retry_after_at("999999", now),
            RetryDelay::MAX
        );
        assert_eq!(
            RetryDelay::from_retry_after_at("1e300", now),
            RetryDelay::MAX
        );
        assert_eq!(
            RetryDelay::from_retry_after_at("0.25", now),
            RetryDelay(Duration::from_millis(250))
        );
    }

    #[test]
    fn retry_after_accepts_http_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let future = httpdate::fmt_http_date(now + Duration::from_secs(12));
        let past = httpdate::fmt_http_date(now - Duration::from_secs(12));
        assert_eq!(
            RetryDelay::from_retry_after_at(&future, now),
            RetryDelay(Duration::from_secs(12))
        );
        assert_eq!(
            RetryDelay::from_retry_after_at(&past, now),
            RetryDelay(Duration::ZERO)
        );

        let distant = httpdate::fmt_http_date(now + Duration::from_secs(300));
        assert_eq!(
            RetryDelay::from_retry_after_at(&distant, now),
            RetryDelay::MAX
        );
    }

    #[test]
    fn retry_jitter_is_bounded_and_preserves_maximum() {
        assert_eq!(
            RetryDelay(Duration::from_secs(1)).with_jitter(Duration::from_secs(2)),
            RetryDelay(Duration::from_secs(1) + RetryDelay::MAX_JITTER)
        );
        assert_eq!(
            RetryDelay::MAX.with_jitter(RetryDelay::MAX_JITTER),
            RetryDelay::MAX
        );
    }

    #[test]
    fn error_body_preview_truncates_on_character_boundaries() {
        let exact = "🦀".repeat(ErrorBodyPreview::MAX_CHARS);
        assert_eq!(
            ErrorBodyPreview::from_bytes(exact.as_bytes()).to_string(),
            exact
        );

        let oversized = format!("{exact}tail");
        let display = ErrorBodyPreview::from_bytes(oversized.as_bytes()).to_string();
        assert_eq!(display.chars().count(), ErrorBodyPreview::MAX_CHARS + 1);
        assert!(display.ends_with('…'));
        assert!(!display.contains("tail"));
    }

    #[test]
    fn error_body_preview_escapes_terminal_controls() {
        let display = ErrorBodyPreview::from_bytes(b"before\x1b[31mred\x1b[0m\nafter").to_string();
        assert!(!display.contains('\x1b'));
        assert!(!display.contains('\n'));
        assert!(display.contains("before"));
        assert!(display.contains("after"));
    }

    #[test]
    fn body_limit_rejects_oversized_declared_body() {
        let mut response: surf::Response = surf::http::Response::new(StatusCode::Ok).into();
        response.set_body(vec![0_u8; 9]);
        let result = futures::executor::block_on(BodyLimit::new(8).read_response(&mut response));
        assert!(matches!(
            result,
            Err(DownloadError::ResponseBodyTooLarge { limit: 8 })
        ));
    }
}
