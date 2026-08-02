//! Resolution of the user-level state directory.
//!
//! This fork keeps its state in `~/.medley` so it never shares a directory with
//! an official Grok Build install: the fork writes provider-scoped credentials
//! and config keys that upstream does not understand, and a shared `~/.grok`
//! corrupts both. Resolution order:
//!
//! 1. `$MEDLEY_HOME`
//! 2. `$GROK_HOME` (accepted for compatibility with existing setups)
//! 3. `~/.medley`, when it exists
//! 4. `~/.grok`, when it exists and `~/.medley` does not — flagged
//!    [`StateDirSource::LegacyMigrationPending`] so an interactive run can offer
//!    the one-time copy, or [`StateDirSource::LegacyKept`] once the user has
//!    declined it (recorded by the [`KEEP_LEGACY_MARKER`] file)
//! 5. `~/.medley` otherwise
//!
//! The two directories are never used together: the migration *copies*, and
//! from the moment `~/.medley` exists rule 3 wins.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Directory name under the user's home.
pub const STATE_DIR_NAME: &str = ".medley";

/// Directory name shared with the official Grok Build install.
pub const LEGACY_STATE_DIR_NAME: &str = ".grok";

/// Environment override for the state directory.
pub const STATE_HOME_ENV: &str = "MEDLEY_HOME";

/// Compatibility environment override, honored after [`STATE_HOME_ENV`].
pub const LEGACY_STATE_HOME_ENV: &str = "GROK_HOME";

/// Written inside the legacy directory when the user declines the migration, so
/// the prompt does not come back every startup.
pub const KEEP_LEGACY_MARKER: &str = ".medley-keep-legacy";

/// Which rule produced the resolved state directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateDirSource {
    /// `$MEDLEY_HOME`.
    StateHomeEnv,
    /// `$GROK_HOME`.
    LegacyStateHomeEnv,
    /// `~/.medley` exists.
    Medley,
    /// `~/.grok` exists, `~/.medley` does not, and the user has not declined
    /// the migration yet.
    LegacyMigrationPending,
    /// `~/.grok` exists and the user chose to keep using it.
    LegacyKept,
    /// Neither directory exists — `~/.medley` will be created.
    Default,
}

impl StateDirSource {
    /// True when a one-time copy into `~/.medley` can still be offered.
    pub fn migration_pending(self) -> bool {
        matches!(self, Self::LegacyMigrationPending)
    }

    /// True when the directory came from an environment override.
    pub fn from_env(self) -> bool {
        matches!(self, Self::StateHomeEnv | Self::LegacyStateHomeEnv)
    }
}

/// A resolved state directory and the rule that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    pub path: PathBuf,
    pub source: StateDirSource,
}

/// The legacy directory to copy from and the directory to copy into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub legacy: PathBuf,
    pub target: PathBuf,
}

/// The home directory state paths hang off.
///
/// Falls back to the current directory when no home resolves, matching the
/// pre-fork behavior so a homeless environment still gets a usable directory.
///
/// Uses [`dunce::canonicalize`] instead of [`std::fs::canonicalize`]: on
/// Windows, std returns a verbatim path (`\\?\C:\Users\...`) which external
/// tools choke on — e.g. `git clone` rejects `\\?\` destinations with
/// "Invalid argument", breaking marketplace cache clones under the state
/// directory. `dunce` strips the prefix whenever the path is safely
/// representable in legacy form; on non-Windows it is identical to
/// `std::fs::canonicalize`.
pub fn home_root() -> PathBuf {
    #[allow(deprecated)]
    let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
    dunce::canonicalize(&home).unwrap_or(home)
}

/// Resolve the state directory from the live process environment.
pub fn resolve() -> StateDir {
    resolve_in(
        &home_root(),
        std::env::var_os(STATE_HOME_ENV).as_deref(),
        std::env::var_os(LEGACY_STATE_HOME_ENV).as_deref(),
    )
}

/// [`resolve`], but `None` when nothing genuinely anchors the directory —
/// neither environment override is set and no home directory is found.
///
/// Callers that must not silently fall back to a cwd-relative directory (which
/// would collide with a project's own `.grok` tree) use this instead.
pub fn resolve_user() -> Option<StateDir> {
    #[allow(deprecated)]
    let anchored = std::env::var_os(STATE_HOME_ENV).is_some()
        || std::env::var_os(LEGACY_STATE_HOME_ENV).is_some()
        || std::env::home_dir().is_some();
    anchored.then(resolve)
}

/// The home-anchored state directory, ignoring both environment overrides.
///
/// Callers use this to tell whether a resolved directory is the default one
/// without duplicating the precedence.
pub fn default_state_dir() -> StateDir {
    resolve_in(&home_root(), None, None)
}

/// [`resolve`] against an explicit home and explicit environment values.
///
/// Reads the filesystem (directory existence, migration marker) but takes no
/// process state, so it is directly testable.
pub fn resolve_in(home: &Path, state_env: Option<&OsStr>, legacy_env: Option<&OsStr>) -> StateDir {
    if let Some(path) = nonempty(state_env) {
        return StateDir {
            path,
            source: StateDirSource::StateHomeEnv,
        };
    }
    if let Some(path) = nonempty(legacy_env) {
        return StateDir {
            path,
            source: StateDirSource::LegacyStateHomeEnv,
        };
    }
    let medley = home.join(STATE_DIR_NAME);
    if medley.is_dir() {
        return StateDir {
            path: medley,
            source: StateDirSource::Medley,
        };
    }
    let legacy = home.join(LEGACY_STATE_DIR_NAME);
    if legacy.is_dir() {
        let source = if legacy.join(KEEP_LEGACY_MARKER).exists() {
            StateDirSource::LegacyKept
        } else {
            StateDirSource::LegacyMigrationPending
        };
        return StateDir {
            path: legacy,
            source,
        };
    }
    StateDir {
        path: medley,
        source: StateDirSource::Default,
    }
}

/// An empty or all-whitespace override is treated as unset — an exported but
/// empty `MEDLEY_HOME=` would otherwise resolve every state path to `""`.
fn nonempty(value: Option<&OsStr>) -> Option<PathBuf> {
    let value = value?;
    let has_content = value
        .to_str()
        .map_or_else(|| !value.is_empty(), |s| !s.trim().is_empty());
    has_content.then(|| PathBuf::from(value))
}

/// The pending one-time migration, when the legacy directory is in use and the
/// user has neither migrated nor declined.
pub fn pending_migration() -> Option<Migration> {
    let home = home_root();
    let resolved = resolve_in(
        &home,
        std::env::var_os(STATE_HOME_ENV).as_deref(),
        std::env::var_os(LEGACY_STATE_HOME_ENV).as_deref(),
    );
    resolved.source.migration_pending().then(|| Migration {
        legacy: resolved.path,
        target: home.join(STATE_DIR_NAME),
    })
}

/// What a migration copied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MigrationStats {
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    /// Sockets, FIFOs and other non-regular entries, which are runtime
    /// artifacts of a live session (e.g. `leader.sock`) and are not copied.
    pub skipped: usize,
}

/// Record that the user wants to keep using the legacy directory, so the
/// migration prompt does not repeat.
pub fn keep_legacy(legacy: &Path) -> std::io::Result<()> {
    std::fs::write(
        legacy.join(KEEP_LEGACY_MARKER),
        b"This directory is shared with an official Grok Build install.\n\
          Delete this file to be offered the one-time copy into ~/.medley again.\n",
    )
}

/// Recursively copy `legacy` into `target`, preserving permissions.
///
/// File modes carry over (`auth.json` stays owner-only), symlinks are recreated
/// rather than followed, and non-regular entries are skipped. `target` must not
/// already exist — a migration never merges into a directory that may hold
/// state from a different install.
pub fn migrate_copy(legacy: &Path, target: &Path) -> std::io::Result<MigrationStats> {
    if target.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} already exists", target.display()),
        ));
    }
    if target.starts_with(legacy) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to copy a directory into itself",
        ));
    }
    let mut stats = MigrationStats::default();
    copy_dir(legacy, target, &mut stats)?;
    Ok(stats)
}

fn copy_dir(from: &Path, to: &Path, stats: &mut MigrationStats) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    copy_permissions(from, to)?;
    stats.dirs += 1;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == OsStr::new(KEEP_LEGACY_MARKER) {
            continue;
        }
        let src = entry.path();
        let dst = to.join(&name);
        // `symlink_metadata` so a symlink is recreated rather than followed —
        // following would both duplicate data and turn a dangling link into an
        // error.
        let meta = std::fs::symlink_metadata(&src)?;
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            copy_symlink(&src, &dst)?;
            stats.symlinks += 1;
        } else if file_type.is_dir() {
            copy_dir(&src, &dst, stats)?;
        } else if file_type.is_file() {
            std::fs::copy(&src, &dst)?;
            stats.files += 1;
        } else {
            stats.skipped += 1;
        }
    }
    Ok(())
}

fn copy_permissions(from: &Path, to: &Path) -> std::io::Result<()> {
    let perms = std::fs::metadata(from)?.permissions();
    std::fs::set_permissions(to, perms)
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
}

#[cfg(windows)]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    let target = std::fs::read_link(src)?;
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(target, dst)
    } else {
        std::os::windows::fs::symlink_file(target, dst)
    }
}

#[cfg(not(any(unix, windows)))]
fn copy_symlink(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn state_home_env_wins_over_everything() {
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(STATE_DIR_NAME)).unwrap();
        std::fs::create_dir_all(home.path().join(LEGACY_STATE_DIR_NAME)).unwrap();
        let resolved = resolve_in(
            home.path(),
            Some(os("/custom/state")),
            Some(os("/legacy/state")),
        );
        assert_eq!(resolved.path, PathBuf::from("/custom/state"));
        assert_eq!(resolved.source, StateDirSource::StateHomeEnv);
    }

    #[test]
    fn legacy_home_env_is_honored_when_state_home_env_is_unset() {
        let home = TempDir::new().unwrap();
        let resolved = resolve_in(home.path(), None, Some(os("/legacy/state")));
        assert_eq!(resolved.path, PathBuf::from("/legacy/state"));
        assert_eq!(resolved.source, StateDirSource::LegacyStateHomeEnv);
    }

    #[test]
    fn empty_env_overrides_are_ignored() {
        let home = TempDir::new().unwrap();
        let resolved = resolve_in(home.path(), Some(os("")), Some(os("   ")));
        assert_eq!(resolved.path, home.path().join(STATE_DIR_NAME));
        assert_eq!(resolved.source, StateDirSource::Default);
    }

    #[test]
    fn existing_medley_dir_wins_over_existing_legacy_dir() {
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(STATE_DIR_NAME)).unwrap();
        std::fs::create_dir_all(home.path().join(LEGACY_STATE_DIR_NAME)).unwrap();
        let resolved = resolve_in(home.path(), None, None);
        assert_eq!(resolved.path, home.path().join(STATE_DIR_NAME));
        assert_eq!(resolved.source, StateDirSource::Medley);
    }

    #[test]
    fn legacy_only_resolves_to_legacy_with_migration_pending() {
        let home = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(LEGACY_STATE_DIR_NAME)).unwrap();
        let resolved = resolve_in(home.path(), None, None);
        assert_eq!(resolved.path, home.path().join(LEGACY_STATE_DIR_NAME));
        assert_eq!(resolved.source, StateDirSource::LegacyMigrationPending);
        assert!(resolved.source.migration_pending());
    }

    #[test]
    fn keep_legacy_marker_stops_the_migration_prompt() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy).unwrap();
        keep_legacy(&legacy).unwrap();
        let resolved = resolve_in(home.path(), None, None);
        assert_eq!(resolved.path, legacy);
        assert_eq!(resolved.source, StateDirSource::LegacyKept);
        assert!(!resolved.source.migration_pending());
    }

    #[test]
    fn neither_dir_present_defaults_to_medley() {
        let home = TempDir::new().unwrap();
        let resolved = resolve_in(home.path(), None, None);
        assert_eq!(resolved.path, home.path().join(STATE_DIR_NAME));
        assert_eq!(resolved.source, StateDirSource::Default);
    }

    #[test]
    fn no_home_falls_back_to_a_cwd_relative_dir() {
        // `home_root()` yields "." when no home resolves; resolution must still
        // produce a usable path rather than panicking or returning "".
        let resolved = resolve_in(Path::new("."), None, None);
        assert!(resolved.path.ends_with(STATE_DIR_NAME));
        assert!(!resolved.path.as_os_str().is_empty());
    }

    #[test]
    fn a_legacy_file_named_dot_grok_is_not_mistaken_for_the_state_dir() {
        let home = TempDir::new().unwrap();
        std::fs::write(home.path().join(LEGACY_STATE_DIR_NAME), b"not a dir").unwrap();
        let resolved = resolve_in(home.path(), None, None);
        assert_eq!(resolved.source, StateDirSource::Default);
    }

    #[test]
    fn migration_copies_the_tree_and_preserves_file_modes() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_STATE_DIR_NAME);
        std::fs::create_dir_all(legacy.join("sessions").join("proj")).unwrap();
        std::fs::write(legacy.join("auth.json"), b"{\"token\":\"secret\"}").unwrap();
        std::fs::write(legacy.join("sessions").join("proj").join("a.json"), b"{}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                legacy.join("auth.json"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }

        let target = home.path().join(STATE_DIR_NAME);
        let stats = migrate_copy(&legacy, &target).unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.dirs, 3);

        assert_eq!(
            std::fs::read_to_string(target.join("auth.json")).unwrap(),
            "{\"token\":\"secret\"}"
        );
        assert!(target.join("sessions").join("proj").join("a.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(target.join("auth.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "auth.json must stay owner-only");
        }
    }

    #[test]
    fn migration_recreates_symlinks_instead_of_following_them() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("real.txt"), b"hi").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("real.txt", legacy.join("link.txt")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file("real.txt", legacy.join("link.txt")).unwrap();

        let target = home.path().join(STATE_DIR_NAME);
        let stats = migrate_copy(&legacy, &target).unwrap();
        assert_eq!(stats.symlinks, 1);
        assert_eq!(stats.files, 1);
        assert!(
            std::fs::symlink_metadata(target.join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn migration_skips_sockets_left_by_a_live_session() {
        use std::os::unix::net::UnixListener;
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("config.toml"), b"x = 1").unwrap();
        let _listener = UnixListener::bind(legacy.join("leader.sock")).unwrap();

        let target = home.path().join(STATE_DIR_NAME);
        let stats = migrate_copy(&legacy, &target).unwrap();
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.files, 1);
        assert!(!target.join("leader.sock").exists());
    }

    #[test]
    fn migration_does_not_copy_the_keep_legacy_marker() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy).unwrap();
        keep_legacy(&legacy).unwrap();
        let target = home.path().join(STATE_DIR_NAME);
        migrate_copy(&legacy, &target).unwrap();
        assert!(!target.join(KEEP_LEGACY_MARKER).exists());
    }

    #[test]
    fn migration_refuses_an_existing_target() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_STATE_DIR_NAME);
        let target = home.path().join(STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let err = migrate_copy(&legacy, &target).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn after_a_migration_resolution_switches_to_medley() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(LEGACY_STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("config.toml"), b"x = 1").unwrap();
        assert_eq!(
            resolve_in(home.path(), None, None).source,
            StateDirSource::LegacyMigrationPending
        );

        migrate_copy(&legacy, &home.path().join(STATE_DIR_NAME)).unwrap();

        let after = resolve_in(home.path(), None, None);
        assert_eq!(after.path, home.path().join(STATE_DIR_NAME));
        assert_eq!(after.source, StateDirSource::Medley);
    }
}
