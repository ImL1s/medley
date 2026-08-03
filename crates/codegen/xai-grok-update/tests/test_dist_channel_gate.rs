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

use common::{FakeBinGuard, make_update_config, reset_home, set_test_version, test_home};
use xai_grok_update::auto_update::{
    UpdateRunMode, auto_update_target, check_update_background, check_update_status,
    ensure_latest_on_disk, run_install_script, run_update, run_update_if_available,
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
        // SAFETY: every test in this file is `#[serial]`.
        unsafe {
            match dist_channel_override {
                Some(value) => std::env::set_var("GROK_TEST_DIST_CHANNEL", value),
                None => std::env::remove_var("GROK_TEST_DIST_CHANNEL"),
            }
            std::env::set_var("GROK_INSTALLER", "npm");
        }
        let npm = FakeBinGuard::install_npm();
        let gh = FakeBinGuard::install_gh();
        npm.set_stdout("\"9.9.9\"\n");
        gh.set_stable_only_stdout("v9.9.9\n");
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

    // The primitive that overwrites the binary refuses on its own authority.
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

/// Control: with the upstream identity selected, the very same call does reach
/// the installer. Without this, every assertion above could pass because the
/// fixture was broken rather than because the gate works.
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
        !fixture.npm.args_log().is_empty(),
        "the control must actually consult npm, or the refusals above prove nothing"
    );
}
