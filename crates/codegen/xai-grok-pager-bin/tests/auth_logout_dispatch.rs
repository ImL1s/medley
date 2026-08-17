//! Hermetic composition-root coverage for provider-scoped CLI logout.

use std::process::Command;

struct IsolatedRoot(std::path::PathBuf);

impl Drop for IsolatedRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn pager_binary() -> std::path::PathBuf {
    option_env!("CARGO_BIN_EXE_xai-grok-pager")
        .map(std::path::PathBuf::from)
        .filter(|path| path.exists())
        .expect("Cargo must provide the pager binary for this integration test")
}

fn isolated_root() -> IsolatedRoot {
    let root = std::env::temp_dir().join(format!(
        "medley-codex-logout-dispatch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock must follow the Unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("create isolated test root");
    IsolatedRoot(root)
}

#[test]
fn openai_codex_logout_dispatch_removes_only_medleys_provider_scope_without_network() {
    let root = isolated_root();
    let home = root.0.join("home");
    let medley_home = root.0.join("medley-home");
    std::fs::create_dir_all(home.join(".codex")).expect("create official Codex home");
    std::fs::create_dir_all(&medley_home).expect("create isolated Medley home");

    let operator_auth = serde_json::json!({
        "key": "operator-auth-fixture",
        "auth_mode": "oidc",
        "create_time": "2026-01-01T00:00:00Z",
        "user_id": "operator",
        "email": "operator@example.invalid",
        "coding_data_retention_opt_out": true
    });
    // An empty access token is deliberately non-revocable. The provider scope
    // is still present on disk, so this exercises local dispatch/removal while
    // making an OAuth network request impossible by construction.
    let codex_scope = serde_json::json!({
        "key": "",
        "auth_mode": "open_ai_codex",
        "create_time": "2026-01-01T00:00:00Z",
        "user_id": "",
        "email": null,
        "oidc_issuer": "https://auth.openai.com",
        "oidc_client_id": "app_EMoamEEZ73f0CkXaXp7hrann"
    });
    let auth_path = medley_home.join("auth.json");
    std::fs::write(
        &auth_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "xai::operator": operator_auth,
            "openai::codex": codex_scope
        }))
        .expect("serialize Medley auth fixture"),
    )
    .expect("write Medley auth fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
            .expect("secure Medley auth fixture");
    }

    let official_store = home.join(".codex/auth.json");
    let official_store_before = br#"{"official_codex_cli":"operator-owned-fixture"}"#;
    std::fs::write(&official_store, official_store_before).expect("seed official Codex CLI store");

    let output = Command::new(pager_binary())
        .args(["logout", "--provider", "openai-codex"])
        .env_clear()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("GROK_HOME", &medley_home)
        .env("MEDLEY_HOME", &medley_home)
        .env("NO_COLOR", "1")
        .output()
        .expect("run real pager binary");

    assert!(
        output.status.success(),
        "provider-scoped logout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let medley_store: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&auth_path).expect("read Medley auth after provider logout"),
    )
    .expect("parse Medley auth after provider logout");
    assert!(
        medley_store.get("openai::codex").is_none(),
        "the selected provider scope must be removed"
    );
    assert_eq!(
        medley_store.get("xai::operator"),
        Some(&operator_auth),
        "provider logout must preserve operator xAI auth"
    );
    assert_eq!(
        std::fs::read(&official_store).expect("read official Codex CLI store"),
        official_store_before,
        "Medley logout must not touch the operator-owned official Codex CLI store"
    );
}
