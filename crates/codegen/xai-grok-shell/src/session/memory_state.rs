//! `SessionMemory` — memory subsystem state for the session actor.
//!
//! Groups storage, flush config, injection state, and telemetry counters
//! that were previously scattered across 15 fields on `SessionActor`.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

/// Memory subsystem state for a session.
pub(crate) struct SessionMemory {
    /// Canonical configured memory storage handle retained across toggles.
    pub configured_storage: Option<crate::session::memory::MemoryStorage>,
    /// Canonical configured memory storage handle.
    /// Wrapped in `RefCell` to allow `/memory on|off` toggle from `&Arc<SessionActor>`.
    pub storage: RefCell<Option<crate::session::memory::MemoryStorage>>,
    /// Whether to write a session summary to memory on session end.
    pub save_on_end: bool,
    /// Shared params for building a fully-configured memory backend.
    /// `None` when memory is disabled.
    pub backend_params: Option<crate::session::memory::MemoryBackendParams>,
    /// First-turn memory injection behavior resolved from local + remote config.
    pub initial_injection_config: crate::config::MemoryInitialInjectionConfig,
    /// Per-process latch: the first-turn injection decision already ran in
    /// this session segment. Cross-segment idempotency comes from
    /// `conversation_has_memory_context`, not this flag.
    pub context_injected: AtomicBool,
    /// Memory flush configuration (from MemoryConfig).
    pub flush_config: crate::config::MemoryFlushConfig,
    /// When `true`, auto-compact checks are suppressed during memory flush.
    pub is_flushing: AtomicBool,
    /// The compaction count at which the last flush ran (once-per-cycle guard).
    pub last_flush_compaction: AtomicU64,
    /// Number of flushes executed in this session.
    pub flush_count: AtomicU64,
    /// Content from the most recent successful flush, used for delta prompts.
    /// Wrapped in `RefCell` because `SessionActor` is single-threaded (LocalSet).
    pub last_flush_content: RefCell<Option<String>>,
    /// Number of successful flushes.
    pub flush_success_count: AtomicU64,
    /// Number of failed flushes.
    pub flush_error_count: AtomicU64,
    /// Counts model-initiated `memory_search` tool calls.
    /// Wrapped in `RefCell` to allow `/memory on|off` toggle from `&Arc<SessionActor>`.
    pub search_counter: RefCell<Option<Arc<AtomicU64>>>,
    /// Counts first-turn memory context injections.
    pub injection_count: AtomicU64,
    /// Counts post-compaction memory re-injection searches.
    pub compaction_recovery_count: AtomicU64,
    /// Total memory chunks added across all sources.
    pub chunks_added: Arc<AtomicU64>,
    /// autoDream consolidation config.
    pub dream_config: crate::config::MemoryDreamConfig,
    /// Number of dream consolidations attempted.
    pub dream_count: AtomicU64,
    /// Number of successful dream consolidations.
    pub dream_success_count: AtomicU64,
    /// Number of failed dream consolidations.
    pub dream_error_count: AtomicU64,
}

impl SessionMemory {
    /// Whether memory is enabled for this session.
    pub(crate) fn is_enabled(&self) -> bool {
        self.storage.borrow().is_some()
    }

    /// Clone the active storage out of the `RefCell`, dropping the borrow immediately.
    pub(crate) fn storage(&self) -> Option<crate::session::memory::MemoryStorage> {
        if self.is_enabled() {
            self.configured_storage()
        } else {
            None
        }
    }

    /// Clone the canonical configured storage, including while memory is off.
    pub(crate) fn configured_storage(&self) -> Option<crate::session::memory::MemoryStorage> {
        self.configured_storage.clone()
    }

    /// Mark the configured memory backend active with its search counter.
    pub(crate) fn enable(&self, search_counter: Arc<AtomicU64>) {
        *self.storage.borrow_mut() = self.configured_storage();
        *self.search_counter.borrow_mut() = Some(search_counter);
    }

    /// Mark memory inactive without discarding its configured storage.
    pub(crate) fn disable(&self) {
        *self.storage.borrow_mut() = None;
        *self.search_counter.borrow_mut() = None;
    }

    /// Attempt to acquire the flush lock. Returns `true` if acquired,
    /// `false` if another flush is already in progress.
    pub(crate) fn try_acquire_flush_lock(&self) -> bool {
        self.is_flushing
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Release the flush lock.
    pub(crate) fn release_flush_lock(&self) {
        self.is_flushing
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record a flush result and increment the appropriate counter.
    ///
    /// Matches the original three-way logic: "written" increments success,
    /// "error" increments error, anything else ("nothing_to_store", "rejected")
    /// increments only the total flush count.
    pub(crate) fn record_flush_result(&self, outcome: &str) {
        self.flush_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match outcome {
            "written" => {
                self.flush_success_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            "error" => {
                self.flush_error_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// Record a dream consolidation result.
    pub(crate) fn record_dream_result(&self, success: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        self.dream_count.fetch_add(1, Relaxed);
        if success {
            self.dream_success_count.fetch_add(1, Relaxed);
        } else {
            self.dream_error_count.fetch_add(1, Relaxed);
        }
    }

    /// Record a neutral dream outcome (nothing to consolidate / skipped).
    pub(crate) fn record_dream_neutral(&self) {
        self.dream_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Open (or create) the memory index for the current workspace.
    ///
    /// Shared helper that extracts embed dimensions from `backend_params`
    /// and opens the index at `<workspace_dir>/index.sqlite`.
    pub(crate) fn open_index(
        &self,
        storage: &crate::session::memory::MemoryStorage,
    ) -> Option<crate::session::memory::MemoryIndex> {
        let embed_dims = self
            .backend_params
            .as_ref()
            .and_then(|p| p.embed_config.as_ref())
            .map_or(1024, |c| c.dimensions);
        let db_path = storage.workspace_dir().join("index.sqlite");
        crate::session::memory::MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            Default::default(),
            embed_dims,
        )
        .ok()
    }

    /// Reindex a file and embed new chunks when embedding is configured.
    pub(crate) async fn reindex_and_embed(&self, path: &std::path::Path, source: &str) {
        let Some(storage) = self.storage() else {
            return;
        };
        if let Some(mut index) = self.open_index(&storage) {
            let _ = index.reindex_file(path, source);
            if let Some(ref params) = self.backend_params
                && let Some(provider) = params.make_embedding_provider().await
            {
                crate::session::memory::embed_missing_chunks(&index, &provider).await;
            }
        }
    }

    /// Remove chunks for the given file paths from the search index.
    ///
    /// Used after dream consolidation deletes processed session files so
    /// that stale chunks don't linger in the index. Best-effort: errors
    /// are logged but don't propagate.
    pub(crate) fn delete_paths_from_index(&self, paths: &[std::path::PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let Some(storage) = self.storage() else {
            return;
        };
        if let Some(mut index) = self.open_index(&storage) {
            let mut total_removed = 0usize;
            for path in paths {
                match index.delete_path(path) {
                    Ok(n) => total_removed += n,
                    Err(e) => {
                        tracing::warn!(
                            target: xai_grok_telemetry::memory_log::TARGET,
                            path = %path.display(),
                            error = %e,
                            "DREAM_CLEANUP: failed to remove chunks from index"
                        );
                    }
                }
            }
            if total_removed > 0 {
                tracing::info!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    chunks_removed = total_removed,
                    files = paths.len(),
                    "DREAM_CLEANUP: removed stale chunks from index"
                );
            }
        }
    }

    /// Collect telemetry counters for session-end summary.
    pub(crate) fn telemetry_snapshot(&self) -> MemoryTelemetry {
        use std::sync::atomic::Ordering::Relaxed;
        MemoryTelemetry {
            flush_count: self.flush_count.load(Relaxed),
            flush_success_count: self.flush_success_count.load(Relaxed),
            flush_error_count: self.flush_error_count.load(Relaxed),
            tool_search_count: self
                .search_counter
                .borrow()
                .as_ref()
                .map_or(0, |c| c.load(Relaxed)),
            injection_count: self.injection_count.load(Relaxed),
            compaction_recovery_count: self.compaction_recovery_count.load(Relaxed),
            chunks_added: self.chunks_added.load(Relaxed),
            dream_count: self.dream_count.load(Relaxed),
            dream_success_count: self.dream_success_count.load(Relaxed),
            dream_error_count: self.dream_error_count.load(Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_retains_canonical_configured_storage() {
        let temp = tempfile::TempDir::new().unwrap();
        let global_dir = temp.path().join("flat-memory-root");
        let workspace_dir = global_dir.join("workspace");
        let storage = crate::session::memory::MemoryStorage::with_paths(
            global_dir.clone(),
            workspace_dir.clone(),
        );
        let memory = SessionMemory {
            configured_storage: Some(storage.clone()),
            storage: RefCell::new(Some(storage)),
            save_on_end: true,
            backend_params: None,
            initial_injection_config: Default::default(),
            context_injected: AtomicBool::new(false),
            flush_config: Default::default(),
            is_flushing: AtomicBool::new(false),
            last_flush_compaction: AtomicU64::new(0),
            flush_count: AtomicU64::new(0),
            last_flush_content: RefCell::new(None),
            flush_success_count: AtomicU64::new(0),
            flush_error_count: AtomicU64::new(0),
            search_counter: RefCell::new(Some(Arc::new(AtomicU64::new(0)))),
            injection_count: AtomicU64::new(0),
            compaction_recovery_count: AtomicU64::new(0),
            chunks_added: Arc::new(AtomicU64::new(0)),
            dream_config: Default::default(),
            dream_count: AtomicU64::new(0),
            dream_success_count: AtomicU64::new(0),
            dream_error_count: AtomicU64::new(0),
        };

        memory.disable();
        assert!(!memory.is_enabled());
        assert!(memory.storage().is_none());
        let configured = memory
            .configured_storage()
            .expect("toggle-off must retain configured storage");
        assert_eq!(configured.global_dir(), global_dir);
        assert_eq!(configured.workspace_dir(), workspace_dir);

        memory.enable(Arc::new(AtomicU64::new(0)));
        let active = memory
            .storage()
            .expect("toggle-on must reactivate configured storage");
        assert_eq!(active.global_dir(), global_dir);
        assert_eq!(active.workspace_dir(), workspace_dir);
    }
}

/// Snapshot of memory telemetry counters for session-end logging.
pub(crate) struct MemoryTelemetry {
    pub flush_count: u64,
    pub flush_success_count: u64,
    pub flush_error_count: u64,
    pub tool_search_count: u64,
    pub injection_count: u64,
    pub compaction_recovery_count: u64,
    pub chunks_added: u64,
    pub dream_count: u64,
    pub dream_success_count: u64,
    pub dream_error_count: u64,
}
