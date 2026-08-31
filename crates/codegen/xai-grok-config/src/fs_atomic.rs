//! Atomic file writes, shared by the managed-cache marker, the signature
//! sidecar, and downstream identifier caches (e.g. the telemetry agent id).

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// Sibling lock path for a config document (`config.toml` → `config.toml.lock`).
pub fn config_lock_path(path: &Path) -> PathBuf {
    match path.file_name() {
        Some(name) => path.with_file_name(format!("{}.lock", name.to_string_lossy())),
        None => path.with_extension("lock"),
    }
}

/// Exclusive advisory lock on a sibling `*.lock` file.
///
/// Released when the returned [`File`] is dropped. Portable: Unix `flock` and
/// Windows `LockFileEx` via `fs2`.
pub fn lock_config_file(path: &Path) -> std::io::Result<File> {
    use fs2::FileExt as _;
    let lock_path = config_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    file.lock_exclusive()?;
    Ok(file)
}

/// If `path` exists as a symlink, return its canonical target so a later
/// rename updates the referent instead of replacing the link. A missing path
/// is returned unchanged. A dangling symlink is an error.
pub fn resolve_write_path(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => dunce::canonicalize(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => match dunce::canonicalize(parent) {
                Ok(parent) => Ok(parent.join(path.file_name().unwrap_or_default())),
                Err(parent_err) if parent_err.kind() == std::io::ErrorKind::NotFound => {
                    Ok(path.to_path_buf())
                }
                Err(parent_err) => Err(parent_err),
            },
            _ => Ok(path.to_path_buf()),
        },
        Err(e) => Err(e),
    }
}

/// Atomic temp + rename so a torn write can't leave a half-written file. The temp
/// name is unique per writer (pid + counter) and `create_new`, so concurrent
/// writers don't collide. `mode` (unix only) is applied at temp-file creation, so
/// the final file never exists with looser permissions. Existing symlinks are
/// followed so the write updates the target instead of replacing the link.
pub fn write_atomically(
    final_path: &Path,
    contents: &str,
    mode: Option<u32>,
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

    let final_path = resolve_write_path(final_path)?;
    let dir = final_path.parent().unwrap_or_else(|| Path::new("."));
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    let nonce = WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{name}.{}.{nonce}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let result = options
        .open(&tmp)
        .and_then(|mut f| f.write_all(contents.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, final_path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt as _;

    #[test]
    fn lock_config_file_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ui]\n").unwrap();
        let held = lock_config_file(&path).unwrap();
        assert!(config_lock_path(&path).is_file());
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(config_lock_path(&path))
            .unwrap();
        assert!(second.try_lock_exclusive().is_err());
        drop(held);
        second.try_lock_exclusive().unwrap();
    }

    #[test]
    fn resolve_write_path_leaves_missing_path_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(
            resolve_write_path(&path).unwrap(),
            dunce::canonicalize(dir.path()).unwrap().join("config.toml")
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_atomically_follows_symlink_target() {
        let dir = tempfile::tempdir().unwrap();
        let target_dir = dir.path().join("dotfiles");
        std::fs::create_dir(&target_dir).unwrap();
        let target = target_dir.join("config.toml");
        let link = dir.path().join("config.toml");
        std::fs::write(&target, "old\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_atomically(&link, "new\n", Some(0o600)).unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "new\n");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomically_rejects_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("missing.toml");
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_atomically(&link, "new\n", None).is_err());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(!target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_atomically_on_pinned_target_ignores_later_retarget() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.toml");
        let second = dir.path().join("second.toml");
        let link = dir.path().join("config.toml");
        std::fs::write(&first, "one\n").unwrap();
        std::fs::write(&second, "two\n").unwrap();
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let pinned = resolve_write_path(&link).unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&second, &link).unwrap();
        write_atomically(&pinned, "pinned\n", None).unwrap();
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "pinned\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "two\n");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "two\n");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_write_path_canonicalizes_symlinked_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let real_home = dir.path().join("dotfiles");
        let linked_home = dir.path().join("home");
        std::fs::create_dir(&real_home).unwrap();
        std::os::unix::fs::symlink(&real_home, &linked_home).unwrap();
        let file = linked_home.join("config.toml");
        std::fs::write(&file, "old\n").unwrap();
        let pinned = resolve_write_path(&file).unwrap();
        assert_eq!(pinned, dunce::canonicalize(&file).unwrap());
        std::fs::remove_file(&linked_home).unwrap();
        let other = dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("config.toml"), "other\n").unwrap();
        std::os::unix::fs::symlink(&other, &linked_home).unwrap();
        write_atomically(&pinned, "pinned\n", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(real_home.join("config.toml")).unwrap(),
            "pinned\n"
        );
        assert_eq!(
            std::fs::read_to_string(other.join("config.toml")).unwrap(),
            "other\n"
        );
    }
}
