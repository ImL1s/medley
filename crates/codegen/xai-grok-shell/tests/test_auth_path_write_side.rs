//! Production writers must honour `GROK_AUTH_PATH` (#434), not just the
//! reader `AuthManager::new` fixed by #409.
//!
//! `tests/test_auth_path_env_precedence.rs` proved the read side: production
//! resolves `GROK_AUTH_PATH` for `AuthManager`. This file proves the write
//! side named in #434 — `store_api_key` / `clear_api_key`
//! (`crates/codegen/xai-grok-shell/src/auth/storage.rs`), the two functions
//! `extensions/auth.rs`'s `handle_set_api_key` calls for every branch (set,
//! clear-on-empty-key, clear-on-absent-key). Before the fix both hardcoded
//! `grok_home.join("auth.json")`, so with `GROK_AUTH_PATH` set:
//!
//! - a stored key silently had no effect (written where nothing reads it),
//! - and — the sharper failure — "clearing" a key left the credential a
//!   fresh process (or `AuthManager`) still reads on disk, because the clear
//!   touched a different file than the one actually in use.
//!
//! Same two structural reasons as the read-side test require this to be its
//! own integration target rather than a `#[cfg(test)]` unit test or a
//! sibling here of `production_reads_the_grok_auth_path_file_outside_grok_home`:
//!
//! - **Integration, not unit.** The env-reading branch of
//!   `resolved_xai_auth_path` only exists in a non-`cfg(test)` build.
//! - **`grok_home()` memoises into a process-wide `OnceLock`,** so a test
//!   needing `grok_home` and the auth path to be *different* directories
//!   cannot share a binary with a test that pins `grok_home` elsewhere — and
//!   `test_auth_path_env_precedence.rs` already pins it for its own case.
//!
//! **Invariant the tests below rely on:** only
//! [`a_credential_written_only_at_the_grok_auth_path_file_is_visible_through_the_resolver`]
//! calls the memoising `grok_home()`; the others pass `state_home.path()`
//! straight into `store_api_key` / `clear_api_key` / `read_api_key` and never
//! touch the process-wide `OnceLock`. That is what lets three tests share one
//! binary despite the `OnceLock` constraint above. A future test added here
//! that calls `grok_home()` with yet another `state_home` would memoise to
//! whichever one runs first and silently poison the rest — give it its own
//! file instead, the way `test_auth_path_env_precedence.rs` already is one.

use std::collections::BTreeMap;

use chrono::Utc;
use xai_grok_shell::auth::{
    AuthMode, GrokAuth, GrokComConfig, clear_api_key, read_api_key, store_api_key,
};
use xai_grok_test_support::EnvGuard;

#[tokio::test]
#[serial_test::serial]
async fn clearing_the_api_key_must_not_leave_it_readable_at_the_grok_auth_path_file() {
    let state_home = tempfile::tempdir().expect("state home");
    let auth_dir = tempfile::tempdir().expect("auth dir");
    let auth_path = auth_dir.path().join("elsewhere.json");
    let default_path = state_home.path().join("auth.json");

    // The discriminator, same role as in the read-side test: if these two
    // ever became the same path, every assertion below would pass whether
    // or not production writers read the variable.
    assert_ne!(
        auth_path, default_path,
        "the fixture must keep GROK_AUTH_PATH and grok_home/auth.json distinct, \
         or this test cannot tell store_api_key/clear_api_key's two possible \
         targets apart"
    );
    assert!(
        !auth_path.exists(),
        "precondition: nothing at the GROK_AUTH_PATH file yet"
    );
    assert!(
        !default_path.exists(),
        "precondition: nothing at grok_home/auth.json yet"
    );

    let _env = [
        EnvGuard::set("MEDLEY_HOME", state_home.path()),
        EnvGuard::set("GROK_HOME", state_home.path()),
        EnvGuard::set("GROK_AUTH_PATH", &auth_path),
        EnvGuard::unset("GROK_AUTH"),
        EnvGuard::unset("XAI_API_KEY"),
        EnvGuard::unset("GROK_CODE_XAI_API_KEY"),
        EnvGuard::unset("GROK_DEPLOYMENT_KEY"),
    ];

    const KEY: &str = "token-set-and-cleared-through-grok-auth-path";

    // --- Store: extensions/auth.rs's `handle_set_api_key` non-empty-key
    // branch calls exactly this, with exactly this `grok_home` argument
    // (`crate::util::grok_home::grok_home()`, which under these env vars
    // resolves to `state_home`).
    store_api_key(state_home.path(), KEY).expect("store api key");

    // The write must land at the file GROK_AUTH_PATH names, never the
    // default — this is the write half of #434: store_api_key hardcoded
    // `grok_home.join("auth.json")` and never consulted the resolver
    // `AuthManager::new` already honoured (#409).
    assert!(
        auth_path.exists(),
        "store_api_key must write to the file GROK_AUTH_PATH names"
    );
    assert!(
        !default_path.exists(),
        "store_api_key must not fall back to grok_home/auth.json while \
         GROK_AUTH_PATH is set"
    );

    // A fresh, independent read (a different function than the one that just
    // wrote) must see it — proves store_api_key and read_api_key now agree
    // on one file instead of each hardcoding their own.
    assert_eq!(
        read_api_key(state_home.path()).as_deref(),
        Some(KEY),
        "a fresh read_api_key call must see the key store_api_key just wrote"
    );

    // --- Clear: extensions/auth.rs's `handle_set_api_key` calls
    // `clear_api_key` for both the empty-key and absent-key branches.
    clear_api_key(state_home.path()).expect("clear api key");

    // The defect this issue is named for. Before the fix, clear_api_key
    // removed the scope from `grok_home/auth.json` (which never had it —
    // read_auth_json on a missing file just fails, so the whole operation
    // was a silent no-op) and left `auth_path`'s credential completely
    // untouched: a "clear" that does not clear. A fresh read here is the
    // proof a real process restart would see the same thing: still logged
    // in.
    assert_eq!(
        read_api_key(state_home.path()),
        None,
        "a fresh read after clear_api_key must not see the credential it \
         claims to have removed — if this is Some(KEY), the clear touched a \
         different file than the one a fresh read (or a new process) \
         actually consults"
    );

    // clear_api_key removes the file entirely once its scope map is empty
    // (see `auth/storage.rs`), so the resolved file itself should be gone —
    // not just unreadable via one particular scope key.
    assert!(
        !auth_path.exists(),
        "clear_api_key must remove the now-empty file at the resolved GROK_AUTH_PATH location"
    );

    // Never touched the default location at any point while GROK_AUTH_PATH
    // was set — the store half and the clear half must agree on that.
    assert!(
        !default_path.exists(),
        "neither store_api_key nor clear_api_key may have touched grok_home/auth.json \
         while GROK_AUTH_PATH named a different file"
    );
}

/// The sharpest reproduction of #434's own title — "clearing an API key
/// leaves a working credential on disk" — isolated from the round trip
/// above so its red/green cannot be explained by `store_api_key` having its
/// own, separate bug.
///
/// The credential is seeded directly at `auth_path` with
/// `std::fs::write` + `serde_json`, never through `store_api_key`. Only
/// `clear_api_key` is exercised. Before #434's fix, `clear_api_key` operated
/// on `grok_home.join("auth.json")` — a file that never had this scope — so
/// the call was a consequence-free no-op and the seeded credential at
/// `auth_path` survived untouched: a "clear" that does not clear, readable
/// by the very next process start (or, here, by `read_auth_json` called
/// fresh afterward).
#[tokio::test]
#[serial_test::serial]
async fn clear_api_key_alone_must_remove_a_credential_seeded_directly_at_the_grok_auth_path_file() {
    let state_home = tempfile::tempdir().expect("state home");
    let auth_dir = tempfile::tempdir().expect("auth dir");
    let auth_path = auth_dir.path().join("elsewhere.json");
    let default_path = state_home.path().join("auth.json");
    assert_ne!(
        auth_path, default_path,
        "the fixture must keep GROK_AUTH_PATH and grok_home/auth.json distinct"
    );

    // `xai::api_key` — `auth::model::API_KEY_SCOPE`, `pub(super)` so not
    // reachable from an integration test; the literal is the scope
    // `store_api_key`/`clear_api_key`/`read_api_key` all operate on.
    const SCOPE: &str = "xai::api_key";
    const KEY: &str = "token-seeded-directly-for-clear-only-repro";
    let credential = GrokAuth {
        key: KEY.to_owned(),
        auth_mode: AuthMode::ApiKey,
        create_time: Utc::now(),
        expires_at: None,
        ..GrokAuth::default()
    };
    let store: BTreeMap<String, GrokAuth> = [(SCOPE.to_owned(), credential)].into_iter().collect();
    std::fs::write(
        &auth_path,
        serde_json::to_string(&store).expect("serialize auth store"),
    )
    .expect("seed the credential directly at auth_path, bypassing store_api_key");

    let _env = [
        EnvGuard::set("MEDLEY_HOME", state_home.path()),
        EnvGuard::set("GROK_HOME", state_home.path()),
        EnvGuard::set("GROK_AUTH_PATH", &auth_path),
        EnvGuard::unset("GROK_AUTH"),
        EnvGuard::unset("XAI_API_KEY"),
        EnvGuard::unset("GROK_CODE_XAI_API_KEY"),
        EnvGuard::unset("GROK_DEPLOYMENT_KEY"),
    ];

    // Precondition checked against the raw file (`read_auth_json` on
    // `auth_path` directly), not `read_api_key` — this test must not depend
    // on `read_api_key` being fixed either, only on `clear_api_key`.
    let seeded = xai_grok_shell::auth::read_auth_json(&auth_path)
        .expect("the file this test just wrote must be readable");
    assert_eq!(
        seeded.len(),
        1,
        "precondition: exactly the one credential seeded directly at \
         auth_path must be present before clearing"
    );

    // extensions/auth.rs's `handle_set_api_key` calls exactly this for both
    // its empty-key and absent-key branches.
    clear_api_key(state_home.path()).expect("clear api key");

    // Read the resolved file directly (not through `read_api_key`, which
    // this test's sibling round trip already exercises) so this assertion
    // depends on nothing but `clear_api_key` and the raw file. `Err` (file
    // gone — `clear_api_key` deletes an emptied file) and `Ok(map)` with the
    // scope removed are both acceptable; only a surviving `xai::api_key`
    // entry is the defect.
    let scopes_remaining = xai_grok_shell::auth::read_auth_json(&auth_path)
        .map(|store| store.len())
        .unwrap_or(0);
    assert_eq!(
        scopes_remaining, 0,
        "clear_api_key must remove the xai::api_key scope from the file \
         GROK_AUTH_PATH names — if this is nonzero, the credential is still \
         on disk where a fresh process (or AuthManager) will read it back \
         as still-authenticated, which is #434's headline defect"
    );
    assert!(
        !default_path.exists(),
        "clear_api_key must not have created or touched grok_home/auth.json \
         while GROK_AUTH_PATH named a different file"
    );
}

/// Companion to the round trip above, isolating just the "setting writes
/// somewhere `AuthManager` never reads" half of #434 with an OAuth-style
/// credential (not the `xai::api_key` scope `store_api_key` manages) written
/// directly to disk, then read back through `read_auth_json` at the resolved
/// path — matching the shape `managed_config.rs`'s three readers and
/// `agent/app.rs`'s startup auth-key hash use.
#[tokio::test]
#[serial_test::serial]
async fn a_credential_written_only_at_the_grok_auth_path_file_is_visible_through_the_resolver() {
    let state_home = tempfile::tempdir().expect("state home");
    let auth_dir = tempfile::tempdir().expect("auth dir");
    let auth_path = auth_dir.path().join("elsewhere.json");
    let default_path = state_home.path().join("auth.json");
    assert_ne!(auth_path, default_path);

    let config = GrokComConfig::default();
    let credential = GrokAuth {
        key: "token-only-reachable-through-grok-auth-path-2".to_owned(),
        auth_mode: AuthMode::ApiKey,
        create_time: Utc::now(),
        expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
        ..GrokAuth::default()
    };
    let store: BTreeMap<String, GrokAuth> =
        [(config.auth_scope(), credential)].into_iter().collect();
    std::fs::write(
        &auth_path,
        serde_json::to_string(&store).expect("serialize auth store"),
    )
    .expect("write the away-from-home auth.json");

    let _env = [
        EnvGuard::set("MEDLEY_HOME", state_home.path()),
        EnvGuard::set("GROK_HOME", state_home.path()),
        EnvGuard::set("GROK_AUTH_PATH", &auth_path),
        EnvGuard::unset("GROK_AUTH"),
        EnvGuard::unset("XAI_API_KEY"),
        EnvGuard::unset("GROK_CODE_XAI_API_KEY"),
        EnvGuard::unset("GROK_DEPLOYMENT_KEY"),
    ];

    // `managed_config.rs`'s readers and `agent/app.rs`'s startup auth-key
    // hash all go through exactly this pair —
    // `resolved_xai_auth_path(&grok_home())` feeding `read_auth_json` — so
    // exercising the pair directly here covers those call sites without
    // needing their private surrounding functions.
    let home = xai_grok_shell::util::grok_home::grok_home();
    assert_eq!(
        home,
        state_home.path(),
        "GROK_HOME must resolve to state_home"
    );
    let resolved = xai_grok_shell::auth::resolved_xai_auth_path(&home);
    assert_eq!(
        resolved, auth_path,
        "resolved_xai_auth_path must follow GROK_AUTH_PATH in production"
    );
    let store = xai_grok_shell::auth::read_auth_json(&resolved)
        .expect("read the store at the resolved path");
    assert!(
        xai_grok_shell::auth::lookup_auth(&store, &config.auth_scope()).is_some(),
        "the credential written only at GROK_AUTH_PATH must be visible through the resolver"
    );
}
