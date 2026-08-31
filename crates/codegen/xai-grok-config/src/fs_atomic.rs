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

/// Atomic temp + rename so a torn write can't leave a half-written file. The temp
/// name is unique per writer (pid + counter) and `create_new`, so concurrent
/// writers don't collide. `mode` (unix only) is applied at temp-file creation, so
/// the final file never exists with looser permissions.
pub fn write_atomically(
    final_path: &Path,
    contents: &str,
    mode: Option<u32>,
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

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
}
