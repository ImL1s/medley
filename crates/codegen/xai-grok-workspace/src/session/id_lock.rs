//! Owner-only per-session lease namespace under `$MEDLEY_HOME/.locks/session-ids`.
//!
//! Leaves are SHA-256 stems plus `.namespace.lock` / `.mutation.lock`. Both are
//! opened through one retained `session-ids` parent in that deterministic order.
//! There is no compatibility path for unshipped PR #332 hex lock names.
//!
//! v0.2.119 shipped no session-ID lock protocol. Upgrade requires a full
//! Medley process drain: already-running v0.2.119 processes cannot participate
//! in rolling coexistence with this namespace.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::Path;

use fs2::FileExt;
use sha2::{Digest, Sha256};

use super::anchored::AnchoredDirectory;

/// Cross-process ownership for one persisted session id.
///
/// Fresh creation and deletion take this exclusively. Loading and discovery
/// take shared leases so they can run together but cannot observe a provisional
/// directory before publication releases the exclusive claim.
#[derive(Debug)]
pub struct SessionIdLock {
    namespace: Option<File>,
    mutation: Option<File>,
}

impl SessionIdLock {
    /// Downgrade an exclusive claim to a lifetime shared mutation lease.
    pub fn transition_exclusive_to_lifetime_shared(&mut self) -> io::Result<()> {
        let mutation = self
            .mutation
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mutation lease missing"))?;
        FileExt::unlock(mutation)?;
        FileExt::lock_shared(mutation)?;
        if let Some(namespace) = self.namespace.as_ref() {
            FileExt::unlock(namespace)?;
        }
        self.namespace.take();
        Ok(())
    }
}

fn session_claim_lock_stem(session_id: &str) -> String {
    format!("{:x}", Sha256::digest(session_id.as_bytes()))
}

fn session_claim_lock_name(session_id: &str) -> String {
    format!("{}.namespace.lock", session_claim_lock_stem(session_id))
}

fn session_mutation_lock_name(session_id: &str) -> String {
    format!("{}.mutation.lock", session_claim_lock_stem(session_id))
}

fn open_or_create_anchored_child(
    parent: &AnchoredDirectory,
    name: &OsStr,
) -> io::Result<AnchoredDirectory> {
    match parent.create_child_dir(name) {
        Ok(child) => Ok(child),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => parent.open_child_dir(name),
        Err(error) => Err(error),
    }
}

/// Traverse/create `$root/.locks/session-ids` through retained handles and
/// tighten both directories to owner-only (`0700` / protected current-user ACL).
pub fn open_session_id_lock_directory(root_dir: &Path) -> io::Result<AnchoredDirectory> {
    let root = AnchoredDirectory::open_root(root_dir)?;
    let locks = open_or_create_anchored_child(&root, OsStr::new(".locks"))?;
    locks.ensure_owner_only()?;
    let session_ids = open_or_create_anchored_child(&locks, OsStr::new("session-ids"))?;
    session_ids.ensure_owner_only()?;
    Ok(session_ids)
}

/// Open namespace then mutation leaves through one retained `session-ids` parent.
pub fn open_session_id_lock_files(root_dir: &Path, session_id: &str) -> io::Result<(File, File)> {
    let lock_dir = open_session_id_lock_directory(root_dir)?;
    let namespace = lock_dir
        .open_or_create_owner_only_child_file(OsStr::new(&session_claim_lock_name(session_id)))?;
    let mutation = lock_dir.open_or_create_owner_only_child_file(OsStr::new(
        &session_mutation_lock_name(session_id),
    ))?;
    Ok((namespace, mutation))
}

/// Exclusive namespace + mutation lease. Callers hold this across create/delete.
pub fn acquire_session_id_lock_sync(
    root_dir: &Path,
    session_id: &str,
) -> io::Result<SessionIdLock> {
    let (namespace, mutation) = open_session_id_lock_files(root_dir, session_id)?;
    FileExt::lock_exclusive(&namespace)?;
    FileExt::lock_exclusive(&mutation)?;
    Ok(SessionIdLock {
        namespace: Some(namespace),
        mutation: Some(mutation),
    })
}

/// Shared namespace + mutation lease for published-session reads.
pub fn acquire_session_id_read_lock_sync(
    root_dir: &Path,
    session_id: &str,
) -> io::Result<SessionIdLock> {
    let (namespace, mutation) = open_session_id_lock_files(root_dir, session_id)?;
    FileExt::lock_shared(&namespace)?;
    FileExt::lock_shared(&mutation)?;
    Ok(SessionIdLock {
        namespace: Some(namespace),
        mutation: Some(mutation),
    })
}

/// Non-blocking exclusive acquisition. `Ok(None)` means another same-version
/// holder already owns the lease.
pub fn try_acquire_session_id_write_lock_sync(
    root_dir: &Path,
    session_id: &str,
) -> io::Result<Option<SessionIdLock>> {
    let (namespace, mutation) = open_session_id_lock_files(root_dir, session_id)?;
    match FileExt::try_lock_exclusive(&namespace) {
        Ok(()) => match FileExt::try_lock_exclusive(&mutation) {
            Ok(()) => Ok(Some(SessionIdLock {
                namespace: Some(namespace),
                mutation: Some(mutation),
            })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        },
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

/// Non-blocking shared acquisition used by discovery to omit a session while
/// its creator still holds the exclusive claim.
pub fn try_acquire_session_id_read_lock_sync(
    root_dir: &Path,
    session_id: &str,
) -> io::Result<Option<SessionIdLock>> {
    let (namespace, mutation) = open_session_id_lock_files(root_dir, session_id)?;
    match FileExt::try_lock_shared(&namespace) {
        Ok(()) => match FileExt::try_lock_shared(&mutation) {
            Ok(()) => Ok(Some(SessionIdLock {
                namespace: Some(namespace),
                mutation: Some(mutation),
            })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        },
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

/// Acquire a source-shared and target-exclusive lease in one deterministic
/// global order so A-to-B and B-to-A copies cannot deadlock.
pub fn acquire_ordered_copy_locks_sync(
    root_dir: &Path,
    source_id: &str,
    target_id: &str,
) -> io::Result<(SessionIdLock, SessionIdLock)> {
    if source_id == target_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fork source and target session ids must differ",
        ));
    }
    let source_name = session_claim_lock_name(source_id);
    let target_name = session_claim_lock_name(target_id);
    if source_name < target_name {
        let source = acquire_session_id_read_lock_sync(root_dir, source_id)?;
        let target = acquire_session_id_lock_sync(root_dir, target_id)?;
        Ok((source, target))
    } else {
        let target = acquire_session_id_lock_sync(root_dir, target_id)?;
        let source = acquire_session_id_read_lock_sync(root_dir, source_id)?;
        Ok((source, target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000355";

    fn lock_dir(root: &Path) -> PathBuf {
        root.join(".locks").join("session-ids")
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn lock_stem_is_sha256_not_raw_or_hex_session_id() {
        let stem = session_claim_lock_stem(SESSION_ID);
        assert_eq!(stem.len(), 64);
        assert!(!stem.contains('-'));
        assert_ne!(stem, SESSION_ID.replace('-', ""));
        assert_eq!(
            session_claim_lock_name(SESSION_ID),
            format!("{stem}.namespace.lock")
        );
        assert_eq!(
            session_mutation_lock_name(SESSION_ID),
            format!("{stem}.mutation.lock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_permissive_dirs_and_files_are_tightened() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let locks = temp.path().join(".locks");
        let session_ids = locks.join("session-ids");
        std::fs::create_dir_all(&session_ids).unwrap();
        std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&session_ids, std::fs::Permissions::from_mode(0o755)).unwrap();

        let namespace_name = session_claim_lock_name(SESSION_ID);
        let mutation_name = session_mutation_lock_name(SESSION_ID);
        let namespace_path = session_ids.join(&namespace_name);
        let mutation_path = session_ids.join(&mutation_name);
        std::fs::write(&namespace_path, b"").unwrap();
        std::fs::write(&mutation_path, b"").unwrap();
        std::fs::set_permissions(&namespace_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&mutation_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let (namespace, mutation) = open_session_id_lock_files(temp.path(), SESSION_ID).unwrap();
        assert_eq!(mode(&locks), 0o700);
        assert_eq!(mode(&session_ids), 0o700);
        assert_eq!(
            namespace.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            mutation.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "spawned by session_id_lock_creation_ignores_permissive_umask"]
    fn subprocess_session_id_lock_under_umask_zero() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let Ok(root) = std::env::var("GROK_TEST_SESSION_ID_LOCK_ROOT") else {
            return;
        };
        unsafe {
            libc::umask(0);
        }
        let (namespace, mutation) =
            open_session_id_lock_files(std::path::Path::new(&root), SESSION_ID).unwrap();
        let root = std::path::Path::new(&root);
        assert_eq!(mode(&root.join(".locks")), 0o700);
        assert_eq!(mode(&lock_dir(root)), 0o700);
        assert_eq!(
            namespace.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            mutation.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(namespace.metadata().unwrap().uid(), unsafe {
            libc::geteuid()
        });
        assert_eq!(namespace.metadata().unwrap().nlink(), 1);
        assert_eq!(mutation.metadata().unwrap().nlink(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn session_id_lock_creation_ignores_permissive_umask() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path()).unwrap();

        #[allow(clippy::disallowed_methods)]
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "--nocapture",
                "session::id_lock::tests::subprocess_session_id_lock_under_umask_zero",
            ])
            .env("GROK_TEST_SESSION_ID_LOCK_ROOT", temp.path())
            .status()
            .expect("spawn session-id lock umask subprocess");
        assert!(status.success(), "umask subprocess failed: {status}");
    }

    #[cfg(unix)]
    #[test]
    fn locks_session_ids_and_leaf_symlinks_fail_closed_without_touching_sentinel() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("sentinel"), b"preserve").unwrap();

        symlink(outside.path(), temp.path().join(".locks")).unwrap();
        assert!(open_session_id_lock_files(temp.path(), SESSION_ID).is_err());
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"preserve"
        );
        assert!(!outside.path().join("session-ids").exists());

        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".locks")).unwrap();
        symlink(
            outside.path(),
            temp.path().join(".locks").join("session-ids"),
        )
        .unwrap();
        assert!(open_session_id_lock_files(temp.path(), SESSION_ID).is_err());
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"preserve"
        );

        let temp = tempfile::tempdir().unwrap();
        let session_ids = open_session_id_lock_directory(temp.path()).unwrap();
        let leaf = lock_dir(temp.path()).join(session_claim_lock_name(SESSION_ID));
        symlink(outside.path().join("sentinel"), &leaf).unwrap();
        assert!(
            session_ids
                .open_or_create_owner_only_child_file(OsStr::new(&session_claim_lock_name(
                    SESSION_ID
                )))
                .is_err()
        );
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"preserve"
        );
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_unix_leaf_is_rejected_without_changing_the_peer() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("peer");
        std::fs::write(&outside, b"peer-bytes").unwrap();
        let session_ids = open_session_id_lock_directory(temp.path()).unwrap();
        let leaf = lock_dir(temp.path()).join(session_claim_lock_name(SESSION_ID));
        std::fs::hard_link(&outside, &leaf).unwrap();

        assert!(
            session_ids
                .open_or_create_owner_only_child_file(OsStr::new(&session_claim_lock_name(
                    SESSION_ID
                )))
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"peer-bytes");
        {
            use std::os::unix::fs::MetadataExt as _;
            assert_eq!(std::fs::metadata(&outside).unwrap().nlink(), 2);
        }
    }

    #[cfg(unix)]
    #[test]
    fn parent_rename_between_retain_and_open_stays_bound_or_fails() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let moved = temp.path().join("moved");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), b"outside").unwrap();

        let retained = open_session_id_lock_directory(&root_path).unwrap();
        std::fs::rename(&root_path, &moved).unwrap();
        symlink(&outside, &root_path).unwrap();

        let namespace = retained
            .open_or_create_owner_only_child_file(OsStr::new(&session_claim_lock_name(SESSION_ID)))
            .unwrap();
        let mutation = retained
            .open_or_create_owner_only_child_file(OsStr::new(&session_mutation_lock_name(
                SESSION_ID,
            )))
            .unwrap();
        drop((namespace, mutation));

        assert!(
            lock_dir(&moved)
                .join(session_claim_lock_name(SESSION_ID))
                .is_file()
        );
        assert!(
            lock_dir(&moved)
                .join(session_mutation_lock_name(SESSION_ID))
                .is_file()
        );
        assert!(!outside.join("session-ids").exists());
        assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"outside");
    }

    #[test]
    fn exclusive_downgrade_allows_shared_readers() {
        let temp = tempfile::tempdir().unwrap();
        let mut lock = acquire_session_id_lock_sync(temp.path(), SESSION_ID).unwrap();
        lock.transition_exclusive_to_lifetime_shared().unwrap();
        assert!(
            try_acquire_session_id_read_lock_sync(temp.path(), SESSION_ID)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn genuine_same_version_exclusion_still_works() {
        let temp = tempfile::tempdir().unwrap();
        let first = acquire_session_id_lock_sync(temp.path(), SESSION_ID).unwrap();
        assert!(
            try_acquire_session_id_write_lock_sync(temp.path(), SESSION_ID)
                .unwrap()
                .is_none()
        );
        assert!(
            try_acquire_session_id_read_lock_sync(temp.path(), SESSION_ID)
                .unwrap()
                .is_none()
        );
        drop(first);

        let shared = acquire_session_id_read_lock_sync(temp.path(), SESSION_ID).unwrap();
        let second_shared = try_acquire_session_id_read_lock_sync(temp.path(), SESSION_ID)
            .unwrap()
            .expect("shared readers may coexist");
        assert!(
            try_acquire_session_id_write_lock_sync(temp.path(), SESSION_ID)
                .unwrap()
                .is_none()
        );
        drop((shared, second_shared));

        let exclusive = try_acquire_session_id_write_lock_sync(temp.path(), SESSION_ID)
            .unwrap()
            .expect("exclusive after readers drop");
        drop(exclusive);
    }

    #[test]
    fn canonical_lock_root_is_beside_sessions_not_under_it() {
        let temp = tempfile::tempdir().unwrap();
        let _ = acquire_session_id_lock_sync(temp.path(), SESSION_ID).unwrap();
        assert!(
            lock_dir(temp.path())
                .join(session_claim_lock_name(SESSION_ID))
                .is_file()
        );
        assert!(
            !temp
                .path()
                .join("sessions/.locks/session-ids")
                .join(session_claim_lock_name(SESSION_ID))
                .exists()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_locks_junction_fails_closed_without_touching_sentinel() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("sentinel"), b"preserve").unwrap();
        let junction = temp.path().join(".locks");
        let mut command = std::process::Command::new("cmd");
        command
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path());
        xai_tty_utils::detach_std_command(&mut command);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(open_session_id_lock_files(temp.path(), SESSION_ID).is_err());
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"preserve"
        );
        assert!(!outside.path().join("session-ids").exists());
    }

    #[test]
    fn ordered_copy_locks_use_one_global_order() {
        let temp = tempfile::tempdir().unwrap();
        let a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let (source, target) = acquire_ordered_copy_locks_sync(temp.path(), a, b).unwrap();
        drop((source, target));
        let (source, target) = acquire_ordered_copy_locks_sync(temp.path(), b, a).unwrap();
        drop((source, target));
        assert!(acquire_ordered_copy_locks_sync(temp.path(), a, a).is_err());
    }
}
