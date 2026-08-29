use std::path::PathBuf;

use messenger_downloader::DownloadError;
pub(crate) use messenger_downloader::{Body, Cached, Fetch, Fresh};
use messenger_interface::types::CacheCategory;

use crate::{INTERFACE_NAME, api::SNOWFLAKE};

const IMG_EXT: &str = "webp";
const DISCORD_CDN: &str = "https://cdn.discordapp.com";

/// A Discord CDN image, identified typesafely instead of by a stringly path
/// segment. Each variant owns enough to derive its CDN URL, cache filename, and
/// [`CacheCategory`], so all of that construction lives here rather than being
/// duplicated at call sites.
///
/// <https://docs.discord.food/reference#cdn-endpoints>
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
    pub(crate) async fn fetch(self) -> Result<PathBuf, DownloadError> {
        let url = self.url();
        let file_name = self.file_name();
        Fetch::<Cached>::cached_fetch(
            INTERFACE_NAME,
            || surf::get(&url),
            [] as [(&str, &str); 0],
            self.category(),
            &file_name,
        )
        .await
        .map(Fetch::into_path)
    }
}
