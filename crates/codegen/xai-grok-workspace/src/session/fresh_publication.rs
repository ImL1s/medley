//! Atomic no-replace publication of a private fresh-session stage.
//!
//! After an ambiguous rename error the implementation reconciles through the
//! retained-handle identity before classifying the outcome as not committed.
//! Errors never carry user or session content.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::anchored::{AnchoredDirectory, AnchoredRenameError};
use super::publication_parent::ensure_publication_parent;

/// Marker that keeps a private stage invisible to discovery until finalize.
pub const UNPUBLISHED_SESSION_MARKER: &str = ".unpublished";

/// Required durable marker of a resumable public session.
pub const SUMMARY_FILE: &str = "summary.json";

/// Structured finalization classification. Display text is stage/operation
/// only — never a path, session id, or user content.
#[derive(Debug)]
pub enum FreshPublicationFinalizeError {
    /// The no-replace namespace rename did not commit.
    NotCommitted {
        stage: FinalizeStage,
        operation: FinalizeOperation,
        error: io::Error,
    },
    /// The anchored rename committed even though a later durability
    /// acknowledgement failed; the caller must preserve the published state.
    CommittedDurability {
        operation: FinalizeOperation,
        error: io::Error,
    },
    /// The rename committed but the public identity could not be verified.
    CommittedIdentity {
        operation: FinalizeOperation,
        error: io::Error,
    },
}

/// Publication stage reported on a finalization error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeStage {
    PreCommit,
    Commit,
    PostCommit,
}

/// Low-cardinality operation reported on a finalization error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeOperation {
    ValidateSummary,
    OccupancyCheck,
    ConsumeStage,
    RemoveMarker,
    OpenRoot,
    OpenSessions,
    OpenPublishedParent,
    NoReplaceRename,
    ReconcileIdentity,
    Sync,
}

impl FreshPublicationFinalizeError {
    pub fn is_committed(&self) -> bool {
        !matches!(self, Self::NotCommitted { .. })
    }

    fn not_committed(stage: FinalizeStage, operation: FinalizeOperation, error: io::Error) -> Self {
        Self::NotCommitted {
            stage,
            operation,
            error,
        }
    }
}

impl std::fmt::Display for FreshPublicationFinalizeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCommitted {
                stage,
                operation,
                error,
            } => write!(
                formatter,
                "publication not committed (stage={stage:?}, op={operation:?}): {error}"
            ),
            Self::CommittedDurability { operation, error } => write!(
                formatter,
                "publication committed with durability failure (op={operation:?}): {error}"
            ),
            Self::CommittedIdentity { operation, error } => write!(
                formatter,
                "publication committed but canonical identity is unverified (op={operation:?}): {error}"
            ),
        }
    }
}

impl std::error::Error for FreshPublicationFinalizeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotCommitted { error, .. }
            | Self::CommittedDurability { error, .. }
            | Self::CommittedIdentity { error, .. } => Some(error),
        }
    }
}

/// Shared stage owner. `Drop` runs exactly once when the last
/// [`FreshPublication`] clone is gone, so concurrent uncommitted drops cannot
/// all observe `strong_count > 1` and skip cleanup.
struct FreshPublicationShared {
    stage_container_anchor: Mutex<Option<AnchoredDirectory>>,
    stage_session_anchor: Mutex<Option<AnchoredDirectory>>,
    committed: AtomicBool,
}

/// Physical publication plan for one fresh session.
#[derive(Clone)]
pub struct FreshPublication {
    root_dir: PathBuf,
    stage_container: PathBuf,
    stage_session: PathBuf,
    published_parent: PathBuf,
    published_session: PathBuf,
    shared: Arc<FreshPublicationShared>,
    published_parent_name: OsString,
    session_name: OsString,
    publish_attempts: Arc<AtomicUsize>,
}

impl FreshPublication {
    /// Create a private owner-only stage bound to `root/.private/session-staging`.
    pub fn prepare(
        root_dir: &Path,
        session_id: &str,
        published_parent_name: &OsStr,
    ) -> io::Result<Self> {
        if session_id.is_empty() || session_id.contains('/') || session_id.contains('\\') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session id must be a single path component",
            ));
        }
        let (staging_root, staging_anchor) = ensure_private_staging_hierarchy(root_dir)?;
        let session_name = OsString::from(session_id);
        let container_name = OsString::from(format!("session-{:x}", {
            use sha2::{Digest, Sha256};
            Sha256::digest(session_id.as_bytes())
        }));
        if let Ok(existing) = staging_anchor.open_child_dir(&container_name) {
            let _ = existing.remove_tree_self();
        }
        let stage_container_anchor = staging_anchor.create_child_dir(&container_name)?;
        stage_container_anchor.ensure_owner_only()?;
        let stage_session_anchor = stage_container_anchor.create_child_dir(&session_name)?;
        stage_session_anchor.ensure_owner_only()?;
        let marker =
            stage_session_anchor.create_child_file_new(OsStr::new(UNPUBLISHED_SESSION_MARKER))?;
        marker.sync_all()?;
        stage_session_anchor.sync()?;

        let stage_container = staging_root.join(&container_name);
        let stage_session = stage_container.join(&session_name);
        let published_parent = root_dir.join("sessions").join(published_parent_name);
        Ok(Self {
            root_dir: root_dir.to_path_buf(),
            stage_container,
            stage_session: stage_session.clone(),
            published_parent: published_parent.clone(),
            published_session: published_parent.join(&session_name),
            shared: Arc::new(FreshPublicationShared {
                stage_container_anchor: Mutex::new(Some(stage_container_anchor)),
                stage_session_anchor: Mutex::new(Some(stage_session_anchor)),
                committed: AtomicBool::new(false),
            }),
            published_parent_name: published_parent_name.to_owned(),
            session_name,
            publish_attempts: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn stage_session(&self) -> &Path {
        &self.stage_session
    }

    pub fn published_session(&self) -> &Path {
        &self.published_session
    }

    pub fn is_committed(&self) -> bool {
        self.shared.committed.load(Ordering::Acquire)
    }

    /// Number of physical no-replace publication attempts performed.
    pub fn publish_attempts(&self) -> usize {
        self.publish_attempts.load(Ordering::Acquire)
    }

    pub fn finalize(&self) -> Result<(), FreshPublicationFinalizeError> {
        self.finalize_with_rename(default_no_replace_rename)
    }

    /// Finalize with an injectable no-replace rename (tests inject collision /
    /// ambiguous EEXIST). Production uses [`Self::finalize`].
    pub fn finalize_with_rename(
        &self,
        rename: impl FnOnce(
            AnchoredDirectory,
            &AnchoredDirectory,
            &OsStr,
        ) -> Result<AnchoredDirectory, AnchoredRenameError>,
    ) -> Result<(), FreshPublicationFinalizeError> {
        self.finalize_with(rename, |sessions_root, published_parent| {
            sync_directory(sessions_root)?;
            sync_directory(published_parent)
        })
    }

    fn finalize_with(
        &self,
        rename: impl FnOnce(
            AnchoredDirectory,
            &AnchoredDirectory,
            &OsStr,
        ) -> Result<AnchoredDirectory, AnchoredRenameError>,
        sync_published: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<(), FreshPublicationFinalizeError> {
        validate_staged_summary(&self.stage_session).map_err(|error| {
            FreshPublicationFinalizeError::not_committed(
                FinalizeStage::PreCommit,
                FinalizeOperation::ValidateSummary,
                error,
            )
        })?;
        if published_path_occupied(&self.published_session) {
            return Err(FreshPublicationFinalizeError::not_committed(
                FinalizeStage::PreCommit,
                FinalizeOperation::OccupancyCheck,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "no-replace publication collided",
                ),
            ));
        }

        let stage_session_anchor = self
            .shared
            .stage_session_anchor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .ok_or_else(|| {
                FreshPublicationFinalizeError::not_committed(
                    FinalizeStage::PreCommit,
                    FinalizeOperation::ConsumeStage,
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "fresh publication stage was already consumed",
                    ),
                )
            })?;
        if let Err(error) = stage_session_anchor
            .remove_marker(OsStr::new(UNPUBLISHED_SESSION_MARKER))
            .and_then(|()| stage_session_anchor.sync())
        {
            restore_stage(&self.shared.stage_session_anchor, stage_session_anchor);
            return Err(FreshPublicationFinalizeError::not_committed(
                FinalizeStage::PreCommit,
                FinalizeOperation::RemoveMarker,
                error,
            ));
        }

        if let Err(error) = AnchoredDirectory::open_root(&self.root_dir) {
            restore_stage(&self.shared.stage_session_anchor, stage_session_anchor);
            return Err(FreshPublicationFinalizeError::not_committed(
                FinalizeStage::PreCommit,
                FinalizeOperation::OpenRoot,
                error,
            ));
        }
        let publication_parent =
            match ensure_publication_parent(&self.root_dir, &self.published_parent_name) {
                Ok(parent) => parent,
                Err(error) => {
                    restore_stage(&self.shared.stage_session_anchor, stage_session_anchor);
                    return Err(FreshPublicationFinalizeError::not_committed(
                        FinalizeStage::PreCommit,
                        FinalizeOperation::OpenPublishedParent,
                        error,
                    ));
                }
            };
        if let Err(error) = publication_parent.revalidate() {
            restore_stage(&self.shared.stage_session_anchor, stage_session_anchor);
            return Err(FreshPublicationFinalizeError::not_committed(
                FinalizeStage::PreCommit,
                FinalizeOperation::OpenPublishedParent,
                error,
            ));
        }
        let sessions_anchor = publication_parent.sessions_anchor();
        let published_parent_anchor = publication_parent.parent_anchor();

        self.publish_attempts.fetch_add(1, Ordering::SeqCst);
        let published_session_anchor = match rename(
            stage_session_anchor,
            &published_parent_anchor,
            &self.session_name,
        ) {
            Ok(child) => child,
            Err(failure) => {
                return self.reconcile_or_abort(failure, &published_parent_anchor);
            }
        };

        self.mark_committed();
        self.drop_stage_container();
        self.finish_committed(
            published_session_anchor,
            &published_parent_anchor,
            &sessions_anchor,
            sync_published,
        )
    }

    fn reconcile_or_abort(
        &self,
        failure: AnchoredRenameError,
        published_parent_anchor: &AnchoredDirectory,
    ) -> Result<(), FreshPublicationFinalizeError> {
        match published_parent_anchor.open_child_dir(&self.session_name) {
            Ok(published) => match published.same_identity(&failure.source) {
                Ok(true) => {
                    drop(failure.source);
                    self.mark_committed();
                    self.drop_stage_container();
                    let sessions = AnchoredDirectory::open_root(&self.root_dir)
                        .and_then(|root| root.open_child_dir(OsStr::new("sessions")));
                    match sessions {
                        Ok(sessions_anchor) => self.finish_committed(
                            published,
                            published_parent_anchor,
                            &sessions_anchor,
                            |sessions_root, published_parent| {
                                sync_directory(sessions_root)?;
                                sync_directory(published_parent)
                            },
                        ),
                        Err(error) => Err(FreshPublicationFinalizeError::CommittedIdentity {
                            operation: FinalizeOperation::ReconcileIdentity,
                            error,
                        }),
                    }
                }
                Ok(false) => {
                    restore_stage(&self.shared.stage_session_anchor, failure.source);
                    Err(FreshPublicationFinalizeError::not_committed(
                        FinalizeStage::Commit,
                        FinalizeOperation::NoReplaceRename,
                        failure.error,
                    ))
                }
                Err(error) => {
                    restore_stage(&self.shared.stage_session_anchor, failure.source);
                    Err(FreshPublicationFinalizeError::not_committed(
                        FinalizeStage::Commit,
                        FinalizeOperation::ReconcileIdentity,
                        error,
                    ))
                }
            },
            Err(_) => {
                restore_stage(&self.shared.stage_session_anchor, failure.source);
                Err(FreshPublicationFinalizeError::not_committed(
                    FinalizeStage::Commit,
                    FinalizeOperation::NoReplaceRename,
                    failure.error,
                ))
            }
        }
    }

    fn finish_committed(
        &self,
        published_session_anchor: AnchoredDirectory,
        published_parent_anchor: &AnchoredDirectory,
        sessions_anchor: &AnchoredDirectory,
        sync_published: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<(), FreshPublicationFinalizeError> {
        if !valid_public_summary(&self.published_session) {
            return Err(FreshPublicationFinalizeError::CommittedIdentity {
                operation: FinalizeOperation::ValidateSummary,
                error: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "committed publication is missing a valid summary",
                ),
            });
        }
        sync_published(&self.root_dir.join("sessions"), &self.published_parent)
            .and_then(|()| published_session_anchor.sync())
            .and_then(|()| published_parent_anchor.sync())
            .and_then(|()| sessions_anchor.sync())
            .map_err(|error| FreshPublicationFinalizeError::CommittedDurability {
                operation: FinalizeOperation::Sync,
                error,
            })
    }

    fn mark_committed(&self) {
        self.shared.committed.store(true, Ordering::Release);
    }

    fn drop_stage_container(&self) {
        if let Some(container) = self
            .shared
            .stage_container_anchor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            && let Err(error) = container.remove_tree_self()
        {
            tracing::warn!(
                %error,
                "failed to remove committed private fresh stage container"
            );
        }
    }
}

impl Drop for FreshPublicationShared {
    fn drop(&mut self) {
        if self.committed.load(Ordering::Acquire) {
            return;
        }
        // Last Arc owner: elect cleanup without a racy strong_count snapshot.
        self.stage_session_anchor
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(container) = self
            .stage_container_anchor
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            && let Err(error) = container.remove_tree_self()
        {
            tracing::warn!(
                %error,
                "failed to remove cancelled private fresh-session stage"
            );
        }
    }
}

fn default_no_replace_rename(
    source: AnchoredDirectory,
    target_parent: &AnchoredDirectory,
    target_name: &OsStr,
) -> Result<AnchoredDirectory, AnchoredRenameError> {
    source.try_rename_self_no_replace(target_parent, target_name)
}

fn restore_stage(slot: &Mutex<Option<AnchoredDirectory>>, anchor: AnchoredDirectory) {
    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(anchor);
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

fn ensure_private_staging_hierarchy(root_dir: &Path) -> io::Result<(PathBuf, AnchoredDirectory)> {
    let root = AnchoredDirectory::open_root(root_dir)?;
    let private = open_or_create_anchored_child(&root, OsStr::new(".private"))?;
    private.ensure_owner_only()?;
    let staging = open_or_create_anchored_child(&private, OsStr::new("session-staging"))?;
    staging.ensure_owner_only()?;
    Ok((root_dir.join(".private/session-staging"), staging))
}

fn published_path_occupied(path: &Path) -> bool {
    match path.try_exists() {
        Ok(true) => true,
        Ok(false) => false,
        Err(_) => true,
    }
}

fn validate_staged_summary(stage_session: &Path) -> io::Result<()> {
    let path = stage_session.join(SUMMARY_FILE);
    let bytes = std::fs::read(&path)?;
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged summary.json is empty",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("staged summary is not json: {error}"),
        )
    })?;
    if !value.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged summary is not an object",
        ));
    }
    Ok(())
}

fn valid_public_summary(published_session: &Path) -> bool {
    validate_staged_summary(published_session).is_ok()
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// True when a public session directory exists without a valid `summary.json`.
pub fn marker_free_public_dir_without_summary(path: &Path) -> bool {
    path.is_dir() && !path.join(UNPUBLISHED_SESSION_MARKER).exists() && !valid_public_summary(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000356";
    const PARENT: &str = "encoded-cwd";

    fn write_summary(dir: &Path) {
        std::fs::write(
            dir.join(SUMMARY_FILE),
            br#"{"info":{"id":"019c0000-0000-7000-8000-000000000356"}}"#,
        )
        .unwrap();
    }

    fn prepare() -> (tempfile::TempDir, FreshPublication) {
        let root = tempfile::tempdir().unwrap();
        let publication =
            FreshPublication::prepare(root.path(), SESSION_ID, OsStr::new(PARENT)).unwrap();
        write_summary(publication.stage_session());
        (root, publication)
    }

    #[test]
    fn clone_drop_does_not_cancel_sibling_publication() {
        let (_root, publication) = prepare();
        let clone = publication.clone();
        drop(clone);
        publication
            .finalize()
            .expect("surviving owner still publishes");
        assert!(publication.is_committed());
        assert!(publication.published_session().join(SUMMARY_FILE).is_file());
    }

    #[test]
    fn concurrent_uncommitted_clone_drops_remove_private_stage() {
        let (_root, publication) = prepare();
        let stage = publication.stage_session().to_path_buf();
        let container = stage
            .parent()
            .expect("stage lives in a container")
            .to_path_buf();
        assert!(stage.is_dir());
        let a = publication.clone();
        let b = publication.clone();
        let first = std::thread::spawn(move || drop(a));
        let second = std::thread::spawn(move || drop(b));
        drop(publication);
        first.join().expect("first clone drop");
        second.join().expect("second clone drop");
        assert!(
            !stage.exists() && !container.exists(),
            "last shared owner must remove the private stage"
        );
    }

    #[test]
    fn finalize_publishes_once_and_removes_private_stage() {
        let (root, publication) = prepare();
        publication.finalize().expect("publish");
        assert!(publication.is_committed());
        assert_eq!(publication.publish_attempts(), 1);
        assert!(publication.published_session().join(SUMMARY_FILE).is_file());
        assert!(
            !publication
                .published_session()
                .join(UNPUBLISHED_SESSION_MARKER)
                .exists()
        );
        assert!(!publication.stage_session().exists());
        assert!(!marker_free_public_dir_without_summary(
            publication.published_session()
        ));
        drop(publication);
        assert!(
            root.path()
                .join("sessions")
                .join(PARENT)
                .join(SESSION_ID)
                .join(SUMMARY_FILE)
                .is_file()
        );
    }

    #[test]
    fn injected_no_replace_collision_before_commit_leaves_no_public_partial_tree() {
        let (_root, publication) = prepare();
        let published = publication.published_session().to_path_buf();
        std::fs::create_dir_all(&published).unwrap();
        std::fs::write(published.join("partial"), b"winner").unwrap();

        let result = publication.finalize();
        assert!(matches!(
            result,
            Err(FreshPublicationFinalizeError::NotCommitted {
                stage: FinalizeStage::PreCommit,
                operation: FinalizeOperation::OccupancyCheck,
                ..
            })
        ));
        assert!(!publication.is_committed());
        assert_eq!(publication.publish_attempts(), 0);
        assert!(!published.join(SUMMARY_FILE).exists());
        drop(publication);
        assert_eq!(std::fs::read(published.join("partial")).unwrap(), b"winner");
        assert!(
            !published.join(UNPUBLISHED_SESSION_MARKER).exists(),
            "abort must not write our unpublished marker into the occupant"
        );
    }

    #[test]
    fn injected_ambiguous_post_commit_eexist_classifies_as_committed() {
        let (_root, publication) = prepare();
        let result = publication.finalize_with_rename(|source, target_parent, target_name| {
            let published = source
                .try_rename_self_no_replace(target_parent, target_name)
                .expect("physical rename committed");
            Err(AnchoredRenameError {
                error: io::Error::new(io::ErrorKind::AlreadyExists, "injected post-commit EEXIST"),
                source: published,
            })
        });
        assert!(
            result.is_ok()
                || matches!(
                    result,
                    Err(FreshPublicationFinalizeError::CommittedDurability { .. })
                        | Err(FreshPublicationFinalizeError::CommittedIdentity { .. })
                ),
            "ambiguous post-commit EEXIST must not be NotCommitted: {result:?}"
        );
        assert!(publication.is_committed());
        assert_eq!(publication.publish_attempts(), 1);
        assert!(publication.published_session().join(SUMMARY_FILE).is_file());
    }

    #[test]
    fn returned_error_never_leaves_marker_free_public_dir_without_summary() {
        let (_root, publication) = prepare();
        std::fs::remove_file(publication.stage_session().join(SUMMARY_FILE)).unwrap();
        let result = publication.finalize();
        assert!(matches!(
            result,
            Err(FreshPublicationFinalizeError::NotCommitted {
                operation: FinalizeOperation::ValidateSummary,
                ..
            })
        ));
        assert!(!publication.published_session().exists());
        drop(publication);
    }

    #[test]
    fn missing_summary_error_does_not_create_public_tree() {
        let root = tempfile::tempdir().unwrap();
        let publication =
            FreshPublication::prepare(root.path(), SESSION_ID, OsStr::new(PARENT)).unwrap();
        assert!(publication.finalize().is_err());
        let public = root.path().join("sessions").join(PARENT).join(SESSION_ID);
        assert!(!public.exists() || !marker_free_public_dir_without_summary(&public));
        drop(publication);
        assert!(!public.exists());
    }

    #[test]
    fn publish_is_invoked_exactly_once_even_on_reconciled_eexist() {
        let (_root, publication) = prepare();
        let attempts_before = publication.publish_attempts();
        let _ = publication.finalize_with_rename(|source, target_parent, target_name| match source
            .try_rename_self_no_replace(target_parent, target_name)
        {
            Ok(published) => Err(AnchoredRenameError {
                error: io::Error::from_raw_os_error(17),
                source: published,
            }),
            Err(failure) => Err(failure),
        });
        assert_eq!(publication.publish_attempts(), attempts_before + 1);
    }

    #[cfg(unix)]
    #[test]
    fn finalize_rejects_symlinked_encoded_cwd_parent() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("canary"), b"fresh-canary").unwrap();
        let publication =
            FreshPublication::prepare(root.path(), SESSION_ID, OsStr::new(PARENT)).unwrap();
        write_summary(publication.stage_session());
        std::fs::create_dir_all(root.path().join("sessions")).unwrap();
        symlink(outside.path(), root.path().join("sessions").join(PARENT)).unwrap();

        assert!(publication.finalize().is_err());
        assert!(!publication.is_committed());
        assert_eq!(
            std::fs::read(outside.path().join("canary")).unwrap(),
            b"fresh-canary"
        );
        assert!(!outside.path().join(SESSION_ID).exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_hierarchy_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let (root, publication) = prepare();
        for directory in [
            root.path().join(".private"),
            root.path().join(".private/session-staging"),
            publication.stage_container.clone(),
            publication.stage_session.clone(),
        ] {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
