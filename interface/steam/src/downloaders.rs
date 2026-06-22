//! Best-effort image caching and the user-identifier resolution built on it.
//!
//! Avatars, emoticons, and stickers are all fetched through
//! [`cache_remote_image`], which downloads once and reuses the on-disk copy
//! thereafter. [`steam_user_identifier`] layers friend-cache lookups on top to
//! turn a bare SteamID into the `Identifier<User>` the UI renders.

use std::error::Error;
use std::path::{Path, PathBuf};

use dashmap::DashMap;
use tracing::warn;

use messenger_interface::types::{ID, Identifier, User, cache_img_dir};

use crate::api_types::{FriendEntry, steam_avatar_cache_category};

const STEAM_CACHE: &str = "steam";

/// Download and cache a remote image to `dir/filename`, returning the local
/// path. Skips the download if the file already exists. Writes to a unique temp
/// file and renames into place, so concurrent readers never observe a partial
/// image. Best-effort: any failure yields `None`. Shared by avatar, emoticon,
/// and sticker caching.
pub(crate) async fn cache_remote_image(url: &str, dir: PathBuf, filename: &str) -> Option<PathBuf> {
    let file_path = dir.join(filename);
    if async_fs::metadata(&file_path).await.is_ok() {
        return Some(file_path);
    }

    let mut response = match surf::get(url).send().await {
        Ok(response) => response,
        Err(err) => {
            warn!("Steam: image download failed for {url}: {err}");
            return None;
        }
    };
    if !response.status().is_success() {
        warn!(
            "Steam: image download failed for {url}: HTTP {}",
            response.status()
        );
        return None;
    }
    let bytes = response.body_bytes().await.ok()?;

    if let Err(err) = write_atomic(&file_path, bytes).await {
        warn!("Steam: image cache write failed for {url}: {err}");
        return None;
    }
    Some(file_path)
}

/// Write `bytes` to `path` atomically: write into a sibling temp file, then
/// rename it into place so a reader never observes a half-written file.
///
/// [`tempfile`] reserves a collision-proof temp name and guards cleanup on
/// failure (its drop removes the temp file); the body write and the rename go
/// through `async_fs`, so the actual I/O stays on the async runtime rather than
/// a blocking call. The `.part` suffix is kept so `sweep_stale_temp_files`
/// reclaims orphans left by a hard kill (SIGKILL / power loss), which skips
/// `Drop`. Mirrors the Discord interface's atomic image caching.
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

/// Download and cache a Steam profile/avatar image, returning the local path.
/// Individual Steam accounts live under `users`; clan/chat/group IDs are not
/// app users, so they live under `misc`. Best-effort: any failure yields no icon.
pub(crate) async fn cache_avatar(steamid: ID, hash_hex: &str) -> Option<PathBuf> {
    let dir = cache_img_dir(steam_avatar_cache_category(steamid), STEAM_CACHE, steamid);
    let url = format!("https://avatars.steamstatic.com/{hash_hex}_full.jpg");
    cache_remote_image(&url, dir, &format!("{hash_hex}.jpg")).await
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
        Some(hash) => cache_avatar(steamid, &hash).await,
        None => None,
    };
    Identifier::new(steamid, User { name, icon })
}
