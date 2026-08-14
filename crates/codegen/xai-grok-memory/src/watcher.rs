//! File watcher for detecting external memory edits.
//!
//! Watches `~/.grok/memory/` for `.md` file changes (create, modify, remove)
//! and accumulates the affected paths.  The search path checks [`is_dirty`]
//! before each query and syncs the index for all dirty paths:
//! - **created / modified** files are reindexed via `MemoryIndex::reindex_file`
//! - **deleted** files have their stale chunks removed via `MemoryIndex::delete_path`
//!
//! Without the deletion handling, chunks from removed files would remain
//! searchable indefinitely.
//!
//! Uses `arc_swap::ArcSwap` for lock-free dirty path tracking — the notify
//! event handler inserts via `rcu`, the search path takes via atomic swap.
//!
//! [`is_dirty`]: MemoryFileWatcher::is_dirty

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwap;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Watches the memory directory for `.md` file changes.
///
/// Lock-free design:
/// - **Insert** (notify thread): `dirty_files.rcu(|old| { clone + insert })`
/// - **Take** (search path): `dirty_files.swap(empty)` — single atomic pointer exchange
/// - **Quick check**: `dirty.load(Relaxed)` — single atomic load, no allocation
pub struct MemoryFileWatcher {
    dirty_files: Arc<ArcSwap<HashSet<PathBuf>>>,
    dirty: Arc<AtomicBool>,
    watcher: std::sync::Mutex<Option<RecommendedWatcher>>,
}

impl MemoryFileWatcher {
    /// Create an in-memory watcher handle without touching the filesystem or
    /// starting an OS watcher. Fresh session actors use this during provisional
    /// construction and activate it only after publication.
    pub fn deferred() -> Self {
        Self {
            dirty_files: Arc::new(ArcSwap::new(Arc::new(HashSet::new()))),
            dirty: Arc::new(AtomicBool::new(false)),
            watcher: std::sync::Mutex::new(None),
        }
    }

    /// Start the OS watcher for an existing memory directory. Idempotent after
    /// success; a failed attempt leaves the handle dormant so callers may retry.
    /// This method never creates the watched directory.
    pub fn activate(&self, memory_dir: &Path) -> bool {
        let mut slot = self
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_some() {
            return true;
        }

        let df = self.dirty_files.clone();
        let d = self.dirty.clone();
        let mut watcher =
            match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                let Ok(event) = res else { return };
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {}
                    _ => return,
                }
                for path in &event.paths {
                    if path.extension().is_some_and(|ext| ext == "md") {
                        let path = path.clone();
                        df.rcu(move |old| {
                            let mut new = (**old).clone();
                            new.insert(path.clone());
                            new
                        });
                        d.store(true, Ordering::Relaxed);
                    }
                }
            }) {
                Ok(watcher) => watcher,
                Err(error) => {
                    tracing::warn!(%error, "failed to create memory file watcher");
                    return false;
                }
            };

        if let Err(error) = watcher.watch(memory_dir, RecursiveMode::Recursive) {
            tracing::warn!(
                path = %memory_dir.display(),
                %error,
                "failed to watch memory directory"
            );
            return false;
        }

        tracing::info!(
            path = %memory_dir.display(),
            "memory file watcher started"
        );
        *slot = Some(watcher);
        true
    }

    pub fn is_started(&self) -> bool {
        self.watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Start watching the given memory directory for `.md` file changes.
    ///
    /// Returns `None` if the watcher fails to initialize (logged, non-fatal).
    pub fn start(memory_dir: &Path) -> Option<Self> {
        let watcher = Self::deferred();
        watcher.activate(memory_dir).then_some(watcher)
    }

    /// Quick check: true if any files have been modified since last take.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Take all accumulated dirty paths, resetting the dirty state.
    /// Returns the paths that changed since the last take.
    pub fn take_dirty(&self) -> Vec<PathBuf> {
        let old = self.dirty_files.swap(Arc::new(HashSet::new()));
        self.dirty.store(false, Ordering::Relaxed);
        old.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_watcher_starts_on_valid_dir() {
        let tmp = TempDir::new().unwrap();
        let watcher = MemoryFileWatcher::start(tmp.path())
            .expect("a supported release environment must create an OS file watcher");
        assert!(watcher.is_started());
    }

    #[test]
    fn deferred_watcher_has_no_filesystem_side_effects() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("not-created-by-watcher");
        let watcher = MemoryFileWatcher::deferred();

        assert!(!watcher.is_started());
        assert!(!missing.exists());
        assert!(!watcher.activate(&missing));
        assert!(!watcher.is_started());
        assert!(
            !missing.exists(),
            "watcher activation must never create the watched directory"
        );
    }

    #[test]
    fn deferred_watcher_can_activate_after_directory_creation() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        let watcher = MemoryFileWatcher::deferred();
        assert!(!watcher.is_started());

        std::fs::create_dir(&memory_dir).unwrap();
        assert!(
            watcher.activate(&memory_dir),
            "a supported release environment must activate the deferred watcher"
        );
        assert!(watcher.is_started());
        assert!(
            watcher.activate(&memory_dir),
            "activation must be idempotent"
        );
    }

    #[test]
    fn test_watcher_initially_clean() {
        let tmp = TempDir::new().unwrap();
        let Some(watcher) = MemoryFileWatcher::start(tmp.path()) else {
            eprintln!("skipping: could not create file watcher (resource limit?)");
            return;
        };
        assert!(!watcher.is_dirty());
        assert!(watcher.take_dirty().is_empty());
    }

    #[test]
    fn test_watcher_detects_md_file_creation() {
        let tmp = TempDir::new().unwrap();
        let Some(watcher) = MemoryFileWatcher::start(tmp.path()) else {
            eprintln!("skipping: could not create file watcher (resource limit?)");
            return;
        };

        // Create a .md file — watcher should detect it
        std::fs::write(tmp.path().join("test.md"), "hello").unwrap();

        // Give the watcher time to process (debounce + OS event delivery)
        std::thread::sleep(std::time::Duration::from_millis(500));

        assert!(watcher.is_dirty(), "should detect .md creation");
        let dirty = watcher.take_dirty();
        assert!(!dirty.is_empty(), "should have dirty paths");
        assert!(dirty[0].extension().unwrap() == "md");
    }

    #[test]
    fn test_watcher_ignores_non_md_files() {
        let tmp = TempDir::new().unwrap();
        let Some(watcher) = MemoryFileWatcher::start(tmp.path()) else {
            eprintln!("skipping: could not create file watcher (resource limit?)");
            return;
        };

        // Create a non-.md file
        std::fs::write(tmp.path().join("test.txt"), "hello").unwrap();
        std::fs::write(tmp.path().join("index.sqlite"), "db").unwrap();

        std::thread::sleep(std::time::Duration::from_millis(500));

        assert!(
            !watcher.is_dirty(),
            "should not detect non-.md file changes"
        );
    }

    #[test]
    fn test_take_dirty_resets_state() {
        let tmp = TempDir::new().unwrap();
        let Some(watcher) = MemoryFileWatcher::start(tmp.path()) else {
            eprintln!("skipping: could not create file watcher (resource limit?)");
            return;
        };

        std::fs::write(tmp.path().join("a.md"), "content").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        let first = watcher.take_dirty();
        assert!(!first.is_empty());
        assert!(!watcher.is_dirty(), "should be clean after take");
        assert!(
            watcher.take_dirty().is_empty(),
            "second take should be empty"
        );
    }
}
