//! `inspect --native-subagent-route <PARENT_SESSION_ID>`.
//!
//! Bounded, no-follow read of `{parent}/subagents/<id>/meta.json`. Surviving
//! receipts are checked with shipped `inspect_document`. Prompts and other
//! task content are never copied into the inspect document.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;
use xai_grok_subagent_resolution::native_route::{InspectDocument, RouteReceipt, inspect_document};

use crate::session::persistence::find_persisted_session_dir_by_id_in_root_result;
use crate::util::grok_home::grok_home;

const MAX_CHILD_DIRS: usize = 1000;
const MAX_META_BYTES: u64 = 256 * 1024;

#[derive(Debug, Deserialize)]
struct MetaSlice {
    parent_session_id: String,
    #[serde(default)]
    native_route_receipt: Option<RouteReceipt>,
}

/// Print native-route inspect for a persisted parent session.
pub fn inspect_native_subagent_route(parent_session_id: &str, json: bool) -> anyhow::Result<()> {
    let output = collect_native_route_inspect(parent_session_id, &grok_home().join("sessions"))?;
    if json {
        print!("{}", output.json);
    } else {
        print!("{}", output.human);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(super) struct NativeRouteInspectOutput {
    pub document: InspectDocument,
    pub json: String,
    pub human: String,
}

pub(super) fn collect_native_route_inspect(
    parent_session_id: &str,
    sessions_root: &Path,
) -> anyhow::Result<NativeRouteInspectOutput> {
    if parent_session_id.is_empty()
        || parent_session_id.contains('/')
        || parent_session_id.contains('\\')
        || parent_session_id.contains('\0')
    {
        bail!("invalid parent session id");
    }
    let Some(session_dir) =
        find_persisted_session_dir_by_id_in_root_result(parent_session_id, sessions_root)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("lookup parent session {parent_session_id}"))?
    else {
        bail!("parent session not found: {parent_session_id}");
    };
    let receipts = collect_child_receipts(&session_dir, parent_session_id)?;
    for receipt in &receipts {
        inspect_document(vec![receipt.clone()]).map_err(|err| anyhow!(err.to_string()))?;
    }
    let document = inspect_document(receipts).map_err(|err| anyhow!(err.to_string()))?;
    let json = serde_json::to_string_pretty(&document)?;
    let human = format_human(&document);
    Ok(NativeRouteInspectOutput {
        document,
        json,
        human,
    })
}

fn collect_child_receipts(
    session_dir: &Path,
    parent_session_id: &str,
) -> anyhow::Result<Vec<RouteReceipt>> {
    let subagents = session_dir.join("subagents");
    match fs::symlink_metadata(&subagents) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("stat {}", subagents.display()));
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("subagents path is not a regular directory");
        }
        Ok(_) => {}
    }
    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(&subagents)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        names.push(name.to_owned());
    }
    names.sort_unstable();
    if names.len() > MAX_CHILD_DIRS {
        bail!("subagent inspect exceeds {MAX_CHILD_DIRS} child directories");
    }
    let mut receipts = Vec::new();
    for name in names {
        let child_dir = subagents.join(&name);
        let Ok(dir_meta) = fs::symlink_metadata(&child_dir) else {
            continue;
        };
        if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
            continue;
        }
        let meta_path = child_dir.join("meta.json");
        let Some(bytes) = read_meta_nofollow(&meta_path)? else {
            continue;
        };
        let Ok(slice) = serde_json::from_slice::<MetaSlice>(&bytes) else {
            continue;
        };
        if slice.parent_session_id != parent_session_id {
            continue;
        }
        if let Some(receipt) = slice.native_route_receipt
            && inspect_document(vec![receipt.clone()]).is_ok()
        {
            receipts.push(receipt);
        }
    }
    Ok(receipts)
}

fn read_meta_nofollow(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
        Ok(metadata) => metadata,
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }
    if metadata.len() > MAX_META_BYTES {
        return Ok(None);
    }
    let mut file = open_nofollow(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_META_BYTES {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn open_nofollow(path: &Path) -> std::io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn format_human(document: &InspectDocument) -> String {
    let mut out = String::new();
    out.push_str(&document.schema);
    out.push('\n');
    out.push_str(&format!("receipts: {}\n", document.receipts.len()));
    for receipt in &document.receipts {
        out.push_str(&format!(
            "- catalog={} attempt={} digest={}\n",
            receipt.selected_catalog_id, receipt.attempt, receipt.route_digest
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_subagent_resolution::native_route::{
        NativeModelSelection, NativeSubagentRouteRequest, SyntheticCatalog, SyntheticCatalogEntry,
        resolve_native_route,
    };

    const PARENT: &str = "019c0000-0000-7000-8000-000000000801";
    const SECRET: &str = "SECRET_TOKEN_DO_NOT_EMIT";

    fn catalog() -> SyntheticCatalog {
        SyntheticCatalog {
            entries: vec![SyntheticCatalogEntry {
                catalog_id: "review-primary".into(),
                wire_model: "gpt-family-wire".into(),
                route_key: "route-sub".into(),
                access_profile: "subscription".into(),
                ready: true,
                unknown_readiness: false,
                local_only: false,
                harness: Some("grok".into()),
                context_tokens: Some(128_000),
                structured_output: true,
                named_capabilities: vec!["structured_output".into()],
            }],
        }
    }

    fn receipt() -> RouteReceipt {
        let request = NativeSubagentRouteRequest {
            schema_version: 1,
            selection: NativeModelSelection::Exact {
                catalog_id: "review-primary".into(),
            },
            required_capabilities: Default::default(),
            capability_ceiling: None,
            consumer_policy_id: None,
            consumer_policy_digest: None,
            parent_catalog_id: None,
            parent_session_id: Some(PARENT.into()),
            child_session_id: Some("child-a".into()),
            resume: None,
        };
        resolve_native_route(&request, &catalog(), 20, 1)
            .expect("fixture receipt")
            .receipt
    }

    fn seed_parent(root: &Path) -> PathBuf {
        let cwd = crate::util::grok_home::encode_cwd_dirname("/repo/inspect");
        let session_dir = root.join("sessions").join(cwd).join(PARENT);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("summary.json"), b"{}").unwrap();
        session_dir
    }

    fn write_meta(session_dir: &Path, child: &str, parent: &str, receipt: Option<&RouteReceipt>) {
        let dir = session_dir.join("subagents").join(child);
        fs::create_dir_all(&dir).unwrap();
        let receipt_json = match receipt {
            Some(receipt) => serde_json::to_value(receipt).unwrap(),
            None => serde_json::Value::Null,
        };
        let body = serde_json::json!({
            "subagent_id": child,
            "parent_session_id": parent,
            "child_session_id": child,
            "subagent_type": "explore",
            "description": "fixture",
            "prompt": SECRET,
            "status": "completed",
            "started_at": "2026-08-31T00:00:00Z",
            "native_route_receipt": receipt_json,
        });
        fs::write(
            dir.join("meta.json"),
            serde_json::to_vec_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn invalid_session_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let err = collect_native_route_inspect("missing-session-id", &root.path().join("sessions"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found") || err.contains("invalid"));
    }

    #[test]
    fn path_session_id_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let err = collect_native_route_inspect("../etc", &root.path().join("sessions"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid"));
    }

    #[test]
    fn surviving_receipt_is_secret_free_and_deterministic() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = seed_parent(root.path());
        let receipt = receipt();
        write_meta(&session_dir, "child-b", PARENT, Some(&receipt));
        write_meta(&session_dir, "child-a", PARENT, Some(&receipt));
        let output = collect_native_route_inspect(PARENT, &root.path().join("sessions")).unwrap();
        assert_eq!(
            output.document.schema,
            "medley.native-subagent-route.inspect/v1"
        );
        assert_eq!(output.document.receipts.len(), 2);
        assert!(
            output
                .json
                .contains("medley.native-subagent-route.inspect/v1")
        );
        assert!(output.human.contains("receipts: 2"));
        assert!(!output.json.contains(SECRET));
        assert!(!output.human.contains(SECRET));
        let again = collect_native_route_inspect(PARENT, &root.path().join("sessions")).unwrap();
        assert_eq!(output.json, again.json);
    }

    #[test]
    fn parent_mismatch_corrupt_oversize_and_symlink_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = seed_parent(root.path());
        let receipt = receipt();
        write_meta(&session_dir, "keep", PARENT, Some(&receipt));
        write_meta(&session_dir, "mismatch", "other-parent", Some(&receipt));
        let corrupt_dir = session_dir.join("subagents").join("corrupt");
        fs::create_dir_all(&corrupt_dir).unwrap();
        fs::write(corrupt_dir.join("meta.json"), b"{not-json").unwrap();
        let huge_dir = session_dir.join("subagents").join("huge");
        fs::create_dir_all(&huge_dir).unwrap();
        fs::write(
            huge_dir.join("meta.json"),
            vec![b'x'; (MAX_META_BYTES as usize) + 8],
        )
        .unwrap();
        #[cfg(unix)]
        {
            let linked = session_dir.join("subagents").join("linked");
            fs::create_dir_all(&linked).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", linked.join("meta.json")).unwrap();
        }
        let output = collect_native_route_inspect(PARENT, &root.path().join("sessions")).unwrap();
        assert_eq!(output.document.receipts.len(), 1);
        assert_eq!(
            output.document.receipts[0].selected_catalog_id,
            "review-primary"
        );
        assert!(!output.json.contains(SECRET));
        assert!(!output.json.contains("root:"));
    }

    #[test]
    fn child_cap_is_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = seed_parent(root.path());
        for i in 0..=MAX_CHILD_DIRS {
            let dir = session_dir.join("subagents").join(format!("c{i:04}"));
            fs::create_dir_all(&dir).unwrap();
        }
        let err = collect_native_route_inspect(PARENT, &root.path().join("sessions"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("1000"));
    }
}
