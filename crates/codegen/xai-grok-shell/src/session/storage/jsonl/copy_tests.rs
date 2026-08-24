use crate::sampling::ConversationItem;
use crate::session::info::Info;
use crate::session::persistence::{CHAT_FORMAT_VERSION, default_model_id};
use crate::session::storage::{
    CopySessionOptions, JsonlStorageAdapter, SessionUpdate, StorageAdapter,
};
use crate::tools::todo::TodoState;
use agent_client_protocol as acp;
use tempfile::TempDir;

#[test]
fn reverse_direction_fork_leases_do_not_deadlock() {
    use std::sync::mpsc;
    use std::time::Duration;

    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();

    let (done_tx, done_rx) = mpsc::channel();
    let mut workers = Vec::new();
    for (source, target) in [("a", "b"), ("b", "a")] {
        let root = root.clone();
        let done_tx = done_tx.clone();
        workers.push(std::thread::spawn(move || {
            let leases =
                crate::session::persistence::acquire_ordered_copy_locks_sync(&root, source, target)
                    .unwrap();
            done_tx.send(()).unwrap();
            drop(leases);
        }));
    }
    drop(done_tx);

    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("first reverse-direction lease acquisition must complete");
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("second reverse-direction lease acquisition must complete");
    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
fn copy_rejects_session_id_path_traversal() {
    assert!(super::validate_session_path_component("../victim").is_err());
    assert!(super::validate_session_path_component("nested/victim").is_err());
    assert!(super::validate_session_path_component("safe-session-id").is_ok());
}

#[tokio::test]
async fn copy_stage_construction_failure_reclaims_deterministic_container() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-stage-construction-failure"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-stage-construction-failure"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();

    super::FAIL_COPY_STAGE_AFTER_CONTAINER_CREATE.with(|fail| fail.set(true));
    let error = super::CopyPublication::begin(
        temp_dir.path(),
        &source,
        &target,
        adapter.session_dir(&target),
    )
    .err()
    .expect("injected construction failure must abort begin");
    assert_eq!(error.kind(), std::io::ErrorKind::Other);

    let container_name =
        crate::session::persistence::session_stage_container_name(target.id.to_string().as_str());
    assert!(
        !temp_dir
            .path()
            .join(".private/session-staging")
            .join(container_name)
            .exists(),
        "failed begin must not leave its deterministic private stage container"
    );
    assert!(!adapter.session_dir(&target).parent().unwrap().exists());
}

#[tokio::test]
async fn no_replace_copy_publication_abort_leaves_no_public_partial() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-publication-guard"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-publication-guard"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();

    let target_dir = adapter.session_dir(&target);
    let target_parent = target_dir.parent().unwrap().to_path_buf();
    {
        let publication =
            super::CopyPublication::begin(temp_dir.path(), &source, &target, target_dir.clone())
                .unwrap();
        let stage_dir = publication.target_dir().to_path_buf();
        std::fs::copy(
            adapter.summary_file(&source),
            stage_dir.join("summary.json"),
        )
        .unwrap();

        let visible = adapter.list_sessions(None).await.unwrap();
        assert_eq!(visible.len(), 1, "provisional target must not be listed");
        assert_eq!(visible[0].info.id, source.id);
        assert!(
            stage_dir
                .join(crate::session::persistence::UNPUBLISHED_SESSION_MARKER)
                .is_file()
        );
        assert!(
            stage_dir.starts_with(temp_dir.path().join(".private/session-staging")),
            "all provisional writes must remain in owner-only private staging"
        );
        assert!(
            !target_dir.exists(),
            "public target must not exist mid-copy"
        );
        assert!(
            !target_parent.exists(),
            "copy must not expose even the encoded cwd parent before publication"
        );
    }
    assert!(
        !target_dir.exists(),
        "aborted copy must never create a public directory"
    );
    assert!(!target_parent.exists());
    assert_eq!(
        std::fs::read_dir(temp_dir.path().join(".private/session-staging"))
            .unwrap()
            .count(),
        0,
        "aborted private stages are reclaimed through retained handles"
    );
}

#[tokio::test]
async fn no_replace_copy_publication_success_moves_private_stage() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-publication-success"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-publication-success"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let target_dir = adapter.session_dir(&target);
    let mut publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, target_dir.clone())
            .unwrap();
    std::fs::copy(
        adapter.summary_file(&source),
        publication.target_dir().join("summary.json"),
    )
    .unwrap();

    publication.publish_with(|| Ok(()), || {}).unwrap();
    assert!(target_dir.join("summary.json").is_file());
    assert_eq!(
        std::fs::read_dir(temp_dir.path().join(".private/session-staging"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn long_cwd_parent_and_metadata_remain_private_until_publication() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-long-private-parent"),
        cwd: "/src".to_string(),
    };
    let long_cwd = format!("/{}", "private-long-cwd-component/".repeat(24));
    let target = Info {
        id: acp::SessionId::new("target-long-private-parent"),
        cwd: long_cwd.clone(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();

    let target_dir = adapter.session_dir(&target);
    let target_parent = target_dir.parent().unwrap().to_path_buf();
    let mut publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, target_dir.clone())
            .unwrap();
    std::fs::copy(
        adapter.summary_file(&source),
        publication.target_dir().join("summary.json"),
    )
    .unwrap();

    assert!(!target_parent.exists());
    assert!(!target_parent.join(".cwd").exists());
    publication.publish_with(|| Ok(()), || {}).unwrap();
    assert_eq!(
        std::fs::read(target_parent.join(".cwd")).unwrap(),
        long_cwd.as_bytes()
    );
    assert!(target_dir.join("summary.json").is_file());
}

#[tokio::test]
async fn concurrent_matching_parent_creation_publishes_session_only() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-parent-race"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-parent-race"),
        cwd: "/dst-parent-race".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let target_dir = adapter.session_dir(&target);
    let target_parent = target_dir.parent().unwrap().to_path_buf();
    let mut publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, target_dir.clone())
            .unwrap();
    std::fs::copy(
        adapter.summary_file(&source),
        publication.target_dir().join("summary.json"),
    )
    .unwrap();

    std::fs::create_dir(&target_parent).unwrap();
    publication.publish_with(|| Ok(()), || {}).unwrap();
    assert!(target_dir.join("summary.json").is_file());
    assert_eq!(
        std::fs::read_dir(temp_dir.path().join(".private/session-staging"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn concurrent_mismatched_long_cwd_parent_fails_closed_and_reclaims_stage() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-mismatched-parent-race"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-mismatched-parent-race"),
        cwd: format!("/{}", "mismatched-parent-race/".repeat(24)),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let target_dir = adapter.session_dir(&target);
    let target_parent = target_dir.parent().unwrap().to_path_buf();
    let mut publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, target_dir.clone())
            .unwrap();
    std::fs::copy(
        adapter.summary_file(&source),
        publication.target_dir().join("summary.json"),
    )
    .unwrap();

    std::fs::create_dir(&target_parent).unwrap();
    std::fs::write(target_parent.join(".cwd"), b"/concurrent/winner").unwrap();
    let error = publication
        .publish_with(|| Ok(()), || {})
        .expect_err("a raced parent with different cwd metadata must fail closed");
    assert!(matches!(
        error,
        super::CopyPublicationFinalizeError::NotCommitted(ref inner)
            if inner.kind() == std::io::ErrorKind::InvalidData
    ));
    drop(publication);
    assert_eq!(
        std::fs::read(target_parent.join(".cwd")).unwrap(),
        b"/concurrent/winner"
    );
    assert!(!target_dir.exists());
    assert_eq!(
        std::fs::read_dir(temp_dir.path().join(".private/session-staging"))
            .unwrap()
            .count(),
        0,
        "losing private parent stage must be fully reclaimed"
    );
}

#[tokio::test]
async fn no_replace_copy_publication_empty_abort_removes_private_stage() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-empty-abort"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-empty-abort"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let target_dir = adapter.session_dir(&target);
    let publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, target_dir.clone())
            .unwrap();
    drop(publication);

    assert!(!target_dir.exists());
    assert_eq!(
        std::fs::read_dir(temp_dir.path().join(".private/session-staging"))
            .unwrap()
            .count(),
        0,
        "empty abort must remove child before its private container through retained handles"
    );
}

#[tokio::test]
async fn copy_failure_after_target_write_leaves_no_public_partial() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-mid-copy-failure"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-mid-copy-failure"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let updates = adapter.updates_file(&source);
    std::fs::create_dir(&updates).unwrap();

    let error = adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .expect_err("opening a directory as updates.jsonl must fail after target creation");
    assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(
        !adapter.session_dir(&target).exists(),
        "failed copy must leave no public target, even after target files were created"
    );
    assert_eq!(
        std::fs::read_dir(temp_dir.path().join(".private/session-staging"))
            .unwrap()
            .count(),
        0,
        "failed non-empty stage must be fully reclaimed through retained handles"
    );
}

#[tokio::test]
async fn published_copy_post_commit_sync_failure_preserves_visible_target() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-post-commit-sync"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-post-commit-sync"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();

    let target_dir = adapter.session_dir(&target);
    let mut publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, target_dir.clone())
            .unwrap();
    let stage_dir = publication.target_dir().to_path_buf();
    std::fs::copy(
        adapter.summary_file(&source),
        stage_dir.join("summary.json"),
    )
    .unwrap();

    let error = publication
        .publish_with(
            || Err(std::io::Error::other("injected post-commit sync failure")),
            || {},
        )
        .expect_err("post-commit sync must report failure");
    assert!(matches!(
        error,
        super::CopyPublicationFinalizeError::CommittedDurability(_)
    ));
    drop(publication);

    assert!(target_dir.is_dir(), "drop must preserve committed fork");
    assert!(target_dir.join("summary.json").is_file());
    assert!(
        !target_dir
            .join(crate::session::persistence::UNPUBLISHED_SESSION_MARKER)
            .exists()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn copy_refuses_symlinked_cwd_parent_without_touching_outside_tree() {
    use std::os::unix::fs::symlink;

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-symlink-parent-guard"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-symlink-parent-guard"),
        cwd: "/dst-symlink-parent".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();

    let outside = temp_dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), b"untouched").unwrap();
    let target_parent = adapter.session_dir(&target).parent().unwrap().to_path_buf();
    symlink(&outside, &target_parent).unwrap();

    let error = adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .expect_err("fork must reject a symlinked cwd parent");
    assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(
        std::fs::read(outside.join("sentinel")).unwrap(),
        b"untouched"
    );
    assert!(
        !outside.join(target.id.to_string()).exists(),
        "fork must not create or remove anything through the symlink"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn parent_swap_cannot_redirect_publication_outside_sessions_tree() {
    use std::os::unix::fs::symlink;

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-parent-swap-guard"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-parent-swap-guard"),
        cwd: "/dst-parent-swap".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();

    let public_target = adapter.session_dir(&target);
    let target_parent = public_target.parent().unwrap().to_path_buf();
    std::fs::create_dir(&target_parent).unwrap();
    let mut publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, public_target.clone())
            .unwrap();
    std::fs::copy(
        adapter.summary_file(&source),
        publication.target_dir().join("summary.json"),
    )
    .unwrap();

    let detached_parent = target_parent.with_extension("detached");
    std::fs::rename(&target_parent, &detached_parent).unwrap();
    let outside = temp_dir.path().join("outside-parent-swap");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), b"untouched").unwrap();
    symlink(&outside, &target_parent).unwrap();

    let error = publication
        .publish_with(|| Ok(()), || {})
        .expect_err("pre-commit parent identity check must reject the swapped parent");
    assert!(matches!(
        error,
        super::CopyPublicationFinalizeError::NotCommitted(_)
    ));
    assert_eq!(
        std::fs::read(outside.join("sentinel")).unwrap(),
        b"untouched"
    );
    assert!(
        !outside.join(target.id.to_string()).exists(),
        "retained target-parent handle must prevent redirecting publication through a replacement symlink"
    );
    assert!(
        !detached_parent.join(target.id.to_string()).exists(),
        "pre-commit identity failure must not publish into the detached original parent"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sessions_root_swap_before_commit_is_not_committed() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-sessions-root-swap"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-sessions-root-swap"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let public_target = adapter.session_dir(&target);
    let mut publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, public_target.clone())
            .unwrap();
    std::fs::copy(
        adapter.summary_file(&source),
        publication.target_dir().join("summary.json"),
    )
    .unwrap();

    let sessions = temp_dir.path().join("sessions");
    let detached = temp_dir.path().join("sessions-detached");
    std::fs::rename(&sessions, &detached).unwrap();
    std::fs::create_dir(&sessions).unwrap();

    let error = publication
        .publish_with(|| Ok(()), || {})
        .expect_err("canonical sessions-root replacement must block commit");
    assert!(matches!(
        error,
        super::CopyPublicationFinalizeError::NotCommitted(_)
    ));
    assert!(
        !detached
            .join(public_target.parent().unwrap().file_name().unwrap())
            .join(target.id.to_string())
            .exists(),
        "pre-commit sessions-root swap must not publish into the detached root"
    );
    assert!(!public_target.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn sessions_root_swap_after_rename_is_committed_unreachable_hard_failure() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-post-rename-sessions-swap"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-post-rename-sessions-swap"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let public_target = adapter.session_dir(&target);
    let sessions = temp_dir.path().join("sessions");
    let detached = temp_dir.path().join("sessions-detached-after-rename");
    let mut publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, public_target.clone())
            .unwrap();
    std::fs::copy(
        adapter.summary_file(&source),
        publication.target_dir().join("summary.json"),
    )
    .unwrap();

    let error = publication
        .publish_with(
            || Ok(()),
            || {
                std::fs::rename(&sessions, &detached).unwrap();
                std::fs::create_dir(&sessions).unwrap();
            },
        )
        .expect_err("committed fork under a detached sessions root must hard fail");
    assert!(matches!(
        error,
        super::CopyPublicationFinalizeError::CommittedUnreachable(_)
    ));
    assert!(
        detached
            .join(public_target.parent().unwrap().file_name().unwrap())
            .join(target.id.to_string())
            .join("summary.json")
            .is_file()
    );
    assert!(!public_target.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn parent_swap_after_rename_is_committed_unreachable_hard_failure() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-post-rename-parent-swap"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-post-rename-parent-swap"),
        cwd: "/dst-post-rename-swap".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let public_target = adapter.session_dir(&target);
    let target_parent = public_target.parent().unwrap().to_path_buf();
    std::fs::create_dir(&target_parent).unwrap();
    let detached_parent = target_parent.with_extension("detached-after-rename");
    let mut publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, public_target.clone())
            .unwrap();
    std::fs::copy(
        adapter.summary_file(&source),
        publication.target_dir().join("summary.json"),
    )
    .unwrap();

    let error = publication
        .publish_with(
            || Ok(()),
            || {
                std::fs::rename(&target_parent, &detached_parent).unwrap();
                std::fs::create_dir(&target_parent).unwrap();
            },
        )
        .expect_err("committed target detached from canonical path must hard fail");
    assert!(matches!(
        error,
        super::CopyPublicationFinalizeError::CommittedUnreachable(_)
    ));
    assert!(
        detached_parent
            .join(target.id.to_string())
            .join("summary.json")
            .is_file(),
        "commit occurred but is no longer canonically reachable"
    );
    assert!(!public_target.exists());
}

#[tokio::test]
async fn no_replace_publication_preserves_concurrent_winner() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-concurrent-winner"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-concurrent-winner"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();

    let public_target = adapter.session_dir(&target);
    let mut publication =
        super::CopyPublication::begin(temp_dir.path(), &source, &target, public_target.clone())
            .unwrap();
    std::fs::copy(
        adapter.summary_file(&source),
        publication.target_dir().join("summary.json"),
    )
    .unwrap();
    std::fs::create_dir_all(&public_target).unwrap();
    std::fs::write(public_target.join("winner"), b"keep me").unwrap();

    let error = publication
        .publish_with(|| Ok(()), || {})
        .expect_err("anchored publication must never replace a concurrent winner");
    assert!(matches!(
        error,
        super::CopyPublicationFinalizeError::NotCommitted(ref inner)
            if inner.kind() == std::io::ErrorKind::AlreadyExists
    ));
    drop(publication);
    assert_eq!(
        std::fs::read(public_target.join("winner")).unwrap(),
        b"keep me"
    );
    assert!(
        !public_target.join("summary.json").exists(),
        "losing private stage must not merge into the winner"
    );
    assert_eq!(
        std::fs::read_dir(temp_dir.path().join(".private/session-staging"))
            .unwrap()
            .count(),
        0,
        "the losing private stage must be reclaimed through its retained anchor"
    );
}

#[tokio::test]
async fn copy_publication_writes_long_cwd_metadata() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-long-cwd-metadata"),
        cwd: "/src".to_string(),
    };
    let long_cwd = format!("/{}", "long-cwd-component/".repeat(24));
    let target = Info {
        id: acp::SessionId::new("target-long-cwd-metadata"),
        cwd: long_cwd.clone(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();

    adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .unwrap();

    let target_dir = adapter.session_dir(&target);
    let target_parent = target_dir.parent().unwrap();
    assert_eq!(
        std::fs::read(target_parent.join(".cwd")).unwrap(),
        long_cwd.as_bytes()
    );
    assert_eq!(
        xai_grok_config::decode_cwd_from_dirname(target_parent),
        Some(long_cwd)
    );
    assert!(target_dir.join("summary.json").is_file());
}

#[tokio::test]
async fn copy_publication_rejects_mismatched_long_cwd_metadata() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-mismatched-long-cwd"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-mismatched-long-cwd"),
        cwd: format!("/{}", "long-cwd-component/".repeat(24)),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let target_dir = adapter.session_dir(&target);
    let target_parent = target_dir.parent().unwrap();
    std::fs::create_dir_all(target_parent).unwrap();
    std::fs::write(target_parent.join(".cwd"), b"/different/cwd").unwrap();

    let error = adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .expect_err("mismatched hashed cwd metadata must fail closed");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(!target_dir.exists());
    assert_eq!(
        std::fs::read(target_parent.join(".cwd")).unwrap(),
        b"/different/cwd"
    );
}

#[tokio::test]
async fn stale_public_marker_collision_fails_closed_without_reclaim() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-stale-public-collision"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-stale-public-collision"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();

    let public_target = adapter.session_dir(&target);
    std::fs::create_dir_all(&public_target).unwrap();
    std::fs::write(
        public_target.join(crate::session::persistence::UNPUBLISHED_SESSION_MARKER),
        b"",
    )
    .unwrap();
    std::fs::write(public_target.join("winner"), b"do not reclaim").unwrap();

    let error = adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .expect_err("legacy public provisional directories must fail closed");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(public_target.join("winner")).unwrap(),
        b"do not reclaim",
        "fork must never path-delete a stale public collision"
    );
}

#[tokio::test]
async fn cross_cwd_malformed_target_entry_is_a_collision() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-cross-cwd-malformed"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-cross-cwd-malformed"),
        cwd: "/new-cwd".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    let other_cwd = Info {
        id: target.id.clone(),
        cwd: "/other-cwd".to_string(),
    };
    let malformed = adapter.session_dir(&other_cwd);
    std::fs::create_dir_all(malformed.parent().unwrap()).unwrap();
    std::fs::write(&malformed, b"malformed collision").unwrap();

    let error = adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .expect_err("any same-id entry under another cwd must reserve the identity");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&malformed).unwrap(), b"malformed collision");
}

#[test]
fn caller_preserves_result_only_after_committed_durability_error() {
    let expected = crate::session::storage::CopySessionResult {
        chat_messages_copied: 3,
        updates_copied: 4,
        plan_state_copied: true,
        plan_mode_state_copied: false,
        signals_copied: false,
        tool_state_copied: false,
        announcement_state_copied: false,
        compaction_segments_copied: 0,
        compaction_checkpoints_copied: 0,
    };
    let actual = super::reconcile_copy_publication(
        &acp::SessionId::new("committed-result"),
        expected.clone(),
        Err(super::CopyPublicationFinalizeError::CommittedDurability(
            std::io::Error::other("injected committed durability failure"),
        )),
    )
    .expect("committed publication must remain caller-visible success");
    assert_eq!(actual.chat_messages_copied, expected.chat_messages_copied);
    assert_eq!(actual.updates_copied, expected.updates_copied);
}

#[test]
fn caller_hard_fails_committed_unreachable_publication() {
    let result = crate::session::storage::CopySessionResult {
        chat_messages_copied: 1,
        updates_copied: 1,
        plan_state_copied: false,
        plan_mode_state_copied: false,
        signals_copied: false,
        tool_state_copied: false,
        announcement_state_copied: false,
        compaction_segments_copied: 0,
        compaction_checkpoints_copied: 0,
    };
    let error = super::reconcile_copy_publication(
        &acp::SessionId::new("unreachable-result"),
        result,
        Err(super::CopyPublicationFinalizeError::CommittedUnreachable(
            std::io::Error::new(std::io::ErrorKind::InvalidData, "detached commit"),
        )),
    )
    .expect_err("committed but unreachable fork must never be reported as success");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn copy_rejects_duplicate_target_without_overwriting_it() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("source-duplicate-guard"),
        cwd: "/src".to_string(),
    };
    let target = Info {
        id: acp::SessionId::new("target-duplicate-guard"),
        cwd: "/dst".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    adapter
        .init_session(&target, default_model_id())
        .await
        .unwrap();
    adapter
        .append_chat_message(&target, &ConversationItem::user("keep me"))
        .await
        .unwrap();

    let error = adapter
        .copy_session_data(&source, &target, CopySessionOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    let loaded = adapter.load_session(&target).await.unwrap();
    assert_eq!(loaded.chat_history.len(), 1);
    assert_eq!(loaded.chat_history[0].text_content(), "keep me");
}

fn fork_user_chunk(session_id: &str, text: &str, prompt_index: usize) -> SessionUpdate {
    let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
        text.to_string(),
    )))
    .meta(
        serde_json::json!({ "promptIndex": prompt_index })
            .as_object()
            .cloned(),
    );
    SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
        acp::SessionId::new(session_id),
        acp::SessionUpdate::UserMessageChunk(chunk),
    )))
}

fn fork_agent_chunk(session_id: &str, text: &str) -> SessionUpdate {
    SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
        acp::SessionId::new(session_id),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text.to_string()),
        ))),
    )))
}

fn fork_rewind_marker(session_id: &str, target_prompt_index: usize) -> SessionUpdate {
    use crate::extensions::notification::{
        SessionNotification as XaiSessionNotification, SessionUpdate as XaiSessionUpdateType,
    };
    SessionUpdate::Xai(Box::new(XaiSessionNotification {
        session_id: acp::SessionId::new(session_id),
        update: XaiSessionUpdateType::RewindMarker {
            target_prompt_index,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        },
        meta: None,
    }))
}

fn chat_user(text: &str, prompt_index: usize) -> ConversationItem {
    let mut item = ConversationItem::user(text);
    item.set_prompt_index(prompt_index);
    item
}

/// Fork truncation targets the live branch (dead-branch runs from a
/// prior rewind overlap its stamps, since indices are branch-local) and keeps
/// prompt N inclusive in both the updates and chat (model-context) files.
#[tokio::test]
async fn copy_session_data_fork_truncates_live_branch_inclusive() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let sid = "src-rewound";
    let source_info = Info {
        id: acp::SessionId::new(sid),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    // Prompt 1 was rewound and retried: P1-dead/A1-dead is the dead branch.
    for update in [
        fork_user_chunk(sid, "P0", 0),
        fork_agent_chunk(sid, "A0"),
        fork_user_chunk(sid, "P1-dead", 1),
        fork_agent_chunk(sid, "A1-dead"),
        fork_rewind_marker(sid, 1),
        fork_user_chunk(sid, "P1b", 1),
        fork_agent_chunk(sid, "A1b"),
        fork_user_chunk(sid, "P2", 2),
    ] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }
    for item in [
        chat_user("P0", 0),
        ConversationItem::assistant("A0"),
        chat_user("P1b", 1),
        ConversationItem::assistant("A1b"),
        chat_user("P2", 2),
    ] {
        adapter
            .append_chat_message(&source_info, &item)
            .await
            .unwrap();
    }

    let fork_at = |target: usize, fork_id: &str| {
        let target_info = Info {
            id: acp::SessionId::new(fork_id),
            cwd: "/src".to_string(),
        };
        let options = CopySessionOptions {
            target_prompt_index: Some(target),
            ..Default::default()
        };
        (target_info, options)
    };

    // Fork at live prompt 1: keeps P0, A0, P1b, A1b in both files. A raw
    // run count would cut inside the dead branch instead.
    let (target_info, options) = fork_at(1, "fork-at-1");
    let result = adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();
    assert_eq!(result.updates_copied, 4);
    assert_eq!(result.chat_messages_copied, 4);
    let loaded = adapter.load_session(&target_info).await.unwrap();
    let last = loaded.updates.last().unwrap();
    assert!(
        matches!(
            last,
            SessionUpdate::Acp(n) if matches!(
                &n.update,
                acp::SessionUpdate::AgentMessageChunk(c)
                    if matches!(&c.content, acp::ContentBlock::Text(t) if t.text == "A1b")
            )
        ),
        "fork must end at the live branch's A1b, got {last:?}"
    );

    // Prompt 0 is kept inclusive; an exclusive cut would copy an empty
    // model context here.
    let (target_info, options) = fork_at(0, "fork-at-0");
    let result = adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();
    assert_eq!(result.updates_copied, 2, "P0 + A0");
    assert_eq!(result.chat_messages_copied, 2, "P0 + A0 in model context");
}

/// Without a `target_prompt_index`, every line streams through: rewind
/// markers and dead branches survive a plain fork. A regression that routes
/// the default path through the rewind filter would strip them silently.
#[tokio::test]
async fn copy_session_data_without_prompt_target_preserves_dead_branches() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let sid = "src-dead-branch";
    let source_info = Info {
        id: acp::SessionId::new(sid),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    for update in [
        fork_user_chunk(sid, "P0", 0),
        fork_agent_chunk(sid, "A0"),
        fork_user_chunk(sid, "P1-dead", 1),
        fork_rewind_marker(sid, 1),
        fork_user_chunk(sid, "P1b", 1),
    ] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }

    let target_info = Info {
        id: acp::SessionId::new("fork-plain"),
        cwd: "/src".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();
    assert_eq!(
        result.updates_copied, 5,
        "dead branch and rewind marker must survive a plain fork"
    );
}

/// The streaming fork copy skips torn or undecodable lines like the load
/// path does, both with and without a prompt-index cut.
#[tokio::test]
async fn copy_session_data_skips_torn_updates_lines() {
    use std::io::Write as _;

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let sid = "src-torn";
    let source_info = Info {
        id: acp::SessionId::new(sid),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    for update in [fork_user_chunk(sid, "P0", 0), fork_agent_chunk(sid, "A0")] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }
    // A torn append (truncated JSON) and an undecodable line.
    let updates_path = adapter.updates_file_path(&source_info).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&updates_path)
        .unwrap();
    file.write_all(b"{\"method\":\"session/update\",\"params\":{tor\n")
        .unwrap();
    file.write_all(&[0xFF, 0xFE, b'\n']).unwrap();
    drop(file);
    adapter
        .append_update(&source_info, &fork_user_chunk(sid, "P1", 1))
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-torn"),
        cwd: "/src".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();
    assert_eq!(result.updates_copied, 3, "P0 + A0 + P1, torn lines dropped");
    let loaded = adapter.load_session(&target_info).await.unwrap();
    assert_eq!(loaded.updates.len(), 3);

    let target_info = Info {
        id: acp::SessionId::new("fork-torn-at-0"),
        cwd: "/src".to_string(),
    };
    let options = CopySessionOptions {
        target_prompt_index: Some(0),
        ..Default::default()
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();
    assert_eq!(result.updates_copied, 2, "P0 + A0; torn tail and P1 cut");
}

/// A torn line inside a multi-chunk user run ends the run during the prompt
/// cut, so the second chunk opens a new counted turn, matching replay's
/// raw-line semantics. Pins the boundary so a classifier change is deliberate.
#[tokio::test]
async fn torn_line_inside_user_run_splits_the_run_for_prompt_cut() {
    use std::io::Write as _;

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let sid = "src-torn-mid-run";
    let source_info = Info {
        id: acp::SessionId::new(sid),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    for update in [
        fork_user_chunk(sid, "P0", 0),
        fork_agent_chunk(sid, "A0"),
        fork_user_chunk(sid, "P1a", 1),
    ] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }
    let updates_path = adapter.updates_file_path(&source_info).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&updates_path)
        .unwrap();
    file.write_all(b"{torn mid-run\n").unwrap();
    drop(file);
    for update in [
        fork_user_chunk(sid, "P1b", 1),
        fork_agent_chunk(sid, "A1"),
        fork_user_chunk(sid, "P2", 2),
    ] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }

    let target_info = Info {
        id: acp::SessionId::new("fork-torn-mid-run"),
        cwd: "/src".to_string(),
    };
    let options = CopySessionOptions {
        target_prompt_index: Some(1),
        ..Default::default()
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();
    // P1b re-counts as a turn after the torn split, so the cut lands before
    // it: P0, A0, P1a survive. The contiguous-run cut would have kept 5.
    assert_eq!(result.updates_copied, 3, "P0 + A0 + P1a");
}

fn create_test_chat_messages() -> Vec<ConversationItem> {
    vec![
        ConversationItem::user("Hello world"),
        ConversationItem::user("How are you?"),
        ConversationItem::user("Test message"),
    ]
}

fn create_test_notification() -> acp::SessionNotification {
    acp::SessionNotification::new(
        acp::SessionId::new("test-session-123"),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new("Test response".to_string()),
        ))),
    )
}

fn create_test_plan_state() -> TodoState {
    TodoState::default()
}

#[tokio::test]
async fn copy_session_data_copies_compaction_segments_when_enabled() {
    use crate::extensions::notification::CompactionSegmentFile;
    use xai_grok_sampling_types::ConversationItem;

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("seg-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    for msg in &create_test_chat_messages() {
        adapter
            .append_chat_message(&source_info, msg)
            .await
            .unwrap();
    }

    // Two compaction segments → compaction/{segment_000.md, segment_001.md, INDEX.md}.
    let seg = |s: &str| CompactionSegmentFile {
        items: vec![ConversationItem::user("a"), ConversationItem::user("b")],
        summary: s.to_string(),
        detail: xai_chat_state::CompactionDetail::Verbose,
        timestamp: "2026-01-01T00:00:00Z".to_string(),
    };
    adapter
        .write_compaction_segment(&source_info, &seg("first"))
        .await
        .unwrap();
    adapter
        .write_compaction_segment(&source_info, &seg("second"))
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("seg-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(
            &source_info,
            &target_info,
            CopySessionOptions {
                copy_compaction_segments: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.compaction_segments_copied, 3); // 2 segments + INDEX.md

    let dst = adapter
        .session_dir(&target_info)
        .join(xai_compaction_transcript::COMPACTION_DIR);
    assert!(dst.join("segment_000.md").is_file());
    assert!(dst.join("segment_001.md").is_file());
    assert!(dst.join("INDEX.md").is_file());
    assert!(
        std::fs::read_to_string(dst.join("segment_000.md"))
            .unwrap()
            .contains("# HISTORICAL -- DO NOT EDIT")
    );

    let target2 = Info {
        id: acp::SessionId::new("seg-dst-default"),
        cwd: "/target2/workspace".to_string(),
    };
    let result2 = adapter
        .copy_session_data(&source_info, &target2, CopySessionOptions::default())
        .await
        .unwrap();
    assert_eq!(result2.compaction_segments_copied, 0);
    assert!(
        !adapter
            .session_dir(&target2)
            .join(xai_compaction_transcript::COMPACTION_DIR)
            .exists()
    );
}

/// Inherited `transcript_hint` text names the parent `session_dir/compaction`.
/// After a production-style fork copy the child must point at its own copied
/// archive so deleting the parent cannot break history (issue #345) — while
/// the transcript and `summary.json`, which are words a user or the model
/// wrote, copy through untouched even when they quote that same path.
#[tokio::test]
async fn issue345_fork_rewrites_parent_compaction_transcript_hint() {
    use crate::extensions::notification::CompactionSegmentFile;
    use xai_chat_state::{CompactionDetail, CompactionMode};

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let sid = "issue345-parent";
    let source_info = Info {
        id: acp::SessionId::new(sid),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    adapter
        .write_compaction_segment(
            &source_info,
            &CompactionSegmentFile {
                items: vec![ConversationItem::user("pre-compact turn")],
                summary: "segment for issue345".to_string(),
                detail: CompactionDetail::Verbose,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            },
        )
        .await
        .unwrap();

    let parent_compaction = adapter
        .session_dir(&source_info)
        .join(xai_compaction_transcript::COMPACTION_DIR);
    let parent_loc = parent_compaction.to_string_lossy().into_owned();
    let hint = CompactionMode::Segments(CompactionDetail::Verbose)
        .transcript_hint(Some(parent_loc.as_str()))
        .expect("segments hint needs a location");
    // Workspace path that cwd transform would rewrite; hint rewrite must not.
    let decoy_cwd_path = "/source/workspace/src/main.rs";
    let inherited = format!("Compacted history still needs {decoy_cwd_path}.{hint}");
    adapter
        .append_chat_message(&source_info, &ConversationItem::user_meta(inherited))
        .await
        .unwrap();
    // The agent quoted the generated hint in its own reply. That is content,
    // not metadata: the fork must leave it alone.
    adapter
        .append_update(&source_info, &fork_agent_chunk(sid, &hint))
        .await
        .unwrap();

    // The dashboard one-liner worded exactly like the generated hint. #423
    // inverted this: it is the model's own display text, so the fork copies
    // it as authored instead of retargeting it.
    let mut source_summary = adapter.read_summary_sync(&source_info).unwrap();
    source_summary.last_turn_summary = Some(hint.clone());
    adapter
        .write_summary_sync(&source_info, &source_summary)
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("issue345-child"),
        cwd: "/target/workspace".to_string(),
    };
    // Same options the production fork path uses, plus skip_cwd_transform so
    // a worktree-style copy still rewrites only the compaction hint.
    adapter
        .copy_session_data_sync(
            &source_info,
            &target_info,
            CopySessionOptions {
                parent_session_id: Some(sid.to_string()),
                copy_compaction_segments: true,
                skip_cwd_transform: true,
                ..Default::default()
            },
        )
        .unwrap();

    let child_compaction = adapter
        .session_dir(&target_info)
        .join(xai_compaction_transcript::COMPACTION_DIR);
    let child_loc = child_compaction.to_string_lossy().into_owned();
    assert_ne!(parent_loc, child_loc, "fork must land in a new session dir");

    let loaded = adapter.load_session(&target_info).await.unwrap();
    let chat_blob: String = loaded
        .chat_history
        .iter()
        .map(ConversationItem::text_content)
        .collect();
    assert!(
        chat_blob.contains(&child_loc),
        "child chat must name the child compaction dir, got {chat_blob}"
    );
    assert!(
        !chat_blob.contains(&parent_loc),
        "child chat must not keep the parent compaction dir, got {chat_blob}"
    );
    assert!(
        chat_blob.contains(decoy_cwd_path),
        "hint rewrite must not touch workspace cwd text, got {chat_blob}"
    );

    let last = loaded
        .summary
        .last_turn_summary
        .as_deref()
        .expect("copied last_turn_summary");
    assert_eq!(
        last, hint,
        "copied summary must keep the authored last_turn_summary verbatim"
    );
    assert!(
        !last.contains(&child_loc),
        "fork must not retarget the copied summary, got {last}"
    );

    let updates_blob = std::fs::read_to_string(adapter.updates_file(&target_info)).unwrap();
    assert!(
        updates_blob.contains(&parent_loc),
        "agent message content must survive the fork verbatim, got {updates_blob}"
    );
    assert!(
        !updates_blob.contains(&child_loc),
        "fork must not rewrite transcript content, got {updates_blob}"
    );

    std::fs::remove_dir_all(adapter.session_dir(&source_info)).unwrap();
    let copied_segment = std::fs::read_to_string(child_compaction.join("segment_000.md")).unwrap();
    assert!(
        copied_segment.contains("# HISTORICAL -- DO NOT EDIT"),
        "child compaction files must stay readable after the parent is deleted"
    );
    assert!(child_compaction.join("INDEX.md").is_file());
}

/// Successor to `issue345_updates_rewrite_runs_after_json_unescape`, which
/// pinned the deleted `updates.jsonl` walk. Windows checkpoints store
/// `C:\\…\\compaction` where `Path::to_string_lossy` yields `C:\…\compaction`,
/// so a textual replace on the file bytes never matches (Codex P2 3793486694).
/// The rebind runs on the deserialized items, so the escaping never enters —
/// this pins that it stays that way.
#[test]
fn issue345_checkpoint_rebind_runs_on_deserialized_items() {
    use crate::extensions::notification::CompactionCheckpointFile;
    use xai_chat_state::{CompactionDetail, CompactionMode};

    let source = std::path::PathBuf::from(r"C:\Users\iml1s\.grok\sessions\parent\compaction");
    let target = std::path::PathBuf::from(r"C:\Users\iml1s\.grok\sessions\child\compaction");
    let hint = CompactionMode::Segments(CompactionDetail::default())
        .transcript_hint(Some(source.to_string_lossy().as_ref()))
        .expect("segments hint needs a location");
    let file = CompactionCheckpointFile {
        checkpoint_id: "win".to_string(),
        prompt_index_at_compaction: 1,
        compacted_history: vec![ConversationItem::user_meta(format!("summary body{hint}"))],
        schema_version: 1,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        original_user_info: None,
        reread_file_paths: vec![],
    };
    let bytes = serde_json::to_vec_pretty(&file).expect("checkpoint bytes");
    assert!(
        String::from_utf8_lossy(&bytes).contains(r"C:\\Users"),
        "fixture must JSON-escape backslashes"
    );

    let rebinds = super::CheckpointRebinds {
        segments: Some((source, target)),
        // Not under test: a pair pointing at itself rebinds nothing.
        transcript: (
            std::path::PathBuf::from(r"C:\same\updates.jsonl"),
            std::path::PathBuf::from(r"C:\same\updates.jsonl"),
        ),
    };
    let rebound = super::rebound_checkpoint_bytes(
        &bytes,
        &rebinds,
        std::path::Path::new("checkpoint.json"),
        &acp::SessionId::new("win-src"),
    )
    .expect("rebind")
    .expect("segments hint must rebind");
    let out: CompactionCheckpointFile = serde_json::from_slice(&rebound).expect("reparse");
    let text = out.compacted_history[0].text_content();
    assert!(text.contains(r"\child\compaction"), "got {text}");
    assert!(!text.contains(r"\parent\compaction"), "got {text}");
}

#[tokio::test]
async fn copied_compaction_hint_survives_source_deletion() {
    use crate::extensions::notification::CompactionSegmentFile;
    use xai_chat_state::{CompactionDetail, CompactionMode};

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let shared_cwd = "/shared/workspace".to_string();
    let source_info = Info {
        id: acp::SessionId::new("compaction-hint-src"),
        cwd: shared_cwd.clone(),
    };
    let target_info = Info {
        id: acp::SessionId::new("compaction-hint-dst"),
        cwd: shared_cwd,
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    let segment = CompactionSegmentFile {
        items: vec![ConversationItem::user("verbatim detail")],
        summary: "segment summary".to_string(),
        detail: CompactionDetail::Verbose,
        timestamp: "2026-01-01T00:00:00Z".to_string(),
    };
    adapter
        .write_compaction_segment(&source_info, &segment)
        .await
        .unwrap();

    let source_archive = adapter
        .session_dir(&source_info)
        .join(xai_compaction_transcript::COMPACTION_DIR);
    let target_archive = adapter
        .session_dir(&target_info)
        .join(xai_compaction_transcript::COMPACTION_DIR);
    let source_archive_text = source_archive.to_string_lossy().into_owned();
    let target_archive_text = target_archive.to_string_lossy().into_owned();
    let hint = CompactionMode::Segments(CompactionDetail::Verbose)
        .transcript_hint(Some(&source_archive_text))
        .unwrap();
    let same_meta_hint_decoy = format!("same-meta quoted canonical decoy:{hint}");
    adapter
        .append_chat_message(
            &source_info,
            &ConversationItem::user_meta(format!(
                "summary body\n{same_meta_hint_decoy}\n\
                 unchanged exact decoy: {source_archive_text}\n\
                 unchanged prefixed decoy: {source_archive_text}-backup{hint}"
            )),
        )
        .await
        .unwrap();
    let quoted_hint = format!("real user quoted the generated hint verbatim:{hint}");
    adapter
        .append_chat_message(&source_info, &ConversationItem::user(quoted_hint.clone()))
        .await
        .unwrap();

    adapter
        .copy_session_data(
            &source_info,
            &target_info,
            CopySessionOptions {
                copy_compaction_segments: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    std::fs::remove_dir_all(adapter.session_dir(&source_info)).unwrap();

    let loaded = adapter.load_session(&target_info).await.unwrap();
    let copied_summary = loaded.chat_history[0].text_content();
    let target_hint = CompactionMode::Segments(CompactionDetail::Verbose)
        .transcript_hint(Some(&target_archive_text))
        .unwrap();
    assert!(copied_summary.ends_with(&target_hint));
    assert!(copied_summary.contains(&same_meta_hint_decoy));
    assert!(copied_summary.contains("summary body"));
    assert!(copied_summary.contains(&format!("unchanged exact decoy: {source_archive_text}")));
    assert!(copied_summary.contains(&format!(
        "unchanged prefixed decoy: {source_archive_text}-backup"
    )));
    assert_eq!(loaded.chat_history[1].text_content(), quoted_hint);
    assert!(!source_archive.exists());
    assert!(target_archive.join("INDEX.md").is_file());
    assert!(
        std::fs::read_to_string(target_archive.join("segment_000.md"))
            .unwrap()
            .contains("verbatim detail")
    );
}

#[tokio::test]
async fn copied_transcript_hint_survives_source_deletion() {
    use xai_chat_state::CompactionMode;

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let shared_cwd = "/shared/workspace".to_string();
    let source_info = Info {
        id: acp::SessionId::new("transcript-hint-src"),
        cwd: shared_cwd.clone(),
    };
    let target_info = Info {
        id: acp::SessionId::new("transcript-hint-dst"),
        cwd: shared_cwd,
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    adapter
        .append_update(
            &source_info,
            &SessionUpdate::Acp(Box::new(create_test_notification())),
        )
        .await
        .unwrap();

    let source_transcript = adapter.updates_file(&source_info);
    let target_transcript = adapter.updates_file(&target_info);
    let source_transcript_text = source_transcript.to_string_lossy().into_owned();
    let target_transcript_text = target_transcript.to_string_lossy().into_owned();
    let hint = CompactionMode::Transcript
        .transcript_hint(Some(&source_transcript_text))
        .unwrap();
    let same_meta_hint_decoy = format!("same-meta quoted canonical decoy:{hint}");
    adapter
        .append_chat_message(
            &source_info,
            &ConversationItem::user_meta(format!(
                "summary body\n{same_meta_hint_decoy}\n\
                 unchanged exact decoy: {source_transcript_text}\n\
                 unchanged prefixed decoy: {source_transcript_text}.backup{hint}"
            )),
        )
        .await
        .unwrap();
    let quoted_hint = format!("real user quoted the generated hint verbatim:{hint}");
    adapter
        .append_chat_message(&source_info, &ConversationItem::user(quoted_hint.clone()))
        .await
        .unwrap();

    adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();
    std::fs::remove_dir_all(adapter.session_dir(&source_info)).unwrap();

    let loaded = adapter.load_session(&target_info).await.unwrap();
    let copied_summary = loaded.chat_history[0].text_content();
    let target_hint = CompactionMode::Transcript
        .transcript_hint(Some(&target_transcript_text))
        .unwrap();
    assert!(copied_summary.ends_with(&target_hint));
    assert!(copied_summary.contains(&same_meta_hint_decoy));
    assert!(copied_summary.contains("summary body"));
    assert!(copied_summary.contains(&format!("unchanged exact decoy: {source_transcript_text}")));
    assert!(copied_summary.contains(&format!(
        "unchanged prefixed decoy: {source_transcript_text}.backup"
    )));
    assert_eq!(loaded.chat_history[1].text_content(), quoted_hint);
    assert!(!source_transcript.exists());
    assert!(target_transcript.is_file());
    assert!(
        std::fs::read_to_string(target_transcript)
            .unwrap()
            .contains("Test response")
    );
}

/// A `compaction_checkpoint` record pointing at `compaction_checkpoints/{id}.json`.
fn checkpoint_record(id: &str) -> SessionUpdate {
    checkpoint_record_with_path(id, &format!("compaction_checkpoints/{id}.json"))
}

/// A `compaction_checkpoint` record with an arbitrary `checkpoint_file` path.
fn checkpoint_record_with_path(id: &str, checkpoint_file: &str) -> SessionUpdate {
    use crate::extensions::notification::{
        CompactionCheckpointInfo, SessionNotification as XaiSessionNotification,
        SessionUpdate as XaiSessionUpdateType,
    };
    SessionUpdate::Xai(Box::new(XaiSessionNotification {
        session_id: acp::SessionId::new("ckpt-src"),
        update: XaiSessionUpdateType::CompactionCheckpoint(Box::new(CompactionCheckpointInfo {
            checkpoint_id: id.to_string(),
            prompt_index_at_compaction: 1,
            checkpoint_file: checkpoint_file.to_string(),
            auto_continue: None,
            schema_version: 1,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })),
        meta: None,
    }))
}

/// A user message chunk stamped with `_meta.promptIndex` so
/// `truncate_for_prompt_by` counts it as a turn.
fn prompt_user_chunk(text: &str, prompt_index: usize) -> SessionUpdate {
    SessionUpdate::Acp(Box::new(acp::SessionNotification::new(
        acp::SessionId::new("ckpt-src"),
        acp::SessionUpdate::UserMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                text.to_string(),
            )))
            .meta(
                serde_json::json!({ "promptIndex": prompt_index })
                    .as_object()
                    .cloned(),
            ),
        ),
    )))
}

async fn write_checkpoint_file(adapter: &JsonlStorageAdapter, info: &Info, id: &str) {
    use crate::extensions::notification::CompactionCheckpointFile;
    adapter
        .write_compaction_checkpoint(
            info,
            &CompactionCheckpointFile {
                checkpoint_id: id.to_string(),
                prompt_index_at_compaction: 1,
                compacted_history: vec![],
                schema_version: 1,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                original_user_info: None,
                reread_file_paths: vec![],
            },
        )
        .await
        .unwrap();
}

/// #345 with the case Codex flagged on PR #383: the fork must rebind the
/// checkpoint's generated hint — the copy a cross-compaction rewind replays —
/// while leaving a transcript that quotes the very same path byte-identical.
/// Transcript records are words a user and the model wrote; rewriting them
/// would hand the model back a prompt nobody typed.
#[tokio::test]
async fn fork_rebinds_checkpoint_hint_without_touching_quoted_transcript_paths() {
    use crate::extensions::notification::{CompactionCheckpointFile, CompactionSegmentFile};
    use xai_chat_state::{CompactionDetail, CompactionMode};

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source_info = Info {
        id: acp::SessionId::new("ckpt-rebind-src"),
        cwd: "/shared/workspace".to_string(),
    };
    let target_info = Info {
        id: acp::SessionId::new("ckpt-rebind-dst"),
        cwd: "/shared/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    adapter
        .write_compaction_segment(
            &source_info,
            &CompactionSegmentFile {
                items: vec![ConversationItem::user("pre-compact turn")],
                summary: "segment summary".to_string(),
                detail: CompactionDetail::Verbose,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            },
        )
        .await
        .unwrap();

    let parent_archive = adapter
        .session_dir(&source_info)
        .join(xai_compaction_transcript::COMPACTION_DIR);
    let parent_loc = parent_archive.to_string_lossy().into_owned();
    let hint = CompactionMode::Segments(CompactionDetail::Verbose)
        .transcript_hint(Some(parent_loc.as_str()))
        .expect("segments hint needs a location");

    // The user asked about the archive by name, and the agent answered naming
    // a file inside it. Both are content; neither is a generated hint. The
    // trailing `/` matters: the deleted rewrite's component boundary let it
    // through, so these are exactly the bytes it corrupted.
    let quoted_prompt = format!("what is in {parent_loc}/segment_000.md?");
    adapter
        .append_update(&source_info, &prompt_user_chunk(&quoted_prompt, 0))
        .await
        .unwrap();
    let quoted_reply = format!("I read {parent_loc}/INDEX.md for you.");
    adapter
        .append_update(
            &source_info,
            &fork_agent_chunk("ckpt-rebind-src", &quoted_reply),
        )
        .await
        .unwrap();

    // The generated hint's replay home: the checkpoint the fork must rebind.
    adapter
        .append_update(&source_info, &checkpoint_record("ckpt-rebind"))
        .await
        .unwrap();
    adapter
        .write_compaction_checkpoint(
            &source_info,
            &CompactionCheckpointFile {
                checkpoint_id: "ckpt-rebind".to_string(),
                prompt_index_at_compaction: 1,
                compacted_history: vec![ConversationItem::user_meta(format!("summary body{hint}"))],
                schema_version: 1,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                original_user_info: None,
                reread_file_paths: vec![],
            },
        )
        .await
        .unwrap();

    adapter
        .copy_session_data(
            &source_info,
            &target_info,
            CopySessionOptions {
                copy_compaction_segments: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let child_archive = adapter
        .session_dir(&target_info)
        .join(xai_compaction_transcript::COMPACTION_DIR);
    let child_loc = child_archive.to_string_lossy().into_owned();
    assert_ne!(parent_loc, child_loc, "fork must land in a new session dir");

    // Transcript: unchanged. Compare JSON-escaped forms so this also holds on
    // Windows, where the path's `\` is stored as `\\`.
    let child_updates = std::fs::read_to_string(adapter.updates_file(&target_info)).unwrap();
    let escaped = |text: &str| {
        serde_json::to_string(text)
            .expect("escape")
            .trim_matches('"')
            .to_string()
    };
    assert!(
        child_updates.contains(&escaped(&quoted_prompt)),
        "user prompt must survive the fork verbatim, got {child_updates}"
    );
    assert!(
        child_updates.contains(&escaped(&quoted_reply)),
        "agent reply must survive the fork verbatim, got {child_updates}"
    );
    assert!(
        !child_updates.contains(&escaped(&child_loc)),
        "no transcript record may name the child archive, got {child_updates}"
    );

    // Checkpoint: rebound, and readable once the parent is gone.
    std::fs::remove_dir_all(adapter.session_dir(&source_info)).unwrap();
    let copied: CompactionCheckpointFile = serde_json::from_slice(
        &std::fs::read(
            adapter
                .session_dir(&target_info)
                .join("compaction_checkpoints/ckpt-rebind.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let restored = copied.compacted_history[0].text_content();
    assert!(
        restored.contains(&child_loc),
        "replayed compacted history must name the child archive, got {restored}"
    );
    assert!(
        !restored.contains(&parent_loc),
        "replayed compacted history must not keep the parent archive, got {restored}"
    );
    assert!(child_archive.join("segment_000.md").is_file());
}

#[tokio::test]
async fn copy_session_data_copies_referenced_compaction_checkpoints() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    // Two records referencing the same file (e.g. a chained fork) must
    // still produce one copy.
    for _ in 0..2 {
        adapter
            .append_update(&source_info, &checkpoint_record("ckpt-a"))
            .await
            .unwrap();
    }
    write_checkpoint_file(&adapter, &source_info, "ckpt-a").await;

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 1);
    assert_eq!(
        result.updates_copied, 2,
        "checkpoint records must be copied"
    );
    let rel = "compaction_checkpoints/ckpt-a.json";
    let copied = std::fs::read(adapter.session_dir(&target_info).join(rel)).unwrap();
    let original = std::fs::read(adapter.session_dir(&source_info).join(rel)).unwrap();
    assert_eq!(copied, original, "checkpoint file must be copied verbatim");
}

#[tokio::test]
async fn fork_filter_copy_skips_compaction_checkpoints() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    adapter
        .append_update(&source_info, &checkpoint_record("ckpt-a"))
        .await
        .unwrap();
    write_checkpoint_file(&adapter, &source_info, "ckpt-a").await;

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    // fork_filter clears the copied updates, so no record survives and no
    // checkpoint file should come along.
    let result = adapter
        .copy_session_data(
            &source_info,
            &target_info,
            CopySessionOptions {
                fork_filter: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 0);
    assert_eq!(
        result.updates_copied, 0,
        "fork_filter clears the transcript"
    );
    assert!(
        !adapter
            .session_dir(&target_info)
            .join("compaction_checkpoints")
            .exists()
    );
}

#[tokio::test]
async fn target_prompt_index_truncation_gates_checkpoint_copy() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    for update in [
        prompt_user_chunk("P0", 0),
        checkpoint_record("ckpt-early"),
        prompt_user_chunk("P1", 1),
        prompt_user_chunk("P2", 2),
        checkpoint_record("ckpt-late"),
    ] {
        adapter.append_update(&source_info, &update).await.unwrap();
    }
    write_checkpoint_file(&adapter, &source_info, "ckpt-early").await;
    write_checkpoint_file(&adapter, &source_info, "ckpt-late").await;

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    // Truncating to prompt 0 keeps [P0, ckpt-early] and drops the rest.
    let result = adapter
        .copy_session_data(
            &source_info,
            &target_info,
            CopySessionOptions {
                target_prompt_index: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 1);
    let dst = adapter
        .session_dir(&target_info)
        .join("compaction_checkpoints");
    assert!(
        dst.join("ckpt-early.json").is_file(),
        "record before the cut keeps its checkpoint file"
    );
    assert!(
        !dst.join("ckpt-late.json").exists(),
        "record after the cut must not pull its checkpoint file"
    );
}

#[tokio::test]
async fn dangling_checkpoint_record_copies_without_file() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    // Record present but its file was never written (already-broken source).
    adapter
        .append_update(&source_info, &checkpoint_record("ckpt-gone"))
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 0);
    assert_eq!(result.updates_copied, 1, "the record itself still copies");
    assert!(
        !adapter
            .session_dir(&target_info)
            .join("compaction_checkpoints/ckpt-gone.json")
            .exists()
    );
}

#[tokio::test]
async fn checkpoint_record_with_non_checkpoint_path_is_not_copied() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    // A doctored record addressing another session file: copying it would
    // clobber the target's rewritten updates.jsonl with raw source bytes.
    adapter
        .append_update(
            &source_info,
            &checkpoint_record_with_path("ckpt-evil", "updates.jsonl"),
        )
        .await
        .unwrap();
    // Real checkpoint dir present so the path-shape guard (not the
    // missing-dir guard) is what rejects the record.
    std::fs::create_dir_all(
        adapter
            .session_dir(&source_info)
            .join("compaction_checkpoints"),
    )
    .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 0);
    // The target updates must keep the transformed record (session id
    // rewritten to the fork), not the source file's raw bytes.
    let loaded = adapter.load_session(&target_info).await.unwrap();
    assert_eq!(loaded.updates.len(), 1);
    match &loaded.updates[0] {
        SessionUpdate::Xai(notification) => {
            assert_eq!(notification.session_id.0.as_ref(), "ckpt-dst");
        }
        other => panic!("Expected Xai update, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_checkpoint_file_is_not_copied() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    adapter
        .append_update(&source_info, &checkpoint_record("ckpt-a"))
        .await
        .unwrap();
    // Plant a symlink where the checkpoint file should be: the copy must
    // not follow it out of the session directory.
    let ckpt_dir = adapter
        .session_dir(&source_info)
        .join("compaction_checkpoints");
    std::fs::create_dir_all(&ckpt_dir).unwrap();
    let outside = temp_dir.path().join("outside.json");
    std::fs::write(&outside, b"outside bytes").unwrap();
    std::os::unix::fs::symlink(&outside, ckpt_dir.join("ckpt-a.json")).unwrap();

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 0);
    assert!(
        !adapter
            .session_dir(&target_info)
            .join("compaction_checkpoints/ckpt-a.json")
            .exists()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_checkpoint_dir_is_not_copied() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("ckpt-src"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    adapter
        .append_update(&source_info, &checkpoint_record("ckpt-a"))
        .await
        .unwrap();
    // Plant the whole compaction_checkpoints dir as a symlink to an
    // outside dir holding a matching .json: nothing may be copied.
    let outside_dir = temp_dir.path().join("outside");
    std::fs::create_dir_all(&outside_dir).unwrap();
    std::fs::write(outside_dir.join("ckpt-a.json"), b"outside bytes").unwrap();
    std::os::unix::fs::symlink(
        &outside_dir,
        adapter
            .session_dir(&source_info)
            .join("compaction_checkpoints"),
    )
    .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("ckpt-dst"),
        cwd: "/target/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    assert_eq!(result.compaction_checkpoints_copied, 0);
    assert!(
        !adapter
            .session_dir(&target_info)
            .join("compaction_checkpoints")
            .exists()
    );
}

#[tokio::test]
async fn copy_session_data_basic() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("source-session-123"),
        cwd: "/source/workspace".to_string(),
    };

    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    let messages = create_test_chat_messages();
    for msg in &messages {
        adapter
            .append_chat_message(&source_info, msg)
            .await
            .unwrap();
    }

    let notification = create_test_notification();
    adapter
        .append_update(&source_info, &SessionUpdate::Acp(Box::new(notification)))
        .await
        .unwrap();

    let plan_state = create_test_plan_state();
    adapter
        .write_plan_state(&source_info, &plan_state)
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-source-session-123-abcd1234"),
        cwd: "/target/workspace".to_string(),
    };

    let options = CopySessionOptions {
        parent_session_id: Some("source-session-123".to_string()),
        new_model_id: None,
        target_prompt_index: None,
        ..Default::default()
    };
    let result = adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();

    assert_eq!(result.chat_messages_copied, 3);
    assert_eq!(result.updates_copied, 1);
    assert!(result.plan_state_copied);

    let loaded = adapter.load_session(&target_info).await.unwrap();
    assert_eq!(loaded.summary.info.id, target_info.id);
    assert_eq!(loaded.summary.info.cwd, "/target/workspace");
    assert_eq!(loaded.summary.session_kind.as_deref(), Some("fork"));
    assert_eq!(
        loaded.summary.parent_session_id,
        Some("source-session-123".to_string())
    );
    assert!(loaded.summary.forked_at.is_some());
    assert_eq!(loaded.chat_history.len(), 3);
    assert_eq!(loaded.updates.len(), 1);
    match &loaded.updates[0] {
        SessionUpdate::Acp(notification) => {
            assert_eq!(
                notification.session_id.0.as_ref(),
                "fork-source-session-123-abcd1234"
            );
        }
        _ => panic!("Expected ACP update"),
    }
    assert!(loaded.plan_state.is_some());
}

#[tokio::test]
async fn copy_session_data_without_plan() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("source-no-plan"),
        cwd: "/source/workspace".to_string(),
    };

    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    adapter
        .append_chat_message(&source_info, &ConversationItem::user("Hello"))
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-source-no-plan-12345678"),
        cwd: "/target/workspace".to_string(),
    };

    let result = adapter
        .copy_session_data(&source_info, &target_info, Default::default())
        .await
        .unwrap();

    assert_eq!(result.chat_messages_copied, 1);
    assert_eq!(result.updates_copied, 0);
    assert!(!result.plan_state_copied);

    let loaded = adapter.load_session(&target_info).await.unwrap();
    assert!(loaded.plan_state.is_none());
}

#[tokio::test]
async fn copy_session_data_transforms_xai_updates() {
    use crate::extensions::notification::{
        DiffContent, SessionNotification as XaiSessionNotification,
        SessionUpdate as XaiSessionUpdateType,
    };

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("source-xai"),
        cwd: "/source".to_string(),
    };

    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    let xai_notification = XaiSessionNotification {
        session_id: acp::SessionId::new("source-xai"),
        update: XaiSessionUpdateType::DiffReview {
            content: vec![DiffContent {
                diff: acp::Diff::new(std::path::PathBuf::from("/test/file.rs"), "new".to_string())
                    .old_text(Some("old".to_string())),
            }],
        },
        meta: None,
    };
    adapter
        .append_update(
            &source_info,
            &SessionUpdate::Xai(Box::new(xai_notification)),
        )
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-source-xai-abcd1234"),
        cwd: "/target".to_string(),
    };

    adapter
        .copy_session_data(&source_info, &target_info, Default::default())
        .await
        .unwrap();

    let loaded = adapter.load_session(&target_info).await.unwrap();
    match &loaded.updates[0] {
        SessionUpdate::Xai(notification) => {
            assert_eq!(
                notification.session_id.0.as_ref(),
                "fork-source-xai-abcd1234"
            );
        }
        _ => panic!("Expected xAI update"),
    }
}

#[tokio::test]
async fn copy_session_data_source_not_found() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("nonexistent"),
        cwd: "/nonexistent".to_string(),
    };

    let target_info = Info {
        id: acp::SessionId::new("fork-nonexistent-abcd1234"),
        cwd: "/target".to_string(),
    };

    let result = adapter
        .copy_session_data(&source_info, &target_info, Default::default())
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn copy_session_data_with_model_override() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("source-model-test"),
        cwd: "/source".to_string(),
    };

    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-model-test"),
        cwd: "/target".to_string(),
    };

    let options = CopySessionOptions {
        parent_session_id: Some("source-model-test".to_string()),
        new_model_id: Some("grok-3".to_string()),
        target_prompt_index: None,
        ..Default::default()
    };
    adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();

    let loaded = adapter.load_session(&target_info).await.unwrap();
    assert_eq!(loaded.summary.current_model_id.0.as_ref(), "grok-3");
    assert_eq!(
        loaded.summary.parent_session_id,
        Some("source-model-test".to_string())
    );
}

#[tokio::test]
async fn copy_session_data_skips_tool_state_directory() {
    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());

    let source_info = Info {
        id: acp::SessionId::new("source-dir-tool-state"),
        cwd: "/source/project".to_string(),
    };

    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    adapter
        .append_chat_message(&source_info, &ConversationItem::user("Hello"))
        .await
        .unwrap();

    let source_dir = adapter.session_dir(&source_info);
    std::fs::create_dir_all(source_dir.join("tool_state.json").join("terminal")).unwrap();

    let target_info = Info {
        id: acp::SessionId::new("fork-dir-tool-state"),
        cwd: "/target/worktree".to_string(),
    };

    let result = adapter
        .copy_session_data(&source_info, &target_info, Default::default())
        .await
        .unwrap();

    assert!(!result.tool_state_copied);
    assert!(
        !adapter
            .session_dir(&target_info)
            .join("tool_state.json")
            .is_file()
    );
}

#[tokio::test]
async fn copy_fork_provenance_persisted_in_summary() {
    let tmp = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    let source_info = Info {
        id: acp::SessionId::new("src-prov"),
        cwd: "/src".to_string(),
    };
    let target_info = Info {
        id: acp::SessionId::new("tgt-prov"),
        cwd: "/tgt".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();

    let options = CopySessionOptions {
        parent_session_id: Some("src-prov".to_string()),
        session_kind: Some("subagent_fork".to_string()),
        fork_context_source: Some("forked".to_string()),
        fork_parent_prompt_id: Some("prompt-42".to_string()),
        ..Default::default()
    };
    adapter
        .copy_session_data(&source_info, &target_info, options)
        .await
        .unwrap();

    let data = adapter.load_session(&target_info).await.unwrap();
    assert_eq!(data.summary.session_kind.as_deref(), Some("subagent_fork"));
    assert_eq!(data.summary.fork_context_source.as_deref(), Some("forked"));
    assert_eq!(
        data.summary.fork_parent_prompt_id.as_deref(),
        Some("prompt-42")
    );
}

#[tokio::test]
async fn copy_session_data_inherits_source_summary_fields() {
    let tmp = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    let source_info = Info {
        id: acp::SessionId::new("src-inherit"),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    adapter
        .update_git_head(
            &source_info,
            Some("abc123".into()),
            Some("feature-branch".into()),
        )
        .await
        .unwrap();
    // Set the profile on disk so the assertion is independent of the
    // process-global configured profile.
    let mut src_summary = adapter.read_summary_sync(&source_info).unwrap();
    src_summary.sandbox_profile = Some("workspace".to_string());
    adapter
        .write_summary_sync(&source_info, &src_summary)
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("tgt-inherit"),
        cwd: "/tgt".to_string(),
    };
    adapter
        .copy_session_data(&source_info, &target_info, CopySessionOptions::default())
        .await
        .unwrap();

    let loaded = adapter.load_summary(&target_info).await.unwrap();
    assert_eq!(loaded.head_commit.as_deref(), Some("abc123"));
    assert_eq!(loaded.head_branch.as_deref(), Some("feature-branch"));
    assert_eq!(loaded.sandbox_profile.as_deref(), Some("workspace"));
}

async fn assert_copy_clears_pending_relocation(fork_filter: bool) {
    use crate::session::persistence::PendingCwdSwitchReminder;

    let tmp = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new(format!("pending-source-{fork_filter}")),
        cwd: "/src".into(),
    };
    let target = Info {
        id: acp::SessionId::new(format!("pending-target-{fork_filter}")),
        cwd: "/target".into(),
    };
    let mut summary = adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    summary.cwd_generation = 3;
    summary.previous_cwd = Some("/older".into());
    summary.pending_cwd_switch_reminder = Some(PendingCwdSwitchReminder {
        cwd_generation: 3,
        previous_cwd: "/src".into(),
        destination_cwd: "/destination".into(),
        content: "switch".into(),
        destination_project_instructions: None,
    });
    adapter.write_summary_sync(&source, &summary).unwrap();
    adapter
        .append_chat_message(
            &source,
            &ConversationItem::working_directory_switch("switch", 3),
        )
        .await
        .unwrap();

    adapter
        .copy_session_data(
            &source,
            &target,
            CopySessionOptions {
                fork_filter,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let copied = adapter.read_summary_sync(&target).unwrap();
    assert_eq!(copied.cwd_generation, 3);
    assert_eq!(copied.previous_cwd.as_deref(), Some("/older"));
    assert!(copied.pending_cwd_switch_reminder.is_none());
    let expected_generation = if fork_filter { 0 } else { 3 };
    assert_eq!(
        copied.cwd_switch_bookkeeping_generation,
        expected_generation
    );
    if !fork_filter {
        let before = copied.num_chat_messages;
        assert!(matches!(
            adapter
                .append_cwd_switch_commit_aware(
                    &target,
                    &ConversationItem::working_directory_switch("switch", 3),
                )
                .await
                .unwrap(),
            xai_chat_state::StrictAppendAck::AlreadyPresent(item)
                if item.text_content() == "switch"
        ));
        let retried = adapter.read_summary_sync(&target).unwrap();
        assert_eq!(retried.num_chat_messages, before);
        assert_eq!(
            adapter
                .read_chat_history_sync(adapter.chat_file(&target), CHAT_FORMAT_VERSION)
                .unwrap()
                .iter()
                .filter(|item| item.working_directory_switch_generation() == Some(3))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn unfiltered_copy_clears_pending_relocation() {
    assert_copy_clears_pending_relocation(false).await;
}

#[tokio::test]
async fn filtered_copy_clears_pending_relocation() {
    assert_copy_clears_pending_relocation(true).await;
}

/// Each sidecar flag gates exactly its own file: one fork per flag disables
/// only that flag and asserts only its file is missing, so a transposed flag
/// or path in the `copy_sidecar_file` call sites fails. A defaults fork then
/// proves all five copy with their contents intact.
#[tokio::test]
async fn sidecar_flags_gate_their_files_independently() {
    let tmp = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(tmp.path().to_path_buf());
    let source = Info {
        id: acp::SessionId::new("src-sidecars"),
        cwd: "/src".to_string(),
    };
    adapter
        .init_session(&source, default_model_id())
        .await
        .unwrap();
    std::fs::write(adapter.plan_file(&source), b"plan").unwrap();
    std::fs::write(adapter.signals_file(&source), b"signals").unwrap();
    std::fs::write(adapter.plan_mode_state_file(&source), b"plan-mode").unwrap();
    std::fs::write(
        adapter.session_dir(&source).join("tool_state.json"),
        b"{\"todo\":[]}",
    )
    .unwrap();
    std::fs::write(adapter.announcement_state_file(&source), b"announcements").unwrap();

    type DisableFlag = fn(&mut CopySessionOptions);
    let cases: [(&str, DisableFlag); 5] = [
        ("plan", |o| o.copy_plan_state = false),
        ("signals", |o| o.copy_signals = false),
        ("plan_mode", |o| o.copy_plan_mode_state = false),
        ("tool_state", |o| o.copy_tool_state = false),
        ("announcement", |o| o.copy_announcement_state = false),
    ];
    for (off, (name, disable)) in cases.iter().enumerate() {
        let target = Info {
            id: acp::SessionId::new(format!("tgt-sidecar-off-{name}")),
            cwd: "/tgt".to_string(),
        };
        let mut options = CopySessionOptions::default();
        disable(&mut options);
        let result = adapter
            .copy_session_data(&source, &target, options)
            .await
            .unwrap();
        let copied = [
            result.plan_state_copied,
            result.signals_copied,
            result.plan_mode_state_copied,
            result.tool_state_copied,
            result.announcement_state_copied,
        ];
        let present = [
            adapter.plan_file(&target).exists(),
            adapter.signals_file(&target).exists(),
            adapter.plan_mode_state_file(&target).exists(),
            adapter
                .session_dir(&target)
                .join("tool_state.json")
                .exists(),
            adapter.announcement_state_file(&target).exists(),
        ];
        for (i, (copied, present)) in copied.into_iter().zip(present).enumerate() {
            let expected = i != off;
            assert_eq!(copied, expected, "{name} off: sidecar {i} copied flag");
            assert_eq!(present, expected, "{name} off: sidecar {i} file present");
        }
    }

    let target_on = Info {
        id: acp::SessionId::new("tgt-sidecars-on"),
        cwd: "/tgt".to_string(),
    };
    let result = adapter
        .copy_session_data(&source, &target_on, CopySessionOptions::default())
        .await
        .unwrap();
    assert!(result.plan_state_copied);
    assert!(result.signals_copied);
    assert!(result.plan_mode_state_copied);
    assert!(result.tool_state_copied);
    assert!(result.announcement_state_copied);
    assert_eq!(
        std::fs::read(adapter.plan_file(&target_on)).unwrap(),
        b"plan"
    );
    assert_eq!(
        std::fs::read(adapter.signals_file(&target_on)).unwrap(),
        b"signals"
    );
    assert_eq!(
        std::fs::read(adapter.plan_mode_state_file(&target_on)).unwrap(),
        b"plan-mode"
    );
    assert_eq!(
        std::fs::read(adapter.session_dir(&target_on).join("tool_state.json")).unwrap(),
        b"{\"todo\":[]}"
    );
    assert_eq!(
        std::fs::read(adapter.announcement_state_file(&target_on)).unwrap(),
        b"announcements"
    );
}

/// Boundary matrix for the capped line reader: exactly-cap content is kept,
/// cap-plus-one is discarded without consuming an index (so the two copy
/// passes stay aligned), a drain spanning several read chunks terminates, and
/// an unterminated within-cap tail is kept.
#[test]
fn capped_line_reader_discards_overlong_lines_without_shifting_indexes() {
    fn collect(input: &[u8], cap: usize) -> Vec<(usize, Vec<u8>)> {
        let mut seen = Vec::new();
        super::for_each_jsonl_line_capped(std::io::Cursor::new(input), cap, |index, line| {
            seen.push((index, line.to_vec()));
            Ok(std::ops::ControlFlow::Continue(()))
        })
        .unwrap();
        seen
    }

    // Exactly cap content bytes: kept.
    assert_eq!(collect(b"abcd\n", 4), vec![(0, b"abcd".to_vec())]);
    // One over cap: discarded; the next line takes the next index, not a
    // shifted one.
    assert_eq!(
        collect(b"aa\nxxxxx\nbb\n", 4),
        vec![(0, b"aa".to_vec()), (1, b"bb".to_vec())]
    );
    // Overlong spanning several drain chunks still finds the line end.
    assert_eq!(
        collect(b"xxxxxxxxxxxxxxxxxxxxx\ncc\n", 4),
        vec![(0, b"cc".to_vec())]
    );
    // Overlong unterminated at EOF: drain hits EOF and stops cleanly.
    assert_eq!(collect(b"aa\nxxxxxxxx", 4), vec![(0, b"aa".to_vec())]);
    // Unterminated within-cap tail is kept, matching the uncapped reader.
    assert_eq!(
        collect(b"aa\nbb", 4),
        vec![(0, b"aa".to_vec()), (1, b"bb".to_vec())]
    );
}

/// `summary.json` carries authored display text, never a generated pointer:
/// `session_summary` is the session title (an LLM one, a `/rename`, or the
/// first ten words of the user's own opening message via
/// `title_fallback_from_user_text`) and `last_turn_summary` is the model's
/// per-turn dashboard one-liner. No writer of either produces a
/// `transcript_hint` -- `build_compacted_history` is the only thing that
/// appends one, and it never reaches a `Summary`. A fork must therefore copy
/// both verbatim, including when the text quotes the parent's archive or
/// reads exactly like a generated hint (issue #423).
#[tokio::test]
async fn issue423_fork_preserves_authored_summary_text_verbatim() {
    use crate::extensions::notification::CompactionSegmentFile;
    use xai_chat_state::{CompactionDetail, CompactionMode};

    let temp_dir = TempDir::new().unwrap();
    let adapter = JsonlStorageAdapter::with_root(temp_dir.path().to_path_buf());
    let source_info = Info {
        id: acp::SessionId::new("issue423-parent"),
        cwd: "/source/workspace".to_string(),
    };
    adapter
        .init_session(&source_info, default_model_id())
        .await
        .unwrap();
    // Fork a genuinely compacted session: that is when the deleted rewrite
    // had a source and target dir to substitute between.
    adapter
        .write_compaction_segment(
            &source_info,
            &CompactionSegmentFile {
                items: vec![ConversationItem::user("pre-compact turn")],
                summary: "segment for issue423".to_string(),
                detail: CompactionDetail::Verbose,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            },
        )
        .await
        .unwrap();

    let parent_loc = adapter
        .session_dir(&source_info)
        .join(xai_compaction_transcript::COMPACTION_DIR)
        .to_string_lossy()
        .into_owned();

    // A title quoting the parent archive one component deep: the deleted
    // rewrite stopped only at a component *continuation*, so a trailing `/`
    // did not protect this.
    let title = format!("Why {parent_loc}/segment_000.md keeps growing");
    // A per-turn summary worded as the generated hint itself -- the exact
    // text the deleted rewrite was hunting for, here authored by the model.
    let last_turn = CompactionMode::Segments(CompactionDetail::Verbose)
        .transcript_hint(Some(parent_loc.as_str()))
        .expect("segments hint needs a location");

    let mut source_summary = adapter.read_summary_sync(&source_info).unwrap();
    source_summary.session_summary = title.clone();
    source_summary.last_turn_summary = Some(last_turn.clone());
    adapter
        .write_summary_sync(&source_info, &source_summary)
        .unwrap();

    let target_info = Info {
        id: acp::SessionId::new("issue423-child"),
        cwd: "/target/workspace".to_string(),
    };
    adapter
        .copy_session_data_sync(
            &source_info,
            &target_info,
            CopySessionOptions {
                parent_session_id: Some(source_info.id.to_string()),
                copy_compaction_segments: true,
                ..Default::default()
            },
        )
        .unwrap();

    let child_loc = adapter
        .session_dir(&target_info)
        .join(xai_compaction_transcript::COMPACTION_DIR)
        .to_string_lossy()
        .into_owned();
    assert_ne!(
        parent_loc, child_loc,
        "fork must land in a new session dir, or this pins nothing"
    );

    let copied = adapter.read_summary_sync(&target_info).unwrap();
    assert_eq!(
        copied.session_summary, title,
        "fork must copy the session title verbatim"
    );
    assert_eq!(
        copied.last_turn_summary.as_deref(),
        Some(last_turn.as_str()),
        "fork must copy the per-turn summary verbatim"
    );
    // Total guard: no field of the copied summary was retargeted.
    let raw = std::fs::read_to_string(adapter.summary_file(&target_info)).unwrap();
    assert!(
        !raw.contains(&child_loc),
        "fork must not retarget any summary.json text, got {raw}"
    );
}
