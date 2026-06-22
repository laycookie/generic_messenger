use std::{
    error::Error,
    path::{Path, PathBuf},
    time::Duration,
};

use futures_timer::Delay;

use facet::Facet;
use messenger_interface::types::{CacheCategory, cache_dir};
use surf::{RequestBuilder, StatusCode};
use tracing::{error, warn};

use crate::{INTERFACE_NAME, api_types::SNOWFLAKE};

const IMG_EXT: &str = "webp";
const DISCORD_CDN: &str = "https://cdn.discordapp.com";

/// A Discord CDN image, identified typesafely instead of by a stringly path
/// segment. Each variant owns enough to derive its CDN URL, cache filename, and
/// [`CacheCategory`], so all of that construction lives here rather than being
/// duplicated at call sites.
pub(crate) enum CdnImage {
    Avatar { user: SNOWFLAKE, hash: String },
    GuildIcon { guild: SNOWFLAKE, hash: String },
    ChannelIcon { channel: SNOWFLAKE, hash: String },
    Emoji { id: SNOWFLAKE, ext: &'static str },
    Sticker { id: SNOWFLAKE, ext: &'static str },
}

impl CdnImage {
    pub(crate) fn avatar(user: SNOWFLAKE, hash: &str) -> Self {
        Self::Avatar {
            user,
            hash: hash.to_owned(),
        }
    }
    pub(crate) fn guild_icon(guild: SNOWFLAKE, hash: &str) -> Self {
        Self::GuildIcon {
            guild,
            hash: hash.to_owned(),
        }
    }
    pub(crate) fn channel_icon(channel: SNOWFLAKE, hash: &str) -> Self {
        Self::ChannelIcon {
            channel,
            hash: hash.to_owned(),
        }
    }
    pub(crate) fn emoji(id: SNOWFLAKE, animated: bool) -> Self {
        Self::Emoji {
            id,
            ext: if animated { "gif" } else { "webp" },
        }
    }

    /// A sticker image, or `None` for formats with no static image (Lottie).
    /// `format_type`: 1 = PNG, 2 = APNG, 3 = Lottie (JSON), 4 = GIF.
    pub(crate) fn sticker(id: SNOWFLAKE, format_type: u8) -> Option<Self> {
        let ext = match format_type {
            4 => "gif",
            1 | 2 => "png",
            _ => return None,
        };
        Some(Self::Sticker { id, ext })
    }

    fn url(&self) -> String {
        match self {
            Self::Avatar { user, hash } => {
                format!("{DISCORD_CDN}/avatars/{user}/{hash}.{IMG_EXT}?size=80&quality=lossless")
            }
            Self::GuildIcon { guild, hash } => {
                format!("{DISCORD_CDN}/icons/{guild}/{hash}.{IMG_EXT}?size=80&quality=lossless")
            }
            Self::ChannelIcon { channel, hash } => format!(
                "{DISCORD_CDN}/channel-icons/{channel}/{hash}.{IMG_EXT}?size=80&quality=lossless"
            ),
            Self::Emoji { id, ext } => format!("{DISCORD_CDN}/emojis/{id}.{ext}?size=48"),
            Self::Sticker { id, ext } => format!("{DISCORD_CDN}/stickers/{id}.{ext}?size=160"),
        }
    }

    /// The cache-relative path of this image *below* its category/platform
    /// directory. Owner-scoped images (avatars/icons) nest under their owner's
    /// id — e.g. `{user}/{hash}.webp` — so a single owner's images share a
    /// directory and distinct owners never collide on a shared hash. Emoji and
    /// stickers are globally identified by their own id, so they sit directly
    /// under the category as `{id}.{ext}`.
    fn file_name(&self) -> String {
        match self {
            Self::Avatar { user: id, hash }
            | Self::GuildIcon { guild: id, hash }
            | Self::ChannelIcon { channel: id, hash } => format!("{id}/{hash}.{IMG_EXT}"),
            Self::Emoji { id, ext } | Self::Sticker { id, ext } => format!("{id}.{ext}"),
        }
    }

    fn category(&self) -> CacheCategory {
        match self {
            Self::Avatar { .. } => CacheCategory::Users,
            Self::GuildIcon { .. } => CacheCategory::Servers,
            Self::ChannelIcon { .. } => CacheCategory::Channels,
            Self::Emoji { .. } => CacheCategory::Emoji,
            Self::Sticker { .. } => CacheCategory::Stickers,
        }
    }

    /// Fetch the image (or load it from the on-disk cache) and return its local
    /// path. A cache hit skips the network. The body is read via the [`Fetch`]
    /// pipeline, so 429s/retries and atomic writes are handled there.
    pub(crate) async fn fetch(self) -> Result<PathBuf, Box<dyn Error + Sync + Send>> {
        Fetch::<Cached>::cached_fetch(
            || surf::get(self.url()),
            Vec::new(),
            self.category(),
            &self.file_name(),
        )
        .await
        .map(|fetched| fetched.into_path())
    }
}

/// Typestate marker: a live, uncached response, read straight from the network.
pub(crate) struct Fresh(surf::Response);
/// Typestate marker: a body that lives on disk at this path.
pub(crate) struct Cached(PathBuf);

/// A typed HTTP fetch. The state `S` — [`Fresh`] or [`Cached`] — gates which
/// operations are available, so the type system enforces the lifecycle:
/// [`fetch`](Fetch::fetch) produces a [`Fresh`], which can be decoded as JSON
/// or [`cache`](Fetch::cache)d into a [`Cached`]; a [`Cached`] yields its path
/// infallibly. There is no way to cache an already-cached fetch or to ask a
/// network-only fetch for a path.
pub(crate) struct Fetch<S>(S);

impl Fetch<Fresh> {
    /// Perform a one-shot request, accepting any 2xx response. Retries on 429
    /// honoring `Retry-After` (reactions hit Discord's rate limits first), and
    /// surfaces the response body of failed requests in the returned error
    /// instead of discarding it — Discord explains failures (e.g. an outdated
    /// token) there. The body is otherwise left unread for the terminal.
    pub(crate) async fn fetch(
        req: impl Fn() -> RequestBuilder,
        headers: Vec<(&str, String)>,
    ) -> Result<Self, Box<dyn Error + Sync + Send>> {
        const MAX_ATTEMPTS: u32 = 3;
        for attempt in 1..=MAX_ATTEMPTS {
            let mut request = req();
            for (key, value) in &headers {
                request = request.header(*key, value.clone());
            }
            let request = request.build();
            let method = request.method().to_string();
            let url = request.url().to_string();

            let mut res = surf::client().send(request).await.map_err(|err| {
                error!("HTTP request failed for {method} {url}: {err}");
                err
            })?;
            let status = res.status();

            if status == StatusCode::TooManyRequests && attempt < MAX_ATTEMPTS {
                let retry_after = res
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
                let body = res.body_string().await.unwrap_or_default();
                error!("HTTP request failed for {method} {url} with status {status:?}: {body}");
                return Err(surf::Error::from_str(status, format!("HTTP {status}: {body}")).into());
            }
            return Ok(Fetch(Fresh(res)));
        }
        unreachable!("the final attempt always returns")
    }

    /// Persist the body to `file_path`, transitioning to a [`Cached`] fetch.
    ///
    /// This only writes — the cache-hit / network short-circuit lives in
    /// [`Fetch::<Cached>::cached_fetch`](Fetch::cached_fetch), so by the time a
    /// `Fresh` reaches here the request has (intentionally) already been made.
    async fn cache(
        self,
        file_path: PathBuf,
    ) -> Result<Fetch<Cached>, Box<dyn Error + Sync + Send>> {
        let bytes = self.0.bytes().await?;
        write_atomic(&file_path, bytes).await?;
        Ok(Fetch(Cached(file_path)))
    }
}

/// The body source behind a [`Fetch`] state, so [`Fetch::json`] can decode the
/// body regardless of whether it comes from the network ([`Fresh`]) or from
/// disk ([`Cached`]).
pub(crate) trait Body {
    async fn bytes(self) -> Result<Vec<u8>, Box<dyn Error + Sync + Send>>;
    async fn json<T: for<'a> Facet<'a>>(self) -> Result<T, Box<dyn Error + Sync + Send>>
    where
        Self: Sized,
    {
        let bytes = self.bytes().await?;
        Ok(facet_json::from_str(std::str::from_utf8(&bytes)?)?)
    }
}
impl Body for Fresh {
    async fn bytes(mut self) -> Result<Vec<u8>, Box<dyn Error + Sync + Send>> {
        Ok(self.0.body_bytes().await?)
    }
}

impl Body for Cached {
    async fn bytes(self) -> Result<Vec<u8>, Box<dyn Error + Sync + Send>> {
        Ok(async_fs::read(self.0).await?)
    }
}
impl<S: Body> Body for Fetch<S> {
    async fn bytes(self) -> Result<Vec<u8>, Box<dyn Error + Sync + Send>> {
        self.0.bytes().await
    }
}

impl Fetch<Cached> {
    /// Resolve a request against the on-disk cache under `category`/`file_name`.
    ///
    /// On a cache hit this returns immediately without touching the network;
    /// otherwise it fetches and persists the body. Composed from
    /// [`Fetch::<Fresh>::fetch`](Fetch::fetch) + [`cache`](Fetch::cache), with
    /// the existence check kept ahead of the fetch so a hit never hits the
    /// network.
    async fn cached_fetch(
        req: impl Fn() -> RequestBuilder,
        headers: Vec<(&str, String)>,
        category: CacheCategory,
        file_name: &str,
    ) -> Result<Self, Box<dyn Error + Sync + Send>> {
        let file_path = cache_dir(category, INTERFACE_NAME).join(file_name);
        if file_path.exists() {
            return Ok(Fetch(Cached(file_path)));
        }
        Fetch::<Fresh>::fetch(req, headers)
            .await?
            .cache(file_path)
            .await
    }

    /// The on-disk path of the cached body. Infallible: a [`Cached`] always
    /// has one.
    fn into_path(self) -> PathBuf {
        self.0.0
    }
}

/// Write `bytes` to `path` atomically: write into a sibling temp file, then
/// rename it into place so a reader never observes a half-written file.
///
/// [`tempfile`] reserves a collision-proof temp name and guards cleanup on
/// failure (its drop removes the temp file); the body write and the rename go
/// through `async_fs`, so the actual I/O stays on the async runtime rather than
/// a blocking call. The `.part` suffix is kept so `sweep_stale_temp_files`
/// reclaims orphans left by a hard kill (SIGKILL / power loss), which skips
/// `Drop`.
async fn write_atomic(path: &Path, bytes: Vec<u8>) -> Result<(), Box<dyn Error + Sync + Send>> {
    // The temp file must share the destination's directory (and thus
    // filesystem) for the final rename to be atomic.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    async_fs::create_dir_all(parent).await?;

    // `into_temp_path` drops the file handle (we reopen via async_fs to write)
    // but keeps the named file on disk, armed for cleanup until `keep`.
    let tmp = tempfile::Builder::new()
        .suffix(".part")
        .tempfile_in(parent)?
        .into_temp_path();

    async_fs::write(&tmp, &bytes).await?;
    async_fs::rename(&tmp, path).await?;
    tmp.keep()?;

    Ok(())
}
