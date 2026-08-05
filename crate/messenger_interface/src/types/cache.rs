//! On-disk image cache layout, access tracking, and periodic cleanup.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, BufRead as _, BufReader, Write as _},
    path::{Component, Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tracing::{debug, warn};

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

const ACCESS_DB_FILE: &str = ".access-v1.db";
const ACCESS_DB_MAX_BYTES: u64 = 16 * 1024 * 1024;
const ACCESS_WRITE_GRANULARITY: Duration = Duration::from_secs(60 * 60);

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

/// Build standard cache directory for an interface image.
pub fn cache_dir(category: CacheCategory, platform: &str) -> PathBuf {
    [CACHE_IMGS_DIR, category.as_str(), platform]
        .iter()
        .collect()
}

/// Build standard cache directory for an interface image owner.
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
/// Interfaces cache images with an atomic download-then-rename: body is
/// written to a temp file carrying this suffix, then renamed into place so a
/// reader never observes a half-written file. Orphans from a hard kill are
/// reclaimed by [`sweep_stale_temp_files`].
pub const TEMP_FILE_SUFFIX: &str = ".part";

/// Cache retention and janitor schedule.
#[derive(Clone, Copy, Debug)]
pub struct CacheCleanupPolicy {
    /// Delete regular cache files not accessed for this duration.
    pub max_idle: Duration,
    /// Time between cleanup passes while app remains open.
    pub interval: Duration,
}

impl Default for CacheCleanupPolicy {
    fn default() -> Self {
        Self {
            max_idle: Duration::from_secs(30 * 24 * 60 * 60),
            interval: Duration::from_secs(6 * 60 * 60),
        }
    }
}

/// Summary from one cache cleanup pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheCleanupStats {
    pub scanned_files: u64,
    pub removed_files: u64,
    pub removed_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct AccessEntry {
    last_access: u64,
    last_persisted: u64,
}

struct AccessDatabase {
    root: PathBuf,
    entries: HashMap<PathBuf, AccessEntry>,
}

impl AccessDatabase {
    fn load(root: PathBuf) -> io::Result<Self> {
        let mut database = Self {
            root,
            entries: HashMap::new(),
        };
        #[cfg(unix)]
        let file = {
            use rustix::fs::{Mode, OFlags, openat};
            use rustix::io::Errno;

            let Some(root) = open_directory_no_follow(&database.root)? else {
                return Ok(database);
            };
            let fd = match openat(
                &root,
                ACCESS_DB_FILE,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(Errno::NOENT) => return Ok(database),
                Err(error) => return Err(error.into()),
            };
            fs::File::from(fd)
        };
        #[cfg(not(unix))]
        let file = {
            validate_no_reparse_components(&database.root)?;
            let path = database.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(database),
                Err(error) => return Err(error),
            };
            if metadata_is_reparse_point(&metadata) || !metadata.file_type().is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cache access database is not a regular file",
                ));
            }
            fs::File::open(path)?
        };
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() || metadata.len() > ACCESS_DB_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cache access database is not a bounded regular file",
            ));
        }

        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { continue };
            let Some((timestamp, encoded_path)) = line.split_once('\t') else {
                continue;
            };
            let Ok(timestamp) = timestamp.parse::<u64>() else {
                continue;
            };
            let Some(relative) = decode_relative_path(encoded_path) else {
                continue;
            };
            database.entries.insert(
                relative,
                AccessEntry {
                    last_access: timestamp,
                    last_persisted: timestamp,
                },
            );
        }
        Ok(database)
    }

    #[cfg(not(unix))]
    fn path(&self) -> PathBuf {
        self.root.join(ACCESS_DB_FILE)
    }

    fn relative(&self, path: &Path) -> io::Result<PathBuf> {
        let relative = path.strip_prefix(&self.root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "cache access path is outside cache root",
            )
        })?;
        if !is_safe_relative_path(relative) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cache access path is not a safe relative path",
            ));
        }
        Ok(relative.to_owned())
    }

    fn record(&mut self, path: &Path, now: u64) -> io::Result<()> {
        let relative = self.relative(path)?;
        let should_persist = {
            let entry = self.entries.entry(relative.clone()).or_insert(AccessEntry {
                last_access: now,
                last_persisted: 0,
            });
            entry.last_access = now;
            now.saturating_sub(entry.last_persisted) >= ACCESS_WRITE_GRANULARITY.as_secs()
        };
        if !should_persist {
            return Ok(());
        }

        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, openat};

            let root = open_or_create_directory_no_follow(&self.root)?;
            let file = openat(
                &root,
                ACCESS_DB_FILE,
                OFlags::WRONLY
                    | OFlags::CREATE
                    | OFlags::APPEND
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(io::Error::from)?;
            let mut file = fs::File::from(file);
            writeln!(file, "{now}\t{}", encode_relative_path(&relative)?)?;
            file.flush()?;
        }
        #[cfg(not(unix))]
        {
            validate_no_reparse_components(&self.root)?;
            fs::create_dir_all(&self.root)?;
            validate_no_reparse_components(&self.root)?;
            let mut options = fs::OpenOptions::new();
            options.create(true).append(true);
            set_private_create_mode(&mut options);
            let mut file = options.open(self.path())?;
            set_private_permissions(&file)?;
            writeln!(file, "{now}\t{}", encode_relative_path(&relative)?)?;
            file.flush()?;
        }
        self.entries
            .get_mut(&relative)
            .expect("cache access entry was inserted above")
            .last_persisted = now;
        Ok(())
    }

    #[cfg(not(unix))]
    fn persist(&mut self) -> io::Result<()> {
        validate_no_reparse_components(&self.root)?;
        fs::create_dir_all(&self.root)?;
        validate_no_reparse_components(&self.root)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".access-v1-")
            .suffix(TEMP_FILE_SUFFIX)
            .tempfile_in(&self.root)?;
        set_private_permissions(temp.as_file())?;

        let mut entries = self.entries.iter().collect::<Vec<_>>();
        entries.sort_unstable_by_key(|(path, _)| *path);
        for (relative, entry) in entries {
            writeln!(
                temp,
                "{}\t{}",
                entry.last_access,
                encode_relative_path(relative)?
            )?;
        }
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(self.path()).map_err(|err| err.error)?;
        sync_parent_directory(&self.root)?;
        for entry in self.entries.values_mut() {
            entry.last_persisted = entry.last_access;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn persist_at(&mut self, directory: &impl rustix::fd::AsFd) -> io::Result<()> {
        use rustix::fs::{AtFlags, Mode, OFlags, fsync, openat, renameat, unlinkat};

        let sequence = ACCESS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!(
            ".access-v1-{}-{sequence}{}",
            std::process::id(),
            TEMP_FILE_SUFFIX
        );
        let fd = openat(
            directory,
            temp_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(io::Error::from)?;
        let mut temp = fs::File::from(fd);
        let write_result = (|| {
            let mut entries = self.entries.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(path, _)| *path);
            for (relative, entry) in entries {
                writeln!(
                    temp,
                    "{}\t{}",
                    entry.last_access,
                    encode_relative_path(relative)?
                )?;
            }
            temp.flush()?;
            temp.sync_all()
        })();
        drop(temp);
        if let Err(error) = write_result {
            let _ = unlinkat(directory, temp_name.as_str(), AtFlags::empty());
            return Err(error);
        }
        if let Err(error) = renameat(directory, temp_name.as_str(), directory, ACCESS_DB_FILE) {
            let _ = unlinkat(directory, temp_name.as_str(), AtFlags::empty());
            return Err(io::Error::from(error));
        }
        fsync(directory).map_err(io::Error::from)?;
        for entry in self.entries.values_mut() {
            entry.last_persisted = entry.last_access;
        }
        Ok(())
    }
}

static ACCESS_DATABASE: OnceLock<Mutex<AccessDatabase>> = OnceLock::new();
static JANITOR_RUNNING: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static ACCESS_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct JanitorRunGuard;

impl Drop for JanitorRunGuard {
    fn drop(&mut self) {
        JANITOR_RUNNING.store(false, Ordering::Release);
    }
}

fn access_database() -> &'static Mutex<AccessDatabase> {
    ACCESS_DATABASE.get_or_init(|| {
        let root = PathBuf::from(CACHE_IMGS_DIR);
        let database = AccessDatabase::load(root.clone()).unwrap_or_else(|err| {
            warn!(error = %err, path = %root.display(), "failed to load cache access database");
            AccessDatabase {
                root,
                entries: HashMap::new(),
            }
        });
        Mutex::new(database)
    })
}

/// Record successful cache-file access in persistent access database.
///
/// Writes are throttled to once per file per hour. In-memory timestamp still
/// advances on every access, avoiding needless disk writes during active use.
#[doc(hidden)]
pub fn record_cache_access(path: &Path) {
    let now = unix_time(SystemTime::now());
    let mut database = access_database()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Err(err) = database.record(path, now) {
        warn!(error = %err, path = %path.display(), "failed to record cache access");
    }
}

/// Run one synchronous cache cleanup pass.
///
/// Missing access records fall back to file modification time, allowing caches
/// created by older app versions to age out. Symlinks are neither followed nor
/// removed.
pub fn cleanup_cache(max_idle: Duration) -> io::Result<CacheCleanupStats> {
    let now = unix_time(SystemTime::now());
    let mut database = access_database()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cleanup_cache_at(&mut database, max_idle, now)
}

/// In-process scheduled cache janitor.
///
/// Cleanup runs immediately, then every [`CacheCleanupPolicy::interval`] on a
/// dedicated filesystem thread. Dropping handle requests shutdown and detaches;
/// app shutdown never waits for a filesystem walk already in progress.
pub struct CacheJanitor {
    stop: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CacheJanitor {
    /// Start cache janitor. Only one janitor may run in a process.
    pub fn start(policy: CacheCleanupPolicy) -> io::Result<Self> {
        if policy.interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cache cleanup interval must be non-zero",
            ));
        }
        if JANITOR_RUNNING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "cache janitor is already running",
            ));
        }

        let (stop, stopped) = mpsc::channel();
        let thread = match thread::Builder::new()
            .name("cache-janitor".to_owned())
            .spawn(move || {
                let _running = JanitorRunGuard;
                loop {
                    match cleanup_cache(policy.max_idle) {
                        Ok(stats) => debug!(
                            scanned_files = stats.scanned_files,
                            removed_files = stats.removed_files,
                            removed_bytes = stats.removed_bytes,
                            "cache cleanup completed"
                        ),
                        Err(err) => warn!(error = %err, "cache cleanup failed"),
                    }

                    match stopped.recv_timeout(policy.interval) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            }) {
            Ok(thread) => thread,
            Err(err) => {
                JANITOR_RUNNING.store(false, Ordering::Release);
                return Err(err);
            }
        };

        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for CacheJanitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        // Dropping JoinHandle detaches. Worker clears JANITOR_RUNNING after
        // current pass observes stop, so a second janitor cannot overlap it.
        self.thread.take();
    }
}

#[cfg(not(unix))]
fn cleanup_cache_at(
    database: &mut AccessDatabase,
    max_idle: Duration,
    now: u64,
) -> io::Result<CacheCleanupStats> {
    validate_no_reparse_components(&database.root)?;
    let root_metadata = match fs::symlink_metadata(&database.root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            database.entries.clear();
            return Ok(CacheCleanupStats::default());
        }
        Err(error) => return Err(error),
    };
    if !root_metadata.file_type().is_dir() || metadata_is_reparse_point(&root_metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cache root is a symlink or reparse point",
        ));
    }
    let mut files = Vec::new();
    let mut directories = Vec::new();
    collect_regular_files(&database.root, &mut files, &mut directories)?;

    let mut stats = CacheCleanupStats::default();
    let mut existing = HashSet::with_capacity(files.len());
    for path in files {
        if path.file_name().is_some_and(|name| name == ACCESS_DB_FILE)
            || path
                .to_str()
                .is_some_and(|path| path.ends_with(TEMP_FILE_SUFFIX))
        {
            continue;
        }

        let relative = database.relative(&path)?;
        existing.insert(relative.clone());
        stats.scanned_files = stats.scanned_files.saturating_add(1);
        let metadata = fs::metadata(&path)?;
        let fallback_access = metadata.modified().map(unix_time).unwrap_or(now);
        let entry = database
            .entries
            .entry(relative.clone())
            .or_insert(AccessEntry {
                last_access: fallback_access,
                last_persisted: 0,
            });
        let idle = now.saturating_sub(entry.last_access);
        if idle >= max_idle.as_secs() {
            fs::remove_file(&path)?;
            stats.removed_files = stats.removed_files.saturating_add(1);
            stats.removed_bytes = stats.removed_bytes.saturating_add(metadata.len());
            database.entries.remove(&relative);
            existing.remove(&relative);
        }
    }

    database
        .entries
        .retain(|relative, _| existing.contains(relative));
    for directory in directories.into_iter().rev() {
        let _ = fs::remove_dir(directory);
    }
    database.persist()?;
    Ok(stats)
}

#[cfg(unix)]
fn cleanup_cache_at(
    database: &mut AccessDatabase,
    max_idle: Duration,
    now: u64,
) -> io::Result<CacheCleanupStats> {
    use std::os::unix::ffi::OsStrExt as _;

    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, openat, statat, unlinkat};
    use rustix::io::Errno;

    fn clean_directory(
        directory: &impl rustix::fd::AsFd,
        relative_directory: &Path,
        database: &mut AccessDatabase,
        max_idle: Duration,
        now: u64,
        existing: &mut HashSet<PathBuf>,
        stats: &mut CacheCleanupStats,
    ) -> io::Result<()> {
        let mut entries = Dir::read_from(directory.as_fd()).map_err(io::Error::from)?;
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(io::Error::from)?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }

            // Never trust d_type alone: some filesystems report Unknown. The
            // no-follow stat observes link itself, not target.
            let stat = match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(Errno::NOENT) => continue,
                Err(err) => return Err(err.into()),
            };
            let file_type = FileType::from_raw_mode(stat.st_mode);
            let name_os = std::ffi::OsStr::from_bytes(name.to_bytes());
            let relative = relative_directory.join(name_os);

            if file_type.is_dir() {
                let child = match openat(
                    directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(child) => child,
                    Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => continue,
                    Err(err) => return Err(err.into()),
                };
                clean_directory(&child, &relative, database, max_idle, now, existing, stats)?;
                // Descriptor-relative removal cannot follow a directory swapped
                // for a symlink between traversal and cleanup.
                let _ = unlinkat(directory, name, AtFlags::REMOVEDIR);
                continue;
            }
            if !file_type.is_file()
                || name.to_bytes() == ACCESS_DB_FILE.as_bytes()
                || name.to_bytes().ends_with(TEMP_FILE_SUFFIX.as_bytes())
            {
                continue;
            }

            let file = match openat(
                directory,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(file) => std::fs::File::from(file),
                Err(Errno::NOENT | Errno::LOOP) => continue,
                Err(err) => return Err(err.into()),
            };
            let metadata = file.metadata()?;
            if !metadata.file_type().is_file() {
                continue;
            }

            existing.insert(relative.clone());
            stats.scanned_files = stats.scanned_files.saturating_add(1);
            let fallback_access = metadata.modified().map(unix_time).unwrap_or(now);
            let entry = database
                .entries
                .entry(relative.clone())
                .or_insert(AccessEntry {
                    last_access: fallback_access,
                    last_persisted: 0,
                });
            if now.saturating_sub(entry.last_access) >= max_idle.as_secs() {
                // `unlinkat` removes named entry itself. If attacker swaps it
                // for symlink, link is removed; target remains untouched.
                match unlinkat(directory, name, AtFlags::empty()) {
                    Ok(()) => {
                        stats.removed_files = stats.removed_files.saturating_add(1);
                        stats.removed_bytes = stats.removed_bytes.saturating_add(metadata.len());
                        database.entries.remove(&relative);
                        existing.remove(&relative);
                    }
                    Err(Errno::NOENT) => {}
                    Err(err) => return Err(err.into()),
                }
            }
        }
        Ok(())
    }

    let Some(root) = open_directory_no_follow(&database.root)? else {
        database.entries.clear();
        return Ok(CacheCleanupStats::default());
    };
    let mut stats = CacheCleanupStats::default();
    let mut existing = HashSet::new();
    clean_directory(
        &root,
        Path::new(""),
        database,
        max_idle,
        now,
        &mut existing,
        &mut stats,
    )?;
    database
        .entries
        .retain(|relative, _| existing.contains(relative));
    // Persist through same verified directory descriptor used for traversal;
    // swapping a path component cannot redirect database replacement.
    database.persist_at(&root)?;
    Ok(stats)
}

#[cfg(unix)]
fn open_directory_no_follow(path: &Path) -> io::Result<Option<rustix::fd::OwnedFd>> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    use rustix::io::Errno;

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let start = if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    let mut directory = openat(CWD, start, flags, Mode::empty()).map_err(io::Error::from)?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cache root contains unsupported path component",
                ));
            }
        };
        directory = match openat(&directory, name, flags, Mode::empty()) {
            Ok(next) => next,
            Err(Errno::NOENT) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
    }
    Ok(Some(directory))
}

/// Create missing cache-root components without ever traversing a symlink.
/// Path-based `create_dir_all` is unsafe here: an intermediate link could make
/// even a failed access-database write create directories outside cache root.
#[cfg(unix)]
fn open_or_create_directory_no_follow(path: &Path) -> io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::{CWD, Mode, OFlags, mkdirat, openat};
    use rustix::io::Errno;

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let start = if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    let mut directory = openat(CWD, start, flags, Mode::empty()).map_err(io::Error::from)?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cache root contains unsupported path component",
                ));
            }
        };
        directory = match openat(&directory, name, flags, Mode::empty()) {
            Ok(next) => next,
            Err(Errno::NOENT) => {
                match mkdirat(&directory, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                openat(&directory, name, flags, Mode::empty()).map_err(io::Error::from)?
            }
            Err(error) => return Err(error.into()),
        };
    }
    Ok(directory)
}

/// Reject static Windows junction/symlink escapes in every existing cache-root
/// component. Later metadata and traversal checks remain necessary because
/// path-based Windows filesystem APIs cannot close TOCTOU races.
#[cfg(windows)]
fn validate_no_reparse_components(path: &Path) -> io::Result<()> {
    let mut current = if path.is_absolute() {
        PathBuf::new()
    } else {
        PathBuf::from(".")
    };
    for component in path.components() {
        match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cache root contains a parent component",
                ));
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                current.push(component.as_os_str());
            }
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata_is_reparse_point(&metadata) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "cache root contains a symlink or reparse point",
                    ));
                }
                if !metadata.file_type().is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        "cache root component is not a directory",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn validate_no_reparse_components(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn collect_regular_files(
    directory: &Path,
    files: &mut Vec<PathBuf>,
    directories: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata_is_reparse_point(&metadata) {
            continue;
        }
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            collect_regular_files(&entry.path(), files, directories)?;
            directories.push(entry.path());
        } else if file_type.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(all(not(unix), not(windows)))]
fn metadata_is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn encode_relative_path(path: &Path) -> io::Result<String> {
    let value = path.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "cache path is not valid UTF-8")
    })?;
    let mut encoded = String::with_capacity(value.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in value.bytes() {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    Ok(encoded)
}

fn decode_relative_path(encoded: &str) -> Option<PathBuf> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let nibble = |byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        bytes.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    let path = PathBuf::from(String::from_utf8(bytes).ok()?);
    is_safe_relative_path(&path).then_some(path)
}

fn unix_time(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn checked_cache_parent(path: &Path) -> io::Result<&Path> {
    let root = Path::new(CACHE_IMGS_DIR);
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache file path is outside cache root",
        )
    })?;
    if !is_safe_relative_path(relative) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache file path contains unsafe components",
        ));
    }
    path.parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache file path has no parent"))
}

/// Verify every existing parent component without following symlinks.
#[doc(hidden)]
pub fn validate_cache_file_parent(path: &Path) -> io::Result<()> {
    let parent = checked_cache_parent(path)?;
    #[cfg(unix)]
    {
        open_directory_no_follow(parent)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cache parent is missing"))?;
    }
    #[cfg(not(unix))]
    {
        validate_no_reparse_components(parent)?;
        let metadata = fs::symlink_metadata(parent)?;
        if !metadata.file_type().is_dir() || metadata_is_reparse_point(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cache parent is a symlink or reparse point",
            ));
        }
    }
    Ok(())
}

/// Create missing parent components without traversing static symlinks.
#[doc(hidden)]
pub fn prepare_cache_file_parent(path: &Path) -> io::Result<()> {
    let parent = checked_cache_parent(path)?;
    #[cfg(unix)]
    {
        open_or_create_directory_no_follow(parent)?;
    }
    #[cfg(not(unix))]
    {
        validate_no_reparse_components(parent)?;
        fs::create_dir_all(parent)?;
        validate_no_reparse_components(parent)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_private_create_mode(_options: &mut fs::OpenOptions) {}

#[cfg(not(unix))]
fn set_private_permissions(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

/// Recursively remove leftover `*.part` files under [`CACHE_IMGS_DIR`].
///
/// Symlinked files and directories are ignored, preventing cleanup from
/// escaping cache root. Call once before downloads begin.
pub fn sweep_stale_temp_files() {
    sweep_stale_temp_files_in(Path::new(CACHE_IMGS_DIR));
}

#[cfg(not(unix))]
fn sweep_stale_temp_files_in(root: &Path) {
    if validate_no_reparse_components(root).is_err() {
        return;
    }
    if !fs::symlink_metadata(root).is_ok_and(|metadata| {
        metadata.file_type().is_dir() && !metadata_is_reparse_point(&metadata)
    }) {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if metadata_is_reparse_point(&metadata) {
            continue;
        }
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            sweep_stale_temp_files_in(&entry.path());
        } else if file_type.is_file()
            && entry
                .path()
                .to_str()
                .is_some_and(|path| path.ends_with(TEMP_FILE_SUFFIX))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(unix)]
fn sweep_stale_temp_files_in(root: &Path) {
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, openat, statat, unlinkat};
    use rustix::io::Errno;

    fn sweep(directory: &impl rustix::fd::AsFd) -> io::Result<()> {
        let mut entries = Dir::read_from(directory).map_err(io::Error::from)?;
        while let Some(entry) = entries.read() {
            let entry = entry.map_err(io::Error::from)?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let stat = match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(Errno::NOENT) => continue,
                Err(err) => return Err(err.into()),
            };
            let file_type = FileType::from_raw_mode(stat.st_mode);
            if file_type.is_dir() {
                let child = match openat(
                    directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) {
                    Ok(child) => child,
                    Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => continue,
                    Err(err) => return Err(err.into()),
                };
                sweep(&child)?;
            } else if file_type.is_file() && name.to_bytes().ends_with(TEMP_FILE_SUFFIX.as_bytes())
            {
                match unlinkat(directory, name, AtFlags::empty()) {
                    Ok(()) | Err(Errno::NOENT) => {}
                    Err(err) => return Err(err.into()),
                }
            }
        }
        Ok(())
    }

    if let Ok(Some(directory)) = open_directory_no_follow(root) {
        let _ = sweep(&directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persist_database(database: &mut AccessDatabase) -> io::Result<()> {
        #[cfg(unix)]
        {
            fs::create_dir_all(&database.root)?;
            let root = open_directory_no_follow(&database.root)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cache root disappeared"))?;
            database.persist_at(&root)
        }
        #[cfg(not(unix))]
        {
            database.persist()
        }
    }

    #[test]
    fn access_database_survives_restart_and_removes_idle_files() {
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join("emoji/steam/stale.png");
        let fresh = root.path().join("emoji/steam/fresh.png");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, b"stale").unwrap();
        fs::write(&fresh, b"fresh").unwrap();

        let mut database = AccessDatabase::load(root.path().to_owned()).unwrap();
        database.record(&stale, 100).unwrap();
        database.record(&fresh, 1_000).unwrap();
        persist_database(&mut database).unwrap();

        let mut restarted = AccessDatabase::load(root.path().to_owned()).unwrap();
        let stats = cleanup_cache_at(&mut restarted, Duration::from_secs(500), 1_200).unwrap();
        assert_eq!(stats.removed_files, 1);
        assert!(!stale.exists());
        assert!(fresh.exists());
        assert_eq!(restarted.entries.len(), 1);
    }

    #[test]
    fn encoded_paths_round_trip_and_reject_parent_components() {
        let path = Path::new("emoji/steam/foo bar.png");
        assert_eq!(
            decode_relative_path(&encode_relative_path(path).unwrap()).as_deref(),
            Some(path)
        );
        assert!(!is_safe_relative_path(Path::new("../outside")));
        assert!(!is_safe_relative_path(Path::new("/outside")));
    }

    #[test]
    fn sweep_removes_regular_temp_files() {
        let root = tempfile::tempdir().unwrap();
        let temp = root.path().join("nested/download.part");
        fs::create_dir_all(temp.parent().unwrap()).unwrap();
        fs::write(&temp, b"partial").unwrap();
        sweep_stale_temp_files_in(root.path());
        assert!(!temp.exists());
    }

    #[cfg(unix)]
    #[test]
    fn sweep_does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let external_temp = outside.path().join("keep.part");
        fs::write(&external_temp, b"not cache data").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();

        sweep_stale_temp_files_in(root.path());
        assert!(external_temp.exists());
    }

    #[cfg(unix)]
    #[test]
    fn destructive_cleanup_rejects_symlinked_root_components() {
        use std::os::unix::fs::symlink;

        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_cache = outside.path().join("imgs");
        fs::create_dir(&outside_cache).unwrap();
        let external_temp = outside_cache.join("keep.part");
        let external_image = outside_cache.join("keep.png");
        fs::write(&external_temp, b"partial").unwrap();
        fs::write(&external_image, b"image").unwrap();

        // Parent component is symlink; final `imgs` component looks like a
        // normal directory to path-based metadata calls.
        symlink(outside.path(), base.path().join(".cache")).unwrap();
        let linked_root = base.path().join(".cache/imgs");
        sweep_stale_temp_files_in(&linked_root);
        assert!(external_temp.exists());

        assert!(AccessDatabase::load(linked_root.clone()).is_err());
        let mut database = AccessDatabase {
            root: linked_root,
            entries: HashMap::new(),
        };
        assert!(
            cleanup_cache_at(&mut database, Duration::ZERO, unix_time(SystemTime::now())).is_err()
        );
        assert!(external_image.exists());
    }

    #[cfg(unix)]
    #[test]
    fn access_record_does_not_create_through_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let base = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), base.path().join(".cache")).unwrap();
        let linked_root = base.path().join(".cache/imgs");
        let mut database = AccessDatabase {
            root: linked_root.clone(),
            entries: HashMap::new(),
        };

        assert!(
            database
                .record(
                    &linked_root.join("avatar.png"),
                    ACCESS_WRITE_GRANULARITY.as_secs()
                )
                .is_err()
        );
        assert!(!outside.path().join("imgs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn access_database_load_rejects_symlink_file() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), root.path().join(ACCESS_DB_FILE)).unwrap();

        assert!(AccessDatabase::load(root.path().to_owned()).is_err());
    }
}
