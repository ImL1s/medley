//! End-to-end proof that a build which cannot prove its distribution identity
//! never reaches upstream's release channel (issue #71).
//!
//! The unit tests in `dist_channel` cover the identity matrix; these cover the
//! wiring: that every entry point in `auto_update` consults it, that none of
//! them invokes an installer, and that nothing lands in `GROK_HOME`.
//!
//! `common::test_home` selects the `upstream` identity for the other suites,
//! which exercise the inherited updater. These tests clear that selection so
//! the process looks like an unstamped build — the state a local `cargo build`
//! ships in, and the "ambiguous identity" case the issue requires to fail
//! closed. [`control_upstream_identity_still_reaches_the_installer`] puts the
//! selection back, so the refusals above are not passing for some unrelated
//! reason.

#![cfg(unix)]

mod common;

use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{FakeBinGuard, make_update_config, reset_home, set_test_version, test_home};
use xai_grok_update::auto_update::{
    UpdateRunMode, apply_channel_switch, auto_update_target, check_update_background,
    check_update_status, ensure_latest_on_disk, install_internal_from_base,
    install_internal_from_bases, install_npm_for_test, run_install_script, run_update,
    run_update_if_available,
};
use xai_grok_update::dist_channel;

/// Fake `npm` + `gh` on PATH, an npm installer selection, and no distribution
/// identity. Any invocation of either fake is a gate failure.
struct Fixture {
    npm: FakeBinGuard,
    gh: FakeBinGuard,
}

impl Fixture {
    fn new(dist_channel_override: Option<&str>) -> Self {
        let _ = test_home();
        reset_home();
        set_test_version("0.1.181");
        let npm = FakeBinGuard::install_npm();
        let gh = FakeBinGuard::install_gh();
        npm.set_stdout("\"9.9.9\"\n");
        gh.set_stable_only_stdout("v9.9.9\n");
        // Last, so it overrides the upstream identity that `test_home` and
        // `FakeBinGuard::install` select for the suites that need it.
        // SAFETY: every test in this file is `#[serial]`.
        unsafe {
            match dist_channel_override {
                Some(value) => std::env::set_var("GROK_TEST_DIST_CHANNEL", value),
                None => std::env::remove_var("GROK_TEST_DIST_CHANNEL"),
            }
            std::env::set_var("GROK_INSTALLER", "npm");
        }
        Self { npm, gh }
    }

    /// No installer was invoked, so nothing could have been downloaded.
    fn assert_no_installer_ran(&self, context: &str) {
        assert!(
            self.npm.args_log().is_empty(),
            "{context}: npm was invoked: {:?}",
            self.npm.args_log()
        );
        assert!(
            self.gh.args_log().is_empty(),
            "{context}: gh was invoked: {:?}",
            self.gh.args_log()
        );
    }

    /// Nothing was installed into or recorded in `GROK_HOME`.
    fn assert_home_untouched(&self, context: &str) {
        let home = test_home();
        for artifact in ["bin", "downloads", "version.json"] {
            assert!(
                !home.join(artifact).exists(),
                "{context}: {artifact} was created in GROK_HOME"
            );
        }
    }
}

/// The acceptance criterion from #71: a binary with no distribution marker
/// refuses at every entry point rather than updating from upstream's channel.
#[tokio::test]
#[serial]
async fn ambiguous_identity_refuses_every_update_entry_point() {
    let fixture = Fixture::new(None);
    let mut config = make_update_config("stable");

    // Sanity: this process really is the ambiguous case under test.
    assert_eq!(
        dist_channel::identity(),
        dist_channel::DistIdentity::Unstamped,
        "fixture must present an unstamped build"
    );

    // `grok update --check` reports the refusal and advertises nothing.
    let status = check_update_status(&config).await;
    let reason = status
        .self_update_disabled
        .as_deref()
        .expect("--check must report why self-update is disabled");
    assert!(
        reason.contains("install.sh"),
        "refusal must point at the installer: {reason}"
    );
    assert!(!status.update_available, "must not advertise an update");
    assert_eq!(
        status.latest_version, None,
        "must not report an upstream version it would never install"
    );
    assert_eq!(status.error, None, "a refusal is not a failed check");

    // Leader convergence and the background TUI download.
    assert_eq!(auto_update_target(&config).await, None);
    let outcome = ensure_latest_on_disk(&config)
        .await
        .expect("refusing is not an error for the leader path");
    assert_eq!(outcome.installed, None);
    assert!(!outcome.relaunch_needed);
    let background = check_update_background(&config).await;
    assert!(background.update.is_none(), "no restart hint");
    assert!(background.download.is_none(), "no background downloader");

    // Launch-time auto-update.
    assert!(
        !run_update_if_available(UpdateRunMode::NonBlocking, false, &config)
            .await
            .expect("refusing is not an error at launch"),
        "launch-time auto-update must not run"
    );

    // Explicit `grok update`, including the flags that force the issue.
    assert_eq!(
        run_update(false, None, None, &mut config).await.unwrap(),
        None
    );
    assert_eq!(
        run_update(true, None, None, &mut config).await.unwrap(),
        None,
        "--force must not bypass the refusal"
    );
    assert_eq!(
        run_update(false, Some("9.9.9"), None, &mut config)
            .await
            .unwrap(),
        None,
        "--version must not bypass the refusal"
    );

    // The orchestrating install entry point refuses on its own authority.
    for installer in ["npm", "gh-release", "internal"] {
        let Err(err) = run_install_script(installer, Some("9.9.9"), &config).await else {
            panic!("install must be refused for {installer}");
        };
        assert!(
            err.to_string().contains("install.sh"),
            "{installer}: refusal must point at the installer: {err}"
        );
    }

    fixture.assert_no_installer_ran("after every entry point");
    fixture.assert_home_untouched("after every entry point");
}

/// `run_install_script` is not the only door. `install_npm_for_test` and
/// `install_internal_from_base*` are public and skip it entirely, so the
/// irreversible leaves behind them carry the check too. Without this, a caller
/// that never touches the orchestration layer would still land on upstream.
#[tokio::test]
#[serial]
async fn ambiguous_identity_refuses_the_public_install_primitives() {
    let fixture = Fixture::new(None);
    let config = make_update_config("stable");

    let err = install_npm_for_test(Some("9.9.9"), "stable", None)
        .expect_err("the npm install primitive must be refused");
    assert!(
        err.to_string().contains("install.sh"),
        "refusal must point at the installer: {err}"
    );

    // The internal installer must refuse *before* fetching. Downloading is not
    // harmless here: the download phase writes the artifact into the state dir
    // and then executes it to smoke-test that it runs. `expect(0)` fails the
    // test on server drop if either endpoint is touched at all.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/stable"))
        .respond_with(ResponseTemplate::new(200).set_body_string("9.9.9"))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/grok-9.9.9-{}", common::host_platform())))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(common::small_good_artifact()))
        .expect(0)
        .mount(&server)
        .await;

    let err = install_internal_from_base(Some("9.9.9"), &config, &server.uri())
        .await
        .expect_err("the internal install primitive must be refused");
    assert!(
        err.to_string().contains("install.sh"),
        "refusal must point at the installer: {err}"
    );

    let err = install_internal_from_bases(Some("9.9.9"), &config, &[server.uri().as_str()])
        .await
        .expect_err("the multi-base internal install primitive must be refused");
    assert!(
        err.to_string().contains("install.sh"),
        "refusal must point at the installer: {err}"
    );

    fixture.assert_no_installer_ran("after the public install primitives");
    fixture.assert_home_untouched("after the public install primitives");
}

/// `update --check --alpha` reaches `apply_channel_switch` before the gated
/// status call, so the switch itself has to refuse. Otherwise a refused check
/// still leaves `channel = "alpha"` in config — a setting that does nothing.
#[tokio::test]
#[serial]
async fn ambiguous_identity_refuses_a_channel_switch() {
    let fixture = Fixture::new(None);
    let mut config = make_update_config("stable");

    apply_channel_switch(Some("alpha"), &mut config).await;

    assert_eq!(config.channel, "stable", "channel must not be switched");
    assert!(
        !test_home().join("config.toml").exists(),
        "a refused channel switch must not write config"
    );
    fixture.assert_no_installer_ran("after a refused channel switch");
}

/// A refused `grok update --stable/--alpha` must not persist the switch: the
/// channel only selects between upstream's pointers, which this build never
/// reads. Also proves the gate runs before any config write.
#[tokio::test]
#[serial]
async fn refused_update_does_not_persist_a_channel_switch() {
    let fixture = Fixture::new(None);
    let mut config = make_update_config("stable");

    assert_eq!(
        run_update(false, None, Some("alpha"), &mut config)
            .await
            .unwrap(),
        None
    );

    assert_eq!(config.channel, "stable", "channel must not be switched");
    assert!(
        !test_home().join("config.toml").exists(),
        "a refused update must not write config"
    );
    fixture.assert_no_installer_ran("after a refused channel switch");
    fixture.assert_home_untouched("after a refused channel switch");
}

/// Control: with the upstream identity selected, the very same calls reach both
/// the version lookup and the installer. Without this, every assertion above
/// could pass because the fixture was broken rather than because the gate works
/// — and it pins down exactly what the gate is suppressing: an `npm view`, and
/// an `npm i -g @xai-official/grok`.
#[tokio::test]
#[serial]
async fn control_upstream_identity_still_reaches_the_installer() {
    let fixture = Fixture::new(Some("upstream"));
    let config = make_update_config("stable");

    let status = check_update_status(&config).await;
    assert_eq!(
        status.self_update_disabled, None,
        "an upstream build must not report a refusal"
    );
    assert_eq!(status.latest_version.as_deref(), Some("9.9.9"));
    assert!(
        fixture.npm.args_log().iter().any(|a| a.contains("view")),
        "the control must consult npm for a version: {:?}",
        fixture.npm.args_log()
    );

    run_install_script("npm", Some("9.9.9"), &config)
        .await
        .expect("an upstream build must be allowed to install");
    assert!(
        fixture
            .npm
            .args_log()
            .iter()
            .any(|a| a.contains("@xai-official/grok@9.9.9")),
        "the control must reach the upstream install the gate exists to stop: {:?}",
        fixture.npm.args_log()
    );
}
