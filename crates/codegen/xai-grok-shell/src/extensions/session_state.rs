//! `x.ai/session/state` reads a session's metadata columns; `x.ai/session/import`
//! writes them, with the transcript, to recreate a session on another host.

use std::path::Path;

use agent_client_protocol as acp;
use serde::Deserialize;
use serde_json::{Value, json};

use super::ExtResult;
use crate::session::persistence::Summary;
use crate::session::storage as st;

/// The summary column, required to load a session.
const SUMMARY_COLUMN: &str = "summary";

/// Logical column name to its file under the session directory. Paths come from the
/// storage layer so import and load never disagree about the on-disk layout. `summary`
/// is last so import writes it last, as the commit marker; keep it there.
const COLUMNS: &[(&str, &str)] = &[
    ("plan", st::PLAN_FILE),
    ("planMode", st::PLAN_MODE_FILE),
    ("signals", st::SIGNALS_FILE),
    ("goal", st::GOAL_STATE_FILE),
    ("announcement", st::ANNOUNCEMENT_STATE_FILE),
    (SUMMARY_COLUMN, st::SUMMARY_FILE),
];

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateRequest {
    session_id: String,
    cwd: String,
}

/// A session id is a UUID (see acp_agent's new_session); requiring that keeps it safe
/// to join into a filesystem path.
fn validate_session_uuid(session_id: &str) -> Result<(), acp::Error> {
    uuid::Uuid::try_parse(session_id)
        .map(|_| ())
        .map_err(|_| acp::Error::invalid_params().data("sessionId must be a UUID"))
}

/// `x.ai/session/state`: return metadata columns keyed by logical name. Errors when
/// the session isn't found on this host, since it reads a single record whose absence
/// is not an empty result (unlike the collection returned by `x.ai/session/updates`).
pub(crate) async fn handle_state(args: &acp::ExtRequest) -> ExtResult {
    let request: StateRequest = super::parse_params(args)?;
    validate_session_uuid(&request.session_id)?;

    let Some(session) = crate::session::persistence::acquire_published_session_read(
        &request.session_id,
        Some(&request.cwd),
    )
    .await
    .map_err(|error| acp::Error::internal_error().data(error.to_string()))?
    else {
        return Err(acp::Error::invalid_params().data("session not found"));
    };
    let mut state = serde_json::Map::new();
    for (column, rel) in COLUMNS {
        if let Ok(text) = std::fs::read_to_string(session.path().join(rel))
            && let Ok(value) = serde_json::from_str::<Value>(&text)
        {
            state.insert((*column).to_string(), value);
        }
    }
    super::to_raw_response(&state)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportRequest {
    session_id: String,
    cwd: String,
    #[serde(default)]
    state: std::collections::HashMap<String, Value>,
    /// One JSON object per `updates.jsonl` line, not pre-serialized strings.
    #[serde(default)]
    updates: Vec<Value>,
}

/// `x.ai/session/import`: recreate a session on this host from mirrored columns and
/// transcript. A session that already exists locally is left unchanged.
pub(crate) async fn handle_import(args: &acp::ExtRequest) -> ExtResult {
    let mut request: ImportRequest = super::parse_params(args)?;
    validate_session_uuid(&request.session_id)?;

    let info = crate::session::info::Info {
        id: acp::SessionId::new(request.session_id.clone()),
        cwd: request.cwd.clone(),
    };
    let dir = crate::session::persistence::session_dir(&info);
    let sessions_root = crate::util::grok_home::grok_home().join("sessions");
    let imported = import_into_root(&mut request, &sessions_root, dir).await?;
    super::to_raw_response(&json!({ "imported": imported }))
}

async fn import_into_root(
    request: &mut ImportRequest,
    sessions_root: &Path,
    dir: std::path::PathBuf,
) -> Result<bool, acp::Error> {
    let mut session = crate::session::persistence::acquire_published_session_write_in_root(
        sessions_root,
        &request.session_id,
        Some(&request.cwd),
    )
    .await
    .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
    if let Some(existing_path) = session.published_path() {
        let existing_path = existing_path.to_path_buf();
        let summary = session
            .read_summary()
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?
            .ok_or_else(|| {
                acp::Error::internal_error()
                    .data("session/import existing winner has no readable summary")
            })?;
        if summary.info.id.to_string() != request.session_id
            || summary.info.cwd != request.cwd
            || existing_path != dir
        {
            return Err(acp::Error::internal_error()
                .data("session/import existing winner identity does not match the request"));
        }
        return Ok(false);
    }

    let Some(summary_value) = request.state.get_mut(SUMMARY_COLUMN) else {
        return Err(acp::Error::invalid_params().data("session/import requires a summary column"));
    };
    let Some(summary) = summary_value.as_object_mut() else {
        return Err(acp::Error::invalid_params().data("session/import summary must be an object"));
    };
    sanitize_summary_for_host(summary, &request.session_id, &request.cwd);
    // Reject a summary that would not load rather than persist one that bricks the
    // session and blocks re-import.
    if Summary::deserialize(&*summary_value).is_err() {
        return Err(acp::Error::invalid_params().data("summary column is not a valid summary"));
    }
    {
        let dir = session
            .begin_new(dir)
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        write_import(dir, &request.state, &request.updates)
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    }
    reconcile_import_publication(session.publish_new_classified())
}

fn reconcile_import_publication(
    publication: Result<(), crate::session::persistence::PublishedSessionFinalizeError>,
) -> Result<bool, acp::Error> {
    use crate::session::persistence::PublishedSessionFinalizeError;

    match publication {
        Ok(()) => Ok(true),
        Err(PublishedSessionFinalizeError::NotCommitted(error)) => {
            Err(acp::Error::internal_error().data(error.to_string()))
        }
        Err(PublishedSessionFinalizeError::CommittedDurability(error)) => {
            tracing::warn!(
                %error,
                "session/import committed but its durability acknowledgement failed"
            );
            Ok(true)
        }
        Err(PublishedSessionFinalizeError::CommittedIdentity(error)) => {
            Err(acp::Error::internal_error().data(format!(
                "session/import committed outside the canonical namespace: {error}"
            )))
        }
    }
}

/// Rewrite a mirrored summary's host-specific fields to describe this host.
fn sanitize_summary_for_host(summary: &mut serde_json::Map<String, Value>, id: &str, cwd: &str) {
    if let Some(info_obj) = summary.get_mut("info").and_then(Value::as_object_mut) {
        info_obj.insert("id".to_string(), Value::String(id.to_string()));
        info_obj.insert("cwd".to_string(), Value::String(cwd.to_string()));
    }
    summary.insert(
        "chat_format_version".to_string(),
        json!(crate::session::persistence::CHAT_FORMAT_VERSION),
    );
    summary.insert("git_remotes".to_string(), json!([]));
    for field in [
        "prompt_display_cwd",
        "source_workspace_dir",
        "git_root_dir",
        "head_commit",
        "head_branch",
        "worktree_label",
        "request_id",
    ] {
        summary.remove(field);
    }
    set_or_remove(
        summary,
        "grok_home",
        crate::session::persistence::grok_home_string(),
    );
    set_or_remove(
        summary,
        "sandbox_profile",
        xai_grok_sandbox::configured_profile_name().map(String::from),
    );
}

fn set_or_remove(obj: &mut serde_json::Map<String, Value>, key: &str, value: Option<String>) {
    match value {
        Some(v) => {
            obj.insert(key.to_string(), Value::String(v));
        }
        None => {
            obj.remove(key);
        }
    }
}

/// Writes summary.json last and each file through an atomic temporary replacement.
/// The caller keeps this whole tree in private staging until the final anchored
/// directory publication, so an interrupted import is never publicly discoverable.
fn write_import(
    dir: &Path,
    state: &std::collections::HashMap<String, Value>,
    updates: &[Value],
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;

    // Clear every file this import owns so a leftover from a failed attempt can't
    // merge with the new snapshot; this import is authoritative.
    let _ = std::fs::remove_file(dir.join(st::CHAT_HISTORY_FILE));
    let _ = std::fs::remove_file(dir.join(st::UPDATES_FILE));
    for (_, rel) in COLUMNS {
        let _ = std::fs::remove_file(dir.join(rel));
    }

    if !updates.is_empty() {
        st::write_jsonl_atomic(&dir.join(st::UPDATES_FILE), updates)?;
    }

    for (column, rel) in COLUMNS {
        if let Some(value) = state.get(*column) {
            write_column(dir, rel, value)?;
        }
    }
    Ok(())
}

fn write_column(dir: &Path, rel: &str, value: &Value) -> std::io::Result<()> {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    st::write_bytes_atomic(&path, value.to_string().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn import_request(session_id: &str, cwd: &str) -> ImportRequest {
        let info = crate::session::info::Info {
            id: acp::SessionId::new(session_id.to_owned()),
            cwd: cwd.to_owned(),
        };
        let summary = Summary::new(&info, crate::session::persistence::default_model_id()).unwrap();
        ImportRequest {
            session_id: session_id.to_owned(),
            cwd: cwd.to_owned(),
            state: [(
                SUMMARY_COLUMN.to_owned(),
                serde_json::to_value(summary).unwrap(),
            )]
            .into_iter()
            .collect(),
            updates: vec![json!({ "method": "session/update", "params": { "seq": 1 } })],
        }
    }

    fn import_dir(sessions_root: &Path, request: &ImportRequest) -> std::path::PathBuf {
        sessions_root
            .join(crate::util::grok_home::encode_cwd_dirname(&request.cwd))
            .join(&request.session_id)
    }

    #[test]
    fn sanitize_summary_for_host_rewrites_host_fields() {
        let mut summary = json!({
            "info": { "id": "s1", "cwd": "/remote/host/work" },
            "chat_format_version": 0,
            "prompt_display_cwd": "/remote/host/work",
            "source_workspace_dir": "/remote/host",
            "git_root_dir": "/remote/host/repo",
            "git_remotes": ["origin"],
            "head_commit": "deadbeef",
            "head_branch": "feature",
            "worktree_label": "wt",
            "request_id": "req-1",
        })
        .as_object()
        .unwrap()
        .clone();

        sanitize_summary_for_host(&mut summary, "s-new", "/local/work");

        assert_eq!(summary["info"]["id"], json!("s-new"));
        assert_eq!(summary["info"]["cwd"], json!("/local/work"));
        assert_eq!(
            summary["chat_format_version"],
            json!(crate::session::persistence::CHAT_FORMAT_VERSION)
        );
        assert_eq!(summary["git_remotes"], json!([]));
        for gone in [
            "prompt_display_cwd",
            "source_workspace_dir",
            "git_root_dir",
            "head_commit",
            "head_branch",
            "worktree_label",
            "request_id",
        ] {
            assert!(!summary.contains_key(gone), "{gone} should be dropped");
        }
    }

    #[test]
    fn write_import_writes_columns_updates_and_drops_stale_chat() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("chat_history.jsonl"), b"stale cache").unwrap();
        // A column left by a failed prior import that the new payload omits.
        std::fs::write(dir.join("signals.json"), b"{\"stale\":true}").unwrap();

        let mut state = std::collections::HashMap::new();
        state.insert(
            "summary".to_string(),
            json!({ "info": { "id": "s1", "cwd": "/work" } }),
        );
        state.insert("plan".to_string(), json!({ "items": [] }));
        state.insert("goal".to_string(), json!({ "active": false }));
        let updates = vec![
            json!({ "method": "session/update", "params": { "a": 1 } }),
            json!({ "method": "session/update", "params": { "b": 2 } }),
        ];

        write_import(dir, &state, &updates).unwrap();

        assert!(dir.join("summary.json").exists(), "summary.json written");
        assert_eq!(
            std::fs::read_to_string(dir.join("plan.json")).unwrap(),
            r#"{"items":[]}"#
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("goal/state.json")).unwrap(),
            r#"{"active":false}"#
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("updates.jsonl"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        assert!(
            !dir.join("chat_history.jsonl").exists(),
            "stale chat cache dropped so load rebuilds"
        );
        assert!(
            !dir.join("signals.json").exists(),
            "orphan column from a failed import dropped"
        );
    }

    #[tokio::test]
    async fn import_publishes_summary_and_removes_visibility_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_root = tmp.path().join("sessions");
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut request = import_request(
            "019c0000-0000-7000-8000-000000000201",
            &cwd.to_string_lossy(),
        );
        let dir = import_dir(&sessions_root, &request);

        assert!(
            import_into_root(&mut request, &sessions_root, dir.clone())
                .await
                .unwrap()
        );
        assert!(dir.join(st::SUMMARY_FILE).is_file());
        assert!(
            !dir.join(crate::session::persistence::UNPUBLISHED_SESSION_MARKER)
                .exists()
        );
        let published = crate::session::persistence::acquire_published_session_read_in_root(
            &sessions_root,
            &request.session_id,
            Some(&request.cwd),
        )
        .await
        .unwrap()
        .expect("import is visible after commit");
        assert_eq!(published.path(), dir);
    }

    #[tokio::test]
    async fn import_atomically_publishes_long_cwd_metadata() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_root = tmp.path().join("sessions");
        let cwd = format!("/{}", "long-cwd-segment/".repeat(40));
        let mut request = import_request("019c0000-0000-7000-8000-000000000205", &cwd);
        let dir = import_dir(&sessions_root, &request);

        assert!(
            import_into_root(&mut request, &sessions_root, dir.clone())
                .await
                .unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(dir.parent().unwrap().join(".cwd")).unwrap(),
            cwd
        );
        assert!(dir.join(st::SUMMARY_FILE).is_file());
    }

    #[test]
    fn import_reports_success_only_for_committed_durability_failure() {
        use crate::session::persistence::PublishedSessionFinalizeError;

        assert!(
            reconcile_import_publication(Err(PublishedSessionFinalizeError::CommittedDurability(
                std::io::Error::other("injected sync failure",)
            ),))
            .expect("a visible canonical import remains successful")
        );
        assert!(
            reconcile_import_publication(Err(PublishedSessionFinalizeError::NotCommitted(
                std::io::Error::other("injected pre-commit failure"),
            )))
            .is_err()
        );
        assert!(
            reconcile_import_publication(Err(PublishedSessionFinalizeError::CommittedIdentity(
                std::io::Error::other("injected canonical identity failure"),
            )))
            .is_err()
        );
    }

    #[tokio::test]
    async fn concurrent_imports_publish_exactly_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_root = tmp.path().join("sessions");
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let session_id = "019c0000-0000-7000-8000-000000000202";
        let mut first = import_request(session_id, &cwd.to_string_lossy());
        let mut second = import_request(session_id, &cwd.to_string_lossy());
        let dir = import_dir(&sessions_root, &first);

        let (first_result, second_result) = tokio::join!(
            import_into_root(&mut first, &sessions_root, dir.clone()),
            import_into_root(&mut second, &sessions_root, dir.clone()),
        );
        let imported = [first_result.unwrap(), second_result.unwrap()];
        assert_eq!(imported.into_iter().filter(|value| *value).count(), 1);
        assert!(dir.join(st::SUMMARY_FILE).is_file());
        assert!(
            !dir.join(crate::session::persistence::UNPUBLISHED_SESSION_MARKER)
                .exists()
        );
    }

    #[tokio::test]
    async fn import_preserves_and_rejects_stale_public_unpublished_collision() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_root = tmp.path().join("sessions");
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut request = import_request(
            "019c0000-0000-7000-8000-000000000203",
            &cwd.to_string_lossy(),
        );
        let stale_dir = sessions_root
            .join(crate::util::grok_home::encode_cwd_dirname("/stale/import"))
            .join(&request.session_id);
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(
            stale_dir.join(crate::session::persistence::UNPUBLISHED_SESSION_MARKER),
            b"",
        )
        .unwrap();
        std::fs::write(stale_dir.join("partial"), b"stale").unwrap();
        let dir = import_dir(&sessions_root, &request);

        let error = import_into_root(&mut request, &sessions_root, dir.clone())
            .await
            .expect_err("a public stale marker is an untrusted collision");
        assert!(
            error.to_string().contains("already")
                || error.to_string().contains("collision")
                || error.to_string().contains("present"),
            "unexpected error: {error}"
        );
        assert!(stale_dir.join("partial").is_file());
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn import_refuses_symlinked_cwd_parent_without_touching_outside_tree() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let sessions_root = tmp.path().join("sessions");
        std::fs::create_dir(&sessions_root).unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let mut request = import_request(
            "019c0000-0000-7000-8000-000000000204",
            &cwd.to_string_lossy(),
        );
        let dir = import_dir(&sessions_root, &request);
        let target_parent = dir.parent().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), b"untouched").unwrap();
        symlink(&outside, target_parent).unwrap();

        let error = import_into_root(&mut request, &sessions_root, dir)
            .await
            .expect_err("import must reject a symlinked cwd parent");
        assert!(!error.to_string().is_empty(), "error must be actionable");
        assert_eq!(
            std::fs::read(outside.join("sentinel")).unwrap(),
            b"untouched"
        );
        assert!(
            !outside.join(&request.session_id).exists(),
            "import must not create or remove anything through the symlink"
        );
    }
}
