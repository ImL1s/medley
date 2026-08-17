//! Centralized unified log for cross-component session observability.
//!
//! Shell writes directly via [`emit()`]. Pager and desktop forward entries
//! over ACP (`x.ai/log` notifications); shell receives them in
//! [`ingest_client_entries()`] and writes on their behalf.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use xai_grok_auth::{CredentialComparison, SentCredentialRelation};
use xai_grok_config::grok_home;

/// Binary version stamped into every log entry. Set once at startup via
/// [`set_version()`]; entries emitted before that get `None`.
static VERSION: OnceLock<String> = OnceLock::new();

/// Register the binary version (e.g. shell's `CARGO_PKG_VERSION`).
/// Call once at startup; subsequent calls are no-ops.
pub fn set_version(ver: &str) {
    let _ = VERSION.set(ver.to_owned());
}

pub const LOG_DIR: &str = "logs";
const LOG_FILE: &str = "unified.jsonl";
pub const MAX_SIZE: u64 = 5 * 1024 * 1024; // 5 MB

/// Safety schema required before a unified-log record may leave the machine.
///
/// Records without this exact value may have been written by a legacy process
/// that emitted credential fragments, so upload filtering fails closed.
pub const CURRENT_CREDENTIAL_SAFETY_SCHEMA: u16 = 1;

/// ACP method name for unified log notifications.
pub const LOG_METHOD: &str = "x.ai/log";

// ---------------------------------------------------------------------------
// Log entry types
// ---------------------------------------------------------------------------

/// Log level for a unified log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display, Serialize, Deserialize)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

/// Component that produced a log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display, Serialize, Deserialize)]
pub enum LogSource {
    #[strum(serialize = "shell")]
    #[serde(rename = "shell")]
    Shell,
    #[strum(serialize = "grok-pager")]
    #[serde(rename = "grok-pager")]
    GrokPager,
    #[strum(serialize = "grok-desktop")]
    #[serde(rename = "grok-desktop")]
    GrokDesktop,
}

/// Finite consumer categories accepted by the credential-diagnostic upload
/// contract. No arbitrary string crosses this boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialDiagnosticConsumer {
    OaiCompatChatCompletionsStream,
    OaiCompatChatCompletions,
    OaiCompatResponsesStream,
    OaiCompatResponses,
    OaiCompatMessagesStream,
    OaiCompatMessages,
    StorageGetUploadLimits,
    StorageCheckExists,
    StorageBatchCheckExists,
    StorageBatchUpload,
    StorageBatchUploadJson,
    StorageDownloadBlob,
    StorageUpload,
    StorageUploadFile,
    StorageUploadStream,
    StorageMultipartInit,
    StorageMultipartComplete,
    StorageGetSignedUploadUrl,
    StorageUploadPart,
    FeedbackSignalsUpdate,
    FeedbackEventRecording,
    FeedbackSubmission,
    FeedbackCompleteRequest,
    FeedbackDismissRequest,
    FeedbackCreateRequest,
    FeedbackFetchConfig,
    FeedbackSendTurnDelta,
    SessionRegistryRegister,
    SessionRegistryUpdate,
    SessionRegistryFinalize,
    SessionRegistrySearch,
    SessionRegistryGet,
    SessionRegistryDownloadUrl,
    SessionRegistryDownload,
    IdleResumeModelRefresh,
    ImageGen,
    VideoGenStart,
    VideoGenPoll,
    WebSearch,
}

impl CredentialDiagnosticConsumer {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OaiCompatChatCompletionsStream => "OaiCompatClient.chat_completions_stream",
            Self::OaiCompatChatCompletions => "OaiCompatClient.chat_completions",
            Self::OaiCompatResponsesStream => "OaiCompatClient.responses_stream",
            Self::OaiCompatResponses => "OaiCompatClient.responses",
            Self::OaiCompatMessagesStream => "OaiCompatClient.messages_stream",
            Self::OaiCompatMessages => "OaiCompatClient.messages",
            Self::StorageGetUploadLimits => "StorageClient.get_upload_limits",
            Self::StorageCheckExists => "StorageClient.check_exists",
            Self::StorageBatchCheckExists => "StorageClient.batch_check_exists",
            Self::StorageBatchUpload => "StorageClient.batch_upload",
            Self::StorageBatchUploadJson => "StorageClient.batch_upload_json",
            Self::StorageDownloadBlob => "StorageClient.download_blob",
            Self::StorageUpload => "StorageClient.upload",
            Self::StorageUploadFile => "StorageClient.upload_file",
            Self::StorageUploadStream => "StorageClient.upload_stream",
            Self::StorageMultipartInit => "StorageClient.multipart_init",
            Self::StorageMultipartComplete => "StorageClient.multipart_complete",
            Self::StorageGetSignedUploadUrl => "StorageClient.get_signed_upload_url",
            Self::StorageUploadPart => "StorageClient.upload_part",
            Self::FeedbackSignalsUpdate => "FeedbackClient.signals_update",
            Self::FeedbackEventRecording => "FeedbackClient.event_recording",
            Self::FeedbackSubmission => "FeedbackClient.feedback_submission",
            Self::FeedbackCompleteRequest => "FeedbackClient.complete_request",
            Self::FeedbackDismissRequest => "FeedbackClient.dismiss_request",
            Self::FeedbackCreateRequest => "FeedbackClient.create_request",
            Self::FeedbackFetchConfig => "FeedbackClient.fetch_config",
            Self::FeedbackSendTurnDelta => "FeedbackClient.send_turn_delta",
            Self::SessionRegistryRegister => "SessionRegistryClient.register",
            Self::SessionRegistryUpdate => "SessionRegistryClient.update",
            Self::SessionRegistryFinalize => "SessionRegistryClient.finalize",
            Self::SessionRegistrySearch => "SessionRegistryClient.search",
            Self::SessionRegistryGet => "SessionRegistryClient.get",
            Self::SessionRegistryDownloadUrl => "SessionRegistryClient.download_url",
            Self::SessionRegistryDownload => "SessionRegistryClient.download",
            Self::IdleResumeModelRefresh => "IdleResumeModelRefresh",
            Self::ImageGen => "ImageGen",
            Self::VideoGenStart => "VideoGen.start",
            Self::VideoGenPoll => "VideoGen.poll",
            Self::WebSearch => "WebSearch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SafeSentCredentialRelation {
    NotSent,
    CurrentUnavailable,
    SameAsCurrent,
    DifferentFromCurrent,
}

impl From<SentCredentialRelation> for SafeSentCredentialRelation {
    fn from(value: SentCredentialRelation) -> Self {
        match value {
            SentCredentialRelation::NotSent => Self::NotSent,
            SentCredentialRelation::CurrentUnavailable => Self::CurrentUnavailable,
            SentCredentialRelation::SameAsCurrent => Self::SameAsCurrent,
            SentCredentialRelation::DifferentFromCurrent => Self::DifferentFromCurrent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialAttributionContext {
    sent_credential_relation: SafeSentCredentialRelation,
    sent_credential_present: bool,
    current_credential_present: bool,
    mint_age_seconds: i64,
    expires_at_seconds_from_now: i64,
    consumer: CredentialDiagnosticConsumer,
    is_stale_snapshot: bool,
}

impl CredentialAttributionContext {
    fn new(
        consumer: CredentialDiagnosticConsumer,
        comparison: CredentialComparison,
        mint_age_seconds: i64,
        expires_at_seconds_from_now: i64,
    ) -> Self {
        Self {
            sent_credential_relation: comparison.relation.into(),
            sent_credential_present: comparison.sent_credential_present(),
            current_credential_present: comparison.current_credential_present,
            mint_age_seconds,
            expires_at_seconds_from_now,
            consumer,
            is_stale_snapshot: comparison.relation == SentCredentialRelation::DifferentFromCurrent,
        }
    }

    fn is_consistent(&self) -> bool {
        let expected = match self.sent_credential_relation {
            SafeSentCredentialRelation::NotSent => (false, self.current_credential_present, false),
            SafeSentCredentialRelation::CurrentUnavailable => (true, false, false),
            SafeSentCredentialRelation::SameAsCurrent => (true, true, false),
            SafeSentCredentialRelation::DifferentFromCurrent => (true, true, true),
        };
        (
            self.sent_credential_present,
            self.current_credential_present,
            self.is_stale_snapshot,
        ) == expected
    }
}

const CREDENTIAL_ATTRIBUTION_MESSAGE: &str = "auth 401 attribution";
const UPLOAD_FAILURE_MESSAGE: &str = "file upload failed";

/// Closed event discriminator for records eligible for diagnostic upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SafeUploadEvent {
    Auth401Attribution,
    UploadFailure,
    UploadRecovered,
}

/// Closed artifact category for operational upload failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadFailureArtifact {
    TraceArtifact,
}

/// Closed failure category; raw error strings never cross this boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadFailureReason {
    UploadFailed,
    GcsUploadFailed,
    DirectUploadFailed,
    DirectUploadTimedOut,
    Other,
}

/// Closed upload backend category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadFailureMethod {
    DirectGcs,
    Proxy,
    DirectS3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UploadFailureContext {
    artifact: UploadFailureArtifact,
    reason: UploadFailureReason,
    method: UploadFailureMethod,
    phase_present: bool,
    status_code: Option<u16>,
    bytes: Option<u64>,
    suppressed_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UploadRecoveryContext {
    method: UploadFailureMethod,
    prior_failure_count: u64,
}

const UPLOAD_RECOVERED_MESSAGE: &str = "file upload recovered";

/// A single unified log entry, written as one JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Credential-observability contract understood by the export filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_safety_schema: Option<u16>,
    /// RFC 3339 timestamp (millisecond precision, UTC).
    pub ts: String,
    /// Component that produced the entry.
    pub src: LogSource,
    /// OS process id of the producer. Critical for cross-process trace
    /// reconstruction because shell/pager/desktop all append to the same
    /// `unified.jsonl`, so multiple shell processes' lines interleave
    /// indistinguishably without it.
    ///
    /// `Option<u32>` is for wire compatibility only -- shell, pager, and
    /// desktop all stamp `Some(std::process::id())` at emit time. A
    /// `None` here means the entry came from an older client/server that
    /// predates this field; current code never emits one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Binary version (e.g. `"0.1.211"`). Stamped by [`set_version()`]
    /// at startup so stale zombie processes are identifiable in logs.
    /// `None` for entries from older binaries that predate this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ver: Option<String>,
    /// Log level.
    pub lvl: LogLevel,
    /// Session ID, if one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    /// Human-readable message.
    pub msg: String,
    /// Structured context fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx: Option<serde_json::Value>,
}

/// Wire format for the `x.ai/log` ACP notification params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogNotificationParams {
    /// Source component identifier.
    pub src: LogSource,
    pub entries: Vec<ClientLogEntry>,
}

/// Entry as sent by a client (no `src` field -- shell stamps it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientLogEntry {
    /// Copied verbatim by the shell. Legacy clients remain unmarked and their
    /// records are therefore ineligible for upload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_safety_schema: Option<u16>,
    pub ts: String,
    /// Client process id. Stamped by the client when the entry is
    /// created; preserved through ACP forwarding so the on-disk log
    /// reflects the originating process.
    ///
    /// Optional only for wire compatibility with clients that predate
    /// this field; in-tree clients always populate it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Binary version. Optional for wire compatibility with older clients.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ver: Option<String>,
    pub lvl: LogLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
    pub msg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctx: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// How often a writer re-checks that its handle still refers to the file at
/// `path`, and that the file is still under [`MAX_SIZE`].
///
/// Time-based rather than byte-based so a low-volume process detects a stale
/// handle just as fast as a chatty one — a process logging one line a minute
/// is precisely the one that would otherwise write into an unlinked inode for
/// hours without noticing.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(2);

struct LogWriter {
    file: File,
    path: PathBuf,
    /// Identity of the inode this handle refers to, re-checked against the
    /// path on the maintenance cadence. `None` on platforms with no cheap
    /// stable file id, where only disappearance is detectable.
    identity: Option<FileIdentity>,
    last_maintenance: Instant,
    /// Set when `path` stopped resolving to our inode **and** reopening it
    /// failed. Writes are dropped while it is set.
    ///
    /// Continuing to append to the old descriptor would be the exact failure
    /// this module was changed to end: bytes land in a file no reader can
    /// find and no process will ever trim. Dropping them is not a loss —
    /// those bytes were already unreadable — and it avoids growing an
    /// invisible file on a disk that is quite possibly full, which is one of
    /// the few ways the reopen fails in the first place. Cleared by the next
    /// successful reopen, retried on the maintenance cadence.
    detached: bool,
}

/// `(dev, ino)` on Unix. Enough to notice that the path now resolves to a
/// different inode than the one we hold open.
type FileIdentity = (u64, u64);

static WRITER: LazyLock<Mutex<Option<LogWriter>>> = LazyLock::new(|| Mutex::new(open_writer()));

/// See [`redirect_to_temp_for_tests`].
static TEST_REDIRECT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Redirect all subsequent unified-log writes **and** snapshot reads to a
/// per-process file under the system temp directory, so test binaries stop
/// writing synthetic events into the developer's real
/// `~/.grok/logs/unified.jsonl` (those bursts inflate exactly the counters
/// an incident responder greps for). Runtime-activated rather than a cargo
/// feature: Bazel compiles production and test targets with one shared
/// feature set, so a feature gate would leak into production builds.
///
/// Idempotent and safe at any point: an already-open writer is re-pointed,
/// so an emit that precedes the redirect cannot pin the real path. Test
/// binaries install it pre-main via `#[ctor]`.
pub fn redirect_to_temp_for_tests() {
    TEST_REDIRECT.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut guard) = WRITER.lock() {
        *guard = open_writer();
    }
}

fn log_path() -> PathBuf {
    if TEST_REDIRECT.load(std::sync::atomic::Ordering::Relaxed) {
        return test_log_dir().join(LOG_FILE);
    }
    grok_home().join(LOG_DIR).join(LOG_FILE)
}

/// Owner-only (0o700), freshly-created directory for the test redirect.
///
/// The stream carries path metadata and credential tail fragments, and the
/// system temp dir is world-writable on Linux: a pre-planted directory or
/// symlink would let another local user read the file — or make the writer
/// and [`trim_file`] operate through a symlink onto a victim file. The
/// non-recursive `create` fails on any pre-existing path instead of
/// adopting it, and the nanos component makes the name unpredictable.
/// Panicking on failure is deliberate: this branch only runs in test
/// binaries, and silently falling back would reopen the hole via
/// `open_writer_at`'s `create_dir_all`.
fn test_log_dir() -> &'static PathBuf {
    static TEST_LOG_DIR: OnceLock<PathBuf> = OnceLock::new();
    TEST_LOG_DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "grok-unified-log-test-{}-{nanos}",
            std::process::id()
        ));
        #[allow(unused_mut)]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&dir)
            .expect("create private unified-log test dir");
        dir
    })
}

pub fn file_size(path: &std::path::Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Identity of whatever file currently lives at `path`, or `None` if nothing
/// does. Compared against the identity captured at open time to detect that
/// our descriptor has been orphaned by a rename or an unlink.
#[cfg(unix)]
fn path_identity(path: &std::path::Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

/// Windows has no comparably cheap stable id from a path stat, so this
/// degrades to presence detection: a deleted log is still healed, a replaced
/// one is not.
#[cfg(not(unix))]
fn path_identity(path: &std::path::Path) -> Option<FileIdentity> {
    fs::metadata(path).ok().map(|_| (0, 0))
}

fn open_writer() -> Option<LogWriter> {
    open_writer_at(log_path())
}

/// Open (creating if needed) a writer for an explicit path.
///
/// Split from [`open_writer`] so a writer re-points at **its own** path when
/// healing a stale handle rather than re-resolving `$GROK_HOME` — which also
/// makes the healing path testable against a temp directory.
fn open_writer_at(path: PathBuf) -> Option<LogWriter> {
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        tracing::warn!("[unified_log] failed to create log dir: {e}");
        return None;
    }

    if file_size(&path) >= MAX_SIZE {
        trim_file(&path);
    }

    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => Some(LogWriter {
            file,
            identity: path_identity(&path),
            path,
            last_maintenance: Instant::now(),
            detached: false,
        }),
        Err(e) => {
            tracing::warn!("[unified_log] failed to open log file: {e}");
            None
        }
    }
}

impl LogWriter {
    /// Re-point at the live file if ours was replaced or removed, and trim if
    /// the file has grown past [`MAX_SIZE`].
    ///
    /// The size check reads the **real** file rather than a per-process byte
    /// counter. A counter only sees this process's own writes, so several
    /// writers sharing one log each believed they were far below the cap while
    /// the file sailed past it — orphaned writers observed at 8.5 MB against a
    /// 5 MB cap.
    ///
    /// Returns whether the handle is safe to write to: `false` once the file
    /// has been replaced or removed and reopening it did not work, so the
    /// caller drops the entry instead of appending it somewhere unreadable.
    fn maintain(&mut self) -> bool {
        if self.last_maintenance.elapsed() < MAINTENANCE_INTERVAL {
            return !self.detached;
        }
        self.last_maintenance = Instant::now();

        if path_identity(&self.path) != self.identity {
            let Some(reopened) = open_writer_at(self.path.clone()) else {
                // Warn on entering the state, not once per tick: a broken log
                // directory would otherwise flood the diagnostic output an
                // operator is trying to read.
                if !self.detached {
                    tracing::warn!(
                        path = %self.path.display(),
                        "[unified_log] log file replaced or removed and reopen failed; \
                         dropping entries until it can be reopened"
                    );
                    self.detached = true;
                }
                return false;
            };
            *self = reopened;
            return true;
        }

        // The path resolves to our inode again — either it always did, or a
        // transient stat failure cleared.
        self.detached = false;

        if file_size(&self.path) >= MAX_SIZE {
            let _ = self.file.flush();
            trim_file(&self.path);
        }
        true
    }
}

fn write_lines(lines: &[u8]) {
    let Ok(mut guard) = WRITER.lock() else { return };
    let writer = match guard.as_mut() {
        Some(w) => w,
        None => return,
    };
    if !writer.maintain() {
        return;
    }

    if let Err(e) = writer.file.write_all(lines) {
        tracing::warn!("[unified_log] write failed: {e}");
    }
}

fn write_entry(entry: &LogEntry) {
    let Ok(mut line) = serde_json::to_vec(entry) else {
        return;
    };
    line.push(b'\n');
    write_lines(&line);
}

/// Drop the oldest lines from the file, keeping roughly the last half,
/// **preserving the inode**.
///
/// Rewrites the retained tail at offset 0 and truncates to match. This must
/// not go through temp + rename: every other process holds an `O_APPEND`
/// descriptor on this inode, and swapping a fresh file in underneath them
/// leaves each one appending to an unlinked inode that nothing can read and
/// nothing will ever trim. That failure was silent and unbounded — a single
/// developer machine accumulated roughly 26 MB across six orphaned inodes,
/// several of them past the 5 MB cap, while the visible log held only what
/// the most recent trimming process happened to write. The unified log was
/// therefore blind during the incident it exists to explain.
///
/// Truncating in place trades the rename's crash-atomicity for the far more
/// valuable property that concurrent writers keep working. A crash between
/// the write and the `set_len` leaves the tail followed by stale bytes; for a
/// line-delimited diagnostic log that costs at most a few garbled lines,
/// against losing every sibling's output indefinitely.
///
/// A sibling appending *during* the rewrite may lose that one line to the
/// truncation. The previous implementation lost every line written after the
/// rename, forever.
///
/// The whole read-modify-write is held under an exclusive advisory lock on
/// the log itself, because trimming in place is only safe for one process at
/// a time — see the comment in the body.
///
/// Known limitation: a single line longer than half the file leaves no
/// newline to cut at, and the trim is skipped rather than split that line.
/// The log then stays over its cap until a shorter line arrives.
pub fn trim_file(path: &std::path::Path) {
    // One trimmer at a time, across processes. Writers decide on the real
    // on-disk size, so when the log crosses the cap every process reaches
    // this function inside the same maintenance window. Two of them
    // interleaving a multi-megabyte rewrite at offset 0 would splice one
    // tail into the other; worse, a trimmer that reads while another is
    // mid-rewrite sees new-tail-over-old-head and computes its own tail from
    // that. Temp + rename was no safer — every process used the same
    // `unified.jsonl.tmp` — it was just rarer, because the old per-process
    // byte counter meant one process did essentially all the trimming.
    //
    // `try_lock`, not `lock`: a contended trim is one somebody else is
    // already doing, so there is nothing to wait for, and waiting would park
    // this process's writer mutex on a foreign process's I/O.
    //
    // A trimmer that decided to trim just before another one finished will
    // find a freshly halved file and halve it again. Losing another half of
    // an over-budget diagnostic log is a far cheaper outcome than interleaved
    // rewrites, so the size is deliberately not re-checked here: callers
    // trim on their own terms and the unit tests trim small files directly.
    let Ok(mut file) = OpenOptions::new().read(true).write(true).open(path) else {
        return;
    };
    if file.try_lock().is_err() {
        return;
    }

    let mut data = Vec::new();
    if let Err(e) = file.read_to_end(&mut data) {
        tracing::warn!("[unified_log] trim read failed: {e}");
        return;
    }
    let half = data.len() / 2;
    // Find the first newline after the halfway point so we don't split a line.
    let start = match data[half..].iter().position(|&b| b == b'\n') {
        Some(pos) => half + pos + 1,
        None => return,
    };
    let tail = &data[start..];

    // Rewind rather than truncate-on-open: the tail is laid down over the
    // head first, and only then is the file shortened, so the retained bytes
    // are never absent from disk.
    if file.rewind().is_err() {
        return;
    }
    if let Err(e) = file.write_all(tail) {
        tracing::warn!("[unified_log] trim rewrite failed: {e}");
        return;
    }
    let _ = file.set_len(tail.len() as u64);
    let _ = file.flush();
    // The lock is released when `file` drops.
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Return a new timestamp string in the unified log format.
fn now_ts() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Emit a log entry from shell itself.
pub fn emit(lvl: LogLevel, msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    let entry = LogEntry {
        // Generic messages and JSON context are intentionally not self-attested
        // as upload-safe. Only typed constructors may stamp the safety schema.
        credential_safety_schema: None,
        ts: now_ts(),
        src: LogSource::Shell,
        pid: Some(std::process::id()),
        ver: VERSION.get().cloned(),
        lvl,
        sid: sid.map(Into::into),
        msg: msg.into(),
        ctx,
    };
    write_entry(&entry);
}

/// Emit the schema-1 credential-attribution shape: a credential-free 401
/// relation composed only of enums, booleans, and integers.
pub fn emit_credential_attribution(
    consumer: CredentialDiagnosticConsumer,
    comparison: CredentialComparison,
    mint_age_seconds: i64,
    expires_at_seconds_from_now: i64,
    sid: Option<&str>,
) {
    let context = CredentialAttributionContext::new(
        consumer,
        comparison,
        mint_age_seconds,
        expires_at_seconds_from_now,
    );
    let entry = LogEntry {
        credential_safety_schema: Some(CURRENT_CREDENTIAL_SAFETY_SCHEMA),
        ts: now_ts(),
        src: LogSource::Shell,
        pid: Some(std::process::id()),
        ver: VERSION.get().cloned(),
        lvl: LogLevel::Warn,
        sid: sid.map(Into::into),
        msg: CREDENTIAL_ATTRIBUTION_MESSAGE.to_string(),
        ctx: serde_json::to_value(context).ok(),
    };
    write_entry(&entry);
}

/// Emit a credential-free operational upload failure using only closed enums,
/// booleans, and bounded numeric metadata.
pub fn emit_upload_failure(
    artifact: UploadFailureArtifact,
    reason: UploadFailureReason,
    method: UploadFailureMethod,
    phase_present: bool,
    status_code: Option<u16>,
    bytes: Option<u64>,
    suppressed_count: u64,
    sid: Option<&str>,
) {
    let context = UploadFailureContext {
        artifact,
        reason,
        method,
        phase_present,
        status_code,
        bytes,
        suppressed_count,
    };
    let entry = LogEntry {
        credential_safety_schema: Some(CURRENT_CREDENTIAL_SAFETY_SCHEMA),
        ts: now_ts(),
        src: LogSource::Shell,
        pid: Some(std::process::id()),
        ver: VERSION.get().cloned(),
        lvl: if method == UploadFailureMethod::DirectS3 {
            LogLevel::Warn
        } else {
            LogLevel::Error
        },
        sid: sid.map(Into::into),
        msg: UPLOAD_FAILURE_MESSAGE.to_owned(),
        ctx: serde_json::to_value(context).ok(),
    };
    write_entry(&entry);
}

/// Emit a low-volume lifecycle event when a successful upload closes an
/// existing failure episode.
pub fn emit_upload_recovered(
    method: UploadFailureMethod,
    prior_failure_count: u64,
    sid: Option<&str>,
) {
    let entry = LogEntry {
        credential_safety_schema: Some(CURRENT_CREDENTIAL_SAFETY_SCHEMA),
        ts: now_ts(),
        src: LogSource::Shell,
        pid: Some(std::process::id()),
        ver: VERSION.get().cloned(),
        lvl: LogLevel::Info,
        sid: sid.map(Into::into),
        msg: UPLOAD_RECOVERED_MESSAGE.to_owned(),
        ctx: serde_json::to_value(UploadRecoveryContext {
            method,
            prior_failure_count,
        })
        .ok(),
    };
    write_entry(&entry);
}

/// Ingest a batch of log entries from a client (pager or desktop).
///
/// Called by the `x.ai/log` notification handler. Entries from
/// [`LogSource::Shell`] are rejected to prevent spoofing.
pub fn ingest_client_entries(src: LogSource, entries: &[ClientLogEntry]) {
    if matches!(src, LogSource::Shell) || entries.is_empty() {
        return;
    }
    // Serialize all entries up front, then write in a single lock acquisition.
    let mut buf = Vec::new();
    for client_entry in entries {
        let entry = client_entry_to_log_entry(src, client_entry);
        if let Ok(mut line) = serde_json::to_vec(&entry) {
            line.push(b'\n');
            buf.extend_from_slice(&line);
        }
    }
    if !buf.is_empty() {
        write_lines(&buf);
    }
}

fn client_entry_to_log_entry(src: LogSource, client_entry: &ClientLogEntry) -> LogEntry {
    LogEntry {
        // ACP clients are not trusted to attest free-form message/context
        // safety. Typed safe records are emitted only by the shell.
        credential_safety_schema: None,
        ts: client_entry.ts.clone(),
        src,
        pid: client_entry.pid,
        ver: client_entry.ver.clone(),
        lvl: client_entry.lvl,
        sid: client_entry.sid.clone(),
        msg: client_entry.msg.clone(),
        ctx: client_entry.ctx.clone(),
    }
}

/// Convenience: emit an info-level entry from shell.
pub fn info(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    emit(LogLevel::Info, msg, sid, ctx);
}

/// Convenience: emit a warn-level entry from shell.
pub fn warn(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    emit(LogLevel::Warn, msg, sid, ctx);
}

/// Convenience: emit an error-level entry from shell.
pub fn error(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    emit(LogLevel::Error, msg, sid, ctx);
}

/// Convenience: emit a debug-level entry from shell.
pub fn debug(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    emit(LogLevel::Debug, msg, sid, ctx);
}

/// The resolved log path, for error messages that point the user here.
pub fn path() -> PathBuf {
    log_path()
}

/// Read the current unified log file and return its contents.
///
/// Returns `None` if the log file doesn't exist or can't be read.
/// Used by diagnostic uploads to capture the log state at a point in time.
pub fn snapshot_log() -> Option<Vec<u8>> {
    let path = log_path();
    // Flush pending writes before reading.
    if let Ok(mut guard) = WRITER.lock()
        && let Some(ref mut w) = *guard
    {
        let _ = w.file.flush();
    }
    // Lock released intentionally — snapshot is approximate.
    match fs::read(&path) {
        Ok(data) if !data.is_empty() => Some(data),
        _ => None,
    }
}

/// Read the unified log and return only entries belonging to the given session.
///
/// Parses each JSONL line, keeps entries where `"sid"` matches `session_id`,
/// and returns the filtered lines as JSONL bytes. Returns `None` if the log
/// is empty or contains no entries for this session.
pub fn snapshot_session_log(session_id: &str) -> Option<Vec<u8>> {
    let path = log_path();
    if let Ok(mut guard) = WRITER.lock()
        && let Some(ref mut w) = *guard
    {
        let _ = w.file.flush();
    }
    let data = match fs::read(&path) {
        Ok(d) if !d.is_empty() => d,
        _ => return None,
    };
    let mut out = Vec::new();
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line)
            && entry.get("sid").and_then(|v| v.as_str()) == Some(session_id)
        {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Read the current unified log and return only typed records proven safe to upload.
///
/// The local file is never rewritten. Each line is independently classified,
/// so malformed, legacy, or concurrently appended legacy records are simply
/// omitted while current safe records remain available.
pub fn snapshot_log_for_upload() -> Option<Vec<u8>> {
    flush_writer();
    snapshot_path_for_upload(&log_path(), None)
}

/// Read the current unified log and return upload-safe records for one session.
pub fn snapshot_session_log_for_upload(session_id: &str) -> Option<Vec<u8>> {
    flush_writer();
    snapshot_path_for_upload(&log_path(), Some(session_id))
}

fn flush_writer() {
    if let Ok(mut guard) = WRITER.lock()
        && let Some(ref mut writer) = *guard
    {
        let _ = writer.file.flush();
    }
}

const LOG_ENTRY_FIELDS: &[&str] = &[
    "credential_safety_schema",
    "ts",
    "src",
    "pid",
    "ver",
    "lvl",
    "sid",
    "msg",
    "ctx",
];

/// Exact legacy identifiers shared with the repository static guard.
const LEGACY_CREDENTIAL_FRAGMENT_FIELDS: &[&str] = &[
    "token_suffix",
    "bearer_tail_fragment",
    "StampedBearerSuffix",
    "SENT_BEARER_PREFIX_LEN",
    "bearer_suffix",
    "sent_bearer_prefix",
    "auth_prefix",
    "key_prefix",
    "rt_prefix",
    "sent_key_prefix",
    "current_key_prefix",
    "tried_rt_prefix",
    "disk_rt_prefix",
    "disk_key_prefix",
    "tried_key_prefix",
    "adopted_key_prefix",
    "prev_key_prefix",
    "new_key_prefix",
    "old_key_prefix",
    "retained_key_prefix",
    "dropped_key_prefix",
    "written_key_prefix",
    "child_key_prefix",
    "parent_key_prefix",
    "attempt_bearer",
    "wire_bearer",
    "failed_bearer",
    "deployment_id_from_key",
    "api_key_id_for",
];

fn snapshot_path_for_upload(path: &std::path::Path, session_id: Option<&str>) -> Option<Vec<u8>> {
    let data = match fs::read(path) {
        Ok(data) if !data.is_empty() => data,
        _ => return None,
    };
    filter_upload_lines(&data, session_id)
}

fn filter_upload_lines(data: &[u8], session_id: Option<&str>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for line in data
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        if object
            .keys()
            .any(|key| !LOG_ENTRY_FIELDS.contains(&key.as_str()))
            || object
                .get("credential_safety_schema")
                .and_then(serde_json::Value::as_u64)
                != Some(u64::from(CURRENT_CREDENTIAL_SAFETY_SCHEMA))
            || contains_legacy_credential_field(&value)
        {
            continue;
        }
        let Ok(entry) = serde_json::from_value::<LogEntry>(value) else {
            continue;
        };
        if session_id.is_some_and(|wanted| entry.sid.as_deref() != Some(wanted)) {
            continue;
        }

        let Some(mut safe_value) = typed_upload_value(&entry) else {
            continue;
        };
        redact_json_strings(&mut safe_value);
        let Ok(mut safe_line) = serde_json::to_vec(&safe_value) else {
            continue;
        };
        safe_line.push(b'\n');
        out.extend_from_slice(&safe_line);
    }
    (!out.is_empty()).then_some(out)
}

/// Rebuild a record from typed safe fields instead of forwarding the
/// self-attested input object. Free-form `msg` and `ctx` never cross the
/// upload boundary; session/version strings survive only under a narrow
/// canonical envelope grammar.
fn typed_upload_value(entry: &LogEntry) -> Option<serde_json::Value> {
    if entry.src != LogSource::Shell || entry.pid.is_none() {
        return None;
    }

    let timestamp = chrono::DateTime::parse_from_rfc3339(&entry.ts)
        .ok()?
        .with_timezone(&Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let version = canonical_envelope_value(
        entry.ver.as_deref().filter(|value| {
            value.as_bytes().first().is_some_and(u8::is_ascii_digit) && value.contains('.')
        }),
        64,
        |byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'),
    );
    let session_id = entry
        .sid
        .as_deref()
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .map(|value| value.to_string());

    let (level, message, event, context) =
        if entry.lvl == LogLevel::Warn && entry.msg == CREDENTIAL_ATTRIBUTION_MESSAGE {
            let context: CredentialAttributionContext =
                serde_json::from_value(entry.ctx.clone()?).ok()?;
            if !context.is_consistent() {
                return None;
            }
            (
                LogLevel::Warn,
                CREDENTIAL_ATTRIBUTION_MESSAGE,
                SafeUploadEvent::Auth401Attribution,
                serde_json::to_value(context).ok()?,
            )
        } else if matches!(entry.lvl, LogLevel::Error | LogLevel::Warn)
            && entry.msg == UPLOAD_FAILURE_MESSAGE
        {
            let context: UploadFailureContext = serde_json::from_value(entry.ctx.clone()?).ok()?;
            let expected_level = if context.method == UploadFailureMethod::DirectS3 {
                LogLevel::Warn
            } else {
                LogLevel::Error
            };
            if entry.lvl != expected_level {
                return None;
            }
            (
                expected_level,
                UPLOAD_FAILURE_MESSAGE,
                SafeUploadEvent::UploadFailure,
                serde_json::to_value(context).ok()?,
            )
        } else if entry.lvl == LogLevel::Info && entry.msg == UPLOAD_RECOVERED_MESSAGE {
            let context: UploadRecoveryContext = serde_json::from_value(entry.ctx.clone()?).ok()?;
            if context.prior_failure_count == 0 {
                return None;
            }
            (
                LogLevel::Info,
                UPLOAD_RECOVERED_MESSAGE,
                SafeUploadEvent::UploadRecovered,
                serde_json::to_value(context).ok()?,
            )
        } else {
            return None;
        };

    Some(serde_json::json!({
        "credential_safety_schema": CURRENT_CREDENTIAL_SAFETY_SCHEMA,
        "ts": timestamp,
        "src": LogSource::Shell,
        "pid": entry.pid,
        "ver": version,
        "lvl": level,
        "sid": session_id,
        "msg": message,
        "event": event,
        "ctx": context,
    }))
}

fn canonical_envelope_value<'a>(
    value: Option<&'a str>,
    max_len: usize,
    allowed: impl Fn(u8) -> bool,
) -> Option<&'a str> {
    value.filter(|value| !value.is_empty() && value.len() <= max_len && value.bytes().all(&allowed))
}

fn contains_legacy_credential_field(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, child)| {
            LEGACY_CREDENTIAL_FRAGMENT_FIELDS.contains(&key.as_str())
                || key.ends_with("_credential_prefix")
                || key.ends_with("_credential_suffix")
                || contains_legacy_credential_field(child)
        }),
        serde_json::Value::Array(values) => values.iter().any(contains_legacy_credential_field),
        _ => false,
    }
}

fn redact_json_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(string) => {
            if let Some(redacted) = crate::redact_common::redact_owned(string) {
                *string = redacted;
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(redact_json_strings),
        serde_json::Value::Object(object) => {
            object.values_mut().for_each(redact_json_strings);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANONICAL_SESSION_ID: &str = "019c43b5-c4ae-7190-b058-693e24669ba9";

    fn self_attested_entry(sid: Option<&str>, msg: &str, ctx: serde_json::Value) -> Vec<u8> {
        let entry = serde_json::json!({
            "credential_safety_schema": CURRENT_CREDENTIAL_SAFETY_SCHEMA,
            "ts": "2026-08-01T00:00:00.000Z",
            "src": "shell",
            "pid": 42,
            "ver": "1.0.0",
            "lvl": "warn",
            "sid": sid,
            "msg": msg,
            "ctx": ctx,
        });
        let mut line = serde_json::to_vec(&entry).unwrap();
        line.push(b'\n');
        line
    }

    fn safe_upload_entry(sid: Option<&str>) -> Vec<u8> {
        self_attested_entry(
            sid,
            CREDENTIAL_ATTRIBUTION_MESSAGE,
            serde_json::to_value(CredentialAttributionContext::new(
                CredentialDiagnosticConsumer::OaiCompatResponses,
                CredentialComparison::different_from_current(),
                12,
                -3,
            ))
            .unwrap(),
        )
    }

    fn safe_operational_upload_entry(sid: Option<&str>) -> Vec<u8> {
        let mut value = serde_json::from_slice::<serde_json::Value>(&self_attested_entry(
            sid,
            UPLOAD_FAILURE_MESSAGE,
            serde_json::to_value(UploadFailureContext {
                artifact: UploadFailureArtifact::TraceArtifact,
                reason: UploadFailureReason::GcsUploadFailed,
                method: UploadFailureMethod::DirectGcs,
                phase_present: true,
                status_code: Some(503),
                bytes: Some(4096),
                suppressed_count: 2,
            })
            .unwrap(),
        ))
        .unwrap();
        value["lvl"] = serde_json::json!("error");
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        line
    }

    fn safe_upload_recovery_entry(sid: Option<&str>) -> Vec<u8> {
        let mut value = serde_json::from_slice::<serde_json::Value>(&self_attested_entry(
            sid,
            UPLOAD_RECOVERED_MESSAGE,
            serde_json::to_value(UploadRecoveryContext {
                method: UploadFailureMethod::Proxy,
                prior_failure_count: 3,
            })
            .unwrap(),
        ))
        .unwrap();
        value["lvl"] = serde_json::json!("info");
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        line
    }

    fn assert_no_secret_fragments(rendered: &str, secret: &str) {
        assert!(!rendered.contains(secret), "secret leaked: {rendered}");
        for window in secret.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !rendered.contains(fragment),
                "secret fragment {fragment:?} leaked: {rendered}"
            );
        }
    }

    /// Pre-main, so no test in this binary can race the lazily-opened
    /// writer onto the developer's real `~/.grok/logs/unified.jsonl`.
    #[ctor::ctor]
    fn redirect_for_tests() {
        redirect_to_temp_for_tests();
    }

    /// The redirect must cover both the writer and the snapshot readers:
    /// an emit lands in a per-process temp file, never under `grok_home()`.
    #[test]
    fn redirect_routes_writes_and_snapshots_to_process_temp_file() {
        info(
            "unified-log redirect probe",
            Some("redirect-probe-sid"),
            None,
        );
        let snapshot = snapshot_log().expect("snapshot after emit");
        assert!(
            String::from_utf8_lossy(&snapshot).contains("unified-log redirect probe"),
            "snapshot must read the same redirected file the writer appended to"
        );
        assert!(
            log_path().starts_with(std::env::temp_dir()),
            "the shared file must live under the temp dir, not grok_home(): {}",
            log_path().display()
        );
    }

    #[test]
    fn log_entry_serializes_minimal() {
        let entry = LogEntry {
            credential_safety_schema: Some(CURRENT_CREDENTIAL_SAFETY_SCHEMA),
            ts: "2025-07-14T10:30:00.123Z".into(),
            src: LogSource::Shell,
            pid: None,
            ver: None,
            lvl: LogLevel::Info,
            sid: None,
            msg: "test".into(),
            ctx: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("sid"));
        assert!(!json.contains("ctx"));
        assert!(!json.contains("pid"));
        assert!(!json.contains("ver"));
        assert!(json.contains("\"src\":\"shell\""));
    }

    #[test]
    fn log_entry_serializes_full() {
        let entry = LogEntry {
            credential_safety_schema: Some(CURRENT_CREDENTIAL_SAFETY_SCHEMA),
            ts: "2025-07-14T10:30:00.123Z".into(),
            src: LogSource::GrokPager,
            pid: Some(4242),
            ver: Some("0.1.211".into()),
            lvl: LogLevel::Warn,
            sid: Some("abc123".into()),
            msg: "connection lost".into(),
            ctx: Some(serde_json::json!({"retry": 3})),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"sid\":\"abc123\""));
        assert!(json.contains("\"retry\":3"));
        assert!(json.contains("\"pid\":4242"));
        assert!(json.contains("\"ver\":\"0.1.211\""));
    }

    #[test]
    fn client_entry_round_trip() {
        let wire = r#"{"ts":"2025-07-14T10:30:00.123Z","lvl":"info","msg":"hello"}"#;
        let entry: ClientLogEntry = serde_json::from_str(wire).unwrap();
        assert_eq!(entry.msg, "hello");
        assert!(entry.credential_safety_schema.is_none());
        assert!(entry.sid.is_none());
        assert!(entry.ctx.is_none());
    }

    #[test]
    fn client_ingest_never_trusts_client_safety_attestation() {
        let client: ClientLogEntry = serde_json::from_str(
            r#"{"credential_safety_schema":1,"ts":"2025-07-14T10:30:00.123Z","lvl":"info","msg":"client"}"#,
        )
        .unwrap();
        let ingested = client_entry_to_log_entry(LogSource::GrokPager, &client);
        assert_eq!(ingested.credential_safety_schema, None);
        assert!(
            !serde_json::to_string(&ingested)
                .unwrap()
                .contains("credential_safety_schema"),
            "shell must never trust a free-form client record",
        );
    }

    #[test]
    fn upload_filter_drops_legacy_unknown_malformed_and_fragment_records() {
        let mut input = Vec::new();
        input.extend_from_slice(b"{not json}\n");
        input.extend_from_slice(
            br#"{"ts":"2026-08-01T00:00:00Z","src":"shell","lvl":"info","msg":"legacy"}"#,
        );
        input.push(b'\n');
        input.extend_from_slice(
            br#"{"credential_safety_schema":2,"ts":"2026-08-01T00:00:00Z","src":"shell","lvl":"info","msg":"future"}"#,
        );
        input.push(b'\n');
        input.extend_from_slice(&self_attested_entry(
            Some("safe-session"),
            "nested fragment",
            serde_json::json!({"nested": [{"current_key_prefix": "CANARY"}]}),
        ));
        let mut unknown = serde_json::from_slice::<serde_json::Value>(&self_attested_entry(
            Some("safe-session"),
            "unknown top level",
            serde_json::json!({}),
        ))
        .unwrap();
        unknown["unexpected"] = serde_json::json!(true);
        input.extend_from_slice(&serde_json::to_vec(&unknown).unwrap());
        input.push(b'\n');
        input.extend_from_slice(&safe_upload_entry(Some("safe-session")));

        let output = filter_upload_lines(&input, None).expect("one safe record remains");
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(CREDENTIAL_ATTRIBUTION_MESSAGE));
        assert!(output.contains("OaiCompatResponses"));
        for rejected in [
            "legacy",
            "future",
            "nested fragment",
            "unknown top level",
            "CANARY",
        ] {
            assert!(
                !output.contains(rejected),
                "rejected record survived: {rejected}"
            );
        }
        assert_eq!(output.lines().count(), 1);
    }

    #[test]
    fn upload_filter_preserves_typed_operational_event_and_session_envelope() {
        let mut input = safe_operational_upload_entry(Some(CANONICAL_SESSION_ID));
        input.extend_from_slice(&safe_upload_entry(Some("session-b")));

        let output = filter_upload_lines(&input, Some(CANONICAL_SESSION_ID))
            .expect("typed operational event remains");
        let lines = String::from_utf8(output).unwrap();
        assert_eq!(lines.lines().count(), 1);
        let value: serde_json::Value = serde_json::from_str(lines.trim()).unwrap();
        assert_eq!(value["credential_safety_schema"], 1);
        assert_eq!(value["ts"], "2026-08-01T00:00:00.000Z");
        assert_eq!(value["src"], "shell");
        assert_eq!(value["lvl"], "error");
        assert_eq!(value["pid"], 42);
        assert_eq!(value["ver"], "1.0.0");
        assert_eq!(value["sid"], CANONICAL_SESSION_ID);
        assert_eq!(value["msg"], UPLOAD_FAILURE_MESSAGE);
        assert_eq!(value["event"], "upload_failure");
        assert_eq!(value["ctx"]["artifact"], "trace_artifact");
        assert_eq!(value["ctx"]["reason"], "gcs_upload_failed");
        assert_eq!(value["ctx"]["method"], "direct_gcs");
        assert_eq!(value["ctx"]["phase_present"], true);
        assert_eq!(value["ctx"]["status_code"], 503);
        assert_eq!(value["ctx"]["bytes"], 4096);
        assert_eq!(value["ctx"]["suppressed_count"], 2);
    }

    #[test]
    fn upload_filter_preserves_non_auth_recovery_lifecycle_without_secret_windows() {
        const SECRET: &str = "M8n6B4v2C0x8Z6a4S2d0F8g6H4j2K0l8";
        let mut value = serde_json::from_slice::<serde_json::Value>(&safe_upload_recovery_entry(
            Some(CANONICAL_SESSION_ID),
        ))
        .unwrap();
        value["ctx"]["error"] = serde_json::json!(SECRET);
        value["ctx"]["gcs_path"] = serde_json::json!(format!("bucket/{SECRET}/trace"));
        let mut input = serde_json::to_vec(&value).unwrap();
        input.push(b'\n');

        let output = filter_upload_lines(&input, Some(CANONICAL_SESSION_ID))
            .expect("typed upload recovery lifecycle remains");
        let rendered = String::from_utf8(output).unwrap();
        let projected: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(projected["ts"], "2026-08-01T00:00:00.000Z");
        assert_eq!(projected["src"], "shell");
        assert_eq!(projected["pid"], 42);
        assert_eq!(projected["ver"], "1.0.0");
        assert_eq!(projected["lvl"], "info");
        assert_eq!(projected["sid"], CANONICAL_SESSION_ID);
        assert_eq!(projected["msg"], UPLOAD_RECOVERED_MESSAGE);
        assert_eq!(projected["event"], "upload_recovered");
        assert_eq!(projected["ctx"]["method"], "proxy");
        assert_eq!(projected["ctx"]["prior_failure_count"], 3);
        assert!(projected["ctx"].get("error").is_none());
        assert!(projected["ctx"].get("gcs_path").is_none());
        assert_no_secret_fragments(&rendered, SECRET);
    }

    #[test]
    fn upload_filter_preserves_auth_event_canonical_version_and_session() {
        let output = filter_upload_lines(
            &safe_upload_entry(Some(CANONICAL_SESSION_ID)),
            Some(CANONICAL_SESSION_ID),
        )
        .expect("auth event remains");
        let value: serde_json::Value = serde_json::from_slice(output.trim_ascii()).unwrap();
        assert_eq!(value["ver"], "1.0.0");
        assert_eq!(value["sid"], CANONICAL_SESSION_ID);
        assert_eq!(value["event"], "auth401_attribution");
    }

    #[test]
    fn upload_filter_drops_unsafe_operational_fields_without_dropping_event() {
        const SECRET: &str = "Q7w5E3r1T9y7U5i3O1p9A7s5D3f1H9j7";
        let mut value = serde_json::from_slice::<serde_json::Value>(
            &safe_operational_upload_entry(Some(CANONICAL_SESSION_ID)),
        )
        .unwrap();
        value["ctx"]["error"] = serde_json::json!(SECRET);
        value["ctx"]["gcs_path"] = serde_json::json!(format!("bucket/{SECRET}/artifact"));
        value["ctx"]["free_form"] = serde_json::json!({"nested": SECRET});
        let mut input = serde_json::to_vec(&value).unwrap();
        input.push(b'\n');

        let output = filter_upload_lines(&input, Some(CANONICAL_SESSION_ID))
            .expect("known typed event remains after unsafe fields are projected out");
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("\"event\":\"upload_failure\""));
        let projected: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        for unsafe_field in ["error", "gcs_path", "free_form"] {
            assert!(projected["ctx"].get(unsafe_field).is_none());
        }
        assert_no_secret_fragments(&rendered, SECRET);
    }

    #[test]
    fn upload_filter_keeps_untyped_and_client_operational_records_fail_closed() {
        let mut client = serde_json::from_slice::<serde_json::Value>(
            &safe_operational_upload_entry(Some(CANONICAL_SESSION_ID)),
        )
        .unwrap();
        client["src"] = serde_json::json!("grok-pager");
        let mut client_line = serde_json::to_vec(&client).unwrap();
        client_line.push(b'\n');
        assert!(filter_upload_lines(&client_line, None).is_none());

        let mut untyped = client;
        untyped["src"] = serde_json::json!("shell");
        untyped
            .as_object_mut()
            .unwrap()
            .remove("credential_safety_schema");
        let mut untyped_line = serde_json::to_vec(&untyped).unwrap();
        untyped_line.push(b'\n');
        assert!(filter_upload_lines(&untyped_line, None).is_none());
    }

    #[test]
    fn upload_filter_omits_noncanonical_secret_session_and_version_envelope_values() {
        const SECRET: &str = "R9t7Y5u3I1o9P7a5S3d1F9g7H5j3K1l9";
        let mut value =
            serde_json::from_slice::<serde_json::Value>(&safe_upload_recovery_entry(Some(SECRET)))
                .unwrap();
        value["ver"] = serde_json::json!(SECRET);
        let mut input = serde_json::to_vec(&value).unwrap();
        input.push(b'\n');

        let output = filter_upload_lines(&input, Some(SECRET))
            .expect("typed event survives unsafe optional envelope values");
        let rendered = String::from_utf8(output).unwrap();
        let projected: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert!(projected["sid"].is_null());
        assert!(projected["ver"].is_null());
        assert_no_secret_fragments(&rendered, SECRET);
    }

    #[test]
    fn upload_filter_rejects_wildcard_credential_fragment_fields() {
        for field in ["access_credential_prefix", "refresh_credential_suffix"] {
            let mut context = serde_json::Map::new();
            context.insert(field.to_owned(), serde_json::json!("fragment"));
            let input = self_attested_entry(None, "unsafe", serde_json::Value::Object(context));
            assert!(
                filter_upload_lines(&input, None).is_none(),
                "accepted {field}"
            );
        }
    }

    #[test]
    fn upload_filter_rejects_opaque_free_form_credentials_fail_closed() {
        let secret = "Q7w5E3r1T9y7U5i3O1p9A7s5";
        for input in [
            self_attested_entry(Some("session-a"), secret, serde_json::json!({})),
            self_attested_entry(
                Some("session-a"),
                CREDENTIAL_ATTRIBUTION_MESSAGE,
                serde_json::json!({"detail": secret}),
            ),
        ] {
            let output = filter_upload_lines(&input, Some("session-a"));
            assert!(output.is_none(), "free-form self-attested record survived");
            let rendered = output
                .map(String::from_utf8)
                .transpose()
                .unwrap()
                .unwrap_or_default();
            assert!(!rendered.contains(secret));
            for window in secret.as_bytes().windows(8) {
                assert!(!rendered.contains(std::str::from_utf8(window).unwrap()));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn upload_snapshot_preserves_file_inode_and_filters_concurrent_legacy_writes() {
        use std::os::unix::fs::MetadataExt;
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unified.jsonl");
        fs::write(&path, safe_upload_entry(Some("session"))).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        let barrier = Arc::new(Barrier::new(2));
        let writer_barrier = Arc::clone(&barrier);
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let mut file = OpenOptions::new().append(true).open(writer_path).unwrap();
            writer_barrier.wait();
            for index in 0..100 {
                if index % 2 == 0 {
                    file.write_all(b"{malformed legacy line}\n").unwrap();
                } else {
                    file.write_all(&safe_upload_entry(Some("session"))).unwrap();
                }
                file.flush().unwrap();
            }
        });

        barrier.wait();
        for _ in 0..20 {
            let snapshot = snapshot_path_for_upload(&path, Some("session")).unwrap();
            let text = String::from_utf8(snapshot).unwrap();
            assert!(!text.contains("malformed legacy line"));
            assert!(text.lines().all(|line| {
                serde_json::from_str::<serde_json::Value>(line).is_ok_and(|value| {
                    value["credential_safety_schema"]
                        == serde_json::json!(CURRENT_CREDENTIAL_SAFETY_SCHEMA)
                })
            }));
        }
        writer.join().unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("malformed legacy line")
        );
    }

    /// The reason this incident was undiagnosable: `trim_file` used to
    /// temp+rename, which swaps the inode out from under every other process
    /// holding an `O_APPEND` descriptor. Their writes then land in an
    /// unlinked inode that no reader can ever see.
    #[cfg(unix)]
    #[test]
    fn trim_file_preserves_the_inode_so_open_handles_survive() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("line {i}\n"));
        }
        fs::write(&path, &content).unwrap();
        let before = fs::metadata(&path).unwrap().ino();

        trim_file(&path);

        assert_eq!(
            fs::metadata(&path).unwrap().ino(),
            before,
            "trim must rewrite in place; replacing the inode strands every \
             sibling process's open log handle",
        );
    }

    /// End-to-end version of the same property: a writer that opened the file
    /// *before* a trim must still be able to append to the file a reader sees
    /// afterwards.
    #[cfg(unix)]
    #[test]
    fn writes_from_a_handle_opened_before_trim_remain_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("line {i}\n"));
        }
        fs::write(&path, &content).unwrap();

        // A sibling process's writer, opened before the trim happens.
        let mut sibling = OpenOptions::new().append(true).open(&path).unwrap();

        trim_file(&path);

        sibling.write_all(b"after trim\n").unwrap();
        sibling.flush().unwrap();

        let visible = fs::read_to_string(&path).unwrap();
        assert!(
            visible.contains("after trim"),
            "a handle opened before the trim must keep writing to the live \
             file, got: {visible:?}",
        );
    }

    /// `maintain` heals a writer whose file was replaced or deleted behind its
    /// back — an older binary still doing temp+rename, an external `rm`, or a
    /// `$TMPDIR` reaper.
    #[cfg(unix)]
    #[test]
    fn maintain_reopens_after_the_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        fs::write(&path, b"original\n").unwrap();

        let mut writer = LogWriter {
            file: OpenOptions::new().append(true).open(&path).unwrap(),
            identity: path_identity(&path),
            path: path.clone(),
            // Force the maintenance cadence to fire on the next call.
            last_maintenance: Instant::now() - MAINTENANCE_INTERVAL,
            detached: false,
        };
        let original_identity = writer.identity;

        // Simulate an older binary's rename-based trim from another process.
        let replacement = dir.path().join("replacement.jsonl");
        fs::write(&replacement, b"replaced\n").unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert_ne!(
            path_identity(&path),
            original_identity,
            "test setup: the path must now resolve to a new inode",
        );

        assert!(
            writer.maintain(),
            "a writer that successfully re-pointed at the live file is writable",
        );
        writer.file.write_all(b"after replacement\n").unwrap();
        writer.file.flush().unwrap();

        let visible = fs::read_to_string(&path).unwrap();
        assert!(
            visible.contains("after replacement"),
            "a writer whose file was replaced must re-point at the live file \
             instead of writing into the orphaned inode, got: {visible:?}",
        );
        assert_eq!(
            writer.identity,
            path_identity(&path),
            "the healed writer must track the new inode",
        );
    }

    /// The same healing path for outright deletion, which is how a
    /// `$TMPDIR` reaper (or a stray `rm`) silences a long-lived agent.
    #[cfg(unix)]
    #[test]
    fn maintain_reopens_after_the_file_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        fs::write(&path, b"original\n").unwrap();

        let mut writer = LogWriter {
            file: OpenOptions::new().append(true).open(&path).unwrap(),
            identity: path_identity(&path),
            path: path.clone(),
            last_maintenance: Instant::now() - MAINTENANCE_INTERVAL,
            detached: false,
        };

        fs::remove_file(&path).unwrap();

        assert!(
            writer.maintain(),
            "a writer that successfully re-pointed at the live file is writable",
        );
        writer.file.write_all(b"after deletion\n").unwrap();
        writer.file.flush().unwrap();

        let visible = fs::read_to_string(&path).expect("log must be recreated");
        assert!(
            visible.contains("after deletion"),
            "a deleted log must be recreated rather than written into the \
             void, got: {visible:?}",
        );
    }

    /// The trim decision must read the real file, not a per-process counter:
    /// with several writers sharing one log, each one's own byte count stays
    /// far below the cap while the file sails past it.
    #[cfg(unix)]
    #[test]
    fn maintain_trims_growth_this_process_did_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        let mut writer = LogWriter {
            file: OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap(),
            identity: path_identity(&path),
            path: path.clone(),
            last_maintenance: Instant::now() - MAINTENANCE_INTERVAL,
            detached: false,
        };

        // Someone else fills the log past the cap; this writer wrote nothing.
        let line = "x".repeat(1023);
        let mut bulk = String::new();
        while bulk.len() as u64 <= MAX_SIZE {
            bulk.push_str(&line);
            bulk.push('\n');
        }
        fs::write(&path, &bulk).unwrap();
        // Rewriting the path in place keeps the inode, so the handle is fine.
        assert_eq!(path_identity(&path), writer.identity);
        assert!(file_size(&path) >= MAX_SIZE);

        assert!(
            writer.maintain(),
            "trimming does not detach the writer; its handle stays usable",
        );

        assert!(
            file_size(&path) < MAX_SIZE,
            "a writer must trim on observed file size, not on its own \
             write counter; size is now {}",
            file_size(&path),
        );
    }

    /// Trimming in place is only safe for one process at a time, and deciding
    /// on the real file size means every writer reaches [`trim_file`] in the
    /// same maintenance window once the log crosses the cap. A trimmer that
    /// finds the log already being rewritten must leave it alone rather than
    /// interleave a second rewrite at offset 0.
    #[test]
    fn trim_file_yields_to_a_concurrent_trimmer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("line {i}\n"));
        }
        fs::write(&path, &content).unwrap();

        // Stand in for another process midway through its own trim.
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        holder.lock().expect("test setup: exclusive lock");

        trim_file(&path);

        // Release before reading: the lock is mandatory on Windows.
        drop(holder);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            content,
            "a contended trim must be skipped, not interleaved with the \
             rewrite already in progress",
        );

        // And it is only deferred, not lost: the next trim proceeds.
        trim_file(&path);
        assert!(fs::read_to_string(&path).unwrap().len() < content.len());
    }

    /// The reopen can itself fail — a log directory replaced by a file, a full
    /// disk, exhausted descriptors. Appending to the old handle anyway would
    /// reproduce the orphaning this module was changed to end, so the writer
    /// drops entries until it can reach the real file again.
    #[cfg(unix)]
    #[test]
    fn maintain_stops_writing_when_the_file_cannot_be_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        fs::create_dir_all(&log_dir).unwrap();
        let path = log_dir.join("test.jsonl");
        fs::write(&path, b"original\n").unwrap();

        let mut writer = LogWriter {
            file: OpenOptions::new().append(true).open(&path).unwrap(),
            identity: path_identity(&path),
            path: path.clone(),
            last_maintenance: Instant::now() - MAINTENANCE_INTERVAL,
            detached: false,
        };

        // Wipe the log's directory and put a regular file in its place, so
        // the path no longer resolves to our inode *and* cannot be reopened.
        fs::remove_dir_all(&log_dir).unwrap();
        fs::write(&log_dir, b"not a directory\n").unwrap();

        assert!(
            !writer.maintain(),
            "a writer that cannot reach the real log must report itself \
             unwritable instead of appending into the orphaned inode",
        );
        assert!(
            !writer.maintain(),
            "and must stay unwritable between maintenance ticks, not just on \
             the tick that discovered the problem",
        );

        // Healing: once the directory is back, the next tick reopens.
        fs::remove_file(&log_dir).unwrap();
        writer.last_maintenance = Instant::now() - MAINTENANCE_INTERVAL;
        assert!(
            writer.maintain(),
            "the writer must recover as soon as the path is usable again",
        );

        writer.file.write_all(b"after recovery\n").unwrap();
        writer.file.flush().unwrap();
        let visible = fs::read_to_string(&path).unwrap();
        assert!(
            visible.contains("after recovery"),
            "the recovered writer must be attached to the visible file, \
             got: {visible:?}",
        );
    }

    #[test]
    fn trim_file_keeps_recent_half() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut content = String::new();
        for i in 0..10 {
            content.push_str(&format!("line {i}\n"));
        }
        fs::write(&path, &content).unwrap();
        trim_file(&path);
        let result = fs::read_to_string(&path).unwrap();
        // Should keep roughly the second half, starting at a line boundary.
        assert!(!result.contains("line 0"));
        assert!(result.contains("line 9"));
        assert!(result.len() < content.len());
        // Every line should be complete (no partial lines).
        for line in result.lines() {
            assert!(line.starts_with("line "));
        }
    }

    #[test]
    fn trim_file_no_newline_in_second_half_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let content = "single-line-no-newline";
        fs::write(&path, content).unwrap();
        trim_file(&path);
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn trim_file_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.jsonl");
        trim_file(&path);
        assert!(!path.exists());
    }

    #[test]
    fn ingest_rejects_shell_src() {
        ingest_client_entries(
            LogSource::Shell,
            &[ClientLogEntry {
                credential_safety_schema: Some(CURRENT_CREDENTIAL_SAFETY_SCHEMA),
                ts: "2025-01-01T00:00:00.000Z".into(),
                pid: None,
                ver: None,
                lvl: LogLevel::Info,
                sid: None,
                msg: "sneaky".into(),
                ctx: None,
            }],
        );
    }

    #[test]
    fn unknown_src_rejected_at_deserialization() {
        for bad in &[
            r#"{"src":"evil","entries":[]}"#,
            r#"{"src":"","entries":[]}"#,
            r#"{"src":"GROK-PAGER","entries":[]}"#,
        ] {
            assert!(serde_json::from_str::<LogNotificationParams>(bad).is_err());
        }
    }

    #[test]
    fn notification_params_round_trip() {
        let params = LogNotificationParams {
            src: LogSource::GrokPager,
            entries: vec![
                ClientLogEntry {
                    credential_safety_schema: Some(CURRENT_CREDENTIAL_SAFETY_SCHEMA),
                    ts: "2025-07-14T10:30:00.123Z".into(),
                    pid: Some(1234),
                    ver: None,
                    lvl: LogLevel::Info,
                    sid: Some("s1".into()),
                    msg: "first".into(),
                    ctx: None,
                },
                ClientLogEntry {
                    credential_safety_schema: Some(CURRENT_CREDENTIAL_SAFETY_SCHEMA),
                    ts: "2025-07-14T10:30:00.456Z".into(),
                    pid: Some(1234),
                    ver: Some("0.1.211".into()),
                    lvl: LogLevel::Error,
                    sid: None,
                    msg: "second".into(),
                    ctx: Some(serde_json::json!({"code": 42})),
                },
            ],
        };
        let json = serde_json::to_string(&params).unwrap();
        let parsed: LogNotificationParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].msg, "first");
        assert_eq!(parsed.entries[1].msg, "second");
    }
}
