//! Best-effort image caching and the user-identifier resolution built on it.
//!
//! Every Steam image — avatars (which double as group/clan icons), emoticons,
//! and stickers — is described by a [`SteamImage`], which owns its CDN URL,
//! cache path, and [`CacheCategory`]. Fetching goes through
//! `messenger_downloader`'s shared cache pipeline, which returns the on-disk
//! copy when present and otherwise downloads once and persists it atomically.
//! [`steam_user_identifier`] layers friend-cache lookups on top to turn a bare
//! SteamID into the `Identifier<User>` the UI renders. Mirrors the Discord
//! interface's typed `CdnImage` caching.

use std::path::PathBuf;

use dashmap::DashMap;
use messenger_downloader::{Cached, DownloadResult, Fetch};

use messenger_interface::types::{CacheCategory, ID, Identifier, User};

use crate::api_types::{FriendEntry, steam_avatar_cache_category};

const INTERFACE_NAME: &str = "steam";

/// Steam avatar/profile-image CDN. User and group avatars are both served here,
/// addressed by their 40-char hex SHA1 hash.
const AVATAR_CDN: &str = "https://avatars.steamstatic.com";
/// Steam emoticon image CDN; the path segment is the bare emoticon name.
const EMOTICON_CDN: &str = "https://community.steamstatic.com/economy/emoticon";
/// Steam sticker image CDN. Stickers are economy items served alongside
/// emoticons; the path segment is the sticker's name/path.
const STICKER_CDN: &str = "https://community.steamstatic.com/economy/sticker";

/// A Steam CDN image, identified typesafely instead of by a stringly URL/path.
/// Each variant owns enough to derive its CDN URL, cache filename, and
/// [`CacheCategory`], so all of that construction lives here rather than being
/// duplicated at call sites. Mirrors the Discord interface's `CdnImage`.
pub(crate) enum SteamImage {
    /// A user *or* group avatar — Steam serves both from the same CDN, keyed by
    /// the hex SHA1 hash. The cache category is derived from the SteamID's
    /// account type (individual → users, otherwise → misc).
    Avatar { steamid: ID, hash: String },
    /// An inline message emoticon, addressed by its bare name.
    Emoticon { name: String },
    /// A message sticker. `reference` is the name/path/URL pulled from the
    /// sticker tag; an absolute URL is used as-is, otherwise it names an item
    /// under the economy sticker CDN.
    Sticker { reference: String },
}

impl SteamImage {
    pub(crate) fn avatar(steamid: ID, hash: &str) -> Self {
        Self::Avatar {
            steamid,
            hash: hash.to_owned(),
        }
    }
    pub(crate) fn emoticon(name: &str) -> Self {
        Self::Emoticon {
            name: name.to_owned(),
        }
    }
    pub(crate) fn sticker(reference: &str) -> Self {
        Self::Sticker {
            reference: reference.to_owned(),
        }
    }

    fn url(&self) -> String {
        match self {
            Self::Avatar { hash, .. } => format!("{AVATAR_CDN}/{hash}_full.jpg"),
            Self::Emoticon { name } => format!("{EMOTICON_CDN}/{name}"),
            Self::Sticker { reference } => {
                if reference.starts_with("http") {
                    reference.clone()
                } else {
                    format!("{STICKER_CDN}/{reference}")
                }
            }
        }
    }

    /// The cache-relative path of this image *below* its category/platform
    /// directory. Avatars nest under their owner's SteamID — `{steamid}/{hash}.jpg`
    /// — so a single owner's images share a directory and distinct owners never
    /// collide on a shared hash. Emoticons and stickers are globally identified
    /// by their own name, so they sit directly under the category.
    fn file_name(&self) -> String {
        match self {
            Self::Avatar { steamid, hash } => format!("{steamid}/{hash}.jpg"),
            Self::Emoticon { name } => format!("{name}.png"),
            Self::Sticker { reference } => format!("{}.png", sticker_cache_key(reference)),
        }
    }

    fn category(&self) -> CacheCategory {
        match self {
            Self::Avatar { steamid, .. } => steam_avatar_cache_category(*steamid),
            Self::Emoticon { .. } => CacheCategory::Emoji,
            Self::Sticker { .. } => CacheCategory::Stickers,
        }
    }

    /// Fetch the image (or load it from the on-disk cache) and return its local
    /// path. A cache hit skips the network; the shared downloader handles the
    /// existence check and atomic write.
    pub(crate) async fn fetch(self) -> DownloadResult<PathBuf> {
        let url = self.url();
        let file_name = self.file_name();
        Fetch::<Cached>::cached_fetch(
            INTERFACE_NAME,
            || surf::get(&url),
            Vec::new(),
            self.category(),
            &file_name,
        )
        .await
        .map(Fetch::into_path)
    }
}

/// Reduce a sticker reference (a name, path, or URL) to a safe single-segment
/// cache key for the filename.
fn sticker_cache_key(reference: &str) -> String {
    reference
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

pub(crate) async fn steam_user_identifier(
    steamid: ID,
    friends: &DashMap<ID, FriendEntry>,
    client_steamid: ID,
    username: &str,
) -> Identifier<User> {
    let cached = friends
        .get(&steamid)
        .map(|entry| (entry.name.clone(), entry.avatar_hash.clone()));
    let name = if steamid == client_steamid {
        username.to_owned()
    } else {
        cached
            .as_ref()
            .and_then(|(name, _)| name.clone())
            .unwrap_or_else(|| steamid.to_string())
    };
    let icon = match cached.and_then(|(_, hash)| hash) {
        Some(hash) => SteamImage::avatar(steamid, &hash).fetch().await.ok(),
        None => None,
    };
    Identifier::new(
        steamid,
        User {
            name,
            tag: None,
            icon,
            status: None,
            voice: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticker_cache_key_sanitizes_to_one_segment() {
        assert_eq!(sticker_cache_key("a/b c.png"), "a_b_c_png");
    }
}
