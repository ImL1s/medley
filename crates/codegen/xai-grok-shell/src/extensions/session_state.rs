//! `x.ai/session/state` reads a session's metadata columns; `x.ai/session/import`
//! writes them, with the transcript, to recreate a session on another host.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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

    let Some(dir) = resolve_session_dir(&request.session_id, &request.cwd) else {
        return Err(acp::Error::invalid_params().data("session not found"));
    };
    let mut state = serde_json::Map::new();
    for (column, rel) in COLUMNS {
        if let Ok(text) = std::fs::read_to_string(dir.join(rel))
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
    handle_import_in(&crate::util::grok_home::grok_home(), args).await
}

/// [`handle_import`] against an explicit GROK_HOME root. The shipped handler
/// and symlink-escape tests share this body.
pub(crate) async fn handle_import_in(root: &Path, args: &acp::ExtRequest) -> ExtResult {
    let mut request: ImportRequest = super::parse_params(args)?;
    validate_session_uuid(&request.session_id)?;

    let encoded = crate::util::grok_home::encode_cwd_dirname(&request.cwd);
    let dir = root
        .join("sessions")
        .join(&encoded)
        .join(&request.session_id);

    // resolve_session_dir gates on summary.json, so an interrupted import (dir created,
    // summary not yet written) is recreated on retry rather than skipped forever.
    let has_local_session =
        resolve_session_dir_in(root, &request.session_id, &request.cwd).is_some();
    if !has_local_session {
        let Some(summary_value) = request.state.get_mut(SUMMARY_COLUMN) else {
            return Err(
                acp::Error::invalid_params().data("session/import requires a summary column")
            );
        };
        let Some(summary) = summary_value.as_object_mut() else {
            return Err(
                acp::Error::invalid_params().data("session/import summary must be an object")
            );
        };
        sanitize_summary_for_host(summary, &request.session_id, &request.cwd);
        // Reject a summary that would not load rather than persist one that bricks the
        // session and blocks re-import.
        if Summary::deserialize(&*summary_value).is_err() {
            return Err(acp::Error::invalid_params().data("summary column is not a valid summary"));
        }
        let _lease = xai_grok_workspace::session::id_lock::acquire_session_id_lock_sync(
            root,
            &request.session_id,
        )
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        let parent = xai_grok_workspace::session::publication_parent::ensure_publication_parent(
            root,
            OsStr::new(&encoded),
        )
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        parent
            .revalidate()
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        parent
            .ensure_session_dir(OsStr::new(&request.session_id))
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        write_cwd_sidecar(&parent, &request.cwd, &encoded)
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        parent
            .revalidate()
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        write_import_checked(&dir, &request.state, &request.updates, || {
            parent.revalidate()
        })
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    }
    super::to_raw_response(&json!({ "imported": !has_local_session }))
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

fn write_cwd_sidecar(
    parent: &xai_grok_workspace::session::publication_parent::PublicationParent,
    cwd: &str,
    encoded: &str,
) -> std::io::Result<()> {
    if encoded == urlencoding::encode(cwd).as_ref() {
        return Ok(());
    }
    match parent
        .parent_anchor()
        .create_child_file_new(OsStr::new(".cwd"))
    {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(cwd.as_bytes())?;
            file.sync_all()
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Writes summary.json last, and each file to a temporary name first, so an interrupted
/// import leaves an incomplete session that load treats as absent.
fn write_import(
    dir: &Path,
    state: &std::collections::HashMap<String, Value>,
    updates: &[Value],
) -> std::io::Result<()> {
    write_import_checked(dir, state, updates, || Ok(()))
}

fn write_import_checked(
    dir: &Path,
    state: &std::collections::HashMap<String, Value>,
    updates: &[Value],
    revalidate: impl Fn() -> std::io::Result<()>,
) -> std::io::Result<()> {
    revalidate()?;
    std::fs::create_dir_all(dir)?;
    revalidate()?;

    // Clear every file this import owns so a leftover from a failed attempt can't
    // merge with the new snapshot; this import is authoritative.
    let _ = std::fs::remove_file(dir.join(st::CHAT_HISTORY_FILE));
    let _ = std::fs::remove_file(dir.join(st::UPDATES_FILE));
    for (_, rel) in COLUMNS {
        let _ = std::fs::remove_file(dir.join(rel));
    }
    revalidate()?;

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

/// The session's directory, or `None` when it isn't found on this host. Falls back to
/// an id scan when `(id, cwd)` has no summary (subagents use their own cwd); both
/// branches require summary.json so a bare directory doesn't count as present.
fn resolve_session_dir(session_id: &str, cwd: &str) -> Option<PathBuf> {
    resolve_session_dir_in(&crate::util::grok_home::grok_home(), session_id, cwd)
}

fn resolve_session_dir_in(root: &Path, session_id: &str, cwd: &str) -> Option<PathBuf> {
    let encoded = crate::util::grok_home::encode_cwd_dirname(cwd);
    let dir = root.join("sessions").join(encoded).join(session_id);
    if dir.join(st::SUMMARY_FILE).is_file() {
        return Some(dir);
    }
    if root == crate::util::grok_home::grok_home() {
        crate::session::persistence::find_session_dir_by_id(session_id)
            .filter(|found| found.join(st::SUMMARY_FILE).is_file())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[cfg(unix)]
    #[tokio::test]
    async fn fork_and_import_reject_symlinked_encoded_cwd_parent() {
        use crate::session::fork::{ForkSessionRequest, fork_session_in};
        use crate::session::info::Info;
        use crate::session::storage::{JsonlStorageAdapter, StorageAdapter};
        use std::collections::BTreeMap;
        use std::os::unix::fs::symlink;

        fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
            let mut out = BTreeMap::new();
            fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
                let mut entries: Vec<_> = std::fs::read_dir(dir)
                    .unwrap()
                    .map(|entry| entry.unwrap())
                    .collect();
                entries.sort_by_key(|entry| entry.file_name());
                for entry in entries {
                    let path = entry.path();
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    let meta = std::fs::symlink_metadata(&path).unwrap();
                    if meta.file_type().is_symlink() {
                        out.insert(
                            rel,
                            format!("symlink->{}", std::fs::read_link(&path).unwrap().display())
                                .into_bytes(),
                        );
                    } else if meta.is_dir() {
                        out.insert(rel, b"dir".to_vec());
                        walk(root, &path, out);
                    } else {
                        out.insert(rel, std::fs::read(&path).unwrap());
                    }
                }
            }
            walk(root, root, &mut out);
            out
        }

        let grok = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("canary"), b"outside-untouched-340").unwrap();
        let before = snapshot(outside.path());

        let target_cwd = "/repo/issue-340/workspace";
        let encoded = crate::util::grok_home::encode_cwd_dirname(target_cwd);
        let sessions = grok.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        symlink(outside.path(), sessions.join(&encoded)).unwrap();

        let source_cwd = "/repo/issue-340/source";
        let source_id = "019c0000-0000-7000-8000-000000000340";
        let storage = JsonlStorageAdapter::with_root(grok.path().to_path_buf());
        let source_info = Info {
            id: acp::SessionId::new(source_id),
            cwd: source_cwd.to_string(),
        };
        storage
            .init_session(&source_info, acp::ModelId::new("grok-code-fast-1"))
            .await
            .unwrap();

        let fork = fork_session_in(
            grok.path().to_path_buf(),
            ForkSessionRequest {
                source_session_id: source_id.to_string(),
                source_cwd: source_cwd.to_string(),
                new_cwd: target_cwd.to_string(),
                new_session_id: Some("019c0000-0000-7000-8000-000000000341".to_string()),
                ..Default::default()
            },
            "test-agent",
            None,
        )
        .await;
        assert!(
            fork.is_err(),
            "fork must reject a symlinked encoded-CWD parent: {fork:?}"
        );

        let import_id = "019c0000-0000-7000-8000-000000000342";
        let import_info = Info {
            id: acp::SessionId::new(import_id),
            cwd: target_cwd.to_string(),
        };
        let summary = crate::session::persistence::Summary::new(
            &import_info,
            acp::ModelId::new("grok-code-fast-1"),
        )
        .unwrap();
        let params = json!({
            "sessionId": import_id,
            "cwd": target_cwd,
            "state": { "summary": summary },
            "updates": []
        });
        let raw = serde_json::value::to_raw_value(&params).unwrap();
        let args = acp::ExtRequest::new("x.ai/session/import", std::sync::Arc::from(raw));
        let imported = handle_import_in(grok.path(), &args).await;
        assert!(
            imported.is_err(),
            "import must reject a symlinked encoded-CWD parent: {imported:?}"
        );

        assert_eq!(snapshot(outside.path()), before);
        assert_eq!(
            std::fs::read(outside.path().join("canary")).unwrap(),
            b"outside-untouched-340"
        );
    }
}
