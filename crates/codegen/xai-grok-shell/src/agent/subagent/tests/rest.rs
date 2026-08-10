#![cfg_attr(rustfmt, rustfmt::skip)]
use super::*;
use crate::test_support::lsp_runtime::{ctx_with_toggle, test_gateway};
use crate::upload::trace::SubagentSpawnedRef;
use xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend;

fn test_catalog_identity(
    model_id: &str,
    route: &str,
    lineage: xai_chat_state::CatalogResolutionLineage,
) -> xai_chat_state::CatalogIdentity {
    xai_chat_state::CatalogIdentity {
        model_id: model_id.to_string(),
        route: route.to_string(),
        lineage,
        auth_scheme: Some(xai_chat_state::CatalogAuthScheme::Bearer),
    }
}
#[test]
fn normalize_forked_context_strips_project_layout() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let big_layout = "<project_layout>\nline1\nline2\nline3\n</project_layout>";
    let items = vec![
            ConversationItem::system("sys"),
            ConversationItem::user(big_layout),
            ConversationItem::assistant("ack"),
        ];
    let (conv, _) = xai_grok_subagent_resolution::context::normalize_forked_context(
        items,
    );
    if let ConversationItem::User(u) = &conv[1] {
        let text = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect::<String>();
        assert!(
                !text.contains("<project_layout>"),
                "project_layout tag should be stripped"
            );
        assert!(!text.contains("line1"), "layout content should be removed");
    } else {
        panic!("expected User at position 1");
    }
}
#[test]
fn normalize_forked_context_consecutive_users() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let items = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("prefix"),
            ConversationItem::user("query"),
            ConversationItem::assistant("response"),
        ];
    let (conv, prefix_len) = xai_grok_subagent_resolution::context::normalize_forked_context(
        items,
    );
    assert_eq!(prefix_len, 2);
    if let ConversationItem::User(u) = &conv[1] {
        let text = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect::<String>();
        assert!(
                text.contains("[User]: prefix"),
                "should include first user msg"
            );
        assert!(
                text.contains("[User]: query"),
                "should include second user msg"
            );
        assert!(
                text.contains("[Assistant]: response"),
                "should include assistant"
            );
    } else {
        panic!("expected User at position 1");
    }
}
/// End-to-end test: after normalization + system prompt replacement,
/// the conversation shape is [System(child's), BackgroundContext].
/// Then the Prompt command appends the task as [2], giving:
/// [System(child's), BackgroundContext, Task].
#[test]
fn end_to_end_normalized_conversation_shape() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let parent_conv = vec![
            ConversationItem::system("parent system prompt"),
            ConversationItem::user("user prefix with project info"),
            ConversationItem::user("implement quicksort"),
            ConversationItem::assistant("here is quicksort"),
        ];
    let (mut conv, prefix_len) = xai_grok_subagent_resolution::context::normalize_forked_context(
        parent_conv,
    );
    assert_eq!(prefix_len, 2);
    assert_eq!(conv.len(), 2);
    if let ConversationItem::System(ref mut sys) = conv[0] {
        sys.content = "child system prompt with tool guidance".into();
    } else {
        panic!("expected System at position 0");
    }
    if let ConversationItem::System(ref sys) = conv[0] {
        assert_eq!(
                sys.content.as_ref(),
                "child system prompt with tool guidance"
            );
    }
    if let ConversationItem::User(ref u) = conv[1] {
        let text = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("<background_context>"));
        assert!(text.contains("[User]: implement quicksort"));
    } else {
        panic!("expected User (background) at position 1");
    }
    let task = "implement bubble sort in Rust";
    conv.push(ConversationItem::user(task));
    assert_eq!(conv.len(), 3);
    assert!(matches!(conv[0], ConversationItem::System(_)));
    assert!(matches!(conv[1], ConversationItem::User(_)));
    assert!(matches!(conv[2], ConversationItem::User(_)));
    if let ConversationItem::User(ref u) = conv[2] {
        let text = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, task, "last user message should be the task");
    }
    assert_eq!(prefix_len, 2);
    assert!(prefix_len < conv.len(), "prefix should not cover the task");
}
/// Verify that the task prompt (not background context) would be the
/// cached prompt text in the session pipeline.
#[test]
fn cached_prompt_text_is_task_not_background() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let parent_conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("parent query"),
            ConversationItem::assistant("parent answer"),
        ];
    let (conv, _) = xai_grok_subagent_resolution::context::normalize_forked_context(
        parent_conv,
    );
    let background_text = if let ConversationItem::User(ref u) = conv[1] {
        u.content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect::<String>()
    } else {
        String::new()
    };
    let task_prompt = "fix the failing test in src/lib.rs";
    assert_ne!(task_prompt, background_text.trim());
    assert!(
            !background_text.contains(task_prompt),
            "background should not contain the task prompt"
        );
    assert!(
            background_text.contains("<background_context>"),
            "background should be the inherited context"
        );
}
/// Verify extract_last_real_user_query would return the task.
#[test]
fn last_user_message_is_task_after_normalization() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let parent_conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("parent context"),
            ConversationItem::assistant("ack"),
        ];
    let (mut conv, _) = xai_grok_subagent_resolution::context::normalize_forked_context(
        parent_conv,
    );
    let task = "deploy the service to staging";
    conv.push(ConversationItem::user(task));
    let last_user = conv
        .iter()
        .rev()
        .find_map(|item| {
            if let ConversationItem::User(u) = item {
                let text: String = u
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        xai_grok_sampling_types::conversation::ContentPart::Text {
                            text,
                        } => Some(text.as_ref()),
                        _ => None,
                    })
                    .collect();
                Some(text)
            } else {
                None
            }
        });
    assert_eq!(
            last_user.as_deref(),
            Some(task),
            "last user message should be the task, not background context"
        );
}
/// Simulate compaction preserving the inherited prefix.
/// The compactor produces [System, UserPrefix, Summary, ...]. The prefix
/// preservation logic takes [System, BackgroundContext] from the original
/// conversation and skips the compacted System, resulting in:
/// [System(inherited), BackgroundContext(inherited), UserPrefix(compacted), Summary, ...]
#[test]
fn compaction_preserves_inherited_prefix() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let parent_conv = vec![
            ConversationItem::system("parent sys"),
            ConversationItem::user("parent question"),
            ConversationItem::assistant("parent answer"),
        ];
    let (conv, prefix_len) = xai_grok_subagent_resolution::context::normalize_forked_context(
        parent_conv,
    );
    assert_eq!(prefix_len, 2);
    let mut full_conv = conv;
    if let ConversationItem::System(ref mut sys) = full_conv[0] {
        sys.content = "child system prompt".into();
    }
    full_conv.push(ConversationItem::user("do the thing"));
    full_conv.push(ConversationItem::assistant("done"));
    let compacted_history = vec![
            ConversationItem::system("fresh system prompt after compaction"),
            ConversationItem::user("user prefix"),
            ConversationItem::user("<compacted_summary>summary of work</compacted_summary>"),
        ];
    let inherited: Vec<_> = full_conv[..prefix_len].to_vec();
    let child_items: Vec<_> = compacted_history
        .into_iter()
        .skip_while(|i| matches!(i, ConversationItem::System(_)))
        .collect();
    let mut preserved = inherited;
    preserved.extend(child_items);
    assert_eq!(preserved.len(), 4);
    if let ConversationItem::System(ref sys) = preserved[0] {
        assert_eq!(sys.content.as_ref(), "child system prompt");
    } else {
        panic!("expected System at [0]");
    }
    if let ConversationItem::User(ref u) = preserved[1] {
        let text: String = u
            .content
            .iter()
            .filter_map(|p| match p {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
                text.contains("<background_context>"),
                "background context should be preserved across compaction"
            );
    } else {
        panic!("expected BackgroundContext User at [1]");
    }
    let system_count = preserved
        .iter()
        .filter(|i| matches!(i, ConversationItem::System(_)))
        .count();
    assert_eq!(
            system_count, 1,
            "should have exactly one System after compaction"
        );
    let bg_count = preserved
        .iter()
        .filter(|i| {
            if let ConversationItem::User(u) = i {
                u.content
                    .iter()
                    .any(|p| {
                        matches!(
                    p,
                    xai_grok_sampling_types::conversation::ContentPart::Text { text } if text.contains("<background_context>")
                )
                    })
            } else {
                false
            }
        })
        .count();
    assert_eq!(
            bg_count, 1,
            "should have exactly one background_context after compaction"
        );
}
/// Verify that compaction with prefix_len=0 (non-forked) passes through unchanged.
#[test]
fn compaction_no_prefix_passes_through() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let compacted = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("summary"),
        ];
    let prefix_len: usize = 0;
    let result = if prefix_len > 0 { unreachable!() } else { compacted.clone() };
    assert_eq!(result.len(), 2);
    assert!(matches!(result[0], ConversationItem::System(_)));
}
#[test]
fn resumed_from_field_in_meta_roundtrips() {
    let meta = SubagentMeta {
        subagent_id: "sa-resumed".into(),
        parent_session_id: "parent".into(),
        child_session_id: "child".into(),
        subagent_type: "general-purpose".into(),
        description: "resumed task".into(),
        prompt: "continue".into(),
        status: "running".into(),
        started_at: chrono::Utc::now(),
        completed_at: None,
        duration_ms: None,
        tool_calls: None,
        turns: None,
        error: None,
        effective_context_source: None,
        context_normalized: false,
        fork_copy_error: None,
        persona: None,
        resumed_from: Some("prev-subagent-id".into()),
        child_cwd: None,
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: None,
    };
    let json = serde_json::to_string(&meta).unwrap();
    assert!(json.contains("resumed_from"));
    assert!(json.contains("prev-subagent-id"));
    let parsed: SubagentMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.resumed_from.as_deref(), Some("prev-subagent-id"));
    let gcs = SubagentSessionMetadata::from_meta(
        &meta,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        0,
    );
    assert_eq!(gcs.resumed_from.as_deref(), Some("prev-subagent-id"));
    let gcs_json = serde_json::to_string(&gcs).unwrap();
    assert!(gcs_json.contains("resumedFrom"));
}
#[test]
fn resumed_from_none_not_serialized_in_meta() {
    let meta = SubagentMeta {
        subagent_id: "sa-fresh".into(),
        parent_session_id: "p".into(),
        child_session_id: "c".into(),
        subagent_type: "explore".into(),
        description: "d".into(),
        prompt: "p".into(),
        status: "running".into(),
        started_at: chrono::Utc::now(),
        completed_at: None,
        duration_ms: None,
        tool_calls: None,
        turns: None,
        error: None,
        effective_context_source: None,
        context_normalized: false,
        fork_copy_error: None,
        persona: None,
        resumed_from: None,
        child_cwd: None,
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: None,
    };
    let json = serde_json::to_string(&meta).unwrap();
    assert!(
            !json.contains("resumed_from"),
            "None resumed_from should be omitted"
        );
}
#[test]
fn backward_compat_meta_without_resumed_from() {
    let json = r#"{
            "subagent_id": "sa1",
            "parent_session_id": "p1",
            "child_session_id": "c1",
            "subagent_type": "explore",
            "description": "d",
            "prompt": "p",
            "status": "completed",
            "started_at": "2026-01-01T00:00:00Z"
        }"#;
    let meta: SubagentMeta = serde_json::from_str(json).unwrap();
    assert!(meta.resumed_from.is_none());
}
#[test]
fn snapshot_ref_field_in_meta_roundtrips() {
    let meta = SubagentMeta {
        subagent_id: "sa-snap".into(),
        parent_session_id: "parent".into(),
        child_session_id: "child".into(),
        subagent_type: "general-purpose".into(),
        description: "snapshot task".into(),
        prompt: "do work".into(),
        status: "completed".into(),
        started_at: chrono::Utc::now(),
        completed_at: Some(chrono::Utc::now()),
        duration_ms: Some(10),
        tool_calls: Some(1),
        turns: Some(1),
        error: None,
        effective_context_source: None,
        context_normalized: false,
        fork_copy_error: None,
        persona: None,
        resumed_from: None,
        child_cwd: None,
        worktree_path: Some("/tmp/grok-wt/sa-snap".into()),
        snapshot_ref: Some("refs/grok/subagent-snapshots/sa-snap".into()),
        effective_model_id: None,
    };
    let json = serde_json::to_string(&meta).unwrap();
    assert!(json.contains("snapshot_ref"));
    assert!(json.contains("refs/grok/subagent-snapshots/sa-snap"));
    let parsed: SubagentMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(
            parsed.snapshot_ref.as_deref(),
            Some("refs/grok/subagent-snapshots/sa-snap")
        );
}
#[test]
fn backward_compat_meta_without_snapshot_ref() {
    let json = r#"{
            "subagent_id": "sa1",
            "parent_session_id": "p1",
            "child_session_id": "c1",
            "subagent_type": "explore",
            "description": "d",
            "prompt": "p",
            "status": "completed",
            "started_at": "2026-01-01T00:00:00Z"
        }"#;
    let meta: SubagentMeta = serde_json::from_str(json).unwrap();
    assert!(meta.snapshot_ref.is_none());
}
/// Minimal completed-status meta for the snapshot-ref persistence tests.
fn snapshot_test_meta(id: &str) -> SubagentMeta {
    SubagentMeta {
        subagent_id: id.into(),
        parent_session_id: "session-A".into(),
        child_session_id: format!("child-{id}"),
        subagent_type: "general-purpose".into(),
        description: "task".into(),
        prompt: "do work".into(),
        status: "completed".into(),
        started_at: chrono::Utc::now(),
        completed_at: Some(chrono::Utc::now()),
        duration_ms: Some(1),
        tool_calls: Some(0),
        turns: Some(1),
        error: None,
        effective_context_source: None,
        context_normalized: false,
        fork_copy_error: None,
        persona: None,
        resumed_from: None,
        child_cwd: None,
        worktree_path: Some("/tmp/grok-wt/subagent-x".into()),
        snapshot_ref: None,
        effective_model_id: None,
    }
}
/// The follow-up writer persists `snapshot_ref` into an already-finalized
/// meta.json so `durable_resume_source_for` rehydrates the disposed worktree.
#[test]
fn update_subagent_meta_snapshot_ref_persists_to_disk() {
    let dir = tempfile::TempDir::new().unwrap();
    assert!(write_subagent_meta(
            dir.path(),
            &snapshot_test_meta("sa-write")
        ));
    assert!(
            update_subagent_meta_snapshot_ref(
                dir.path(),
                "refs/grok/subagents/sa-write",
                "completed"
            ),
            "persisting the ref into an existing meta.json must report success"
        );
    let data = std::fs::read_to_string(dir.path().join("meta.json")).unwrap();
    let reread: SubagentMeta = serde_json::from_str(&data).unwrap();
    assert_eq!(
            reread.snapshot_ref.as_deref(),
            Some("refs/grok/subagents/sa-write")
        );
    assert_eq!(reread.status, "completed");
    assert_eq!(
            reread.worktree_path.as_deref(),
            Some("/tmp/grok-wt/subagent-x")
        );
}
/// Missing meta.json → the writer reports failure (it `warn!`s), so the
/// completion path keeps the worktree instead of removing it ref-less.
#[test]
fn update_subagent_meta_snapshot_ref_reports_failure_when_meta_missing() {
    let dir = tempfile::TempDir::new().unwrap();
    assert!(!update_subagent_meta_snapshot_ref(
            dir.path(),
            "refs/grok/subagents/sa-missing",
            "completed"
        ));
}
/// A stale non-terminal record (e.g. completed-status write failed) is
/// promoted to terminal alongside the snapshot_ref, so the durable resume
/// fallback accepts it after the worktree is removed.
#[test]
fn snapshot_ref_write_promotes_nonterminal_status_to_terminal() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut meta = snapshot_test_meta("sa-promote");
    meta.status = "running".into();
    assert!(write_subagent_meta(dir.path(), &meta));
    assert!(update_subagent_meta_snapshot_ref(
            dir.path(),
            "refs/grok/subagents/x",
            "completed"
        ));
    let data = std::fs::read_to_string(dir.path().join("meta.json")).unwrap();
    let reread: SubagentMeta = serde_json::from_str(&data).unwrap();
    assert_eq!(
            Some("refs/grok/subagents/x"),
            reread.snapshot_ref.as_deref()
        );
    assert_eq!("completed", reread.status);
}
/// Gate defaults OFF: no config, no remote → snapshotting disabled, so the
/// completion path keeps the worktree preserved (no production change).
#[test]
fn subagent_worktree_snapshot_gate_defaults_off() {
    let ctx = ctx_with_toggle(std::collections::HashMap::new());
    assert!(!ctx.resolve_subagent_worktree_snapshot_enabled());
}
/// Remote remote settings value enables the gate when no local override exists.
#[test]
fn subagent_worktree_snapshot_gate_remote_enables() {
    let mut ctx = ctx_with_toggle(std::collections::HashMap::new());
    ctx.remote_settings = Some(crate::util::config::RemoteSettings {
        subagent_worktree_snapshot_enabled: Some(true),
        ..Default::default()
    });
    assert!(ctx.resolve_subagent_worktree_snapshot_enabled());
}
/// Local config wins over remote (kill-switch parity with the other gates).
#[test]
fn subagent_worktree_snapshot_gate_local_overrides_remote() {
    let mut config = crate::agent::config::Config::default();
    config.features.subagent_worktree_snapshot = Some(false);
    let mut ctx = ctx_with_toggle(std::collections::HashMap::new());
    ctx.agent_config = Some(config);
    ctx.remote_settings = Some(crate::util::config::RemoteSettings {
        subagent_worktree_snapshot_enabled: Some(true),
        ..Default::default()
    });
    assert!(
            !ctx.resolve_subagent_worktree_snapshot_enabled(),
            "local [features] subagent_worktree_snapshot=false must override remote enable"
        );
}
/// Local config alone enables the gate (the per-deployment rollout lever).
#[test]
fn subagent_worktree_snapshot_gate_local_enables() {
    let mut config = crate::agent::config::Config::default();
    config.features.subagent_worktree_snapshot = Some(true);
    let mut ctx = ctx_with_toggle(std::collections::HashMap::new());
    ctx.agent_config = Some(config);
    assert!(ctx.resolve_subagent_worktree_snapshot_enabled());
}
/// Subagent spawns carry concrete ask_user_question timeout params (the
/// session-level config follows the child) while bash stays on tool
/// defaults. Tier precedence itself is pinned by the resolver's own
/// tests; asserting concrete values here would read the host's disk
/// layers and flake on configured dev machines.
#[test]
fn subagent_tool_params_carry_ask_user_question_timeouts() {
    let ctx = ctx_with_toggle(std::collections::HashMap::new());
    let params = ctx.resolve_tool_params_json();
    assert!(params.bash.is_none(), "bash must stay on tool defaults");
    let ask = params
        .ask_user_question
        .expect("subagents must receive resolved ask_user_question params");
    assert!(ask.get("timeout_enabled").is_some_and(|v| v.is_boolean()));
    assert!(ask.get("timeout_secs").is_some_and(|v| v.is_u64()));
}
/// End-to-end glue: gate ON + a worktree present runs the completion
/// sequence (snapshot → persist ref to meta.json → remove) and verifies the
/// durable shell resume fallback sees the ref after removal.
#[tokio::test]
async fn completion_snapshot_sequence_persists_ref_then_removes_worktree() {
    xai_test_utils::require_git!();
    use xai_test_utils::git::{git_commit_all, init_git_repo};
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_git_repo(&repo);
    std::fs::write(repo.join("tracked.txt"), "original").unwrap();
    git_commit_all(&repo, "initial");
    let wt = temp.path().join("subagent-glue-1");
    xai_fast_worktree::WorktreeBuilder::new(&repo, &wt)
        .standalone(true)
        .create()
        .unwrap();
    std::fs::write(wt.join("tracked.txt"), "edited").unwrap();
    let mut config = crate::agent::config::Config::default();
    config.features.subagent_worktree_snapshot = Some(true);
    let mut ctx = ctx_with_toggle(std::collections::HashMap::new());
    ctx.agent_config = Some(config);
    assert!(ctx.resolve_subagent_worktree_snapshot_enabled());
    let meta_dir = temp.path().join("meta");
    write_subagent_meta(&meta_dir, &snapshot_test_meta("glue-1"));
    let ref_name = "refs/grok/subagents/glue-1";
    let snapshot_ref = crate::session::worktree::snapshot_subagent_worktree(
            &wt,
            &repo,
            ref_name,
        )
        .await
        .unwrap();
    assert!(update_subagent_meta_snapshot_ref(
            &meta_dir,
            &snapshot_ref,
            "completed"
        ));
    crate::session::worktree::remove_subagent_worktree(&wt).await.unwrap();
    let data = std::fs::read_to_string(meta_dir.join("meta.json")).unwrap();
    let reread: SubagentMeta = serde_json::from_str(&data).unwrap();
    assert_eq!(reread.snapshot_ref.as_deref(), Some(ref_name));
    assert!(
            !wt.exists(),
            "worktree dir should be removed after the sequence"
        );
}
#[test]
fn subagent_session_metadata_roundtrip() {
    let meta = SubagentMeta {
        subagent_id: "sa-1".into(),
        parent_session_id: "parent-1".into(),
        child_session_id: "child-1".into(),
        subagent_type: "general-purpose".into(),
        description: "test task".into(),
        prompt: "do something".into(),
        status: "completed".into(),
        started_at: chrono::Utc::now(),
        completed_at: Some(chrono::Utc::now()),
        duration_ms: Some(1234),
        tool_calls: Some(5),
        turns: Some(2),
        error: None,
        effective_context_source: Some("new".into()),
        context_normalized: false,
        fork_copy_error: None,
        persona: Some("reviewer".into()),
        resumed_from: None,
        child_cwd: None,
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: None,
    };
    let session_meta = SubagentSessionMetadata::from_meta(
        &meta,
        Some("grok-4.5"),
        Some("/workspace"),
        Some("/tmp/worktree"),
        Some("worktree"),
        Some("read-only"),
        Some("medium"),
        Some("rust-dev"),
        Some("prompt-123"),
        1,
    );
    assert_eq!(session_meta.schema_version, 1);
    assert_eq!(session_meta.session_kind, "subagent");
    assert_eq!(session_meta.subagent_id, "sa-1");
    assert_eq!(session_meta.parent_session_id, "parent-1");
    assert_eq!(session_meta.description, "test task");
    assert_eq!(session_meta.model_id.as_deref(), Some("grok-4.5"));
    assert_eq!(session_meta.role.as_deref(), Some("rust-dev"));
    assert_eq!(session_meta.persona.as_deref(), Some("reviewer"));
    assert!(!session_meta.context_normalized);
    assert_eq!(session_meta.depth, 1);
    let json = serde_json::to_string_pretty(&session_meta).unwrap();
    let deserialized: SubagentSessionMetadata = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.session_kind, "subagent");
    assert_eq!(deserialized.subagent_id, "sa-1");
    assert_eq!(deserialized.description, "test task");
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
    value.as_object_mut().unwrap().remove("description");
    let legacy: SubagentSessionMetadata = serde_json::from_value(value).unwrap();
    assert!(legacy.description.is_empty());
    assert!(json.contains("schemaVersion"));
    assert!(json.contains("sessionKind"));
}
#[test]
fn subagent_session_metadata_non_forked() {
    let meta = SubagentMeta {
        subagent_id: "sa-2".into(),
        parent_session_id: "parent-2".into(),
        child_session_id: "child-2".into(),
        subagent_type: "explore".into(),
        description: "search code".into(),
        prompt: "find auth".into(),
        status: "running".into(),
        started_at: chrono::Utc::now(),
        completed_at: None,
        duration_ms: None,
        tool_calls: None,
        turns: None,
        error: None,
        effective_context_source: Some("new".into()),
        context_normalized: false,
        fork_copy_error: None,
        persona: Some("implementer".into()),
        resumed_from: None,
        child_cwd: None,
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: None,
    };
    let session_meta = SubagentSessionMetadata::from_meta(
        &meta,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        0,
    );
    assert_eq!(session_meta.session_kind, "subagent");
    assert!(!session_meta.context_normalized);
    assert_eq!(session_meta.depth, 0);
    assert!(session_meta.model_id.is_none());
    assert!(session_meta.worktree_path.is_none());
}
#[test]
fn subagent_session_metadata_backward_compat_deserialization() {
    let json = r#"{
            "schemaVersion": 1,
            "sessionId": "s1",
            "sessionKind": "subagent",
            "subagentId": "sa1",
            "childSessionId": "c1",
            "parentSessionId": "p1",
            "subagentType": "explore",
            "startedAt": "2026-01-01T00:00:00Z",
            "status": "completed",
            "depth": 0
        }"#;
    let meta: SubagentSessionMetadata = serde_json::from_str(json).unwrap();
    assert_eq!(meta.session_kind, "subagent");
    assert!(meta.persona.is_none());
    assert!(meta.role.is_none());
    assert!(!meta.context_normalized);
}
#[test]
fn upload_lifecycle_spawn_then_completion_preserves_fields() {
    let spawn_meta = SubagentMeta {
        subagent_id: "sa-lifecycle".into(),
        parent_session_id: "parent-1".into(),
        child_session_id: "child-1".into(),
        subagent_type: "general-purpose".into(),
        description: "test task".into(),
        prompt: "do something".into(),
        status: "running".to_string(),
        started_at: chrono::Utc::now(),
        completed_at: None,
        duration_ms: None,
        tool_calls: None,
        turns: None,
        error: None,
        effective_context_source: Some("forked".into()),
        context_normalized: true,
        fork_copy_error: None,
        persona: Some("implementer".into()),
        resumed_from: None,
        child_cwd: None,
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: None,
    };
    let spawn_gcs = SubagentSessionMetadata::from_meta(
        &spawn_meta,
        Some("grok-4.5"),
        Some("/workspace"),
        None,
        Some("worktree"),
        Some("all"),
        Some("medium"),
        Some("rust-dev"),
        Some("prompt-42"),
        1,
    );
    assert_eq!(spawn_gcs.status, "running");
    assert!(spawn_gcs.completed_at.is_none());
    assert!(spawn_gcs.duration_ms.is_none());
    assert_eq!(spawn_gcs.model_id.as_deref(), Some("grok-4.5"));
    assert_eq!(spawn_gcs.cwd.as_deref(), Some("/workspace"));
    assert_eq!(spawn_gcs.role.as_deref(), Some("rust-dev"));
    assert_eq!(spawn_gcs.parent_prompt_id.as_deref(), Some("prompt-42"));
    assert_eq!(spawn_gcs.depth, 1);
    let mut completed_meta = spawn_meta.clone();
    completed_meta.status = "completed".to_string();
    completed_meta.completed_at = Some(chrono::Utc::now());
    completed_meta.duration_ms = Some(5000);
    completed_meta.tool_calls = Some(12);
    completed_meta.turns = Some(3);
    let completion_gcs = SubagentSessionMetadata::from_meta(
        &completed_meta,
        Some("grok-4.5"),
        Some("/workspace"),
        Some("/tmp/worktree-1"),
        Some("worktree"),
        Some("all"),
        Some("medium"),
        Some("rust-dev"),
        Some("prompt-42"),
        1,
    );
    assert_eq!(completion_gcs.status, "completed");
    assert!(completion_gcs.completed_at.is_some());
    assert_eq!(completion_gcs.duration_ms, Some(5000));
    assert_eq!(completion_gcs.tool_calls, Some(12));
    assert_eq!(completion_gcs.turns, Some(3));
    assert_eq!(completion_gcs.model_id.as_deref(), Some("grok-4.5"));
    assert_eq!(completion_gcs.cwd.as_deref(), Some("/workspace"));
    assert_eq!(completion_gcs.role.as_deref(), Some("rust-dev"));
    assert_eq!(
            completion_gcs.parent_prompt_id.as_deref(),
            Some("prompt-42")
        );
    assert_eq!(
            completion_gcs.worktree_path.as_deref(),
            Some("/tmp/worktree-1")
        );
    assert_eq!(completion_gcs.depth, 1);
    assert_eq!(spawn_gcs.child_session_id, completion_gcs.child_session_id);
}
#[test]
fn upload_lifecycle_failure_preserves_error() {
    let meta = SubagentMeta {
        subagent_id: "sa-fail".into(),
        parent_session_id: "p".into(),
        child_session_id: "c".into(),
        subagent_type: "explore".into(),
        description: "d".into(),
        prompt: "p".into(),
        status: "failed".to_string(),
        started_at: chrono::Utc::now(),
        completed_at: Some(chrono::Utc::now()),
        duration_ms: Some(100),
        tool_calls: Some(0),
        turns: Some(0),
        error: Some("session spawn error".into()),
        effective_context_source: Some("new".into()),
        context_normalized: false,
        fork_copy_error: None,
        persona: None,
        resumed_from: None,
        child_cwd: None,
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: None,
    };
    let gcs = SubagentSessionMetadata::from_meta(
        &meta,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        0,
    );
    assert_eq!(gcs.status, "failed");
    assert_eq!(gcs.error.as_deref(), Some("session spawn error"));
    assert_eq!(gcs.session_kind, "subagent");
}
#[test]
fn initial_context_source_resumed_variant() {
    let source = InitialContextSource::Resumed;
    assert!(matches!(source, InitialContextSource::Resumed));
    assert_ne!(source, InitialContextSource::New);
}
#[test]
fn session_metadata_session_kind_for_resumed() {
    let meta = SubagentMeta {
        subagent_id: "sa-resume".into(),
        parent_session_id: "p".into(),
        child_session_id: "c".into(),
        subagent_type: "general-purpose".into(),
        description: "d".into(),
        prompt: "p".into(),
        status: "running".into(),
        started_at: chrono::Utc::now(),
        completed_at: None,
        duration_ms: None,
        tool_calls: None,
        turns: None,
        error: None,
        effective_context_source: Some("resumed".into()),
        context_normalized: false,
        fork_copy_error: None,
        persona: None,
        resumed_from: Some("prev-id".into()),
        child_cwd: None,
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: None,
    };
    let gcs = SubagentSessionMetadata::from_meta(
        &meta,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        0,
    );
    assert_eq!(
            gcs.session_kind, "subagent_resume",
            "resumed subagents should have session_kind=subagent_resume"
        );
    assert_eq!(gcs.resumed_from.as_deref(), Some("prev-id"));
}
/// Resume must preserve only the System head (`Some(1)`) while passing the full
/// transcript through intact — a whole-transcript prefix is what pinned compaction.
#[test]
fn resume_initial_context_preserves_head_only() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let mut conversation = vec![ConversationItem::system("sys")];
    for i in 0..8 {
        conversation.push(ConversationItem::user(format!("u{i}")));
        conversation.push(ConversationItem::assistant(format!("a{i}")));
    }
    let original_len = conversation.len();
    let ctx = resume_initial_context(conversation);
    assert_eq!(ctx.source, InitialContextSource::Resumed);
    assert!(ctx.copy_error.is_none());
    assert_eq!(
            ctx.prefix_len,
            Some(1),
            "resume preserves only the System head, not the full transcript"
        );
    assert_eq!(
            ctx.conversation.len(),
            original_len,
            "transcript preserved intact"
        );
}
#[test]
fn resume_prefix_len_is_system_head_only() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let mut conversation = vec![ConversationItem::system("sys")];
    for i in 0..6 {
        conversation.push(ConversationItem::user(format!("u{i}")));
        conversation.push(ConversationItem::assistant(format!("a{i}")));
    }
    assert_eq!(resume_inherited_prefix_len(&conversation), 1);
}
#[test]
fn resume_prefix_len_is_zero_without_system_head() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let conversation = vec![
            ConversationItem::user("task"),
            ConversationItem::assistant("done"),
        ];
    assert_eq!(resume_inherited_prefix_len(&conversation), 0);
}
#[test]
fn resume_prefix_len_counts_consecutive_system_head() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let conversation = vec![
            ConversationItem::system("sys a"),
            ConversationItem::system("sys b"),
            ConversationItem::user("work"),
        ];
    assert_eq!(resume_inherited_prefix_len(&conversation), 2);
}
#[test]
fn resume_source_worktree_reuse() {
    let source_with_worktree = ResumeSourceData {
        subagent_id: "sub-wt".into(),
        child_session_id: "child-wt".into(),
        child_cwd: "/tmp/worktree".into(),
        worktree_path: Some(
            PathBuf::from("/home/user/.grok/worktrees/myrepo/subagent-sub-wt"),
        ),
        snapshot_ref: None,
        subagent_type: "general-purpose".into(),
        persona: None,
        model_id: None,
    };
    let worktree = source_with_worktree.worktree_path.clone();
    assert_eq!(
            worktree.as_deref(),
            Some(Path::new(
                "/home/user/.grok/worktrees/myrepo/subagent-sub-wt",
            )),
            "should reuse source worktree"
        );
    let source_without_worktree = ResumeSourceData {
        subagent_id: "sub-no-wt".into(),
        child_session_id: "child-no-wt".into(),
        child_cwd: "/workspace".into(),
        worktree_path: None,
        snapshot_ref: None,
        subagent_type: "general-purpose".into(),
        persona: None,
        model_id: None,
    };
    assert!(
            source_without_worktree.worktree_path.is_none(),
            "no worktree to reuse"
        );
}
#[test]
fn resolve_child_cwd_uses_override_when_no_worktree() {
    let parent = PathBuf::from("/parent/workspace");
    let result = resolve_child_cwd(None, Some("/target/dir"), &parent);
    assert_eq!(result, PathBuf::from("/target/dir"));
}
#[test]
fn resolve_child_cwd_worktree_takes_precedence_over_override() {
    let parent = PathBuf::from("/parent/workspace");
    let worktree = Path::new("/worktree/path");
    let result = resolve_child_cwd(Some(worktree), Some("/target/dir"), &parent);
    assert_eq!(result, PathBuf::from(worktree));
}
#[test]
fn resolve_child_cwd_falls_back_to_parent_when_no_overrides() {
    let parent = PathBuf::from("/parent/workspace");
    let result = resolve_child_cwd(None, None, &parent);
    assert_eq!(result, parent);
}
#[test]
fn resolve_child_cwd_empty_override_falls_back_to_parent() {
    let parent = PathBuf::from("/parent/workspace");
    let result = resolve_child_cwd(None, Some(""), &parent);
    assert_eq!(result, parent);
}
#[test]
fn resume_inherited_cwd_requires_existing_non_worktree_dir() {
    let dir = tempfile::TempDir::new().unwrap();
    let existing = dir.path().to_string_lossy().into_owned();
    let present = ResumeSourceData {
        subagent_id: "sub-present".into(),
        child_session_id: "child-present".into(),
        child_cwd: existing.clone(),
        worktree_path: None,
        snapshot_ref: None,
        subagent_type: "general-purpose".into(),
        persona: None,
        model_id: None,
    };
    assert_eq!(
            resume_inherited_cwd(Some(&present)),
            Some(existing.as_str())
        );
    let missing = ResumeSourceData {
        child_cwd: "/no/such/dir/grok-missing".into(),
        ..present.clone()
    };
    assert_eq!(resume_inherited_cwd(Some(&missing)), None);
    let worktree_source = ResumeSourceData {
        child_cwd: existing.clone(),
        worktree_path: Some(dir.path().to_path_buf()),
        ..present.clone()
    };
    assert_eq!(resume_inherited_cwd(Some(&worktree_source)), None);
    assert_eq!(resume_inherited_cwd(None), None);
}
#[test]
fn select_override_cwd_resume_never_falls_through_to_request_cwd() {
    let source = ResumeSourceData {
        subagent_id: "sub-wt".into(),
        child_session_id: "child-wt".into(),
        child_cwd: "/tmp/whatever".into(),
        worktree_path: Some(
            PathBuf::from("/home/user/.grok/worktrees/repo/subagent-sub-wt"),
        ),
        snapshot_ref: None,
        subagent_type: "general-purpose".into(),
        persona: None,
        model_id: None,
    };
    assert_eq!(select_override_cwd(Some(&source), Some("/x")), None);
}
#[test]
fn select_override_cwd_fresh_spawn_uses_request_cwd() {
    assert_eq!(select_override_cwd(None, Some("/x")), Some("/x"));
}
#[test]
fn resumed_session_uses_current_runtime_contract() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let mut conversation = [
        ConversationItem::system("old source system prompt"),
        ConversationItem::user("task 1"),
        ConversationItem::assistant("done"),
    ];
    let current_prompt = "freshly rendered current system prompt";
    if let Some(ConversationItem::System(sys)) = conversation.first_mut() {
        sys.content = current_prompt.into();
    }
    match &conversation[0] {
        ConversationItem::System(sys) => {
            assert_eq!(sys.content.as_ref(), current_prompt);
            assert!(!sys.content.contains("old source"));
        }
        _ => panic!("first item should be System"),
    }
    assert_eq!(conversation.len(), 3);
}
#[test]
fn token_estimation_for_window_safety() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let conversation = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("Hello, how are you?"),
            ConversationItem::assistant("I'm doing well, thank you!"),
        ];
    let estimated = xai_chat_state::estimate_conversation_tokens(&conversation);
    assert!(estimated > 0, "should produce non-zero estimate");
    assert!(
            estimated < 100,
            "short conversation should have small token estimate"
        );
    assert_eq!(xai_chat_state::estimate_conversation_tokens(&[]), 0);
}
#[test]
fn token_estimation_accounts_for_images() {
    use xai_grok_sampling_types::conversation::{ContentPart, ConversationItem, UserItem};
    let text_only = vec![ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: "describe this".into(),
            }],
            synthetic_reason: None,
            ..Default::default()
        })];
    let text_tokens = xai_chat_state::estimate_conversation_tokens(&text_only);
    let with_image = vec![ConversationItem::User(UserItem {
            content: vec![
                ContentPart::Text {
                    text: "describe this".into(),
                },
                ContentPart::Image {
                    url: "data:image/png;base64,abc".into(),
                },
            ],
            synthetic_reason: None,
            ..Default::default()
        })];
    let image_tokens = xai_chat_state::estimate_conversation_tokens(&with_image);
    assert_eq!(
            image_tokens,
            text_tokens + 765,
            "one image should add 765 tokens"
        );
    let multi_image = vec![ConversationItem::User(UserItem {
            content: vec![
                ContentPart::Image { url: "img1".into() },
                ContentPart::Image { url: "img2".into() },
                ContentPart::Image { url: "img3".into() },
            ],
            synthetic_reason: None,
            ..Default::default()
        })];
    let multi_tokens = xai_chat_state::estimate_conversation_tokens(&multi_image);
    assert_eq!(multi_tokens, 765 * 3, "three images = 3 * 765 tokens");
}
#[test]
fn durable_fallback_roundtrips_child_cwd_and_worktree() {
    let dir = std::env::temp_dir()
        .join("grok-test-durable-resume")
        .join(uuid::Uuid::now_v7().to_string());
    let _ = std::fs::create_dir_all(&dir);
    let meta = SubagentMeta {
        subagent_id: "sa-dur".into(),
        parent_session_id: "parent-dur".into(),
        child_session_id: "child-dur".into(),
        subagent_type: "general-purpose".into(),
        description: "d".into(),
        prompt: "p".into(),
        status: "completed".into(),
        started_at: chrono::Utc::now(),
        completed_at: Some(chrono::Utc::now()),
        duration_ms: Some(100),
        tool_calls: Some(1),
        turns: Some(1),
        error: None,
        effective_context_source: None,
        context_normalized: false,
        fork_copy_error: None,
        persona: Some("implementer".into()),
        resumed_from: None,
        child_cwd: Some("/workspace/project".into()),
        worktree_path: Some("/tmp/grok-wt/sa-dur".into()),
        snapshot_ref: None,
        effective_model_id: Some("grok-3".into()),
    };
    write_subagent_meta(&dir, &meta);
    let data = std::fs::read_to_string(dir.join("meta.json")).unwrap();
    let loaded: SubagentMeta = serde_json::from_str(&data).unwrap();
    assert_eq!(loaded.child_cwd.as_deref(), Some("/workspace/project"));
    assert_eq!(loaded.worktree_path.as_deref(), Some("/tmp/grok-wt/sa-dur"));
    assert_eq!(loaded.status, "completed");
    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn durable_fallback_rejects_running_status() {
    let dir = std::env::temp_dir()
        .join("grok-test-durable-status")
        .join(uuid::Uuid::now_v7().to_string());
    let parent_dir = dir.join("subagents").join("sa-running");
    let _ = std::fs::create_dir_all(&parent_dir);
    let meta = SubagentMeta {
        subagent_id: "sa-running".into(),
        parent_session_id: "parent-x".into(),
        child_session_id: "child-running".into(),
        subagent_type: "explore".into(),
        description: "d".into(),
        prompt: "p".into(),
        status: "running".into(),
        started_at: chrono::Utc::now(),
        completed_at: None,
        duration_ms: None,
        tool_calls: None,
        turns: None,
        error: None,
        effective_context_source: None,
        context_normalized: false,
        fork_copy_error: None,
        persona: None,
        resumed_from: None,
        child_cwd: Some("/workspace".into()),
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: None,
    };
    write_subagent_meta(&parent_dir, &meta);
    let data = std::fs::read_to_string(parent_dir.join("meta.json")).unwrap();
    let loaded: SubagentMeta = serde_json::from_str(&data).unwrap();
    let is_terminal = matches!(loaded.status.as_str(), "completed" | "failed" | "cancelled");
    assert!(
            !is_terminal,
            "status=running should NOT be considered terminal/resumable"
        );
    let _ = std::fs::remove_dir_all(&dir);
}
/// Count persisted `SubagentFinished{status:"cancelled"}` for `id` on a
/// session cmd channel, asserting field consistency.
fn drain_cancelled_finish_cmds(
    cmd_rx: &mut mpsc::UnboundedReceiver<SessionCommand>,
    id: &str,
) -> usize {
    let mut count = 0;
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let SessionCommand::XaiSessionNotification { notification } = cmd
            && let SessionUpdate::SubagentFinished { subagent_id, status, error, .. } = &notification
                .update && subagent_id == id
        {
            assert_eq!(status, "cancelled");
            assert_eq!(error.as_deref(), Some("interrupted by process restart"));
            count += 1;
        }
    }
    count
}
/// Count live `SubagentFinished{status:"cancelled"}` for `id` broadcast to
/// the gateway, asserting method + typed payload (not substring matching).
fn drain_cancelled_finish_broadcasts(
    gateway_rx: &mut mpsc::UnboundedReceiver<
        crate::test_support::lsp_runtime::GatewayOut,
    >,
    id: &str,
) -> usize {
    let mut count = 0;
    while let Ok(msg) = gateway_rx.try_recv() {
        let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
            continue;
        };
        assert_eq!(args.request.method.as_ref(), "x.ai/session_notification");
        let notification: SessionNotification = serde_json::from_str(
                args.request.params.get(),
            )
            .expect("params must deserialize as SessionNotification");
        if let SessionUpdate::SubagentFinished { subagent_id, status, .. } = &notification
            .update && subagent_id == id
        {
            assert_eq!(status, "cancelled");
            count += 1;
        }
    }
    count
}
/// A `running` meta with no terminal counterpart, as left by a dead process.
fn running_test_meta(id: &str, parent_session_id: &str) -> SubagentMeta {
    SubagentMeta {
        subagent_id: id.into(),
        parent_session_id: parent_session_id.into(),
        child_session_id: format!("child-{id}"),
        subagent_type: "explore".into(),
        description: "task".into(),
        prompt: "do work".into(),
        status: "running".into(),
        started_at: chrono::Utc::now(),
        completed_at: None,
        duration_ms: None,
        tool_calls: None,
        turns: None,
        error: None,
        effective_context_source: None,
        context_normalized: false,
        fork_copy_error: None,
        persona: None,
        resumed_from: None,
        child_cwd: Some("/workspace".into()),
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: None,
    }
}
fn inspection(id: &str, status: SubagentSnapshotStatus) -> SubagentInspection {
    SubagentInspection {
        snapshot: SubagentSnapshot {
            subagent_id: id.to_string(),
            description: "task".to_string(),
            subagent_type: "explore".to_string(),
            status,
            started_at_epoch_ms: 0,
            duration_ms: 50,
            persona: None,
        },
        parent_session_id: "parent-x".to_string(),
        child_session_id: format!("child-{id}"),
        fork_parent_prompt_id: None,
        resumed_from: None,
    }
}
async fn reconcile_with_inspections(
    unfinished: &[(String, String)],
    inspections: HashMap<String, Option<SubagentInspection>>,
    session_dir: &Path,
    gateway: &GatewaySender,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
) {
    let expected = inspections.len();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let backend = ChannelBackend::new(event_tx);
    let respond = async move {
        for _ in 0..expected {
            let event = event_rx.recv().await.expect("inspection event");
            let SubagentEvent::Inspect(request) = event else {
                panic!("expected Inspect event");
            };
            let value = inspections.get(&request.subagent_id).cloned().flatten();
            let _ = request.respond_to.send(value);
        }
    };
    tokio::join!(
            reconcile_orphaned_subagents_with_backend(
                unfinished,
                &backend,
                session_dir,
                "parent-x",
                gateway,
                parent_cmd_tx,
            ),
            respond,
        );
}
#[tokio::test]
async fn reconcile_orphan_flips_running_meta_to_cancelled() {
    use crate::test_support::lsp_runtime::test_gateway_with_receiver;
    let session_dir = tempfile::TempDir::new().unwrap();
    let id = "sa-orphan";
    let sub_dir = session_dir.path().join("subagents").join(id);
    write_subagent_meta(&sub_dir, &running_test_meta(id, "parent-x"));
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    reconcile_with_inspections(
            &[],
            HashMap::from([(id.to_string(), None)]),
            session_dir.path(),
            &gateway,
            Some(&cmd_tx),
        )
        .await;
    let reread: SubagentMeta = serde_json::from_str(
            &std::fs::read_to_string(sub_dir.join("meta.json")).unwrap(),
        )
        .unwrap();
    assert_eq!(reread.status, "cancelled");
    assert_eq!(reread.tool_calls, Some(0));
    assert_eq!(reread.turns, Some(0));
    assert_eq!(drain_cancelled_finish_cmds(&mut cmd_rx, id), 1);
    assert_eq!(
            drain_cancelled_finish_broadcasts(&mut gateway_rx, id),
            1
        );
}
#[tokio::test]
async fn reconcile_orphan_skips_shared_actor_live_child() {
    let session_dir = tempfile::TempDir::new().unwrap();
    let id = "sa-live";
    let sub_dir = session_dir.path().join("subagents").join(id);
    write_subagent_meta(&sub_dir, &running_test_meta(id, "parent-x"));
    reconcile_with_inspections(
            &[],
            HashMap::from([
                (
                    id.to_string(),
                    Some(inspection(id, SubagentSnapshotStatus::Initializing)),
                ),
            ]),
            session_dir.path(),
            &test_gateway(),
            None,
        )
        .await;
    let reread: SubagentMeta = serde_json::from_str(
            &std::fs::read_to_string(sub_dir.join("meta.json")).unwrap(),
        )
        .unwrap();
    assert_eq!(reread.status, "running");
}
#[tokio::test]
async fn reconcile_reemits_shared_actor_terminal_outcome() {
    let session_dir = tempfile::TempDir::new().unwrap();
    let id = "sa-raced";
    let sub_dir = session_dir.path().join("subagents").join(id);
    write_subagent_meta(&sub_dir, &running_test_meta(id, "parent-x"));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    reconcile_with_inspections(
            &[(id.to_string(), format!("child-{id}"))],
            HashMap::from([
                (
                    id.to_string(),
                    Some(
                        inspection(
                            id,
                            SubagentSnapshotStatus::Completed {
                                output: "done".to_string(),
                                tool_calls: 7,
                                turns: 2,
                                worktree_path: None,
                            },
                        ),
                    ),
                ),
            ]),
            session_dir.path(),
            &test_gateway(),
            Some(&cmd_tx),
        )
        .await;
    let finish = std::iter::from_fn(|| cmd_rx.try_recv().ok())
        .find_map(|command| {
            let SessionCommand::XaiSessionNotification { notification } = command else {
                return None;
            };
            let SessionUpdate::SubagentFinished { status, tool_calls, .. } = notification
                .update else {
                return None;
            };
            Some((status, tool_calls))
        });
    assert_eq!(finish, Some(("completed".to_string(), 7)));
    let reread: SubagentMeta = serde_json::from_str(
            &std::fs::read_to_string(sub_dir.join("meta.json")).unwrap(),
        )
        .unwrap();
    assert_eq!(reread.status, "running");
}
#[tokio::test]
async fn reconcile_dedups_replay_and_running_meta_sources() {
    let session_dir = tempfile::TempDir::new().unwrap();
    let id = "sa-crash";
    let sub_dir = session_dir.path().join("subagents").join(id);
    write_subagent_meta(&sub_dir, &running_test_meta(id, "parent-x"));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    reconcile_with_inspections(
            &[(id.to_string(), format!("child-{id}"))],
            HashMap::from([(id.to_string(), None)]),
            session_dir.path(),
            &test_gateway(),
            Some(&cmd_tx),
        )
        .await;
    assert_eq!(drain_cancelled_finish_cmds(&mut cmd_rx, id), 1);
}
#[test]
fn resume_rejects_conflicting_subagent_type() {
    let source = ResumeSourceData {
        subagent_id: "sub-gp".into(),
        child_session_id: "child-gp".into(),
        child_cwd: "/workspace".into(),
        worktree_path: None,
        snapshot_ref: None,
        subagent_type: "general-purpose".into(),
        persona: None,
        model_id: None,
    };
    let request_type = "explore";
    assert_ne!(
            request_type, source.subagent_type,
            "conflicting types should be detected"
        );
}
#[test]
fn resume_rejects_conflicting_persona() {
    let source = ResumeSourceData {
        subagent_id: "sub-impl".into(),
        child_session_id: "child-impl".into(),
        child_cwd: "/workspace".into(),
        worktree_path: None,
        snapshot_ref: None,
        subagent_type: "general-purpose".into(),
        persona: Some("implementer".into()),
        model_id: None,
    };
    let request_persona = Some("reviewer".to_string());
    let conflict = request_persona.as_deref() != source.persona.as_deref();
    assert!(conflict, "different persona should be detected as conflict");
}
#[test]
fn resume_allows_matching_identity() {
    let source = ResumeSourceData {
        subagent_id: "sub-ok".into(),
        child_session_id: "child-ok".into(),
        child_cwd: "/workspace".into(),
        worktree_path: None,
        snapshot_ref: None,
        subagent_type: "general-purpose".into(),
        persona: Some("implementer".into()),
        model_id: Some("grok-3".into()),
    };
    assert_eq!("general-purpose", source.subagent_type);
    assert_eq!(Some("implementer"), source.persona.as_deref());
    assert_eq!(Some("grok-3"), source.model_id.as_deref());
}
#[test]
fn resume_identity_does_not_gate_on_model() {
    let source = ResumeSourceData {
        subagent_id: "sub-model".into(),
        child_session_id: "child-model".into(),
        child_cwd: "/workspace".into(),
        worktree_path: None,
        snapshot_ref: None,
        subagent_type: "general-purpose".into(),
        persona: None,
        model_id: Some("grok-3".into()),
    };
    assert!(
            xai_grok_subagent_resolution::validate_resume_identity(
                "general-purpose",
                None,
                &source,
            )
            .is_ok()
        );
    assert_eq!(
            source.model_id.as_deref(),
            Some("grok-3"),
            "source model remains available for pinning"
        );
}
#[test]
fn durable_meta_roundtrips_effective_model_id() {
    let dir = std::env::temp_dir()
        .join("grok-test-model-roundtrip")
        .join(uuid::Uuid::now_v7().to_string());
    let _ = std::fs::create_dir_all(&dir);
    let meta = SubagentMeta {
        subagent_id: "sa-model".into(),
        parent_session_id: "parent".into(),
        child_session_id: "child".into(),
        subagent_type: "general-purpose".into(),
        description: "d".into(),
        prompt: "p".into(),
        status: "completed".into(),
        started_at: chrono::Utc::now(),
        completed_at: Some(chrono::Utc::now()),
        duration_ms: Some(100),
        tool_calls: Some(1),
        turns: Some(1),
        error: None,
        effective_context_source: None,
        context_normalized: false,
        fork_copy_error: None,
        persona: None,
        resumed_from: None,
        child_cwd: Some("/workspace".into()),
        worktree_path: None,
        snapshot_ref: None,
        effective_model_id: Some("grok-3".into()),
    };
    write_subagent_meta(&dir, &meta);
    let data = std::fs::read_to_string(dir.join("meta.json")).unwrap();
    let loaded: SubagentMeta = serde_json::from_str(&data).unwrap();
    assert_eq!(
            loaded.effective_model_id.as_deref(),
            Some("grok-3"),
            "model ID should round-trip through meta.json"
        );
    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn resume_model_pinning_overrides_default_resolution() {
    let source_model = Some("grok-3".to_string());
    let resolved_model = "grok-light";
    let needs_pin = source_model.as_deref() != Some(resolved_model);
    assert!(
            needs_pin,
            "resolved model differs from source — pinning should trigger"
        );
    let resolved_same = "grok-3";
    let no_pin = source_model.as_deref() == Some(resolved_same);
    assert!(no_pin, "same model — no pinning needed");
}
#[test]
fn resume_window_safety_rejects_instead_of_swapping() {
    let estimated_tokens: u64 = 100_000;
    let child_window: u64 = 256_000;
    const SAFE_RESUME_PERCENT: u64 = 80;
    let threshold = child_window * SAFE_RESUME_PERCENT / 100;
    assert!(
            estimated_tokens <= threshold,
            "100k tokens should be within 80% of 256k window"
        );
    let large_transcript: u64 = 210_000;
    assert!(
            large_transcript > threshold,
            "210k tokens exceeds 80% of 256k window — resume should be rejected"
        );
}
#[test]
fn provenance_carries_resumed_from() {
    let prov = SubagentProvenance {
        fork_parent_prompt_id: Some("prompt-1".into()),
        resumed_from: Some("prev-agent-id".into()),
    };
    assert_eq!(prov.resumed_from.as_deref(), Some("prev-agent-id"));
    let fresh = SubagentProvenance::default();
    assert!(fresh.resumed_from.is_none());
}
#[test]
fn notification_subagent_spawned_includes_resumed_from() {
    let notification = SessionUpdate::SubagentSpawned {
        subagent_id: "sa-resumed".into(),
        parent_session_id: "parent".into(),
        parent_prompt_id: Some("prompt-1".into()),
        child_session_id: "child-resumed".into(),
        subagent_type: "general-purpose".into(),
        description: "fix review feedback".into(),
        effective_context_source: Some("resumed".into()),
        context_normalized: false,
        capability_mode: None,
        persona: Some("implementer".into()),
        role: None,
        model: None,
        resumed_from: Some("prev-agent-id".into()),
        workflow_run_id: None,
    };
    let json = serde_json::to_value(&notification).unwrap();
    assert_eq!(json["resumed_from"], "prev-agent-id");
    assert_eq!(json["effective_context_source"], "resumed");
    assert_eq!(json["role"], serde_json::Value::Null);
    assert_eq!(json["model"], serde_json::Value::Null);
    let fresh = SessionUpdate::SubagentSpawned {
        subagent_id: "sa-fresh".into(),
        parent_session_id: "p".into(),
        parent_prompt_id: None,
        child_session_id: "c".into(),
        subagent_type: "explore".into(),
        description: "d".into(),
        effective_context_source: Some("new".into()),
        context_normalized: false,
        capability_mode: None,
        persona: None,
        role: None,
        model: None,
        resumed_from: None,
        workflow_run_id: None,
    };
    let json = serde_json::to_value(&fresh).unwrap();
    assert!(json.get("resumed_from").is_none());
    assert!(json.get("role").is_none());
    assert!(json.get("model").is_none());
}
#[test]
fn upload_ref_includes_resumed_from() {
    let ref_resumed = SubagentSpawnedRef {
        subagent_id: "sa-r".into(),
        child_session_id: "child-r".into(),
        subagent_type: "general-purpose".into(),
        description: "goal achievement skeptic".into(),
        persona: Some("implementer".into()),
        resumed_from: Some("prev-agent".into()),
    };
    let json = serde_json::to_value(&ref_resumed).unwrap();
    assert_eq!(json["resumed_from"], "prev-agent");
    assert_eq!(json["description"], "goal achievement skeptic");
    let ref_fresh = SubagentSpawnedRef {
        subagent_id: "sa-f".into(),
        child_session_id: "child-f".into(),
        subagent_type: "explore".into(),
        description: String::new(),
        persona: None,
        resumed_from: None,
    };
    let json = serde_json::to_value(&ref_fresh).unwrap();
    assert!(json.get("resumed_from").is_none());
    assert!(json.get("description").is_none());
    let parsed: SubagentSpawnedRef = serde_json::from_value(json).unwrap();
    assert!(parsed.description.is_empty());
}
#[test]
fn turn_active_flag_defaults_to_false() {
    let presentation = SubagentPresentation::new();
    assert!(
            !presentation
                .turn_active_flag()
                .load(std::sync::atomic::Ordering::Relaxed)
        );
}
#[test]
fn turn_active_flag_shared_via_arc() {
    let presentation = SubagentPresentation::new();
    let flag = presentation.turn_active_flag();
    assert!(!flag.load(std::sync::atomic::Ordering::Relaxed));
    flag.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
            presentation
                .turn_active_flag()
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    flag.store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(
            !presentation
                .turn_active_flag()
                .load(std::sync::atomic::Ordering::Relaxed)
        );
}
fn ctx_with_parent_chat_state(
    session_model_id: &str,
    inference_slug: &str,
    global_model_id: &str,
    available_models: indexmap::IndexMap<String, crate::agent::config::ModelEntry>,
) -> SubagentSpawnContext {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new(session_model_id);
    ctx.sampling_config_model_id = acp::ModelId::new(session_model_id);
    ctx.sampling_config.model = inference_slug.to_string();
    ctx.parent_chat_state = Some(spawn_test_parent_chat_state_with_catalog_identity(
        session_model_id,
        inference_slug,
    ));
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        available_models.clone(),
        acp::ModelId::new(global_model_id),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    ctx.available_models = available_models;
    ctx
}
#[tokio::test]
async fn read_parent_sampling_config_keeps_auto_catalog_id_with_routing_slug() {
    let mut models = indexmap::IndexMap::new();
    models.insert("auto".to_string(), test_model_entry("grok-4.5"));
    let ctx = ctx_with_parent_chat_state("auto", "grok-4.5", "composer-2-fast", models);
    let (config, model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(config.model, "grok-4.5");
    assert_eq!(model_id.0.as_ref(), "auto");
}
#[tokio::test]
async fn read_parent_sampling_config_keeps_auto_when_catalog_has_slug_key_only() {
    use xai_grok_sampler::AuthScheme;

    for (committed_auth, remapped_auth, expected_identity_auth) in [
        (
            xai_chat_state::CatalogAuthScheme::Bearer,
            AuthScheme::None,
            xai_chat_state::CatalogAuthScheme::None,
        ),
        (
            xai_chat_state::CatalogAuthScheme::None,
            AuthScheme::Bearer,
            xai_chat_state::CatalogAuthScheme::Bearer,
        ),
    ] {
        let mut models = indexmap::IndexMap::new();
        let mut remapped = test_model_entry("grok-4.5");
        remapped.info.auth_scheme = remapped_auth;
        models.insert("grok-4.5".to_string(), remapped);
        let ctx = ctx_with_parent_chat_state("auto", "grok-4.5", "auto", models);
        let chat = ctx.parent_chat_state.as_ref().expect("chat state");
        let mut snapshot = chat.snapshot().await.expect("chat snapshot");
        snapshot.catalog_identity = Some(xai_chat_state::CatalogIdentity {
            model_id: "auto".to_string(),
            route: "grok-4.5".to_string(),
            lineage: xai_chat_state::CatalogResolutionLineage::UniqueRoute,
            auth_scheme: Some(committed_auth),
        });
        chat.restore_snapshot(snapshot);
        let prepared = read_parent_prepared_model(&ctx).await;
        assert_eq!(prepared.sampling_config.model, "grok-4.5");
        assert_eq!(prepared.model_id.0.as_ref(), "grok-4.5");
        assert_eq!(
            prepared.catalog_identity.auth_scheme,
            Some(expected_identity_auth),
            "a UniqueRoute remap must commit the remapped entry's auth with its new id"
        );

        let mut nested = ctx_with_parent_chat_state(
            "grok-4.5",
            "grok-4.5",
            "grok-4.5",
            indexmap::IndexMap::new(),
        );
        nested.sampling_config.auth_scheme = match remapped_auth {
            AuthScheme::Bearer => AuthScheme::None,
            AuthScheme::None => AuthScheme::Bearer,
            AuthScheme::XApiKey => unreachable!("table excludes x-api-key"),
        };
        let nested_chat = nested.parent_chat_state.as_ref().expect("nested chat state");
        let mut nested_snapshot = nested_chat.snapshot().await.expect("nested chat snapshot");
        nested_snapshot.catalog_identity = Some(prepared.catalog_identity);
        nested_chat.restore_snapshot(nested_snapshot);

        let nested_prepared = read_parent_prepared_model(&nested).await;
        assert_eq!(
            nested_prepared.sampling_config.auth_scheme,
            remapped_auth,
            "a grandchild catalog miss must retain the auth committed by the remap"
        );
    }
}
#[tokio::test]
async fn read_parent_sampling_config_fallback_uses_session_model_id() {
    let mut models = indexmap::IndexMap::new();
    models.insert("composer-2-fast".to_string(), test_model_entry("composer-2-fast"));
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("composer-2-fast");
    ctx.sampling_config_model_id = acp::ModelId::new("composer-2-fast");
    ctx.parent_chat_state = None;
    ctx.sampling_config.model = "composer-2-fast".to_string();
    ctx.available_models = models;
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        indexmap::IndexMap::new(),
        acp::ModelId::new("auto"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(config.model, "composer-2-fast");
    assert_eq!(model_id.0.as_ref(), "composer-2-fast");
    assert_ne!(model_id.0.as_ref(), "auto");
}

#[tokio::test]
async fn read_parent_sampling_config_fallback_binds_caps_to_startup_model() {
    let startup_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let switched_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut startup = test_model_entry("startup-routing-model");
    startup.info.codex_wire = Some(startup_caps.clone());
    let mut switched = test_model_entry("switched-routing-model");
    switched.info.codex_wire = Some(switched_caps);
    let mut models = indexmap::IndexMap::new();
    models.insert("startup-model".to_string(), startup);
    models.insert("switched-model".to_string(), switched);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_chat_state = None;
    ctx.model_id = acp::ModelId::new("switched-model");
    ctx.sampling_config_model_id = acp::ModelId::new("startup-model");
    ctx.sampling_config.model = "startup-routing-model".to_string();
    ctx.available_models = models.clone();
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        models,
        acp::ModelId::new("switched-model"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "startup-model");
    assert_eq!(config.model, "startup-routing-model");
    assert_eq!(config.codex_wire, Some(startup_caps));
}

#[tokio::test]
async fn read_parent_sampling_config_fallback_reused_key_keeps_baseline_wire() {
    let baseline_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let replacement_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut replacement = test_model_entry("replacement-routing-model");
    replacement.info.codex_wire = Some(replacement_caps);
    let mut models = indexmap::IndexMap::new();
    models.insert("auto".to_string(), replacement);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_chat_state = None;
    ctx.model_id = acp::ModelId::new("auto");
    ctx.sampling_config_model_id = acp::ModelId::new("auto");
    ctx.sampling_config.model = "old-routing-model".to_string();
    ctx.sampling_config.codex_wire = Some(baseline_caps.clone());
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        models,
        acp::ModelId::new("auto"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "auto");
    assert_eq!(config.model, "old-routing-model");
    assert_eq!(config.codex_wire, Some(baseline_caps));
}
#[tokio::test]
async fn read_parent_sampling_config_ignores_global_default() {
    let mut models = indexmap::IndexMap::new();
    models.insert("composer-2-fast".to_string(), test_model_entry("composer-2-fast"));
    let ctx = ctx_with_parent_chat_state(
        "composer-2-fast",
        "composer-2-fast",
        "auto",
        models,
    );
    let (config, model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(config.model, "composer-2-fast");
    assert_eq!(model_id.0.as_ref(), "composer-2-fast");
    assert_ne!(
            model_id.0.as_ref(),
            ctx.models_manager.current_model_id().0.as_ref(),
        );
}
/// Every subagent config path must carry the live bearer resolver: a
/// config frozen at spawn 401s for the rest of the subagent's life once
/// the parent rotates its token (the wake-from-sleep failure mode).
/// First-party base URL so the assertion holds whether the catalog memo
/// reports `NotByok` or `Unknown`.
#[tokio::test]
async fn read_parent_sampling_config_fallback_wires_bearer_resolver() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_chat_state = None;
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
    );
    ctx.sampling_config.model = "grok-4.5".to_string();
    ctx.sampling_config.base_url = "https://api.x.ai/v1".to_string();
    let (config, _) = read_parent_sampling_config(&ctx).await;
    assert!(config.bearer_resolver.is_some());
}

#[tokio::test]
async fn read_parent_sampling_config_fallback_binds_auth_to_selected_catalog_entry() {
    let shared_slug = "shared-routing-slug";
    let mut selected = test_model_entry(shared_slug);
    selected.info.auth_scheme = xai_grok_sampler::AuthScheme::Bearer;
    let mut shadow = test_model_entry("unrelated-routing-slug");
    shadow.api_key = Some("shadow-byok-key".to_string());
    shadow.info.auth_scheme = xai_grok_sampler::AuthScheme::None;
    let mut models = indexmap::IndexMap::new();
    models.insert("selected-entry".to_string(), selected);
    models.insert(shared_slug.to_string(), shadow);
    let mut ctx = ctx_with_parent_chat_state(
        "selected-entry",
        shared_slug,
        "selected-entry",
        models,
    );
    ctx.parent_chat_state = None;
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
    );
    ctx.sampling_config.base_url = "https://api.x.ai/v1".to_string();

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "selected-entry");
    assert_eq!(config.auth_scheme, xai_grok_sampler::AuthScheme::Bearer);
    assert!(
        config.bearer_resolver.is_some(),
        "fallback auth must use the same selected entry as its capabilities"
    );
}
/// The inherit-live path honors `would_strip_fallback_key` like the
/// other two paths (it used to install the resolver unconditionally,
/// stripping a no-session parent's env-key fallback).
#[tokio::test]
async fn read_parent_sampling_config_live_never_strips_a_fallback_key() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
    );
    ctx.auth = None;
    let chat = spawn_test_parent_chat_state("grok-4.5");
    chat.update_credentials(xai_chat_state::Credentials::bound(
        Some("xai-env-fallback".to_string()),
        xai_chat_state::AuthType::SessionToken,
        xai_grok_sampler::CredentialSource::XaiSession,
    ));
    ctx.parent_chat_state = Some(chat);
    let (config, _) = read_parent_sampling_config(&ctx).await;
    assert!(
            config.bearer_resolver.is_none(),
            "with no session, the live path must not displace a fallback key"
        );
    assert_eq!(config.api_key.as_deref(), Some("xai-env-fallback"));
}
/// #136 steps 2–3: the live parent inherit must surface the provenance
/// bound onto parent credentials when no declared header is present.
/// Header re-derivation alone left ordinary session-token parents
/// unlabelled (`None`), which is the under-restricting direction for L3.
#[tokio::test]
async fn read_parent_sampling_config_live_carries_stored_session_source() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
    );
    let chat = spawn_test_parent_chat_state("grok-4.5");
    if let Some(mut cfg) = chat.get_sampling_config().await {
        cfg.base_url = "https://api.x.ai/v1".to_string();
        chat.update_sampling_config(cfg);
    }
    chat.update_credentials(xai_chat_state::Credentials::bound(
        Some("parent-session-jwt".to_string()),
        xai_chat_state::AuthType::SessionToken,
        xai_grok_sampler::CredentialSource::XaiSession,
    ));
    ctx.parent_chat_state = Some(chat);
    let (config, _) = read_parent_sampling_config(&ctx).await;
    assert_eq!(
        config.credential_source,
        Some(xai_grok_sampler::CredentialSource::XaiSession),
        "live inherit must re-emit the parent's stored provenance, not None"
    );
    assert_eq!(config.api_key.as_deref(), Some("parent-session-jwt"));
}
/// Same as the session case for a BYOK parent key.
#[tokio::test]
async fn read_parent_sampling_config_live_carries_stored_byok_source() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
    );
    let chat = spawn_test_parent_chat_state("grok-4.5");
    chat.update_credentials(xai_chat_state::Credentials::bound(
        Some("parent-byok-key".to_string()),
        xai_chat_state::AuthType::ApiKey,
        xai_grok_sampler::CredentialSource::ModelApiKey,
    ));
    ctx.parent_chat_state = Some(chat);
    let (config, _) = read_parent_sampling_config(&ctx).await;
    assert_eq!(
        config.credential_source,
        Some(xai_grok_sampler::CredentialSource::ModelApiKey),
        "live inherit must re-emit the parent's stored BYOK provenance, not None"
    );
    assert_eq!(config.api_key.as_deref(), Some("parent-byok-key"));
}
/// #180 seam: dual-auth parent — model `api_key` plus a declared credential
/// header still in the maps. `read_parent_sampling_config` must keep the
/// stored `ModelApiKey` label, not invent `ExplicitHeader` from the maps
/// while leaving `api_key` set (the L3 false-refuse on External).
///
/// Cheap fixture: same parent-chat spawn as the BYOK / declared-header
/// siblings. Also asserts `SamplingClient::new` accepts the inherited
/// config — coverage through the crate boundary.
#[tokio::test]
async fn read_parent_sampling_config_dual_auth_keeps_model_api_key_not_explicit_header() {
    const MODEL_KEY: &str = "sk-upstream-byok";
    const EDGE_HEADER: &str = "x-api-key";
    const EDGE_VALUE: &str = "sk-gateway-edge";
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
    );
    let chat = spawn_test_parent_chat_state("grok-4.5");
    let mut cfg = chat
        .get_sampling_config()
        .await
        .expect("test parent has a sampling config");
    cfg.base_url = "https://gateway.example/v1".to_string();
    cfg.endpoint_trust = Some(xai_grok_sampler::EndpointTrustClass::External);
    cfg.extra_headers
        .insert(EDGE_HEADER.to_string(), EDGE_VALUE.to_string());
    chat.update_sampling_config(cfg);
    chat.update_credentials(xai_chat_state::Credentials::bound(
        Some(MODEL_KEY.to_string()),
        xai_chat_state::AuthType::ApiKey,
        xai_grok_sampler::CredentialSource::ModelApiKey,
    ));
    ctx.parent_chat_state = Some(chat);

    let (inherited, _) = read_parent_sampling_config(&ctx).await;
    assert!(
        matches!(
            inherited.credential_source,
            Some(xai_grok_sampler::CredentialSource::ModelApiKey)
        ),
        "dual-auth parent inherit must keep stored ModelApiKey; inventing \
         ExplicitHeader from the maps while api_key remains is the #180 L3 \
         false-refuse. got={:?}",
        inherited.credential_source
    );
    assert!(
        !matches!(
            inherited.credential_source,
            Some(xai_grok_sampler::CredentialSource::ExplicitHeader { .. })
        ),
        "dual-auth parent inherit must not invent ExplicitHeader from the \
         header maps. (Value withheld.)"
    );
    assert!(
        inherited.api_key.as_deref() == Some(MODEL_KEY),
        "dual-auth parent inherit must keep the model-owned api_key. \
         (Value withheld.)"
    );
    assert!(
        inherited
            .extra_headers
            .get(EDGE_HEADER)
            .is_some_and(|v| v.as_str() == EDGE_VALUE),
        "declared gateway edge header must still ship in extra_headers. \
         (Value withheld.)"
    );
    xai_grok_sampler::SamplingClient::new(inherited).expect(
        "dual-auth parent inherit with stored ModelApiKey must construct on \
         an external origin; re-labelling ExplicitHeader while keeping \
         api_key is the #180 L3 false-refuse",
    );
}
/// `would_strip_fallback_key` on the inherit-fallback path: the baseline
/// keeps the env `XAI_API_KEY` even while `auth_type` flips to
/// `SessionToken`, and no resolver may displace it.
#[tokio::test]
async fn read_parent_sampling_config_fallback_never_strips_a_fallback_key() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_chat_state = None;
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
    );
    ctx.auth = None;
    ctx.sampling_config.model = "grok-4.5".to_string();
    ctx.sampling_config.base_url = "https://api.x.ai/v1".to_string();
    ctx.sampling_config.api_key = Some("xai-env-fallback".to_string());
    let (config, _) = read_parent_sampling_config(&ctx).await;
    assert!(config.bearer_resolver.is_none());
    assert_eq!(config.api_key.as_deref(), Some("xai-env-fallback"));
}
#[tokio::test]
async fn read_parent_sampling_config_fallback_no_resolver_for_api_key_method() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_chat_state = None;
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
    );
    ctx.sampling_config.model = "grok-4.5".to_string();
    ctx.sampling_config.base_url = "https://api.x.ai/v1".to_string();
    let (config, _) = read_parent_sampling_config(&ctx).await;
    assert!(config.bearer_resolver.is_none());
}

#[tokio::test]
async fn read_parent_sampling_config_fallback_catalog_miss_keeps_bound_byok_source_terminal() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_chat_state = None;
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
    );
    ctx.auth = Some(crate::auth::GrokAuth {
        key: "session-jwt".to_string(),
        ..crate::auth::GrokAuth::test_default()
    });
    ctx.sampling_config_model_id = acp::ModelId::new("removed-byok-entry");
    ctx.sampling_config.model = "removed-byok-route".to_string();
    ctx.sampling_config.base_url = "https://api.x.ai/v1".to_string();
    ctx.sampling_config.api_key = Some("provider-model-key".to_string());
    ctx.sampling_config.credential_source = Some(
        xai_grok_sampler::CredentialSource::AuthProvider {
            name: "removed-provider".to_string(),
        },
    );
    ctx.available_models.clear();

    let prepared = read_parent_prepared_model(&ctx).await;

    assert_eq!(prepared.model_id.0.as_ref(), "removed-byok-entry");
    assert_eq!(
        prepared.sampling_config.api_key.as_deref(),
        Some("provider-model-key")
    );
    assert!(
        prepared.sampling_config.bearer_resolver.is_none(),
        "actor-unavailable fallback must not replace bound provider auth with the session resolver"
    );
}
/// The override path wires the resolver for a session key regardless of
/// freshness. Hard-expired (the post-sleep 401 window) is the case that
/// matters: gating on wire-validity would freeze the subagent for life;
/// the sampler strips the dead seeded key at request time instead.
#[test]
fn resolve_model_override_wires_resolver_for_fresh_and_hard_expired_session_keys() {
    for auth in [
        crate::auth::GrokAuth {
            key: "session-jwt".into(),
            ..crate::auth::GrokAuth::test_default()
        },
        crate::auth::GrokAuth {
            key: "hard-expired-session-jwt".into(),
            create_time: chrono::Utc::now() - chrono::Duration::hours(2),
            expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
            ..crate::auth::GrokAuth::test_default()
        },
    ] {
        let key = auth.key.clone();
        let mut ctx = ctx_with_toggle(HashMap::new());
        ctx.auth_method_id = acp::AuthMethodId::new(
            crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
        );
        ctx.auth = Some(auth);
        ctx.available_models
            .insert("grok-4.5".to_string(), test_model_entry("grok-4.5"));
        let (config, _) = resolve_model_override_to_config("grok-4.5", &ctx).unwrap();
        assert!(config.bearer_resolver.is_some(), "key={key}");
    }
}

#[test]
fn resolve_model_override_prepared_carries_authoritative_reasoning_menu() {
    use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

    let mut ctx = ctx_with_toggle(HashMap::new());
    let mut entry = test_model_entry("reasoning-model");
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "max".into(),
        value: ReasoningEffort::Max,
        label: "Maximum".into(),
        description: None,
        default: true,
    }];
    ctx.available_models
        .insert("reasoning-model".to_string(), entry);

    let prepared = resolve_model_override_to_prepared("reasoning-model", &ctx).unwrap();
    assert!(prepared.supports_reasoning_effort);
    assert_eq!(prepared.reasoning_efforts.len(), 1);
    assert_eq!(prepared.reasoning_efforts[0].value, ReasoningEffort::Max);
}
/// #110: the pinned-model override path is a separate branch from live
/// parent inheritance, and it wires the parent session resolver too. A pinned
/// model authenticated only by a header the user declared must not get one:
/// `SamplingClient::post` treats the resolver as the sole auth source and
/// removes the declared header before sending.
#[test]
fn resolve_model_override_keeps_a_declared_header_over_the_session() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.auth_method_id =
        acp::AuthMethodId::new(crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID);
    ctx.auth = Some(crate::auth::GrokAuth {
        key: "session-jwt".into(),
        ..crate::auth::GrokAuth::test_default()
    });
    let mut entry = test_model_entry("grok-4.5");
    entry
        .info
        .extra_headers
        .insert("Authorization".into(), "Bearer vendor-sentinel".into());
    ctx.available_models.insert("grok-4.5".to_string(), entry);

    let (config, _) = resolve_model_override_to_config("grok-4.5", &ctx).unwrap();
    assert!(
        config.bearer_resolver.is_none(),
        "a declared header must not be replaced by the session resolver"
    );
    assert_eq!(
        config.credential_source,
        Some(xai_grok_sampler::CredentialSource::ExplicitHeader {
            header: "authorization".to_owned(),
            env: None,
        })
    );
}
/// `would_strip_fallback_key` on the override path. `XAI_API_KEY`'s
/// presence varies by environment, so assert the rule itself rather
/// than one branch of it.
#[test]
fn resolve_model_override_to_config_never_strips_a_fallback_key() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
    );
    ctx.auth = None;
    ctx.available_models.insert("grok-4.5".to_string(), test_model_entry("grok-4.5"));
    let (config, _) = resolve_model_override_to_config("grok-4.5", &ctx).unwrap();
    assert_eq!(
            config.bearer_resolver.is_some(),
            config.api_key.is_none(),
            "with no session, a resolver is installed only when it displaces nothing"
        );
}
/// A wired resolver is the sampler's sole auth source, so it must never
/// displace a per-model key.
#[test]
fn resolve_model_override_to_config_no_resolver_for_byok_model() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
    );
    let mut byok = test_model_entry("byok-model");
    byok.api_key = Some("sk-byok".to_string());
    ctx.available_models.insert("byok-model".to_string(), byok);
    let (config, _) = resolve_model_override_to_config("byok-model", &ctx).unwrap();
    assert!(config.bearer_resolver.is_none());
    assert_eq!(config.api_key.as_deref(), Some("sk-byok"));
}

#[test]
fn explicit_subagent_model_rejects_unavailable_custom_harness() {
    let active = xai_grok_agent::AgentDefinition::grok_build_plan();
    assert_eq!(
        validate_subagent_model_harness(&active, "missing-custom-harness", None),
        Err(SubagentModelHarnessError::Unavailable),
    );
}

#[test]
fn explicit_subagent_model_rejects_incompatible_resolved_harness() {
    let active = xai_grok_agent::AgentDefinition::grok_build_plan();
    let required = xai_grok_agent::AgentDefinition::codex();
    assert_eq!(
        validate_subagent_model_harness(&active, "codex", Some(&required)),
        Err(SubagentModelHarnessError::Incompatible),
    );
}

#[test]
fn explicit_subagent_model_resolves_and_rejects_same_named_external_harness() {
    let mut active = xai_grok_agent::AgentDefinition::default_grok_build();
    active.plugin_name = Some("plugin-one".to_owned());
    active.source_path = Some(std::path::PathBuf::from(
        "/plugins/plugin-one/agents/grok-build.md",
    ));
    active.prompt_body = Some("Plugin-owned grok-build prompt".to_owned());
    let cwd = tempfile::tempdir().unwrap();
    assert_eq!(
        resolve_and_validate_subagent_model_harness(&active, "grok-build", cwd.path(), None),
        Err(SubagentModelHarnessError::Incompatible),
    );
}

#[test]
fn explicit_subagent_model_accepts_exact_or_stock_compatible_harness() {
    let active = xai_grok_agent::AgentDefinition::codex();
    assert_eq!(
        validate_subagent_model_harness(&active, "codex", None),
        Ok(()),
    );
    let stock = xai_grok_agent::AgentDefinition::grok_build_plan();
    let required_stock = xai_grok_agent::AgentDefinition::default_grok_build();
    assert_eq!(
        validate_subagent_model_harness(&stock, "grok-build", Some(&required_stock)),
        Ok(()),
    );
}
#[tokio::test]
async fn read_parent_sampling_config_resolves_backend_search_from_catalog() {
    let mut entry = test_model_entry("grok-4.5");
    entry.info.supports_backend_search = true;
    let mut models = indexmap::IndexMap::new();
    models.insert("auto".to_string(), entry);
    let mut ctx = ctx_with_parent_chat_state("auto", "grok-4.5", "auto", models);
    ctx.sampling_config.supports_backend_search = false;
    let (config, _model_id) = read_parent_sampling_config(&ctx).await;
    assert!(
            config.supports_backend_search,
            "subagent should inherit backend-tools capability from the live model catalog"
        );
}
#[tokio::test]
async fn read_parent_sampling_config_resolves_codex_wire_from_the_catalog_not_the_parent() {
    // The two values must DIFFER or this test passes without proving
    // anything — the parent and a subagent usually run the same model,
    // which is exactly why #277 sat unnoticed.
    //
    // The asymmetry is real: `supports_reasoning_summary_parameter` is
    // `Some(false)` for Spark, which rejects `reasoning.summary`, and
    // `Some(true)` for the Sol preset, which accepts it. A subagent handed
    // the parent's capabilities sends a field its own model rejects.
    let child_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let parent_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut entry = test_model_entry("grok-4.5");
    entry.info.codex_wire = Some(child_caps.clone());
    let mut models = indexmap::IndexMap::new();
    models.insert("auto".to_string(), entry);
    let mut ctx = ctx_with_parent_chat_state("auto", "grok-4.5", "auto", models);
    ctx.sampling_config.codex_wire = Some(parent_caps);
    let (config, _model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(
        config.codex_wire,
        Some(child_caps),
        "the subagent's wire capabilities must come from its own catalog \
         entry, not from whatever model the parent happened to be running"
    );
}
#[tokio::test]
async fn read_parent_sampling_config_fallback_resolves_backend_search_from_catalog() {
    let mut entry = test_model_entry("composer-2-fast");
    entry.info.supports_backend_search = true;
    let mut models = indexmap::IndexMap::new();
    models.insert("composer-2-fast".to_string(), entry);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("composer-2-fast");
    ctx.sampling_config_model_id = acp::ModelId::new("composer-2-fast");
    ctx.parent_chat_state = None;
    ctx.sampling_config.model = "composer-2-fast".to_string();
    ctx.sampling_config.supports_backend_search = false;
    ctx.available_models = models.clone();
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        models,
        acp::ModelId::new("composer-2-fast"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(model_id.0.as_ref(), "composer-2-fast");
    assert!(
            config.supports_backend_search,
            "fallback path should also resolve backend-tools capability from the catalog"
        );
}
/// The fallback path (parent chat-state unavailable) re-resolves the other
/// three catalog facts, so it must re-resolve this one too — the live-path
/// test alone left half the surface uncovered.
#[tokio::test]
async fn read_parent_sampling_config_fallback_resolves_codex_wire_from_catalog() {
    let child_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let parent_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut entry = test_model_entry("composer-2-fast");
    entry.info.codex_wire = Some(child_caps.clone());
    let mut models = indexmap::IndexMap::new();
    models.insert("composer-2-fast".to_string(), entry);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("composer-2-fast");
    ctx.sampling_config_model_id = acp::ModelId::new("composer-2-fast");
    ctx.parent_chat_state = None;
    ctx.sampling_config.model = "composer-2-fast".to_string();
    ctx.sampling_config.codex_wire = Some(parent_caps);
    ctx.available_models = models.clone();
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        models,
        acp::ModelId::new("composer-2-fast"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, _model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(
        config.codex_wire,
        Some(child_caps),
        "the fallback path must resolve wire capabilities from the catalog too"
    );
}

/// A catalog miss is not always "no capabilities". A runtime-only model
/// (#159) can be absent from the config-derived catalog while the subagent
/// inherits the parent's model, and returning `None` would silently strip
/// capabilities the parent legitimately had.
#[tokio::test]
async fn read_parent_sampling_config_catalog_miss_keeps_parent_wire_for_the_same_model() {
    let parent_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("runtime-only-model");
    ctx.sampling_config_model_id = acp::ModelId::new("runtime-only-model");
    ctx.parent_chat_state = None;
    ctx.sampling_config.model = "runtime-only-model".to_string();
    ctx.sampling_config.codex_wire = Some(parent_caps.clone());
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        indexmap::IndexMap::new(),
        acp::ModelId::new("runtime-only-model"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, _model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(
        config.codex_wire,
        Some(parent_caps),
        "same model, catalog miss: the parent's value is the right one"
    );
}

#[tokio::test]
async fn read_parent_sampling_config_live_catalog_miss_keeps_same_model_baseline_wire() {
    let baseline_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let mut ctx = ctx_with_parent_chat_state(
        "runtime-only-model",
        "runtime-only-model",
        "runtime-only-model",
        indexmap::IndexMap::new(),
    );
    ctx.sampling_config.model = "runtime-only-model".to_string();
    ctx.sampling_config.codex_wire = Some(baseline_caps.clone());

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "runtime-only-model");
    assert_eq!(config.codex_wire, Some(baseline_caps));
}

#[tokio::test]
async fn read_parent_sampling_config_reused_startup_key_different_route_rejects_baseline_wire() {
    let baseline_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut ctx = ctx_with_parent_chat_state(
        "reused-key",
        "startup-routing-model",
        "reused-key",
        indexmap::IndexMap::new(),
    );
    ctx.sampling_config.model = "startup-routing-model".to_string();
    ctx.sampling_config.supports_backend_search = true;
    ctx.sampling_config.codex_wire = Some(baseline_caps);
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.sampling_config.model = "switched-routing-model".to_string();
    snapshot.catalog_identity = Some(test_catalog_identity(
        "reused-key",
        "switched-routing-model",
        xai_chat_state::CatalogResolutionLineage::ExactKey,
    ));
    chat.restore_snapshot(snapshot);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "reused-key");
    assert_eq!(config.model, "switched-routing-model");
    assert!(!config.supports_backend_search);
    assert_eq!(config.codex_wire, None);
}

#[tokio::test]
async fn read_parent_sampling_config_uses_live_catalog_after_same_route_refresh() {
    use xai_grok_sampler::AuthScheme;

    let frozen_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let mut frozen = test_model_entry("stable-routing-model");
    frozen.api_key = Some("stale-model-key".to_string());
    frozen.info.agent_type = "stale-harness".to_string();
    frozen.info.auth_scheme = AuthScheme::Bearer;
    frozen.info.supports_reasoning_effort = true;
    frozen.info.auto_compact_threshold_percent = Some(70);
    frozen.info.codex_wire = Some(frozen_caps.clone());
    let mut models = indexmap::IndexMap::new();
    models.insert("stable-entry".to_string(), frozen);
    let ctx = ctx_with_parent_chat_state(
        "stable-entry",
        "stable-routing-model",
        "stable-entry",
        models,
    );

    let mut refreshed = test_model_entry("stable-routing-model");
    refreshed.info.agent_type = "live-harness".to_string();
    refreshed.info.auth_scheme = AuthScheme::None;
    refreshed.info.supports_reasoning_effort = false;
    refreshed.info.auto_compact_threshold_percent = Some(90);
    refreshed.info.supports_backend_search = true;
    let refreshed_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    refreshed.info.codex_wire = Some(refreshed_caps.clone());
    ctx.models_manager.insert_test_entry("stable-entry", refreshed);

    let prepared = read_parent_prepared_model(&ctx).await;
    let config = prepared.sampling_config;
    let model_id = prepared.model_id;

    assert_eq!(model_id.0.as_ref(), "stable-entry");
    assert_eq!(config.auth_scheme, AuthScheme::None);
    assert!(config.supports_backend_search);
    assert_eq!(config.codex_wire, Some(refreshed_caps));
    assert_ne!(config.codex_wire, Some(frozen_caps));
    assert!(
        !prepared.supports_reasoning_effort,
        "reasoning overrides must follow the live prepared catalog snapshot"
    );
    assert!(!prepared.model_has_own_credentials);
    assert_eq!(prepared.agent_type, "live-harness");
    assert_eq!(prepared.auto_compact_threshold_percent, Some(90));
}

#[tokio::test]
async fn inherited_subagent_effort_reconciles_after_catalog_refresh() {
    use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

    let mut frozen = test_model_entry("stable-routing-model");
    frozen.info.supports_reasoning_effort = true;
    frozen.info.reasoning_effort = Some(ReasoningEffort::High);
    frozen.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "high".into(),
        value: ReasoningEffort::High,
        label: "High".into(),
        description: None,
        default: true,
    }];
    let ctx = ctx_with_parent_chat_state(
        "stable-entry",
        "stable-routing-model",
        "stable-entry",
        indexmap::IndexMap::from([("stable-entry".to_string(), frozen)]),
    );
    let chat = ctx.parent_chat_state.as_ref().expect("parent chat state");
    let mut snapshot = chat.snapshot().await.expect("parent snapshot");
    snapshot.sampling_config.reasoning_effort = Some(ReasoningEffort::High);
    chat.restore_snapshot(snapshot);

    let mut refreshed = test_model_entry("stable-routing-model");
    refreshed.info.supports_reasoning_effort = true;
    refreshed.info.reasoning_effort = Some(ReasoningEffort::Low);
    refreshed.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "low".into(),
        value: ReasoningEffort::Low,
        label: "Low".into(),
        description: None,
        default: true,
    }];
    ctx.models_manager.insert_test_entry("stable-entry", refreshed);

    let prepared = read_parent_prepared_model(&ctx).await;
    assert_eq!(
        prepared.sampling_config.reasoning_effort,
        Some(ReasoningEffort::High),
        "the resident parent remains immutable after catalog refresh"
    );
    assert_eq!(prepared.reasoning_efforts.len(), 1);
    assert_eq!(prepared.reasoning_efforts[0].value, ReasoningEffort::Low);

    let mut child = prepared.sampling_config.clone();
    reconcile_inherited_subagent_reasoning_effort(
        &mut child,
        prepared.supports_reasoning_effort,
        &prepared.reasoning_efforts,
    );
    assert_eq!(child.reasoning_effort, Some(ReasoningEffort::Low));
    assert_eq!(
        chat.snapshot()
            .await
            .expect("parent snapshot after child preparation")
            .sampling_config
            .reasoning_effort,
        Some(ReasoningEffort::High),
        "child reconciliation must not mutate the resident parent"
    );

    let mut legacy_minimal = prepared.sampling_config.clone();
    legacy_minimal.reasoning_effort = Some(ReasoningEffort::Minimal);
    reconcile_inherited_subagent_reasoning_effort(&mut legacy_minimal, true, &[]);
    assert_eq!(
        legacy_minimal.reasoning_effort,
        Some(ReasoningEffort::Minimal),
        "menu-less legacy models still offer minimal"
    );

    let none_menu = [ReasoningEffortOption {
        id: "none".into(),
        value: ReasoningEffort::None,
        label: "None".into(),
        description: None,
        default: true,
    }];
    let mut explicit_none = prepared.sampling_config.clone();
    explicit_none.reasoning_effort = Some(ReasoningEffort::None);
    reconcile_inherited_subagent_reasoning_effort(&mut explicit_none, true, &none_menu);
    assert_eq!(explicit_none.reasoning_effort, Some(ReasoningEffort::None));
}

#[tokio::test]
async fn read_parent_sampling_config_missing_committed_id_ignores_same_route_survivor() {
    use xai_grok_sampler::AuthScheme;

    let committed_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let survivor_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut survivor = test_model_entry("shared-routing-model");
    survivor.info.auth_scheme = AuthScheme::None;
    survivor.info.codex_wire = Some(survivor_caps);
    let mut models = indexmap::IndexMap::new();
    models.insert("surviving-entry".to_string(), survivor);
    let mut ctx = ctx_with_parent_chat_state(
        "startup-entry",
        "shared-routing-model",
        "surviving-entry",
        models,
    );
    ctx.sampling_config.model = "shared-routing-model".to_string();
    ctx.sampling_config.auth_scheme = AuthScheme::Bearer;
    ctx.sampling_config.codex_wire = Some(committed_caps.clone());
    ctx.sampling_config.supports_backend_search = true;
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.catalog_identity = Some(test_catalog_identity(
        "removed-entry",
        "shared-routing-model",
        xai_chat_state::CatalogResolutionLineage::ExactKey,
    ));
    snapshot
        .catalog_identity
        .as_mut()
        .expect("catalog identity")
        .auth_scheme = Some(xai_chat_state::CatalogAuthScheme::None);
    chat.restore_snapshot(snapshot);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "removed-entry");
    assert_eq!(config.auth_scheme, AuthScheme::None);
    assert!(config.api_key.is_none());
    assert!(!config.supports_backend_search);
    assert_eq!(config.codex_wire, None);
}

#[tokio::test]
async fn read_parent_sampling_config_legacy_committed_identity_missing_auth_fails_closed() {
    use xai_grok_sampler::AuthScheme;

    let mut ctx = ctx_with_parent_chat_state(
        "startup-bearer-entry",
        "legacy-removed-route",
        "startup-bearer-entry",
        indexmap::IndexMap::new(),
    );
    ctx.sampling_config.auth_scheme = AuthScheme::Bearer;
    ctx.sampling_config.api_key = Some("startup-bearer-key".to_string());
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    chat.update_credentials(xai_chat_state::Credentials::bound(
        Some("legacy-committed-key".to_string()),
        xai_chat_state::AuthType::ApiKey,
        xai_grok_sampler::CredentialSource::ModelApiKey,
    ));
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.catalog_identity = Some(xai_chat_state::CatalogIdentity {
        model_id: "legacy-removed-entry".to_string(),
        route: "legacy-removed-route".to_string(),
        lineage: xai_chat_state::CatalogResolutionLineage::ExactKey,
        auth_scheme: None,
    });
    chat.restore_snapshot(snapshot);

    let prepared = read_parent_prepared_model(&ctx).await;

    assert_eq!(prepared.model_id.0.as_ref(), "legacy-removed-entry");
    assert_eq!(prepared.sampling_config.auth_scheme, AuthScheme::None);
    assert!(
        prepared.sampling_config.api_key.is_none(),
        "a legacy committed identity with unknown auth must not borrow startup credentials"
    );
    assert!(prepared.sampling_config.bearer_resolver.is_none());
}

#[tokio::test]
async fn read_parent_sampling_config_legacy_identity_recovers_exact_auth_for_nested_miss() {
    use xai_grok_sampler::AuthScheme;

    let mut recovered = test_model_entry("legacy-route");
    recovered.info.auth_scheme = AuthScheme::XApiKey;
    let mut models = indexmap::IndexMap::new();
    models.insert("legacy-entry".to_string(), recovered);
    let ctx = ctx_with_parent_chat_state(
        "legacy-entry",
        "legacy-route",
        "legacy-entry",
        models,
    );
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.catalog_identity = Some(xai_chat_state::CatalogIdentity {
        model_id: "legacy-entry".to_string(),
        route: "legacy-route".to_string(),
        lineage: xai_chat_state::CatalogResolutionLineage::ExactKey,
        auth_scheme: None,
    });
    chat.restore_snapshot(snapshot);

    let recovered_prepared = read_parent_prepared_model(&ctx).await;
    assert_eq!(recovered_prepared.sampling_config.auth_scheme, AuthScheme::XApiKey);
    assert_eq!(
        recovered_prepared.catalog_identity.auth_scheme,
        Some(xai_chat_state::CatalogAuthScheme::XApiKey),
        "an exact catalog hit must upgrade a legacy identity with the authoritative auth"
    );

    let mut nested = ctx_with_parent_chat_state(
        "legacy-entry",
        "legacy-route",
        "legacy-entry",
        indexmap::IndexMap::new(),
    );
    nested.sampling_config.auth_scheme = AuthScheme::None;
    let nested_chat = nested.parent_chat_state.as_ref().expect("nested chat state");
    let mut nested_snapshot = nested_chat.snapshot().await.expect("nested chat snapshot");
    nested_snapshot.catalog_identity = Some(recovered_prepared.catalog_identity);
    nested_chat.restore_snapshot(nested_snapshot);

    let nested_prepared = read_parent_prepared_model(&nested).await;
    assert_eq!(nested_prepared.sampling_config.auth_scheme, AuthScheme::XApiKey);
    assert_eq!(
        nested_prepared.catalog_identity.auth_scheme,
        Some(xai_chat_state::CatalogAuthScheme::XApiKey)
    );
}

#[tokio::test]
async fn read_parent_sampling_config_removed_switched_bearer_ignores_none_startup_auth() {
    use xai_grok_sampler::AuthScheme;

    let mut ctx = ctx_with_parent_chat_state(
        "startup-none-entry",
        "switched-bearer-route",
        "startup-none-entry",
        indexmap::IndexMap::new(),
    );
    ctx.sampling_config_model_id = acp::ModelId::new("startup-none-entry");
    ctx.sampling_config.auth_scheme = AuthScheme::None;
    ctx.sampling_config.api_key = None;
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    chat.update_credentials(xai_chat_state::Credentials::bound(
        Some("switched-live-bearer".to_string()),
        xai_chat_state::AuthType::SessionToken,
        xai_grok_sampler::CredentialSource::XaiSession,
    ));
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.catalog_identity = Some(xai_chat_state::CatalogIdentity {
        model_id: "removed-bearer-entry".to_string(),
        route: "switched-bearer-route".to_string(),
        lineage: xai_chat_state::CatalogResolutionLineage::ExactKey,
        auth_scheme: Some(xai_chat_state::CatalogAuthScheme::Bearer),
    });
    chat.restore_snapshot(snapshot);

    let prepared = read_parent_prepared_model(&ctx).await;

    assert_eq!(prepared.model_id.0.as_ref(), "removed-bearer-entry");
    assert_eq!(prepared.catalog_identity.model_id, "removed-bearer-entry");
    assert_eq!(prepared.sampling_config.auth_scheme, AuthScheme::Bearer);
    assert_eq!(
        prepared.sampling_config.api_key.as_deref(),
        Some("switched-live-bearer")
    );
}

#[tokio::test]
async fn read_parent_sampling_config_removed_switched_byok_never_attaches_session_resolver() {
    use xai_grok_sampler::AuthScheme;

    let mut ctx = ctx_with_parent_chat_state(
        "startup-session-entry",
        "switched-byok-route",
        "startup-session-entry",
        indexmap::IndexMap::new(),
    );
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
    );
    ctx.auth = Some(crate::auth::GrokAuth {
        key: "startup-session-token".to_string(),
        ..crate::auth::GrokAuth::test_default()
    });
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut live_config = chat
        .get_sampling_config()
        .await
        .expect("test parent has sampling config");
    live_config.base_url = "https://api.x.ai/v1".to_string();
    chat.update_sampling_config(live_config);
    chat.update_credentials(xai_chat_state::Credentials::bound(
        Some("switched-model-key".to_string()),
        xai_chat_state::AuthType::ApiKey,
        xai_grok_sampler::CredentialSource::ModelApiKey,
    ));
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.catalog_identity = Some(xai_chat_state::CatalogIdentity {
        model_id: "removed-byok-entry".to_string(),
        route: "switched-byok-route".to_string(),
        lineage: xai_chat_state::CatalogResolutionLineage::ExactKey,
        auth_scheme: Some(xai_chat_state::CatalogAuthScheme::Bearer),
    });
    chat.restore_snapshot(snapshot);

    let prepared = read_parent_prepared_model(&ctx).await;

    assert_eq!(prepared.model_id.0.as_ref(), "removed-byok-entry");
    assert_eq!(
        prepared.sampling_config.api_key.as_deref(),
        Some("switched-model-key")
    );
    assert_eq!(
        prepared.sampling_config.credential_source,
        Some(xai_grok_sampler::CredentialSource::ModelApiKey)
    );
    assert!(
        prepared.sampling_config.bearer_resolver.is_none(),
        "bound model-key provenance must remain terminal after its catalog entry disappears"
    );
}

#[tokio::test]
async fn read_parent_sampling_config_opaque_name_override_keeps_committed_capabilities() {
    let committed_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let mut entry = test_model_entry("catalog-routing-model");
    entry.info.supports_backend_search = true;
    entry.info.codex_wire = Some(committed_caps.clone());
    let mut models = indexmap::IndexMap::new();
    models.insert("catalog-entry".to_string(), entry);
    let mut colliding = test_model_entry("opaque-backend-routing-hint");
    colliding.info.codex_wire = Some(xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    });
    models.insert("colliding-entry".to_string(), colliding);
    let ctx = ctx_with_parent_chat_state(
        "catalog-entry",
        "catalog-routing-model",
        "catalog-entry",
        models,
    );
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.sampling_config.model = "opaque-backend-routing-hint".to_string();
    chat.restore_snapshot(snapshot);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "catalog-entry");
    assert_eq!(config.model, "opaque-backend-routing-hint");
    assert!(config.supports_backend_search);
    assert_eq!(config.codex_wire, Some(committed_caps.clone()));

    // The inherited child stores the catalog's original route rather than
    // its opaque sampling override, so a grandchild resolves the same entry.
    let catalog_identity = ctx
        .parent_chat_state
        .as_ref()
        .expect("chat state")
        .get_sampling_config_with_model_id()
        .await
        .and_then(|(_, identity)| identity)
        .expect("selected catalog identity");
    let (mock, _persistence_rx) = xai_chat_state::MockChatPersistence::new();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let nested_chat = xai_chat_state::ChatStateActor::spawn_with_pruning_and_catalog_identity(
        vec![],
        test_sampling_config(&config.model),
        Some(catalog_identity),
        xai_chat_state::PruningConfig::default(),
        Box::new(mock),
        event_tx,
        tokio_util::sync::CancellationToken::new(),
    );
    let mut nested_ctx = ctx;
    nested_ctx.model_id = model_id.clone();
    nested_ctx.sampling_config_model_id = model_id;
    nested_ctx.sampling_config = config;
    nested_ctx.parent_chat_state = Some(nested_chat);

    let (nested_config, nested_model_id) = read_parent_sampling_config(&nested_ctx).await;

    assert_eq!(nested_model_id.0.as_ref(), "catalog-entry");
    assert_eq!(nested_config.model, "opaque-backend-routing-hint");
    assert!(nested_config.supports_backend_search);
    assert_eq!(nested_config.codex_wire, Some(committed_caps));
}

#[tokio::test]
async fn read_parent_sampling_config_switched_entry_opaque_override_keeps_capabilities() {
    let switched_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let mut switched = test_model_entry("switched-routing-model");
    switched.info.codex_wire = Some(switched_caps.clone());
    let mut models = indexmap::IndexMap::new();
    models.insert("switched-entry".to_string(), switched);
    let mut ctx = ctx_with_parent_chat_state(
        "switched-entry",
        "switched-routing-model",
        "switched-entry",
        models,
    );
    ctx.sampling_config_model_id = acp::ModelId::new("startup-entry");
    ctx.sampling_config.model = "startup-routing-model".to_string();
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.sampling_config.model = "opaque-switched-routing-hint".to_string();
    snapshot.catalog_identity = Some(test_catalog_identity(
        "switched-entry",
        "switched-routing-model",
        xai_chat_state::CatalogResolutionLineage::ExactKey,
    ));
    chat.restore_snapshot(snapshot);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "switched-entry");
    assert_eq!(config.model, "opaque-switched-routing-hint");
    assert_eq!(config.codex_wire, Some(switched_caps));
}

#[tokio::test]
async fn read_parent_sampling_config_unique_route_remaps_through_opaque_override() {
    let replacement_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let mut replacement = test_model_entry("retained-routing-model");
    replacement.info.supports_backend_search = true;
    replacement.info.codex_wire = Some(replacement_caps.clone());
    let mut colliding = test_model_entry("removed-alias");
    colliding.info.codex_wire = Some(xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    });
    let mut models = indexmap::IndexMap::new();
    models.insert("replacement-entry".to_string(), replacement);
    models.insert("colliding-entry".to_string(), colliding);
    let mut ctx = ctx_with_parent_chat_state(
        "removed-alias",
        "retained-routing-model",
        "startup-entry",
        models,
    );
    ctx.sampling_config_model_id = acp::ModelId::new("startup-entry");
    ctx.sampling_config.model = "startup-routing-model".to_string();
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.sampling_config.model = "removed-alias".to_string();
    snapshot.catalog_identity = Some(test_catalog_identity(
        "removed-alias",
        "retained-routing-model",
        xai_chat_state::CatalogResolutionLineage::UniqueRoute,
    ));
    chat.restore_snapshot(snapshot);

    let prepared = read_parent_prepared_model(&ctx).await;

    assert_eq!(prepared.model_id.0.as_ref(), "replacement-entry");
    assert_eq!(
        prepared.sampling_config.model,
        "removed-alias"
    );
    assert!(prepared.sampling_config.supports_backend_search);
    assert_eq!(prepared.sampling_config.codex_wire, Some(replacement_caps));
    assert_eq!(prepared.catalog_identity.model_id, "replacement-entry");
    assert_eq!(
        prepared.catalog_identity.route,
        "retained-routing-model"
    );
}

#[tokio::test]
async fn read_parent_sampling_config_opaque_override_rejects_reused_committed_key() {
    let baseline_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let replacement_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut replacement = test_model_entry("replacement-routing-model");
    replacement.info.codex_wire = Some(replacement_caps);
    let mut models = indexmap::IndexMap::new();
    models.insert("catalog-entry".to_string(), replacement);
    let mut ctx = ctx_with_parent_chat_state(
        "catalog-entry",
        "original-routing-model",
        "catalog-entry",
        models,
    );
    ctx.sampling_config.model = "original-routing-model".to_string();
    ctx.sampling_config.codex_wire = Some(baseline_caps.clone());
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.sampling_config.model = "opaque-backend-routing-hint".to_string();
    snapshot.catalog_identity = Some(test_catalog_identity(
        "catalog-entry",
        "original-routing-model",
        xai_chat_state::CatalogResolutionLineage::ExactKey,
    ));
    chat.restore_snapshot(snapshot);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "catalog-entry");
    assert_eq!(config.codex_wire, Some(baseline_caps));
}

#[tokio::test]
async fn read_parent_sampling_config_fallback_missing_id_ignores_same_route_survivor() {
    let baseline_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let mut survivor = test_model_entry("shared-routing-model");
    survivor.info.codex_wire = Some(xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    });
    let mut models = indexmap::IndexMap::new();
    models.insert("surviving-entry".to_string(), survivor);
    let mut ctx = ctx_with_parent_chat_state(
        "removed-entry",
        "shared-routing-model",
        "surviving-entry",
        models,
    );
    ctx.parent_chat_state = None;
    ctx.sampling_config.codex_wire = Some(baseline_caps.clone());

    let prepared = read_parent_prepared_model(&ctx).await;
    let config = prepared.sampling_config;

    assert_eq!(prepared.model_id.0.as_ref(), "removed-entry");
    assert_eq!(prepared.catalog_identity.model_id, "removed-entry");
    assert_eq!(prepared.catalog_identity.route, "shared-routing-model");
    assert_eq!(
        prepared.catalog_identity.lineage,
        xai_chat_state::CatalogResolutionLineage::ExactKey
    );
    assert_eq!(config.codex_wire, Some(baseline_caps));
}

/// A refreshed catalog can replace a retained catalog id (`auto`) with the
/// routing slug key. That is still the same inherited model, and the child
/// must use the refreshed entry's capabilities rather than losing them.
#[tokio::test]
async fn read_parent_sampling_config_resolves_wire_after_catalog_id_becomes_slug_key() {
    let refreshed_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let stale_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut entry = test_model_entry("grok-4.5");
    entry.info.codex_wire = Some(refreshed_caps.clone());
    let mut models = indexmap::IndexMap::new();
    models.insert("grok-4.5".to_string(), entry);
    let mut ctx = ctx_with_parent_chat_state("auto", "grok-4.5", "auto", models);
    ctx.sampling_config.model = "grok-4.5".to_string();
    ctx.sampling_config.codex_wire = Some(stale_caps);
    let mut selection_catalog = indexmap::IndexMap::new();
    selection_catalog.insert("auto".to_string(), test_model_entry("grok-4.5"));
    let selected_identity = crate::agent::models::resolve_catalog_identity(
        &selection_catalog,
        &acp::ModelId::new("grok-4.5"),
    )
    .expect("unique route selection");
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.catalog_identity = Some(selected_identity);
    chat.restore_snapshot(snapshot);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "grok-4.5");
    assert_eq!(
        config.codex_wire,
        Some(refreshed_caps),
        "a retained catalog id and its refreshed routing-slug key are the same model"
    );
}

/// An exact stable key is not an alias merely because its routing slug differs.
/// If refresh removes that key, a same-route survivor must not inherit the
/// prepared session's auth or wire capabilities.
#[tokio::test]
async fn read_parent_sampling_config_exact_stable_key_never_remaps_to_same_route_survivor() {
    let baseline_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let replacement_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut selection_catalog = indexmap::IndexMap::new();
    selection_catalog.insert(
        "prod-grok-build".to_string(),
        test_model_entry("grok-4.5"),
    );
    let selected_identity = crate::agent::models::resolve_catalog_identity(
        &selection_catalog,
        &acp::ModelId::new("prod-grok-build"),
    )
    .expect("exact production catalog selection");
    assert_eq!(
        selected_identity.lineage,
        xai_chat_state::CatalogResolutionLineage::ExactKey
    );

    let mut survivor = test_model_entry("grok-4.5");
    survivor.info.codex_wire = Some(replacement_caps);
    let mut refreshed_catalog = indexmap::IndexMap::new();
    refreshed_catalog.insert("replacement".to_string(), survivor);
    let mut ctx = ctx_with_parent_chat_state(
        "prod-grok-build",
        "grok-4.5",
        "replacement",
        refreshed_catalog,
    );
    ctx.sampling_config.codex_wire = Some(baseline_caps.clone());
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.catalog_identity = Some(selected_identity);
    chat.restore_snapshot(snapshot);

    let prepared = read_parent_prepared_model(&ctx).await;
    let config = prepared.sampling_config;
    assert_eq!(prepared.model_id.0.as_ref(), "prod-grok-build");
    assert_eq!(prepared.catalog_identity.model_id, "prod-grok-build");
    assert_eq!(prepared.catalog_identity.route, "grok-4.5");
    assert_eq!(config.codex_wire, Some(baseline_caps));
}

/// A present catalog entry with no wire metadata is authoritative. After a
/// session switch it must not fall through to the process startup model's
/// baseline capabilities.
#[tokio::test]
async fn read_parent_sampling_config_present_none_never_inherits_startup_model_wire() {
    let startup_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut models = indexmap::IndexMap::new();
    models.insert("switched-model".to_string(), test_model_entry("switched-model"));
    let mut ctx = ctx_with_parent_chat_state(
        "switched-model",
        "switched-model",
        "switched-model",
        models,
    );
    ctx.sampling_config_model_id = acp::ModelId::new("startup-model");
    ctx.sampling_config.model = "startup-model".to_string();
    ctx.sampling_config.codex_wire = Some(startup_caps);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "switched-model");
    assert_eq!(
        config.codex_wire, None,
        "a present entry with no wire metadata must not inherit the startup model's flags"
    );
}

#[tokio::test]
async fn read_parent_sampling_config_same_entry_none_overrides_stale_baseline_wire() {
    let stale_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut models = indexmap::IndexMap::new();
    models.insert(
        "same-catalog-entry".to_string(),
        test_model_entry("same-routing-model"),
    );
    let mut ctx = ctx_with_parent_chat_state(
        "same-catalog-entry",
        "same-routing-model",
        "same-catalog-entry",
        models,
    );
    ctx.sampling_config.codex_wire = Some(stale_caps);
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.catalog_identity = Some(test_catalog_identity(
        "same-catalog-entry",
        "same-routing-model",
        xai_chat_state::CatalogResolutionLineage::ExactKey,
    ));
    chat.restore_snapshot(snapshot);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "same-catalog-entry");
    assert_eq!(config.codex_wire, None);
}

/// The actor commits a switch before the outer session handle. A spawn in
/// that window must use the model identity from the live actor snapshot for
/// both routing and wire capabilities.
#[tokio::test]
async fn read_parent_sampling_config_inflight_switch_uses_live_model_wire() {
    let old_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let new_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let mut old_entry = test_model_entry("shared-routing-model");
    old_entry.info.codex_wire = Some(old_caps);
    let mut new_entry = test_model_entry("shared-routing-model");
    new_entry.info.codex_wire = Some(new_caps.clone());
    new_entry.info.supports_backend_search = true;
    let mut models = indexmap::IndexMap::new();
    models.insert("old-catalog-id".to_string(), old_entry);
    models.insert("new-catalog-id".to_string(), new_entry);
    let mut ctx = ctx_with_parent_chat_state(
        "old-catalog-id",
        "shared-routing-model",
        "old-catalog-id",
        models,
    );
    ctx.sampling_config.model = "shared-routing-model".to_string();
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.catalog_identity = Some(test_catalog_identity(
        "new-catalog-id",
        "shared-routing-model",
        xai_chat_state::CatalogResolutionLineage::ExactKey,
    ));
    chat.restore_snapshot(snapshot);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(config.model, "shared-routing-model");
    assert_eq!(model_id.0.as_ref(), "new-catalog-id");
    assert_eq!(
        config.codex_wire,
        Some(new_caps),
        "wire flags must be resolved from the live model snapshot, not the stale session handle"
    );
    assert!(config.supports_backend_search);
}

#[tokio::test]
async fn read_parent_sampling_config_reused_catalog_key_keeps_sampled_model_wire() {
    let old_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(false),
        ..Default::default()
    };
    let replacement_caps = xai_grok_sampling_types::CodexWireCapabilities {
        supports_reasoning_summary_parameter: Some(true),
        ..Default::default()
    };
    let mut replacement = test_model_entry("replacement-routing-model");
    replacement.info.codex_wire = Some(replacement_caps);
    let mut old = test_model_entry("old-routing-model");
    old.info.codex_wire = Some(old_caps.clone());
    let mut models = indexmap::IndexMap::new();
    models.insert("auto".to_string(), replacement.clone());
    // Exact-key shadow: the sampled routing slug is itself a key for an
    // unrelated entry. Route resolution must find `legacy-entry` instead.
    models.insert("old-routing-model".to_string(), replacement);
    models.insert("legacy-entry".to_string(), old);
    let mut ctx = ctx_with_parent_chat_state("auto", "old-routing-model", "auto", models);
    ctx.sampling_config.codex_wire = Some(old_caps.clone());
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.catalog_identity = Some(test_catalog_identity(
        "auto",
        "old-routing-model",
        xai_chat_state::CatalogResolutionLineage::ExactKey,
    ));
    chat.restore_snapshot(snapshot);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "auto");
    assert_eq!(config.model, "old-routing-model");
    assert_eq!(config.codex_wire, Some(old_caps));

    let mut baseline_snapshot = chat.snapshot().await.expect("chat snapshot");
    baseline_snapshot.catalog_identity = None;
    chat.restore_snapshot(baseline_snapshot);
    let (baseline_config, baseline_model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(baseline_model_id.0.as_ref(), "legacy-entry");
    assert_eq!(baseline_config.codex_wire, config.codex_wire);
}

#[tokio::test]
async fn read_parent_sampling_config_resolves_compactions_remaining_from_catalog() {
    use xai_grok_sampling_types::CompactionsRemaining;
    let mut entry = test_model_entry("grok-4.5");
    entry.info.compactions_remaining = Some(CompactionsRemaining::Dynamic(true));
    let mut models = indexmap::IndexMap::new();
    models.insert("auto".to_string(), entry);
    let mut ctx = ctx_with_parent_chat_state("auto", "grok-4.5", "auto", models);
    ctx.sampling_config.compactions_remaining = None;
    let (config, _model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(
            config.compactions_remaining,
            Some(CompactionsRemaining::Dynamic(true)),
            "subagent should inherit compactions-remaining capability from the live model catalog"
        );
}
#[tokio::test]
async fn read_parent_sampling_config_fallback_resolves_compactions_remaining_from_catalog() {
    use xai_grok_sampling_types::CompactionsRemaining;
    let mut entry = test_model_entry("composer-2-fast");
    entry.info.compactions_remaining = Some(CompactionsRemaining::Dynamic(true));
    let mut models = indexmap::IndexMap::new();
    models.insert("composer-2-fast".to_string(), entry);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("composer-2-fast");
    ctx.sampling_config_model_id = acp::ModelId::new("composer-2-fast");
    ctx.parent_chat_state = None;
    ctx.sampling_config.model = "composer-2-fast".to_string();
    ctx.sampling_config.compactions_remaining = None;
    ctx.available_models = models.clone();
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        models,
        acp::ModelId::new("composer-2-fast"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(model_id.0.as_ref(), "composer-2-fast");
    assert_eq!(
            config.compactions_remaining,
            Some(CompactionsRemaining::Dynamic(true)),
            "fallback path should also resolve compactions-remaining capability from the catalog"
        );
}
#[tokio::test]
async fn read_parent_sampling_config_strips_api_key_for_auth_scheme_none() {
    use xai_grok_sampler::AuthScheme;
    let inference_slug = "local-none-model";
    let chat_state = spawn_test_parent_chat_state(inference_slug);
    chat_state.update_credentials(xai_chat_state::Credentials::bound(
        Some("stale-session-jwt".to_string()),
        xai_chat_state::AuthType::SessionToken,
        xai_grok_sampler::CredentialSource::XaiSession,
    ));
    let mut models = indexmap::IndexMap::new();
    let mut entry = test_model_entry(inference_slug);
    entry.info.auth_scheme = AuthScheme::None;
    models.insert("local-none".to_string(), entry);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("local-none");
    ctx.parent_chat_state = Some(chat_state);
    ctx.sampling_config.auth_scheme = AuthScheme::None;
    ctx.available_models = models.clone();
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        models,
        acp::ModelId::new("auto"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, _model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(config.auth_scheme, AuthScheme::None);
    assert!(
        config.api_key.is_none(),
        "stale chat_state JWT must not inherit onto AuthScheme::None subagent"
    );
}
#[tokio::test]
async fn read_parent_sampling_config_prefers_catalog_key_over_shared_wire_slug() {
    use xai_grok_sampler::AuthScheme;
    let shared_slug = "shared-routing-slug";
    let chat_state = spawn_test_parent_chat_state(shared_slug);
    chat_state.update_credentials(xai_chat_state::Credentials::bound(
        Some("stale-session-jwt".to_string()),
        xai_chat_state::AuthType::SessionToken,
        xai_grok_sampler::CredentialSource::XaiSession,
    ));
    let mut models = indexmap::IndexMap::new();
    let mut bearer_entry = test_model_entry(shared_slug);
    bearer_entry.info.auth_scheme = AuthScheme::Bearer;
    models.insert("builtin-bearer".to_string(), bearer_entry);
    let mut none_entry = test_model_entry(shared_slug);
    none_entry.info.auth_scheme = AuthScheme::None;
    models.insert("none-alias".to_string(), none_entry);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("none-alias");
    ctx.parent_chat_state = Some(chat_state);
    ctx.sampling_config.auth_scheme = AuthScheme::None;
    ctx.available_models = models.clone();
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        models,
        acp::ModelId::new("auto"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(model_id.0.as_ref(), "none-alias");
    assert_eq!(config.auth_scheme, AuthScheme::None);
    assert!(
        config.api_key.is_none(),
        "catalog None alias must win over the shared-slug Bearer entry"
    );
}

/// Capability and auth facts must come from the same catalog entry. An exact
/// key equal to the routing slug must not shadow the selected entry's BYOK
/// classification and suppress its xAI session resolver.
#[tokio::test]
async fn read_parent_sampling_config_binds_bearer_gate_to_selected_catalog_entry() {
    let shared_slug = "shared-routing-slug";
    let mut selected = test_model_entry(shared_slug);
    selected.info.auth_scheme = xai_grok_sampler::AuthScheme::Bearer;
    let mut shadow = test_model_entry("unrelated-routing-slug");
    shadow.api_key = Some("shadow-byok-key".to_string());
    let mut models = indexmap::IndexMap::new();
    models.insert("selected-entry".to_string(), selected);
    models.insert(shared_slug.to_string(), shadow);
    let mut ctx = ctx_with_parent_chat_state(
        "selected-entry",
        shared_slug,
        "selected-entry",
        models,
    );
    ctx.auth_method_id = acp::AuthMethodId::new(
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
    );
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    let mut snapshot = chat.snapshot().await.expect("chat snapshot");
    snapshot.sampling_config.base_url = "https://api.x.ai/v1".to_string();
    chat.restore_snapshot(snapshot);

    let (config, model_id) = read_parent_sampling_config(&ctx).await;

    assert_eq!(model_id.0.as_ref(), "selected-entry");
    assert!(
        config.bearer_resolver.is_some(),
        "the unrelated exact-key BYOK entry must not shadow selected-entry auth facts"
    );
}

#[tokio::test]
async fn read_parent_sampling_config_catalog_miss_respects_parent_auth_scheme_none() {
    use xai_grok_sampler::AuthScheme;
    let inference_slug = "not-in-effective-catalog-xyz";
    let chat_state = spawn_test_parent_chat_state(inference_slug);
    chat_state.update_credentials(xai_chat_state::Credentials::bound(
        Some("stale-session-jwt".to_string()),
        xai_chat_state::AuthType::SessionToken,
        xai_grok_sampler::CredentialSource::XaiSession,
    ));
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("no-catalog-model");
    ctx.parent_chat_state = Some(chat_state);
    ctx.sampling_config.auth_scheme = AuthScheme::None;
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        indexmap::IndexMap::new(),
        acp::ModelId::new("auto"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, _model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(
        config.auth_scheme,
        AuthScheme::None,
        "catalog miss must not silently default to Bearer when parent baseline is None"
    );
    assert!(
        config.api_key.is_none(),
        "stale chat_state JWT must not survive catalog miss on AuthScheme::None parent"
    );
}
#[tokio::test]
async fn read_parent_sampling_config_available_models_none_strips_with_bearer_parent_baseline() {
    use xai_grok_sampler::AuthScheme;
    let inference_slug = "local-none-resolved";
    let chat_state = spawn_test_parent_chat_state(inference_slug);
    chat_state.update_credentials(xai_chat_state::Credentials::bound(
        Some("stale-session-jwt".to_string()),
        xai_chat_state::AuthType::SessionToken,
        xai_grok_sampler::CredentialSource::XaiSession,
    ));
    let mut models = indexmap::IndexMap::new();
    let mut entry = test_model_entry(inference_slug);
    entry.info.auth_scheme = AuthScheme::None;
    models.insert("local-none".to_string(), entry);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("local-none");
    ctx.parent_chat_state = Some(chat_state);
    // Parent spawn baseline is still Bearer (agent-level snapshot).
    ctx.sampling_config.auth_scheme = AuthScheme::Bearer;
    ctx.sampling_config.api_key = Some("parent-bearer-must-not-win".to_string());
    ctx.available_models = models.clone();
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        models,
        acp::ModelId::new("auto"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, _model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(
        config.auth_scheme,
        AuthScheme::None,
        "available_models auth_scheme must win over parent Bearer baseline"
    );
    assert!(
        config.api_key.is_none(),
        "stale JWT must strip when available_models resolves AuthScheme::None"
    );
}
#[tokio::test]
async fn read_parent_sampling_config_stale_none_baseline_keeps_bearer_from_catalog() {
    use xai_grok_sampler::AuthScheme;
    let inference_slug = "switched-bearer-model";
    let chat_state = spawn_test_parent_chat_state(inference_slug);
    chat_state.update_credentials(xai_chat_state::Credentials::bound(
        Some("live-session-bearer".to_string()),
        xai_chat_state::AuthType::SessionToken,
        xai_grok_sampler::CredentialSource::XaiSession,
    ));
    let mut models = indexmap::IndexMap::new();
    let mut entry = test_model_entry(inference_slug);
    entry.info.auth_scheme = AuthScheme::Bearer;
    models.insert("bearer-model".to_string(), entry);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("bearer-model");
    ctx.parent_chat_state = Some(chat_state);
    // Stale agent-level snapshot from startup on a None model.
    ctx.sampling_config.auth_scheme = AuthScheme::None;
    ctx.available_models = models.clone();
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        models,
        acp::ModelId::new("auto"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, _model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(
        config.auth_scheme,
        AuthScheme::Bearer,
        "catalog Bearer must win over stale None parent baseline"
    );
    assert_eq!(
        config.api_key.as_deref(),
        Some("live-session-bearer"),
        "child must inherit live credentials when catalog resolves Bearer"
    );
}
#[tokio::test]
async fn read_parent_sampling_config_stale_none_baseline_keeps_x_api_key_from_catalog() {
    use xai_grok_sampler::AuthScheme;
    let inference_slug = "switched-x-api-key-model";
    let chat_state = spawn_test_parent_chat_state(inference_slug);
    chat_state.update_credentials(xai_chat_state::Credentials::bound(
        Some("live-byok-key".to_string()),
        xai_chat_state::AuthType::ApiKey,
        xai_grok_sampler::CredentialSource::ModelApiKey,
    ));
    let mut models = indexmap::IndexMap::new();
    let mut entry = test_model_entry(inference_slug);
    entry.info.auth_scheme = AuthScheme::XApiKey;
    models.insert("x-api-key-model".to_string(), entry);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.model_id = acp::ModelId::new("x-api-key-model");
    ctx.parent_chat_state = Some(chat_state);
    ctx.sampling_config.auth_scheme = AuthScheme::None;
    ctx.available_models = models.clone();
    ctx.models_manager = crate::agent::models::ModelsManager::new(
        None,
        models,
        acp::ModelId::new("auto"),
        ctx.auth_manager.clone(),
        crate::agent::config::Config::default(),
    );
    let (config, _model_id) = read_parent_sampling_config(&ctx).await;
    assert_eq!(
        config.auth_scheme,
        AuthScheme::XApiKey,
        "catalog XApiKey must win over stale None parent baseline"
    );
    assert_eq!(
        config.api_key.as_deref(),
        Some("live-byok-key"),
        "child must inherit live credentials when catalog resolves XApiKey"
    );
}
/// Drive the REAL precedence path
/// (`resolve_effective_model_config`, which `run_shell_child`
/// calls) with BOTH an explicit `runtime_override_model` AND a
/// `[subagents.models]` pin for the same agent present, asserting the
/// runtime override wins; with `None` (inherit) the pin wins (precedence
/// handed back); and an unknown override falls through to the pin.
#[tokio::test]
async fn runtime_override_wins_over_subagents_models_pin_in_precedence_path() {
    use xai_grok_agent::config::ModelOverride;
    let build_ctx = || {
        let mut models = indexmap::IndexMap::new();
        models.insert("goal-model".to_string(), test_model_entry("goal-model"));
        models.insert("pinned-model".to_string(), test_model_entry("pinned-model"));
        let mut ctx = ctx_with_toggle(HashMap::new());
        ctx.available_models = models;
        ctx.subagent_model_overrides = HashMap::from([
            ("explore".to_string(), "pinned-model".to_string()),
        ]);
        ctx
    };
    let ctx = build_ctx();
    let (config, model_id) = resolve_effective_model_config(
            Some("goal-model"),
            "explore",
            &ModelOverride::Inherit,
            &ctx,
        )
        .await;
    assert_eq!(
            config.model, "goal-model",
            "the goal runtime override must win over the `[subagents.models]` pin",
        );
    assert_eq!(model_id.0.as_ref(), "goal-model");
    let ctx = build_ctx();
    let (config, model_id) = resolve_effective_model_config(
            None,
            "explore",
            &ModelOverride::Inherit,
            &ctx,
        )
        .await;
    assert_eq!(
            config.model, "pinned-model",
            "with no runtime override, the `[subagents.models]` pin wins",
        );
    assert_eq!(model_id.0.as_ref(), "pinned-model");
    let ctx = build_ctx();
    let (config, _) = resolve_effective_model_config(
            Some("does-not-exist"),
            "explore",
            &ModelOverride::Inherit,
            &ctx,
        )
        .await;
    assert_eq!(
            config.model, "pinned-model",
            "an unknown override falls through to the pin",
        );
}

#[tokio::test]
async fn forked_request_never_falls_through_missing_parent_key_to_model_pins() {
    use xai_grok_agent::config::ModelOverride;

    let mut models = indexmap::IndexMap::new();
    for key in ["runtime-pin", "config-pin", "definition-pin"] {
        models.insert(key.to_string(), test_model_entry(key));
    }
    let mut ctx = ctx_with_parent_chat_state(
        "removed-parent-key",
        "parent-routing-model",
        "runtime-pin",
        models,
    );
    ctx.subagent_model_overrides
        .insert("explore".to_string(), "config-pin".to_string());
    let chat = ctx.parent_chat_state.as_ref().expect("chat state");
    chat.update_credentials(xai_chat_state::Credentials::bound(
        Some("parent-model-key".to_string()),
        xai_chat_state::AuthType::ApiKey,
        xai_grok_sampler::CredentialSource::ModelApiKey,
    ));

    let prepared = resolve_request_prepared_model(
        true,
        Some("runtime-pin"),
        "explore",
        &ModelOverride::Override("definition-pin".to_string()),
        &ctx,
    )
    .await;

    assert_eq!(prepared.model_id.0.as_ref(), "removed-parent-key");
    assert_eq!(prepared.sampling_config.model, "parent-routing-model");
    assert_eq!(
        prepared.sampling_config.api_key.as_deref(),
        Some("parent-model-key")
    );
    assert!(
        prepared.agent_type.is_empty(),
        "the spawn path must fail closed when the removed parent key has no live harness facts"
    );
}
/// A `fork_context = true` spawn must infer on the parent session model
/// (`ctx.model_id`) for per-model radix reuse, even when a
/// `[subagents.models]` pin and an `AgentDefinition.model` override are
/// both present. `run_shell_child` forces
/// `effective_runtime.model = Some(ctx.model_id)` on the fork path after
/// other override sources; the runtime override wins in
/// `resolve_effective_model_config`.
#[tokio::test]
async fn fork_context_pins_parent_model_over_overrides() {
    use xai_grok_agent::config::ModelOverride;
    let build_ctx = || {
        let mut ctx = ctx_with_toggle(HashMap::new());
        ctx.sampling_config.model = "parent-model".to_string();
        ctx.model_id = acp::ModelId::new("parent-model");
        ctx.available_models
            .insert("parent-model".to_string(), test_model_entry("parent-model"));
        ctx.available_models
            .insert("pinned-model".to_string(), test_model_entry("pinned-model"));
        ctx.available_models
            .insert("agentdef-model".to_string(), test_model_entry("agentdef-model"));
        ctx.subagent_model_overrides
            .insert("general-purpose".to_string(), "pinned-model".to_string());
        ctx
    };
    let agent_def = ModelOverride::Override("agentdef-model".to_string());
    let ctx = build_ctx();
    let fork_context = true;
    let mut runtime_override: Option<String> = None;
    if fork_context {
        runtime_override = Some(ctx.model_id.0.to_string());
    }
    let (config, model_id) = resolve_effective_model_config(
            runtime_override.as_deref(),
            "general-purpose",
            &agent_def,
            &ctx,
        )
        .await;
    assert_eq!(
            config.model, "parent-model",
            "fork_context must pin the parent model over the [subagents.models] pin and agent-def override",
        );
    assert_eq!(model_id.0.as_ref(), "parent-model");
    let ctx = build_ctx();
    let (config, model_id) = resolve_effective_model_config(
            None,
            "general-purpose",
            &agent_def,
            &ctx,
        )
        .await;
    assert_eq!(
            config.model, "pinned-model",
            "without the fork pin the [subagents.models] override wins",
        );
    assert_eq!(model_id.0.as_ref(), "pinned-model");
}
/// With no explicit pin, the subagent inherits the parent model for any
/// parent model, with no special-casing (a "heavy"/custom parent
/// is treated identically to any other).
#[tokio::test]
async fn resolve_subagent_inherits_parent_model_without_pins() {
    use xai_grok_agent::config::ModelOverride;
    for parent_model in ["grok-4.5", "composer-2-fast", "my-custom-byok-model"] {
        let mut ctx = ctx_with_toggle(HashMap::new());
        ctx.sampling_config.model = parent_model.to_string();
        ctx.model_id = acp::ModelId::new(parent_model);
        ctx.sampling_config_model_id = acp::ModelId::new(parent_model);
        let (config, model_id) = resolve_subagent_sampling_config(
                "explore",
                &ModelOverride::Inherit,
                &ctx,
            )
            .await;
        assert_eq!(
                config.model, parent_model,
                "subagent must inherit parent model {parent_model:?} when no pin is set",
            );
        assert_eq!(model_id.0.as_ref(), parent_model);
    }
}
/// An explicit `[subagents.models]` pin routes the subagent to that
/// model regardless of the parent model — both a light parent
/// (`grok-4.5`) and a custom parent (`composer-2-fast`)
/// honor the pin identically now that the heavy-model gate is gone.
#[tokio::test]
async fn resolve_subagent_config_override_pin_applies_for_any_parent() {
    use xai_grok_agent::config::ModelOverride;
    for parent_model in ["grok-4.5", "composer-2-fast"] {
        let mut ctx = ctx_with_toggle(HashMap::new());
        ctx.sampling_config.model = parent_model.to_string();
        ctx.model_id = acp::ModelId::new(parent_model);
        ctx.available_models
            .insert("pinned-model".to_string(), test_model_entry("pinned-model"));
        ctx.subagent_model_overrides
            .insert("explore".to_string(), "pinned-model".to_string());
        let (config, model_id) = resolve_subagent_sampling_config(
                "explore",
                &ModelOverride::Inherit,
                &ctx,
            )
            .await;
        assert_eq!(
                config.model, "pinned-model",
                "config pin must win for parent {parent_model:?}",
            );
        assert_eq!(model_id.0.as_ref(), "pinned-model");
    }
}
/// An explicit `AgentDefinition.model = Override(id)` pin routes the
/// subagent to that model even when the parent runs a light model.
#[tokio::test]
async fn resolve_subagent_agent_definition_pin_applies_for_light_parent() {
    use xai_grok_agent::config::ModelOverride;
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.sampling_config.model = "grok-4.5".to_string();
    ctx.model_id = acp::ModelId::new("grok-4.5");
    ctx.available_models
        .insert("pinned-model".to_string(), test_model_entry("pinned-model"));
    let agent_model = ModelOverride::Override("pinned-model".to_string());
    let (config, model_id) = resolve_subagent_sampling_config(
            "explore",
            &agent_model,
            &ctx,
        )
        .await;
    assert_eq!(config.model, "pinned-model");
    assert_eq!(model_id.0.as_ref(), "pinned-model");
}
/// Priority 1 (`[subagents.models]`) wins over Priority 2
/// (`AgentDefinition.model`) when both pins are set and both resolve.
#[tokio::test]
async fn resolve_subagent_config_override_wins_over_agent_definition() {
    use xai_grok_agent::config::ModelOverride;
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.sampling_config.model = "grok-4.5".to_string();
    ctx.model_id = acp::ModelId::new("grok-4.5");
    ctx.available_models
        .insert("config-pin".to_string(), test_model_entry("config-pin"));
    ctx.available_models
        .insert("agentdef-pin".to_string(), test_model_entry("agentdef-pin"));
    ctx.subagent_model_overrides.insert("explore".to_string(), "config-pin".to_string());
    let agent_model = ModelOverride::Override("agentdef-pin".to_string());
    let (config, model_id) = resolve_subagent_sampling_config(
            "explore",
            &agent_model,
            &ctx,
        )
        .await;
    assert_eq!(config.model, "config-pin");
    assert_eq!(model_id.0.as_ref(), "config-pin");
}
/// An unresolvable `[subagents.models]` pin (model absent from
/// `available_models`) falls through to inherit the parent model.
#[tokio::test]
async fn resolve_subagent_config_override_unknown_model_falls_through_to_inherit() {
    use xai_grok_agent::config::ModelOverride;
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.sampling_config.model = "grok-4.5".to_string();
    ctx.model_id = acp::ModelId::new("grok-4.5");
    ctx.sampling_config_model_id = acp::ModelId::new("grok-4.5");
    ctx.subagent_model_overrides
        .insert("explore".to_string(), "does-not-exist".to_string());
    let (config, model_id) = resolve_subagent_sampling_config(
            "explore",
            &ModelOverride::Inherit,
            &ctx,
        )
        .await;
    assert_eq!(config.model, "grok-4.5");
    assert_eq!(model_id.0.as_ref(), "grok-4.5");
}
/// An unresolvable `AgentDefinition.model` pin (model absent from
/// `available_models`) falls through to inherit the parent model.
#[tokio::test]
async fn resolve_subagent_agent_definition_unknown_model_falls_through_to_inherit() {
    use xai_grok_agent::config::ModelOverride;
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.sampling_config.model = "grok-4.5".to_string();
    ctx.model_id = acp::ModelId::new("grok-4.5");
    ctx.sampling_config_model_id = acp::ModelId::new("grok-4.5");
    let agent_model = ModelOverride::Override("does-not-exist".to_string());
    let (config, model_id) = resolve_subagent_sampling_config(
            "explore",
            &agent_model,
            &ctx,
        )
        .await;
    assert_eq!(config.model, "grok-4.5");
    assert_eq!(model_id.0.as_ref(), "grok-4.5");
}
/// Spawn-time credentials are cache-only: a cold spawn has no key,
/// never the parent session key.
#[tokio::test]
async fn subagent_override_provider_model_spawns_cache_only_credentials() {
    use xai_grok_agent::config::ModelOverride;
    let dir = tempfile::tempdir().unwrap();
    let provider = crate::auth::test_counting_provider(
        "test-subagent-spawn",
        dir.path(),
    );
    let mut entry = test_model_entry("proxied-model");
    entry.info.base_url = "https://gateway.example/v1".to_string();
    entry.auth_provider = Some(provider.clone());
    let mut models = indexmap::IndexMap::new();
    models.insert("proxied".to_string(), entry);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.sampling_config.model = "grok-4.5".to_string();
    ctx.model_id = acp::ModelId::new("grok-4.5");
    ctx.available_models = models;
    ctx.auth = Some(crate::auth::GrokAuth {
        key: "parent-session-jwt".to_string(),
        ..Default::default()
    });
    ctx.subagent_model_overrides.insert("explore".to_string(), "proxied".to_string());
    let (config, model_id) = resolve_subagent_sampling_config(
            "explore",
            &ModelOverride::Inherit,
            &ctx,
        )
        .await;
    assert_eq!(model_id.0.as_ref(), "proxied");
    assert_eq!(
            config.api_key, None,
            "a cold cache spawns with no key, never the parent session key"
        );
    provider.ensure_fresh_token(None).await.rotated().unwrap();
    let (config, _) = resolve_subagent_sampling_config(
            "explore",
            &ModelOverride::Inherit,
            &ctx,
        )
        .await;
    assert_eq!(config.api_key.as_deref(), Some("tok-1"));
    assert_eq!(config.base_url, "https://gateway.example/v1");
}
#[test]
fn subagent_resolution_diagnostics_never_emit_parent_or_child_credentials() {
    // Keep the sentinel high-entropy: embedding diagnostic field names such as
    // `credential` would make an 8-byte-window assertion match the safe JSON
    // key rather than leaked secret material.
    let parent_secret = "GB002P-Q7w5E3r1T9y7Z6x4C2v8";
    let child_secret = "GB002C-A7s5D3f1G9h7J6k4L2m8";
    let parent = xai_grok_sampler::SamplerConfig {
        model: "parent-model".to_string(),
        base_url: "https://api.x.ai/v1".to_string(),
        api_key: Some(parent_secret.to_string()),
        ..xai_grok_sampler::SamplerConfig::default()
    };
    let child = xai_grok_sampler::SamplerConfig {
        model: "child-model".to_string(),
        base_url: "https://provider.example/v1".to_string(),
        api_key: Some(child_secret.to_string()),
        ..xai_grok_sampler::SamplerConfig::default()
    };

    let context = subagent_model_resolution_context(
        "executor",
        "config_override",
        &child,
        &acp::ModelId::new("child-model"),
        &parent,
    );
    assert_eq!(context["child_credential_present"], true);
    assert_eq!(context["parent_credential_present"], true);
    assert_eq!(context["keys_match"], false);
    let rendered = context.to_string();
    for secret in [parent_secret, child_secret] {
        assert!(!rendered.contains(secret));
        for window in secret.as_bytes().windows(8) {
            assert!(!rendered.contains(std::str::from_utf8(window).unwrap()));
        }
    }
}
#[test]
fn non_cursor_persona_injected_as_system_reminder() {
    use xai_grok_sampling_types::conversation::{ConversationItem, SyntheticReason};
    let persona = "You are a pragmatic implementer.";
    let mut conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("task"),
        ];
    let mut prefix_len: usize = 2;
    let reminder = ConversationItem::system_reminder(
        format!(
            "<system-reminder>\n{persona}\n</system-reminder>"
        ),
    );
    let insert_at = prefix_len.min(conv.len());
    conv.insert(insert_at, reminder);
    prefix_len += 1;
    assert_eq!(conv.len(), 3, "conversation should have 3 items");
    assert_eq!(prefix_len, 3, "prefix_len should be incremented");
    if let ConversationItem::User(ref u) = conv[2] {
        assert_eq!(u.synthetic_reason, Some(SyntheticReason::SystemReminder));
        let text = u
            .content
            .first()
            .map(|c| match c {
                xai_grok_sampling_types::conversation::ContentPart::Text { text } => {
                    text.as_ref()
                }
                _ => "",
            });
        assert!(
                text.unwrap_or("").contains("<system-reminder>"),
                "should use hyphen tag format"
            );
        assert!(
                text.unwrap_or("").contains(persona),
                "should contain the persona instructions"
            );
    } else {
        panic!("expected User variant for system_reminder");
    }
}
#[test]
fn persona_injection_skipped_for_resumed() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let persona_instructions = Some("Be thorough.".to_string());
    let context_source = InitialContextSource::Resumed;
    let mut conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("old turn"),
        ];
    let original_len = conv.len();
    let mut prefix_len = original_len;
    if context_source != InitialContextSource::Resumed
        && let Some(ref pi) = persona_instructions
    {
        let reminder = ConversationItem::system_reminder(
            format!(
                "<system-reminder>\n{pi}\n</system-reminder>"
            ),
        );
        let insert_at = prefix_len.min(conv.len());
        conv.insert(insert_at, reminder);
        prefix_len += 1;
    }
    assert_eq!(
            conv.len(),
            original_len,
            "resumed session should not get persona injected"
        );
    assert_eq!(prefix_len, original_len, "prefix_len should be unchanged");
}
#[test]
fn persona_injection_into_empty_conversation() {
    use xai_grok_sampling_types::conversation::ConversationItem;
    let mut conv: Vec<ConversationItem> = vec![];
    let mut prefix_len: usize = 0;
    let reminder = ConversationItem::system_reminder(
        "<system-reminder>\nDo X.\n</system-reminder>".to_string(),
    );
    let insert_at = prefix_len.min(conv.len());
    conv.insert(insert_at, reminder);
    prefix_len += 1;
    assert_eq!(conv.len(), 1);
    assert_eq!(prefix_len, 1);
    assert!(matches!(& conv[0], ConversationItem::User(_)));
}
mod cancellation_error_message_tests {
    use super::super::cancellation_error_message;
    use crate::session::commands::CancellationContext;
    use xai_file_utils::events::types::CancellationCategory;
    #[test]
    fn permission_rejected_with_context() {
        let ctx = CancellationContext {
            tool_name: Some("run_terminal_cmd".into()),
            reason: Some("User rejected the execution".into()),
            ..Default::default()
        };
        let msg = cancellation_error_message(
            Some(CancellationCategory::PermissionRejected),
            Some(&ctx),
        );
        assert!(msg.contains("user rejected permission"));
        assert!(msg.contains("run_terminal_cmd"));
        assert!(msg.contains("User rejected the execution"));
    }
    #[test]
    fn permission_rejected_without_context() {
        let msg = cancellation_error_message(
            Some(CancellationCategory::PermissionRejected),
            None,
        );
        assert!(msg.contains("user rejected a permission prompt"));
    }
    #[test]
    fn permission_cancelled() {
        let msg = cancellation_error_message(
            Some(CancellationCategory::PermissionCancelled),
            None,
        );
        assert!(msg.contains("user cancelled a permission prompt"));
    }
    #[test]
    fn hook_denied_with_context() {
        let ctx = CancellationContext {
            tool_name: Some("run_terminal_cmd".into()),
            reason: Some("blocked by policy".into()),
            hook_name: Some("safe-shell-guard".into()),
            ..Default::default()
        };
        let msg = cancellation_error_message(
            Some(CancellationCategory::HookDenied),
            Some(&ctx),
        );
        assert!(msg.contains("hook denied"));
        assert!(msg.contains("safe-shell-guard"));
        assert!(msg.contains("run_terminal_cmd"));
    }
    #[test]
    fn hook_denied_without_context() {
        let msg = cancellation_error_message(
            Some(CancellationCategory::HookDenied),
            None,
        );
        assert!(msg.contains("blocked by a hook"));
    }
    #[test]
    fn mid_turn_abort() {
        let msg = cancellation_error_message(
            Some(CancellationCategory::MidTurnAbort),
            None,
        );
        assert!(msg.contains("aborted mid-turn"));
    }
    #[test]
    fn no_category_no_context() {
        let msg = cancellation_error_message(None, None);
        assert_eq!(msg, "Subagent turn was cancelled");
    }
    #[test]
    fn partial_context_only_tool_name() {
        let ctx = CancellationContext {
            tool_name: Some("search_replace".into()),
            ..Default::default()
        };
        let msg = cancellation_error_message(
            Some(CancellationCategory::PermissionRejected),
            Some(&ctx),
        );
        assert!(msg.contains("search_replace"));
    }
    #[test]
    fn empty_context_falls_back() {
        let ctx = CancellationContext::default();
        let msg = cancellation_error_message(
            Some(CancellationCategory::PermissionRejected),
            Some(&ctx),
        );
        assert!(msg.contains("user rejected a permission prompt"));
    }
}
fn make_pool(names: &[&str]) -> crate::session::mcp_servers::SharedMcpPool {
    use crate::session::mcp_servers::{McpClient, McpState, SharedMcpPool};
    let mut state = McpState::new(vec![]);
    for &name in names {
        state.owned_clients.insert(name.to_string(), Arc::new(McpClient::stub(name)));
    }
    SharedMcpPool::from_state(&state)
}
fn pool_names(pool: &crate::session::mcp_servers::SharedMcpPool) -> Vec<String> {
    let mut names: Vec<String> = pool.server_names().map(str::to_string).collect();
    names.sort();
    names
}
#[test]
fn filter_inheritance_all_passes_everything_through() {
    let pool = make_pool(&["github", "linear", "slack"]);
    let result = super::filter_pool_by_inheritance(
        pool,
        &xai_grok_agent::config::McpInheritance::All,
    );
    let result = result.expect("All should return Some");
    assert_eq!(pool_names(&result), vec!["github", "linear", "slack"]);
}
#[test]
fn filter_inheritance_none_returns_none() {
    let pool = make_pool(&["github", "linear"]);
    let result = super::filter_pool_by_inheritance(
        pool,
        &xai_grok_agent::config::McpInheritance::None,
    );
    assert!(result.is_none());
}
#[test]
fn filter_inheritance_named_selects_specific_servers() {
    let pool = make_pool(&["github", "linear", "slack", "jira"]);
    let result = super::filter_pool_by_inheritance(
        pool,
        &xai_grok_agent::config::McpInheritance::Named(
            vec!["github".into(), "slack".into()],
        ),
    );
    let result = result.expect("Named should return Some");
    assert_eq!(pool_names(&result), vec!["github", "slack"]);
}
#[test]
fn filter_inheritance_except_excludes_specific_servers() {
    let pool = make_pool(&["github", "linear", "slack", "jira"]);
    let result = super::filter_pool_by_inheritance(
        pool,
        &xai_grok_agent::config::McpInheritance::Except(
            vec!["linear".into(), "jira".into()],
        ),
    );
    let result = result.expect("Except should return Some");
    assert_eq!(pool_names(&result), vec!["github", "slack"]);
}
#[test]
fn filter_inheritance_named_empty_list_gives_empty_pool() {
    let pool = make_pool(&["github", "linear"]);
    let result = super::filter_pool_by_inheritance(
        pool,
        &xai_grok_agent::config::McpInheritance::Named(vec![]),
    );
    let result = result.expect("Named([]) should return Some (empty pool)");
    assert_eq!(result.server_names().count(), 0);
}
#[test]
fn filter_inheritance_except_empty_list_keeps_all() {
    let pool = make_pool(&["github", "linear"]);
    let result = super::filter_pool_by_inheritance(
        pool,
        &xai_grok_agent::config::McpInheritance::Except(vec![]),
    );
    let result = result.expect("Except([]) should return Some");
    assert_eq!(pool_names(&result), vec!["github", "linear"]);
}
#[test]
fn filter_inheritance_named_nonexistent_servers_ignored() {
    let pool = make_pool(&["github", "linear"]);
    let result = super::filter_pool_by_inheritance(
        pool,
        &xai_grok_agent::config::McpInheritance::Named(
            vec![
                "nonexistent".into(),
                "github".into(),
            ],
        ),
    );
    let result = result.expect("Named should return Some");
    assert_eq!(pool_names(&result), vec!["github"]);
}
#[test]
fn filter_inheritance_except_nonexistent_servers_ignored() {
    let pool = make_pool(&["github", "linear"]);
    let result = super::filter_pool_by_inheritance(
        pool,
        &xai_grok_agent::config::McpInheritance::Except(vec!["nonexistent".into()]),
    );
    let result = result.expect("Except should return Some");
    assert_eq!(pool_names(&result), vec!["github", "linear"]);
}
#[test]
fn filter_inheritance_named_all_nonexistent_gives_empty() {
    let pool = make_pool(&["github", "linear"]);
    let result = super::filter_pool_by_inheritance(
        pool,
        &xai_grok_agent::config::McpInheritance::Named(vec!["foo".into(), "bar".into()]),
    );
    let result = result.expect("Named should return Some");
    assert_eq!(result.server_names().count(), 0);
}
#[test]
fn filter_inheritance_except_all_servers_gives_empty() {
    let pool = make_pool(&["github", "linear"]);
    let result = super::filter_pool_by_inheritance(
        pool,
        &xai_grok_agent::config::McpInheritance::Except(
            vec!["github".into(), "linear".into()],
        ),
    );
    let result = result.expect("Except should return Some");
    assert_eq!(result.server_names().count(), 0);
}
#[test]
fn resolve_inherited_pool_all_passes_parent_pool() {
    let pool = make_pool(&["github", "atlassian"]);
    let result = super::resolve_inherited_mcp_pool(
            Some(pool),
            &xai_grok_agent::config::McpInheritance::All,
        )
        .expect("All should return Some");
    assert_eq!(pool_names(&result), vec!["atlassian", "github"]);
}
#[test]
fn resolve_inherited_pool_none_returns_none() {
    let pool = make_pool(&["github", "atlassian"]);
    let result = super::resolve_inherited_mcp_pool(
        Some(pool),
        &xai_grok_agent::config::McpInheritance::None,
    );
    assert!(result.is_none());
}
#[test]
fn resolve_inherited_pool_named_filters() {
    let pool = make_pool(&["github", "atlassian", "slack"]);
    let result = super::resolve_inherited_mcp_pool(
            Some(pool),
            &xai_grok_agent::config::McpInheritance::Named(vec!["atlassian".into()]),
        )
        .expect("Named should return Some");
    assert_eq!(pool_names(&result), vec!["atlassian"]);
}
#[test]
fn resolve_inherited_pool_missing_parent_returns_none() {
    let result = super::resolve_inherited_mcp_pool(
        None,
        &xai_grok_agent::config::McpInheritance::All,
    );
    assert!(result.is_none());
}
/// Plugin agents must still inherit the parent pool under default
/// `mcpInheritance: all`. The product rule is: plugins cannot *declare*
/// mcpServers, but they do inherit already-connected parent servers.
#[test]
fn plugin_agents_inherit_parent_mcp_pool_by_default() {
    assert!(
            !super::agent_owned_mcp_servers_allowed(true),
            "plugin agents must not declare agent-owned mcpServers"
        );
    assert!(
            super::agent_owned_mcp_servers_allowed(false),
            "non-plugin agents may declare agent-owned mcpServers"
        );
    let pool = make_pool(&["atlassian", "github"]);
    let inherited = super::resolve_inherited_mcp_pool(
            Some(pool),
            &xai_grok_agent::config::McpInheritance::All,
        )
        .expect("plugin children inherit parent pool with mcpInheritance=all");
    assert_eq!(pool_names(&inherited), vec!["atlassian", "github"]);
}
#[test]
fn restricted_children_reject_agent_owned_mcp_servers_before_session_spawn() {
    use xai_tool_types::SubagentCapabilityMode;

    let mut definition = xai_grok_agent::config::AgentDefinition::parse(
        r#"---
name: restricted-mcp
description: restricted MCP admission fixture
mcpServers:
  - parent-server
  - inline-server:
      type: stdio
      command: sh
      args: ["-c", "exit 0"]
---
fixture
"#,
    )
    .expect("agent definition fixture must parse");
    for mode in [
        SubagentCapabilityMode::ReadOnly,
        SubagentCapabilityMode::ReadWrite,
        SubagentCapabilityMode::Execute,
    ] {
        let error = super::agent_owned_mcp_server_admission_error(&definition, Some(mode))
            .expect("restricted mode must reject agent-owned MCP startup");
        assert!(error.contains("mcpServers"));
        assert!(error.contains("All"));
    }
    assert!(
        super::agent_owned_mcp_server_admission_error(
            &definition,
            Some(SubagentCapabilityMode::All)
        )
        .is_none()
    );
    assert!(super::agent_owned_mcp_server_admission_error(&definition, None).is_none());

    definition.plugin_name = Some("fixture-plugin".into());
    assert!(
        super::agent_owned_mcp_server_admission_error(
            &definition,
            Some(SubagentCapabilityMode::ReadOnly)
        )
        .is_none(),
        "plugin-owned configs keep the existing ignore path"
    );

    definition.plugin_name = None;
    definition.mcp_servers.clear();
    definition.mcp_inheritance = xai_grok_agent::config::McpInheritance::All;
    assert!(
        super::agent_owned_mcp_server_admission_error(
            &definition,
            Some(SubagentCapabilityMode::ReadOnly)
        )
        .is_none(),
        "already-connected parent MCP inheritance must remain available"
    );
    assert!(super::agent_owned_mcp_servers_allowed(false));
    assert!(!super::agent_owned_mcp_servers_allowed(true));
}
#[test]
fn plugin_agents_can_opt_out_via_mcp_inheritance_none() {
    let pool = make_pool(&["atlassian"]);
    let inherited = super::resolve_inherited_mcp_pool(
        Some(pool),
        &xai_grok_agent::config::McpInheritance::None,
    );
    assert!(
            inherited.is_none(),
            "mcpInheritance: none must drop the parent pool for every source"
        );
}
fn make_test_skill(
    name: &str,
    plugin: Option<&str>,
) -> xai_grok_tools::implementations::skills::types::SkillInfo {
    xai_grok_tools::implementations::skills::types::SkillInfo {
        name: name.into(),
        display_name: None,
        description: format!("{name} skill"),
        path: format!("/skills/{name}/SKILL.md"),
        scope: xai_grok_tools::implementations::skills::types::SkillScope::Local,
        enabled: true,
        user_invocable: true,
        plugin_name: plugin.map(Into::into),
        when_to_use: None,
        short_description: None,
        author: None,
        argument_hint: None,
        license: None,
        compatibility: None,
        metadata: None,
        config_source: None,
        plugin_version: None,
        plugin_root: None,
        plugin_data: None,
        allowed_tools: None,
        model: None,
        effort: None,
        disable_model_invocation: false,
        has_user_specified_description: false,
        paths: None,
        body: None,
    }
}
#[test]
fn skills_inherited_count_zero_when_inherit_disabled() {
    let inherit_skills = false;
    let parent_skills = Some(vec![make_test_skill("skill-a", None)]);
    let count = if inherit_skills {
        parent_skills.as_ref().map(|s| s.len() as u32).unwrap_or(0)
    } else {
        0
    };
    assert_eq!(count, 0, "should be 0 when inherit_skills is false");
}
#[test]
fn skills_inherited_count_matches_parent_skills_len() {
    let inherit_skills = true;
    let parent_skills = Some(
        vec![
            make_test_skill("codegen-conventions", None),
            make_test_skill("tui-release", Some("my-plugin")),
        ],
    );
    let count = if inherit_skills {
        parent_skills.as_ref().map(|s| s.len() as u32).unwrap_or(0)
    } else {
        0
    };
    assert_eq!(count, 2);
}
/// Both directions of the publisher→parent goal gate: flipping it
/// would silently kill live-token wiring end-to-end.
#[test]
fn goal_tick_cmd_tx_gates_on_goal_enabled() {
    let (tx, _rx) = mpsc::unbounded_channel::<SessionCommand>();
    assert!(
            goal_tick_cmd_tx(true, Some(&tx)).is_some(),
            "goal on + channel present must wire ticks",
        );
    assert!(
            goal_tick_cmd_tx(false, Some(&tx)).is_none(),
            "goal off must not pay the per-tick send",
        );
    assert!(goal_tick_cmd_tx(true, None).is_none());
    assert!(goal_tick_cmd_tx(false, None).is_none());
}
/// Producer side of the goal live-token wiring: a publisher tick must
/// land on the parent command channel as a `SubagentProgress`
/// notification carrying the child's signal values.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn progress_publisher_delivers_ticks_to_parent_cmd_channel() {
    use crate::session::signals::SessionSignalsHandle;
    use crate::test_support::lsp_runtime::test_gateway;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let signals = SessionSignalsHandle::new();
            signals.increment_turn();
            signals.record_tool_call("bash");
            tokio::task::yield_now().await;
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
            let cancel = tokio_util::sync::CancellationToken::new();
            spawn_progress_publisher(
                signals,
                test_gateway(),
                "parent-1".to_string(),
                "sub-1".to_string(),
                "child-1".to_string(),
                std::time::Instant::now(),
                cancel.clone(),
                Some(cmd_tx),
            );
            let cmd = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    cmd_rx.recv(),
                )
                .await
                .expect("a tick must arrive within the publish interval")
                .expect("channel open");
            cancel.cancel();
            let SessionCommand::XaiSessionNotification { notification } = cmd else {
                panic!("expected XaiSessionNotification");
            };
            let SessionUpdate::SubagentProgress {
                subagent_id,
                parent_session_id,
                turn_count,
                tool_call_count,
                ..
            } = notification.update else {
                panic!("expected SubagentProgress, got {:?}", notification.update);
            };
            assert_eq!(subagent_id, "sub-1");
            assert_eq!(parent_session_id, "parent-1");
            assert_eq!(turn_count, 1);
            assert_eq!(tool_call_count, 1);
        })
        .await;
}
