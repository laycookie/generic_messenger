#![feature(random)]

//! Shared cache-backed HTTP download primitives for messenger adapters.
//!
//! Interface crates still own their typed CDN image enums, because URL and
//! cache-category rules are platform-specific. This crate owns the common body
//! lifecycle: HTTP fetches, Discord-style 429 retry, JSON decoding, cache hits,
//! and atomic download-then-rename writes.

use std::path::PathBuf;

use facet::Facet;

mod cache;
mod error;
mod http;
mod image_validation;
mod storage;

pub use error::DownloadError;
pub use storage::write_atomic;

/// Typestate marker: a live, uncached response, read straight from the network.
pub struct Fresh(surf::Response);

/// Typestate marker: a body that lives on disk at this path.
pub struct Cached(PathBuf);

/// A typed HTTP fetch. The state `S` gates which operations are available:
/// [`Fetch<Fresh>`] can be decoded as a body or cached to disk, while
/// [`Fetch<Cached>`] can be read from disk or converted into its cache path.
pub struct Fetch<S>(S);

/// The body source behind a [`Fetch`] state, so callers can decode data
/// regardless of whether it came from the network or disk.
#[allow(async_fn_in_trait)]
pub trait Body {
    async fn bytes(self) -> Result<Vec<u8>, DownloadError>;

    async fn json<T: for<'a> Facet<'a>>(self) -> Result<T, DownloadError>
    where
        Self: Sized,
    {
        let bytes = self.bytes().await?;
        Ok(facet_json::from_str(std::str::from_utf8(&bytes)?)?)
    }
}

impl<S: Body> Body for Fetch<S> {
    async fn bytes(self) -> Result<Vec<u8>, DownloadError> {
        self.0.bytes().await
    }
}
