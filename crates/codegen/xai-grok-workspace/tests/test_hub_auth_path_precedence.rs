//! #482: the standalone hub (`xai-workspace-server`) must honour
//! `GROK_AUTH_PATH` in its own environment, exactly like every other medley
//! binary already does (`AuthManager`, `store_api_key`/`clear_api_key`, the
//! hot-reload watcher — #409, #434). Before this fix `hub_auth::default_auth_path`
//! hardcoded `grok_home.join("auth.json")`, ignoring the variable entirely.
//!
//! Three phases in **one** test function, not three separate `#[test]`s:
//! `xai_grok_config::grok_home()` memoises into a process-wide `OnceLock`
//! (the same constraint `xai-grok-shell/tests/test_auth_path_env_precedence.rs`
//! documents), so a second `#[test]` in this binary that wanted its own
//! `grok_home` could not get one — the first call anywhere in the process
//! wins for the rest of it. Sequencing all three phases in one function
//! sidesteps that entirely: `grok_home` is set up once, resolves once, and
//! every phase reuses the same resolved value while only `GROK_AUTH_PATH`
//! and the explicit `auth_config` argument change between phases.

use std::io::Write;

use url::Url;
use xai_computer_hub_sdk::AuthCredential;
use xai_grok_workspace::hub_auth::{default_auth_path, provider};

/// OIDC-shaped auth.json matching `hub_auth`'s own `provider_loopback_uses_bearer`
/// test fixture. `read_auth_entry` filters to entries with both
/// `refresh_token` and `oidc_issuer` present *before* `provider` branches on
/// loopback vs. not, so both are required even though the loopback branch
/// itself never uses them for the credential (only `key` and `user_id`).
fn write_auth_json(path: &std::path::Path, key: &str, user_id: &str) {
    let mut f = std::fs::File::create(path).expect("create auth.json");
    write!(
        f,
        r#"{{ "oidc": {{ "key": "{key}", "user_id": "{user_id}", "refresh_token": "rt", "oidc_issuer": "https://auth.x.ai", "oidc_client_id": "c1" }} }}"#
    )
    .expect("write auth.json");
}

/// Three phases, one function — not a stylistic choice. `xai_grok_config::grok_home()`
/// (reached via `default_auth_path` → `user_grok_home`) memoises into a
/// process-wide `OnceLock`: the first call anywhere in this binary decides
/// `grok_home` for every call after it, in every test, for the rest of the
/// process. A second `#[test]` in this file wanting its own distinct
/// `grok_home` could not have one — cargo runs each `#[test]` fn in the same
/// process, so it would silently inherit whichever directory phase 1 below
/// already resolved. Sequencing phases inside one function turns that
/// constraint into the mechanism: `grok_home` resolves once (in phase 1),
/// and phases 2 and 3 deliberately reuse it while only `GROK_AUTH_PATH` and
/// the explicit `auth_config` argument change underneath.
#[test]
fn hub_default_auth_path_honours_grok_auth_path_then_falls_back_then_yields_to_explicit_flag() {
    let state_home = tempfile::tempdir().expect("state home");
    let auth_dir = tempfile::tempdir().expect("auth dir");
    let auth_path = auth_dir.path().join("elsewhere.json");
    let default_path = state_home.path().join("auth.json");

    // The discriminator, same role as every other #409/#434/#482 test: if
    // these two ever became the same path, phase 1 and phase 2 below would
    // pass whether or not `default_auth_path` reads the variable at all.
    assert_ne!(
        auth_path, default_path,
        "the fixture must keep GROK_AUTH_PATH and grok_home/auth.json distinct"
    );

    // SAFETY: this is the only test in this binary (one integration target
    // = one process), so no other test can observe or race these env
    // mutations or the grok_home() OnceLock they seed.
    unsafe {
        std::env::set_var("MEDLEY_HOME", state_home.path());
        std::env::set_var("GROK_HOME", state_home.path());
    }

    // ---- Phase 1: GROK_AUTH_PATH set, no explicit --auth-config ----
    // This call is also what memoises grok_home() for the rest of the
    // process; every later phase reuses this same resolved value.
    unsafe { std::env::set_var("GROK_AUTH_PATH", &auth_path) };
    let resolved = default_auth_path().expect("resolve with GROK_AUTH_PATH set");
    assert_eq!(
        resolved, auth_path,
        "default_auth_path must follow GROK_AUTH_PATH when no --auth-config is given"
    );
    assert!(
        !default_path.exists(),
        "resolving must not have created grok_home/auth.json"
    );

    // ---- Phase 2: GROK_AUTH_PATH unset, falls back to grok_home/auth.json ----
    unsafe { std::env::remove_var("GROK_AUTH_PATH") };
    let resolved = default_auth_path().expect("resolve with GROK_AUTH_PATH unset");
    assert_eq!(
        resolved, default_path,
        "default_auth_path must fall back to grok_home/auth.json once GROK_AUTH_PATH is unset"
    );

    // ---- Phase 3: GROK_AUTH_PATH set AND an explicit auth_config passed —
    // the flag must win. This is the regression a future refactor is most
    // likely to introduce (e.g. moving the GROK_AUTH_PATH check earlier so
    // it fires even when an explicit path was given).
    let explicit_path = auth_dir.path().join("explicit.json");
    write_auth_json(&explicit_path, "explicit-flag-token", "explicit-user");
    write_auth_json(&auth_path, "env-var-token", "env-user");
    unsafe { std::env::set_var("GROK_AUTH_PATH", &auth_path) };

    let url = Url::parse("ws://localhost:9988/v1/tools").expect("parse loopback url");
    let auth = provider(&url, Some(&explicit_path))
        .expect("provider must build from the explicit path even with GROK_AUTH_PATH set");
    match auth.current() {
        AuthCredential::Bearer { token } => assert_eq!(
            token, "explicit-flag-token",
            "an explicit auth_config argument must win over GROK_AUTH_PATH — explicit beats ambient"
        ),
        other => panic!("expected Bearer, got {other:?}"),
    }
    let identity = auth.identity().expect("identity present");
    assert_eq!(
        identity.user_id, "explicit-user",
        "the identity must also come from the explicit path, not the env-named one"
    );

    unsafe {
        std::env::remove_var("GROK_AUTH_PATH");
        std::env::remove_var("GROK_HOME");
        std::env::remove_var("MEDLEY_HOME");
    }
}
