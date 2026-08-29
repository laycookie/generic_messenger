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
use messenger_downloader::{Cached, DownloadError, Fetch};
use sha2::{Digest as _, Sha256};

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
const MAX_ITEM_REFERENCE_LEN: usize = 512;

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

    fn url(&self) -> Result<String, DownloadError> {
        match self {
            Self::Avatar { hash, .. } => {
                validate_avatar_hash(hash)?;
                cdn_item_url(AVATAR_CDN, &format!("{hash}_full.jpg"))
            }
            Self::Emoticon { name } => cdn_item_url(EMOTICON_CDN, name),
            Self::Sticker { reference } => {
                if let Ok(url) = surf::Url::parse(reference) {
                    validate_sticker_url(&url)?;
                    Ok(url.into())
                } else {
                    cdn_item_url(STICKER_CDN, reference)
                }
            }
        }
    }

    /// The cache-relative path of this image *below* its category/platform
    /// directory. Avatars nest under their owner's SteamID — `{steamid}/{hash}.jpg`
    /// — so a single owner's images share a directory and distinct owners never
    /// collide on a shared hash. Emoticons and stickers are globally identified
    /// by their own name, so they sit directly under the category.
    fn file_name(&self) -> Result<String, DownloadError> {
        match self {
            Self::Avatar { steamid, hash } => {
                validate_avatar_hash(hash)?;
                Ok(format!("{steamid}/{hash}.jpg"))
            }
            Self::Emoticon { name } => {
                validate_item_reference(name)?;
                Ok(format!("{}.png", reference_cache_key("emoticon", name)))
            }
            Self::Sticker { reference } => {
                validate_item_reference(reference)?;
                Ok(format!("{}.png", reference_cache_key("sticker", reference)))
            }
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
    pub(crate) async fn fetch(self) -> Result<PathBuf, DownloadError> {
        let url = self.url()?;
        let file_name = self.file_name()?;
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

fn validate_item_reference(reference: &str) -> Result<(), DownloadError> {
    if reference.is_empty()
        || reference.len() > MAX_ITEM_REFERENCE_LEN
        || reference.chars().any(char::is_control)
        || matches!(reference, "." | "..")
    {
        return Err(DownloadError::InvalidInput("invalid Steam image reference"));
    }
    Ok(())
}

fn validate_avatar_hash(hash: &str) -> Result<(), DownloadError> {
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DownloadError::InvalidInput(
            "invalid Steam avatar SHA-1 hash",
        ));
    }
    Ok(())
}

fn cdn_item_url(base: &str, item: &str) -> Result<String, DownloadError> {
    validate_item_reference(item)?;
    let mut url = surf::Url::parse(base)?;
    url.path_segments_mut()
        .map_err(|_| DownloadError::InvalidInput("Steam CDN URL cannot contain path segments"))?
        .push(item);
    Ok(url.into())
}

fn validate_sticker_url(url: &surf::Url) -> Result<(), DownloadError> {
    const ALLOWED_HOSTS: [&str; 2] = [
        "community.steamstatic.com",
        "community.akamai.steamstatic.com",
    ];

    validate_item_reference(url.as_str())?;
    let trusted = url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url
            .host_str()
            .is_some_and(|host| ALLOWED_HOSTS.contains(&host))
        && url.path().starts_with("/economy/sticker/");
    if !trusted {
        return Err(DownloadError::InvalidInput(
            "Steam sticker URL must use approved HTTPS Steam CDN endpoint",
        ));
    }
    Ok(())
}

fn reference_cache_key(kind: &str, reference: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0]);
    hasher.update(reference.as_bytes());
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
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
    fn remote_references_become_digest_cache_keys() {
        let key = SteamImage::emoticon("../../outside").file_name().unwrap();
        assert_eq!(key.len(), 64 + ".png".len());
        assert!(!key.contains(['/', '\\']));
    }

    #[test]
    fn item_names_are_encoded_as_single_url_segments() {
        let url = SteamImage::emoticon("face/../../private").url().unwrap();
        assert!(url.starts_with(EMOTICON_CDN));
        assert!(url.contains("face%2F..%2F..%2Fprivate"));
    }

    #[test]
    fn sticker_urls_reject_ssrf_targets() {
        for target in [
            "http://community.steamstatic.com/economy/sticker/wave",
            "https://127.0.0.1/economy/sticker/wave",
            "https://community.steamstatic.com.evil.test/economy/sticker/wave",
            "https://community.steamstatic.com:444/economy/sticker/wave",
            "https://community.steamstatic.com/not-stickers/wave",
        ] {
            assert!(SteamImage::sticker(target).url().is_err(), "{target}");
        }
    }

    #[test]
    fn sticker_urls_accept_expected_cdn() {
        let target = "https://community.steamstatic.com/economy/sticker/wave.png";
        assert_eq!(SteamImage::sticker(target).url().unwrap(), target);
    }

    #[test]
    fn avatar_hash_requires_sha1_shape() {
        assert!(SteamImage::avatar(1, "../../secret").url().is_err());
        assert!(
            SteamImage::avatar(1, "0123456789abcdef0123456789abcdef01234567")
                .url()
                .is_ok()
        );
    }
}
