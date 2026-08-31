//! Atomic file writes, shared by the managed-cache marker, the signature
//! sidecar, and downstream identifier caches (e.g. the telemetry agent id).

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

fn sibling_config_lock_path(path: &Path) -> PathBuf {
    match path.file_name() {
        Some(name) => path.with_file_name(format!("{}.lock", name.to_string_lossy())),
        None => path.with_extension("lock"),
    }
}

fn path_is_under_grok_home(path: &Path, home: &Path) -> bool {
    if path.starts_with(home) {
        return true;
    }
    match (dunce::canonicalize(path), dunce::canonicalize(home)) {
        (Ok(canonical), Ok(home)) => canonical.starts_with(&home),
        _ => false,
    }
}

/// Lock path for a config document.
///
/// User/managed files under [`crate::grok_home`] keep a sibling
/// `config.toml.lock`. Project-scoped configs (e.g. `<repo>/.grok/config.toml`)
/// use a hashed lock under `$GROK_HOME/locks/config/` so MCP mutations never
/// leave untracked `.lock` artifacts in the working tree (#532 review).
pub fn config_lock_path(path: &Path) -> PathBuf {
    let home = crate::grok_home();
    if path_is_under_grok_home(path, &home) {
        return sibling_config_lock_path(path);
    }
    let digest = blake3::hash(path.to_string_lossy().as_bytes());
    home.join("locks")
        .join("config")
        .join(format!("{}.lock", digest.to_hex()))
}

/// Exclusive advisory lock on the path from [`config_lock_path`].
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

/// Resolve `path` to a stable destination, then lock that destination.
///
/// Retries if the logical path retargets while the lock is being acquired so
/// the held lock and the I/O path always refer to the same file.
pub fn lock_config_destination(path: &Path) -> std::io::Result<(File, PathBuf)> {
    for _ in 0..8 {
        let dest = resolve_write_path(path)?;
        let lock = lock_config_file(&dest)?;
        let dest2 = resolve_write_path(path)?;
        if dest == dest2 {
            return Ok((lock, dest));
        }
    }
    Err(std::io::Error::other(
        "config destination changed while acquiring lock",
    ))
}

/// If `path` exists as a symlink, return its canonical target so a later
/// rename updates the referent instead of replacing the link. A missing path
/// is resolved through the nearest existing ancestor so symlink aliases of
/// that ancestor hash to the same lock destination (#532 review). A dangling
/// symlink is an error.
pub fn resolve_write_path(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => dunce::canonicalize(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(canonicalize_missing_path(path)?)
        }
        Err(e) => Err(e),
    }
}

/// Walk up from a missing `path` until an existing ancestor can be
/// canonicalized, then re-append the missing suffix.
fn canonicalize_missing_path(path: &Path) -> std::io::Result<PathBuf> {
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match dunce::canonicalize(&cursor) {
            Ok(canonical) => {
                let mut out = canonical;
                for part in suffix.iter().rev() {
                    out.push(part);
                }
                return Ok(out);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let name = cursor
                    .file_name()
                    .map(|n| n.to_os_string())
                    .filter(|n| !n.is_empty());
                let Some(name) = name else {
                    return Ok(path.to_path_buf());
                };
                let Some(parent) = cursor.parent() else {
                    return Ok(path.to_path_buf());
                };
                if parent.as_os_str().is_empty() {
                    return Ok(path.to_path_buf());
                }
                suffix.push(name);
                cursor = parent.to_path_buf();
            }
            Err(err) => return Err(err),
        }
    }
}

/// Atomic temp + rename so a torn write can't leave a half-written file. The temp
/// name is unique per writer (pid + counter) and `create_new`, so concurrent
/// writers don't collide. `mode` (unix only) is applied at temp-file creation
/// and then again with `set_permissions` so umask cannot strip group bits
/// (e.g. 0640 with umask 077). Existing symlinks are followed so the write
/// updates the target instead of replacing the link.
///
/// Callers that already pinned a destination must use [`write_atomically_at`]
/// so a later symlink substitution cannot redirect the rename.
pub fn write_atomically(
    final_path: &Path,
    contents: &str,
    mode: Option<u32>,
) -> std::io::Result<()> {
    let final_path = resolve_write_path(final_path)?;
    write_atomically_at(&final_path, contents, mode)
}

/// Like [`write_atomically`], but writes `final_path` verbatim (no canonicalize).
pub fn write_atomically_at(
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
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        drop(file);
        std::fs::rename(&tmp, final_path)
    })();
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
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::state_home::StateHomeGuard::pin(home.path());
        let path = home.path().join("config.toml");
        std::fs::write(&path, "[ui]\n").unwrap();
        let held = lock_config_file(&path).unwrap();
        assert!(config_lock_path(&path).is_file());
        assert_eq!(
            config_lock_path(&path),
            sibling_config_lock_path(&path),
            "locks under grok_home stay as siblings"
        );
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
    fn project_config_locks_live_outside_the_working_tree() {
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::state_home::StateHomeGuard::pin(home.path());
        let repo = tempfile::tempdir().unwrap();
        let project = repo.path().join(".grok").join("config.toml");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, "[mcp_servers]\n").unwrap();
        let held = lock_config_file(&project).unwrap();
        let lock = config_lock_path(&project);
        assert!(lock.starts_with(home.path().join("locks").join("config")));
        assert!(
            !lock.starts_with(repo.path()),
            "project locks must not land in the working tree"
        );
        assert!(!repo.path().join(".grok").join("config.toml.lock").exists());
        drop(held);
    }

    #[test]
    fn lock_config_destination_pins_resolved_path() {
        let home = tempfile::tempdir().unwrap();
        let _pin = crate::state_home::StateHomeGuard::pin(home.path());
        let path = home.path().join("config.toml");
        std::fs::write(&path, "[ui]\n").unwrap();
        let (held, dest) = lock_config_destination(&path).unwrap();
        assert_eq!(dest, resolve_write_path(&path).unwrap());
        drop(held);
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
    fn resolve_write_path_canonicalizes_missing_path_via_symlinked_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real-root");
        let alias = dir.path().join("alias-root");
        std::fs::create_dir(&real_root).unwrap();
        std::os::unix::fs::symlink(&real_root, &alias).unwrap();
        // No `.grok/` yet — canonicalize through the symlinked ancestor and
        // keep the missing suffix so both aliases share one lock hash.
        let via_alias = alias.join(".grok").join("config.toml");
        let via_real = real_root.join(".grok").join("config.toml");
        let pinned_alias = resolve_write_path(&via_alias).unwrap();
        let pinned_real = resolve_write_path(&via_real).unwrap();
        assert_eq!(pinned_alias, pinned_real);
        assert_eq!(
            pinned_alias,
            dunce::canonicalize(&real_root).unwrap().join(".grok").join("config.toml")
        );
        assert_eq!(
            config_lock_path(&pinned_alias),
            config_lock_path(&pinned_real),
            "project locks must not diverge across symlink aliases"
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
    fn write_atomically_applies_saved_mode_after_umask() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        unsafe extern "C" {
            fn umask(mask: u32) -> u32;
        }
        let prev = unsafe { umask(0o077) };
        let result = write_atomically(&path, "new\n", Some(0o640));
        unsafe { umask(prev) };
        result.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "umask must not strip dest group bits");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
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
        write_atomically_at(&pinned, "pinned\n", None).unwrap();
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "pinned\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "two\n");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "two\n");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomically_at_does_not_follow_dest_replaced_with_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("config.toml");
        let other = dir.path().join("other.toml");
        std::fs::write(&dest, "orig\n").unwrap();
        std::fs::write(&other, "other\n").unwrap();
        let pinned = dest.clone();
        std::fs::remove_file(&dest).unwrap();
        std::os::unix::fs::symlink(&other, &dest).unwrap();
        write_atomically_at(&pinned, "pinned\n", None).unwrap();
        assert!(
            !dest.symlink_metadata().unwrap().file_type().is_symlink(),
            "verbatim write must replace the symlink, not follow it"
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "pinned\n");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "other\n");
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
        write_atomically_at(&pinned, "pinned\n", None).unwrap();
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
