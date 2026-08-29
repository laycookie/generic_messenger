//! Typed failures produced by download, validation, and cache operations.

use std::{io, str::Utf8Error};

use facet::Facet;
use facet_error as error;

/// Failure produced while fetching, decoding, validating, or caching a body.
#[derive(Debug, Facet)]
#[facet(derive(Error))]
#[non_exhaustive]
#[repr(u8)]
pub enum DownloadError {
    /// I/O failed: {0}
    Io(
        #[facet(opaque)]
        #[facet(error::from)]
        io::Error,
    ),

    /// HTTP failed: {0}
    Http(#[facet(opaque)] surf::Error),

    /// URL parsing failed: {0}
    Url(
        #[facet(opaque)]
        #[facet(error::from)]
        surf::http::url::ParseError,
    ),

    /// response text was not valid UTF-8: {0}
    Utf8(
        #[facet(opaque)]
        #[facet(error::from)]
        Utf8Error,
    ),

    /// JSON response decoding failed: {0}
    Json(
        #[facet(opaque)]
        #[facet(error::source)]
        Box<facet_json::DeserializeError>,
    ),

    /// invalid request input: {0}
    InvalidInput(&'static str),

    /// cache filename must be a non-empty relative path without traversal
    InvalidCacheFilename,

    /// cache entry exists but is not a regular file
    CacheEntryNotRegularFile,

    /// downloaded cache body is not a valid supported image
    InvalidImageBody,

    /// HTTP response body exceeds {limit} byte limit
    ResponseBodyTooLarge { limit: usize },

    /// HTTP request exceeded {seconds} second deadline
    RequestTimedOut { seconds: u64 },

    /// HTTP response body exceeded {seconds} second deadline
    ResponseBodyTimedOut { seconds: u64 },
}

impl From<facet_json::DeserializeError> for DownloadError {
    fn from(error: facet_json::DeserializeError) -> Self {
        Self::Json(Box::new(error))
    }
}

impl From<surf::Error> for DownloadError {
    fn from(error: surf::Error) -> Self {
        Self::Http(error)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn facet_error_formats_fields_and_exposes_sources() {
        assert!(std::mem::size_of::<DownloadError>() <= 64);

        let oversized = DownloadError::ResponseBodyTooLarge { limit: 42 };
        assert_eq!(
            oversized.to_string(),
            "HTTP response body exceeds 42 byte limit"
        );

        let io_error: DownloadError = io::Error::other("disk unavailable").into();
        assert_eq!(io_error.to_string(), "I/O failed: disk unavailable");
        assert!(io_error.source().is_some());
    }
}
