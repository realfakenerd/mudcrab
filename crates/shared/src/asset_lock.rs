//! Advisory locks for asset directories.
//!
//! The converter and the runtime coordinate access to an asset directory
//! through a small sibling lock file (see [`asset_lock_path`]), so a lock can
//! be taken even before the directory itself exists. Acquisition is
//! nonblocking: when another process holds an incompatible lock, the call
//! fails immediately with [`AssetLockError`] instead of waiting.
//!
//! [`AssetLock`] is an RAII guard. The underlying lock file stays open — and
//! the lock stays held — for the guard's lifetime, and is released when the
//! guard is dropped.
//!
//! Locking uses only the stable [`File::try_lock`] and
//! [`File::try_lock_shared`] APIs; no extra dependencies required.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

/// Suffix appended to an asset directory name to form its sibling lock file.
///
/// For asset directory `/data/assets`, the lock file is `/data/assets.lock`.
pub const ASSET_LOCK_FILE_SUFFIX: &str = ".lock";

/// Strength of an [`AssetLock`].
///
/// Any number of [`Shared`](AssetLockMode::Shared) holders may coexist, while
/// an [`Exclusive`](AssetLockMode::Exclusive) holder excludes every other
/// holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetLockMode {
    /// Non-exclusive read access.
    Shared,
    /// Exclusive write access.
    Exclusive,
}

impl std::fmt::Display for AssetLockMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssetLockMode::Shared => f.write_str("shared-read"),
            AssetLockMode::Exclusive => f.write_str("exclusive-write"),
        }
    }
}

/// RAII guard holding an advisory lock on an asset directory.
///
/// Acquire with [`AssetLock::acquire_shared`] for reads or
/// [`AssetLock::acquire_exclusive`] for writes. Both attempts are nonblocking
/// and fail with [`AssetLockError`] when another process holds an
/// incompatible lock. Dropping the guard releases the lock.
#[derive(Debug)]
pub struct AssetLock {
    _file: File,
    asset_dir: PathBuf,
    lock_path: PathBuf,
    mode: AssetLockMode,
}

impl AssetLock {
    /// Acquires a nonblocking shared lock for reading `asset_dir`.
    ///
    /// Succeeds alongside other shared holders; fails when another process
    /// holds an exclusive lock.
    pub fn acquire_shared(asset_dir: impl AsRef<Path>) -> Result<Self, AssetLockError> {
        Self::acquire(asset_dir.as_ref(), AssetLockMode::Shared)
    }

    /// Acquires a nonblocking exclusive lock for writing `asset_dir`.
    ///
    /// Fails when another process holds any shared or exclusive lock.
    pub fn acquire_exclusive(asset_dir: impl AsRef<Path>) -> Result<Self, AssetLockError> {
        Self::acquire(asset_dir.as_ref(), AssetLockMode::Exclusive)
    }

    /// The asset directory this lock guards.
    pub fn asset_dir(&self) -> &Path {
        &self.asset_dir
    }

    /// The sibling lock file backing this lock.
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// The strength of this lock.
    pub fn mode(&self) -> AssetLockMode {
        self.mode
    }

    fn acquire(asset_dir: &Path, mode: AssetLockMode) -> Result<Self, AssetLockError> {
        let resolved = resolve_asset_path(asset_dir).map_err(|source| {
            AssetLockError::io(asset_dir, &asset_lock_path(asset_dir), mode, source)
        })?;
        let lock_path = asset_lock_path(&resolved);
        let file = match mode {
            AssetLockMode::Shared => File::open(&lock_path).or_else(|error| {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error);
                }
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&lock_path)
            }),
            AssetLockMode::Exclusive => OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path),
        }
        .map_err(|source| AssetLockError::io(asset_dir, &lock_path, mode, source))?;
        let result = match mode {
            AssetLockMode::Shared => file.try_lock_shared(),
            AssetLockMode::Exclusive => file.try_lock(),
        };
        match result {
            Ok(()) => Ok(AssetLock {
                _file: file,
                asset_dir: resolved,
                lock_path,
                mode,
            }),
            Err(TryLockError::WouldBlock) => Err(AssetLockError::held(asset_dir, &lock_path, mode)),
            Err(TryLockError::Error(source)) => {
                Err(AssetLockError::io(asset_dir, &lock_path, mode, source))
            }
        }
    }
}

/// Computes the sibling lock file guarding `asset_dir`.
///
/// `/data/assets` maps to `/data/assets.lock`; a trailing separator is
/// ignored, so `/data/assets/` maps to the same file. The asset directory
/// itself does not need to exist — only its parent must exist when the lock
/// is acquired.
pub fn asset_lock_path(asset_dir: &Path) -> PathBuf {
    let resolved = resolve_asset_path(asset_dir).ok();
    let asset_dir = resolved.as_deref().unwrap_or(asset_dir);
    match asset_dir.file_name() {
        Some(name) => {
            let mut lock_name = name.to_os_string();
            lock_name.push(ASSET_LOCK_FILE_SUFFIX);
            asset_dir.with_file_name(lock_name)
        }
        None => {
            let mut lock = asset_dir.as_os_str().to_owned();
            lock.push(ASSET_LOCK_FILE_SUFFIX);
            PathBuf::from(lock)
        }
    }
}

/// Resolves existing symlinks and normalizes a path whose final components may
/// not exist yet. Resolution errors other than missing components propagate.
pub fn resolve_asset_path(path: &Path) -> std::io::Result<PathBuf> {
    use std::path::Component;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::Normal(name) => {
                resolved.push(name);
                match std::fs::canonicalize(&resolved) {
                    Ok(path) => resolved = path,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        if resolved
                            .symlink_metadata()
                            .is_ok_and(|metadata| metadata.is_symlink())
                        {
                            return Err(error);
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(resolved)
}

/// Failure to acquire an [`AssetLock`].
#[derive(Debug)]
pub enum AssetLockError {
    /// Another process holds an incompatible lock; retry after it finishes.
    Held {
        asset_dir: PathBuf,
        lock_path: PathBuf,
        mode: AssetLockMode,
    },
    /// The lock file could not be opened or locked due to an I/O error.
    Io {
        asset_dir: PathBuf,
        lock_path: PathBuf,
        mode: AssetLockMode,
        source: std::io::Error,
    },
}

impl AssetLockError {
    fn held(asset_dir: &Path, lock_path: &Path, mode: AssetLockMode) -> Self {
        AssetLockError::Held {
            asset_dir: asset_dir.to_owned(),
            lock_path: lock_path.to_owned(),
            mode,
        }
    }

    fn io(asset_dir: &Path, lock_path: &Path, mode: AssetLockMode, source: std::io::Error) -> Self {
        AssetLockError::Io {
            asset_dir: asset_dir.to_owned(),
            lock_path: lock_path.to_owned(),
            mode,
            source,
        }
    }

    /// Whether another process holds an incompatible lock, as opposed to an
    /// I/O failure while opening or locking the file.
    pub fn is_held(&self) -> bool {
        matches!(self, AssetLockError::Held { .. })
    }

    /// The asset directory the failed acquisition targeted.
    pub fn asset_dir(&self) -> &Path {
        match self {
            AssetLockError::Held { asset_dir, .. } => asset_dir,
            AssetLockError::Io { asset_dir, .. } => asset_dir,
        }
    }

    /// The sibling lock file the failed acquisition used.
    pub fn lock_path(&self) -> &Path {
        match self {
            AssetLockError::Held { lock_path, .. } => lock_path,
            AssetLockError::Io { lock_path, .. } => lock_path,
        }
    }

    /// The lock strength the failed acquisition requested.
    pub fn mode(&self) -> AssetLockMode {
        match self {
            AssetLockError::Held { mode, .. } => *mode,
            AssetLockError::Io { mode, .. } => *mode,
        }
    }
}

impl std::fmt::Display for AssetLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssetLockError::Held {
                asset_dir,
                lock_path,
                mode,
            } => write!(
                f,
                "cannot acquire {mode} lock on asset directory `{}`: \
                 another process holds an incompatible lock on `{}`; \
                 stop or wait for the other process, then retry",
                asset_dir.display(),
                lock_path.display(),
            ),
            AssetLockError::Io {
                asset_dir,
                lock_path,
                mode,
                source,
            } => write!(
                f,
                "cannot open lock file `{}` for {mode} lock on asset directory `{}`: {source}",
                lock_path.display(),
                asset_dir.display(),
            ),
        }
    }
}

impl std::error::Error for AssetLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AssetLockError::Held { .. } => None,
            AssetLockError::Io { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_asset_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock runs before the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "shared-asset-lock-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn remove_lock_file(asset_dir: &Path) {
        let _ = std::fs::remove_file(asset_lock_path(asset_dir));
    }

    #[test]
    fn shared_locks_coexist() {
        let asset_dir = scratch_asset_dir("shared");
        let first = AssetLock::acquire_shared(&asset_dir).expect("first shared lock");
        let second = AssetLock::acquire_shared(&asset_dir).expect("second shared lock");
        assert_eq!(first.lock_path(), second.lock_path());
        assert_eq!(first.lock_path(), asset_lock_path(&asset_dir));
        drop(first);
        drop(second);
        remove_lock_file(&asset_dir);
    }

    #[test]
    fn existing_shared_lock_uses_a_read_only_descriptor() {
        use std::fs;
        use std::io::Write;
        let asset_dir = scratch_asset_dir("read-only");
        drop(AssetLock::acquire_exclusive(&asset_dir).unwrap());
        let path = asset_lock_path(&asset_dir);
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
        let mut shared = AssetLock::acquire_shared(&asset_dir).unwrap();
        assert!(shared._file.write_all(b"must not write").is_err());
        assert!(AssetLock::acquire_shared(&asset_dir).is_ok());
        drop(shared);
        remove_lock_file(&asset_dir);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_parent_allows_provisioned_readers_but_missing_lock_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let parent = scratch_asset_dir("read-only-parent");
        std::fs::create_dir(&parent).unwrap();
        let assets = parent.join("assets");
        drop(AssetLock::acquire_exclusive(&assets).unwrap());
        std::fs::set_permissions(
            asset_lock_path(&assets),
            std::fs::Permissions::from_mode(0o444),
        )
        .unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let reader = AssetLock::acquire_shared(&assets).unwrap();
        let missing = AssetLock::acquire_shared(parent.join("missing"));
        let writer = AssetLock::acquire_exclusive(&assets);
        // Root bypasses mode bits; the descriptor-level test still covers it.
        let can_write = File::create(parent.join("permission-probe")).is_ok();
        drop(reader);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&parent).unwrap();
        if !can_write {
            assert!(matches!(missing, Err(AssetLockError::Io { .. })));
            assert!(matches!(writer, Err(AssetLockError::Io { .. })));
        }
    }

    #[test]
    fn exclusive_conflicts_with_shared() {
        let asset_dir = scratch_asset_dir("exclusive");
        let shared = AssetLock::acquire_shared(&asset_dir).expect("shared lock");
        let error = AssetLock::acquire_exclusive(&asset_dir).expect_err("exclusive must conflict");
        assert!(error.is_held(), "expected a held error, got {error:?}");
        assert_eq!(error.asset_dir(), asset_dir);
        assert_eq!(error.mode(), AssetLockMode::Exclusive);
        drop(shared);
        remove_lock_file(&asset_dir);
    }

    #[test]
    fn released_lock_can_be_reacquired() {
        let asset_dir = scratch_asset_dir("reacquire");
        assert!(
            !asset_dir.exists(),
            "the asset directory must stay absent so the test covers the missing-directory case"
        );
        let lock_path = asset_lock_path(&asset_dir);
        {
            let _held = AssetLock::acquire_exclusive(&asset_dir).expect("exclusive lock");
        }
        let reacquired = AssetLock::acquire_exclusive(&asset_dir).expect("reacquire after release");
        assert_eq!(reacquired.lock_path(), lock_path);
        drop(reacquired);
        remove_lock_file(&asset_dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_share_the_asset_lock() {
        let root = scratch_asset_dir("aliases");
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let held = AssetLock::acquire_shared(&real).unwrap();
        assert!(AssetLock::acquire_exclusive(&alias).unwrap_err().is_held());
        assert_eq!(asset_lock_path(&alias), held.lock_path());
        drop(held);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unresolved_parent_components_cannot_hide_an_asset_path() {
        let root = scratch_asset_dir("resolve");
        std::fs::create_dir(&root).unwrap();
        let resolved = resolve_asset_path(&root.join("new/../assets/report.json")).unwrap();
        assert_eq!(
            resolved,
            root.canonicalize().unwrap().join("assets/report.json")
        );
        assert!(!root.join("new").exists());
        std::fs::remove_dir(root).unwrap();
    }
}
