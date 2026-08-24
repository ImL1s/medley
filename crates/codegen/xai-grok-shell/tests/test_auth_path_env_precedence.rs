//! Production must honour `GROK_AUTH_PATH` (#409).
//!
//! #409 closed that channel under `cfg(test)` on both auth-path resolvers, and
//! flipped the in-crate fixture that used to assert the opposite. That fixture
//! is a `cfg(test)` build and structurally cannot test production behaviour any
//! more, so without this file nothing would assert that the variable is honoured
//! at all.
//!
//! Its own integration target, for two reasons that are not interchangeable:
//!
//! - **Integration, not unit.** `cargo test` links the lib *without*
//!   `cfg(test)` for integration targets, and the env-reading branch of
//!   `resolved_xai_auth_path` only exists in that build. No `#[test]` inside the
//!   crate can reach it.
//! - **Its own target, not a sibling test in an existing one.** `grok_home()`
//!   memoises into a process-wide `OnceLock`, so one test binary can only ever
//!   have one state directory. This test needs `grok_home` and the auth path to
//!   be *different* directories — that is the entire point — so it cannot share
//!   a binary with a test that pins `grok_home` somewhere else.
//!
//! `tests/test_auth_provider_command_e2e.rs` also sets `GROK_AUTH_PATH`, but to
//! `grok_home/auth.json` — the very file the default branch resolves. The
//! env-honouring branch and the default branch land on the same path there, so
//! that test passes identically whether production reads the variable or ignores
//! it entirely. It sets the variable to *neutralise* an ambient one, not to
//! exercise it. Keeping the two paths distinct below is what makes this test
//! able to tell the branches apart.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use xai_grok_shell::auth::{AuthManager, AuthMode, GrokAuth, GrokComConfig, try_ensure_fresh_auth};
use xai_grok_test_support::EnvGuard;

/// Written only to the away-from-home auth file, never to `grok_home/auth.json`.
/// Reading it back is therefore proof that the resolver followed the env var.
const ENV_PATH_TOKEN: &str = "token-only-reachable-through-grok-auth-path";

#[tokio::test]
#[serial_test::serial]
async fn production_reads_the_grok_auth_path_file_outside_grok_home() {
    let state_home = tempfile::tempdir().expect("state home");
    let auth_dir = tempfile::tempdir().expect("auth dir");
    let auth_path = auth_dir.path().join("elsewhere.json");
    let default_path = state_home.path().join("auth.json");

    let config = GrokComConfig::default();
    let credential = GrokAuth {
        key: ENV_PATH_TOKEN.to_owned(),
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

    // The discriminator. If these two ever became the same path, every
    // assertion below would pass whether or not production reads the variable
    // — which is exactly how the previous coverage claim was wrong.
    assert_ne!(
        auth_path, default_path,
        "the fixture must keep GROK_AUTH_PATH and grok_home/auth.json distinct, \
         or this test cannot tell the two resolution branches apart"
    );
    assert!(
        !default_path.exists(),
        "precondition: grok_home/auth.json must be absent, so only the env path can satisfy a read"
    );

    // An array, not a `vec!` — clippy rejects the allocation, and the binding
    // keeps every guard alive to the end of the test either way.
    let _env = [
        EnvGuard::set("MEDLEY_HOME", state_home.path()),
        EnvGuard::set("GROK_HOME", state_home.path()),
        EnvGuard::set("GROK_AUTH_PATH", &auth_path),
        // An inline credential satisfies `AuthManager::new` before it resolves
        // any path at all, so leaving this set would fake a pass.
        EnvGuard::unset("GROK_AUTH"),
        EnvGuard::unset("XAI_API_KEY"),
        EnvGuard::unset("GROK_CODE_XAI_API_KEY"),
        EnvGuard::unset("GROK_DEPLOYMENT_KEY"),
    ];

    // The production composition root: it resolves `grok_home()` itself and
    // hands the result to `AuthManager::new`, so this covers the whole chain a
    // real startup walks.
    let via_startup = try_ensure_fresh_auth(&config)
        .await
        .expect("production startup auth must load the credential named by GROK_AUTH_PATH");
    assert_eq!(
        via_startup.key, ENV_PATH_TOKEN,
        "startup auth resolved a different file than GROK_AUTH_PATH named"
    );

    // And the constructor on its own, which never consults `grok_home()` — so
    // this half keeps its meaning no matter what pinned the `OnceLock` first.
    let via_constructor = Arc::new(AuthManager::new(state_home.path(), config.clone()))
        .auth()
        .await
        .expect("AuthManager::new must honour GROK_AUTH_PATH in a non-cfg(test) build");
    assert_eq!(
        via_constructor.key, ENV_PATH_TOKEN,
        "the constructor resolved a different file than GROK_AUTH_PATH named"
    );

    // The write half: nothing may have quietly fallen back to the default
    // location, which is how #434's split between readers and writers shows up.
    assert!(
        !default_path.exists(),
        "production must neither read nor write grok_home/auth.json while \
         GROK_AUTH_PATH names another file"
    );
}
