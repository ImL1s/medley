//! Installed grok CLI version, lockstepped with shipping binaries.

use semver::Version;

pub const TEST_VERSION_ENV: &str = "GROK_TEST_VERSION";

pub const VERSION: &str = match option_env!("GROK_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Build-time env var carrying the distribution channel marker.
///
/// Set by this fork's release workflow alongside `GROK_VERSION`. A build that
/// does not set it produces a binary with no distribution identity.
pub const DIST_CHANNEL_ENV: &str = "MEDLEY_CHANNEL";

/// Env var a **dev build** may set to choose a distribution identity at run
/// time. Deliberately ignored by any binary that carries [`DIST_CHANNEL_STAMP`],
/// so a published build's identity cannot be overridden from the environment.
pub const TEST_DIST_CHANNEL_ENV: &str = "GROK_TEST_DIST_CHANNEL";

/// Distribution channel baked in at compile time from `MEDLEY_CHANNEL`.
///
/// `None` on any build that did not set it — a local `cargo build`, a
/// third-party rebuild, or an upstream build that never had the variable. The
/// updater treats `None` as an unproven identity and refuses to self-update
/// (see `xai_grok_update::dist_channel`), because a binary that cannot say
/// which distribution it belongs to also cannot say whose releases are the
/// right ones to install.
pub const DIST_CHANNEL_STAMP: Option<&str> = option_env!("MEDLEY_CHANNEL");

/// Target triple this binary was built for, from Cargo's `TARGET`.
///
/// `None` only if something built this crate without a build script running.
/// There is no runtime equivalent: `std::env::consts` reports the arch and OS
/// separately and cannot distinguish `gnu` from `musl`, which is precisely the
/// distinction that decides whether a Linux archive runs on a given machine.
pub const BUILD_TARGET: Option<&str> = option_env!("MEDLEY_BUILD_TARGET");

/// Upstream commit this tree was synced from, read at build time from
/// `SOURCE_REV` at the workspace root.
///
/// `None` for a build with no `SOURCE_REV` in scope — a vendored crate or a
/// package tarball. A fork release states its base so "which upstream is this
/// built on" is answerable from the binary alone, without the repository.
pub const UPSTREAM_BASE: Option<&str> = option_env!("MEDLEY_UPSTREAM_BASE");

/// [`TEST_VERSION_ENV`] override first, then [`VERSION`]. Trimmed so
/// non-semver-aware callers can pass the result straight into parsing.
pub fn installed() -> String {
    std::env::var(TEST_VERSION_ENV)
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| VERSION.to_string())
}

pub fn installed_semver() -> Result<Version, semver::Error> {
    Version::parse(&installed())
}

/// Format the compiled version with a channel label for user-facing display.
///
/// `channel_label` is a pre-formatted suffix such as `" [alpha]"`, `" [stable]"`,
/// or `""` (empty when no cached pointer is available). Obtain it from
/// `xai_grok_update::channel_label()`.
///
/// Example: `"0.2.5 [stable]"` or `"0.2.5 [alpha]"`.
pub fn display_version(channel_label: &str) -> String {
    format!("{}{}", VERSION, channel_label)
}

/// Format a version-with-commit string with a channel label.
///
/// Same semantics as [`display_version`] but for the full
/// `"0.2.5 (abc1234)"` string.
pub fn display_version_with_commit(version_with_commit: &str, channel_label: &str) -> String {
    format!("{}{}", version_with_commit, channel_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Display formatting invariant matrix — verifies label appending
    /// works correctly across all label states (alpha, stable, empty).
    #[test]
    fn test_display_version_formatting_matrix() {
        let cases: &[(&str, &str, &str)] = &[
            // (version_with_commit,    label,        expected_suffix)
            ("0.2.5 (abc1234)", " [alpha]", "0.2.5 (abc1234) [alpha]"),
            ("0.2.5 (abc1234)", " [stable]", "0.2.5 (abc1234) [stable]"),
            ("0.2.5 (abc1234)", "", "0.2.5 (abc1234)"),
            (
                "0.1.220-alpha.2 (def0)",
                " [alpha]",
                "0.1.220-alpha.2 (def0) [alpha]",
            ),
        ];
        for (vwc, label, expected) in cases {
            assert_eq!(
                display_version_with_commit(vwc, label),
                *expected,
                "display_version_with_commit({:?}, {:?})",
                vwc,
                label,
            );
        }
        // display_version uses compiled VERSION — just verify the label appends
        assert_eq!(display_version(""), VERSION);
        assert!(display_version(" [stable]").ends_with("[stable]"));
    }
}

#[cfg(test)]
mod build_stamp_tests {
    /// Both constants are wired through the build script, and a build script
    /// that silently fails to set an env var leaves `option_env!` as `None` —
    /// no compile error, no warning, just a field that reports null forever.
    /// These assert the wiring, not the values.
    #[test]
    fn the_build_target_is_a_full_triple() {
        let target = super::BUILD_TARGET.expect("Cargo always sets TARGET for build scripts");
        assert!(
            target.matches('-').count() >= 2,
            "expected a triple like aarch64-apple-darwin, got {target:?} — \
             arch and OS alone cannot tell gnu from musl"
        );
    }

    /// Holds in a checkout, which is where tests run. A vendored or packaged
    /// build has no `SOURCE_REV` and correctly reports `None`; that case
    /// cannot be reached from here, so this only pins the checkout behaviour.
    #[test]
    fn the_upstream_base_is_a_commit_id() {
        let base =
            super::UPSTREAM_BASE.expect("SOURCE_REV sits at the workspace root of every checkout");
        assert_eq!(
            base.len(),
            40,
            "expected a full git object id, got {base:?}"
        );
        assert!(
            base.bytes().all(|b| b.is_ascii_hexdigit()),
            "expected hex, got {base:?}"
        );
        assert_eq!(base.trim(), base, "the trailing newline was not trimmed");
    }
}
