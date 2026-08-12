use chrono::{DateTime, Utc};
use fs2::FileExt;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::StorageMode;

use crate::remote::RemoteSync;

use crate::sampling::Client as OaiCompatClient;
use crate::sampling::ConversationItem;
use crate::session::export::ExportedMetadata;
use xai_grok_shell_base::util::anchored_directory::AnchoredDirectory;
use xai_grok_workspace::session::file_state::RewindPoint;

use crate::session::signals::SessionSignals;
use crate::session::storage::relocation::{RelocationError, RelocationView};
use crate::session::storage::{JsonlStorageAdapter, StorageAdapter};
use crate::tools::todo::TodoState;
use crate::util::grok_home::grok_home;
use agent_client_protocol as acp;
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
use xai_grok_sampling_types::ReasoningEffort;

use crate::extensions::notification::{
    DISK_FULL_ERROR_TYPE, DISK_FULL_USER_MESSAGE, RetryState,
    SessionNotification as XaiSessionNotification, SessionUpdate as XaiSessionUpdate,
};
use crate::session::info::Info;
use tokio::sync::{mpsc, watch};

#[cfg(test)]
struct PublishFreshAckTestHook {
    entered_tx: tokio::sync::oneshot::Sender<()>,
    release_rx: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
fn publish_fresh_ack_test_hooks()
-> &'static std::sync::Mutex<std::collections::HashMap<String, PublishFreshAckTestHook>> {
    static HOOKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, PublishFreshAckTestHook>>,
    > = std::sync::OnceLock::new();
    HOOKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
pub(crate) struct PublishFreshAckTestController {
    session_id: String,
    entered_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    release_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(test)]
impl PublishFreshAckTestController {
    pub(crate) async fn wait_until_entered(&mut self) {
        self.entered_rx
            .take()
            .expect("publish-fresh acknowledgement hook may be entered only once")
            .await
            .expect(
                "persistence actor exited before reaching the publish-fresh acknowledgement hook",
            );
    }

    pub(crate) fn release(&mut self) {
        self.release_tx
            .take()
            .expect("publish-fresh acknowledgement hook may be released only once")
            .send(())
            .expect("persistence actor exited before publish-fresh acknowledgement hook release");
    }
}

#[cfg(test)]
impl Drop for PublishFreshAckTestController {
    fn drop(&mut self) {
        publish_fresh_ack_test_hooks()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.session_id);
    }
}

#[cfg(test)]
pub(crate) fn install_publish_fresh_ack_test_hook(
    session_id: &str,
) -> PublishFreshAckTestController {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let previous = publish_fresh_ack_test_hooks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            session_id.to_owned(),
            PublishFreshAckTestHook {
                entered_tx,
                release_rx,
            },
        );
    assert!(
        previous.is_none(),
        "only one publish-fresh acknowledgement hook may be installed per session id"
    );
    PublishFreshAckTestController {
        session_id: session_id.to_owned(),
        entered_rx: Some(entered_rx),
        release_tx: Some(release_tx),
    }
}

#[cfg(test)]
async fn block_publish_fresh_ack_test_hook(session_id: &str) {
    let hook = publish_fresh_ack_test_hooks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(session_id);
    let Some(hook) = hook else {
        return;
    };
    let _ = hook.entered_tx.send(());
    let _ = hook.release_rx.await;
}

#[cfg(test)]
fn fresh_lease_transition_failure_test_hooks()
-> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static HOOKS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    HOOKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
struct FreshLeaseTransitionFailureTestGuard {
    session_id: String,
}

#[cfg(test)]
impl Drop for FreshLeaseTransitionFailureTestGuard {
    fn drop(&mut self) {
        fresh_lease_transition_failure_test_hooks()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.session_id);
    }
}

#[cfg(test)]
fn install_fresh_lease_transition_failure_test_hook(
    session_id: &str,
) -> FreshLeaseTransitionFailureTestGuard {
    let inserted = fresh_lease_transition_failure_test_hooks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_id.to_owned());
    assert!(
        inserted,
        "only one lease transition failure hook may be installed per session id"
    );
    FreshLeaseTransitionFailureTestGuard {
        session_id: session_id.to_owned(),
    }
}

#[cfg(test)]
fn take_fresh_lease_transition_failure_test_hook(session_id: &str) -> bool {
    fresh_lease_transition_failure_test_hooks()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(session_id)
}

/// Current chat history format version.
/// - Version 0: Legacy ChatRequestMessage format (default for old sessions)
/// - Version 1: ConversationItem format (used for new sessions)
pub const CHAT_FORMAT_VERSION: u8 = 1;

#[derive(Debug, Clone)]
pub struct PersistenceContentChunk {
    content_chunks: Vec<acp::ContentBlock>,
}

impl PersistenceContentChunk {
    pub(crate) fn new(content_chunks: Vec<acp::ContentBlock>) -> Self {
        Self { content_chunks }
    }
}

/// Mirrors generated titles to the session registry after local persistence succeeds.
#[derive(Clone)]
pub(crate) struct RegistryGeneratedTitleSync {
    pub client: crate::agent::session_registry_client::SessionRegistryClient,
    pub suppress_for_zdr: bool,
}

use crate::session::storage::SessionUpdate;
use serde::{Deserialize, Serialize};

// /btw side question persistence types

/// A single /btw side question entry persisted to `btw_history.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BtwEntry {
    /// Unique ID for this side question.
    pub btw_session_id: String,
    /// The parent session ID.
    pub parent_session_id: String,
    /// When the question was asked.
    pub asked_at: DateTime<Utc>,
    /// The user's question.
    pub question: String,
    /// The model's response (empty if failed).
    pub answer: String,
    /// Model used.
    pub model: String,
    /// Whether the request succeeded.
    pub success: bool,
    /// Error message if failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Model-call attempts made (1 = no retry). Entries written before this
    /// field existed deserialize as 1.
    #[serde(default = "default_btw_attempts")]
    pub attempts: u32,
}

fn default_btw_attempts() -> u32 {
    1
}

// Local feedback persistence types

/// A feedback entry persisted to `~/.grok/sessions/.../feedback.jsonl`.
///
/// Uses a tagged enum so different feedback types are self-describing in the
/// JSONL file (currently only `UserFeedback`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LocalFeedbackEntry {
    /// Regular user feedback (spontaneous or solicited via heuristics)
    UserFeedback(UserFeedbackEntry),
}

/// A user feedback entry (thumbs, stars, text, or dismiss).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFeedbackEntry {
    pub submitted_at: DateTime<Utc>,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_number: Option<i64>,
    /// Whether this was a response to a server-initiated FeedbackRequest
    pub solicited: bool,
    /// The feedback request ID (only set for solicited feedback)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// True if the user dismissed the feedback request without responding
    #[serde(default, skip_serializing_if = "is_false")]
    pub dismissed: bool,
    /// The full submission payload (omitted when dismissed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submission: Option<prod_mc_cli_chat_proxy_types::feedback_types::FeedbackSubmission>,
}

/// Helper for `#[serde(skip_serializing_if)]` on bool fields.
pub(crate) fn is_false(v: &bool) -> bool {
    !v
}

#[cfg(test)]
mod feedback_tests {
    use super::*;
    use prod_mc_cli_chat_proxy_types::feedback_types::{
        ClientType, FeedbackSubmission, FeedbackType, RatingType,
    };

    fn make_submission(thumbs_up: bool) -> FeedbackSubmission {
        FeedbackSubmission {
            session_id: "session-abc".into(),
            user_id: None,
            client_type: ClientType::Tui,
            feedback_type: if thumbs_up {
                FeedbackType::Rating
            } else {
                FeedbackType::RatingWithText
            },
            turn_number: Some(7),
            rating_type: Some(RatingType::Thumbs),
            rating_value: Some(if thumbs_up { 1 } else { -1 }),
            feedback_text: if thumbs_up {
                None
            } else {
                Some("could be better".into())
            },
            model_id: Some("grok-3-fast".into()),
            resolved_model_id: Some("grok-4.5".into()),
            ..Default::default()
        }
    }

    #[test]
    fn test_user_feedback_spontaneous_roundtrip() {
        let entry = LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: "session-abc".into(),
            turn_number: Some(7),
            solicited: false,
            request_id: None,
            dismissed: false,
            submission: Some(make_submission(true)),
        });

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""type":"user_feedback""#));
        assert!(!json.contains("dismissed")); // skip_serializing_if = is_false
        assert!(!json.contains("requestId")); // skip_serializing_if = Option::is_none

        let parsed: LocalFeedbackEntry = serde_json::from_str(&json).unwrap();
        let LocalFeedbackEntry::UserFeedback(ref uf) = parsed;
        assert!(!uf.solicited);
        assert!(!uf.dismissed);
        assert!(uf.submission.is_some());
        assert_eq!(uf.session_id, "session-abc");
    }

    #[test]
    fn test_user_feedback_solicited_roundtrip() {
        let entry = LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: "session-abc".into(),
            turn_number: Some(14),
            solicited: true,
            request_id: Some("req-123".into()),
            dismissed: false,
            submission: Some(make_submission(false)),
        });

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""requestId":"req-123""#));
        assert!(json.contains(r#""solicited":true"#));

        let parsed: LocalFeedbackEntry = serde_json::from_str(&json).unwrap();
        let LocalFeedbackEntry::UserFeedback(ref uf) = parsed;
        assert!(uf.solicited);
        assert_eq!(uf.request_id.as_deref(), Some("req-123"));
        let sub = uf.submission.as_ref().unwrap();
        assert_eq!(sub.feedback_text.as_deref(), Some("could be better"));
    }

    #[test]
    fn test_user_feedback_dismiss_roundtrip() {
        let entry = LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: "session-abc".into(),
            turn_number: None,
            solicited: true,
            request_id: Some("req-456".into()),
            dismissed: true,
            submission: None,
        });

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains(r#""dismissed":true"#));
        assert!(!json.contains("submission")); // skip_serializing_if = Option::is_none

        let parsed: LocalFeedbackEntry = serde_json::from_str(&json).unwrap();
        let LocalFeedbackEntry::UserFeedback(ref uf) = parsed;
        assert!(uf.dismissed);
        assert!(uf.submission.is_none());
    }

    #[test]
    fn test_feedback_jsonl_multi_line_roundtrip() {
        // Simulate multiple entries written to a JSONL file
        let entries = vec![
            LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
                submitted_at: chrono::Utc::now(),
                session_id: "s1".into(),
                turn_number: Some(1),
                solicited: false,
                request_id: None,
                dismissed: false,
                submission: Some(make_submission(true)),
            }),
            LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
                submitted_at: chrono::Utc::now(),
                session_id: "s1".into(),
                turn_number: None,
                solicited: true,
                request_id: Some("req-1".into()),
                dismissed: true,
                submission: None,
            }),
        ];

        // Serialize to JSONL
        let mut jsonl = String::new();
        for entry in &entries {
            let line = serde_json::to_string(entry).unwrap();
            jsonl.push_str(&line);
            jsonl.push('\n');
        }

        // Deserialize each line
        let parsed: Vec<LocalFeedbackEntry> = jsonl
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        assert_eq!(parsed.len(), 2);
        assert!(matches!(parsed[0], LocalFeedbackEntry::UserFeedback(_)));
        assert!(matches!(parsed[1], LocalFeedbackEntry::UserFeedback(_)));

        // Verify the dismiss entry
        let LocalFeedbackEntry::UserFeedback(ref uf) = parsed[1];
        assert!(uf.dismissed);
        assert!(uf.solicited);
    }
}

#[derive(Debug, Clone)]
pub struct CopiedSessionFile {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct SessionStateCopy {
    pub files: Vec<CopiedSessionFile>,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum PersistenceMsg {
    /// A session update (ACP update or xAI extension update)
    Update(SessionUpdate),
    AppendUpdateDurablyAndAck {
        update: SessionUpdate,
        respond_to:
            tokio::sync::oneshot::Sender<Result<(), crate::session::storage::AppendUpdateError>>,
    },
    ContentChunk(PersistenceContentChunk),
    ReplaceSummarySamplingConfig(xai_grok_sampler::SamplerConfig),
    Chat(ConversationItem),
    AppendCwdSwitchAndAck {
        item: ConversationItem,
        respond_to: tokio::sync::oneshot::Sender<
            Result<xai_chat_state::StrictAppendAck, xai_chat_state::StrictAppendError>,
        >,
    },
    /// Replace the entire chat history (used for compaction)
    ReplaceChatHistory(Vec<ConversationItem>),
    /// Replace the entire chat history and acknowledge the actual storage
    /// result. This is not a cross-file transaction; model switches use
    /// `ModelSwitchAndAck` below.
    ReplaceChatHistoryAndAck {
        messages: Vec<ConversationItem>,
        respond_to: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    CurrentModel {
        model_id: acp::ModelId,
        catalog_identity: Option<xai_chat_state::CatalogIdentity>,
        /// The active agent definition name (e.g. `"grok-build"`).
        /// Persisted in `summary.agent_name` so session resume doesn't depend
        /// on the mutable model catalog.
        agent_name: Option<String>,
        reasoning_effort: Option<Option<ReasoningEffort>>,
    },
    /// Persist the current model/harness selection and acknowledge the actual
    /// storage result. This is not a cross-file transaction; model switches use
    /// `ModelSwitchAndAck` below.
    CurrentModelAndAck {
        model_id: acp::ModelId,
        catalog_identity: Option<xai_chat_state::CatalogIdentity>,
        agent_name: Option<String>,
        reasoning_effort: Option<Option<ReasoningEffort>>,
        respond_to: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    /// Commit the chat/model generation as one crash-consistent operation.
    ModelSwitchAndAck {
        messages: Vec<ConversationItem>,
        model_id: acp::ModelId,
        catalog_identity: Option<xai_chat_state::CatalogIdentity>,
        agent_name: Option<String>,
        reasoning_effort: Option<ReasoningEffort>,
        summary_sampling_config: Option<xai_grok_sampler::SamplerConfig>,
        respond_to: tokio::sync::oneshot::Sender<
            Result<(), crate::session::storage::ModelSwitchCommitError>,
        >,
    },
    PlanState(TodoState),
    /// Plan mode lifecycle state to persist
    PlanModeState(crate::session::plan_mode::PlanModeSnapshot),
    /// A rewind point to persist
    RewindPoint(RewindPoint),
    /// Truncate rewind points from a specific prompt index (inclusive).
    /// Syncs the persisted file with the in-memory FileStateTracker after rewind.
    TruncateRewindPoints {
        from_index: usize,
    },
    /// Merge rewind points at indices >= `target_index` into the previous point
    /// (read-modify-write on disk, after a ConversationOnly rewind). Disk is
    /// authoritative, so a partial in-memory tracker can't truncate history.
    MergeRewindPointsFrom {
        target_index: usize,
    },
    /// Collection ID for telemetry tracing
    CollectionId(String),
    /// Monotonic telemetry turn counter and optional request_id for trace metadata/filenames.
    /// This is the "next turn" value (i.e., after increment).
    NextTraceTurn {
        next_trace_turn: u64,
        request_id: Option<String>,
    },
    /// Persist a snapshot of the session signals.
    Signals(SessionSignals),
    /// Persist announcement tracking state (MCP + skill announcement dedup).
    AnnouncementState(crate::session::announcement_state::AnnouncementState),
    /// Persist goal mode orchestration state.
    GoalModeState(crate::session::goal_tracker::GoalOrchestration),
    DeleteGoalModeState {
        respond_to: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    WorkflowRunState(crate::session::workflow::store::WorkflowRunManifest),
    WorkflowRunStateAndAck {
        manifest: crate::session::workflow::store::WorkflowRunManifest,
        respond_to: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    DeleteWorkflowRunState(String),
    /// Persist a local feedback entry (user feedback)
    Feedback(LocalFeedbackEntry),
    /// Persist a /btw side question entry
    Btw(BtwEntry),
    /// Persist updated HEAD commit and branch to summary.
    GitHead {
        commit: Option<String>,
        branch: Option<String>,
    },
    /// Persist a compaction checkpoint file to `compaction_checkpoints/{id}.json`.
    CompactionCheckpoint(crate::extensions::notification::CompactionCheckpointFile),
    /// Persist a compaction request+response artifact to
    /// `compaction_requests/{request_id}.json`. Used for offline prompt
    /// iteration — captures the exact ConversationItem list sent to the
    /// compaction model plus the summary it returned (or the final error).
    /// The file rides on the post-turn session archive to cloud storage automatically;
    /// no separate upload path is needed.
    CompactionRequest(crate::extensions::notification::CompactionRequestFile),
    /// Persist a recap request+response artifact to
    /// `recap_requests/{request_id}.json`. Same GCS ride-along as
    /// compaction requests; enables offline recap prompt / garble replay.
    RecapRequest(crate::extensions::notification::RecapRequestFile),
    /// Persist a compaction segment (`Segments` mode).
    CompactionSegment(crate::extensions::notification::CompactionSegmentFile),
    /// Generated session title from background LLM task.
    /// Routed back through the persistence channel so the storage write
    /// stays sequential with other summary.json mutations.
    GeneratedTitle(String),
    /// Per-turn dashboard summary as `(text, prompt_id)`; replaces (`Some`)
    /// or clears (`None`, on conversation rewind) the previous one in
    /// `summary.json`.
    LastTurnSummary(Option<(String, String)>),
    /// Delete a provisional fresh session and stop without flushing or
    /// remote writeback. Used only when `/new` fails its final auth seal.
    AbortFreshAndDelete {
        publication_gate: crate::session::SessionPublicationGate,
        respond_to: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    /// Durably prepare a provisional fresh session for publication, then release
    /// its cross-process id claim when the supplied gate becomes published. The
    /// acknowledgement confirms all pending state and existing session files are
    /// durable and that the actor has armed the gate; it does not release the claim.
    PublishFresh {
        publication_gate: crate::session::SessionPublicationGate,
        respond_to: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    /// Enable remote writeback for a session created `Local` before remote
    /// settings resolved (non-blocking startup); backfills its local history.
    UpgradeToWriteback {
        auth_manager: Arc<crate::auth::AuthManager>,
    },
    Flush,
    /// Flush all pending writes, then signal the caller once the flush is complete.
    /// Unlike `Flush` (fire-and-forget), this is a **sync barrier**: the caller's
    /// oneshot only resolves after `flush_pending()` finishes writing to disk.
    FlushAndAck {
        respond_to: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    ProbeWritable {
        respond_to: tokio::sync::oneshot::Sender<io::Result<()>>,
    },
    /// Flush all pending writes, then copy the current session directory contents and return
    /// the in-memory snapshot to the caller (who can tar.gz + upload to GCS, etc.).
    CopyFile {
        one_shot: tokio::sync::oneshot::Sender<anyhow::Result<SessionStateCopy>>,
    },
}

pub use xai_grok_shared::session::session_dir;

type RelocationResult<T> = crate::session::storage::relocation::Result<T>;
type SummaryReader = fn(&Path) -> RelocationResult<Summary>;

/// Presence of this file keeps a freshly-created session out of every local
/// discovery path until the final, synchronous publication commit.
pub(crate) const UNPUBLISHED_SESSION_MARKER: &str = ".unpublished";

fn storage_view(sessions_root: &Path) -> RelocationResult<RelocationView> {
    RelocationView::load_for_sessions_root(sessions_root)
}

/// Check if a session exists locally under the given cwd.
///
/// This is the correct check for the `-r` resume path: a session is only
/// "already local" if it lives under the **same** cwd as the current invocation.
/// A session stored under a different cwd does NOT satisfy this check — the
/// caller must still run the remote restore into the requested cwd.
pub fn session_exists_for_cwd(session_id: &str, cwd: &str) -> bool {
    let sessions_root = crate::util::grok_home::grok_home().join("sessions");
    session_exists_for_cwd_in_root(session_id, cwd, &sessions_root)
}

/// A directory is a resumable session only if it has a `summary.json`; this
/// skips `images/`-only stubs that would otherwise hijack `--resume`. Used by
/// the resume/restore resolution path; `find_session_dir_by_id` intentionally
/// stays dir-only for non-resume compatibility.
fn is_persisted_session_dir(session_path: &Path) -> bool {
    !has_unpublished_session_marker(session_path) && session_path.join("summary.json").is_file()
}

fn has_unpublished_session_marker(session_dir: &Path) -> bool {
    std::fs::symlink_metadata(session_dir.join(UNPUBLISHED_SESSION_MARKER)).is_ok()
}

/// Inner implementation of `session_exists_for_cwd` with an injectable root.
/// Separated for deterministic tempdir-based tests.
fn session_exists_for_cwd_in_root(session_id: &str, cwd: &str, sessions_root: &Path) -> bool {
    let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
    let session_path = sessions_root.join(&encoded).join(session_id);
    is_persisted_session_dir(&session_path)
}

/// Find the local child session id that was previously restored from `remote_session_id`
/// in the given `cwd`.
///
/// When a remote session is restored, a new local child is created with
/// `summary.parent_session_id == remote_session_id`.  On a second
/// `grok -r <remote_id>` in the same cwd, this function returns the already-restored
/// child so no duplicate restore is performed.
///
/// If multiple children match (e.g., from pre-fix duplicate restores), the
/// most recently used one is returned.  Selection is fully deterministic:
/// 1. Newest `updated_at` timestamp in `summary.json`
/// 2. Newest session directory mtime as a tie-breaker (catches equal timestamps)
/// 3. Lexicographically largest session id as the final stable tie-breaker
///
/// Returns `Some(local_child_id)` when at least one matching child is found.
/// Returns `None` when no child with `parent_session_id == remote_session_id` exists.
pub fn find_local_child_for_remote(remote_session_id: &str, cwd: &str) -> Option<String> {
    let sessions_root = crate::util::grok_home::grok_home().join("sessions");
    find_local_child_for_remote_in_root(remote_session_id, cwd, &sessions_root)
}

/// Resolve a session ID to one that is available locally under `cwd`.
///
/// Checks in order:
///   1. `session_id` exists directly under `cwd` → returns it as-is.
///   2. A previously restored child of `session_id` exists → returns the child ID.
///   3. Neither found → returns `None` (caller should restore from remote).
pub fn resolve_local_session(session_id: &str, cwd: &str) -> Option<String> {
    if session_exists_for_cwd(session_id, cwd) {
        return Some(session_id.to_string());
    }
    find_local_child_for_remote(session_id, cwd)
}

// Repo-wide session resolution (for worktree resume)

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LocalSessionResolutionKind {
    ExactCwd,
    RestoredChildInExactCwd,
    SameRepoDifferentCwd,
    RestoredChildInSameRepoDifferentCwd,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResolvedLocalSession {
    pub session_id: String,
    pub cwd: String,
    pub resolution_kind: LocalSessionResolutionKind,
}

/// Resolve a session across multiple candidate cwds for worktree resume.
///
/// The first cwd in `candidate_cwds` should be the exact current cwd so it
/// gets priority. For each candidate, checks both direct session existence
/// and previously-restored children.
///
/// Returns `None` when no local match exists in any candidate.
pub(crate) fn resolve_local_session_for_repo(
    session_id: &str,
    candidate_cwds: &[&str],
) -> Option<ResolvedLocalSession> {
    let sessions_root = crate::util::grok_home::grok_home().join("sessions");
    resolve_local_session_for_repo_in_root(session_id, candidate_cwds, &sessions_root)
}

pub(crate) fn resolve_local_session_for_repo_in_root(
    session_id: &str,
    candidate_cwds: &[&str],
    sessions_root: &Path,
) -> Option<ResolvedLocalSession> {
    for (i, &cwd) in candidate_cwds.iter().enumerate() {
        let is_exact = i == 0;

        if session_exists_for_cwd_in_root(session_id, cwd, sessions_root) {
            return Some(ResolvedLocalSession {
                session_id: session_id.to_owned(),
                cwd: cwd.to_owned(),
                resolution_kind: if is_exact {
                    LocalSessionResolutionKind::ExactCwd
                } else {
                    LocalSessionResolutionKind::SameRepoDifferentCwd
                },
            });
        }

        if let Some(child_id) = find_local_child_for_remote_in_root(session_id, cwd, sessions_root)
        {
            return Some(ResolvedLocalSession {
                session_id: child_id,
                cwd: cwd.to_owned(),
                resolution_kind: if is_exact {
                    LocalSessionResolutionKind::RestoredChildInExactCwd
                } else {
                    LocalSessionResolutionKind::RestoredChildInSameRepoDifferentCwd
                },
            });
        }
    }
    None
}
fn find_local_child_for_remote_in_root(
    remote_session_id: &str,
    cwd: &str,
    sessions_root: &Path,
) -> Option<String> {
    let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
    let cwd_dir = sessions_root.join(&encoded);
    if !cwd_dir.exists() {
        return None;
    }

    // Collect all matching children.  Multiple can exist when a user ran
    // `grok -r <remote_id>` before this fix was deployed.
    // Tuple: (updated_at, dir_mtime_nanos, session_id) — all sorted descending.
    let mut candidates: Vec<(String, u128, String)> = Vec::new();

    let entries = std::fs::read_dir(&cwd_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if has_unpublished_session_marker(&path) {
            continue;
        }
        let summary_path = path.join("summary.json");
        if !summary_path.exists() {
            continue;
        }
        // Parse minimum fields without deserializing the full Summary,
        // so we don't fail on missing/extra fields from older/newer formats.
        if let Ok(raw) = std::fs::read_to_string(&summary_path)
            && let Ok(partial) = serde_json::from_str::<serde_json::Value>(&raw)
            && partial.get("parent_session_id").and_then(|v| v.as_str()) == Some(remote_session_id)
            && let Some(session_id) = path.file_name().and_then(|n| n.to_str())
        {
            let updated_at = partial
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Directory mtime as a tie-breaker for equal updated_at values.
            let dir_mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            candidates.push((updated_at, dir_mtime, session_id.to_string()));
        }
    }

    // Sort descending by all three keys for full determinism.
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
    candidates.into_iter().next().map(|(_, _, id)| id)
}

/// Check if a session exists locally by session ID.
/// Searches across ALL cwd directories under `~/.grok/sessions/`.
///
/// Use `session_exists_for_cwd` instead when the target cwd is known
/// (e.g., the `-r` resume path) to avoid false-positive matches.
/// Find a session by ID across **all** CWD directories under `~/.grok/sessions/`.
///
/// Unlike [`resolve_local_session`] which only checks a single CWD,
/// this scans every encoded-CWD subdirectory. Returns the decoded CWD path
/// that contains the session, or `None` if not found anywhere.
///
/// This is used by the pager's `--resume` to find sessions that were created
/// in a different CWD (e.g., a worktree) than the one the user is currently in.
pub fn resolve_local_session_any_cwd(session_id: &str) -> Option<String> {
    resolve_local_session_any_cwd_result(session_id)
        .ok()
        .flatten()
}

pub(crate) fn resolve_local_session_any_cwd_result(session_id: &str) -> io::Result<Option<String>> {
    resolve_local_session_any_cwd_in_root(session_id, &grok_home().join("sessions"))
        .map_err(io::Error::other)
}

fn resolve_local_session_any_cwd_in_root(
    session_id: &str,
    sessions_root: &Path,
) -> Result<Option<String>, crate::session::storage::relocation::RelocationError> {
    let Some(session_path) = storage_view(sessions_root)?.find_persisted_session_dir(session_id)?
    else {
        return Ok(None);
    };
    Ok(session_path
        .parent()
        .and_then(crate::util::grok_home::decode_cwd_from_dirname))
}

/// Scan all CWD directories for a session and return its directory path.
pub fn find_session_dir_by_id(session_id: &str) -> Option<PathBuf> {
    find_any_session_dir_by_id_result(session_id).ok().flatten()
}

pub(crate) fn find_persisted_session_dir_by_id_result(
    session_id: &str,
) -> io::Result<Option<PathBuf>> {
    find_persisted_session_dir_by_id_in_root_result(session_id, &grok_home().join("sessions"))
}

pub(crate) fn find_persisted_session_dir_by_id_in_root_result(
    session_id: &str,
    sessions_root: &Path,
) -> io::Result<Option<PathBuf>> {
    storage_view(sessions_root)
        .and_then(|view| view.find_persisted_session_dir(session_id))
        .map_err(io::Error::other)
}

pub(crate) fn find_any_session_dir_by_id_result(session_id: &str) -> io::Result<Option<PathBuf>> {
    storage_view(&grok_home().join("sessions"))
        .and_then(|view| view.find_any_session_dir(session_id))
        .map_err(io::Error::other)
}

#[cfg(test)]
fn session_exists_in_root(session_id: &str, sessions_root: &Path) -> bool {
    find_persisted_session_dir_by_id_in_root_result(session_id, sessions_root)
        .is_ok_and(|path| path.is_some())
}

/// Find and read a session summary given only its ID (scans all CWD directories).
pub(crate) fn find_summary_by_session_id(session_id: &str) -> Option<Summary> {
    find_summary_by_session_id_in_root(session_id, &grok_home().join("sessions"))
}

/// Inner implementation with injectable root for testing.
pub(crate) fn find_summary_by_session_id_in_root(
    session_id: &str,
    sessions_root: &Path,
) -> Option<Summary> {
    let path = storage_view(sessions_root)
        .ok()?
        .find_persisted_session_dir(session_id)
        .ok()
        .flatten()?;
    read_summary_from_dir(&path).ok()
}

fn read_summary_from_dir(session_dir: &Path) -> RelocationResult<Summary> {
    let path = session_dir.join("summary.json");
    let bytes = std::fs::read(&path).map_err(|error| RelocationError::Io {
        operation: "read",
        path: path.clone(),
        source: error,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| RelocationError::Json { path, source })
}

/// The most recently updated local session summary for `cwd` (by
/// `last_active_at` else `updated_at`), or `None` if there are no local sessions
/// for that cwd. Sync and local-only — suitable for the startup path that must
/// resolve the sandbox profile before the (irreversible) OS sandbox is applied.
fn most_recent_local_summary_for_cwd_in_root(cwd: &str, sessions_root: &Path) -> Option<Summary> {
    most_recent_local_summary_for_cwd_in_view(
        cwd,
        &storage_view(sessions_root).ok()?,
        read_summary_from_dir,
    )
    .ok()
    .flatten()
}

fn most_recent_local_summary_for_cwd_in_view(
    cwd: &str,
    view: &RelocationView,
    read_summary: SummaryReader,
) -> RelocationResult<Option<Summary>> {
    let mut best: Option<Summary> = None;
    for session_dir in view.session_dirs(Some(cwd))? {
        let summary = match read_summary(&session_dir) {
            Ok(summary) => summary,
            Err(RelocationError::Json { .. }) => continue,
            Err(RelocationError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => return Err(error),
        };
        if summary.is_hidden() {
            continue;
        }
        if best.as_ref().is_none_or(|current| {
            let time = summary.last_active_at.unwrap_or(summary.updated_at);
            let current_time = current.last_active_at.unwrap_or(current.updated_at);
            time > current_time
                || (time == current_time && summary.info.id.0.as_ref() < current.info.id.0.as_ref())
        }) {
            best = Some(summary);
        }
    }
    Ok(best)
}

/// Sync, local-only session summaries for `cwd` (hidden sessions filtered).
/// For startup paths that must resolve a resume target before the
/// irreversible OS sandbox is applied; async callers use [`list_summaries`].
///
/// Listing failures propagate so pre-sandbox callers can fail closed;
/// individual unreadable summaries are skipped, matching the async path's
/// tolerance for a single corrupt file.
pub fn local_summaries_for_cwd_sync(cwd: &str) -> io::Result<Vec<Summary>> {
    local_summaries_for_cwd_sync_in_root(cwd, &grok_home().join("sessions"))
}

fn local_summaries_for_cwd_sync_in_root(
    cwd: &str,
    sessions_root: &Path,
) -> io::Result<Vec<Summary>> {
    let view = storage_view(sessions_root).map_err(io::Error::other)?;
    let dirs = view.session_dirs(Some(cwd)).map_err(io::Error::other)?;
    Ok(dirs
        .iter()
        .filter_map(|dir| read_summary_from_dir(dir).ok())
        .filter(|s| !s.is_hidden())
        .collect())
}

/// Best-effort lookup of the sandbox profile persisted with a session that is
/// about to be resumed, used at startup to restore the session's profile before
/// the (irreversible) OS sandbox is applied.
///
/// - `session_id`: the explicit id from `--resume <id>` / `--load <id>` /
///   `-s <id>`. Resolved directly across all cwds, then — for a remote id that
///   was restored into a local child — via that child's `parent_session_id`.
/// - `cwd`: the current working directory. Used to resolve a remote id to its
///   local child, and as the lookup key for `-c` / `--continue` and bare
///   `--resume` (most-recent-for-cwd).
///
/// Returns `None` when not resuming, the session isn't found locally, or it has
/// no persisted profile (sessions created before this was tracked) — callers
/// then fall back to the normal config/CLI resolution.
pub fn resumed_session_sandbox_profile(
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> Option<String> {
    resumed_session_sandbox_profile_in_root(session_id, cwd, &grok_home().join("sessions"))
}

fn resumed_session_sandbox_profile_in_root(
    session_id: Option<&str>,
    cwd: Option<&str>,
    sessions_root: &Path,
) -> Option<String> {
    if let Some(id) = session_id.filter(|s| !s.is_empty()) {
        // Direct match by id (across all cwds).
        if let Some(summary) = find_summary_by_session_id_in_root(id, sessions_root) {
            return summary.sandbox_profile;
        }
        // A remote id resumes into a local child (fresh id, `parent_session_id`
        // = remote id). Mirror the canonical resume path so the peek doesn't
        // miss the restored session's saved profile.
        if let Some(cwd) = cwd
            && let Some(child) = find_local_child_for_remote_in_root(id, cwd, sessions_root)
        {
            return find_summary_by_session_id_in_root(&child, sessions_root)
                .and_then(|s| s.sandbox_profile);
        }
        return None;
    }
    if let Some(cwd) = cwd {
        return most_recent_local_summary_for_cwd_in_root(cwd, sessions_root)
            .and_then(|s| s.sandbox_profile);
    }
    None
}

/// Get file path for storing a large prompt.
/// Creates the prompts subdirectory if it doesn't exist.
/// Path format: `{session_dir}/prompts/prompt_{prompt_index}.txt`
pub(crate) fn get_prompt_file_path(info: &Info, prompt_index: usize) -> PathBuf {
    let prompts_dir = session_dir(info).join("prompts");
    std::fs::create_dir_all(&prompts_dir).ok();
    prompts_dir.join(format!("prompt_{}.txt", prompt_index))
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingCwdSwitchReminder {
    pub cwd_generation: u64,
    pub previous_cwd: String,
    #[serde(alias = "cwd")]
    pub destination_cwd: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_project_instructions: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Summary {
    pub info: Info,
    /// Monotonic generation of the authoritative cwd in `info.cwd`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cwd_generation: u64,
    /// Cwd immediately preceding the current generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_cwd: Option<String>,
    /// Reminder staged for exactly-once append during relocation completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_cwd_switch_reminder: Option<PendingCwdSwitchReminder>,
    /// Latest switch generation reflected in `num_chat_messages` bookkeeping.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cwd_switch_bookkeeping_generation: u64,
    pub session_summary: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub num_messages: usize,
    #[serde(default)]
    pub num_chat_messages: usize,
    pub current_model_id: acp::ModelId,
    /// Canonical catalog selection lineage. Absent in legacy summaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_identity: Option<xai_chat_state::CatalogIdentity>,
    /// Parent session ID if this session was forked from another session
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Timestamp when this session was forked (only set for forked sessions)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_at: Option<DateTime<Utc>>,
    /// Collection ID for telemetry trace uploads (one per session)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_id: Option<String>,
    /// Next telemetry trace turn id (monotonic, persisted).
    /// Used to generate unique turn ids for telemetry metadata/filenames even across rewinds.
    #[serde(default)]
    pub next_trace_turn: u64,
    /// Chat history format version:
    /// - 0 (default): Legacy ChatRequestMessage format
    /// - 1: ConversationItem format
    #[serde(default)]
    pub chat_format_version: u8,
    /// Stable display path for forked sessions.
    ///
    /// When set, the system prompt's `Workspace Path` and prompt metadata
    /// paths show this value instead of the real worktree/overlay path
    /// (`info.cwd`). Persisted so the override survives session
    /// restore/reload without the caller needing to resend it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_display_cwd: Option<String>,
    /// What created this session: `"fork"`, `"subagent"`, `"subagent_fork"`, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<String>,
    /// How the session's initial context was bootstrapped: `"new"` or `"forked"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_context_source: Option<String>,
    /// The parent prompt/turn ID that triggered this fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_parent_prompt_id: Option<String>,
    /// Number of conversation items inherited from the parent session.
    /// During compaction, items below this index are preserved as-is
    /// (the "inherited prefix"). Only items after this boundary are
    /// summarized. `None` means no inherited prefix (non-forked session).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_prefix_len: Option<usize>,
    /// Visibility override. None = default for `session_kind`, Some = explicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// The original workspace directory this worktree session was spawned from.
    /// Used by clients to group worktree sessions under their source workspace
    /// regardless of the worktree's actual `cwd`. Only set when
    /// `session_kind == "worktree"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_workspace_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_root_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git_remotes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Absolute path to the `.grok` directory, used by reconstruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grok_home: Option<String>,
    /// When the session last had content added (user or model messages).
    /// Only advanced locally by `append_update` / `append_chat_message`;
    /// never touched by remote registry operations or metadata-only writes.
    /// `None` for sessions created before this field was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<DateTime<Utc>>,
    /// LLM-generated session title persisted separately from `session_summary`.
    /// When present, this is preferred for display over `session_summary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_title: Option<String>,
    /// True when `generated_title` was set by a manual `/rename` (vs auto LLM
    /// title). Manual titles render inline in the prompt's top border on
    /// resume.
    #[serde(default, skip_serializing_if = "is_false")]
    pub title_is_manual: bool,
    /// Human-readable label for the worktree directory (e.g. "nuke-v-tables").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_label: Option<String>,
    /// The agent definition name that was active when the session was last saved.
    /// Used during session resume to avoid re-deriving from the (mutable) model
    /// catalog — if the model is removed or its `agent_type` changes between
    /// sessions, this persisted value ensures the correct harness is restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// The OS sandbox profile this session ran under (e.g. "workspace",
    /// "strict", "off", or a custom name). Persisted so a resumed session is
    /// restored to the same profile instead of silently falling back to the
    /// config default — which would otherwise break commands that worked before
    /// (a stricter profile denies filesystem/network the session relied on).
    /// `None` for sessions created before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Ultra-short summary of the most recent successful turn, shown as the
    /// dashboard row's secondary line (via the roster for non-attached
    /// clients). Displayed until replaced by the next successful turn (or
    /// cleared by a conversation rewind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_summary: Option<String>,
    /// Prompt id of the turn `last_turn_summary` describes (provenance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_summary_prompt_id: Option<String>,
}

/// Current `grok_home` as a UTF-8 string, or `None` if the path isn't valid UTF-8.
pub(crate) fn grok_home_string() -> Option<String> {
    crate::util::grok_home::grok_home()
        .to_str()
        .map(String::from)
}

pub fn default_model_id() -> acp::ModelId {
    acp::ModelId::new(crate::models::default_model())
}

impl Summary {
    pub(crate) fn new(info: &Info, model_id: acp::ModelId) -> std::io::Result<Self> {
        let git_metadata =
            xai_grok_workspace::session::git::resolve_persisted_session_git_metadata_sync(
                std::path::Path::new(&info.cwd),
            );
        Ok(Self {
            info: info.clone(),
            cwd_generation: 0,
            previous_cwd: None,
            pending_cwd_switch_reminder: None,
            cwd_switch_bookkeeping_generation: 0,
            session_summary: String::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            num_messages: 0,
            num_chat_messages: 0,
            current_model_id: model_id,
            catalog_identity: None,
            parent_session_id: None,
            forked_at: None,
            collection_id: None,
            next_trace_turn: 0,
            chat_format_version: CHAT_FORMAT_VERSION,
            prompt_display_cwd: None,
            session_kind: None,
            fork_context_source: None,
            fork_parent_prompt_id: None,
            inherited_prefix_len: None,
            hidden: None,
            source_workspace_dir: None,
            git_root_dir: git_metadata.git_root_dir,
            git_remotes: git_metadata.git_remotes,
            head_commit: git_metadata.head_commit,
            head_branch: git_metadata.head_branch,
            request_id: None,
            grok_home: grok_home_string(),
            last_active_at: None,
            generated_title: None,
            title_is_manual: false,
            worktree_label: crate::session::worktree::lookup_worktree_label(&info.cwd),
            agent_name: None,
            sandbox_profile: None,
            reasoning_effort: None,
            last_turn_summary: None,
            last_turn_summary_prompt_id: None,
        })
    }

    /// Whether this session should be excluded from history listings.
    pub fn is_hidden(&self) -> bool {
        self.hidden.unwrap_or(
            self.session_kind
                .as_deref()
                .is_some_and(|k| k.starts_with("subagent")),
        )
    }

    /// Preferred display title: `generated_title` if non-empty, else `session_summary`.
    pub fn display_title(&self) -> &str {
        self.generated_title
            .as_deref()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .unwrap_or(&self.session_summary)
    }

    /// [`Self::display_title`] as an `Option`, `None` when blank.
    pub fn display_title_opt(&self) -> Option<String> {
        let title = self.display_title().trim();
        (!title.is_empty()).then(|| title.to_string())
    }

    /// The manually-`/rename`d title (trimmed), `None` for auto-generated or
    /// blank titles. Binds to `generated_title` — the field `title_is_manual`
    /// describes — never the `session_summary` display fallback, so a stale
    /// flag over a blank manual title can't relabel an auto summary as
    /// manual. When `Some`, it equals [`Self::display_title_opt`] (a
    /// non-blank `generated_title` wins the display chain).
    pub fn manual_title_opt(&self) -> Option<String> {
        self.title_is_manual
            .then_some(self.generated_title.as_deref())
            .flatten()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    }

    /// Last-change time (unix millis): `last_active_at`, else `updated_at`.
    pub fn last_change_unix_ms(&self) -> i64 {
        self.last_active_at
            .unwrap_or(self.updated_at)
            .timestamp_millis()
    }
}

#[cfg(test)]
mod is_hidden_tests {
    use super::*;

    fn summary_with_kind(kind: Option<&str>) -> Summary {
        Summary {
            session_kind: kind.map(String::from),
            hidden: None,
            ..Summary::new(
                &Info {
                    id: acp::SessionId::new("test"),
                    cwd: "/tmp".into(),
                },
                default_model_id(),
            )
            .unwrap()
        }
    }

    #[test]
    fn summary_round_trips_and_defaults_reasoning_effort() {
        let mut s = summary_with_kind(None);
        s.reasoning_effort = None;
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            !json.contains("reasoning_effort"),
            "a None effort must not be serialized"
        );
        let back: Summary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reasoning_effort, None);

        s.reasoning_effort = Some(ReasoningEffort::Xhigh);
        let json = serde_json::to_string(&s).unwrap();
        let back: Summary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.reasoning_effort, Some(ReasoningEffort::Xhigh));
    }

    #[test]
    fn hidden_for_all_subagent_kinds() {
        for kind in ["subagent", "subagent_fork", "subagent_resume"] {
            assert!(
                summary_with_kind(Some(kind)).is_hidden(),
                "{kind} should be hidden"
            );
        }
    }

    #[test]
    fn not_hidden_for_regular_sessions() {
        assert!(!summary_with_kind(None).is_hidden());
        assert!(!summary_with_kind(Some("fork")).is_hidden());
        assert!(!summary_with_kind(Some("worktree")).is_hidden());
    }

    #[test]
    fn explicit_hidden_overrides_session_kind() {
        let mut s = summary_with_kind(Some("subagent"));
        s.hidden = Some(false);
        assert!(!s.is_hidden(), "explicit hidden=false overrides kind");

        let mut s = summary_with_kind(None);
        s.hidden = Some(true);
        assert!(s.is_hidden(), "explicit hidden=true overrides kind");
    }
}

#[cfg(test)]
mod head_fields_tests {
    use super::*;

    #[test]
    fn summary_round_trips_head_fields_through_json() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.head_commit = Some("abc123def456".into());
        summary.head_branch = Some("main".into());

        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: Summary = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.head_commit.as_deref(), Some("abc123def456"));
        assert_eq!(deserialized.head_branch.as_deref(), Some("main"));
    }

    #[test]
    fn summary_deserializes_without_head_fields_backward_compat() {
        // Simulate an old summary.json that lacks head_commit/head_branch.
        let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "num_chat_messages": 0,
            "current_model_id": "test-model"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert!(summary.head_commit.is_none());
        assert!(summary.head_branch.is_none());
    }

    #[test]
    fn summary_relocation_metadata_is_backward_compatible() {
        let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "num_chat_messages": 0,
            "current_model_id": "test-model"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.cwd_generation, 0);
        assert!(summary.previous_cwd.is_none());
        assert!(summary.pending_cwd_switch_reminder.is_none());
        assert_eq!(summary.cwd_switch_bookkeeping_generation, 0);

        let serialized = serde_json::to_value(summary).unwrap();
        for field in [
            "cwd_generation",
            "previous_cwd",
            "pending_cwd_switch_reminder",
            "cwd_switch_bookkeeping_generation",
        ] {
            assert!(serialized.get(field).is_none());
        }
    }

    #[test]
    fn summary_relocation_metadata_round_trips() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/new".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.cwd_generation = 2;
        summary.previous_cwd = Some("/old".into());
        summary.pending_cwd_switch_reminder = Some(PendingCwdSwitchReminder {
            cwd_generation: 2,
            previous_cwd: "/old".into(),
            destination_cwd: "/new".into(),
            content: "moved".into(),
            destination_project_instructions: Some("target rules".into()),
        });

        let serialized = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            serialized["pending_cwd_switch_reminder"]["destination_cwd"],
            "/new"
        );
        assert!(
            serialized["pending_cwd_switch_reminder"]
                .get("cwd")
                .is_none()
        );
        let back: Summary = serde_json::from_value(serialized).unwrap();
        assert_eq!(back.cwd_generation, 2);
        assert_eq!(back.previous_cwd.as_deref(), Some("/old"));
        assert_eq!(
            back.pending_cwd_switch_reminder,
            summary.pending_cwd_switch_reminder
        );
        assert_eq!(back.info.cwd, "/new");

        let pending: PendingCwdSwitchReminder = serde_json::from_value(serde_json::json!({
            "cwd_generation": 2,
            "previous_cwd": "/old",
            "cwd": "/new",
            "content": "moved"
        }))
        .unwrap();
        assert_eq!(pending.destination_cwd, "/new");
    }

    #[test]
    fn summary_skips_none_head_fields_in_serialized_json() {
        let summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        // In a non-git directory the fields will be None.
        // Verify they are omitted from the JSON output.
        let json = serde_json::to_string(&summary).unwrap();
        // head_commit should not appear if the cwd has a repo (it might),
        // but verify the skip_serializing_if attribute works for None.
        if summary.head_commit.is_none() {
            assert!(!json.contains("head_commit"));
        }
        if summary.head_branch.is_none() {
            assert!(!json.contains("head_branch"));
        }
    }
}

#[cfg(test)]
mod generated_title_tests {
    use super::*;

    #[test]
    fn summary_round_trips_generated_title_through_json() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.generated_title = Some("Refactor auth middleware".into());
        summary.worktree_label = Some("auth-refactor".into());

        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: Summary = serde_json::from_str(&json).unwrap();

        assert_eq!(
            deserialized.generated_title.as_deref(),
            Some("Refactor auth middleware")
        );
        assert_eq!(
            deserialized.worktree_label.as_deref(),
            Some("auth-refactor")
        );
    }

    #[test]
    fn summary_deserializes_without_new_fields_backward_compat() {
        let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "first prompt text",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 5,
            "num_chat_messages": 3,
            "current_model_id": "test-model"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert!(summary.generated_title.is_none());
        assert!(summary.worktree_label.is_none());
        assert_eq!(summary.session_summary, "first prompt text");
    }

    #[test]
    fn summary_skips_none_generated_title_in_json() {
        let summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("generated_title"));
        assert!(!json.contains("worktree_label"));
    }

    #[test]
    fn summary_includes_generated_title_when_set() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.generated_title = Some("Fix K8s deployment".into());
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("generated_title"));
        assert!(json.contains("Fix K8s deployment"));
    }

    #[test]
    fn summary_deserializes_with_all_fields_present() {
        let json = r#"{
            "info": { "id": "full-session", "cwd": "/tmp" },
            "session_summary": "first prompt",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 10,
            "num_chat_messages": 5,
            "current_model_id": "test-model",
            "head_branch": "feature/xyz",
            "git_root_dir": "/home/user/myrepo",
            "generated_title": "Implement XYZ feature",
            "worktree_label": "xyz-feature"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert_eq!(
            summary.generated_title.as_deref(),
            Some("Implement XYZ feature")
        );
        assert_eq!(summary.worktree_label.as_deref(), Some("xyz-feature"));
        assert_eq!(summary.head_branch.as_deref(), Some("feature/xyz"));
        assert_eq!(summary.git_root_dir.as_deref(), Some("/home/user/myrepo"));
    }

    // ── display_title direct tests ──────────────────────────────────────

    #[test]
    fn display_title_returns_generated_title_when_set() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.generated_title = Some("Refactor auth layer".into());
        assert_eq!(summary.display_title(), "Refactor auth layer");
    }

    #[test]
    fn display_title_falls_back_on_empty_generated_title() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.session_summary = "first prompt fallback".into();
        summary.generated_title = Some(String::new());
        assert_eq!(summary.display_title(), "first prompt fallback");
    }

    #[test]
    fn display_title_falls_back_on_none_generated_title() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.session_summary = "session summary fallback".into();
        summary.generated_title = None;
        assert_eq!(summary.display_title(), "session summary fallback");
    }

    // ── title_is_manual / manual_title_opt ──────────────────────────────

    #[test]
    fn title_is_manual_round_trips_through_json() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.generated_title = Some("Manual Title".into());
        summary.title_is_manual = true;

        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("title_is_manual"));
        let deserialized: Summary = serde_json::from_str(&json).unwrap();

        assert!(deserialized.title_is_manual);
        assert_eq!(
            deserialized.manual_title_opt().as_deref(),
            Some("Manual Title")
        );
    }

    #[test]
    fn title_is_manual_defaults_false_and_skips_when_unset() {
        // Old summary.json without the field: default false, so pre-existing
        // renames show no border title until renamed again.
        let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "first prompt text",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 5,
            "num_chat_messages": 3,
            "current_model_id": "test-model",
            "generated_title": "Old Rename"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert!(!summary.title_is_manual);
        assert!(summary.manual_title_opt().is_none());
        assert_eq!(summary.display_title_opt().as_deref(), Some("Old Rename"));

        // And false is omitted on write, keeping old files byte-stable.
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("title_is_manual"));
    }

    #[test]
    fn manual_title_opt_none_for_auto_generated_title() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.generated_title = Some("Auto Title".into());

        assert!(summary.manual_title_opt().is_none());
        assert_eq!(summary.display_title_opt().as_deref(), Some("Auto Title"));
    }

    /// A stale `title_is_manual` over a blank `generated_title` (e.g. written
    /// by an old client before the ext boundary rejected blank renames) must
    /// not relabel the `session_summary` display fallback as manual.
    #[test]
    fn manual_title_opt_ignores_stale_flag_over_blank_generated_title() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.session_summary = "auto first-prompt summary".into();
        summary.generated_title = Some("   ".into());
        summary.title_is_manual = true;

        assert!(summary.manual_title_opt().is_none());
        assert_eq!(
            summary.display_title_opt().as_deref(),
            Some("auto first-prompt summary")
        );
    }
}

#[derive(Clone)]
pub struct PersistenceHandle {
    pub tx: mpsc::UnboundedSender<PersistenceMsg>,
    noop: bool,
    disk_full_rx: watch::Receiver<bool>,
    fresh_publication: Option<FreshPublication>,
}

fn actor_channel(
    fresh_publication: Option<FreshPublication>,
) -> (
    PersistenceHandle,
    mpsc::UnboundedReceiver<PersistenceMsg>,
    mpsc::WeakUnboundedSender<PersistenceMsg>,
    watch::Sender<bool>,
) {
    let (tx, rx) = mpsc::unbounded_channel::<PersistenceMsg>();
    let (disk_full_tx, disk_full_rx) = watch::channel(false);
    let weak = tx.downgrade();
    let handle = PersistenceHandle {
        tx,
        noop: false,
        disk_full_rx,
        fresh_publication,
    };
    (handle, rx, weak, disk_full_tx)
}

#[derive(Debug)]
pub(crate) enum DurableAppendError {
    NotCommitted(io::Error),
    Committed(io::Error),
    AcknowledgementLost(io::Error),
}

impl std::fmt::Display for DurableAppendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(error)
            | Self::Committed(error)
            | Self::AcknowledgementLost(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for DurableAppendError {}

impl From<crate::session::storage::AppendUpdateError> for DurableAppendError {
    fn from(error: crate::session::storage::AppendUpdateError) -> Self {
        use crate::session::storage::AppendUpdateError;
        match error {
            AppendUpdateError::NotCommitted(error) => Self::NotCommitted(error),
            AppendUpdateError::Committed(error) => Self::Committed(error),
        }
    }
}

impl PersistenceHandle {
    #[cfg(test)]
    pub(crate) fn from_sender_for_test(tx: mpsc::UnboundedSender<PersistenceMsg>) -> Self {
        Self::from_parts_for_test(tx, watch::channel(false).1)
    }

    #[cfg(test)]
    pub(crate) fn from_parts_for_test(
        tx: mpsc::UnboundedSender<PersistenceMsg>,
        disk_full_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            tx,
            noop: false,
            disk_full_rx,
            fresh_publication: None,
        }
    }

    pub fn noop() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self {
            tx,
            noop: true,
            disk_full_rx: watch::channel(false).1,
            fresh_publication: None,
        }
    }

    pub fn is_noop(&self) -> bool {
        self.noop
    }

    #[cfg(test)]
    pub(crate) fn is_disk_full(&self) -> bool {
        *self.disk_full_rx.borrow()
    }

    pub(crate) fn subscribe_disk_full(&self) -> watch::Receiver<bool> {
        self.disk_full_rx.clone()
    }

    pub(crate) fn fresh_publication(&self) -> Option<FreshPublication> {
        self.fresh_publication.clone()
    }

    pub(crate) fn physical_session_dir(&self, info: &Info) -> PathBuf {
        self.fresh_publication
            .as_ref()
            .map(FreshPublication::physical_path)
            .unwrap_or_else(|| session_dir(info))
    }

    /// Abort a provisional fresh session, wait for gate-tracked session
    /// threads to exit, delete local storage, and wait until the persistence
    /// actor has stopped. No buffered state is flushed.
    pub(crate) fn request_abort_fresh_and_delete(
        tx: &mpsc::UnboundedSender<PersistenceMsg>,
        publication_gate: crate::session::SessionPublicationGate,
    ) -> io::Result<tokio::sync::oneshot::Receiver<io::Result<()>>> {
        // Wake an actor that has acknowledged PublishFresh and is deliberately
        // quiescent awaiting this gate before it can process the abort message.
        publication_gate.abort();
        let (respond_to, response) = tokio::sync::oneshot::channel();
        tx.send(PersistenceMsg::AbortFreshAndDelete {
            publication_gate,
            respond_to,
        })
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "session persistence actor stopped before provisional abort",
            )
        })?;
        Ok(response)
    }

    /// Abort a provisional fresh session, wait for gate-tracked session
    /// threads to exit, delete local storage, and wait until the persistence
    /// actor has stopped. No buffered state is flushed.
    pub(crate) async fn abort_fresh_and_delete(
        tx: &mpsc::UnboundedSender<PersistenceMsg>,
        publication_gate: crate::session::SessionPublicationGate,
    ) -> io::Result<()> {
        let response = Self::request_abort_fresh_and_delete(tx, publication_gate)?;
        response.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "session persistence actor stopped before provisional abort acknowledgement",
            )
        })?
    }

    /// Durably prepare and arm publication of a provisional fresh session.
    /// A successful acknowledgement is a strict barrier: pending state has been
    /// drained and all existing session files and directory entries have been
    /// synced. The id claim remains held until the gate becomes published.
    pub(crate) async fn publish_fresh(
        tx: &mpsc::UnboundedSender<PersistenceMsg>,
        publication_gate: crate::session::SessionPublicationGate,
    ) -> io::Result<()> {
        let (respond_to, response) = tokio::sync::oneshot::channel();
        tx.send(PersistenceMsg::PublishFresh {
            publication_gate,
            respond_to,
        })
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "session persistence actor stopped before provisional publication",
            )
        })?;
        response.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "session persistence actor stopped before provisional publication acknowledgement",
            )
        })?
    }

    /// Append after older buffered updates and wait for the durable barrier.
    ///
    /// [`DurableAppendError::NotCommitted`] is safe to retry; [`DurableAppendError::Committed`]
    /// means the replay line landed; [`DurableAppendError::AcknowledgementLost`] has unknown status.
    /// No-op handles return `Unsupported`.
    pub(crate) async fn append_update_durably(
        &self,
        update: SessionUpdate,
    ) -> Result<(), DurableAppendError> {
        if self.noop {
            return Err(DurableAppendError::NotCommitted(io::Error::new(
                io::ErrorKind::Unsupported,
                "durable session update append is unsupported by a no-op persistence handle",
            )));
        }
        let (respond_to, response) = tokio::sync::oneshot::channel();
        self.tx
            .send(PersistenceMsg::AppendUpdateDurablyAndAck { update, respond_to })
            .map_err(|_| {
                DurableAppendError::NotCommitted(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "session persistence actor stopped before durable append dispatch",
                ))
            })?;
        response
            .await
            .map_err(|_| {
                DurableAppendError::AcknowledgementLost(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "session persistence actor stopped before durable append acknowledgement",
                ))
            })?
            .map_err(DurableAppendError::from)
    }
}

enum PendingAppendOutcome {
    CommittedOk(acp::SessionNotification),
    CommittedErr(acp::SessionNotification, io::Error),
    NotCommittedErr(acp::SessionNotification, io::Error),
}

struct SessionPersistence {
    info: Info,
    storage: Arc<dyn StorageAdapter>,
    /// Shared visibility lease retained for the lifetime of a loaded actor so
    /// repair/delete/import exclusive operations cannot replace its files
    /// while it appends.
    _published_session_lease: Option<SessionIdLock>,
    /// Pending ACP notification for merging consecutive text chunks
    pending_notification: Option<acp::SessionNotification>,
    rx: mpsc::UnboundedReceiver<PersistenceMsg>,
    remote_sync: Option<RemoteSync>,
    /// True only for sessions created this run (not resumed); gates the
    /// writeback backfill so a resumed, already-synced session isn't re-sent.
    created_fresh: bool,
    /// Held from fresh storage initialization through checked publication or
    /// abort so another process cannot load provisional on-disk state.
    fresh_claim: Option<FreshSessionClaim>,
    /// Armed by the checked `PublishFresh` handshake. Terminal abort clears
    /// the waiter but deliberately retains `fresh_claim` for locked cleanup.
    pending_publication_gate: Option<crate::session::SessionPublicationGate>,
    /// Terminal abort is fail-closed: once observed, a later message cannot
    /// re-arm publication with a different gate while cleanup is pending.
    fresh_publication_aborted: bool,
    /// WebSocket-based relay sync for real-time session sharing.
    /// This streams updates to the relay backend in addition to local persistence.
    relay_sync: Option<crate::relay::RelaySync>,
    /// Session title generation lifecycle.
    summary: crate::session::summary::SummaryGenerator,
    registry_title_sync: Option<RegistryGeneratedTitleSync>,
    /// Client gateway for `SessionSummaryGenerated` notifications. Used to
    /// announce an auto-generated title only once it has actually been adopted
    /// (see the `GeneratedTitle` handler), so a title rejected for racing a
    /// manual `/rename` never reaches the client. `None` for the subagent
    /// variant, whose lifecycle notifications are handled by the coordinator.
    gateway: Option<GatewaySender>,
    disk_full_tx: watch::Sender<bool>,
    disk_full_notified: bool,
}

impl SessionPersistence {
    fn try_merge_text(prev: &mut acp::ContentBlock, new: &acp::ContentBlock) -> bool {
        match (prev, new) {
            (acp::ContentBlock::Text(prev_text), acp::ContentBlock::Text(new_text))
                if prev_text.annotations.is_none()
                    && prev_text.meta.is_none()
                    && new_text.annotations.is_none()
                    && new_text.meta.is_none() =>
            {
                prev_text.text.push_str(&new_text.text);
                true
            }
            _ => false,
        }
    }

    // Empty chunks are chunks that have no content and no meta.
    fn is_empty_chunk(update: &acp::SessionUpdate) -> bool {
        match update {
            acp::SessionUpdate::AgentMessageChunk(chunk)
            | acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                let empty_text =
                    matches!(&chunk.content, acp::ContentBlock::Text(t) if t.text.is_empty());
                let no_meta = chunk.meta.is_none();
                empty_text && no_meta
            }
            _ => false,
        }
    }

    /// Attempt to merge consecutive ACP text notifications to reduce storage writes.
    /// Returns Some(notification) if the pending notification should be written now.
    fn maybe_merge_notification(
        &mut self,
        incoming: &acp::SessionNotification,
    ) -> Option<acp::SessionNotification> {
        // Always skip empty chunks - don't store them at all
        if Self::is_empty_chunk(&incoming.update) {
            return None;
        }

        let Some(pending) = self.pending_notification.take() else {
            self.pending_notification = Some(incoming.clone());
            return None;
        };

        let pending_update = pending.update.clone();
        match (&incoming.update, pending_update) {
            (
                acp::SessionUpdate::AgentMessageChunk(new_chunk),
                acp::SessionUpdate::AgentMessageChunk(mut pending_chunk),
            )
            | (
                acp::SessionUpdate::AgentThoughtChunk(new_chunk),
                acp::SessionUpdate::AgentThoughtChunk(mut pending_chunk),
            ) => {
                let did_merge = pending_chunk.meta.is_none()
                    && new_chunk.meta.is_none()
                    && Self::try_merge_text(&mut pending_chunk.content, &new_chunk.content);

                if did_merge {
                    let merged_update = match &incoming.update {
                        acp::SessionUpdate::AgentMessageChunk(_) => {
                            acp::SessionUpdate::AgentMessageChunk(pending_chunk)
                        }
                        acp::SessionUpdate::AgentThoughtChunk(_) => {
                            acp::SessionUpdate::AgentThoughtChunk(pending_chunk)
                        }
                        _ => unreachable!(),
                    };
                    self.pending_notification = Some(
                        acp::SessionNotification::new(incoming.session_id.clone(), merged_update)
                            .meta(incoming.meta.clone()),
                    );
                    None
                } else {
                    self.pending_notification = Some(incoming.clone());
                    Some(pending)
                }
            }
            _ => {
                self.pending_notification = Some(incoming.clone());
                Some(pending)
            }
        }
    }

    async fn write_update(
        &mut self,
        update: &SessionUpdate,
    ) -> Result<(), crate::session::storage::AppendUpdateError> {
        let result = self
            .storage
            .append_update_commit_aware(&self.info, update)
            .await;
        self.observe_append_update(&result);
        result
    }

    fn observe_io<T>(&mut self, result: &io::Result<T>) {
        match result {
            Ok(_) => self.clear_disk_full(),
            Err(error) if is_disk_full_io_error(error) => self.mark_disk_full(),
            Err(_) => {}
        }
    }

    fn observe_append_update(
        &mut self,
        result: &Result<(), crate::session::storage::AppendUpdateError>,
    ) {
        match result {
            Ok(()) => self.clear_disk_full(),
            Err(
                crate::session::storage::AppendUpdateError::NotCommitted(error)
                | crate::session::storage::AppendUpdateError::Committed(error),
            ) if is_disk_full_io_error(error) => self.mark_disk_full(),
            Err(_) => {}
        }
    }

    fn mark_disk_full(&mut self) {
        if !*self.disk_full_tx.borrow() {
            let _ = self.disk_full_tx.send(true);
        }
        if self.disk_full_notified {
            return;
        }
        self.disk_full_notified = true;
        self.emit_disk_full_notification();
    }

    fn clear_disk_full(&mut self) {
        if *self.disk_full_tx.borrow() {
            let _ = self.disk_full_tx.send(false);
        }
        self.disk_full_notified = false;
    }

    fn emit_disk_full_notification(&self) {
        let Some(gateway) = &self.gateway else {
            return;
        };
        let notification = XaiSessionNotification {
            session_id: self.info.id.clone(),
            update: XaiSessionUpdate::RetryState(RetryState::Failed {
                error_type: DISK_FULL_ERROR_TYPE.to_string(),
                message: DISK_FULL_USER_MESSAGE.to_string(),
            }),
            meta: None,
        };
        if let Ok(params) = serde_json::value::to_raw_value(&notification) {
            gateway.forward_fire_and_forget(acp::ExtNotification::new(
                "x.ai/session_notification",
                params.into(),
            ));
        }
    }

    async fn probe_writable(&self) -> io::Result<()> {
        let dir = self
            .storage
            .updates_file_path(&self.info)
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "session directory is unknown; cannot probe disk space",
                )
            })?;
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            let probe = dir.join(".disk_ok");
            std::fs::write(&probe, b"ok")?;
            let _ = std::fs::remove_file(&probe);
            io::Result::Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    fn queue_acp_sync(&self, notification: acp::SessionNotification) {
        if let Some(sync) = &self.remote_sync {
            sync.queue(notification.clone());
        }
        if let Some(relay) = &self.relay_sync {
            relay.queue(notification);
        }
    }

    /// Enable writeback for a session created `Local` before settings resolved:
    /// build the sync and (for a fresh session) backfill its local-only history.
    /// No-op once syncing, so a repeat upgrade is harmless.
    async fn upgrade_to_writeback(&mut self, auth_manager: Arc<crate::auth::AuthManager>) {
        if self.remote_sync.is_some() {
            return;
        }
        // Flush the merge-pending notification so the backfill re-reads it.
        let _ = self.flush_pending().await;
        let persisted = match self.storage.load_session(&self.info).await {
            Ok(persisted) => persisted,
            Err(error) => {
                tracing::warn!(%error, "writeback upgrade: failed to load session for backfill");
                return;
            }
        };
        let remote_sync = match init_remote_sync(
            &persisted.summary,
            StorageMode::Writeback,
            Some(auth_manager),
        ) {
            Ok(Some(remote_sync)) => remote_sync,
            // ZDR team, or nothing to do: leave the session local-only.
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "writeback upgrade: remote sync init failed");
                return;
            }
        };
        // Fresh-only backfill; see `backfill_updates_to_sync`.
        let backfilled =
            backfill_updates_to_sync(self.created_fresh, persisted.updates, &remote_sync);
        if self.created_fresh {
            tracing::info!(
                session_id = %self.info.id,
                backfilled,
                "writeback enabled after settings arrival; backfilled local-only history",
            );
        } else {
            tracing::info!(
                session_id = %self.info.id,
                "writeback enabled for resumed session; forward-only, no backfill",
            );
        }
        self.remote_sync = Some(remote_sync);
    }

    fn finish_pending_append(
        notification: acp::SessionNotification,
        result: Result<(), crate::session::storage::AppendUpdateError>,
    ) -> PendingAppendOutcome {
        match result {
            Ok(()) => PendingAppendOutcome::CommittedOk(notification),
            Err(crate::session::storage::AppendUpdateError::NotCommitted(error)) => {
                PendingAppendOutcome::NotCommittedErr(notification, error)
            }
            Err(crate::session::storage::AppendUpdateError::Committed(error)) => {
                PendingAppendOutcome::CommittedErr(notification, error)
            }
        }
    }

    /// Restore uncommitted failures; sync committed records before returning errors.
    async fn drain_pending(&mut self) -> Result<(), crate::session::storage::AppendUpdateError> {
        if let Some(notification) = self.pending_notification.take() {
            let result = self
                .write_update(&SessionUpdate::Acp(Box::new(notification.clone())))
                .await;
            match Self::finish_pending_append(notification, result) {
                PendingAppendOutcome::CommittedOk(notification) => {
                    self.queue_acp_sync(notification);
                }
                PendingAppendOutcome::CommittedErr(notification, error) => {
                    self.queue_acp_sync(notification);
                    return Err(crate::session::storage::AppendUpdateError::Committed(error));
                }
                PendingAppendOutcome::NotCommittedErr(notification, error) => {
                    self.pending_notification = Some(notification);
                    return Err(crate::session::storage::AppendUpdateError::NotCommitted(
                        error,
                    ));
                }
            }
        }
        Ok(())
    }

    async fn handle_durable_append(
        &mut self,
        update: SessionUpdate,
    ) -> Result<(), crate::session::storage::AppendUpdateError> {
        self.drain_pending().await?;
        let result = self
            .storage
            .append_update_durable_commit_aware(&self.info, &update)
            .await;
        self.observe_append_update(&result);
        match (&update, &result) {
            (SessionUpdate::Acp(notification), Ok(()))
            | (
                SessionUpdate::Acp(notification),
                Err(crate::session::storage::AppendUpdateError::Committed(_)),
            ) => self.queue_acp_sync((**notification).clone()),
            _ => {}
        }
        result
    }

    /// Flush any pending merged ACP notification to disk and remote sync.
    /// A no-op drain must not clear the disk-full latch.
    async fn flush_pending(&mut self) -> io::Result<()> {
        let result = self
            .drain_pending()
            .await
            .map_err(crate::session::storage::AppendUpdateError::into_io_error);
        if let Err(error) = &result {
            tracing::warn!(%error, "failed to write pending update");
        }
        if let Some(sync) = &self.remote_sync {
            sync.flush();
        }
        if let Some(relay) = &self.relay_sync {
            relay.flush();
        }
        result
    }

    /// Flush pending writes and sync all session files to disk.
    /// Called before CopyFile to ensure all data is persisted.
    async fn flush_and_sync(&mut self) {
        let _ = self.flush_pending().await;
        if let Err(e) = self.storage.sync_session_files(&self.info).await {
            tracing::warn!(?e, "Failed to sync session files to disk");
        }
    }

    /// Once the publication gate aborts, the actor may only consume the
    /// matching cleanup request. Every earlier queued mutation is discarded so
    /// provisional storage and sync backends cannot observe post-seal work.
    async fn await_fresh_abort_cleanup(&mut self) {
        self.fresh_publication_aborted = true;
        self.pending_notification = None;
        self.remote_sync = None;
        self.relay_sync = None;

        while let Some(msg) = self.rx.recv().await {
            let PersistenceMsg::AbortFreshAndDelete {
                publication_gate,
                respond_to,
            } = msg
            else {
                continue;
            };

            publication_gate.abort();
            publication_gate.wait_until_session_threads_exit().await;
            let is_provisional = self.created_fresh
                && self
                    .fresh_claim
                    .as_ref()
                    .is_some_and(|claim| !claim.publication.is_committed());
            if self.created_fresh
                && self
                    .fresh_claim
                    .as_ref()
                    .is_some_and(|claim| claim.publication.is_committed())
                && let Some(claim) = self.fresh_claim.take()
            {
                match claim.into_published_lease() {
                    Ok(lease) => self._published_session_lease = Some(lease),
                    Err(failure) => {
                        tracing::error!(
                            session_id = %self.info.id,
                            error = %failure.error,
                            "failed retaining committed session lease after terminal abort"
                        );
                        self.fresh_claim = Some(failure.claim);
                    }
                }
            }
            let result = if is_provisional {
                self.storage.delete_session(&self.info).await
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to delete a published or resumed session through provisional abort",
                ))
            };
            if result.is_ok() {
                crate::session::storage::search::notify_session_updated(
                    &self.info.id.to_string(),
                    &self.info.cwd,
                );
            }
            let _ = respond_to.send(result);
            return;
        }
    }

    async fn run(mut self) {
        // Persistence traffic counts as worktree activity; debounced so
        // long-resident sessions (leader/remote, active for days without a
        // re-open) stay out of gc expiry without per-message DB writes.
        // The constructors fire the t=0 touch, so this starts at now().
        let mut last_worktree_touch = std::time::Instant::now();
        loop {
            // Once PublishFresh is acknowledged, no later persistence message
            // may mutate the staged tree before the synchronous namespace
            // commit and path rebind. Abort wakes this wait by aborting gate.
            if let Some(publication_gate) = self.pending_publication_gate.take() {
                let published = publication_gate.wait_until_published().await;
                if published {
                    if let Some(claim) = self.fresh_claim.take() {
                        if !claim.publication.is_committed() {
                            tracing::error!(
                                session_id = %self.info.id,
                                "fresh publication gate completed before durable storage publication"
                            );
                            drop(claim);
                            self.fresh_publication_aborted = true;
                            return;
                        } else {
                            match claim.into_published_lease() {
                                Ok(lease) => self._published_session_lease = Some(lease),
                                Err(failure) => {
                                    tracing::error!(
                                        session_id = %self.info.id,
                                        error = %failure.error,
                                        "failed downgrading fresh publication lease; retaining exclusive namespace lease"
                                    );
                                    self.fresh_claim = Some(failure.claim);
                                }
                            }
                            spawn_worktree_touch(&self.info);
                        }
                    } else {
                        tracing::error!(
                            session_id = %self.info.id,
                            "fresh publication gate completed without an id claim"
                        );
                    }
                } else {
                    self.await_fresh_abort_cleanup().await;
                    return;
                }
                continue;
            }
            let Some(msg) = self.rx.recv().await else {
                break;
            };
            if self.fresh_claim.is_none()
                && last_worktree_touch.elapsed() >= WORKTREE_TOUCH_INTERVAL
            {
                last_worktree_touch = std::time::Instant::now();
                // Detached on purpose: opportunistic refresh, no ordering need.
                spawn_worktree_touch(&self.info);
            }
            match msg {
                PersistenceMsg::AbortFreshAndDelete {
                    publication_gate,
                    respond_to,
                } => {
                    let is_provisional = self.created_fresh
                        && self
                            .fresh_claim
                            .as_ref()
                            .is_some_and(|claim| !claim.publication.is_committed());
                    if self.created_fresh
                        && self
                            .fresh_claim
                            .as_ref()
                            .is_some_and(|claim| claim.publication.is_committed())
                        && let Some(claim) = self.fresh_claim.take()
                    {
                        match claim.into_published_lease() {
                            Ok(lease) => self._published_session_lease = Some(lease),
                            Err(failure) => {
                                tracing::error!(
                                    session_id = %self.info.id,
                                    error = %failure.error,
                                    "failed retaining committed session lease after rejected abort"
                                );
                                self.fresh_claim = Some(failure.claim);
                            }
                        }
                    }
                    if is_provisional {
                        self.fresh_publication_aborted = true;
                        publication_gate.abort();
                        publication_gate.wait_until_session_threads_exit().await;
                        self.pending_publication_gate = None;
                        self.pending_notification = None;
                        self.remote_sync = None;
                        self.relay_sync = None;
                    }
                    let result = if is_provisional {
                        self.storage.delete_session(&self.info).await
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "refusing to delete a published or resumed session through provisional abort",
                        ))
                    };
                    if result.is_ok() {
                        crate::session::storage::search::notify_session_updated(
                            &self.info.id.to_string(),
                            &self.info.cwd,
                        );
                    }
                    let _ = respond_to.send(result);
                    if is_provisional {
                        return;
                    }
                }
                PersistenceMsg::PublishFresh {
                    publication_gate,
                    respond_to,
                } => {
                    let eligibility = if !self.created_fresh
                        || self.fresh_claim.is_none()
                        || self.fresh_publication_aborted
                    {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "fresh session is published, aborted, or has no publication claim",
                        ))
                    } else if self.pending_publication_gate.is_some() {
                        Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "fresh session publication is already armed",
                        ))
                    } else {
                        Ok(())
                    };
                    let result = match eligibility {
                        Ok(()) => match self.flush_pending().await {
                            Ok(()) => self.storage.sync_session_files(&self.info).await,
                            Err(error) => Err(error),
                        },
                        Err(error) => Err(error),
                    };
                    if result.is_ok() {
                        self.pending_publication_gate = Some(publication_gate);
                    }
                    #[cfg(test)]
                    block_publish_fresh_ack_test_hook(&self.info.id.to_string()).await;
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::UpgradeToWriteback { auth_manager } => {
                    self.upgrade_to_writeback(auth_manager).await;
                }
                PersistenceMsg::Flush => {
                    let _ = self.flush_pending().await;
                }
                PersistenceMsg::FlushAndAck { respond_to } => {
                    let result = self.flush_pending().await;
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::ProbeWritable { respond_to } => {
                    let result = self.probe_writable().await;
                    self.observe_io(&result);
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::Update(update) => {
                    match update {
                        SessionUpdate::Acp(notification) => {
                            // ACP notifications use merging to coalesce consecutive text chunks
                            if let Some(to_write) = self.maybe_merge_notification(&notification) {
                                match self
                                    .write_update(&SessionUpdate::Acp(Box::new(to_write.clone())))
                                    .await
                                {
                                    Ok(())
                                    | Err(crate::session::storage::AppendUpdateError::Committed(
                                        _,
                                    )) => {
                                        self.queue_acp_sync(to_write);
                                    }
                                    Err(error) => tracing::warn!(%error, "failed to write update"),
                                }
                            }
                        }
                        SessionUpdate::Xai(_) => {
                            // xAI notifications are written directly without merging
                            if let Err(error) = self.write_update(&update).await {
                                tracing::warn!(%error, "failed to write update");
                            }
                        }
                    }
                }
                PersistenceMsg::AppendUpdateDurablyAndAck { update, respond_to } => {
                    let result = self.handle_durable_append(update).await;
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::Chat(chat_msg) => {
                    let result = self
                        .storage
                        .append_chat_message(&self.info, &chat_msg)
                        .await;
                    self.observe_io(&result);
                    if let Err(e) = result {
                        tracing::warn!(?e, "failed to write chat message");
                    }
                }
                PersistenceMsg::AppendCwdSwitchAndAck { item, respond_to } => {
                    let result = self
                        .storage
                        .append_cwd_switch_commit_aware(&self.info, &item)
                        .await
                        .map_err(|error| match error {
                            crate::session::storage::AppendCwdSwitchError::NotCommitted(error) => {
                                xai_chat_state::StrictAppendError::NotCommitted(error)
                            }
                            crate::session::storage::AppendCwdSwitchError::Committed {
                                acknowledgement,
                                source,
                            } => xai_chat_state::StrictAppendError::Committed {
                                acknowledgement,
                                source,
                            },
                        });
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::ReplaceChatHistory(messages) => {
                    tracing::info!(
                        num_messages = messages.len(),
                        "Replacing chat history (compaction)"
                    );
                    let result = self
                        .storage
                        .replace_chat_history(&self.info, &messages)
                        .await;
                    self.observe_io(&result);
                    if let Err(e) = result {
                        tracing::warn!(?e, "failed to replace chat history");
                    }
                }
                PersistenceMsg::ReplaceChatHistoryAndAck {
                    messages,
                    respond_to,
                } => {
                    let result = self
                        .storage
                        .replace_chat_history(&self.info, &messages)
                        .await;
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::CurrentModel {
                    model_id,
                    catalog_identity,
                    agent_name,
                    reasoning_effort,
                } => {
                    if let Err(e) = self
                        .storage
                        .update_current_model_identity_and_agent(
                            &self.info,
                            &model_id,
                            catalog_identity.as_ref(),
                            agent_name.as_deref(),
                            reasoning_effort,
                        )
                        .await
                    {
                        tracing::warn!(?e, "failed to update current model");
                    }
                    if let Some(sync) = &self.remote_sync {
                        sync.set_model(model_id.0.to_string(), catalog_identity, agent_name);
                    }
                }
                PersistenceMsg::CurrentModelAndAck {
                    model_id,
                    catalog_identity,
                    agent_name,
                    reasoning_effort,
                    respond_to,
                } => {
                    let result = self
                        .storage
                        .update_current_model_identity_and_agent(
                            &self.info,
                            &model_id,
                            catalog_identity.as_ref(),
                            agent_name.as_deref(),
                            reasoning_effort,
                        )
                        .await;
                    if result.is_ok()
                        && let Some(sync) = &self.remote_sync
                    {
                        sync.set_model(model_id.0.to_string(), catalog_identity, agent_name);
                    }
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::ModelSwitchAndAck {
                    messages,
                    model_id,
                    catalog_identity,
                    agent_name,
                    reasoning_effort,
                    summary_sampling_config,
                    respond_to,
                } => {
                    let result = self
                        .storage
                        .commit_model_switch_with_identity(
                            &self.info,
                            &messages,
                            &model_id,
                            catalog_identity.as_ref(),
                            agent_name.as_deref(),
                            reasoning_effort,
                        )
                        .await;
                    if result.is_ok() || result.as_ref().is_err_and(|error| error.is_committed()) {
                        if let Some(sync) = &self.remote_sync {
                            sync.set_model(model_id.0.to_string(), catalog_identity, agent_name);
                        }
                        if let Some(config) = summary_sampling_config {
                            let model = config.model.clone();
                            match crate::sampling::Client::new(config) {
                                Ok(client) => self.summary.replace_sampling_client(client, model),
                                Err(error) => tracing::warn!(
                                    %error,
                                    "failed to replace inherited session summary sampling config"
                                ),
                            }
                        }
                    }
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::PlanState(state) => {
                    if let Err(e) = self.storage.write_plan_state(&self.info, &state).await {
                        tracing::warn!(?e, "failed to write plan state");
                    }
                }
                PersistenceMsg::PlanModeState(state) => {
                    if let Err(e) = self.storage.write_plan_mode_state(&self.info, &state).await {
                        tracing::warn!(?e, "failed to write plan mode state");
                    }
                }
                PersistenceMsg::GoalModeState(state) => {
                    if let Err(e) = self.storage.write_goal_mode_state(&self.info, &state).await {
                        tracing::warn!(?e, "failed to write goal mode state");
                    }
                }
                PersistenceMsg::DeleteGoalModeState { respond_to } => {
                    let result = self.storage.delete_goal_mode_state(&self.info).await;
                    if let Err(e) = &result {
                        tracing::warn!(?e, "failed to delete goal mode state");
                    }
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::WorkflowRunState(manifest) => {
                    if let Err(error) = self
                        .storage
                        .write_workflow_run_state(&self.info, &manifest)
                        .await
                    {
                        tracing::warn!(run_id = %manifest.state.run_id, ?error, "failed to write workflow run state");
                    }
                }
                PersistenceMsg::WorkflowRunStateAndAck {
                    manifest,
                    respond_to,
                } => {
                    let result = self
                        .storage
                        .write_workflow_run_state(&self.info, &manifest)
                        .await;
                    if let Err(error) = &result {
                        tracing::warn!(run_id = %manifest.state.run_id, ?error, "failed to write acknowledged workflow run state");
                    }
                    let _ = respond_to.send(result);
                }
                PersistenceMsg::DeleteWorkflowRunState(run_id) => {
                    if let Err(e) = self
                        .storage
                        .delete_workflow_run_state(&self.info, &run_id)
                        .await
                    {
                        tracing::warn!(%run_id, ?e, "failed to delete workflow run state");
                    }
                }
                PersistenceMsg::ContentChunk(content_chunks) => {
                    let content_part = content_chunks
                        .content_chunks
                        .into_iter()
                        .filter_map(|content_chunk| match content_chunk {
                            acp::ContentBlock::Text(text) => Some(text.text),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.summary.update(content_part);

                    // Notify session search index so this turn becomes searchable
                    crate::session::storage::search::notify_session_updated(
                        &self.info.id.to_string(),
                        &self.info.cwd,
                    );
                }
                PersistenceMsg::ReplaceSummarySamplingConfig(config) => {
                    let model = config.model.clone();
                    match crate::sampling::Client::new(config) {
                        Ok(client) => self.summary.replace_sampling_client(client, model),
                        Err(error) => {
                            tracing::warn!(%error, "failed to replace session summary sampling config")
                        }
                    }
                }
                PersistenceMsg::GeneratedTitle(title) => {
                    // Auto-generated titles must never overwrite a title the
                    // user set via `/rename`. `set_generated_title_if_absent`
                    // writes only when the session still has no title (checked
                    // atomically under the summary lock) and reports whether it
                    // did, so a manual rename that raced this generation wins
                    // and its title is not clobbered locally or on remotes.
                    match self
                        .storage
                        .set_generated_title_if_absent(&self.info, title.clone())
                        .await
                    {
                        Ok(true) => {
                            // Announce to clients only now that the title is
                            // adopted, so a title rejected for racing a manual
                            // `/rename` never overwrites the client's title.
                            crate::session::summary::notify_client(
                                &self.gateway,
                                &self.info,
                                &title,
                            );
                            if let Some(sync) = &self.remote_sync {
                                sync.set_title(title.clone());
                            }
                            if let Some(reg) = self.registry_title_sync.as_ref()
                                && !reg.suppress_for_zdr
                            {
                                let client = reg.client.clone();
                                let sid = self.info.id.to_string();
                                let t = title;
                                tokio::spawn(async move {
                                    let req =
                                        crate::agent::session_registry_client::UpdateRequest {
                                            summary: Some(t),
                                            first_prompt: None,
                                            last_turn_number: None,
                                            repo_head_at_end: None,
                                            restorable_turn_number: None,
                                        };
                                    if let Err(e) = client.update(&sid, &req).await {
                                        tracing::warn!(
                                            error = %e,
                                            session_id = %sid,
                                            "session registry summary sync failed after title generation"
                                        );
                                    }
                                });
                            }
                        }
                        Ok(false) => {
                            tracing::debug!(
                                "skipped auto-generated title; session already has a title"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(?e, "failed to persist generated session title");
                        }
                    }
                }
                PersistenceMsg::LastTurnSummary(summary) => {
                    if let Err(e) = self
                        .storage
                        .set_last_turn_summary(&self.info, summary)
                        .await
                    {
                        tracing::warn!(?e, "failed to persist last turn summary");
                    }
                }
                PersistenceMsg::RewindPoint(point) => {
                    let result = self.storage.append_rewind_point(&self.info, &point).await;
                    self.observe_io(&result);
                    if let Err(e) = result {
                        tracing::warn!(?e, "failed to write rewind point");
                    }
                }
                PersistenceMsg::TruncateRewindPoints { from_index } => {
                    if let Err(e) = self
                        .storage
                        .truncate_rewind_points_from(&self.info, from_index)
                        .await
                    {
                        tracing::warn!(?e, from_index, "failed to truncate rewind points");
                    }
                }
                PersistenceMsg::MergeRewindPointsFrom { target_index } => {
                    if let Err(e) = self
                        .storage
                        .merge_rewind_points_from(&self.info, target_index)
                        .await
                    {
                        tracing::warn!(?e, target_index, "failed to merge rewind points");
                    }
                }
                PersistenceMsg::CollectionId(collection_id) => {
                    if let Err(e) = self
                        .storage
                        .update_collection_id(&self.info, &collection_id)
                        .await
                    {
                        tracing::warn!(?e, "failed to write collection id");
                    }
                }
                PersistenceMsg::NextTraceTurn {
                    next_trace_turn,
                    request_id,
                } => {
                    if let Err(e) = self
                        .storage
                        .update_next_trace_turn(&self.info, next_trace_turn, request_id.as_deref())
                        .await
                    {
                        tracing::warn!(?e, "failed to write next trace turn");
                    }
                }
                PersistenceMsg::Signals(signals) => {
                    if let Err(e) = self.storage.write_signals(&self.info, &signals).await {
                        tracing::warn!(?e, "failed to write session signals");
                    }
                }
                PersistenceMsg::AnnouncementState(state) => {
                    if let Err(e) = self
                        .storage
                        .write_announcement_state(&self.info, &state)
                        .await
                    {
                        tracing::warn!(?e, "failed to write announcement state");
                    }
                }
                PersistenceMsg::Feedback(entry) => {
                    if let Err(e) = self.storage.append_feedback(&self.info, &entry).await {
                        tracing::warn!(?e, "failed to write feedback entry");
                    }
                }
                PersistenceMsg::Btw(entry) => {
                    if let Err(e) = self.storage.append_btw(&self.info, &entry).await {
                        tracing::warn!(?e, "failed to write btw entry");
                    }
                }
                PersistenceMsg::GitHead { commit, branch } => {
                    if let Err(e) = self
                        .storage
                        .update_git_head(&self.info, commit, branch)
                        .await
                    {
                        tracing::warn!(?e, "failed to persist git HEAD");
                    }
                }
                PersistenceMsg::CompactionCheckpoint(checkpoint) => {
                    if let Err(e) = self
                        .storage
                        .write_compaction_checkpoint(&self.info, &checkpoint)
                        .await
                    {
                        tracing::warn!(?e, "failed to write compaction checkpoint file");
                    }
                }
                PersistenceMsg::CompactionRequest(request) => {
                    if let Err(e) = self
                        .storage
                        .write_compaction_request(&self.info, &request)
                        .await
                    {
                        tracing::warn!(?e, "failed to write compaction request artifact");
                    }
                }
                PersistenceMsg::RecapRequest(request) => {
                    if let Err(e) = self.storage.write_recap_request(&self.info, &request).await {
                        tracing::warn!(?e, "failed to write recap request artifact");
                    }
                }
                PersistenceMsg::CompactionSegment(segment) => {
                    if let Err(e) = self
                        .storage
                        .write_compaction_segment(&self.info, &segment)
                        .await
                    {
                        tracing::warn!(?e, "failed to write compaction segment");
                    }
                }
                PersistenceMsg::CopyFile { one_shot } => {
                    // Flush pending writes and sync all session files to disk before copying.
                    self.flush_and_sync().await;

                    let result = self.copy_session_dir_to_memory().await;
                    let _ = one_shot.send(result);
                }
            }
        }

        // Drain the merge buffer on channel close.
        let _ = self.flush_pending().await;
    }

    async fn copy_session_dir_to_memory(&self) -> anyhow::Result<SessionStateCopy> {
        let session_dir = session_dir(&self.info);
        tokio::task::spawn_blocking(move || {
            let mut files = Vec::new();

            if !session_dir.exists() {
                return Ok(SessionStateCopy { files });
            }

            collect_session_files_recursive(&session_dir, &session_dir, &mut files);
            collect_mcp_stderr_logs(&mut files);

            Ok(SessionStateCopy { files })
        })
        .await?
    }
}

/// Collect MCP server stderr logs from `~/.grok/logs/mcp/` for inclusion in the session archive.
fn collect_mcp_stderr_logs(files: &mut Vec<CopiedSessionFile>) {
    let mcp_log_dir = xai_grok_config::grok_home().join("logs").join("mcp");
    let Ok(entries) = std::fs::read_dir(&mcp_log_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path.extension().is_some_and(|ext| ext == "log")
            && let Ok(data) = std::fs::read(&path)
            && !data.is_empty()
        {
            let name = format!(
                "mcp_stderr/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            files.push(CopiedSessionFile { name, data });
        }
    }
}

/// Recursively collect all files from `dir` into `files`, using paths relative to `base`.
/// This captures subdirectories like `prompts/` which contain large-prompt files
/// referenced by truncated chat history entries.
fn collect_session_files_recursive(base: &Path, dir: &Path, files: &mut Vec<CopiedSessionFile>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(?dir, ?e, "Failed to read directory during session copy");
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let rel_path = match path.strip_prefix(base) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let Some(name) = rel_path.to_str() else {
                continue;
            };
            let data = match std::fs::read(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(?e, "Failed to read session file during copy");
                    continue;
                }
            };
            files.push(CopiedSessionFile {
                name: name.to_string(),
                data,
            });
        } else if path.is_dir() {
            collect_session_files_recursive(base, &path, files);
        }
    }
}

/// Queue a fresh session's local-only ACP history to `remote_sync` (xAI updates
/// are never synced), returning the count. Resumed sessions are forward-only:
/// their prior history may already be on the backend (which appends by content,
/// no per-message id), so re-sending would duplicate.
fn backfill_updates_to_sync(
    created_fresh: bool,
    updates: Vec<SessionUpdate>,
    remote_sync: &RemoteSync,
) -> usize {
    if !created_fresh {
        return 0;
    }
    let mut backfilled = 0usize;
    for update in updates {
        if let SessionUpdate::Acp(notification) = update {
            remote_sync.queue(*notification);
            backfilled += 1;
        }
    }
    remote_sync.flush();
    backfilled
}

fn init_remote_sync(
    summary: &Summary,
    storage_mode: StorageMode,
    auth_manager: Option<Arc<crate::auth::AuthManager>>,
) -> io::Result<Option<RemoteSync>> {
    match storage_mode {
        StorageMode::Local => Ok(None),
        StorageMode::Writeback => {
            let auth_manager = auth_manager.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    crate::auth::with_login_instruction(
                    |prog| {
                        format!("Writeback storage mode requires authentication. Run `{prog} login` first.")
                    },
                    "Writeback storage mode requires authentication. Sign in first.",
                ),
                )
            })?;
            if let Some(auth) = auth_manager.current_or_expired() {
                if auth.is_zdr_team() {
                    tracing::debug!("ZDR team: skipping remote sync");
                    return Ok(None);
                }
            } else {
                tracing::warn!(
                    "writeback: no auth loaded yet, ZDR check skipped (backend enforces server-side)"
                );
            }
            tracing::info!("Writeback mode enabled, syncing to backend");
            let client =
                crate::remote::BackendClient::new().with_auth_manager(auth_manager.clone());
            let metadata = ExportedMetadata::from_summary(summary);
            Ok(Some(RemoteSync::new(
                summary.info.id.to_string(),
                metadata,
                client,
            )))
        }
    }
}

struct PulledSession {
    info: Info,
    lifetime_lease: SessionIdLock,
}

/// Fetch, canonically claim, and marker-publish a remote session. Network I/O
/// happens before any exclusive filesystem claim is taken.
async fn try_pull_from_remote(
    info: &Info,
    client: &crate::remote::BackendClient,
) -> io::Result<Option<PulledSession>> {
    // BackendClient resolves auth internally via its auth_manager.
    if client.auth_manager.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote pull requires an authenticated backend client",
        ));
    }

    tracing::info!(session_id = %info.id, "Session not found locally, trying backend");

    let fetched = match crate::remote::pull::fetch_session(&info.id.0, client).await {
        Ok(crate::remote::pull::FetchResult::Fetched(fetched)) => fetched,
        Ok(crate::remote::pull::FetchResult::NotFound) => {
            tracing::debug!(session_id = %info.id, "Session not found on backend either");
            return Ok(None);
        }
        Err(e) => {
            tracing::warn!(session_id = %info.id, error = %e, "Backend pull failed");
            return Err(match e {
                error @ (crate::remote::pull::PullError::SessionIdMismatch { .. }
                | crate::remote::pull::PullError::MalformedSession { .. }) => {
                    io::Error::new(io::ErrorKind::InvalidData, error)
                }
                error @ crate::remote::pull::PullError::Backend(_) => io::Error::other(error),
            });
        }
    };

    let sessions_root = grok_home().join("sessions");
    let claim = acquire_canonical_session_claim(
        &sessions_root,
        &info.id.to_string(),
        Some(&fetched.info().cwd),
    )
    .await
    .inspect_err(|error| {
        tracing::warn!(session_id = %info.id, %error, "Could not claim local session hydration");
    })?;

    match claim {
        CanonicalSessionClaim::Existing(existing) => {
            validate_existing_remote_pull(&sessions_root, &info.id.to_string(), existing).map(Some)
        }
        CanonicalSessionClaim::Vacant(mut writer) => {
            let pulled_info = fetched.info().clone();
            let session_dir = sessions_root
                .join(crate::util::grok_home::encode_cwd_dirname(&pulled_info.cwd))
                .join(info.id.to_string());
            let stage = writer
                .begin_new(session_dir)
                .inspect_err(|error| {
                    tracing::warn!(session_id = %info.id, %error, "Could not begin local session hydration");
                })?
                .to_path_buf();
            let num_messages = crate::remote::pull::hydrate::write_to_dir(&stage, &fetched)
                .and_then(|count| {
                    crate::remote::pull::hydrate::sync_tree_durable(&stage).map(|()| count)
                })
                .map_err(|error| {
                    tracing::warn!(session_id = %info.id, %error, "Backend session hydration failed");
                    io::Error::other(error)
                })?;
            match writer.publish_new_classified() {
                Ok(()) => {}
                Err(PublishedSessionFinalizeError::NotCommitted(error)) => {
                    tracing::warn!(session_id = %info.id, %error, "Backend session publication did not commit");
                    return Err(error);
                }
                Err(PublishedSessionFinalizeError::CommittedDurability(error)) => {
                    tracing::warn!(session_id = %info.id, %error, "Backend session publication committed but durability acknowledgement failed");
                }
                Err(PublishedSessionFinalizeError::CommittedIdentity(error)) => {
                    tracing::error!(session_id = %info.id, %error, "Backend session publication committed but canonical identity verification failed");
                    return Err(error);
                }
            }
            let published = writer
                .into_lifetime_read()
                .inspect_err(|error| {
                    tracing::warn!(session_id = %info.id, %error, "Backend session lease handoff failed");
                })?;
            tracing::info!(
                session_id = %info.id,
                pulled_cwd = %pulled_info.cwd,
                num_messages,
                "Pulled session from backend"
            );
            Ok(Some(PulledSession {
                info: pulled_info,
                lifetime_lease: published.into_lifetime_lease(),
            }))
        }
    }
}

fn validate_existing_remote_pull(
    sessions_root: &Path,
    requested_session_id: &str,
    existing: PublishedSessionRead,
) -> io::Result<PulledSession> {
    let summary = existing.read_summary().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("canonical pulled session has invalid summary: {error}"),
        )
    })?;
    if summary.info.id.to_string() != requested_session_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "canonical pulled session id mismatch: requested {requested_session_id}, found {}",
                summary.info.id
            ),
        ));
    }
    let expected_path = sessions_root
        .join(crate::util::grok_home::encode_cwd_dirname(
            &summary.info.cwd,
        ))
        .join(requested_session_id);
    if existing.path() != expected_path {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "canonical pulled session path mismatch for {requested_session_id}: expected {}, found {}",
                expected_path.display(),
                existing.path().display()
            ),
        ));
    }
    Ok(PulledSession {
        info: summary.info,
        lifetime_lease: existing.into_lifetime_lease(),
    })
}

pub(crate) fn is_disk_full_io_error(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::StorageFull {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(
            e.raw_os_error(),
            Some(raw) if raw == libc::ENOSPC || raw == libc::EDQUOT
        )
    }
    #[cfg(windows)]
    {
        const ERROR_DISK_FULL: i32 = 112;
        const ERROR_HANDLE_DISK_FULL: i32 = 39;
        matches!(
            e.raw_os_error(),
            Some(ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL)
        )
    }
    #[cfg(not(any(unix, windows)))]
    false
}

/// Map a persistence `io::Error` into an `acp::Error` with a human-friendly
/// `message` and a stable `data.code` for log aggregation.
pub(crate) fn io_error_to_acp(e: &io::Error) -> acp::Error {
    let (message, code) = if is_disk_full_io_error(e) {
        ("No space left on device", "FS_DISK_QUOTA_EXCEEDED")
    } else {
        match e.kind() {
            io::ErrorKind::NotFound => ("Path not found.", "FS_NOT_FOUND"),
            io::ErrorKind::PermissionDenied => ("Permission denied.", "FS_PERMISSION_DENIED"),
            _ => {
                tracing::warn!(error = %e, kind = ?e.kind(), raw_os = ?e.raw_os_error(), "unclassified persistence I/O error");
                ("An unexpected I/O error occurred.", "FS_OTHER")
            }
        }
    };
    acp::Error::new(acp::ErrorCode::InternalError.into(), message.to_string()).data(Some(
        serde_json::json!({
            "code": code,
            "detail": e.to_string(),
        }),
    ))
}

#[cfg(test)]
mod io_error_to_acp_tests {
    use super::io_error_to_acp;
    use std::io;

    #[test]
    fn storage_full_maps_to_no_space_left() {
        let io = io::Error::from(io::ErrorKind::StorageFull);
        assert!(super::is_disk_full_io_error(&io));
        let acp_err = io_error_to_acp(&io);
        assert_eq!(acp_err.message, "No space left on device");
        assert_eq!(acp_err.data.unwrap()["code"], "FS_DISK_QUOTA_EXCEEDED");
    }
}

/// Best-effort worktree liveness touch: stamp `last_accessed_at` on the
/// worktree containing this session's cwd so `grok worktree gc` expires by
/// last use, not creation time. Lives here — not in a `StorageAdapter` —
/// so every session create/load path shares it regardless of backend.
fn spawn_worktree_touch(info: &Info) -> tokio::task::JoinHandle<()> {
    let cwd = info.cwd.clone();
    tokio::task::spawn_blocking(move || {
        crate::session::worktree::touch_worktree_for_cwd(&cwd);
    })
}

/// Bound on how long session open waits for the liveness touch to commit —
/// generous vs the DB's 5s busy_timeout without letting a pathologically
/// locked worktrees.db stall init.
const WORKTREE_TOUCH_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Touch the worktree and wait (bounded) for the write to commit before the
/// session open completes: a detached touch can land after gc's pre-removal
/// re-check reads the row, letting gc delete a worktree that is actively
/// being opened or resumed. Awaiting a blocking-pool task does not block the
/// runtime; on timeout the task keeps running detached (the old
/// fire-and-forget behavior) and init proceeds.
async fn touch_worktree_for_session(info: &Info) {
    if tokio::time::timeout(WORKTREE_TOUCH_INIT_TIMEOUT, spawn_worktree_touch(info))
        .await
        .is_err()
    {
        tracing::debug!(
            cwd = %info.cwd,
            "worktree liveness touch still pending at session open"
        );
    }
}

/// Floor between activity-driven worktree touches from the persistence actor.
const WORKTREE_TOUCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Physical publication plan for one fresh session.
///
/// Before final auth/catalog sealing, every write is bound to `stage_session`,
/// below the owner-only private namespace. `finalize` performs the single
/// no-replace namespace commit and then atomically rebinds future writes.
#[derive(Clone, Debug)]
pub(crate) struct FreshPublication {
    root_dir: PathBuf,
    stage_container: PathBuf,
    stage_session: PathBuf,
    published_parent: PathBuf,
    published_session: PathBuf,
    stage_container_anchor: Arc<std::sync::Mutex<Option<AnchoredDirectory>>>,
    stage_session_anchor: Arc<std::sync::Mutex<Option<AnchoredDirectory>>>,
    published_parent_name: OsString,
    session_name: OsString,
    path_binding: crate::session::storage::jsonl::SessionPathBinding,
    committed: Arc<std::sync::atomic::AtomicBool>,
}

impl FreshPublication {
    pub(crate) fn physical_path(&self) -> PathBuf {
        self.path_binding.path()
    }

    fn is_committed(&self) -> bool {
        self.committed.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn finalize(&self) -> Result<(), FreshPublicationFinalizeError> {
        finalize_fresh_publication_sync(self)
    }
}

/// Process-held ownership of the cross-process advisory lock for one fresh
/// session id.
///
/// The lock file is deliberately reusable rather than deleted on drop. Its
/// existence carries no state; only the OS lock does, so a crash releases the
/// claim without leaving a tombstone that blocks a later retry.
#[derive(Debug)]
struct FreshSessionClaim {
    session_id_lock: Option<SessionIdLock>,
    publication: FreshPublication,
    cleanup_armed: bool,
}

#[derive(Debug)]
struct FreshLeaseTransitionError {
    error: io::Error,
    claim: FreshSessionClaim,
}

impl FreshSessionClaim {
    fn disarm(mut self) {
        self.cleanup_armed = false;
    }

    fn into_published_lease(self) -> Result<SessionIdLock, FreshLeaseTransitionError> {
        #[cfg(test)]
        if self
            .publication
            .published_session
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(take_fresh_lease_transition_failure_test_hook)
        {
            return self.into_published_lease_with(|lease| {
                lease.transition_exclusive_to_lifetime_shared_with(|_| {
                    Err(io::Error::other("injected shared-lock transition failure"))
                })
            });
        }
        self.into_published_lease_with(|lease| lease.transition_exclusive_to_lifetime_shared())
    }

    fn into_published_lease_with(
        mut self,
        transition: impl FnOnce(&mut SessionIdLock) -> io::Result<()>,
    ) -> Result<SessionIdLock, FreshLeaseTransitionError> {
        // Do not take the lease before downgrade succeeds. Return the entire
        // claim on failure so the actor can restore it and retain the exclusive
        // namespace lock instead of silently dropping its last owner.
        if let Err(error) = transition(
            self.session_id_lock
                .as_mut()
                .expect("fresh claim owns one session id lease"),
        ) {
            return Err(FreshLeaseTransitionError { error, claim: self });
        }
        self.cleanup_armed = false;
        Ok(self
            .session_id_lock
            .take()
            .expect("successfully downgraded fresh lease"))
    }
}

impl Drop for FreshSessionClaim {
    fn drop(&mut self) {
        if !self.cleanup_armed {
            return;
        }
        if self.publication.is_committed() {
            return;
        }
        // Start cleanup from the retained stage-container handle rather than
        // the diagnostic pathname. Dropping the child handle first also
        // permits ancestor removal on Windows. Recursive child cleanup is
        // still best-effort against hostile same-UID entry swaps; see #342.
        self.publication
            .stage_session_anchor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let stage_container = self
            .publication
            .stage_container_anchor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(stage_container) = stage_container
            && let Err(error) = stage_container.remove_tree_self()
        {
            tracing::warn!(
                path = %self.publication.stage_container.display(),
                %error,
                "failed to remove cancelled private fresh-session stage"
            );
        }
    }
}

fn session_claim_lock_stem(session_id: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(session_id.as_bytes()))
}

pub(crate) fn session_stage_container_name(session_id: &str) -> OsString {
    OsString::from(format!("session-{}", session_claim_lock_stem(session_id)))
}

fn session_claim_lock_name(session_id: &str) -> String {
    format!("{}.namespace.lock", session_claim_lock_stem(session_id))
}

fn session_mutation_lock_name(session_id: &str) -> String {
    format!("{}.mutation.lock", session_claim_lock_stem(session_id))
}

/// Cross-process ownership for one persisted session id.
///
/// Fresh creation and deletion take this exclusively. Loading and discovery
/// take shared leases, so they can run together but cannot observe or mutate a
/// provisional directory before publication releases the exclusive claim.
#[derive(Debug)]
pub(crate) struct SessionIdLock {
    namespace: Option<std::fs::File>,
    mutation: Option<std::fs::File>,
}

impl SessionIdLock {
    fn transition_exclusive_to_lifetime_shared(&mut self) -> io::Result<()> {
        self.transition_exclusive_to_lifetime_shared_with(FileExt::lock_shared)
    }

    fn transition_exclusive_to_lifetime_shared_with(
        &mut self,
        lock_shared: impl FnOnce(&std::fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        self.transition_exclusive_to_lifetime_shared_with_unlock(lock_shared, |namespace| {
            FileExt::unlock(namespace)
        })
    }

    fn transition_exclusive_to_lifetime_shared_with_unlock(
        &mut self,
        lock_shared: impl FnOnce(&std::fs::File) -> io::Result<()>,
        unlock_namespace: impl FnOnce(&std::fs::File) -> io::Result<()>,
    ) -> io::Result<()> {
        let mutation = self.mutation.as_ref().expect("mutation lease");
        // Windows LockFileEx does not convert overlapping modes on one handle;
        // explicitly unlock before acquiring shared. Namespace remains held
        // exclusively on every failure path.
        FileExt::unlock(mutation)?;
        lock_shared(mutation)?;
        if let Some(namespace) = self.namespace.as_ref() {
            unlock_namespace(namespace)?;
        }
        self.namespace.take();
        Ok(())
    }

    fn retain_lifetime_shared(mut self) -> io::Result<Self> {
        if let Some(namespace) = self.namespace.as_ref() {
            FileExt::unlock(namespace)?;
        }
        self.namespace.take();
        Ok(self)
    }
}

/// A published session resolved while holding its per-id shared visibility lock.
///
/// Keep this value alive for the entire filesystem read.  Resolving a path and
/// then dropping the lock would allow a concurrent delete/import to replace the
/// directory between the visibility check and the actual I/O.
#[derive(Debug)]
pub(crate) struct PublishedSessionRead {
    _session_id_lock: SessionIdLock,
    session_dir: PathBuf,
}

impl PublishedSessionRead {
    pub(crate) fn path(&self) -> &Path {
        &self.session_dir
    }

    pub(crate) fn read_summary(&self) -> io::Result<Summary> {
        read_summary_from_dir(&self.session_dir).map_err(io::Error::other)
    }

    fn into_lifetime_lease(self) -> SessionIdLock {
        self._session_id_lock
    }
}

/// Exclusive access to one session id, used by import and out-of-band repair.
///
/// New content is always written below the owner-only private staging namespace.
/// The public namespace changes only at the anchored no-replace rename commit.
#[derive(Debug)]
struct PublishedSessionStage {
    stage_container: PathBuf,
    stage_session: PathBuf,
    stage_container_anchor: Option<AnchoredDirectory>,
    stage_session_anchor: Option<AnchoredDirectory>,
    target_parent_name: OsString,
    target_session_name: OsString,
    target_session: PathBuf,
}

#[derive(Debug)]
pub(crate) enum PublishedSessionFinalizeError {
    NotCommitted(io::Error),
    CommittedDurability(io::Error),
    CommittedIdentity(io::Error),
}

impl std::fmt::Display for PublishedSessionFinalizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(error) => write!(formatter, "publication not committed: {error}"),
            Self::CommittedDurability(error) => {
                write!(
                    formatter,
                    "publication committed with durability failure: {error}"
                )
            }
            Self::CommittedIdentity(error) => {
                write!(
                    formatter,
                    "publication committed but canonical identity is unverified: {error}"
                )
            }
        }
    }
}

impl std::error::Error for PublishedSessionFinalizeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotCommitted(error)
            | Self::CommittedDurability(error)
            | Self::CommittedIdentity(error) => Some(error),
        }
    }
}

#[derive(Debug)]
pub(crate) struct PublishedSessionWrite {
    session_id_lock: Option<SessionIdLock>,
    sessions_root: PathBuf,
    session_id: String,
    published_dir: Option<PathBuf>,
    stage: Option<PublishedSessionStage>,
}

impl PublishedSessionWrite {
    pub(crate) fn published_path(&self) -> Option<&Path> {
        self.published_dir.as_deref()
    }

    pub(crate) fn read_summary(&self) -> io::Result<Option<Summary>> {
        self.published_dir
            .as_deref()
            .map(read_summary_from_dir)
            .transpose()
            .map_err(io::Error::other)
    }

    fn into_lifetime_read(mut self) -> io::Result<PublishedSessionRead> {
        let session_dir = self
            .published_dir
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "session was not published"))?;
        let mut session_id_lock = self
            .session_id_lock
            .take()
            .expect("published writer owns one session id lease");
        session_id_lock.transition_exclusive_to_lifetime_shared()?;
        Ok(PublishedSessionRead {
            _session_id_lock: session_id_lock,
            session_dir,
        })
    }

    pub(crate) fn begin_new(&mut self, session_dir: PathBuf) -> io::Result<&Path> {
        if self.published_dir.is_some() || self.stage.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "session id is already present",
            ));
        }
        let target_parent = session_dir.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "session directory has no parent",
            )
        })?;
        if target_parent.parent() != Some(self.sessions_root.as_path())
            || session_dir.file_name() != Some(OsStr::new(&self.session_id))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session directory does not match the locked session id",
            ));
        }
        // Any public namespace entry with this id, including a legacy marker-
        // bearing directory under another cwd, is preserved and fails closed.
        if public_session_id_namespace_present(&self.sessions_root, &self.session_id)?
            || std::fs::symlink_metadata(&session_dir).is_ok()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "public session target already exists: {}",
                    session_dir.display()
                ),
            ));
        }

        let root_dir = session_id_lock_root_for_sessions_root(&self.sessions_root)?;
        let (staging_root, staging_root_anchor) =
            ensure_private_staging_hierarchy_anchored(root_dir)?;
        reclaim_abandoned_session_stages(&staging_root_anchor, &self.session_id)?;
        let stage_container_name = session_stage_container_name(&self.session_id);
        let mut stage_container_anchor =
            Some(staging_root_anchor.create_child_dir(&stage_container_name)?);
        if let Err(error) = stage_container_anchor
            .as_ref()
            .expect("new stage container")
            .ensure_owner_only()
        {
            let _ = stage_container_anchor
                .take()
                .expect("new stage container")
                .remove_tree_self();
            return Err(error);
        }
        let stage_container = staging_root.join(&stage_container_name);
        let target_session_name = OsString::from(self.session_id.as_str());
        let stage_session_anchor = match stage_container_anchor
            .as_ref()
            .expect("new stage container")
            .create_child_dir(&target_session_name)
        {
            Ok(session) => session,
            Err(error) => {
                let _ = stage_container_anchor
                    .take()
                    .expect("new stage container")
                    .remove_tree_self();
                return Err(error);
            }
        };
        if let Err(error) = stage_session_anchor.ensure_owner_only() {
            drop(stage_session_anchor);
            let _ = stage_container_anchor
                .take()
                .expect("new stage container")
                .remove_tree_self();
            return Err(error);
        }
        let stage_session = stage_container.join(&target_session_name);
        if let Err(error) = create_unpublished_session_marker(&stage_session_anchor) {
            drop(stage_session_anchor);
            let _ = stage_container_anchor
                .take()
                .expect("new stage container")
                .remove_tree_self();
            return Err(error);
        }
        self.stage = Some(PublishedSessionStage {
            stage_container,
            stage_session,
            stage_container_anchor,
            stage_session_anchor: Some(stage_session_anchor),
            target_parent_name: target_parent
                .file_name()
                .expect("validated direct child parent")
                .to_os_string(),
            target_session_name,
            target_session: session_dir,
        });
        Ok(&self.stage.as_ref().expect("stage assigned").stage_session)
    }

    pub(crate) fn publish_new(&mut self) -> io::Result<()> {
        self.publish_new_classified().map_err(|error| match error {
            PublishedSessionFinalizeError::NotCommitted(error)
            | PublishedSessionFinalizeError::CommittedDurability(error)
            | PublishedSessionFinalizeError::CommittedIdentity(error) => error,
        })
    }

    pub(crate) fn publish_new_classified(&mut self) -> Result<(), PublishedSessionFinalizeError> {
        self.publish_new_with(|sessions_root, published_parent| {
            sync_directory(sessions_root)?;
            sync_directory(published_parent)
        })
    }

    fn publish_new_with(
        &mut self,
        sync_after_commit: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<(), PublishedSessionFinalizeError> {
        let mut stage = self.stage.take().ok_or_else(|| {
            PublishedSessionFinalizeError::NotCommitted(io::Error::new(
                io::ErrorKind::NotFound,
                "no private staged session to publish",
            ))
        })?;
        let summary = match read_valid_staged_summary(&stage.stage_session) {
            Ok(summary) => summary,
            Err(error) => {
                self.stage = Some(stage);
                return Err(PublishedSessionFinalizeError::NotCommitted(error));
            }
        };
        let stage_session_anchor = stage
            .stage_session_anchor
            .as_ref()
            .expect("private stage session anchor");
        if let Err(error) = stage_session_anchor
            .remove_marker(OsStr::new(UNPUBLISHED_SESSION_MARKER))
            .and_then(|()| stage_session_anchor.sync())
        {
            self.stage = Some(stage);
            return Err(PublishedSessionFinalizeError::NotCommitted(error));
        }

        let root_dir = match session_id_lock_root_for_sessions_root(&self.sessions_root) {
            Ok(root_dir) => root_dir,
            Err(error) => {
                self.stage = Some(stage);
                return Err(PublishedSessionFinalizeError::NotCommitted(error));
            }
        };
        let root_anchor = match AnchoredDirectory::open_root(root_dir) {
            Ok(root) => root,
            Err(error) => {
                self.stage = Some(stage);
                return Err(PublishedSessionFinalizeError::NotCommitted(error));
            }
        };
        let sessions_anchor =
            match open_or_create_anchored_child(&root_anchor, OsStr::new("sessions")) {
                Ok(sessions) => sessions,
                Err(error) => {
                    self.stage = Some(stage);
                    return Err(PublishedSessionFinalizeError::NotCommitted(error));
                }
            };

        let (published_parent_anchor, published_session_anchor) = match sessions_anchor
            .open_child_dir(&stage.target_parent_name)
        {
            Ok(parent) => {
                if let Err(error) = validate_existing_cwd_metadata(
                    &parent,
                    &stage.target_parent_name,
                    &summary.info.cwd,
                ) {
                    self.stage = Some(stage);
                    return Err(PublishedSessionFinalizeError::NotCommitted(error));
                }
                let stage_session = stage
                    .stage_session_anchor
                    .take()
                    .expect("private stage session anchor");
                let child = match stage_session
                    .try_rename_self_no_replace(&parent, &stage.target_session_name)
                {
                    Ok(child) => child,
                    Err(failure) => {
                        stage.stage_session_anchor = Some(failure.source);
                        self.stage = Some(stage);
                        return Err(PublishedSessionFinalizeError::NotCommitted(failure.error));
                    }
                };
                if let Some(container) = stage.stage_container_anchor.take()
                    && let Err(error) = container.remove_tree_self()
                {
                    tracing::warn!(%error, "failed to remove committed private stage container");
                }
                (parent, child)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let container = stage
                    .stage_container_anchor
                    .take()
                    .expect("private stage container anchor");
                if let Err(error) = write_staged_cwd_metadata_if_needed(
                    &container,
                    &stage.target_parent_name,
                    &summary.info.cwd,
                ) {
                    stage.stage_container_anchor = Some(container);
                    self.stage = Some(stage);
                    return Err(PublishedSessionFinalizeError::NotCommitted(error));
                }
                // Windows cannot rename the ancestor while its child handle
                // remains open. Drop it, then recover it from the returned
                // container if a concurrent winner creates the parent.
                drop(stage.stage_session_anchor.take());
                match container
                    .try_rename_self_no_replace(&sessions_anchor, &stage.target_parent_name)
                {
                    Ok(published_parent) => {
                        self.published_dir = Some(stage.target_session.clone());
                        let child = published_parent
                            .open_child_dir(&stage.target_session_name)
                            .map_err(PublishedSessionFinalizeError::CommittedIdentity)?;
                        (published_parent, child)
                    }
                    Err(failure) if failure.error.kind() == io::ErrorKind::AlreadyExists => {
                        let container = failure.source;
                        let child = match container.open_child_dir(&stage.target_session_name) {
                            Ok(child) => child,
                            Err(error) => {
                                stage.stage_container_anchor = Some(container);
                                self.stage = Some(stage);
                                return Err(PublishedSessionFinalizeError::NotCommitted(error));
                            }
                        };
                        let parent = match sessions_anchor.open_child_dir(&stage.target_parent_name)
                        {
                            Ok(parent) => parent,
                            Err(error) => {
                                stage.stage_session_anchor = Some(child);
                                stage.stage_container_anchor = Some(container);
                                self.stage = Some(stage);
                                return Err(PublishedSessionFinalizeError::NotCommitted(error));
                            }
                        };
                        if let Err(error) = validate_existing_cwd_metadata(
                            &parent,
                            &stage.target_parent_name,
                            &summary.info.cwd,
                        ) {
                            stage.stage_session_anchor = Some(child);
                            stage.stage_container_anchor = Some(container);
                            self.stage = Some(stage);
                            return Err(PublishedSessionFinalizeError::NotCommitted(error));
                        }
                        let child = match child
                            .try_rename_self_no_replace(&parent, &stage.target_session_name)
                        {
                            Ok(child) => child,
                            Err(failure) => {
                                stage.stage_session_anchor = Some(failure.source);
                                stage.stage_container_anchor = Some(container);
                                self.stage = Some(stage);
                                return Err(PublishedSessionFinalizeError::NotCommitted(
                                    failure.error,
                                ));
                            }
                        };
                        if let Err(error) = container.remove_tree_self() {
                            tracing::warn!(%error, "failed to remove committed private stage container");
                        }
                        (parent, child)
                    }
                    Err(failure) => {
                        stage.stage_container_anchor = Some(failure.source);
                        self.stage = Some(stage);
                        return Err(PublishedSessionFinalizeError::NotCommitted(failure.error));
                    }
                }
            }
            Err(error) => {
                self.stage = Some(stage);
                return Err(PublishedSessionFinalizeError::NotCommitted(error));
            }
        };

        self.published_dir = Some(stage.target_session);
        verify_canonical_publication_identity(
            root_dir,
            &root_anchor,
            &sessions_anchor,
            &stage.target_parent_name,
            &published_parent_anchor,
            &stage.target_session_name,
            &published_session_anchor,
        )
        .map_err(PublishedSessionFinalizeError::CommittedIdentity)?;
        sync_after_commit(
            &self.sessions_root,
            self.published_dir
                .as_ref()
                .expect("published path assigned")
                .parent()
                .expect("published session has parent"),
        )
        .and_then(|()| published_session_anchor.sync())
        .and_then(|()| published_parent_anchor.sync())
        .and_then(|()| sessions_anchor.sync())
        .map_err(PublishedSessionFinalizeError::CommittedDurability)
    }
}

impl Drop for PublishedSessionWrite {
    fn drop(&mut self) {
        if let Some(mut stage) = self.stage.take() {
            // Start cleanup from the retained stage-container handle rather
            // than the diagnostic pathname. Recursive child cleanup is still
            // best-effort against hostile same-UID entry swaps; see #342.
            drop(stage.stage_session_anchor.take());
            if let Some(stage_container) = stage.stage_container_anchor.take()
                && let Err(error) = stage_container.remove_tree_self()
            {
                tracing::warn!(
                    path = %stage.stage_container.display(),
                    %error,
                    "failed to remove aborted private session stage"
                );
            }
        }
    }
}

fn open_session_id_lock_directory(root_dir: &Path) -> io::Result<AnchoredDirectory> {
    let root = AnchoredDirectory::open_root(root_dir)?;
    let locks = open_or_create_anchored_child(&root, OsStr::new(".locks"))?;
    locks.ensure_owner_only()?;
    let session_ids = open_or_create_anchored_child(&locks, OsStr::new("session-ids"))?;
    session_ids.ensure_owner_only()?;
    Ok(session_ids)
}

fn open_session_id_lock_files(
    root_dir: &Path,
    session_id: &str,
) -> io::Result<(std::fs::File, std::fs::File)> {
    let lock_dir = open_session_id_lock_directory(root_dir)?;
    Ok((
        lock_dir.open_or_create_owner_only_child_file(OsStr::new(&session_claim_lock_name(
            session_id,
        )))?,
        lock_dir.open_or_create_owner_only_child_file(OsStr::new(&session_mutation_lock_name(
            session_id,
        )))?,
    ))
}

fn session_id_lock_root_for_sessions_root(sessions_root: &Path) -> io::Result<&Path> {
    sessions_root.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "sessions root must have a grok-home parent",
        )
    })
}

fn acquire_session_id_lock_sync(root_dir: &Path, session_id: &str) -> io::Result<SessionIdLock> {
    let (namespace, mutation) = open_session_id_lock_files(root_dir, session_id)?;
    FileExt::lock_exclusive(&namespace)?;
    FileExt::lock_exclusive(&mutation)?;
    Ok(SessionIdLock {
        namespace: Some(namespace),
        mutation: Some(mutation),
    })
}

fn acquire_session_id_read_lock_sync(
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

async fn acquire_session_id_lock(root_dir: &Path, session_id: &str) -> io::Result<SessionIdLock> {
    let root_dir = root_dir.to_path_buf();
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || acquire_session_id_lock_sync(&root_dir, &session_id))
        .await
        .map_err(io::Error::other)?
}

async fn acquire_session_id_read_lock(
    root_dir: &Path,
    session_id: &str,
) -> io::Result<SessionIdLock> {
    let root_dir = root_dir.to_path_buf();
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || acquire_session_id_read_lock_sync(&root_dir, &session_id))
        .await
        .map_err(io::Error::other)?
}

/// Acquire a source-shared and target-exclusive lease in one deterministic
/// global order, preventing A-to-B and B-to-A fork copies from deadlocking.
pub(crate) fn acquire_ordered_copy_locks_sync(
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

fn resolve_published_session_dir_locked(
    sessions_root: &Path,
    session_id: &str,
    preferred_cwd: Option<&str>,
) -> io::Result<Option<PathBuf>> {
    if let Some(cwd) = preferred_cwd {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let preferred = sessions_root.join(encoded).join(session_id);
        if is_persisted_session_dir(&preferred) {
            return Ok(Some(preferred));
        }
    }
    find_persisted_session_dir_by_id_in_root_result(session_id, sessions_root)
}

pub(crate) async fn acquire_published_session_read(
    session_id: &str,
    preferred_cwd: Option<&str>,
) -> io::Result<Option<PublishedSessionRead>> {
    acquire_published_session_read_in_root(&grok_home().join("sessions"), session_id, preferred_cwd)
        .await
}

pub(crate) async fn acquire_published_session_read_in_root(
    sessions_root: &Path,
    session_id: &str,
    preferred_cwd: Option<&str>,
) -> io::Result<Option<PublishedSessionRead>> {
    let sessions_root = sessions_root.to_path_buf();
    let session_id = session_id.to_owned();
    let preferred_cwd = preferred_cwd.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let lock_root = session_id_lock_root_for_sessions_root(&sessions_root)?;
        let session_id_lock = acquire_session_id_read_lock_sync(lock_root, &session_id)?;
        let Some(session_dir) = resolve_published_session_dir_locked(
            &sessions_root,
            &session_id,
            preferred_cwd.as_deref(),
        )?
        else {
            return Ok(None);
        };
        Ok(Some(PublishedSessionRead {
            _session_id_lock: session_id_lock,
            session_dir,
        }))
    })
    .await
    .map_err(io::Error::other)?
}

pub(crate) async fn acquire_published_session_write(
    session_id: &str,
    preferred_cwd: Option<&str>,
) -> io::Result<PublishedSessionWrite> {
    acquire_published_session_write_in_root(
        &grok_home().join("sessions"),
        session_id,
        preferred_cwd,
    )
    .await
}

pub(crate) async fn acquire_published_session_write_in_root(
    sessions_root: &Path,
    session_id: &str,
    preferred_cwd: Option<&str>,
) -> io::Result<PublishedSessionWrite> {
    let sessions_root = sessions_root.to_path_buf();
    let session_id = session_id.to_owned();
    let preferred_cwd = preferred_cwd.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let lock_root = session_id_lock_root_for_sessions_root(&sessions_root)?;
        let session_id_lock = acquire_session_id_lock_sync(lock_root, &session_id)?;
        reclaim_abandoned_session_stages_in_root(lock_root, &session_id)?;
        let published_dir = resolve_published_session_dir_locked(
            &sessions_root,
            &session_id,
            preferred_cwd.as_deref(),
        )?;
        Ok(PublishedSessionWrite {
            session_id_lock: Some(session_id_lock),
            sessions_root,
            session_id,
            published_dir,
            stage: None,
        })
    })
    .await
    .map_err(io::Error::other)?
}

enum CanonicalSessionClaim {
    Existing(PublishedSessionRead),
    Vacant(Box<PublishedSessionWrite>),
}

fn acquire_canonical_session_claim_sync(
    sessions_root: &Path,
    session_id: &str,
    preferred_cwd: Option<&str>,
) -> io::Result<CanonicalSessionClaim> {
    let lock_root = session_id_lock_root_for_sessions_root(sessions_root)?;
    let (namespace, mutation) = open_session_id_lock_files(lock_root, session_id)?;
    FileExt::lock_exclusive(&namespace)?;

    if let Some(session_dir) =
        resolve_published_session_dir_locked(sessions_root, session_id, preferred_cwd)?
    {
        // Readers must not wait for an already-running actor's lifetime shared
        // mutation lease. Shared acquisition is compatible, while namespace
        // remains exclusive throughout the handoff.
        FileExt::lock_shared(&mutation)?;
        FileExt::unlock(&namespace)?;
        return Ok(CanonicalSessionClaim::Existing(PublishedSessionRead {
            _session_id_lock: SessionIdLock {
                namespace: None,
                mutation: Some(mutation),
            },
            session_dir,
        }));
    }

    FileExt::lock_exclusive(&mutation)?;
    Ok(CanonicalSessionClaim::Vacant(Box::new(
        PublishedSessionWrite {
            session_id_lock: Some(SessionIdLock {
                namespace: Some(namespace),
                mutation: Some(mutation),
            }),
            sessions_root: sessions_root.to_path_buf(),
            session_id: session_id.to_owned(),
            published_dir: None,
            stage: None,
        },
    )))
}

async fn acquire_canonical_session_claim(
    sessions_root: &Path,
    session_id: &str,
    preferred_cwd: Option<&str>,
) -> io::Result<CanonicalSessionClaim> {
    let sessions_root = sessions_root.to_path_buf();
    let session_id = session_id.to_owned();
    let preferred_cwd = preferred_cwd.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        acquire_canonical_session_claim_sync(&sessions_root, &session_id, preferred_cwd.as_deref())
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
fn try_acquire_session_id_write_lock_sync(
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

/// Attempt to take a shared per-session visibility lock without waiting.
///
/// Discovery paths use this to omit a fresh session while its creator still
/// owns the lock. In particular, the lock remains held across the synchronous
/// marker removal and [`crate::session::SessionPublicationGate::publish`], so a
/// marker-free provisional directory cannot leak through a concurrent list.
pub(crate) fn try_acquire_session_id_read_lock_sync(
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
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

fn ensure_private_staging_hierarchy_anchored(
    root_dir: &Path,
) -> io::Result<(PathBuf, AnchoredDirectory)> {
    let root = AnchoredDirectory::open_root(root_dir)?;
    let private = open_or_create_anchored_child(&root, OsStr::new(".private"))?;
    private.ensure_owner_only()?;
    let staging = open_or_create_anchored_child(&private, OsStr::new("session-staging"))?;
    staging.ensure_owner_only()?;
    Ok((root_dir.join(".private/session-staging"), staging))
}

fn create_unpublished_session_marker(session_dir: &AnchoredDirectory) -> io::Result<()> {
    let marker = session_dir.create_child_file_new(OsStr::new(UNPUBLISHED_SESSION_MARKER))?;
    marker.sync_all()?;
    session_dir.sync()
}

fn reclaim_abandoned_session_stages(
    staging_root: &AnchoredDirectory,
    session_id: &str,
) -> io::Result<()> {
    let session_name = OsStr::new(session_id);
    let deterministic_container_name = session_stage_container_name(session_id);
    for container_name in staging_root.child_names()? {
        let Ok(container) = staging_root.open_child_dir(&container_name) else {
            // A link/reparse point or a concurrently removed entry is not a
            // reclaimable private stage container.
            continue;
        };
        let session = container.open_child_dir(session_name).ok();
        if session.is_none() && container_name != deterministic_container_name {
            continue;
        }

        // A reclaimable container is exactly one staged session plus optional
        // cwd metadata used when publishing a whole cwd parent. Preserve any
        // other shape: it may be unrelated state or evidence of corruption.
        let Ok(child_names) = container.child_names() else {
            continue;
        };
        if child_names
            .iter()
            .any(|name| name.as_os_str() == session_name)
            && session.is_none()
        {
            // An entry with the target name exists but is not a real direct
            // child directory (symlink/reparse/non-directory). Preserve it.
            continue;
        }
        if child_names
            .iter()
            .any(|name| name.as_os_str() != session_name && name.as_os_str() != OsStr::new(".cwd"))
        {
            continue;
        }
        if child_names
            .iter()
            .any(|name| name.as_os_str() == OsStr::new(".cwd"))
            && container.open_child_file(OsStr::new(".cwd")).is_err()
        {
            // Do not treat a symlink/reparse point (or non-regular metadata
            // entry) as the optional cwd metadata file.
            continue;
        }

        // Only already-private, same-user state is eligible. The container
        // checks operate on retained handles; recursive child cleanup remains
        // best-effort against hostile same-UID entry swaps (see #342).
        if !container.is_owner_only().unwrap_or(false)
            || session
                .as_ref()
                .is_some_and(|session| !session.is_owner_only().unwrap_or(false))
        {
            continue;
        }
        drop(session);
        container.remove_tree_self()?;
        staging_root.sync()?;
    }
    Ok(())
}

/// Reclaim an abandoned private stage for `session_id`.
///
/// The caller must hold that session id's exclusive namespace and mutation
/// locks. This root-based form is shared by copy/import publication flows.
pub(crate) fn reclaim_abandoned_session_stages_in_root(
    root_dir: &Path,
    session_id: &str,
) -> io::Result<()> {
    let (_, staging_root) = ensure_private_staging_hierarchy_anchored(root_dir)?;
    reclaim_abandoned_session_stages(&staging_root, session_id)
}

fn verify_canonical_publication_identity(
    root_path: &Path,
    root: &AnchoredDirectory,
    sessions: &AnchoredDirectory,
    parent_name: &OsStr,
    published_parent: &AnchoredDirectory,
    session_name: &OsStr,
    published_session: &AnchoredDirectory,
) -> io::Result<()> {
    let canonical_root = AnchoredDirectory::open_root(root_path)?;
    if !canonical_root.same_identity(root)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "publication root identity is no longer canonically reachable",
        ));
    }
    let canonical_sessions = canonical_root.open_child_dir(OsStr::new("sessions"))?;
    if !canonical_sessions.same_identity(sessions)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "published sessions anchor is no longer canonically reachable",
        ));
    }
    let canonical_parent = canonical_sessions.open_child_dir(parent_name)?;
    if !canonical_parent.same_identity(published_parent)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "published cwd parent identity is not canonically reachable",
        ));
    }
    let canonical_session = canonical_parent.open_child_dir(session_name)?;
    if !canonical_session.same_identity(published_session)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "published session identity is not canonically reachable",
        ));
    }
    Ok(())
}

fn read_valid_staged_summary(stage_session: &Path) -> io::Result<Summary> {
    let summary_path = stage_session.join("summary.json");
    let metadata = std::fs::symlink_metadata(&summary_path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "published session requires a regular summary.json",
        ));
    }
    let bytes = std::fs::read(&summary_path)?;
    serde_json::from_slice::<Summary>(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn write_staged_cwd_metadata_if_needed(
    stage_container: &AnchoredDirectory,
    encoded_parent: &OsStr,
    cwd: &str,
) -> io::Result<()> {
    if encoded_parent == OsStr::new(urlencoding::encode(cwd).as_ref()) {
        return Ok(());
    }
    let mut metadata = stage_container.create_child_file_new(OsStr::new(".cwd"))?;
    use std::io::Write as _;
    metadata.write_all(cwd.as_bytes())?;
    metadata.sync_all()?;
    stage_container.sync()
}

pub(crate) fn validate_existing_cwd_metadata(
    published_parent: &AnchoredDirectory,
    encoded_parent: &OsStr,
    cwd: &str,
) -> io::Result<()> {
    if encoded_parent == OsStr::new(urlencoding::encode(cwd).as_ref()) {
        return Ok(());
    }
    let mut metadata = published_parent.open_child_file(OsStr::new(".cwd"))?;
    let mut bytes = Vec::new();
    use std::io::Read as _;
    metadata.read_to_end(&mut bytes)?;
    if bytes != cwd.as_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "published cwd metadata does not exactly match the session cwd",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum FreshPublicationFinalizeError {
    /// The no-replace namespace rename did not commit, so provisional creation
    /// may be safely aborted even if its private marker was already removed.
    NotCommitted(io::Error),
    /// The anchored namespace rename committed even though a later durability
    /// acknowledgement failed; the caller must preserve the published state.
    CommittedDurability(io::Error),
    CommittedIdentity(io::Error),
}

impl std::fmt::Display for FreshPublicationFinalizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted(error) => write!(formatter, "publication not committed: {error}"),
            Self::CommittedDurability(error) => {
                write!(
                    formatter,
                    "publication committed with durability failure: {error}"
                )
            }
            Self::CommittedIdentity(error) => {
                write!(
                    formatter,
                    "publication committed but canonical identity is unverified: {error}"
                )
            }
        }
    }
}

impl std::error::Error for FreshPublicationFinalizeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotCommitted(error)
            | Self::CommittedDurability(error)
            | Self::CommittedIdentity(error) => Some(error),
        }
    }
}

fn require_real_directory(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a real directory",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory is a reparse point",
            ));
        }
    }
    Ok(())
}

fn restore_fresh_stage_session(publication: &FreshPublication, anchor: AnchoredDirectory) {
    *publication
        .stage_session_anchor
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(anchor);
}

fn restore_fresh_stage_container(publication: &FreshPublication, anchor: AnchoredDirectory) {
    *publication
        .stage_container_anchor
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(anchor);
}

fn finalize_fresh_publication_sync_with(
    publication: &FreshPublication,
    sync_published: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<(), FreshPublicationFinalizeError> {
    let summary = read_valid_staged_summary(&publication.stage_session)
        .map_err(FreshPublicationFinalizeError::NotCommitted)?;
    let stage_session_anchor = publication
        .stage_session_anchor
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
        .ok_or_else(|| {
            FreshPublicationFinalizeError::NotCommitted(io::Error::new(
                io::ErrorKind::NotFound,
                "fresh publication stage was already consumed",
            ))
        })?;
    if let Err(error) = stage_session_anchor
        .remove_marker(OsStr::new(UNPUBLISHED_SESSION_MARKER))
        .and_then(|()| stage_session_anchor.sync())
    {
        restore_fresh_stage_session(publication, stage_session_anchor);
        return Err(FreshPublicationFinalizeError::NotCommitted(error));
    }

    let root_anchor = match AnchoredDirectory::open_root(&publication.root_dir) {
        Ok(root) => root,
        Err(error) => {
            restore_fresh_stage_session(publication, stage_session_anchor);
            return Err(FreshPublicationFinalizeError::NotCommitted(error));
        }
    };
    let sessions_anchor = match open_or_create_anchored_child(&root_anchor, OsStr::new("sessions"))
    {
        Ok(sessions) => sessions,
        Err(error) => {
            restore_fresh_stage_session(publication, stage_session_anchor);
            return Err(FreshPublicationFinalizeError::NotCommitted(error));
        }
    };

    let (published_parent_anchor, published_session_anchor) = match sessions_anchor
        .open_child_dir(&publication.published_parent_name)
    {
        Ok(parent) => {
            if let Err(error) = validate_existing_cwd_metadata(
                &parent,
                &publication.published_parent_name,
                &summary.info.cwd,
            ) {
                restore_fresh_stage_session(publication, stage_session_anchor);
                return Err(FreshPublicationFinalizeError::NotCommitted(error));
            }
            let child = match stage_session_anchor
                .try_rename_self_no_replace(&parent, &publication.session_name)
            {
                Ok(child) => child,
                Err(failure) => {
                    restore_fresh_stage_session(publication, failure.source);
                    return Err(FreshPublicationFinalizeError::NotCommitted(failure.error));
                }
            };
            if let Some(container) = publication
                .stage_container_anchor
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
                && let Err(error) = container.remove_tree_self()
            {
                tracing::warn!(%error, "failed to remove committed private fresh stage container");
            }
            (parent, child)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let stage_container_anchor = match publication
                .stage_container_anchor
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                Some(container) => container,
                None => {
                    restore_fresh_stage_session(publication, stage_session_anchor);
                    return Err(FreshPublicationFinalizeError::NotCommitted(io::Error::new(
                        io::ErrorKind::NotFound,
                        "fresh publication container was already consumed",
                    )));
                }
            };
            if let Err(error) = write_staged_cwd_metadata_if_needed(
                &stage_container_anchor,
                &publication.published_parent_name,
                &summary.info.cwd,
            ) {
                restore_fresh_stage_session(publication, stage_session_anchor);
                restore_fresh_stage_container(publication, stage_container_anchor);
                return Err(FreshPublicationFinalizeError::NotCommitted(error));
            }
            drop(stage_session_anchor);
            match stage_container_anchor
                .try_rename_self_no_replace(&sessions_anchor, &publication.published_parent_name)
            {
                Ok(published_parent) => {
                    publication
                        .path_binding
                        .rebind(publication.published_session.clone());
                    publication
                        .committed
                        .store(true, std::sync::atomic::Ordering::Release);
                    let child = published_parent
                        .open_child_dir(&publication.session_name)
                        .map_err(FreshPublicationFinalizeError::CommittedIdentity)?;
                    (published_parent, child)
                }
                Err(failure) if failure.error.kind() == io::ErrorKind::AlreadyExists => {
                    let container = failure.source;
                    let child = match container.open_child_dir(&publication.session_name) {
                        Ok(child) => child,
                        Err(error) => {
                            restore_fresh_stage_container(publication, container);
                            return Err(FreshPublicationFinalizeError::NotCommitted(error));
                        }
                    };
                    let parent =
                        match sessions_anchor.open_child_dir(&publication.published_parent_name) {
                            Ok(parent) => parent,
                            Err(error) => {
                                restore_fresh_stage_session(publication, child);
                                restore_fresh_stage_container(publication, container);
                                return Err(FreshPublicationFinalizeError::NotCommitted(error));
                            }
                        };
                    if let Err(error) = validate_existing_cwd_metadata(
                        &parent,
                        &publication.published_parent_name,
                        &summary.info.cwd,
                    ) {
                        restore_fresh_stage_session(publication, child);
                        restore_fresh_stage_container(publication, container);
                        return Err(FreshPublicationFinalizeError::NotCommitted(error));
                    }
                    let child = match child
                        .try_rename_self_no_replace(&parent, &publication.session_name)
                    {
                        Ok(child) => child,
                        Err(failure) => {
                            restore_fresh_stage_session(publication, failure.source);
                            restore_fresh_stage_container(publication, container);
                            return Err(FreshPublicationFinalizeError::NotCommitted(failure.error));
                        }
                    };
                    if let Err(error) = container.remove_tree_self() {
                        tracing::warn!(%error, "failed to remove committed private fresh stage container");
                    }
                    (parent, child)
                }
                Err(failure) => {
                    restore_fresh_stage_container(publication, failure.source);
                    return Err(FreshPublicationFinalizeError::NotCommitted(failure.error));
                }
            }
        }
        Err(error) => {
            restore_fresh_stage_session(publication, stage_session_anchor);
            return Err(FreshPublicationFinalizeError::NotCommitted(error));
        }
    };

    publication
        .path_binding
        .rebind(publication.published_session.clone());
    publication
        .committed
        .store(true, std::sync::atomic::Ordering::Release);
    verify_canonical_publication_identity(
        &publication.root_dir,
        &root_anchor,
        &sessions_anchor,
        &publication.published_parent_name,
        &published_parent_anchor,
        &publication.session_name,
        &published_session_anchor,
    )
    .map_err(FreshPublicationFinalizeError::CommittedIdentity)?;
    sync_published(
        &publication.root_dir.join("sessions"),
        &publication.published_parent,
    )
    .and_then(|()| published_session_anchor.sync())
    .and_then(|()| published_parent_anchor.sync())
    .and_then(|()| sessions_anchor.sync())
    .map_err(FreshPublicationFinalizeError::CommittedDurability)
}

fn finalize_fresh_publication_sync(
    publication: &FreshPublication,
) -> Result<(), FreshPublicationFinalizeError> {
    finalize_fresh_publication_sync_with(publication, |sessions_root, published_parent| {
        sync_directory(sessions_root)?;
        sync_directory(published_parent)
    })
}

#[cfg(test)]
fn staged_publication_for_test(root_dir: &Path, info: &Info) -> FreshPublication {
    let staging_root = root_dir.join(".private/session-staging");
    let stage_container = std::fs::read_dir(&staging_root)
        .expect("read private staging root")
        .map(|entry| entry.expect("read stage container").path())
        .find(|container| container.join(info.id.to_string()).is_dir())
        .expect("fresh stage for test session");
    let stage_container_name = stage_container.file_name().unwrap().to_os_string();
    let session_name = OsString::from(info.id.to_string());
    let stage_session = stage_container.join(&session_name);
    let published_parent_name =
        OsString::from(crate::util::grok_home::encode_cwd_dirname(&info.cwd));
    let published_parent = root_dir.join("sessions").join(&published_parent_name);
    let root_anchor = AnchoredDirectory::open_root(root_dir).unwrap();
    let private_anchor = root_anchor.open_child_dir(OsStr::new(".private")).unwrap();
    let staging_root_anchor = Arc::new(
        private_anchor
            .open_child_dir(OsStr::new("session-staging"))
            .unwrap(),
    );
    let stage_container_anchor = staging_root_anchor
        .open_child_dir(&stage_container_name)
        .unwrap();
    let stage_session_anchor = stage_container_anchor
        .open_child_dir(&session_name)
        .unwrap();
    FreshPublication {
        root_dir: root_dir.to_path_buf(),
        stage_container,
        stage_session: stage_session.clone(),
        published_session: published_parent.join(&session_name),
        published_parent,
        stage_container_anchor: Arc::new(std::sync::Mutex::new(Some(stage_container_anchor))),
        stage_session_anchor: Arc::new(std::sync::Mutex::new(Some(stage_session_anchor))),
        published_parent_name,
        session_name,
        path_binding: crate::session::storage::jsonl::SessionPathBinding::new(stage_session),
        committed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

#[cfg(test)]
fn finalize_fresh_publication_in_root_sync(
    root_dir: &Path,
    info: &Info,
) -> Result<(), FreshPublicationFinalizeError> {
    staged_publication_for_test(root_dir, info).finalize()
}

fn public_session_id_namespace_present(sessions_root: &Path, session_id: &str) -> io::Result<bool> {
    let cwd_entries = match std::fs::read_dir(sessions_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for cwd_entry in cwd_entries {
        let cwd_entry = cwd_entry?;
        let cwd_type = cwd_entry.file_type()?;
        if !cwd_type.is_dir() || cwd_type.is_symlink() {
            continue;
        }
        if std::fs::symlink_metadata(cwd_entry.path().join(session_id)).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn claim_fresh_session_sync(
    root_dir: &Path,
    session_id: &str,
    published_session: PathBuf,
) -> io::Result<FreshSessionClaim> {
    let session_id_lock = acquire_session_id_lock_sync(root_dir, session_id)?;
    let (staging_root, staging_root_anchor) = ensure_private_staging_hierarchy_anchored(root_dir)?;

    reclaim_abandoned_session_stages(&staging_root_anchor, session_id)?;

    if public_session_id_namespace_present(&root_dir.join("sessions"), session_id)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("a persisted session with id {session_id} already exists"),
        ));
    }

    let session_name = OsString::from(session_id);
    let published_parent = published_session
        .parent()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "published session has no parent",
            )
        })?
        .to_path_buf();
    if published_parent.parent() != Some(root_dir.join("sessions").as_path())
        || published_session.file_name() != Some(session_name.as_os_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "published session is not a direct session-id child",
        ));
    }
    let published_parent_name = published_parent
        .file_name()
        .expect("validated published parent")
        .to_os_string();

    let stage_container_name = session_stage_container_name(session_id);
    let mut stage_container_anchor =
        Some(staging_root_anchor.create_child_dir(&stage_container_name)?);
    if let Err(error) = stage_container_anchor
        .as_ref()
        .expect("new fresh stage container")
        .ensure_owner_only()
    {
        let _ = stage_container_anchor
            .take()
            .expect("new fresh stage container")
            .remove_tree_self();
        return Err(error);
    }
    let stage_container = staging_root.join(&stage_container_name);
    let stage_session_anchor = match stage_container_anchor
        .as_ref()
        .expect("new fresh stage container")
        .create_child_dir(&session_name)
    {
        Ok(session) => session,
        Err(error) => {
            let _ = stage_container_anchor
                .take()
                .expect("new fresh stage container")
                .remove_tree_self();
            return Err(error);
        }
    };
    if let Err(error) = stage_session_anchor.ensure_owner_only() {
        drop(stage_session_anchor);
        let _ = stage_container_anchor
            .take()
            .expect("new fresh stage container")
            .remove_tree_self();
        return Err(error);
    }
    let stage_session = stage_container.join(&session_name);
    if let Err(error) = create_unpublished_session_marker(&stage_session_anchor) {
        drop(stage_session_anchor);
        let _ = stage_container_anchor
            .take()
            .expect("new fresh stage container")
            .remove_tree_self();
        return Err(error);
    }

    let path_binding =
        crate::session::storage::jsonl::SessionPathBinding::new(stage_session.clone());
    let publication = FreshPublication {
        root_dir: root_dir.to_path_buf(),
        stage_container,
        stage_session,
        published_parent,
        published_session,
        stage_container_anchor: Arc::new(std::sync::Mutex::new(stage_container_anchor)),
        stage_session_anchor: Arc::new(std::sync::Mutex::new(Some(stage_session_anchor))),
        published_parent_name,
        session_name,
        path_binding,
        committed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    Ok(FreshSessionClaim {
        session_id_lock: Some(session_id_lock),
        publication,
        cleanup_armed: true,
    })
}

async fn claim_fresh_session(root_dir: &Path, info: &Info) -> io::Result<FreshSessionClaim> {
    let root_dir = root_dir.to_path_buf();
    let session_id = info.id.to_string();
    let published_session = root_dir
        .join("sessions")
        .join(crate::util::grok_home::encode_cwd_dirname(&info.cwd))
        .join(&session_id);
    tokio::task::spawn_blocking(move || {
        claim_fresh_session_sync(&root_dir, &session_id, published_session)
    })
    .await
    .map_err(io::Error::other)?
}

pub(crate) async fn new(
    info: &Info,
    model_id: acp::ModelId,
    sampling_client: OaiCompatClient,
    storage_mode: StorageMode,
    auth_manager: Option<Arc<crate::auth::AuthManager>>,
    relay_sync: Option<crate::relay::RelaySync>,
    gateway: Option<GatewaySender>,
    session_summary_model: String,
    registry_title_sync: Option<RegistryGeneratedTitleSync>,
) -> io::Result<PersistenceHandle> {
    let root_dir = grok_home();
    let fresh_claim = claim_fresh_session(&root_dir, info).await?;
    let fresh_publication = fresh_claim.publication.clone();
    let storage: Box<dyn StorageAdapter> = Box::new(
        JsonlStorageAdapter::with_session_path_binding(fresh_publication.path_binding.clone()),
    );

    // Initialize session in storage
    let mut summary = storage.init_session(info, model_id.clone()).await?;

    // Update model if different
    if summary.current_model_id != model_id {
        storage.update_current_model(info, &model_id).await?;
        summary.current_model_id = model_id;
    }

    let (handle, rx, summary_tx, disk_full_tx) = actor_channel(Some(fresh_publication));

    let info_clone = info.clone();
    let storage: Arc<dyn StorageAdapter> = Arc::from(storage);
    let remote_sync = init_remote_sync(&summary, storage_mode, auth_manager)?;
    tokio::task::spawn(async move {
        let persistence = SessionPersistence {
            info: info_clone,
            storage: storage.clone(),
            _published_session_lease: None,
            pending_notification: None,
            rx,
            remote_sync: remote_sync.clone(),
            created_fresh: true,
            fresh_claim: Some(fresh_claim),
            pending_publication_gate: None,
            fresh_publication_aborted: false,
            relay_sync,
            summary: crate::session::summary::SummaryGenerator::new(
                crate::session::summary::SummaryConfig {
                    sampling_client,
                    model: session_summary_model,
                    persistence_tx: summary_tx,
                },
            ),
            registry_title_sync,
            gateway,
            disk_full_tx,
            disk_full_notified: false,
        };
        persistence.run().await;
    });

    Ok(handle)
}

/// Create a persistence handle that writes to an explicit directory on disk.
///
/// Used for subagent child sessions whose files live under the parent's
/// session directory: `{parent_session_dir}/subagents/{subagent_id}/`.
///
/// Unlike [`new()`], this:
/// - Uses `JsonlStorageAdapter::with_explicit_session_dir()` to bypass
///   the standard `{root}/sessions/{cwd}/{id}/` path computation.
/// - Skips remote sync (subagent sessions are not synced to cloud).
/// - Skips relay sync (subagent sessions are not shared).
/// - Skips gateway (lifecycle notifications are handled by the coordinator).
pub(crate) async fn new_with_explicit_dir(
    info: &Info,
    target_dir: PathBuf,
    model_id: acp::ModelId,
    sampling_client: OaiCompatClient,
    session_summary_model: String,
) -> io::Result<PersistenceHandle> {
    let summary_path = target_dir.join("summary.json");
    let storage: Box<dyn StorageAdapter> =
        Box::new(JsonlStorageAdapter::with_explicit_session_dir(target_dir));

    // Initialize session in storage (creates summary.json, etc.)
    let mut summary = storage.init_session(info, model_id.clone()).await?;
    touch_worktree_for_session(info).await;
    if summary.session_kind.is_none() {
        summary.session_kind = Some("subagent".to_string());
    }
    let summary_json = serde_json::to_vec_pretty(&summary)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(&summary_path, summary_json)?;

    if summary.current_model_id != model_id {
        storage.update_current_model(info, &model_id).await?;
        summary.current_model_id = model_id;
    }

    let (handle, rx, summary_tx, disk_full_tx) = actor_channel(None);

    let info_clone = info.clone();
    let storage: Arc<dyn StorageAdapter> = Arc::from(storage);
    tokio::task::spawn(async move {
        let persistence = SessionPersistence {
            info: info_clone,
            storage: storage.clone(),
            _published_session_lease: None,
            pending_notification: None,
            rx,
            remote_sync: None,
            created_fresh: false,
            fresh_claim: None,
            pending_publication_gate: None,
            fresh_publication_aborted: false,
            relay_sync: None,
            summary: crate::session::summary::SummaryGenerator::new(
                crate::session::summary::SummaryConfig {
                    sampling_client,
                    model: session_summary_model,
                    persistence_tx: summary_tx,
                },
            ),
            registry_title_sync: None,
            gateway: None,
            disk_full_tx,
            disk_full_notified: false,
        };
        persistence.run().await;
    });

    Ok(handle)
}

pub struct PersistedInfo {
    pub summary: Summary,
    pub chat_history: Vec<ConversationItem>,
    /// All session updates (ACP updates and xAI extension updates) in chronological order
    pub updates: Vec<SessionUpdate>,
    pub plan_state: Option<TodoState>,
    pub rewind_points: Vec<RewindPoint>,
    /// Persisted session signals (None for old sessions without signals file)
    pub signals: Option<SessionSignals>,
    pub workflow_runs: Vec<crate::session::workflow::store::RestoredWorkflowRun>,
}

/// Same as PersistedInfo but without updates - for memory efficiency when streaming
pub struct PersistedInfoLight {
    pub summary: Summary,
    pub chat_history: Vec<ConversationItem>,
    pub plan_state: Option<TodoState>,
    pub plan_mode_state: Option<crate::session::plan_mode::PlanModeSnapshot>,
    /// Path to updates file for streaming reads
    pub updates_file_path: Option<std::path::PathBuf>,
    /// Adapter-owned path to `rewind_points.jsonl` for the session's
    /// `FileStateTracker` to load lazily. `None` if the backend doesn't persist
    /// rewind points to a streamable file.
    pub rewind_points_file_path: Option<std::path::PathBuf>,
    /// Persisted session signals (None for old sessions without signals file)
    pub signals: Option<SessionSignals>,
    /// Persisted announcement tracking state (None for sessions before this feature)
    pub announcement_state: Option<crate::session::announcement_state::AnnouncementState>,
    /// Persisted goal mode orchestration state (None for sessions without goal mode)
    pub goal_mode_state: Option<crate::session::goal_tracker::GoalOrchestration>,
    pub workflow_runs: Vec<crate::session::workflow::store::RestoredWorkflowRun>,
}

/// On a local NotFound, try pulling from the backend. Preserve the original
/// NotFound only when the backend has no session; propagate all pull failures.
async fn pull_on_miss(
    info: &Info,
    client: &crate::remote::BackendClient,
    err: io::Error,
) -> io::Result<PulledSession> {
    if err.kind() != io::ErrorKind::NotFound {
        return Err(err);
    }
    map_pull_on_miss_result(err, try_pull_from_remote(info, client).await)
}

fn map_pull_on_miss_result(
    original_not_found: io::Error,
    pulled: io::Result<Option<PulledSession>>,
) -> io::Result<PulledSession> {
    match pulled {
        Ok(Some(pulled)) => Ok(pulled),
        Ok(None) => Err(original_not_found),
        Err(error) => Err(error),
    }
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "wired when session restore flow calls load")
)]
pub(crate) async fn load(
    info: &Info,
    sampling_client: OaiCompatClient,
    storage_mode: StorageMode,
    auth_manager: Option<Arc<crate::auth::AuthManager>>,
    backend: Option<&crate::remote::BackendClient>,
    relay_sync: Option<crate::relay::RelaySync>,
    gateway: Option<GatewaySender>,
    session_summary_model: String,
    registry_title_sync: Option<RegistryGeneratedTitleSync>,
) -> io::Result<(PersistedInfo, PersistenceHandle)> {
    let root_dir = grok_home();
    let initial_session_id_lock =
        acquire_session_id_read_lock(&root_dir, &info.id.to_string()).await?;
    let storage: Box<dyn StorageAdapter> =
        Box::new(JsonlStorageAdapter::with_root(root_dir.clone()));

    let (persisted, loaded_info, session_id_lock) = match storage.load_session(info).await {
        Ok(p) => (
            p,
            info.clone(),
            initial_session_id_lock.retain_lifetime_shared()?,
        ),
        Err(e) => match backend {
            Some(client) => {
                // A local miss must release the initial shared lease before
                // remote fetch and canonical exclusive hydration.
                drop(initial_session_id_lock);
                let pulled = pull_on_miss(info, client, e).await?;
                let p = storage.load_session(&pulled.info).await?;
                (p, pulled.info, pulled.lifetime_lease)
            }
            None => return Err(e),
        },
    };
    // Touch on load too: resuming must reset the worktree's gc expiry clock.
    touch_worktree_for_session(&loaded_info).await;

    let persisted_info = PersistedInfo {
        summary: persisted.summary,
        chat_history: persisted.chat_history,
        updates: persisted.updates,
        plan_state: persisted.plan_state,
        rewind_points: persisted.rewind_points,
        signals: persisted.signals,
        workflow_runs: persisted.workflow_runs,
    };

    let (handle, rx, summary_tx, disk_full_tx) = actor_channel(None);

    let storage: Arc<dyn StorageAdapter> = Arc::from(storage);
    let remote_sync = init_remote_sync(&persisted_info.summary, storage_mode, auth_manager)?;

    let has_title = !persisted_info.summary.display_title().is_empty();
    tokio::task::spawn(async move {
        let mut summary_gen = crate::session::summary::SummaryGenerator::new(
            crate::session::summary::SummaryConfig {
                sampling_client,
                model: session_summary_model,
                persistence_tx: summary_tx,
            },
        );
        if has_title {
            summary_gen.mark_done();
        }
        let persistence = SessionPersistence {
            info: loaded_info,
            storage: storage.clone(),
            _published_session_lease: Some(session_id_lock),
            pending_notification: None,
            rx,
            remote_sync: remote_sync.clone(),
            created_fresh: false,
            fresh_claim: None,
            pending_publication_gate: None,
            fresh_publication_aborted: false,
            relay_sync,
            summary: summary_gen,
            registry_title_sync,
            gateway,
            disk_full_tx,
            disk_full_notified: false,
        };
        persistence.run().await;
    });

    Ok((persisted_info, handle))
}

/// Like `load`, but doesn't load updates into memory.
/// Instead, provides the path to the updates file for streaming reads.
/// Use this for memory-efficient session loading when replaying updates.
pub(crate) async fn load_light(
    info: &Info,
    sampling_client: OaiCompatClient,
    storage_mode: StorageMode,
    auth_manager: Option<Arc<crate::auth::AuthManager>>,
    backend: Option<&crate::remote::BackendClient>,
    relay_sync: Option<crate::relay::RelaySync>,
    gateway: Option<GatewaySender>,
    session_summary_model: String,
    registry_title_sync: Option<RegistryGeneratedTitleSync>,
) -> io::Result<(PersistedInfoLight, PersistenceHandle)> {
    let root_dir = grok_home();
    let initial_session_id_lock =
        acquire_session_id_read_lock(&root_dir, &info.id.to_string()).await?;
    let storage: Box<dyn StorageAdapter> =
        Box::new(JsonlStorageAdapter::with_root(root_dir.clone()));

    let (persisted, loaded_info, session_id_lock) =
        match storage.load_session_without_updates(info).await {
            Ok(p) => (
                p,
                info.clone(),
                initial_session_id_lock.retain_lifetime_shared()?,
            ),
            Err(e) => match backend {
                Some(client) => {
                    drop(initial_session_id_lock);
                    let pulled = pull_on_miss(info, client, e).await?;
                    let p = storage.load_session_without_updates(&pulled.info).await?;
                    (p, pulled.info, pulled.lifetime_lease)
                }
                None => return Err(e),
            },
        };
    // Touch on load too: resuming must reset the worktree's gc expiry clock.
    touch_worktree_for_session(&loaded_info).await;

    let updates_file_path = storage.updates_file_path(&loaded_info);
    let rewind_points_file_path = storage.rewind_points_file_path(&loaded_info);

    let persisted_info = PersistedInfoLight {
        summary: persisted.summary,
        chat_history: persisted.chat_history,
        plan_state: persisted.plan_state,
        plan_mode_state: persisted.plan_mode_state,
        updates_file_path,
        rewind_points_file_path,
        signals: persisted.signals,
        announcement_state: persisted.announcement_state,
        goal_mode_state: persisted.goal_mode_state,
        workflow_runs: persisted.workflow_runs,
    };

    let (handle, rx, summary_tx, disk_full_tx) = actor_channel(None);

    let storage: Arc<dyn StorageAdapter> = Arc::from(storage);
    let remote_sync = init_remote_sync(&persisted_info.summary, storage_mode, auth_manager)?;

    let has_title = !persisted_info.summary.display_title().is_empty();
    tokio::task::spawn(async move {
        let mut summary_gen = crate::session::summary::SummaryGenerator::new(
            crate::session::summary::SummaryConfig {
                sampling_client,
                model: session_summary_model,
                persistence_tx: summary_tx,
            },
        );
        if has_title {
            summary_gen.mark_done();
        }
        let persistence = SessionPersistence {
            info: loaded_info,
            storage: storage.clone(),
            _published_session_lease: Some(session_id_lock),
            pending_notification: None,
            rx,
            remote_sync: remote_sync.clone(),
            created_fresh: false,
            fresh_claim: None,
            pending_publication_gate: None,
            fresh_publication_aborted: false,
            relay_sync,
            summary: summary_gen,
            registry_title_sync,
            gateway,
            disk_full_tx,
            disk_full_notified: false,
        };
        persistence.run().await;
    });

    Ok((persisted_info, handle))
}

/// List session summaries, optionally filtered by cwd (absolute path string).
/// Returns summaries sorted by `last_active_at` (else `updated_at`) descending.
fn recover_session_relocations_in(root: &Path) -> crate::session::storage::relocation::Result<()> {
    crate::session::storage::relocation::RelocationStorage::new(root.into()).recover_all()
}

pub async fn list_summaries(cwd: Option<&str>) -> io::Result<Vec<Summary>> {
    let root_dir = crate::util::grok_home::grok_home();
    let recovery_root = root_dir.clone();
    tokio::task::spawn_blocking(move || recover_session_relocations_in(&recovery_root))
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?;
    let storage: Box<dyn StorageAdapter> = Box::new(JsonlStorageAdapter::with_root(root_dir));
    storage.list_sessions(cwd).await
}

/// Failure modes of [`delete_session_history`].
///
/// Kept distinct so callers can surface a precise message: a remote
/// failure is reported separately from a local-disk failure because the
/// remote delete runs first and aborts the whole operation (see the doc
/// on [`delete_session_history`]).
#[derive(Debug, thiserror::Error)]
pub enum DeleteSessionError {
    /// Acquiring the local visibility lock or resolving the on-disk session
    /// directory failed.
    #[error("failed to resolve local session: {0}")]
    List(#[source] io::Error),
    /// The remote (writeback) copy could not be deleted; local bits were
    /// left untouched so the operation can be retried.
    #[error("failed to delete remote session data: {0}")]
    Remote(#[source] crate::remote::client::BackendError),
    /// The local on-disk session directory could not be removed.
    #[error("failed to delete session: {0}")]
    Local(#[source] io::Error),
}

/// Where a session copy was actually removed by [`delete_session_history`].
///
/// Both fields are `false` when nothing existed to delete (still a
/// success). Callers use [`Self::any_removed`] to decide between a
/// "deleted" and a "not found" message without conflating a remote-only
/// delete with a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionDeletion {
    /// A local on-disk session directory was found and removed.
    pub local_removed: bool,
    /// A remote (writeback) copy was found and removed. `false` when
    /// `needs_remote` was not set, or the remote copy was already absent
    /// (the backend returned `404`).
    pub remote_removed: bool,
}

impl SessionDeletion {
    /// `true` when a copy was removed from at least one location.
    pub fn any_removed(self) -> bool {
        self.local_removed || self.remote_removed
    }
}

fn find_deletable_local_info_in_root(
    root_dir: &Path,
    session_id: &str,
    cwd: Option<&str>,
) -> io::Result<Option<Info>> {
    let sessions_root = root_dir.join("sessions");
    let view = storage_view(&sessions_root).map_err(io::Error::other)?;
    let session_dir = match cwd {
        Some(cwd) => view
            .session_dirs(Some(cwd))
            .map_err(io::Error::other)?
            .into_iter()
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name == std::ffi::OsStr::new(session_id))
            }),
        None => view
            .find_persisted_session_dir(session_id)
            .map_err(io::Error::other)?,
    };
    let Some(session_dir) = session_dir else {
        return Ok(None);
    };
    Ok(read_summary_from_dir(&session_dir)
        .ok()
        .filter(|summary| summary.info.id.0.as_ref() == session_id && !summary.is_hidden())
        .map(|summary| summary.info))
}

/// Permanently delete a session's history: the remote (writeback) copy
/// when `needs_remote`, the local on-disk session directory, and the
/// FTS search-index entry.
///
/// Idempotent: a session that is missing locally (e.g. remote-only)
/// still succeeds, and a remote `404` (copy already gone) is treated as
/// success rather than an error. When `needs_remote` is set the remote
/// delete runs *first* and is authoritative — only on its success (or a
/// `404`) are the local bits removed. This ordering prevents a partial
/// delete where the local copy is nuked but the remote copy lingers and
/// re-appears on the next session list.
///
/// Returns a [`SessionDeletion`] recording which copies (local / remote)
/// were actually removed; both fields `false` means nothing existed
/// (still `Ok`).
pub async fn delete_session_history(
    session_id: &str,
    cwd: Option<&str>,
    needs_remote: bool,
    auth_manager: Arc<crate::auth::AuthManager>,
) -> Result<SessionDeletion, DeleteSessionError> {
    let root_dir = crate::util::grok_home::grok_home();
    let _session_id_lock = acquire_session_id_lock(&root_dir, session_id)
        .await
        .map_err(DeleteSessionError::List)?;

    // Resolve under the same cross-process id lock held by fresh creation.
    // This waits out the marker-free finalizer-to-gate window and prevents a
    // delete from racing a creator that is still provisional.
    let local_info = find_deletable_local_info_in_root(&root_dir, session_id, cwd)
        .map_err(DeleteSessionError::List)?;

    // Remote delete first (authoritative for cloud history). A genuine
    // failure aborts before any local mutation so the row does not
    // reappear; a `404` means the copy is already gone, so deletion stays
    // idempotent and falls through to local cleanup.
    let remote_removed = if needs_remote {
        let result = crate::remote::client::BackendClient::new()
            .with_auth_manager(auth_manager)
            .delete_session_data(session_id)
            .await;
        classify_remote_delete(result)?
    } else {
        false
    };

    let Some(info) = local_info else {
        return Ok(SessionDeletion {
            local_removed: false,
            remote_removed,
        });
    };

    JsonlStorageAdapter::default()
        .delete_session(&info)
        .await
        .map_err(DeleteSessionError::Local)?;

    // Evict from the search index: the indexer re-reads the (now
    // missing) summary and drops the document.
    crate::session::storage::search::notify_session_updated(&info.id.to_string(), &info.cwd);

    Ok(SessionDeletion {
        local_removed: true,
        remote_removed,
    })
}

/// Classify a remote `delete_session_data` result, reporting whether a
/// remote copy was actually removed: a `2xx` means a copy was deleted
/// (`Ok(true)`), a `404` means it was already gone so deletion stays
/// idempotent (`Ok(false)`), and any other backend error aborts the
/// delete (`Err`) so local bits are left untouched and it can be retried.
fn classify_remote_delete(
    result: Result<(), crate::remote::client::BackendError>,
) -> Result<bool, DeleteSessionError> {
    use crate::remote::client::BackendError;
    match result {
        Ok(()) => Ok(true),
        Err(BackendError::RequestFailed { status: 404, .. }) => Ok(false),
        Err(e) => Err(DeleteSessionError::Remote(e)),
    }
}

#[cfg(test)]
#[path = "persistence_tests.rs"]
mod durable_update_tests;

#[cfg(test)]
mod fresh_abort_terminal_tests {
    use super::*;
    use crate::session::storage::jsonl::AppendDurability;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn aborted_fresh_publication_drops_queued_mutations_and_sync_side_effects() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000139";
        let root = tempfile::tempdir().expect("temporary grok home");
        let info = Info {
            id: acp::SessionId::new(SESSION_ID),
            cwd: "/repo/publication/abort-terminal".to_owned(),
        };
        let published_session = root
            .path()
            .join("sessions")
            .join(crate::util::grok_home::encode_cwd_dirname(&info.cwd))
            .join(SESSION_ID);
        let fresh_claim = claim_fresh_session_sync(root.path(), SESSION_ID, published_session)
            .expect("fresh session claim");
        let stage_session = fresh_claim.publication.stage_session.clone();
        let append_attempts = Arc::new(AtomicUsize::new(0));
        let observed_attempts = append_attempts.clone();
        let storage = Arc::new(JsonlStorageAdapter::with_update_append_probe(
            stage_session.clone(),
            move |_: AppendDurability| {
                observed_attempts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        ));
        storage
            .init_session(&info, default_model_id())
            .await
            .expect("initialize provisional session");

        let publication_gate = crate::session::SessionPublicationGate::pending();
        let (remote_sync, mut remote_observed) = RemoteSync::test_observer();

        let (tx, rx) = mpsc::unbounded_channel();
        let (disk_full_tx, _disk_full_rx) = watch::channel(false);
        let sampling_client =
            OaiCompatClient::new(xai_grok_sampler::SamplerConfig::default()).unwrap();
        let actor = tokio::spawn(
            SessionPersistence {
                info: info.clone(),
                storage: storage.clone(),
                _published_session_lease: None,
                pending_notification: None,
                rx,
                remote_sync: Some(remote_sync),
                created_fresh: true,
                fresh_claim: Some(fresh_claim),
                pending_publication_gate: None,
                fresh_publication_aborted: false,
                relay_sync: None,
                summary: crate::session::summary::SummaryGenerator::new(
                    crate::session::summary::SummaryConfig {
                        sampling_client,
                        model: String::new(),
                        persistence_tx: tx.downgrade(),
                    },
                ),
                registry_title_sync: None,
                gateway: None,
                disk_full_tx,
                disk_full_notified: false,
            }
            .run(),
        );

        PersistenceHandle::publish_fresh(&tx, publication_gate.clone())
            .await
            .expect("arm publication gate");
        publication_gate.abort();
        tx.send(PersistenceMsg::Update(SessionUpdate::Acp(Box::new(
            acp::SessionNotification::new(
                info.id.clone(),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new("must be discarded")),
                )),
            ),
        ))))
        .expect("queue update behind aborted publication");
        tx.send(PersistenceMsg::Chat(ConversationItem::user(
            "must be discarded",
        )))
        .expect("queue chat behind aborted publication");
        tx.send(PersistenceMsg::UpgradeToWriteback {
            auth_manager: Arc::new(crate::auth::AuthManager::new(
                root.path(),
                crate::auth::GrokComConfig::default(),
            )),
        })
        .expect("queue writeback upgrade behind aborted publication");
        let (barrier_tx, barrier_rx) = tokio::sync::oneshot::channel();
        tx.send(PersistenceMsg::FlushAndAck {
            respond_to: barrier_tx,
        })
        .expect("queue abort-only barrier");
        assert!(
            tokio::time::timeout(Duration::from_secs(1), barrier_rx)
                .await
                .expect("abort-only actor must consume the queued barrier")
                .is_err(),
            "abort-only actor must discard rather than execute queued barriers"
        );

        let persisted = storage
            .load_session(&info)
            .await
            .expect("read still-provisional session before cleanup");
        assert!(persisted.updates.is_empty());
        assert!(persisted.chat_history.is_empty());
        assert_eq!(append_attempts.load(Ordering::SeqCst), 0);
        assert!(
            !matches!(
                tokio::time::timeout(Duration::from_millis(50), remote_observed.recv()).await,
                Ok(Some(_))
            ),
            "the remote dispatch path must not receive an aborted update"
        );

        PersistenceHandle::abort_fresh_and_delete(&tx, publication_gate)
            .await
            .expect("delete provisional session");
        actor.await.expect("persistence actor join");
        assert!(!stage_session.exists());
    }
}

#[cfg(test)]
mod delete_session_history_tests {
    use super::{DeleteSessionError, SessionDeletion, classify_remote_delete};
    use crate::remote::client::BackendError;

    #[test]
    fn remote_ok_reports_removed() {
        assert!(
            classify_remote_delete(Ok(())).unwrap(),
            "a 2xx delete must report that a remote copy was removed"
        );
    }

    #[test]
    fn remote_404_is_treated_as_already_deleted() {
        let removed = classify_remote_delete(Err(BackendError::RequestFailed { status: 404 }))
            .expect("a 404 means the remote copy is gone — deletion must stay idempotent");
        assert!(
            !removed,
            "a 404 must report that nothing was removed remotely"
        );
    }

    #[test]
    fn remote_non_404_request_failure_aborts() {
        let res = classify_remote_delete(Err(BackendError::RequestFailed { status: 500 }));
        assert!(matches!(res, Err(DeleteSessionError::Remote(_))));
    }

    #[test]
    fn remote_auth_failure_aborts() {
        let res = classify_remote_delete(Err(BackendError::Auth("denied".into())));
        assert!(matches!(res, Err(DeleteSessionError::Remote(_))));
    }

    #[test]
    fn any_removed_reflects_either_location() {
        assert!(!SessionDeletion::default().any_removed());
        assert!(
            SessionDeletion {
                local_removed: true,
                remote_removed: false,
            }
            .any_removed()
        );
        assert!(
            SessionDeletion {
                local_removed: false,
                remote_removed: true,
            }
            .any_removed(),
            "a remote-only delete must count as removed"
        );
    }
}

/// List the `limit` most recently modified session summaries across all
/// workspaces. Uses stat-based mtime sorting to avoid reading every
/// summary file on disk; final order uses `last_active_at` else `updated_at`.
pub async fn list_recent_summaries(limit: usize) -> io::Result<Vec<Summary>> {
    let root_dir = crate::util::grok_home::grok_home();
    let recovery_root = root_dir.clone();
    tokio::task::spawn_blocking(move || recover_session_relocations_in(&recovery_root))
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?;
    let storage = JsonlStorageAdapter::with_root(root_dir);
    storage.list_sessions_recent(limit).await
}

// Session folder TTL cleanup

/// Guard ensuring session cleanup runs at most once per process.
static CLEANUP_SESSIONS_ONCE: std::sync::Once = std::sync::Once::new();

/// Default TTL for stale session files (30 days).
const DEFAULT_CLEANUP_TTL_DAYS: u32 = 30;

/// Walk `~/.grok/sessions/` and delete files with mtime older than `ttl_days`.
/// Removes empty session directories after file cleanup.
/// Skips `skip_session_dir` if provided (current session).
///
/// This is a **synchronous** function intended to be called via
/// `tokio::task::spawn_blocking` so it runs on the thread pool and
/// never competes with the agent's single-threaded `LocalSet`.
pub(crate) fn cleanup_stale_sessions(skip_session_dir: Option<&Path>) {
    CLEANUP_SESSIONS_ONCE.call_once(|| {
        let ttl_days = resolve_cleanup_ttl_days();
        let root = grok_home();
        if let Err(error) = recover_session_relocations_in(&root) {
            tracing::error!(%error, "session relocation recovery failed before TTL cleanup");
            return;
        }
        let sessions_root = root.join("sessions");
        let relocation_view = match storage_view(&sessions_root) {
            Ok(view) => view,
            Err(error) => {
                tracing::error!(%error, "session relocation snapshot failed before TTL cleanup");
                return;
            }
        };

        tracing::info!(
            target: "xai_grok_shell::session::persistence",
            sessions_root = %sessions_root.display(),
            ttl_days,
            skip = ?skip_session_dir.map(|p| p.display().to_string()),
            "SESSION_CLEANUP_START: scanning for stale session files"
        );

        let stats = cleanup_stale_sessions_inner(
            &sessions_root,
            ttl_days,
            skip_session_dir,
            &relocation_view,
            &root,
            CleanupLevel::SessionsRoot,
        );

        tracing::info!(
            target: "xai_grok_shell::session::persistence",
            sessions_root = %sessions_root.display(),
            files_deleted = stats.files_deleted,
            dirs_removed = stats.dirs_removed,
            errors = stats.errors,
            "SESSION_CLEANUP_DONE"
        );
    });
}

/// Resolve TTL from config.toml `[storage] cleanup_ttl_days`, falling back to 30.
fn resolve_cleanup_ttl_days() -> u32 {
    // Try to load config and read [storage] section
    if let Ok(layers) = crate::config::ConfigLayers::load() {
        let effective = layers.effective_config_disk_only();
        if let Some(storage) = effective.get("storage")
            && let Some(ttl) = storage.get("cleanup_ttl_days")
            && let Some(days) = ttl.as_integer()
            && days > 0
        {
            return days as u32;
        }
    }
    DEFAULT_CLEANUP_TTL_DAYS
}

#[derive(Default)]
struct CleanupStats {
    files_deleted: u32,
    dirs_removed: u32,
    errors: u32,
}

#[derive(Clone, Copy)]
enum CleanupLevel {
    SessionsRoot,
    Cwd,
    Session,
}

/// Recursive cleanup: delete stale files, then rmdir empty dirs (post-order).
fn cleanup_stale_sessions_inner(
    root: &Path,
    ttl_days: u32,
    skip: Option<&Path>,
    relocation_view: &crate::session::storage::relocation::RelocationView,
    grok_home: &Path,
    level: CleanupLevel,
) -> CleanupStats {
    let mut stats = CleanupStats::default();

    if has_unpublished_session_marker(root) {
        return stats;
    }

    if root
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
    {
        return stats;
    }
    if let Some(skip_dir) = skip
        && root == skip_dir
    {
        return stats;
    }

    let Ok(entries) = std::fs::read_dir(root) else {
        return stats;
    };

    for entry_result in entries {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(
                    target: "xai_grok_shell::session::persistence",
                    error = %e,
                    "SESSION_CLEANUP_READ_ERROR"
                );
                stats.errors += 1;
                continue;
            }
        };
        let path = entry.path();

        if let Some(skip_dir) = skip
            && path == skip_dir
        {
            continue;
        }

        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            if matches!(level, CleanupLevel::SessionsRoot)
                && relocation_view.protects_cwd_dir(&path)
            {
                continue;
            }
            let lease = if matches!(level, CleanupLevel::Cwd) {
                let summary = path.join("summary.json");
                let summary_type = match std::fs::symlink_metadata(&summary) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        let child_stats = cleanup_stale_sessions_inner(
                            &path,
                            ttl_days,
                            skip,
                            relocation_view,
                            grok_home,
                            CleanupLevel::Session,
                        );
                        stats.files_deleted += child_stats.files_deleted;
                        stats.dirs_removed += child_stats.dirs_removed;
                        stats.errors += child_stats.errors;
                        if child_stats.files_deleted > 0 && std::fs::remove_dir(&path).is_ok() {
                            stats.dirs_removed += 1;
                        }
                        continue;
                    }
                    Err(error) => {
                        stats.errors += 1;
                        tracing::debug!(
                            target: "xai_grok_shell::session::persistence",
                            path = %summary.display(),
                            %error,
                            "SESSION_CLEANUP_METADATA_ERROR"
                        );
                        continue;
                    }
                };
                if !summary_type.file_type().is_file() || summary_type.file_type().is_symlink() {
                    continue;
                }
                let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let storage = crate::session::storage::relocation::RelocationStorage::new(
                    grok_home.to_path_buf(),
                );
                let Ok(lease) = storage.acquire(id) else {
                    continue;
                };
                match storage.read_journal(id) {
                    Err(crate::session::storage::relocation::RelocationError::JournalMissing(
                        _,
                    )) => Some(lease),
                    _ => continue,
                }
            } else {
                None
            };
            let next = match level {
                CleanupLevel::SessionsRoot => CleanupLevel::Cwd,
                CleanupLevel::Cwd | CleanupLevel::Session => CleanupLevel::Session,
            };
            let child_stats = cleanup_stale_sessions_inner(
                &path,
                ttl_days,
                skip,
                relocation_view,
                grok_home,
                next,
            );
            stats.files_deleted += child_stats.files_deleted;
            stats.dirs_removed += child_stats.dirs_removed;
            stats.errors += child_stats.errors;

            // Only attempt remove_dir if this subtree actually had stale
            // files deleted in this pass. Otherwise we risk removing dirs
            // that were deliberately created for use by concurrent sessions.
            if child_stats.files_deleted > 0 && std::fs::remove_dir(&path).is_ok() {
                stats.dirs_removed += 1;
                tracing::debug!(
                    target: "xai_grok_shell::session::persistence",
                    dir = %path.display(),
                    "SESSION_CLEANUP_RMDIR"
                );
            }
            drop(lease);
        } else if let Ok(mtime) = metadata.modified()
            && is_stale(mtime, ttl_days)
        {
            if std::fs::remove_file(&path).is_ok() {
                stats.files_deleted += 1;
                tracing::debug!(
                    target: "xai_grok_shell::session::persistence",
                    file = %path.display(),
                    "SESSION_CLEANUP_DELETE"
                );
            } else {
                stats.errors += 1;
            }
        }
    }

    stats
}

fn is_stale(mtime: std::time::SystemTime, ttl_days: u32) -> bool {
    let ttl = std::time::Duration::from_secs(u64::from(ttl_days) * 86400);
    mtime.elapsed().is_ok_and(|age| age > ttl)
}

#[cfg(test)]
mod agent_name_persistence_tests {
    use super::*;

    #[test]
    fn summary_round_trips_agent_name_through_json() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.agent_name = Some("cursor".into());

        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: Summary = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.agent_name.as_deref(), Some("cursor"));
    }

    #[test]
    fn summary_deserializes_without_agent_name_backward_compat() {
        // Simulate an old summary.json that lacks agent_name — must still
        // deserialize successfully (serde default → None).
        let json = r#"{
            "info": { "id": "old-session", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "num_chat_messages": 0,
            "current_model_id": "test-model"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert!(
            summary.agent_name.is_none(),
            "old summaries without agent_name should deserialize as None"
        );
    }

    #[test]
    fn summary_skips_none_agent_name_in_serialized_json() {
        let summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        let json = serde_json::to_string(&summary).unwrap();
        assert!(
            !json.contains("agent_name"),
            "None agent_name should not appear in serialized JSON"
        );
    }

    #[test]
    fn summary_includes_agent_name_when_set() {
        let mut summary = Summary::new(
            &Info {
                id: acp::SessionId::new("test"),
                cwd: "/tmp".into(),
            },
            default_model_id(),
        )
        .unwrap();
        summary.agent_name = Some("cursor".into());
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("agent_name"));
        assert!(json.contains("cursor"));
    }

    #[test]
    fn summary_round_trips_various_agent_names() {
        for name in [
            "cursor",
            "grok-build",
            "grok-build-plan",
            "codex",
            "browser-use",
        ] {
            let mut summary = Summary::new(
                &Info {
                    id: acp::SessionId::new("test"),
                    cwd: "/tmp".into(),
                },
                default_model_id(),
            )
            .unwrap();
            summary.agent_name = Some(name.into());

            let json = serde_json::to_string(&summary).unwrap();
            let deserialized: Summary = serde_json::from_str(&json).unwrap();
            assert_eq!(
                deserialized.agent_name.as_deref(),
                Some(name),
                "round-trip failed for agent_name={name}"
            );
        }
    }

    #[test]
    fn summary_with_agent_name_in_full_json() {
        // Verify agent_name deserializes correctly alongside all other fields.
        let json = r#"{
            "info": { "id": "full-session", "cwd": "/tmp" },
            "session_summary": "test session",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 10,
            "num_chat_messages": 5,
            "current_model_id": "cursor-model",
            "agent_name": "cursor",
            "generated_title": "Fix editor mode",
            "head_branch": "main"
        }"#;
        let summary: Summary = serde_json::from_str(json).unwrap();
        assert_eq!(summary.agent_name.as_deref(), Some("cursor"));
        assert_eq!(summary.current_model_id.0.as_ref(), "cursor-model");
        assert_eq!(summary.generated_title.as_deref(), Some("Fix editor mode"));
    }
}

#[cfg(test)]
mod collect_session_files_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn collects_top_level_files_with_flat_names() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("chat_history.jsonl"), b"line1\nline2").unwrap();
        fs::write(dir.path().join("summary.json"), b"{}").unwrap();

        let mut files = Vec::new();
        collect_session_files_recursive(dir.path(), dir.path(), &mut files);

        files.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "chat_history.jsonl");
        assert_eq!(files[0].data, b"line1\nline2");
        assert_eq!(files[1].name, "summary.json");
        assert_eq!(files[1].data, b"{}");
    }

    #[test]
    fn collects_subdirectory_files_with_relative_paths() {
        let dir = TempDir::new().unwrap();
        let prompts_dir = dir.path().join("prompts");
        fs::create_dir(&prompts_dir).unwrap();
        fs::write(prompts_dir.join("prompt_0.txt"), b"long prompt content").unwrap();
        fs::write(prompts_dir.join("prompt_1.txt"), b"another long prompt").unwrap();
        fs::write(dir.path().join("summary.json"), b"{}").unwrap();

        let mut files = Vec::new();
        collect_session_files_recursive(dir.path(), dir.path(), &mut files);

        files.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].name, "prompts/prompt_0.txt");
        assert_eq!(files[0].data, b"long prompt content");
        assert_eq!(files[1].name, "prompts/prompt_1.txt");
        assert_eq!(files[2].name, "summary.json");
    }

    #[test]
    fn collects_nested_subdirectories() {
        let dir = TempDir::new().unwrap();
        let deep = dir.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.txt"), b"deep").unwrap();
        fs::write(dir.path().join("top.txt"), b"top").unwrap();

        let mut files = Vec::new();
        collect_session_files_recursive(dir.path(), dir.path(), &mut files);

        files.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "a/b/deep.txt");
        assert_eq!(files[1].name, "top.txt");
    }

    #[test]
    fn nonexistent_directory_returns_empty() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does_not_exist");

        let mut files = Vec::new();
        collect_session_files_recursive(&missing, &missing, &mut files);

        assert!(files.is_empty());
    }

    #[test]
    fn empty_directory_returns_empty() {
        let dir = TempDir::new().unwrap();

        let mut files = Vec::new();
        collect_session_files_recursive(dir.path(), dir.path(), &mut files);

        assert!(files.is_empty());
    }

    #[test]
    fn skips_empty_subdirectories() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("empty_subdir")).unwrap();
        fs::write(dir.path().join("file.txt"), b"data").unwrap();

        let mut files = Vec::new();
        collect_session_files_recursive(dir.path(), dir.path(), &mut files);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "file.txt");
    }
}

#[cfg(test)]
mod session_exists_tests {
    use super::session_exists_in_root;
    use std::fs;
    use tempfile::TempDir;

    fn make_root() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn returns_false_when_root_does_not_exist() {
        let root = std::path::PathBuf::from("/nonexistent/grok/sessions");
        assert!(!session_exists_in_root("any-id", &root));
    }

    #[test]
    fn returns_false_when_root_is_empty() {
        let tmp = make_root();
        let root = tmp.path().join("sessions");
        fs::create_dir_all(&root).unwrap();
        assert!(!session_exists_in_root("my-session", &root));
    }

    #[test]
    fn returns_true_when_session_dir_exists_under_any_cwd() {
        let tmp = make_root();
        let root = tmp.path().join("sessions");
        // Simulate sessions/<encoded-cwd>/<session-id>/
        let session_dir = root.join("some_cwd_dir").join("my-session-id");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("summary.json"), b"{}").unwrap();

        assert!(session_exists_in_root("my-session-id", &root));
    }

    #[test]
    fn returns_false_when_session_id_is_a_file_not_a_dir() {
        let tmp = make_root();
        let root = tmp.path().join("sessions");
        let cwd_dir = root.join("some_cwd_dir");
        fs::create_dir_all(&cwd_dir).unwrap();
        // Create a file instead of a directory with the session id name
        fs::write(cwd_dir.join("my-session-id"), b"").unwrap();

        assert!(!session_exists_in_root("my-session-id", &root));
    }

    #[test]
    fn returns_false_for_different_session_id() {
        let tmp = make_root();
        let root = tmp.path().join("sessions");
        let session_dir = root.join("some_cwd_dir").join("session-a");
        fs::create_dir_all(&session_dir).unwrap();

        assert!(!session_exists_in_root("session-b", &root));
    }

    #[test]
    fn finds_session_across_multiple_cwd_dirs() {
        let tmp = make_root();
        let root = tmp.path().join("sessions");
        // Two persisted sessions under different cwd directories.
        let other = root.join("cwd1").join("other-session");
        let target = root.join("cwd2").join("target-session");
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(other.join("summary.json"), b"{}").unwrap();
        fs::write(target.join("summary.json"), b"{}").unwrap();

        assert!(session_exists_in_root("target-session", &root));
        assert!(!session_exists_in_root("missing-session", &root));
    }
}

#[cfg(test)]
mod find_summary_by_session_id_tests {
    use super::find_summary_by_session_id_in_root;
    use std::fs;
    use tempfile::TempDir;

    fn write_summary(root: &std::path::Path, cwd_dir: &str, session_id: &str, json: &str) {
        let dir = root.join(cwd_dir).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("summary.json"), json).unwrap();
    }

    fn minimal_summary(head_commit: &str, head_branch: &str) -> String {
        serde_json::json!({
            "info": { "id": "test-session", "cwd": "/tmp" },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "current_model_id": "grok-3",
            "head_commit": head_commit,
            "head_branch": head_branch
        })
        .to_string()
    }

    #[test]
    fn returns_none_when_root_missing() {
        let result =
            find_summary_by_session_id_in_root("any", &std::path::PathBuf::from("/nonexistent"));
        assert!(result.is_none());
    }

    #[test]
    fn returns_none_when_no_matching_session() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        write_summary(&root, "cwd1", "other-id", &minimal_summary("abc", "main"));
        assert!(find_summary_by_session_id_in_root("missing-id", &root).is_none());
    }

    #[test]
    fn finds_summary_across_cwd_dirs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        write_summary(
            &root,
            "encoded_cwd",
            "target-session",
            &minimal_summary("deadbeef", "feature/x"),
        );

        let found = find_summary_by_session_id_in_root("target-session", &root).unwrap();
        assert_eq!(found.head_commit.as_deref(), Some("deadbeef"));
        assert_eq!(found.head_branch.as_deref(), Some("feature/x"));
    }

    #[test]
    fn skips_malformed_summary() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        // Write invalid JSON
        let dir = root.join("cwd1").join("bad-session");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("summary.json"), b"not-json").unwrap();

        assert!(find_summary_by_session_id_in_root("bad-session", &root).is_none());
    }
}

#[cfg(test)]
mod resumed_sandbox_profile_tests {
    use super::{
        RelocationError, RelocationView, most_recent_local_summary_for_cwd_in_root,
        most_recent_local_summary_for_cwd_in_view, read_summary_from_dir,
        resumed_session_sandbox_profile_in_root,
    };
    use std::{fs, io};
    use tempfile::TempDir;

    /// Write a session summary under the *encoded* cwd dir (matching how the
    /// resume helpers locate sessions). `sandbox_profile` is included only when
    /// `Some`, mirroring older summaries that predate the field.
    fn write_session(
        root: &std::path::Path,
        cwd: &str,
        session_id: &str,
        updated_at: &str,
        last_active_at: Option<&str>,
        sandbox_profile: Option<&str>,
        hidden: bool,
    ) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        let mut summary = serde_json::json!({
            "info": { "id": session_id, "cwd": cwd },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": updated_at,
            "num_messages": 0,
            "current_model_id": "grok-3",
        });
        if let Some(la) = last_active_at {
            summary["last_active_at"] = serde_json::Value::String(la.to_string());
        }
        if let Some(profile) = sandbox_profile {
            summary["sandbox_profile"] = serde_json::Value::String(profile.to_string());
        }
        if hidden {
            summary["hidden"] = serde_json::Value::Bool(true);
        }
        fs::write(dir.join("summary.json"), summary.to_string()).unwrap();
    }

    #[test]
    fn explicit_id_returns_persisted_profile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        write_session(
            &root,
            "/work/a",
            "sess-1",
            "2026-01-01T00:00:00Z",
            None,
            Some("strict"),
            false,
        );

        assert_eq!(
            resumed_session_sandbox_profile_in_root(Some("sess-1"), None, &root),
            Some("strict".to_string())
        );
    }

    #[test]
    fn explicit_id_without_persisted_profile_is_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        // Older session, created before the field existed.
        write_session(
            &root,
            "/work/a",
            "sess-old",
            "2026-01-01T00:00:00Z",
            None,
            None,
            false,
        );

        assert_eq!(
            resumed_session_sandbox_profile_in_root(Some("sess-old"), None, &root),
            None
        );
    }

    #[test]
    fn explicit_remote_id_resolves_local_child_profile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/remote";
        // A remote session restored into a local child: the child has a fresh
        // id and records `parent_session_id` = the remote id.
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join("local-child");
        fs::create_dir_all(&dir).unwrap();
        let summary = serde_json::json!({
            "info": { "id": "local-child", "cwd": cwd },
            "session_summary": "",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "num_messages": 0,
            "current_model_id": "grok-3",
            "parent_session_id": "remote-xyz",
            "sandbox_profile": "workspace",
        });
        fs::write(dir.join("summary.json"), summary.to_string()).unwrap();

        // No session dir is named "remote-xyz"; resolve via the child (cwd-scoped).
        assert_eq!(
            resumed_session_sandbox_profile_in_root(Some("remote-xyz"), Some(cwd), &root),
            Some("workspace".to_string())
        );
        // Without a cwd the child can't be located -> None.
        assert_eq!(
            resumed_session_sandbox_profile_in_root(Some("remote-xyz"), None, &root),
            None
        );
    }

    #[test]
    fn empty_or_missing_id_and_no_cwd_is_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        assert_eq!(
            resumed_session_sandbox_profile_in_root(Some(""), None, &root),
            None
        );
        assert_eq!(
            resumed_session_sandbox_profile_in_root(None, None, &root),
            None
        );
    }

    #[test]
    fn most_recent_cwd_picks_latest_session_profile() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/proj";
        write_session(
            &root,
            cwd,
            "older",
            "2026-01-01T00:00:00Z",
            None,
            Some("workspace"),
            false,
        );
        write_session(
            &root,
            cwd,
            "newer",
            "2026-06-01T00:00:00Z",
            None,
            Some("off"),
            false,
        );

        assert_eq!(
            most_recent_local_summary_for_cwd_in_root(cwd, &root)
                .unwrap()
                .info
                .id
                .0
                .to_string(),
            "newer"
        );
        assert_eq!(
            resumed_session_sandbox_profile_in_root(None, Some(cwd), &root),
            Some("off".to_string())
        );
    }

    #[test]
    fn most_recent_cwd_skips_corrupt_summary() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/proj";
        write_session(
            &root,
            cwd,
            "valid",
            "2026-06-01T00:00:00Z",
            None,
            Some("workspace"),
            false,
        );
        let corrupt_dir = root
            .join(crate::util::grok_home::encode_cwd_dirname(cwd))
            .join("corrupt");
        fs::create_dir_all(&corrupt_dir).unwrap();
        fs::write(corrupt_dir.join("summary.json"), b"not-json").unwrap();

        let picked = most_recent_local_summary_for_cwd_in_root(cwd, &root).unwrap();
        assert_eq!(picked.info.id.0.as_ref(), "valid");
    }

    #[test]
    fn most_recent_cwd_skips_raced_not_found() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/proj";
        write_session(
            &root,
            cwd,
            "valid",
            "2026-06-01T00:00:00Z",
            None,
            Some("workspace"),
            false,
        );
        write_session(
            &root,
            cwd,
            "removed",
            "2026-07-01T00:00:00Z",
            None,
            Some("strict"),
            false,
        );
        let view = RelocationView::load_for_sessions_root(&root).unwrap();

        let picked = most_recent_local_summary_for_cwd_in_view(cwd, &view, |session_dir| {
            if session_dir.ends_with("removed") {
                Err(RelocationError::Io {
                    operation: "read",
                    path: session_dir.join("summary.json"),
                    source: io::Error::new(io::ErrorKind::NotFound, "injected"),
                })
            } else {
                read_summary_from_dir(session_dir)
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(picked.info.id.0.as_ref(), "valid");
    }

    #[test]
    fn most_recent_cwd_propagates_non_not_found_io_errors() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/proj";
        write_session(
            &root,
            cwd,
            "older",
            "2026-01-01T00:00:00Z",
            None,
            Some("workspace"),
            false,
        );
        write_session(
            &root,
            cwd,
            "unreadable-newer",
            "2026-06-01T00:00:00Z",
            None,
            Some("strict"),
            false,
        );
        let view = RelocationView::load_for_sessions_root(&root).unwrap();

        let error = most_recent_local_summary_for_cwd_in_view(cwd, &view, |session_dir| {
            if session_dir.ends_with("unreadable-newer") {
                Err(RelocationError::Io {
                    operation: "read",
                    path: session_dir.join("summary.json"),
                    source: io::Error::new(io::ErrorKind::PermissionDenied, "injected"),
                })
            } else {
                read_summary_from_dir(session_dir)
            }
        })
        .unwrap_err();
        assert!(matches!(
            error,
            RelocationError::Io { source, .. }
                if source.kind() == io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn most_recent_cwd_prefers_last_active_at_over_updated_at() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/proj";
        write_session(
            &root,
            cwd,
            "recent_activity",
            "2026-02-01T00:00:00Z",
            Some("2026-05-01T00:00:00Z"),
            Some("workspace"),
            false,
        );
        write_session(
            &root,
            cwd,
            "stale_activity",
            "2026-04-01T00:00:00Z",
            Some("2026-01-01T00:00:00Z"),
            Some("off"),
            false,
        );

        let picked = most_recent_local_summary_for_cwd_in_root(cwd, &root).unwrap();
        assert_eq!(picked.info.id.0.as_ref(), "recent_activity");
        assert_eq!(
            resumed_session_sandbox_profile_in_root(None, Some(cwd), &root),
            Some("workspace".to_string())
        );
    }

    #[test]
    fn most_recent_cwd_skips_hidden_session() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work/proj";
        // Older, visible session.
        write_session(
            &root,
            cwd,
            "visible",
            "2026-01-01T00:00:00Z",
            None,
            Some("workspace"),
            false,
        );
        // Newer, hidden (e.g. subagent) session — the most-recent peek must
        // ignore it, matching what `list_sessions` resumes.
        write_session(
            &root,
            cwd,
            "hidden-newer",
            "2026-06-01T00:00:00Z",
            None,
            Some("off"),
            true,
        );

        assert_eq!(
            most_recent_local_summary_for_cwd_in_root(cwd, &root)
                .unwrap()
                .info
                .id
                .0
                .to_string(),
            "visible"
        );
        assert_eq!(
            resumed_session_sandbox_profile_in_root(None, Some(cwd), &root),
            Some("workspace".to_string())
        );
    }

    #[test]
    fn most_recent_cwd_with_no_sessions_is_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        assert_eq!(
            resumed_session_sandbox_profile_in_root(None, Some("/empty/cwd"), &root),
            None
        );
    }
}

#[cfg(test)]
mod session_exists_for_cwd_tests {
    use super::{
        UNPUBLISHED_SESSION_MARKER, resolve_local_session_any_cwd_in_root,
        session_exists_for_cwd_in_root, session_exists_in_root,
    };
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn returns_true_when_session_exists_under_matching_cwd() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/alpha";
        let session_id = "my-session";

        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("summary.json"), b"{}").unwrap();

        assert!(session_exists_for_cwd_in_root(session_id, cwd, &root));
    }

    #[test]
    fn unpublished_session_is_hidden_from_direct_and_all_cwd_resolution() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/pending";
        let session_id = "pending-session";
        let dir = root
            .join(crate::util::grok_home::encode_cwd_dirname(cwd))
            .join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(UNPUBLISHED_SESSION_MARKER), b"").unwrap();
        fs::write(dir.join("summary.json"), b"{}").unwrap();

        assert!(!session_exists_for_cwd_in_root(session_id, cwd, &root));
        assert!(!session_exists_in_root(session_id, &root));
        assert_eq!(
            resolve_local_session_any_cwd_in_root(session_id, &root).unwrap(),
            None
        );
    }

    #[test]
    fn returns_false_when_session_absent_under_cwd() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        fs::create_dir_all(&root).unwrap();

        assert!(!session_exists_for_cwd_in_root(
            "missing",
            "/project/alpha",
            &root
        ));
    }

    /// Regression test for the cross-cwd false-positive.
    ///
    /// Before the fix, `restore_if_not_local` used `session_exists_by_id` which
    /// scanned ALL cwd directories.  A session present only under cwd-A would cause
    /// it to skip remote restore when the user resumed from cwd-B — then the
    /// `LoadSession` call would fail because the session directory did not exist
    /// under cwd-B.
    ///
    /// The cwd-specific check (`session_exists_for_cwd`) must return `false` for
    /// cwd-B even when the global scan returns `true` (because it finds cwd-A).
    #[test]
    fn session_under_different_cwd_is_not_considered_present() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let session_id = "cross-cwd-session";

        // Create the session only under cwd-A (a real session has a summary.json).
        let encoded_a = crate::util::grok_home::encode_cwd_dirname("/project/alpha");
        let dir_a = root.join(&encoded_a).join(session_id);
        fs::create_dir_all(&dir_a).unwrap();
        fs::write(dir_a.join("summary.json"), b"{}").unwrap();

        // Global scan (old behaviour) finds it — this is the incorrect check
        assert!(
            session_exists_in_root(session_id, &root),
            "global scan must find the session under cwd-A"
        );

        // Cwd-specific check must return false for cwd-B
        assert!(
            !session_exists_for_cwd_in_root(session_id, "/project/beta", &root),
            "cwd-specific check must return false for cwd-B; remote restore must not be skipped"
        );

        // And true for cwd-A (sanity)
        assert!(
            session_exists_for_cwd_in_root(session_id, "/project/alpha", &root),
            "cwd-specific check must return true for the matching cwd-A"
        );
    }

    /// An `images/`-only stub (no `summary.json`) is not a resumable session.
    #[test]
    fn images_only_stub_is_not_a_session() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/alpha";
        let session_id = "stub-session";

        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let images = root.join(&encoded).join(session_id).join("images");
        fs::create_dir_all(&images).unwrap();
        fs::write(images.join("image-1.png"), b"png").unwrap();

        assert!(
            !session_exists_for_cwd_in_root(session_id, cwd, &root),
            "an images-only stub (no summary.json) must not be a resumable session"
        );
    }

    /// The all-cwd scan skips a stub and returns the real session's cwd.
    #[test]
    fn resolve_local_session_any_cwd_skips_stub_and_finds_real() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let session_id = "real-session";

        // Real session under cwd-A.
        let cwd_a = "/project/alpha";
        let encoded_a = crate::util::grok_home::encode_cwd_dirname(cwd_a);
        let dir_a = root.join(&encoded_a).join(session_id);
        fs::create_dir_all(&dir_a).unwrap();
        fs::write(dir_a.join("summary.json"), b"{}").unwrap();

        // Images-only stub for the SAME id under cwd-B.
        let cwd_b = "/project/beta";
        let encoded_b = crate::util::grok_home::encode_cwd_dirname(cwd_b);
        let images_b = root.join(&encoded_b).join(session_id).join("images");
        fs::create_dir_all(&images_b).unwrap();
        fs::write(images_b.join("image-1.png"), b"png").unwrap();

        assert_eq!(
            resolve_local_session_any_cwd_in_root(session_id, &root)
                .unwrap()
                .as_deref(),
            Some(cwd_a),
            "must anchor to the real session's cwd, not the stub's"
        );
    }

    #[test]
    fn find_summary_by_session_id_reads_cross_cwd_uuid() {
        use super::find_summary_by_session_id_in_root;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let session_id = "019f870d-6976-7d73-a12a-52e9d4aebcd4";
        let cwd = "/project/elsewhere";
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("summary.json"),
            serde_json::json!({
                "info": { "id": session_id, "cwd": cwd },
                "session_summary": "cross-cwd hit",
                "created_at": "2026-03-01T00:00:00Z",
                "updated_at": "2026-03-01T00:00:00Z",
                "num_messages": 2,
                "num_chat_messages": 1,
                "current_model_id": "test",
            })
            .to_string(),
        )
        .unwrap();

        let summary = find_summary_by_session_id_in_root(session_id, &root)
            .expect("CLI --resume finds this summary by id across cwds");
        assert_eq!(summary.info.id.0.as_ref(), session_id);
        assert_eq!(summary.info.cwd, cwd);
        assert_eq!(summary.session_summary, "cross-cwd hit");
    }
}

#[cfg(test)]
mod find_local_child_tests {
    use super::{UNPUBLISHED_SESSION_MARKER, find_local_child_for_remote_in_root};
    use filetime::{self, FileTime};
    use std::fs;
    use tempfile::TempDir;

    fn make_session_with_parent(
        root: &std::path::Path,
        cwd: &str,
        session_id: &str,
        parent_id: &str,
    ) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        let summary = serde_json::json!({ "parent_session_id": parent_id });
        fs::write(dir.join("summary.json"), summary.to_string()).unwrap();
    }

    #[test]
    fn returns_child_id_when_parent_matches() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        make_session_with_parent(root.as_path(), "/work", "local-child-uuid", "remote-abc");

        let found = find_local_child_for_remote_in_root("remote-abc", "/work", &root);
        assert_eq!(found.as_deref(), Some("local-child-uuid"));
    }

    #[test]
    fn unpublished_restored_child_is_not_resolved() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/work";
        let child_id = "pending-local-child";
        make_session_with_parent(root.as_path(), cwd, child_id, "remote-abc");
        let dir = root
            .join(crate::util::grok_home::encode_cwd_dirname(cwd))
            .join(child_id);
        fs::write(dir.join(UNPUBLISHED_SESSION_MARKER), b"").unwrap();

        assert!(find_local_child_for_remote_in_root("remote-abc", cwd, &root).is_none());
    }

    #[test]
    fn returns_none_when_no_child_exists() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let encoded = crate::util::grok_home::encode_cwd_dirname("/work");
        fs::create_dir_all(root.join(&encoded)).unwrap();

        let found = find_local_child_for_remote_in_root("remote-abc", "/work", &root);
        assert!(found.is_none());
    }

    #[test]
    fn returns_none_for_different_parent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        make_session_with_parent(root.as_path(), "/work", "local-child-uuid", "remote-xyz");

        let found = find_local_child_for_remote_in_root("remote-abc", "/work", &root);
        assert!(found.is_none());
    }

    /// Regression: a second `grok -r <remote_id>` must return the existing child
    /// without creating a new restore, not return `None`.
    #[test]
    fn repeated_resume_returns_existing_child() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        make_session_with_parent(root.as_path(), "/project", "child-1", "remote-parent");

        let first = find_local_child_for_remote_in_root("remote-parent", "/project", &root);
        let second = find_local_child_for_remote_in_root("remote-parent", "/project", &root);
        assert_eq!(first, second);
        assert_eq!(first.as_deref(), Some("child-1"));
    }

    /// With multiple pre-existing children, the function must return the newest
    /// one deterministically rather than picking an arbitrary filesystem order.
    #[test]
    fn duplicate_children_returns_newest_by_updated_at() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project";
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);

        // Older child — earlier timestamp.
        let old_dir = root.join(&encoded).join("old-child");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(
            old_dir.join("summary.json"),
            r#"{"parent_session_id":"remote-parent","updated_at":"2026-01-01T10:00:00Z"}"#,
        )
        .unwrap();

        // Newer child — later timestamp.
        let new_dir = root.join(&encoded).join("new-child");
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(
            new_dir.join("summary.json"),
            r#"{"parent_session_id":"remote-parent","updated_at":"2026-06-01T10:00:00Z"}"#,
        )
        .unwrap();

        let found = find_local_child_for_remote_in_root("remote-parent", cwd, &root);
        assert_eq!(
            found.as_deref(),
            Some("new-child"),
            "must return the newest child by updated_at"
        );
    }

    /// When two children share the same `updated_at` the tie must be broken
    /// deterministically, not by filesystem enumeration order.
    /// The lexicographically largest session id is the final stable tie-breaker.
    #[test]
    fn duplicate_children_equal_timestamps_stable_tiebreak() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project-tie";
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let same_ts = "2026-03-15T12:00:00Z";

        let mut dirs = Vec::new();
        for name in ["aaaa-uuid", "zzzz-uuid", "mmmm-uuid"] {
            let dir = root.join(&encoded).join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("summary.json"),
                format!(r#"{{"parent_session_id":"remote-tie","updated_at":"{same_ts}"}}"#),
            )
            .unwrap();
            dirs.push(dir);
        }

        // Force all directories to have *exactly* the same mtime so the
        // lexicographic session_id comparison is the actual tie-breaker.
        // Without this, nanosecond-precision filesystem mtimes can differ.
        let fixed_mtime = FileTime::from_unix_time(1700000000, 0);
        for dir in &dirs {
            filetime::set_file_mtime(dir, fixed_mtime).unwrap();
        }

        let found = find_local_child_for_remote_in_root("remote-tie", cwd, &root);
        // All share the same updated_at and mtime.
        // The lexicographic tie-breaker must always pick "zzzz-uuid".
        assert_eq!(
            found.as_deref(),
            Some("zzzz-uuid"),
            "lexicographically largest id must win the three-way tie"
        );
    }
}

#[cfg(test)]
mod resolve_local_session_tests {
    use super::{find_local_child_for_remote_in_root, session_exists_for_cwd_in_root};
    use std::fs;
    use tempfile::TempDir;

    // resolve_local_session delegates to the same _in_root helpers tested above,
    // so we test the composition logic via the public function indirectly by
    // setting up the on-disk structures under a fake grok home.
    // For unit isolation, we test the equivalent logic via the inner helpers.

    fn setup_session(root: &std::path::Path, cwd: &str, session_id: &str) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("summary.json"), b"{}").unwrap();
    }

    fn setup_child_session(root: &std::path::Path, cwd: &str, child_id: &str, parent_id: &str) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(child_id);
        fs::create_dir_all(&dir).unwrap();
        let summary = serde_json::json!({ "parent_session_id": parent_id });
        fs::write(dir.join("summary.json"), summary.to_string()).unwrap();
    }

    #[test]
    fn exact_match_returns_original_id() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/alpha";
        let sid = "sess-123";

        setup_session(&root, cwd, sid);

        // Exact match: session_exists_for_cwd → true
        assert!(session_exists_for_cwd_in_root(sid, cwd, &root));
        // The composed function should return the original id.
        // (We can't call resolve_local_session directly because it uses grok_home(),
        //  but the logic is: if session_exists → Some(session_id.to_string()),
        //  else find_local_child → child_id. Tested via inner helpers.)
        assert_eq!(
            Some(sid.to_string()),
            if session_exists_for_cwd_in_root(sid, cwd, &root) {
                Some(sid.to_string())
            } else {
                find_local_child_for_remote_in_root(sid, cwd, &root)
            }
        );
    }

    #[test]
    fn child_match_returns_child_id() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/beta";
        let remote_id = "remote-abc";
        let child_id = "local-child-xyz";

        setup_child_session(&root, cwd, child_id, remote_id);

        assert!(!session_exists_for_cwd_in_root(remote_id, cwd, &root));
        assert_eq!(
            Some(child_id.to_string()),
            find_local_child_for_remote_in_root(remote_id, cwd, &root)
        );
    }

    #[test]
    fn no_match_returns_none() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/gamma";
        fs::create_dir_all(root.join(crate::util::grok_home::encode_cwd_dirname(cwd))).unwrap();

        assert!(!session_exists_for_cwd_in_root("missing", cwd, &root));
        assert_eq!(
            None,
            find_local_child_for_remote_in_root("missing", cwd, &root)
        );
    }

    #[test]
    fn exact_match_takes_priority_over_child() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("sessions");
        let cwd = "/project/delta";
        let sid = "sess-both";

        // Create both an exact session and a child of the same remote id.
        setup_session(&root, cwd, sid);
        setup_child_session(&root, cwd, "local-child-from-same", sid);

        // Exact match should take priority.
        assert!(session_exists_for_cwd_in_root(sid, cwd, &root));
    }
}

#[cfg(test)]
mod repo_wide_resolution_tests {
    use super::*;
    use std::fs;

    fn setup_session(root: &Path, cwd: &str, session_id: &str) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("summary.json"), b"{}").unwrap();
    }

    fn setup_child_session(root: &Path, cwd: &str, child_id: &str, parent_id: &str) {
        let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
        let dir = root.join(&encoded).join(child_id);
        fs::create_dir_all(&dir).unwrap();
        let summary = format!(
            r#"{{"session_id":"{child_id}","parent_session_id":"{parent_id}","updated_at":"2024-01-01T00:00:00Z"}}"#
        );
        fs::write(dir.join("summary.json"), summary).unwrap();
    }

    #[test]
    fn exact_cwd_takes_priority_over_same_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";
        let other_cwd = "/repo/worktree-1";

        setup_session(&root, exact_cwd, "sess-A");
        setup_session(&root, other_cwd, "sess-A");

        let result =
            resolve_local_session_for_repo_in_root("sess-A", &[exact_cwd, other_cwd], &root);
        let r = result.unwrap();
        assert_eq!(r.session_id, "sess-A");
        assert_eq!(r.cwd, exact_cwd);
        assert_eq!(r.resolution_kind, LocalSessionResolutionKind::ExactCwd);
    }

    /// An `images/`-only stub in the exact cwd is skipped; resolution anchors to
    /// the real session in a sibling cwd. Mirrors the cross-dir resume bug.
    #[test]
    fn skips_images_only_stub_and_resolves_real_sibling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";
        let sibling_cwd = "/repo/worktree-1";

        let encoded = crate::util::grok_home::encode_cwd_dirname(exact_cwd);
        let images = root.join(&encoded).join("sess-A").join("images");
        fs::create_dir_all(&images).unwrap();
        fs::write(images.join("image-1.png"), b"png").unwrap();
        setup_session(&root, sibling_cwd, "sess-A");

        let result =
            resolve_local_session_for_repo_in_root("sess-A", &[exact_cwd, sibling_cwd], &root);
        let r = result.expect("must skip the stub and find the real sibling session");
        assert_eq!(r.cwd, sibling_cwd);
        assert_eq!(
            r.resolution_kind,
            LocalSessionResolutionKind::SameRepoDifferentCwd
        );
    }

    #[test]
    fn falls_back_to_same_repo_cwd_when_not_in_exact() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";
        let other_cwd = "/repo/worktree-1";

        // Session only exists in other_cwd
        setup_session(&root, other_cwd, "sess-B");

        let result =
            resolve_local_session_for_repo_in_root("sess-B", &[exact_cwd, other_cwd], &root);
        let r = result.unwrap();
        assert_eq!(r.session_id, "sess-B");
        assert_eq!(r.cwd, other_cwd);
        assert_eq!(
            r.resolution_kind,
            LocalSessionResolutionKind::SameRepoDifferentCwd
        );
    }

    #[test]
    fn finds_restored_child_in_exact_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";

        setup_child_session(&root, exact_cwd, "local-child", "remote-sess");

        let result = resolve_local_session_for_repo_in_root("remote-sess", &[exact_cwd], &root);
        let r = result.unwrap();
        assert_eq!(r.session_id, "local-child");
        assert_eq!(r.cwd, exact_cwd);
        assert_eq!(
            r.resolution_kind,
            LocalSessionResolutionKind::RestoredChildInExactCwd
        );
    }

    #[test]
    fn finds_restored_child_in_same_repo_different_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";
        let other_cwd = "/repo/worktree-2";

        // Restored child only in other_cwd
        setup_child_session(&root, other_cwd, "restored-child", "remote-sess");

        let result =
            resolve_local_session_for_repo_in_root("remote-sess", &[exact_cwd, other_cwd], &root);
        let r = result.unwrap();
        assert_eq!(r.session_id, "restored-child");
        assert_eq!(r.cwd, other_cwd);
        assert_eq!(
            r.resolution_kind,
            LocalSessionResolutionKind::RestoredChildInSameRepoDifferentCwd
        );
    }

    #[test]
    fn returns_none_when_no_candidate_has_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let result = resolve_local_session_for_repo_in_root(
            "nonexistent",
            &["/cwd-1", "/cwd-2", "/cwd-3"],
            &root,
        );
        assert!(result.is_none());
    }

    #[test]
    fn direct_session_preferred_over_restored_child_in_same_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let cwd = "/repo/main";

        // Both exist: direct session AND a restored child for the same remote
        setup_session(&root, cwd, "sess-X");
        setup_child_session(&root, cwd, "child-of-X", "sess-X");

        let result = resolve_local_session_for_repo_in_root("sess-X", &[cwd], &root);
        let r = result.unwrap();
        // Direct match should win
        assert_eq!(r.session_id, "sess-X");
        assert_eq!(r.resolution_kind, LocalSessionResolutionKind::ExactCwd);
    }

    #[test]
    fn direct_in_later_cwd_preferred_over_child_in_same_later_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let exact_cwd = "/repo/main";
        let other_cwd = "/repo/worktree-1";

        // Nothing in exact_cwd; both direct and child in other_cwd
        setup_session(&root, other_cwd, "sess-Y");
        setup_child_session(&root, other_cwd, "child-of-Y", "sess-Y");

        let result =
            resolve_local_session_for_repo_in_root("sess-Y", &[exact_cwd, other_cwd], &root);
        let r = result.unwrap();
        assert_eq!(r.session_id, "sess-Y");
        assert_eq!(
            r.resolution_kind,
            LocalSessionResolutionKind::SameRepoDifferentCwd
        );
    }

    #[test]
    fn empty_candidates_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();

        let result = resolve_local_session_for_repo_in_root("any-sess", &[], &root);
        assert!(result.is_none());
    }

    #[test]
    fn resolution_kind_serde_round_trip() {
        let kinds = [
            LocalSessionResolutionKind::ExactCwd,
            LocalSessionResolutionKind::RestoredChildInExactCwd,
            LocalSessionResolutionKind::SameRepoDifferentCwd,
            LocalSessionResolutionKind::RestoredChildInSameRepoDifferentCwd,
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let deser: LocalSessionResolutionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, deser);
        }
    }

    #[test]
    fn resolved_local_session_serde_round_trip() {
        let resolved = ResolvedLocalSession {
            session_id: "sess-123".into(),
            cwd: "/repo/main".into(),
            resolution_kind: LocalSessionResolutionKind::SameRepoDifferentCwd,
        };
        let json = serde_json::to_string(&resolved).unwrap();
        let deser: ResolvedLocalSession = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.session_id, "sess-123");
        assert_eq!(deser.cwd, "/repo/main");
        assert_eq!(
            deser.resolution_kind,
            LocalSessionResolutionKind::SameRepoDifferentCwd
        );
    }
}

#[cfg(test)]
mod fresh_session_claim_tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::time::Duration;

    const CHILD_MODE_ENV: &str = "GROK_TEST_SESSION_CLAIM_MODE";
    const CHILD_ROOT_ENV: &str = "GROK_TEST_SESSION_CLAIM_ROOT";
    const CHILD_ID_ENV: &str = "GROK_TEST_SESSION_CLAIM_ID";
    const CHILD_CWD_ENV: &str = "GROK_TEST_SESSION_CLAIM_CWD";
    const CHILD_RELEASE_ENV: &str = "GROK_TEST_SESSION_CLAIM_RELEASE";
    const CHILD_READY: &str = "__GROK_SESSION_CLAIM_READY__";
    const CHILD_RESULT_PREFIX: &str = "__GROK_SESSION_CLAIM_RESULT__=";
    const CHILD_FINISH_TIMEOUT: Duration = Duration::from_secs(15);

    fn test_session_dir(root: &Path, cwd: &str, session_id: &str) -> PathBuf {
        root.join("sessions")
            .join(crate::util::grok_home::encode_cwd_dirname(cwd))
            .join(session_id)
    }

    fn test_info(session_id: &str, cwd: &str) -> Info {
        Info {
            id: acp::SessionId::new(session_id),
            cwd: cwd.to_owned(),
        }
    }

    fn test_sampling_client() -> OaiCompatClient {
        OaiCompatClient::new(xai_grok_sampler::SamplerConfig::default())
            .expect("test sampling client")
    }

    fn write_valid_staged_summary(claim: &FreshSessionClaim, info: &Info) {
        let summary = Summary::new(info, default_model_id()).expect("test summary");
        std::fs::write(
            claim.publication.stage_session.join("summary.json"),
            serde_json::to_vec_pretty(&summary).expect("serialize test summary"),
        )
        .expect("write staged summary");
    }

    #[test]
    fn failed_lease_downgrade_keeps_namespace_exclusive_until_claim_drop() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000144";
        const CWD: &str = "/repo/publication/downgrade-failure";
        let root = tempfile::tempdir().expect("temporary grok home");
        let claim = claim_fresh_session_sync(
            root.path(),
            SESSION_ID,
            test_session_dir(root.path(), CWD, SESSION_ID),
        )
        .expect("fresh claim");

        let result = claim.into_published_lease_with(|lease| {
            lease.transition_exclusive_to_lifetime_shared_with(|_| {
                assert!(
                    try_acquire_session_id_write_lock_sync(root.path(), SESSION_ID)
                        .expect("try competing writer during failed downgrade")
                        .is_none(),
                    "namespace lease must remain exclusive while mutation downgrade fails"
                );
                Err(io::Error::other("injected shared-lock failure"))
            })
        });
        let failure = result.unwrap_err();
        assert_eq!(failure.error.kind(), io::ErrorKind::Other);
        assert!(
            try_acquire_session_id_write_lock_sync(root.path(), SESSION_ID)
                .expect("try writer while failed claim is retained")
                .is_none(),
            "returned failed claim must retain the exclusive namespace lease"
        );
        drop(failure.claim);
        assert!(
            try_acquire_session_id_write_lock_sync(root.path(), SESSION_ID)
                .expect("try writer after retained claim drops")
                .is_some()
        );
    }

    #[test]
    fn failed_namespace_unlock_keeps_namespace_exclusive_until_claim_drop() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000145";
        const CWD: &str = "/repo/publication/namespace-unlock-failure";
        let root = tempfile::tempdir().expect("temporary grok home");
        let claim = claim_fresh_session_sync(
            root.path(),
            SESSION_ID,
            test_session_dir(root.path(), CWD, SESSION_ID),
        )
        .expect("fresh claim");

        let result = claim.into_published_lease_with(|lease| {
            lease.transition_exclusive_to_lifetime_shared_with_unlock(FileExt::lock_shared, |_| {
                Err(io::Error::other("injected namespace-unlock failure"))
            })
        });
        let failure = result.unwrap_err();
        assert_eq!(failure.error.kind(), io::ErrorKind::Other);
        assert!(
            failure
                .claim
                .session_id_lock
                .as_ref()
                .expect("failed claim retains its session-id lock")
                .namespace
                .is_some(),
            "an unlock error must not discard the namespace handle"
        );
        assert!(
            try_acquire_session_id_write_lock_sync(root.path(), SESSION_ID)
                .expect("try writer while failed claim is retained")
                .is_none(),
            "returned failed claim must retain the exclusive namespace lease"
        );
        drop(failure.claim);
        assert!(
            try_acquire_session_id_write_lock_sync(root.path(), SESSION_ID)
                .expect("try writer after retained claim drops")
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_identity_proof_rejects_replaced_root_path() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let moved_root = temp.path().join("moved-root");
        std::fs::create_dir(&root_path).unwrap();
        let root = AnchoredDirectory::open_root(&root_path).unwrap();
        let sessions = root.create_child_dir(OsStr::new("sessions")).unwrap();
        let parent = sessions.create_child_dir(OsStr::new("cwd")).unwrap();
        let session = parent.create_child_dir(OsStr::new("session")).unwrap();
        verify_canonical_publication_identity(
            &root_path,
            &root,
            &sessions,
            OsStr::new("cwd"),
            &parent,
            OsStr::new("session"),
            &session,
        )
        .unwrap();

        std::fs::rename(&root_path, &moved_root).unwrap();
        std::fs::create_dir_all(root_path.join("sessions/cwd/session")).unwrap();
        let error = verify_canonical_publication_identity(
            &root_path,
            &root,
            &sessions,
            OsStr::new("cwd"),
            &parent,
            OsStr::new("session"),
            &session,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn final_publication_collision_is_not_committed() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000120";
        const CWD: &str = "/repo/publication/pre-unlink-failure";
        let root = tempfile::tempdir().expect("temporary grok home");
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let claim = claim_fresh_session_sync(root.path(), SESSION_ID, session_dir.clone())
            .expect("fresh claim");
        write_valid_staged_summary(&claim, &test_info(SESSION_ID, CWD));
        std::fs::create_dir_all(&session_dir).expect("create colliding target");

        let result = claim.publication.finalize();

        assert!(matches!(
            result,
            Err(FreshPublicationFinalizeError::NotCommitted(_))
        ));
        assert!(!claim.publication.is_committed());
        assert!(claim.publication.stage_session.is_dir());
        drop(claim);
    }

    #[test]
    fn final_publication_moves_private_stage_and_rebinds_storage_path() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000121";
        const CWD: &str = "/repo/publication/post-unlink-failure";
        let root = tempfile::tempdir().expect("temporary grok home");
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let claim = claim_fresh_session_sync(root.path(), SESSION_ID, session_dir.clone())
            .expect("fresh claim");
        write_valid_staged_summary(&claim, &test_info(SESSION_ID, CWD));
        let old_stage = claim.publication.stage_session.clone();
        assert!(
            !session_dir.parent().expect("published parent").exists(),
            "exercise missing-parent whole-container publication"
        );

        claim.publication.finalize().expect("publish private stage");
        assert!(claim.publication.is_committed());
        assert_eq!(claim.publication.physical_path(), session_dir);
        assert!(!old_stage.exists());
        assert!(!session_dir.join(UNPUBLISHED_SESSION_MARKER).exists());
        claim.disarm();
    }

    #[test]
    fn final_publication_atomically_publishes_long_cwd_metadata() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000143";
        let cwd = format!("/repo/{}/workspace", "very-long-component".repeat(24));
        assert_ne!(
            crate::util::grok_home::encode_cwd_dirname(&cwd),
            urlencoding::encode(&cwd).as_ref()
        );
        let root = tempfile::tempdir().expect("temporary grok home");
        let session_dir = test_session_dir(root.path(), &cwd, SESSION_ID);
        let claim = claim_fresh_session_sync(root.path(), SESSION_ID, session_dir.clone())
            .expect("fresh long-cwd claim");
        write_valid_staged_summary(&claim, &test_info(SESSION_ID, &cwd));

        claim
            .publication
            .finalize()
            .expect("publish long-cwd stage");

        let parent = session_dir.parent().expect("published cwd parent");
        assert_eq!(std::fs::read(parent.join(".cwd")).unwrap(), cwd.as_bytes());
        assert!(session_dir.join("summary.json").is_file());
        claim.disarm();
    }

    #[test]
    fn final_publication_missing_summary_is_not_committed() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000129";
        const CWD: &str = "/repo/publication/missing-summary";
        let root = tempfile::tempdir().unwrap();
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let claim = claim_fresh_session_sync(root.path(), SESSION_ID, session_dir.clone()).unwrap();

        assert!(matches!(
            claim.publication.finalize(),
            Err(FreshPublicationFinalizeError::NotCommitted(_))
        ));
        assert!(!session_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn final_publication_rejects_symlinked_sessions_root() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000130";
        const CWD: &str = "/repo/publication/sessions-symlink";
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let claim = claim_fresh_session_sync(root.path(), SESSION_ID, session_dir.clone()).unwrap();
        write_valid_staged_summary(&claim, &test_info(SESSION_ID, CWD));
        std::os::unix::fs::symlink(outside.path(), root.path().join("sessions")).unwrap();

        assert!(matches!(
            claim.publication.finalize(),
            Err(FreshPublicationFinalizeError::NotCommitted(_))
        ));
        assert!(!outside.path().join(SESSION_ID).exists());
    }

    #[test]
    fn post_rename_sync_failure_is_committed_and_drop_preserves_public_tree() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000131";
        const CWD: &str = "/repo/publication/post-rename-sync";
        let root = tempfile::tempdir().unwrap();
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let claim = claim_fresh_session_sync(root.path(), SESSION_ID, session_dir.clone()).unwrap();
        write_valid_staged_summary(&claim, &test_info(SESSION_ID, CWD));

        let result = finalize_fresh_publication_sync_with(&claim.publication, |_, _| {
            Err(io::Error::other("injected post-rename sync failure"))
        });
        assert!(matches!(
            result,
            Err(FreshPublicationFinalizeError::CommittedDurability(_))
        ));
        drop(claim);
        assert!(session_dir.join("summary.json").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn fresh_private_hierarchy_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000132";
        const CWD: &str = "/repo/publication/private-mode";
        let root = tempfile::tempdir().unwrap();
        let claim = claim_fresh_session_sync(
            root.path(),
            SESSION_ID,
            test_session_dir(root.path(), CWD, SESSION_ID),
        )
        .unwrap();
        for directory in [
            root.path().join(".private"),
            root.path().join(".private/session-staging"),
            claim.publication.stage_container.clone(),
            claim.publication.stage_session.clone(),
        ] {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    async fn production_new(info: &Info) -> io::Result<PersistenceHandle> {
        new(
            info,
            default_model_id(),
            test_sampling_client(),
            StorageMode::Local,
            None,
            None,
            None,
            String::new(),
            None,
        )
        .await
    }

    fn print_child_ready() {
        println!("{CHILD_READY}");
        std::io::stdout().flush().expect("flush child ready marker");
    }

    fn wait_for_child_release(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !path.is_file() {
            assert!(
                std::time::Instant::now() < deadline,
                "parent did not release production session child"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Re-executed libtest child for genuine cross-process flock coverage.
    #[test]
    #[ignore = "spawned by cross-process fresh-session claim tests"]
    fn subprocess_session_claim_entry() {
        let Ok(mode) = std::env::var(CHILD_MODE_ENV) else {
            return;
        };
        let root = PathBuf::from(std::env::var_os(CHILD_ROOT_ENV).expect("child root"));
        let session_id = std::env::var(CHILD_ID_ENV).expect("child session id");
        let cwd = std::env::var(CHILD_CWD_ENV).expect("child cwd");
        let release_path =
            PathBuf::from(std::env::var_os(CHILD_RELEASE_ENV).expect("child release marker path"));

        let result = match mode.as_str() {
            "claim" => {
                print_child_ready();
                let session_dir = test_session_dir(&root, &cwd, &session_id);
                match claim_fresh_session_sync(&root, &session_id, session_dir) {
                    Ok(_claim) => "CLAIMED".to_owned(),
                    Err(error) => format!("ERROR:{:?}", error.kind()),
                }
            }
            "load" => {
                print_child_ready();
                match acquire_session_id_lock_sync(&root, &session_id) {
                    Ok(_lock) => {
                        if test_session_dir(&root, &cwd, &session_id)
                            .join("summary.json")
                            .is_file()
                        {
                            "PRESENT".to_owned()
                        } else {
                            "ABSENT".to_owned()
                        }
                    }
                    Err(error) => format!("ERROR:{:?}", error.kind()),
                }
            }
            "production-new" => {
                print_child_ready();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("production new child runtime");
                match runtime.block_on(production_new(&test_info(&session_id, &cwd))) {
                    Ok(_handle) => "CREATED".to_owned(),
                    Err(error) => format!("ERROR:{:?}", error.kind()),
                }
            }
            "production-new-hold-publish"
            | "production-new-hold-finalized-publish"
            | "production-new-hold-abort" => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("production creator child runtime");
                let handle = runtime
                    .block_on(production_new(&test_info(&session_id, &cwd)))
                    .expect("production PersistenceHandle::new");
                let publication_gate = crate::session::SessionPublicationGate::pending();
                runtime
                    .block_on(PersistenceHandle::publish_fresh(
                        &handle.tx,
                        publication_gate.clone(),
                    ))
                    .expect("arm fresh publication gate");
                let finalized_before_gate = mode == "production-new-hold-finalized-publish";
                if finalized_before_gate {
                    handle
                        .fresh_publication()
                        .expect("fresh publication plan")
                        .finalize()
                        .expect("durably finalize fresh storage before holding the gate");
                }
                print_child_ready();
                wait_for_child_release(&release_path);
                if mode != "production-new-hold-abort" {
                    if !finalized_before_gate {
                        handle
                            .fresh_publication()
                            .expect("fresh publication plan")
                            .finalize()
                            .expect("durably publish fresh storage");
                    }
                    publication_gate.publish();
                    let (respond_to, response) = tokio::sync::oneshot::channel();
                    handle
                        .tx
                        .send(PersistenceMsg::ProbeWritable { respond_to })
                        .expect("send post-publication persistence barrier");
                    runtime
                        .block_on(response)
                        .expect("persistence actor stopped before publication barrier")
                        .expect("post-publication persistence barrier");
                    "PUBLISHED".to_owned()
                } else {
                    runtime
                        .block_on(PersistenceHandle::abort_fresh_and_delete(
                            &handle.tx,
                            publication_gate,
                        ))
                        .expect("abort production fresh session");
                    "ABORTED".to_owned()
                }
            }
            "production-load" | "production-load-light" => {
                print_child_ready();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("production load child runtime");
                let info = test_info(&session_id, &cwd);
                let load_result = if mode == "production-load" {
                    runtime
                        .block_on(load(
                            &info,
                            test_sampling_client(),
                            StorageMode::Local,
                            None,
                            None,
                            None,
                            None,
                            String::new(),
                            None,
                        ))
                        .map(|(persisted, _handle)| persisted.summary.info.id)
                } else {
                    runtime
                        .block_on(load_light(
                            &info,
                            test_sampling_client(),
                            StorageMode::Local,
                            None,
                            None,
                            None,
                            None,
                            String::new(),
                            None,
                        ))
                        .map(|(persisted, _handle)| persisted.summary.info.id)
                };
                match load_result {
                    Ok(loaded_id) if loaded_id == info.id => "PRESENT".to_owned(),
                    Ok(loaded_id) => format!("WRONG_ID:{loaded_id}"),
                    Err(error) => format!("ERROR:{:?}", error.kind()),
                }
            }
            "published-read" => {
                print_child_ready();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("published read child runtime");
                match runtime.block_on(acquire_published_session_read_in_root(
                    &root.join("sessions"),
                    &session_id,
                    Some(&cwd),
                )) {
                    Ok(Some(session)) if session.read_summary().is_ok() => "PRESENT".to_owned(),
                    Ok(Some(_)) => "INVALID_SUMMARY".to_owned(),
                    Ok(None) => "ABSENT".to_owned(),
                    Err(error) => format!("ERROR:{:?}", error.kind()),
                }
            }
            "production-list" => {
                print_child_ready();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("production list child runtime");
                match runtime.block_on(list_summaries(Some(&cwd))) {
                    Ok(summaries)
                        if summaries
                            .iter()
                            .any(|summary| summary.info.id.to_string() == session_id) =>
                    {
                        "PRESENT".to_owned()
                    }
                    Ok(_) => "ABSENT".to_owned(),
                    Err(error) => format!("ERROR:{:?}", error.kind()),
                }
            }
            "production-delete" => {
                print_child_ready();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("production delete child runtime");
                let auth_manager = Arc::new(crate::auth::AuthManager::new(
                    &root,
                    crate::auth::GrokComConfig::default(),
                ));
                match runtime.block_on(delete_session_history(
                    &session_id,
                    Some(&cwd),
                    false,
                    auth_manager,
                )) {
                    Ok(deletion)
                        if !deletion.any_removed()
                            && test_session_dir(&root, &cwd, &session_id).is_dir() =>
                    {
                        "ABSENT_PRESERVED".to_owned()
                    }
                    Ok(deletion) => format!(
                        "REMOVED:{}:{}",
                        deletion.local_removed, deletion.remote_removed
                    ),
                    Err(error) => format!("ERROR:{error}"),
                }
            }
            other => panic!("unknown child session-claim mode: {other}"),
        };
        println!("{CHILD_RESULT_PREFIX}{result}");
    }

    fn child_test_filter() -> String {
        let module = module_path!();
        let module = module.split_once("::").map_or(module, |(_, rest)| rest);
        format!("{module}::subprocess_session_claim_entry")
    }

    #[allow(clippy::disallowed_methods)] // isolated libtest subprocess fixture
    fn spawn_claim_child(
        mode: &str,
        root: &Path,
        session_id: &str,
        cwd: &str,
    ) -> std::process::Child {
        let executable = std::env::current_exe().expect("current libtest executable");
        let release_path = root.join("child-release");
        let mut command = std::process::Command::new(executable);
        command
            .args(["--ignored", "--exact", "--nocapture", &child_test_filter()])
            .env(CHILD_MODE_ENV, mode)
            .env(CHILD_ROOT_ENV, root)
            .env(CHILD_ID_ENV, session_id)
            .env(CHILD_CWD_ENV, cwd)
            .env(CHILD_RELEASE_ENV, &release_path)
            .env("GROK_HOME", root)
            .env_remove("MEDLEY_HOME")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        xai_tty_utils::detach_std_command(&mut command);
        let mut child = command.spawn().expect("spawn session-claim child");

        {
            let stdout = child.stdout.as_mut().expect("child stdout");
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                let bytes = reader.read_line(&mut line).expect("read child marker");
                assert!(bytes > 0, "child exited before its ready marker");
                if line.trim() == CHILD_READY {
                    break;
                }
            }
        }
        child
    }

    fn release_claim_child(root: &Path) {
        std::fs::write(root.join("child-release"), b"release")
            .expect("release production session child");
    }

    fn assert_claim_child_is_blocked(child: &mut std::process::Child, operation: &str) {
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            child
                .try_wait()
                .unwrap_or_else(|error| panic!("poll {operation}: {error}"))
                .is_none(),
            "{operation} must remain blocked while fresh state is provisional"
        );
    }

    fn finish_claim_child(mut child: std::process::Child) -> String {
        let deadline = std::time::Instant::now() + CHILD_FINISH_TIMEOUT;
        loop {
            match child.try_wait().expect("poll session-claim child") {
                Some(status) => {
                    let mut stdout = String::new();
                    child
                        .stdout
                        .take()
                        .expect("child stdout")
                        .read_to_string(&mut stdout)
                        .expect("read child stdout");
                    let mut stderr = String::new();
                    child
                        .stderr
                        .take()
                        .expect("child stderr")
                        .read_to_string(&mut stderr)
                        .expect("read child stderr");
                    assert!(
                        status.success(),
                        "session-claim child failed ({status:?})\nstdout:\n{stdout}\nstderr:\n{stderr}"
                    );
                    return stdout;
                }
                None if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("session-claim child did not finish after lock release");
                }
            }
        }
    }

    fn assert_child_result(stdout: &str, expected: &str) {
        assert!(
            stdout
                .lines()
                .any(|line| line.trim() == format!("{CHILD_RESULT_PREFIX}{expected}")),
            "missing child result {expected:?} in stdout:\n{stdout}"
        );
    }

    #[test]
    fn cross_process_duplicate_claim_rechecks_other_encoded_cwd_after_lock() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000104";
        const CREATOR_CWD: &str = "/repo/concurrent-claim/creator";
        const CONTENDER_CWD: &str = "/repo/concurrent-claim/other-worktree";

        let root = tempfile::tempdir().expect("temporary grok home");
        let session_dir = test_session_dir(root.path(), CREATOR_CWD, SESSION_ID);
        let first_claim = claim_fresh_session_sync(root.path(), SESSION_ID, session_dir.clone())
            .expect("first claim succeeds");

        let mut contender = spawn_claim_child("claim", root.path(), SESSION_ID, CONTENDER_CWD);
        assert!(
            contender
                .try_wait()
                .expect("poll blocked contender")
                .is_none(),
            "the real second process must wait for the creator's id claim"
        );

        write_valid_staged_summary(&first_claim, &test_info(SESSION_ID, CREATOR_CWD));

        first_claim
            .publication
            .finalize()
            .expect("durably publish first claim");
        first_claim.disarm();
        let stdout = finish_claim_child(contender);
        assert_child_result(&stdout, "ERROR:AlreadyExists");

        std::fs::remove_dir_all(&session_dir).expect("remove persisted session");
        let retry = claim_fresh_session_sync(
            root.path(),
            SESSION_ID,
            test_session_dir(root.path(), CONTENDER_CWD, SESSION_ID),
        )
        .expect("the reusable lock file is not a permanent tombstone");
        drop(retry);
    }

    #[test]
    fn fresh_claim_stages_marker_and_reclaims_stale_marked_dir_across_cwds() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000114";
        const STALE_CWD: &str = "/repo/stale-creator";
        const RETRY_CWD: &str = "/repo/retry-creator";

        let root = tempfile::tempdir().expect("temporary grok home");
        let stale_dir = test_session_dir(root.path(), STALE_CWD, SESSION_ID);
        let stale = claim_fresh_session_sync(root.path(), SESSION_ID, stale_dir.clone())
            .expect("initial fresh claim");
        assert!(
            stale
                .publication
                .stage_session
                .join(UNPUBLISHED_SESSION_MARKER)
                .is_file(),
            "the marker must exist before any summary write"
        );
        let stale_stage = stale.publication.stage_container.clone();
        std::fs::write(stale.publication.stage_session.join("summary.json"), b"{}")
            .expect("provisional summary");
        stale.disarm();

        let retry_dir = test_session_dir(root.path(), RETRY_CWD, SESSION_ID);
        let retry = claim_fresh_session_sync(root.path(), SESSION_ID, retry_dir.clone())
            .expect("new lock owner reclaims crashed provisional state");
        assert_eq!(
            retry.publication.stage_container, stale_stage,
            "deterministic per-id stage container is safely reclaimed then reused"
        );
        assert!(
            !retry
                .publication
                .stage_session
                .join("summary.json")
                .exists(),
            "crashed stage contents must not survive deterministic container reuse"
        );
        assert!(
            retry
                .publication
                .stage_session
                .join(UNPUBLISHED_SESSION_MARKER)
                .is_file()
        );
        assert!(!retry_dir.exists(), "fresh identity stays out of sessions/");
        drop(retry);
    }

    #[test]
    fn fresh_claim_invalid_public_path_does_not_create_private_stage() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000152";
        let root = tempfile::tempdir().expect("temporary grok home");
        let invalid = root.path().join("not-sessions").join(SESSION_ID);

        let error = claim_fresh_session_sync(root.path(), SESSION_ID, invalid)
            .expect_err("invalid publication target");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let staging = root.path().join(".private/session-staging");
        assert_eq!(std::fs::read_dir(staging).unwrap().count(), 0);
    }

    #[test]
    fn fresh_claim_reclaims_markerless_exact_stage_and_preserves_other_shapes() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000145";
        const OTHER_ID: &str = "019c0000-0000-7000-8000-000000000146";
        const CWD: &str = "/repo/stage-preservation";
        let root = tempfile::tempdir().unwrap();
        let (_path, staging) = ensure_private_staging_hierarchy_anchored(root.path()).unwrap();

        let unrelated = staging.create_child_dir(OsStr::new("unrelated")).unwrap();
        let unrelated_session = unrelated.create_child_dir(OsStr::new(OTHER_ID)).unwrap();
        create_unpublished_session_marker(&unrelated_session).unwrap();
        let markerless = staging.create_child_dir(OsStr::new("markerless")).unwrap();
        markerless.create_child_dir(OsStr::new(SESSION_ID)).unwrap();
        markerless
            .create_child_file_new(OsStr::new(".cwd"))
            .unwrap();
        let extra = staging
            .create_child_dir(OsStr::new("extra-sibling"))
            .unwrap();
        extra.create_child_dir(OsStr::new(SESSION_ID)).unwrap();
        extra.create_child_dir(OsStr::new("preserve-me")).unwrap();
        drop(unrelated_session);
        drop(unrelated);
        drop(markerless);
        drop(extra);

        let claim = claim_fresh_session_sync(
            root.path(),
            SESSION_ID,
            test_session_dir(root.path(), CWD, SESSION_ID),
        )
        .unwrap();
        assert!(
            root.path()
                .join(".private/session-staging/unrelated")
                .is_dir()
        );
        assert!(
            !root
                .path()
                .join(".private/session-staging/markerless")
                .exists()
        );
        assert!(
            root.path()
                .join(".private/session-staging/extra-sibling/preserve-me")
                .is_dir()
        );
        drop(claim);
    }

    #[cfg(unix)]
    #[test]
    fn fresh_claim_does_not_follow_staging_symlinks() {
        use std::os::unix::fs::symlink;

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000147";
        const CWD: &str = "/repo/stage-symlink";
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join(SESSION_ID)).unwrap();
        std::fs::write(
            outside
                .path()
                .join(SESSION_ID)
                .join(UNPUBLISHED_SESSION_MARKER),
            b"outside marker",
        )
        .unwrap();
        std::fs::write(outside.path().join("sentinel"), b"preserve").unwrap();
        let (staging_path, _staging) =
            ensure_private_staging_hierarchy_anchored(root.path()).unwrap();
        symlink(outside.path(), staging_path.join("linked-container")).unwrap();

        let real_container = staging_path.join("linked-session-container");
        std::fs::create_dir(&real_container).unwrap();
        symlink(
            outside.path().join(SESSION_ID),
            real_container.join(SESSION_ID),
        )
        .unwrap();

        let claim = claim_fresh_session_sync(
            root.path(),
            SESSION_ID,
            test_session_dir(root.path(), CWD, SESSION_ID),
        )
        .unwrap();
        assert!(staging_path.join("linked-container").is_symlink());
        assert!(real_container.join(SESSION_ID).is_symlink());
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"preserve"
        );
        drop(claim);
    }

    #[cfg(unix)]
    #[test]
    fn fresh_claim_preserves_non_private_marked_stage() {
        use std::os::unix::fs::PermissionsExt as _;

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000149";
        const CWD: &str = "/repo/non-private-stage";
        let root = tempfile::tempdir().unwrap();
        let (staging_path, staging) =
            ensure_private_staging_hierarchy_anchored(root.path()).unwrap();
        let container = staging.create_child_dir(OsStr::new("permissive")).unwrap();
        let session = container.create_child_dir(OsStr::new(SESSION_ID)).unwrap();
        create_unpublished_session_marker(&session).unwrap();
        drop(session);
        drop(container);
        std::fs::set_permissions(
            staging_path.join("permissive"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let claim = claim_fresh_session_sync(
            root.path(),
            SESSION_ID,
            test_session_dir(root.path(), CWD, SESSION_ID),
        )
        .unwrap();
        assert!(staging_path.join("permissive").is_dir());
        assert_eq!(
            std::fs::metadata(staging_path.join("permissive"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        drop(claim);
    }

    #[cfg(windows)]
    #[test]
    fn fresh_claim_does_not_follow_staging_junctions() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000148";
        const CWD: &str = "C:\\repo\\stage-junction";
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(outside.path().join(SESSION_ID)).unwrap();
        std::fs::write(
            outside
                .path()
                .join(SESSION_ID)
                .join(UNPUBLISHED_SESSION_MARKER),
            b"outside marker",
        )
        .unwrap();
        std::fs::write(outside.path().join("sentinel"), b"preserve").unwrap();
        let (staging_path, _staging) =
            ensure_private_staging_hierarchy_anchored(root.path()).unwrap();
        let junction = staging_path.join("linked-container");
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

        let claim = claim_fresh_session_sync(
            root.path(),
            SESSION_ID,
            test_session_dir(root.path(), CWD, SESSION_ID),
        )
        .unwrap();
        assert!(junction.exists());
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"preserve"
        );
        drop(claim);
    }

    #[test]
    fn cross_process_load_waits_for_fresh_publish() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000106";
        const CWD: &str = "/repo/load-after-publish";

        let root = tempfile::tempdir().expect("temporary grok home");
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let creator = claim_fresh_session_sync(root.path(), SESSION_ID, session_dir.clone())
            .expect("creator claim");
        let mut loader = spawn_claim_child("load", root.path(), SESSION_ID, CWD);
        assert!(
            loader.try_wait().expect("poll blocked loader").is_none(),
            "load must remain blocked while fresh state is provisional"
        );

        write_valid_staged_summary(&creator, &test_info(SESSION_ID, CWD));
        creator
            .publication
            .finalize()
            .expect("durably publish creator");
        creator.disarm();

        let stdout = finish_claim_child(loader);
        assert_child_result(&stdout, "PRESENT");
    }

    #[test]
    fn cross_process_load_observes_not_found_after_fresh_abort() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000107";
        const CWD: &str = "/repo/load-after-abort";

        let root = tempfile::tempdir().expect("temporary grok home");
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let creator = claim_fresh_session_sync(root.path(), SESSION_ID, session_dir.clone())
            .expect("creator claim");
        std::fs::write(
            creator.publication.stage_session.join("summary.json"),
            b"{}",
        )
        .expect("write provisional summary");

        let mut loader = spawn_claim_child("load", root.path(), SESSION_ID, CWD);
        assert!(
            loader.try_wait().expect("poll blocked loader").is_none(),
            "load must remain blocked while fresh state is provisional"
        );
        drop(creator);

        let stdout = finish_claim_child(loader);
        assert_child_result(&stdout, "ABSENT");
        assert!(
            !session_dir.exists(),
            "abort must remove provisional state before releasing the id claim"
        );
    }

    #[test]
    fn production_new_blocks_duplicate_creator_until_publish_then_rejects_it() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000108";
        const CREATOR_CWD: &str = "/repo/production-new/creator";
        const CONTENDER_CWD: &str = "/repo/production-new/contender";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator = spawn_claim_child(
            "production-new-hold-publish",
            root.path(),
            SESSION_ID,
            CREATOR_CWD,
        );
        let mut contender =
            spawn_claim_child("production-new", root.path(), SESSION_ID, CONTENDER_CWD);
        assert_claim_child_is_blocked(&mut contender, "production persistence::new");

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "PUBLISHED");
        assert_child_result(&finish_claim_child(contender), "ERROR:AlreadyExists");
    }

    #[test]
    fn cross_process_list_ignores_pending_then_sees_publication() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000113";
        const CWD: &str = "/repo/production-discovery/publication";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator =
            spawn_claim_child("production-new-hold-publish", root.path(), SESSION_ID, CWD);

        let pending_list = spawn_claim_child("production-list", root.path(), SESSION_ID, CWD);
        assert_child_result(&finish_claim_child(pending_list), "ABSENT");

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "PUBLISHED");

        let published_list = spawn_claim_child("production-list", root.path(), SESSION_ID, CWD);
        assert_child_result(&finish_claim_child(published_list), "PRESENT");
    }

    #[test]
    fn cross_process_list_skips_and_delete_waits_between_finalizer_and_gate() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000122";
        const CWD: &str = "/repo/production-discovery/finalizer-gate-gap";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator = spawn_claim_child(
            "production-new-hold-finalized-publish",
            root.path(),
            SESSION_ID,
            CWD,
        );
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        assert!(session_dir.join("summary.json").is_file());
        assert!(
            !session_dir.join(UNPUBLISHED_SESSION_MARKER).exists(),
            "fixture must pause after marker removal and before gate publication"
        );

        let pending_list = spawn_claim_child("production-list", root.path(), SESSION_ID, CWD);
        assert_child_result(&finish_claim_child(pending_list), "ABSENT");

        let mut pending_delete =
            spawn_claim_child("production-delete", root.path(), SESSION_ID, CWD);
        assert_claim_child_is_blocked(
            &mut pending_delete,
            "production delete during finalizer-to-gate gap",
        );

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "PUBLISHED");
        assert_child_result(&finish_claim_child(pending_delete), "REMOVED:true:false");
        assert!(
            !session_dir.exists(),
            "delete may remove the session only after publication releases the id lock"
        );
    }

    #[test]
    fn cross_process_load_waits_between_finalizer_and_gate() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000123";
        const CWD: &str = "/repo/production-load/finalizer-gate-gap";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator = spawn_claim_child(
            "production-new-hold-finalized-publish",
            root.path(),
            SESSION_ID,
            CWD,
        );
        assert!(
            !test_session_dir(root.path(), CWD, SESSION_ID)
                .join(UNPUBLISHED_SESSION_MARKER)
                .exists(),
            "fixture must pause after marker removal and before gate publication"
        );

        let mut loader = spawn_claim_child("production-load", root.path(), SESSION_ID, CWD);
        assert_claim_child_is_blocked(&mut loader, "production load during finalizer-to-gate gap");

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "PUBLISHED");
        assert_child_result(&finish_claim_child(loader), "PRESENT");
    }

    #[test]
    fn published_read_waits_for_marker_publication_and_retains_visibility_lease() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000124";
        const CWD: &str = "/repo/published-read/marker";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator =
            spawn_claim_child("production-new-hold-publish", root.path(), SESSION_ID, CWD);
        assert!(
            !test_session_dir(root.path(), CWD, SESSION_ID).exists(),
            "pending fresh storage must not expose cwd or id under sessions/"
        );
        let mut reader = spawn_claim_child("published-read", root.path(), SESSION_ID, CWD);
        assert_claim_child_is_blocked(&mut reader, "published session read with marker present");

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "PUBLISHED");
        assert_child_result(&finish_claim_child(reader), "PRESENT");
    }

    #[test]
    fn published_read_waits_between_marker_removal_and_publication_gate() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000125";
        const CWD: &str = "/repo/published-read/finalizer-gate-gap";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator = spawn_claim_child(
            "production-new-hold-finalized-publish",
            root.path(),
            SESSION_ID,
            CWD,
        );
        assert!(
            !test_session_dir(root.path(), CWD, SESSION_ID)
                .join(UNPUBLISHED_SESSION_MARKER)
                .exists()
        );
        let mut reader = spawn_claim_child("published-read", root.path(), SESSION_ID, CWD);
        assert_claim_child_is_blocked(
            &mut reader,
            "published session read during finalizer-to-gate gap",
        );

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "PUBLISHED");
        assert_child_result(&finish_claim_child(reader), "PRESENT");
    }

    #[test]
    fn published_read_waits_for_abort_then_reports_absent() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000126";
        const CWD: &str = "/repo/published-read/abort";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator = spawn_claim_child("production-new-hold-abort", root.path(), SESSION_ID, CWD);
        let mut reader = spawn_claim_child("published-read", root.path(), SESSION_ID, CWD);
        assert_claim_child_is_blocked(&mut reader, "published session read before abort");

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "ABORTED");
        assert_child_result(&finish_claim_child(reader), "ABSENT");
    }

    #[tokio::test]
    async fn unpublished_write_drop_removes_partial_private_tree() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000127";
        const CWD: &str = "/repo/published-write/abort";

        let root = tempfile::tempdir().expect("temporary grok home");
        let sessions_root = root.path().join("sessions");
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let mut writer =
            acquire_published_session_write_in_root(&sessions_root, SESSION_ID, Some(CWD))
                .await
                .expect("exclusive published-session writer");
        let dir = writer
            .begin_new(session_dir.clone())
            .expect("begin marker-hidden write");
        let private_stage = dir.to_path_buf();
        std::fs::write(dir.join("summary.json"), b"{}").expect("write partial summary");
        std::fs::write(dir.join("partial"), b"not committed").expect("write partial payload");
        assert!(private_stage.starts_with(root.path().join(".private/session-staging")));
        assert!(!session_dir.exists());

        drop(writer);
        assert!(!session_dir.exists(), "abort never creates a public tree");
        assert!(
            !private_stage.exists(),
            "abort removes the retained private stage"
        );
        assert!(
            acquire_published_session_read_in_root(&sessions_root, SESSION_ID, Some(CWD))
                .await
                .expect("read after abort")
                .is_none(),
            "aborted write never becomes published"
        );
    }

    #[tokio::test]
    async fn published_write_precommit_failure_restores_stage_for_drop_cleanup() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000153";
        const CWD: &str = "/repo/published-write/precommit-failure";

        let root = tempfile::tempdir().expect("temporary grok home");
        let sessions_root = root.path().join("sessions");
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let mut writer =
            acquire_published_session_write_in_root(&sessions_root, SESSION_ID, Some(CWD))
                .await
                .expect("exclusive published-session writer");
        let stage = writer
            .begin_new(session_dir.clone())
            .expect("begin private write")
            .to_path_buf();
        let stage_container = stage.parent().unwrap().to_path_buf();
        std::fs::write(stage.join("summary.json"), b"not-json").unwrap();

        assert!(matches!(
            writer.publish_new_classified(),
            Err(PublishedSessionFinalizeError::NotCommitted(_))
        ));
        drop(writer);

        assert!(!stage_container.exists());
        assert!(!session_dir.exists());
    }

    #[tokio::test]
    async fn published_write_begin_reclaims_markerless_crash_stage() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000150";
        const STALE_CWD: &str = "/repo/published-write/stale-stage";
        const NEW_CWD: &str = "/repo/published-write/retry";

        let root = tempfile::tempdir().expect("temporary grok home");
        let stale = claim_fresh_session_sync(
            root.path(),
            SESSION_ID,
            test_session_dir(root.path(), STALE_CWD, SESSION_ID),
        )
        .expect("create stale private stage");
        let stale_container = stale.publication.stage_container.clone();
        std::fs::remove_file(
            stale
                .publication
                .stage_session
                .join(UNPUBLISHED_SESSION_MARKER),
        )
        .expect("simulate crash after marker removal");
        stale.disarm();

        let sessions_root = root.path().join("sessions");
        let mut writer =
            acquire_published_session_write_in_root(&sessions_root, SESSION_ID, Some(NEW_CWD))
                .await
                .expect("exclusive published-session writer");
        let new_stage = writer
            .begin_new(test_session_dir(root.path(), NEW_CWD, SESSION_ID))
            .expect("retry after markerless crash")
            .to_path_buf();

        assert_eq!(new_stage.parent(), Some(stale_container.as_path()));
        assert!(new_stage.join(UNPUBLISHED_SESSION_MARKER).is_file());
        assert!(
            !new_stage.join("summary.json").exists(),
            "the deterministic container was reclaimed before reuse"
        );
    }

    #[tokio::test]
    async fn published_write_acquire_reclaims_empty_post_commit_stage() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000151";
        const CWD: &str = "/repo/published-write/post-child-commit-crash";

        let root = tempfile::tempdir().expect("temporary grok home");
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        std::fs::create_dir_all(&session_dir).expect("existing published session");
        let summary = Summary::new(&test_info(SESSION_ID, CWD), default_model_id()).unwrap();
        std::fs::write(
            session_dir.join("summary.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )
        .unwrap();
        std::fs::write(session_dir.join("sentinel"), b"published").unwrap();

        let (staging_path, staging) =
            ensure_private_staging_hierarchy_anchored(root.path()).unwrap();
        let container_name = session_stage_container_name(SESSION_ID);
        let residue = staging.create_child_dir(&container_name).unwrap();
        residue.ensure_owner_only().unwrap();
        residue.create_child_file_new(OsStr::new(".cwd")).unwrap();
        drop(residue);
        let residue_path = staging_path.join(&container_name);

        let writer = acquire_published_session_write_in_root(
            &root.path().join("sessions"),
            SESSION_ID,
            Some(CWD),
        )
        .await
        .expect("exclusive published-session writer");

        assert_eq!(writer.published_path(), Some(session_dir.as_path()));
        assert!(!residue_path.exists());
        assert_eq!(
            std::fs::read(session_dir.join("sentinel")).unwrap(),
            b"published"
        );
    }

    #[tokio::test]
    async fn published_write_post_commit_sync_failure_preserves_visible_directory() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000133";
        const CWD: &str = "/repo/published-write/post-commit-sync";

        let root = tempfile::tempdir().expect("temporary grok home");
        let sessions_root = root.path().join("sessions");
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let mut writer =
            acquire_published_session_write_in_root(&sessions_root, SESSION_ID, Some(CWD))
                .await
                .expect("exclusive published-session writer");
        let dir = writer
            .begin_new(session_dir.clone())
            .expect("begin marker-hidden write");
        let summary =
            Summary::new(&test_info(SESSION_ID, CWD), default_model_id()).expect("summary");
        std::fs::write(
            dir.join("summary.json"),
            serde_json::to_vec_pretty(&summary).expect("serialize summary"),
        )
        .expect("write summary");

        let error = writer
            .publish_new_with(|_, _| Err(io::Error::other("injected post-commit sync failure")))
            .expect_err("post-commit sync must report failure");
        assert!(matches!(
            error,
            PublishedSessionFinalizeError::CommittedDurability(_)
        ));
        assert_eq!(writer.published_path(), Some(session_dir.as_path()));
        let published = writer
            .into_lifetime_read()
            .expect("committed lease handoff");
        assert!(
            try_acquire_session_id_write_lock_sync(root.path(), SESSION_ID)
                .expect("try competing writer")
                .is_none(),
            "committed publication must retain a lifetime lease"
        );
        drop(published);
        assert!(
            try_acquire_session_id_write_lock_sync(root.path(), SESSION_ID)
                .expect("try writer after lease drop")
                .is_some()
        );

        assert!(session_dir.is_dir(), "drop must preserve committed session");
        assert!(session_dir.join("summary.json").is_file());
        assert!(!session_dir.join(UNPUBLISHED_SESSION_MARKER).exists());
    }

    #[tokio::test]
    async fn stale_public_marker_is_preserved_and_new_publication_fails_closed() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000142";
        const OLD_CWD: &str = "/repo/published-write/stale-old";
        const NEW_CWD: &str = "/repo/published-write/stale-new";
        let root = tempfile::tempdir().expect("temporary grok home");
        let sessions_root = root.path().join("sessions");
        let stale = test_session_dir(root.path(), OLD_CWD, SESSION_ID);
        std::fs::create_dir_all(&stale).expect("legacy stale public directory");
        std::fs::write(stale.join(UNPUBLISHED_SESSION_MARKER), b"")
            .expect("legacy publication marker");
        std::fs::write(stale.join("sentinel"), b"preserve").expect("stale sentinel");

        let mut writer =
            acquire_published_session_write_in_root(&sessions_root, SESSION_ID, Some(NEW_CWD))
                .await
                .expect("exclusive writer");
        let error = writer
            .begin_new(test_session_dir(root.path(), NEW_CWD, SESSION_ID))
            .expect_err("stale public id must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(stale.join("sentinel")).unwrap(), b"preserve");
    }

    #[tokio::test]
    async fn published_access_uses_the_canonical_grok_home_session_id_lock_root() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000128";

        let root = tempfile::tempdir().expect("temporary grok home");
        let sessions_root = root.path().join("sessions");
        let writer = acquire_published_session_write_in_root(&sessions_root, SESSION_ID, None)
            .await
            .expect("published writer lock");

        let lock_name = session_claim_lock_name(SESSION_ID);
        assert!(
            root.path()
                .join(".locks/session-ids")
                .join(&lock_name)
                .is_file(),
            "published access must share the canonical lock root with fresh/load/delete"
        );
        assert!(
            !sessions_root
                .join(".locks/session-ids")
                .join(lock_name)
                .exists(),
            "a sessions/.locks split-brain lock would not exclude fresh creators"
        );
        drop(writer);
    }

    #[cfg(unix)]
    #[test]
    fn session_id_lock_namespace_is_owner_only_and_rejects_link_indirection() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000152";
        let root = tempfile::tempdir().expect("temporary grok home");
        let locks = root.path().join(".locks");
        let session_ids = locks.join("session-ids");
        std::fs::create_dir(&locks).expect("legacy lock directory");
        std::fs::set_permissions(&locks, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (namespace, mutation) =
            open_session_id_lock_files(root.path(), SESSION_ID).expect("secure session lock files");
        assert_eq!(
            std::fs::metadata(&locks).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&session_ids)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            namespace.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            mutation.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop((namespace, mutation));

        let linked_root = tempfile::tempdir().expect("linked grok home");
        let outside = tempfile::tempdir().expect("outside lock target");
        std::fs::write(outside.path().join("sentinel"), b"preserve").unwrap();
        symlink(outside.path(), linked_root.path().join(".locks")).unwrap();
        assert!(open_session_id_lock_files(linked_root.path(), SESSION_ID).is_err());
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"preserve"
        );
        assert!(!outside.path().join("session-ids").exists());
    }

    #[test]
    fn remote_pull_publication_two_misses_publish_once_and_hide_partial_tree() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000134";
        const CWD: &str = "/repo/remote-pull/two-misses";

        let root = tempfile::tempdir().expect("temporary grok home");
        let sessions_root = root.path().join("sessions");
        let session_dir = test_session_dir(root.path(), CWD, SESSION_ID);
        let start = Arc::new(std::sync::Barrier::new(2));
        let publishers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (hydrated_tx, hydrated_rx) = std::sync::mpsc::channel();
        let publish = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        let mut loaders = Vec::new();
        for _ in 0..2 {
            let sessions_root = sessions_root.clone();
            let session_dir = session_dir.clone();
            let start = start.clone();
            let publishers = publishers.clone();
            let hydrated_tx = hydrated_tx.clone();
            let publish = publish.clone();
            loaders.push(std::thread::spawn(move || {
                start.wait();
                match acquire_canonical_session_claim_sync(&sessions_root, SESSION_ID, Some(CWD))
                    .expect("canonical remote-miss claim")
                {
                    CanonicalSessionClaim::Existing(existing) => {
                        let summary = existing.read_summary().expect("published summary");
                        assert_eq!(summary.info.id.to_string(), SESSION_ID);
                        assert!(!existing.path().join("partial").exists());
                        existing
                    }
                    CanonicalSessionClaim::Vacant(mut writer) => {
                        publishers.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let stage = writer.begin_new(session_dir).expect("marker-hidden stage");
                        let summary = Summary::new(&test_info(SESSION_ID, CWD), default_model_id())
                            .expect("summary");
                        std::fs::write(
                            stage.join("summary.json"),
                            serde_json::to_vec_pretty(&summary).expect("serialize summary"),
                        )
                        .expect("write summary");
                        std::fs::write(stage.join("partial"), b"not-yet-complete")
                            .expect("write partial sentinel");
                        hydrated_tx.send(()).expect("signal hydrated stage");
                        let (lock, ready) = &*publish;
                        let mut released = lock.lock().expect("publication barrier mutex");
                        while !*released {
                            released = ready.wait(released).expect("publication barrier wait");
                        }
                        std::fs::remove_file(stage.join("partial"))
                            .expect("finish hydration before publish");
                        writer.publish_new().expect("publish hydrated session");
                        writer.into_lifetime_read().expect("lifetime shared lease")
                    }
                }
            }));
        }
        drop(hydrated_tx);

        hydrated_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("one miss must become the publisher");
        assert!(
            !session_dir.exists(),
            "private hydration must not create public state"
        );

        let reader_root = sessions_root.clone();
        let (reader_tx, reader_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let visible = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(acquire_published_session_read_in_root(
                    &reader_root,
                    SESSION_ID,
                    Some(CWD),
                ))
                .expect("published reader");
            reader_tx.send(visible).expect("return published reader");
        });
        assert!(
            reader_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "reader must not observe marker-hidden hydration"
        );

        let (lock, ready) = &*publish;
        *lock.lock().expect("publication barrier mutex") = true;
        ready.notify_one();
        for loader in loaders {
            let published = loader.join().expect("remote miss loader");
            assert!(!published.path().join("partial").exists());
        }
        let visible = reader_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader after publication")
            .expect("published session must be visible");
        assert_eq!(
            visible.read_summary().unwrap().info.id.to_string(),
            SESSION_ID
        );
        reader.join().expect("published reader thread");
        assert_eq!(publishers.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!session_dir.join(UNPUBLISHED_SESSION_MARKER).exists());
        assert!(!session_dir.join("partial").exists());
    }

    fn published_remote_pull_winner(
        sessions_root: &Path,
        physical_cwd: &str,
        requested_session_id: &str,
        summary_info: &Info,
    ) -> PublishedSessionRead {
        let session_dir = sessions_root
            .join(crate::util::grok_home::encode_cwd_dirname(physical_cwd))
            .join(requested_session_id);
        std::fs::create_dir_all(&session_dir).expect("create published winner");
        let summary =
            Summary::new(summary_info, default_model_id()).expect("published winner summary");
        std::fs::write(
            session_dir.join("summary.json"),
            serde_json::to_vec_pretty(&summary).expect("serialize published winner summary"),
        )
        .expect("write published winner summary");

        match acquire_canonical_session_claim_sync(
            sessions_root,
            requested_session_id,
            Some(physical_cwd),
        )
        .expect("claim published winner")
        {
            CanonicalSessionClaim::Existing(existing) => existing,
            CanonicalSessionClaim::Vacant(_) => panic!("published winner must already exist"),
        }
    }

    #[test]
    fn remote_pull_publication_existing_winner_rejects_summary_id_mismatch() {
        const REQUESTED_ID: &str = "019c0000-0000-7000-8000-000000000135";
        const RETURNED_ID: &str = "019c0000-0000-7000-8000-000000000136";
        const CWD: &str = "/repo/remote-pull/id-mismatch";

        let root = tempfile::tempdir().expect("temporary grok home");
        let sessions_root = root.path().join("sessions");
        let existing = published_remote_pull_winner(
            &sessions_root,
            CWD,
            REQUESTED_ID,
            &test_info(RETURNED_ID, CWD),
        );

        let error = validate_existing_remote_pull(&sessions_root, REQUESTED_ID, existing)
            .err()
            .expect("summary identity mismatch must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("session id mismatch"));
        assert!(error.to_string().contains(REQUESTED_ID));
        assert!(error.to_string().contains(RETURNED_ID));
    }

    #[test]
    fn remote_pull_publication_existing_winner_rejects_summary_cwd_path_mismatch() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000137";
        const PHYSICAL_CWD: &str = "/repo/remote-pull/physical";
        const SUMMARY_CWD: &str = "/repo/remote-pull/summary";

        let root = tempfile::tempdir().expect("temporary grok home");
        let sessions_root = root.path().join("sessions");
        let existing = published_remote_pull_winner(
            &sessions_root,
            PHYSICAL_CWD,
            SESSION_ID,
            &test_info(SESSION_ID, SUMMARY_CWD),
        );

        let error = validate_existing_remote_pull(&sessions_root, SESSION_ID, existing)
            .err()
            .expect("summary cwd/path mismatch must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("session path mismatch"));
        assert!(
            error
                .to_string()
                .contains(&crate::util::grok_home::encode_cwd_dirname(SUMMARY_CWD))
        );
        assert!(
            error
                .to_string()
                .contains(&crate::util::grok_home::encode_cwd_dirname(PHYSICAL_CWD))
        );
    }

    #[test]
    fn remote_pull_publication_integrity_error_survives_pull_on_miss_mapping() {
        let original = io::Error::new(io::ErrorKind::NotFound, "original local miss");
        let integrity = io::Error::new(
            io::ErrorKind::InvalidData,
            "backend returned a mismatched session identity",
        );

        let error = map_pull_on_miss_result(original, Err(integrity))
            .err()
            .expect("integrity error must not collapse into the local miss");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "backend returned a mismatched session identity"
        );

        let original = io::Error::new(io::ErrorKind::NotFound, "original local miss");
        let error = map_pull_on_miss_result(original, Ok(None))
            .err()
            .expect("genuine remote absence must retain the original local miss");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(error.to_string(), "original local miss");
    }

    #[test]
    fn production_load_light_waits_for_publish_then_reads_session() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000109";
        const CWD: &str = "/repo/production-load-light/publish";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator =
            spawn_claim_child("production-new-hold-publish", root.path(), SESSION_ID, CWD);
        let mut loader = spawn_claim_child("production-load-light", root.path(), SESSION_ID, CWD);
        assert_claim_child_is_blocked(&mut loader, "production persistence::load_light");

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "PUBLISHED");
        assert_child_result(&finish_claim_child(loader), "PRESENT");
    }

    #[test]
    fn production_load_waits_for_publish_then_reads_session() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000111";
        const CWD: &str = "/repo/production-load/publish";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator =
            spawn_claim_child("production-new-hold-publish", root.path(), SESSION_ID, CWD);
        let mut loader = spawn_claim_child("production-load", root.path(), SESSION_ID, CWD);
        assert_claim_child_is_blocked(&mut loader, "production persistence::load");

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "PUBLISHED");
        assert_child_result(&finish_claim_child(loader), "PRESENT");
    }

    #[test]
    fn production_load_waits_for_abort_then_returns_not_found() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000110";
        const CWD: &str = "/repo/production-load/abort";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator = spawn_claim_child("production-new-hold-abort", root.path(), SESSION_ID, CWD);
        let mut loader = spawn_claim_child("production-load", root.path(), SESSION_ID, CWD);
        assert_claim_child_is_blocked(&mut loader, "production persistence::load");

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "ABORTED");
        assert_child_result(&finish_claim_child(loader), "ERROR:NotFound");
    }

    #[test]
    fn production_load_light_waits_for_abort_then_returns_not_found() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000112";
        const CWD: &str = "/repo/production-load-light/abort";

        let root = tempfile::tempdir().expect("temporary grok home");
        let creator = spawn_claim_child("production-new-hold-abort", root.path(), SESSION_ID, CWD);
        let mut loader = spawn_claim_child("production-load-light", root.path(), SESSION_ID, CWD);
        assert_claim_child_is_blocked(&mut loader, "production persistence::load_light");

        release_claim_child(root.path());
        assert_child_result(&finish_claim_child(creator), "ABORTED");
        assert_child_result(&finish_claim_child(loader), "ERROR:NotFound");
    }

    #[test]
    fn armed_claim_cleans_only_its_retained_private_stage() {
        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000105";
        let root = tempfile::tempdir().expect("temporary grok home");
        let session_dir = test_session_dir(root.path(), "/repo/cancelled", SESSION_ID);

        let claim = claim_fresh_session_sync(root.path(), SESSION_ID, session_dir.clone())
            .expect("claim new path");
        let private_stage = claim.publication.stage_container.clone();
        std::fs::write(claim.publication.stage_session.join("summary.json"), b"{}")
            .expect("simulate private initialized summary");
        std::fs::create_dir_all(&session_dir).expect("create unrelated public target");
        std::fs::write(session_dir.join("owned-by-someone-else"), b"keep")
            .expect("write unrelated public content");
        drop(claim);
        assert!(
            !private_stage.exists(),
            "dropping an armed claim must clean its retained private stage"
        );
        assert!(
            session_dir.join("owned-by-someone-else").is_file(),
            "retained cleanup must preserve public pathname data"
        );
    }
}

#[cfg(test)]
mod actor_lifetime_tests {
    use super::*;

    #[tokio::test]
    async fn dropping_the_session_handle_closes_the_actor_channel() {
        let (handle, mut rx, summary_tx, _disk_full_tx) = actor_channel(None);

        drop(handle);

        assert!(
            summary_tx.upgrade().is_none(),
            "the generator's sender must not keep the channel open"
        );
        assert!(
            rx.recv().await.is_none(),
            "the actor's receive loop must end once the session drops its handle"
        );
    }
}
