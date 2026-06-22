//! On-disk image cache layout and the temp-file conventions interfaces use to
//! populate it atomically.

use std::path::{Path, PathBuf};

/// Base directory for cached images (avatars, icons, etc.).
///
/// Each messenger implementation should store images under
/// `{CACHE_IMGS_DIR}/{category}/{platform}/{id}/` where category
/// is a [`CacheCategory`].
///
/// NOTE: this is relative to the process working directory, so the cache
/// location depends on where the app is launched from.
/// TODO: resolve against an XDG cache dir instead.
pub const CACHE_IMGS_DIR: &str = "./.cache/imgs";

/// Logical grouping for cached images.
pub enum CacheCategory {
    Users,
    Servers,
    Channels,
    Emoji,
    Stickers,
    /// Images whose owner/type is unknown or does not map to a standard
    /// messenger entity.
    Misc,
    Custom(&'static str),
}
impl CacheCategory {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Users => "users",
            Self::Servers => "servers",
            Self::Channels => "channels",
            Self::Emoji => "emoji",
            Self::Stickers => "stickers",
            Self::Misc => "misc",
            Self::Custom(s) => s,
        }
    }
}

/// Build the standard cache directory for an interface image.
pub fn cache_dir(category: CacheCategory, platform: &str) -> PathBuf {
    [CACHE_IMGS_DIR, category.as_str(), platform]
        .iter()
        .collect()
}

/// Build the standard cache directory for an interface image.
pub fn cache_img_dir(
    category: CacheCategory,
    platform: &str,
    id: impl std::fmt::Display,
) -> PathBuf {
    let id = id.to_string();
    [CACHE_IMGS_DIR, category.as_str(), platform, &id]
        .iter()
        .collect()
}

/// Suffix used for in-progress download temp files.
///
/// Interfaces cache images with an atomic download-then-rename: the body is
/// written to a temp file carrying this suffix, then renamed into place so a
/// reader never observes a half-written file. The temp file is removed on
/// graceful failure, so a download that errors out leaves nothing behind.
/// Orphans from a hard kill, which skips that cleanup, are reclaimed by
/// [`sweep_stale_temp_files`].
pub const TEMP_FILE_SUFFIX: &str = ".part";

/// Recursively remove leftover `*.part` files under [`CACHE_IMGS_DIR`].
///
/// The atomic caching described on [`TEMP_FILE_SUFFIX`] removes its temp file on
/// graceful failure, but a hard kill (`SIGKILL`, power loss) skips that cleanup.
/// Call this once at startup, before any downloads begin, to reclaim orphans
/// left by a previous run. Running it while downloads are in flight could delete
/// a live temp file, so it must only run before the cache is used. Best-effort:
/// I/O errors are ignored.
pub fn sweep_stale_temp_files() {
    fn sweep(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                sweep(&path);
            } else if path.to_str().is_some_and(|p| p.ends_with(TEMP_FILE_SUFFIX)) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    sweep(Path::new(CACHE_IMGS_DIR));
}
