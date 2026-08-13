//! Session fork/copy for the JSONL adapter.
//!
//! The `updates.jsonl` transcript is unbounded, so the copy streams it line by
//! line: peak memory tracks a single capped line, plus one small per-line
//! record when a prompt cut is requested. Chat history stays materialized: its
//! transforms need random access and the compacted history is bounded by the
//! context window.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use crate::sampling::{
    ContentPart, ConversationItem, SyntheticReason, conversation_truncate_for_prompt,
    fork_filter_chat, transform_conversation_cwd,
};
use crate::session::info::Info;
use crate::session::persistence::{
    CHAT_FORMAT_VERSION, SessionIdLock, Summary, acquire_ordered_copy_locks_sync,
};
use crate::session::storage::jsonl::{JsonlStorageAdapter, transform_session_id_in_update};
use crate::session::storage::{
    CopySessionOptions, CopySessionResult, RewindStep, SessionUpdate, SessionUpdateEnvelope,
    filter_rewind_by, rewind_step_for_line, truncate_for_prompt_by,
};
use agent_client_protocol as acp;
use xai_grok_shell_base::util::anchored_directory::AnchoredDirectory;

use super::SessionDirMode;

#[cfg(test)]
#[path = "copy_tests.rs"]
mod tests;

fn is_orchestration_projection_update(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::Xai(notification)
            if matches!(
                &notification.update,
                crate::extensions::notification::SessionUpdate::WorkflowUpdated { .. }
                    | crate::extensions::notification::SessionUpdate::GoalUpdated { .. }
            )
    )
}

/// Rebind only the canonical pointer block emitted by `CompactionMode`.
///
/// The referenced path or even a quoted hint can legitimately appear in the
/// summary body. `build_compacted_history` appends the generated hint as the
/// final suffix, so only that suffix is eligible for rebinding.
fn rebind_compaction_hint(
    items: &mut [ConversationItem],
    mode: xai_chat_state::CompactionMode,
    source_path: &Path,
    target_path: &Path,
) {
    let source_path = source_path.to_string_lossy();
    let target_path = target_path.to_string_lossy();
    let (Some(source_hint), Some(target_hint)) = (
        mode.transcript_hint(Some(source_path.as_ref())),
        mode.transcript_hint(Some(target_path.as_ref())),
    ) else {
        return;
    };
    for item in items {
        let ConversationItem::User(user) = item else {
            continue;
        };
        if user.synthetic_reason != Some(SyntheticReason::CompactionMeta) {
            continue;
        }
        for part in &mut user.content {
            let ContentPart::Text { text } = part else {
                continue;
            };
            if let Some(prefix) = text.strip_suffix(&source_hint) {
                let mut rebound = String::with_capacity(prefix.len() + target_hint.len());
                rebound.push_str(prefix);
                rebound.push_str(&target_hint);
                *text = rebound.into();
            }
        }
    }
}

/// Updates written plus the `compaction_checkpoints/{uuid}.json` files the
/// surviving records reference, collected in the same pass.
#[derive(Default)]
struct CopiedUpdates {
    count: usize,
    checkpoint_files: BTreeSet<String>,
}

/// Longest `updates.jsonl` line the copy will buffer; anything past it is
/// corruption (e.g. a tail that lost its newlines) and is discarded without
/// being buffered. Discarded lines consume no index in either pass, unlike
/// torn lines, which classify as [`RewindStep::Other`] and end a user run.
const MAX_UPDATE_LINE_BYTES: usize = 64 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    static FAIL_COPY_STAGE_AFTER_CONTAINER_CREATE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

/// Locks and private publication state held for the complete copy.
///
/// The shared source lease prevents deletion/relocation while files are read.
/// The exclusive target lease is retained while all target files are written
/// under `.private/session-staging`; the finished directory is then published
/// with an anchored no-replace rename. No incomplete fork is ever created in
/// the public sessions namespace.
struct CopyPublication {
    _source_lock: SessionIdLock,
    _target_lock: SessionIdLock,
    root_dir: PathBuf,
    root_anchor: AnchoredDirectory,
    staging_anchor: AnchoredDirectory,
    sessions_anchor: AnchoredDirectory,
    stage_container_anchor: Option<AnchoredDirectory>,
    stage_dir_anchor: Option<AnchoredDirectory>,
    target_parent_name: OsString,
    target_cwd: String,
    stage_name: String,
    target_name: String,
    target_dir: PathBuf,
    public_target_dir: PathBuf,
    cleanup_armed: bool,
}

#[derive(Debug)]
enum CopyPublicationFinalizeError {
    NotCommitted(io::Error),
    CommittedUnreachable(io::Error),
    CommittedDurability(io::Error),
}

impl std::fmt::Display for CopyPublicationFinalizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(error) => {
                write!(formatter, "fork publication not committed: {error}")
            }
            Self::CommittedUnreachable(error) => {
                write!(
                    formatter,
                    "committed fork is not canonically reachable: {error}"
                )
            }
            Self::CommittedDurability(error) => {
                write!(
                    formatter,
                    "fork committed with durability acknowledgement failure: {error}"
                )
            }
        }
    }
}

impl std::error::Error for CopyPublicationFinalizeError {}

impl CopyPublication {
    fn begin(
        root_dir: &Path,
        source_info: &Info,
        target_info: &Info,
        target_dir: PathBuf,
    ) -> io::Result<Self> {
        let source_id = source_info.id.to_string();
        let target_id = target_info.id.to_string();
        validate_session_path_component(&source_id)?;
        validate_session_path_component(&target_id)?;
        if source_id == target_id {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "fork target session id must differ from source session id",
            ));
        }

        let (source_lock, target_lock) =
            acquire_ordered_copy_locks_sync(root_dir, &source_id, &target_id)?;
        crate::session::persistence::reclaim_abandoned_session_stages_in_root(
            root_dir, &target_id,
        )?;

        let root_anchor = AnchoredDirectory::open_root(root_dir)?;
        let sessions_anchor = open_or_create_child_dir(&root_anchor, OsStr::new("sessions"))?;
        let sessions_root = root_dir.join("sessions");
        let target_parent = target_dir
            .parent()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fork target has no parent directory",
                )
            })?
            .to_path_buf();
        if target_parent.parent() != Some(sessions_root.as_path()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fork target parent is not a direct child of the sessions root",
            ));
        }
        if persisted_session_id_present(&sessions_root, &target_id)? || target_dir.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("a session with id {target_id} already exists"),
            ));
        }

        let target_parent_name = target_parent
            .file_name()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fork target parent has no directory name",
                )
            })?
            .to_owned();
        let expected_parent_name =
            OsString::from(crate::util::grok_home::encode_cwd_dirname(&target_info.cwd));
        if target_parent_name != expected_parent_name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fork target parent does not match the target cwd",
            ));
        }
        let private_anchor = open_or_create_child_dir(&root_anchor, OsStr::new(".private"))?;
        let staging_anchor =
            open_or_create_child_dir(&private_anchor, OsStr::new("session-staging"))?;
        private_anchor.ensure_owner_only()?;
        staging_anchor.ensure_owner_only()?;
        let stage_container_name =
            crate::session::persistence::session_stage_container_name(&target_id);
        let stage_name = stage_container_name.to_string_lossy().into_owned();
        let stage_container_anchor = staging_anchor.create_child_dir(&stage_container_name)?;
        if let Err(error) = stage_container_anchor.ensure_owner_only() {
            let _ = stage_container_anchor.remove_self();
            return Err(error);
        }
        #[cfg(test)]
        if FAIL_COPY_STAGE_AFTER_CONTAINER_CREATE.with(|fail| fail.replace(false)) {
            let error = io::Error::other("injected copy stage construction failure");
            let _ = stage_container_anchor.remove_self();
            return Err(error);
        }
        let stage_dir_anchor = match stage_container_anchor.create_child_dir(OsStr::new(&target_id))
        {
            Ok(directory) => {
                if let Err(error) = directory.ensure_owner_only() {
                    let _ = directory.remove_self();
                    let _ = stage_container_anchor.remove_self();
                    return Err(error);
                }
                directory
            }
            Err(error) => {
                let _ = stage_container_anchor.remove_self();
                return Err(error);
            }
        };
        let stage_dir = root_dir
            .join(".private/session-staging")
            .join(&stage_name)
            .join(&target_id);
        let prepare_result = (|| {
            let marker = stage_dir_anchor.create_child_file_new(OsStr::new(
                crate::session::persistence::UNPUBLISHED_SESSION_MARKER,
            ))?;
            marker.sync_all()?;
            stage_dir_anchor.sync()?;
            stage_container_anchor.sync()
        })();
        if let Err(error) = prepare_result {
            let _ = stage_dir_anchor.remove_marker(OsStr::new(
                crate::session::persistence::UNPUBLISHED_SESSION_MARKER,
            ));
            if stage_dir_anchor.remove_self().is_ok() {
                let _ = stage_container_anchor.remove_self();
            }
            return Err(error);
        }
        Ok(Self {
            _source_lock: source_lock,
            _target_lock: target_lock,
            root_dir: root_dir.to_path_buf(),
            root_anchor,
            staging_anchor,
            sessions_anchor,
            stage_container_anchor: Some(stage_container_anchor),
            stage_dir_anchor: Some(stage_dir_anchor),
            target_parent_name,
            target_cwd: target_info.cwd.clone(),
            stage_name,
            target_name: target_id,
            target_dir: stage_dir,
            public_target_dir: target_dir,
            cleanup_armed: true,
        })
    }

    fn target_dir(&self) -> &Path {
        &self.target_dir
    }

    fn publish(mut self) -> Result<(), CopyPublicationFinalizeError> {
        self.publish_with(|| Ok(()), || {})
    }

    fn publish_with(
        &mut self,
        sync_after_commit: impl FnOnce() -> io::Result<()>,
        after_rename: impl FnOnce(),
    ) -> Result<(), CopyPublicationFinalizeError> {
        if public_session_occupied(&self.public_target_dir) {
            return Err(CopyPublicationFinalizeError::NotCommitted(
                publication_collision_error(),
            ));
        }
        let canonical_root = AnchoredDirectory::open_root(&self.root_dir).map_err(|error| {
            CopyPublicationFinalizeError::NotCommitted(io_step("open canonical root", error))
        })?;
        if !self
            .root_anchor
            .same_identity(&canonical_root)
            .map_err(CopyPublicationFinalizeError::NotCommitted)?
        {
            return Err(CopyPublicationFinalizeError::NotCommitted(io::Error::new(
                io::ErrorKind::InvalidData,
                "fork root identity changed before publication",
            )));
        }
        let canonical_sessions = canonical_root
            .open_child_dir(OsStr::new("sessions"))
            .map_err(|error| {
                CopyPublicationFinalizeError::NotCommitted(io_step(
                    "open canonical sessions",
                    error,
                ))
            })?;
        if !self
            .sessions_anchor
            .same_identity(&canonical_sessions)
            .map_err(CopyPublicationFinalizeError::NotCommitted)?
        {
            return Err(CopyPublicationFinalizeError::NotCommitted(io::Error::new(
                io::ErrorKind::InvalidData,
                "sessions root identity changed before fork publication",
            )));
        }
        sync_tree(&self.target_dir).map_err(|error| {
            CopyPublicationFinalizeError::NotCommitted(io_step("sync staged tree", error))
        })?;
        let marker = OsStr::new(crate::session::persistence::UNPUBLISHED_SESSION_MARKER);

        let dest_parent_exists = matches!(
            self.root_dir
                .join("sessions")
                .join(&self.target_parent_name)
                .try_exists(),
            Ok(true)
        );
        let (published_parent_anchor, published_anchor, whole_parent_published) =
            match canonical_sessions.open_child_dir(&self.target_parent_name) {
                Ok(parent) => {
                    crate::session::persistence::validate_existing_cwd_metadata(
                        &parent,
                        &self.target_parent_name,
                        &self.target_cwd,
                    )
                    .map_err(|error| {
                        CopyPublicationFinalizeError::NotCommitted(io_step(
                            "validate existing dest parent cwd metadata",
                            error,
                        ))
                    })?;
                    let stage_dir_anchor = self.stage_dir_anchor.as_ref().ok_or_else(|| {
                        CopyPublicationFinalizeError::NotCommitted(io::Error::other(
                            "fork stage anchor is unavailable",
                        ))
                    })?;
                    stage_dir_anchor.remove_marker(marker).map_err(|error| {
                        CopyPublicationFinalizeError::NotCommitted(io_step(
                            "remove unpublished marker on existing dest parent",
                            error,
                        ))
                    })?;
                    stage_dir_anchor.sync().map_err(|error| {
                        CopyPublicationFinalizeError::NotCommitted(io_step(
                            "sync stage before rename into existing dest parent",
                            error,
                        ))
                    })?;
                    let stage_dir_anchor = self.stage_dir_anchor.take().expect("checked above");
                    let published = match stage_dir_anchor
                        .try_rename_self_no_replace(&parent, OsStr::new(&self.target_name))
                    {
                        Ok(published) => published,
                        Err(failure) => {
                            self.stage_dir_anchor = Some(failure.source);
                            let error = if failure.error.kind() == io::ErrorKind::AlreadyExists
                                || public_session_occupied(&self.public_target_dir)
                            {
                                publication_collision_error()
                            } else {
                                io_step("rename session into existing dest parent", failure.error)
                            };
                            return Err(CopyPublicationFinalizeError::NotCommitted(error));
                        }
                    };
                    (parent, published, false)
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound || !dest_parent_exists => {
                    let container = self.stage_container_anchor.as_ref().ok_or_else(|| {
                        CopyPublicationFinalizeError::NotCommitted(io::Error::other(
                            "fork stage container anchor is unavailable",
                        ))
                    })?;
                    crate::session::persistence::write_staged_cwd_metadata_if_needed(
                        container,
                        &self.target_parent_name,
                        &self.target_cwd,
                    )
                    .map_err(|error| {
                        CopyPublicationFinalizeError::NotCommitted(io_step(
                            "write staged cwd metadata",
                            error,
                        ))
                    })?;
                    let stage_dir_anchor = self.stage_dir_anchor.as_ref().ok_or_else(|| {
                        CopyPublicationFinalizeError::NotCommitted(io::Error::other(
                            "fork stage anchor is unavailable",
                        ))
                    })?;
                    stage_dir_anchor.remove_marker(marker).map_err(|error| {
                        CopyPublicationFinalizeError::NotCommitted(io_step(
                            "remove unpublished marker before container rename",
                            error,
                        ))
                    })?;
                    stage_dir_anchor.sync().map_err(|error| {
                        CopyPublicationFinalizeError::NotCommitted(io_step(
                            "sync stage before container rename",
                            error,
                        ))
                    })?;
                    // Windows cannot rename an ancestor with its child handle open.
                    let stage_dir_anchor = self.stage_dir_anchor.take().expect("checked above");
                    drop(stage_dir_anchor);
                    let container = self.stage_container_anchor.take().expect("checked above");
                    match container
                        .try_rename_self_no_replace(&canonical_sessions, &self.target_parent_name)
                    {
                        Ok(published_parent) => {
                            let published = published_parent
                                .open_child_dir(OsStr::new(&self.target_name))
                                .map_err(CopyPublicationFinalizeError::CommittedUnreachable)?;
                            (published_parent, published, true)
                        }
                        Err(failure) => {
                            if public_session_occupied(&self.public_target_dir) {
                                self.stage_container_anchor = Some(failure.source);
                                return Err(CopyPublicationFinalizeError::NotCommitted(
                                    publication_collision_error(),
                                ));
                            }
                            // GHA probes of a missing sessions child can return
                            // ACCESS_DENIED; that is not a real collision.
                            let parent_taken = matches!(
                                self.root_dir
                                    .join("sessions")
                                    .join(&self.target_parent_name)
                                    .try_exists(),
                                Ok(true)
                            );
                            let container = failure.source;
                            self.stage_container_anchor = Some(container);
                            let child = self
                                .stage_container_anchor
                                .as_ref()
                                .expect("restored above")
                                .open_child_dir(OsStr::new(&self.target_name))
                                .map_err(|error| {
                                    CopyPublicationFinalizeError::NotCommitted(io_step(
                                        "reopen stage child after container rename",
                                        error,
                                    ))
                                })?;
                            self.stage_dir_anchor = Some(child);
                            if !parent_taken {
                                let published_parent = match canonical_sessions
                                    .create_child_dir(&self.target_parent_name)
                                {
                                    Ok(parent) => parent,
                                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                                        canonical_sessions
                                            .open_child_dir(&self.target_parent_name)
                                            .map_err(|open_error| {
                                                CopyPublicationFinalizeError::NotCommitted(io_step(
                                                    "open dest parent after create collision",
                                                    open_error,
                                                ))
                                            })?
                                    }
                                    Err(create_error) => {
                                        let Some(dest_parent) = self.public_target_dir.parent()
                                        else {
                                            return Err(
                                                CopyPublicationFinalizeError::NotCommitted(
                                                    io::Error::new(
                                                        io::ErrorKind::InvalidInput,
                                                        "fork target has no parent directory",
                                                    ),
                                                ),
                                            );
                                        };
                                        std::fs::create_dir_all(dest_parent).map_err(
                                            |path_error| {
                                                CopyPublicationFinalizeError::NotCommitted(
                                                    io_step(
                                                        "create_dir_all dest parent after handle create failed",
                                                        path_error,
                                                    ),
                                                )
                                            },
                                        )?;
                                        canonical_sessions
                                            .open_child_dir(&self.target_parent_name)
                                            .map_err(|open_error| {
                                                CopyPublicationFinalizeError::NotCommitted(io_step(
                                                    "open dest parent after create_dir_all",
                                                    open_error,
                                                ))
                                            })?
                                    }
                                };
                                crate::session::persistence::write_staged_cwd_metadata_if_needed(
                                    &published_parent,
                                    &self.target_parent_name,
                                    &self.target_cwd,
                                )
                                .map_err(|error| {
                                    CopyPublicationFinalizeError::NotCommitted(io_step(
                                        "write dest parent cwd metadata",
                                        error,
                                    ))
                                })?;
                                if public_session_occupied(&self.public_target_dir) {
                                    return Err(CopyPublicationFinalizeError::NotCommitted(
                                        publication_collision_error(),
                                    ));
                                }
                                let child = self.stage_dir_anchor.take().expect("restored above");
                                match child.try_rename_self_no_replace(
                                    &published_parent,
                                    OsStr::new(&self.target_name),
                                ) {
                                    Ok(published) => (published_parent, published, false),
                                    Err(failure) => {
                                        self.stage_dir_anchor = Some(failure.source);
                                        let src = self
                                            .root_dir
                                            .join(".private")
                                            .join("session-staging")
                                            .join(&self.stage_name)
                                            .join(&self.target_name);
                                        let _ = self.stage_dir_anchor.take();
                                        publish_stage_dir_no_replace(&src, &self.public_target_dir)
                                            .map_err(|error| {
                                                CopyPublicationFinalizeError::NotCommitted(io_step(
                                                    "path publish session child",
                                                    error,
                                                ))
                                            })?;
                                        let published = published_parent
                                            .open_child_dir(OsStr::new(&self.target_name))
                                            .map_err(|error| {
                                                CopyPublicationFinalizeError::CommittedUnreachable(
                                                    io_step(
                                                        "reopen published child after path publish",
                                                        error,
                                                    ),
                                                )
                                            })?;
                                        (published_parent, published, false)
                                    }
                                }
                            } else {
                                let parent = match canonical_sessions
                                    .open_child_dir(&self.target_parent_name)
                                {
                                    Ok(parent) => parent,
                                    Err(error) => {
                                        if public_session_occupied(&self.public_target_dir) {
                                            return Err(
                                                CopyPublicationFinalizeError::NotCommitted(
                                                    publication_collision_error(),
                                                ),
                                            );
                                        }
                                        return Err(CopyPublicationFinalizeError::NotCommitted(
                                            error,
                                        ));
                                    }
                                };
                                crate::session::persistence::validate_existing_cwd_metadata(
                                    &parent,
                                    &self.target_parent_name,
                                    &self.target_cwd,
                                )
                                .map_err(CopyPublicationFinalizeError::NotCommitted)?;
                                let child = self.stage_dir_anchor.take().expect("restored above");
                                let published = match child.try_rename_self_no_replace(
                                    &parent,
                                    OsStr::new(&self.target_name),
                                ) {
                                    Ok(published) => published,
                                    Err(failure) => {
                                        self.stage_dir_anchor = Some(failure.source);
                                        let error = if failure.error.kind()
                                            == io::ErrorKind::AlreadyExists
                                            || public_session_occupied(&self.public_target_dir)
                                        {
                                            publication_collision_error()
                                        } else {
                                            failure.error
                                        };
                                        return Err(CopyPublicationFinalizeError::NotCommitted(
                                            error,
                                        ));
                                    }
                                };
                                (parent, published, false)
                            }
                        }
                    }
                }
                Err(error) => {
                    if public_session_occupied(&self.public_target_dir) {
                        return Err(CopyPublicationFinalizeError::NotCommitted(
                            publication_collision_error(),
                        ));
                    }
                    return Err(CopyPublicationFinalizeError::NotCommitted(io_step(
                        &format!(
                            "open dest parent exists={dest_parent_exists} name={:?}",
                            self.target_parent_name
                        ),
                        error,
                    )));
                }
            };
        after_rename();
        // The no-replace rename publishes the fork. Never let a later sync
        // failure re-arm Drop cleanup for a directory readers can observe.
        self.cleanup_armed = false;
        self.verify_canonical_publication(&published_parent_anchor, &published_anchor)
            .map_err(CopyPublicationFinalizeError::CommittedUnreachable)?;
        if !whole_parent_published && let Some(container) = self.stage_container_anchor.take() {
            container
                .remove_tree_self()
                .map_err(CopyPublicationFinalizeError::CommittedDurability)?;
        }
        self.staging_anchor
            .sync()
            .map_err(CopyPublicationFinalizeError::CommittedDurability)?;
        published_parent_anchor
            .sync()
            .map_err(CopyPublicationFinalizeError::CommittedDurability)?;
        self.sessions_anchor
            .sync()
            .map_err(CopyPublicationFinalizeError::CommittedDurability)?;
        self.verify_canonical_publication(&published_parent_anchor, &published_anchor)
            .map_err(CopyPublicationFinalizeError::CommittedUnreachable)?;
        sync_after_commit().map_err(CopyPublicationFinalizeError::CommittedDurability)
    }

    fn verify_canonical_publication(
        &self,
        published_parent_anchor: &AnchoredDirectory,
        published_anchor: &AnchoredDirectory,
    ) -> io::Result<()> {
        let canonical_root = AnchoredDirectory::open_root(&self.root_dir).map_err(|error| {
            io::Error::new(error.kind(), format!("open canonical root: {error}"))
        })?;
        if !self.root_anchor.same_identity(&canonical_root)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fork root identity changed after publication",
            ));
        }
        let canonical_sessions = canonical_root
            .open_child_dir(OsStr::new("sessions"))
            .map_err(|error| {
                io::Error::new(error.kind(), format!("open canonical sessions: {error}"))
            })?;
        if !self.sessions_anchor.same_identity(&canonical_sessions)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sessions root identity changed after fork publication",
            ));
        }
        let current_parent = canonical_sessions
            .open_child_dir(&self.target_parent_name)
            .map_err(|error| {
                io::Error::new(error.kind(), format!("open canonical parent: {error}"))
            })?;
        if !published_parent_anchor.same_identity(&current_parent)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fork target parent identity changed after publication",
            ));
        }
        let reopened = current_parent
            .open_child_dir(OsStr::new(&self.target_name))
            .map_err(|error| {
                io::Error::new(error.kind(), format!("open canonical fork: {error}"))
            })?;
        if !published_anchor.same_identity(&reopened)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "published fork identity does not match the committed stage",
            ));
        }
        Ok(())
    }
}

impl Drop for CopyPublication {
    fn drop(&mut self) {
        if !self.cleanup_armed {
            return;
        }
        // The container can also hold staged long-CWD metadata. Reclaim the
        // entire retained tree rather than assuming it becomes empty after the
        // session child is removed.
        drop(self.stage_dir_anchor.take());
        if let Some(container) = self.stage_container_anchor.take()
            && let Err(error) = container.remove_tree_self()
        {
            tracing::warn!(
                stage = %self.stage_name,
                %error,
                "failed to reclaim private fork stage through retained handles"
            );
        }
    }
}

fn reconcile_copy_publication(
    target_id: &acp::SessionId,
    result: CopySessionResult,
    publication: Result<(), CopyPublicationFinalizeError>,
) -> io::Result<CopySessionResult> {
    match publication {
        Ok(()) => Ok(result),
        Err(CopyPublicationFinalizeError::NotCommitted(error)) => Err(error),
        Err(CopyPublicationFinalizeError::CommittedUnreachable(error)) => Err(error),
        Err(CopyPublicationFinalizeError::CommittedDurability(error)) => {
            tracing::warn!(
                session_id = %target_id,
                %error,
                "fork publication committed but durability acknowledgement failed"
            );
            Ok(result)
        }
    }
}

fn io_step(step: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{step}: {error}"))
}

fn publication_collision_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no-replace publication collided",
    )
}

fn public_session_occupied(path: &Path) -> bool {
    path.try_exists().unwrap_or(true)
}

fn publish_stage_dir_no_replace(source: &Path, dest: &Path) -> io::Result<()> {
    match crate::session::storage::relocation::publish_directory_no_replace(source, dest) {
        Ok(()) => Ok(()),
        Err(crate::session::storage::relocation::RelocationError::Collision(_)) => {
            Err(publication_collision_error())
        }
        Err(crate::session::storage::relocation::RelocationError::Io { source, .. }) => Err(source),
        Err(error) => Err(io::Error::other(error)),
    }
}

fn open_or_create_child_dir(
    parent: &AnchoredDirectory,
    name: &OsStr,
) -> io::Result<AnchoredDirectory> {
    match parent.create_child_dir(name) {
        Ok(directory) => Ok(directory),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => parent.open_child_dir(name),
        Err(error) => Err(error),
    }
}

fn validate_session_path_component(session_id: &str) -> io::Result<()> {
    let path = Path::new(session_id);
    let mut components = path.components();
    let valid = matches!(components.next(), Some(std::path::Component::Normal(component)) if component == std::ffi::OsStr::new(session_id))
        && components.next().is_none();
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session id must be a single path component",
        ));
    }
    Ok(())
}

fn require_real_directory(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("not a real directory: {}", path.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("directory is a reparse point: {}", path.display()),
            ));
        }
    }
    Ok(())
}

fn persisted_session_id_present(sessions_root: &Path, session_id: &str) -> io::Result<bool> {
    require_real_directory(sessions_root)?;
    let cwd_entries = match std::fs::read_dir(sessions_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for cwd_entry in cwd_entries {
        let cwd_entry = cwd_entry?;
        if !cwd_entry.file_type()?.is_dir() {
            continue;
        }
        require_real_directory(&cwd_entry.path())?;
        let candidate = cwd_entry.path().join(session_id);
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn sync_tree(path: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_dir() {
            sync_tree(&entry.path())?;
        } else if metadata.file_type().is_file() {
            std::fs::File::open(entry.path())?.sync_all()?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unexpected non-file in fork target: {}",
                    entry.path().display()
                ),
            ));
        }
    }
    sync_directory(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// [`for_each_jsonl_line_capped`] with the production cap.
fn for_each_jsonl_line<R: BufRead>(
    reader: R,
    f: impl FnMut(usize, &[u8]) -> io::Result<ControlFlow<()>>,
) -> io::Result<()> {
    for_each_jsonl_line_capped(reader, MAX_UPDATE_LINE_BYTES, f)
}

/// Invoke `f` with the index and bytes of each non-empty line, reusing one
/// capped line buffer. Lines over `cap` content bytes are discarded without
/// being buffered whole and consume no index. `f` returns `Break` to stop
/// early. Raw bytes rather than the typed `UpdatesIterator`: classification
/// must tolerate non-UTF-8 lines, and both copy passes need identical line
/// indexes.
fn for_each_jsonl_line_capped<R: BufRead>(
    mut reader: R,
    cap: usize,
    mut f: impl FnMut(usize, &[u8]) -> io::Result<ControlFlow<()>>,
) -> io::Result<()> {
    let mut buf = Vec::new();
    let mut index = 0;
    let mut discarded = 0usize;
    let result = loop {
        buf.clear();
        let n = reader
            .by_ref()
            .take(cap as u64 + 1)
            .read_until(b'\n', &mut buf)?;
        if n == 0 {
            break Ok(());
        }
        if buf.len() > cap && buf.last() != Some(&b'\n') {
            discarded += 1;
            if discarded == 1 {
                tracing::warn!(
                    max_bytes = cap,
                    "discarding over-long updates.jsonl line during fork copy"
                );
            }
            // Drain the remainder of the line without retaining it.
            loop {
                buf.clear();
                let n = reader
                    .by_ref()
                    .take(cap as u64)
                    .read_until(b'\n', &mut buf)?;
                if n == 0 || buf.last() == Some(&b'\n') {
                    break;
                }
            }
            continue;
        }
        let line = buf.trim_ascii();
        if line.is_empty() {
            continue;
        }
        if f(index, line)?.is_break() {
            break Ok(());
        }
        index += 1;
    };
    if discarded > 1 {
        tracing::warn!(
            discarded,
            max_bytes = cap,
            "discarded over-long updates.jsonl lines during fork copy"
        );
    }
    result
}

/// Indexes (in non-empty-line order) of the source lines that survive rewind
/// filtering and the `target_prompt_index` cut, holding one classification per
/// line instead of the lines. As in replay, an unparseable line classifies as
/// [`RewindStep::Other`] (ending a user run) and is skipped later at parse.
fn surviving_line_indexes<R: BufRead>(
    reader: R,
    target_prompt_index: usize,
) -> io::Result<Vec<usize>> {
    struct LineRecord {
        index: usize,
        step: RewindStep,
    }
    let mut records = Vec::new();
    for_each_jsonl_line(reader, |index, line| {
        let step = std::str::from_utf8(line).map_or(RewindStep::Other, rewind_step_for_line);
        records.push(LineRecord { index, step });
        Ok(ControlFlow::Continue(()))
    })?;
    let mut records = filter_rewind_by(records, |record| record.step);
    let keep = truncate_for_prompt_by(&records, target_prompt_index, |record| record.step);
    records.truncate(keep);
    Ok(records.into_iter().map(|record| record.index).collect())
}

/// Streaming writer for the fork target's `updates.jsonl`. Corruption-tolerant
/// like the load path: a torn or undecodable line is skipped with a warning
/// instead of failing the fork.
struct UpdateLineWriter<'a> {
    writer: BufWriter<std::fs::File>,
    source: &'a Path,
    target_session_id: &'a acp::SessionId,
    copied: CopiedUpdates,
    skipped_lines: usize,
}

impl<'a> UpdateLineWriter<'a> {
    fn try_new(
        target: &Path,
        source: &'a Path,
        target_session_id: &'a acp::SessionId,
    ) -> io::Result<Self> {
        Ok(Self {
            writer: BufWriter::new(std::fs::File::create(target)?),
            source,
            target_session_id,
            copied: CopiedUpdates::default(),
            skipped_lines: 0,
        })
    }

    fn copy_line(&mut self, line: &[u8]) -> io::Result<()> {
        let update = match std::str::from_utf8(line).map(SessionUpdateEnvelope::from_str) {
            Ok(Ok(update)) => update,
            Ok(Err(error)) => {
                self.skip_torn_line(&error);
                return Ok(());
            }
            Err(error) => {
                self.skip_torn_line(&error);
                return Ok(());
            }
        };
        if is_orchestration_projection_update(&update) {
            return Ok(());
        }
        if let SessionUpdate::Xai(notification) = &update
            && let crate::extensions::notification::SessionUpdate::CompactionCheckpoint(info) =
                &notification.update
        {
            self.copied
                .checkpoint_files
                .insert(info.checkpoint_file.clone());
        }
        let update = transform_session_id_in_update(update, self.target_session_id);
        let envelope = SessionUpdateEnvelope::from_update(&update).map_err(invalid_data)?;
        serde_json::to_writer(&mut self.writer, &envelope).map_err(invalid_data)?;
        self.writer.write_all(b"\n")?;
        self.copied.count += 1;
        Ok(())
    }

    fn skip_torn_line(&mut self, error: &dyn std::fmt::Display) {
        self.skipped_lines += 1;
        if self.skipped_lines == 1 {
            tracing::warn!(
                error = %error,
                path = %self.source.display(),
                "skipping unparseable updates.jsonl line during fork copy (torn append?)"
            );
        }
    }

    fn finish(mut self) -> io::Result<CopiedUpdates> {
        // The first skipped line already warned with its parse error.
        if self.skipped_lines > 1 {
            tracing::warn!(
                skipped = self.skipped_lines,
                copied = self.copied.count,
                path = %self.source.display(),
                "skipped unparseable session update lines during fork copy"
            );
        }
        self.writer.flush()?;
        Ok(self.copied)
    }
}

/// Copy `source` (an `updates.jsonl`) to `target` without materializing it.
/// With a `target_prompt_index`, pass one computes the surviving line set and
/// pass two writes exactly those lines; without one, every line streams
/// through, preserving rewind markers and dead branches. Both passes read one
/// pinned, rewound file handle, so their line indexes cannot skew under a
/// concurrent rename; `updates.jsonl` is append-only by contract, so lines
/// appended after pass one land past every survivor index.
fn copy_updates_streaming(
    source: &Path,
    target: &Path,
    target_session_id: &acp::SessionId,
    target_prompt_index: Option<usize>,
) -> io::Result<CopiedUpdates> {
    let mut writer = UpdateLineWriter::try_new(target, source, target_session_id)?;
    let mut file = match std::fs::File::open(source) {
        Ok(file) => file,
        // A missing source is an empty transcript; still write the target.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return writer.finish(),
        Err(error) => return Err(error),
    };
    match target_prompt_index {
        None => {
            for_each_jsonl_line(BufReader::new(file), |_, line| {
                writer.copy_line(line)?;
                Ok(ControlFlow::Continue(()))
            })?;
        }
        Some(target_idx) => {
            let survivors = surviving_line_indexes(BufReader::new(&mut file), target_idx)?;
            file.seek(io::SeekFrom::Start(0))?;
            let mut survivors = survivors.into_iter().peekable();
            for_each_jsonl_line(BufReader::new(file), |index, line| {
                if survivors.next_if_eq(&index).is_some() {
                    writer.copy_line(line)?;
                }
                Ok(if survivors.peek().is_none() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            })?;
        }
    }
    writer.finish()
}

impl JsonlStorageAdapter {
    /// Fully synchronous implementation of `copy_session_data`, for use on a
    /// blocking thread; every caller reaches it through `spawn_blocking`.
    pub(crate) fn copy_session_data_sync(
        &self,
        source_info: &Info,
        target_info: &Info,
        options: CopySessionOptions,
    ) -> io::Result<CopySessionResult> {
        let public_target_dir = self.session_dir(target_info);
        let publication = match &self.dir_mode {
            SessionDirMode::FromRoot(root_dir) => Some(CopyPublication::begin(
                root_dir,
                source_info,
                target_info,
                public_target_dir.clone(),
            )?),
            // Explicit adapters address a caller-owned directory rather than a
            // globally discoverable session identity. Preserve their existing
            // direct-copy contract.
            SessionDirMode::Explicit(_) => None,
        };
        let target_dir = publication
            .as_ref()
            .map(|publication| publication.target_dir().to_path_buf())
            .unwrap_or_else(|| public_target_dir.clone());
        if publication.is_none() {
            std::fs::create_dir_all(&target_dir)?;
        }
        let target_adapter = JsonlStorageAdapter::with_explicit_session_dir(target_dir.clone());

        let source_summary = self.read_summary_sync(source_info)?;
        let chat_format_version = source_summary.chat_format_version;

        let mut chat_to_copy: Vec<ConversationItem> =
            self.read_chat_history_sync(self.chat_file(source_info), chat_format_version)?;

        if let Some(target_idx) = options.target_prompt_index {
            // +1: the cut keeps the target prompt inclusive.
            let keep = conversation_truncate_for_prompt(&chat_to_copy, target_idx + 1);
            chat_to_copy.truncate(keep);
        }

        if options.fork_filter {
            fork_filter_chat(&mut chat_to_copy);
        }

        for target in [
            target_adapter.workflows_dir(target_info),
            target_adapter
                .goal_mode_state_file(target_info)
                .parent()
                .expect("goal state has a parent")
                .to_path_buf(),
        ] {
            match std::fs::remove_dir_all(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }

        // The child inherits everything below this boundary; compaction
        // preserves it.
        let inherited_prefix_len = if options.fork_filter {
            Some(chat_to_copy.len())
        } else {
            options.inherited_prefix_len
        };

        // Worktree forks skip the cwd rewrite: their display_cwd already
        // shows the model the original project path, and rewritten
        // conversation paths would contradict it.
        if !options.skip_cwd_transform && source_info.cwd != target_info.cwd {
            transform_conversation_cwd(&mut chat_to_copy, &source_info.cwd, &target_info.cwd);
        }

        // Compaction summaries carry an absolute pointer to the source
        // session's immutable segment archive. A fork copies that archive, so
        // retarget the exact pointer to the child's final public location. Do
        // this independently of the workspace CWD transform: source and target
        // sessions commonly share a CWD, and the session archive lives under
        // the storage root rather than inside that workspace.
        if options.copy_compaction_segments {
            let source_compaction_dir = self
                .session_dir(source_info)
                .join(xai_chat_state::compaction_transcript::COMPACTION_DIR);
            let target_compaction_dir =
                public_target_dir.join(xai_chat_state::compaction_transcript::COMPACTION_DIR);
            rebind_compaction_hint(
                &mut chat_to_copy,
                xai_chat_state::CompactionMode::Segments(
                    xai_chat_state::CompactionDetail::default(),
                ),
                &source_compaction_dir,
                &target_compaction_dir,
            );
        }

        // Transcript-mode summaries similarly point at the source session's
        // updates.jsonl. Normal forks copy that transcript; bind the exact
        // source file pointer to the child's public file. A fork-filter copy
        // intentionally starts with an empty transcript, so it must not claim
        // to preserve the parent's raw transcript.
        if !options.fork_filter {
            let source_updates = self.updates_file(source_info);
            let target_updates = public_target_dir.join("updates.jsonl");
            rebind_compaction_hint(
                &mut chat_to_copy,
                xai_chat_state::CompactionMode::Transcript,
                &source_updates,
                &target_updates,
            );
        }

        if options.strip_reasoning {
            chat_to_copy = xai_chat_state::compaction_utils::strip_reasoning_blocks(chat_to_copy);
        }

        let num_chat_messages = chat_to_copy.len();
        let cwd_switch_bookkeeping_generation = chat_to_copy
            .iter()
            .filter_map(ConversationItem::working_directory_switch_generation)
            .max()
            .unwrap_or(0);

        // Release chat history before the (typically much larger) updates copy.
        {
            let mut writer = BufWriter::new(std::fs::File::create(
                target_adapter.chat_file(target_info),
            )?);
            for item in &chat_to_copy {
                serde_json::to_writer(&mut writer, item).map_err(invalid_data)?;
                writer.write_all(b"\n")?;
            }
            writer.flush()?;
        }
        drop(chat_to_copy);

        // A fork_filter copy (subagent context bootstrap) starts the child with
        // an empty replay transcript, so the source updates are never read.
        let copied_updates = if options.fork_filter {
            std::fs::write(target_adapter.updates_file(target_info), b"")?;
            CopiedUpdates::default()
        } else {
            copy_updates_streaming(
                &self.updates_file(source_info),
                &target_adapter.updates_file(target_info),
                &target_info.id,
                options.target_prompt_index,
            )?
        };
        let checkpoint_files = copied_updates.checkpoint_files;
        let num_messages = copied_updates.count;

        let target_summary = fork_summary(
            source_summary,
            target_info,
            &options,
            ForkCounters {
                num_messages,
                num_chat_messages,
                cwd_switch_bookkeeping_generation,
                inherited_prefix_len,
            },
        );
        let summary_bytes = serde_json::to_vec_pretty(&target_summary).map_err(invalid_data)?;
        std::fs::write(target_adapter.summary_file(target_info), summary_bytes)?;

        let plan_copied = copy_sidecar_file(
            options.copy_plan_state,
            &self.plan_file(source_info),
            &target_adapter.plan_file(target_info),
        )?;
        let signals_copied = copy_sidecar_file(
            options.copy_signals,
            &self.signals_file(source_info),
            &target_adapter.signals_file(target_info),
        )?;
        let plan_mode_state_copied = copy_sidecar_file(
            options.copy_plan_mode_state,
            &self.plan_mode_state_file(source_info),
            &target_adapter.plan_mode_state_file(target_info),
        )?;
        let tool_state_copied = copy_sidecar_file(
            options.copy_tool_state,
            &self.session_dir(source_info).join("tool_state.json"),
            &target_dir.join("tool_state.json"),
        )?;
        let announcement_state_copied = copy_sidecar_file(
            options.copy_announcement_state,
            &self.announcement_state_file(source_info),
            &target_adapter.announcement_state_file(target_info),
        )?;

        // Copied verbatim: the archive is immutable, so no cwd rewrite.
        let compaction_segments_copied = if options.copy_compaction_segments {
            let src_dir = self
                .session_dir(source_info)
                .join(xai_chat_state::compaction_transcript::COMPACTION_DIR);
            let mut copied = 0usize;
            if src_dir.is_dir() {
                let dst_dir =
                    target_dir.join(xai_chat_state::compaction_transcript::COMPACTION_DIR);
                std::fs::create_dir_all(&dst_dir)?;
                for entry in std::fs::read_dir(&src_dir)? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        std::fs::copy(entry.path(), dst_dir.join(entry.file_name()))?;
                        copied += 1;
                    }
                }
            }
            copied
        } else {
            0
        };

        let compaction_checkpoints_copied = copy_referenced_checkpoints(
            &checkpoint_files,
            &self.session_dir(source_info),
            &target_dir,
            &source_info.id,
        )?;

        let result = CopySessionResult {
            chat_messages_copied: num_chat_messages,
            updates_copied: num_messages,
            plan_state_copied: plan_copied,
            plan_mode_state_copied,
            signals_copied,
            tool_state_copied,
            announcement_state_copied,
            compaction_segments_copied,
            compaction_checkpoints_copied,
        };
        match publication {
            Some(publication) => {
                reconcile_copy_publication(&target_info.id, result, publication.publish())
            }
            None => Ok(result),
        }
    }
}

/// Counters produced by this copy that feed the fork target's summary, named
/// so the same-typed counts cannot transpose.
struct ForkCounters {
    num_messages: usize,
    num_chat_messages: usize,
    cwd_switch_bookkeeping_generation: u64,
    inherited_prefix_len: Option<usize>,
}

/// Build the fork target's summary: counters from this copy, fork identity
/// from `options`, and per field either inheritance from the source or a
/// fresh-session reset.
fn fork_summary(
    source: Summary,
    target_info: &Info,
    options: &CopySessionOptions,
    counters: ForkCounters,
) -> Summary {
    Summary {
        info: target_info.clone(),
        cwd_generation: source.cwd_generation,
        previous_cwd: source.previous_cwd,
        pending_cwd_switch_reminder: None,
        cwd_switch_bookkeeping_generation: counters.cwd_switch_bookkeeping_generation,
        session_summary: source.session_summary,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        num_messages: counters.num_messages,
        num_chat_messages: counters.num_chat_messages,
        current_model_id: options
            .new_model_id
            .clone()
            .map(acp::ModelId::new)
            .unwrap_or_else(|| source.current_model_id.clone()),
        catalog_identity: options
            .new_model_id
            .is_none()
            .then_some(source.catalog_identity)
            .flatten(),
        parent_session_id: options.parent_session_id.clone(),
        forked_at: Some(chrono::Utc::now()),
        collection_id: None,
        next_trace_turn: 0,
        chat_format_version: CHAT_FORMAT_VERSION,
        prompt_display_cwd: options.prompt_display_cwd.clone(),
        session_kind: Some(
            options
                .session_kind
                .clone()
                .unwrap_or_else(|| "fork".to_string()),
        ),
        fork_context_source: options.fork_context_source.clone(),
        fork_parent_prompt_id: options.fork_parent_prompt_id.clone(),
        inherited_prefix_len: counters.inherited_prefix_len,
        hidden: None,
        source_workspace_dir: options.source_workspace_dir.clone(),
        git_root_dir: None,
        git_remotes: Vec::new(),
        head_commit: source.head_commit,
        head_branch: source.head_branch,
        request_id: None,
        // Fresh local grok_home, not inherited from source: the fork lives on this machine.
        grok_home: crate::session::persistence::grok_home_string(),
        last_active_at: source.last_active_at,
        generated_title: source.generated_title,
        // A fork keeps the parent's title, so its manual-ness rides along.
        title_is_manual: source.title_is_manual,
        worktree_label: source.worktree_label,
        agent_name: options
            .new_model_id
            .is_none()
            .then_some(source.agent_name)
            .flatten(),
        sandbox_profile: source.sandbox_profile,
        reasoning_effort: source.reasoning_effort,
        // Full forks keep the parent's last turn. Partial forks
        // (`target_prompt_index`) may drop that turn, so clear the summary
        // rather than showing work that is not in the child conversation.
        last_turn_summary: if options.target_prompt_index.is_some() {
            None
        } else {
            source.last_turn_summary
        },
        last_turn_summary_prompt_id: if options.target_prompt_index.is_some() {
            None
        } else {
            source.last_turn_summary_prompt_id
        },
    }
}

/// Copy one optional sidecar file (plan, signals, tool state, ...) when
/// enabled and present; reports whether a copy happened. A sidecar that
/// exists but is not a regular file is skipped with a warning rather than
/// failing the fork.
fn copy_sidecar_file(enabled: bool, src: &Path, dst: &Path) -> io::Result<bool> {
    if !enabled {
        return Ok(false);
    }
    if !src.is_file() {
        if src.exists() {
            tracing::warn!(
                path = %src.display(),
                "sidecar is not a regular file; skipping copy",
            );
        }
        return Ok(false);
    }
    std::fs::copy(src, dst)?;
    Ok(true)
}

/// Copy the `compaction_checkpoints/{uuid}.json` files referenced by the
/// retained records; returns how many copied. Records are user-editable data,
/// so only the exact path shape this feature writes may resolve, symlinks are
/// never followed, and dangling references are skipped rather than failing
/// the fork (otherwise every /rewind in the target session would fail).
fn copy_referenced_checkpoints(
    checkpoint_files: &BTreeSet<String>,
    source_session_dir: &Path,
    target_dir: &Path,
    source_id: &acp::SessionId,
) -> io::Result<usize> {
    if checkpoint_files.is_empty() {
        return Ok(0);
    }
    // The per-file `symlink_metadata` below only vets the final path
    // component, so the intermediate `compaction_checkpoints` dir must itself
    // be a real directory; a symlinked dir would resolve every matching name
    // outside the session.
    match std::fs::symlink_metadata(source_session_dir.join("compaction_checkpoints")) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(meta) => {
            tracing::warn!(
                file_type = ?meta.file_type(),
                session_id = %source_id,
                "compaction_checkpoints is not a real directory; skipping checkpoint copy",
            );
            return Ok(0);
        }
        // Dir gone means every record is dangling; same policy as a missing
        // checkpoint file.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::warn!(
                session_id = %source_id,
                "compaction_checkpoints directory missing; skipping checkpoint copy",
            );
            return Ok(0);
        }
        Err(error) => return Err(error),
    }
    let mut copied = 0usize;
    for checkpoint_file in checkpoint_files {
        let relative = Path::new(checkpoint_file);
        // A doctored record path must not address other session files (e.g.
        // the fork's rewritten updates.jsonl).
        let well_formed = relative.parent() == Some(Path::new("compaction_checkpoints"))
            && relative.extension() == Some("json".as_ref());
        if !well_formed {
            tracing::warn!(
                checkpoint_file = %checkpoint_file,
                session_id = %source_id,
                "skipping compaction checkpoint with unexpected path during copy",
            );
            continue;
        }
        let src = source_session_dir.join(relative);
        match std::fs::symlink_metadata(&src) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(meta) => {
                // This feature only ever writes regular files, so don't
                // follow symlinks planted in the source session.
                tracing::warn!(
                    path = %src.display(),
                    file_type = ?meta.file_type(),
                    session_id = %source_id,
                    "compaction checkpoint source is not a regular file; skipping copy",
                );
                continue;
            }
            // Already-dangling record (e.g. a chained fork of a broken
            // session): the copy can't invent the file, so don't fail.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(
                    path = %src.display(),
                    session_id = %source_id,
                    "compaction checkpoint file missing from source; skipping copy",
                );
                continue;
            }
            Err(error) => return Err(error),
        }
        let dst = target_dir.join(relative);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)?;
        copied += 1;
    }
    Ok(copied)
}

fn invalid_data(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
