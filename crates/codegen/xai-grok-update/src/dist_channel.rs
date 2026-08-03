//! Which distribution this binary belongs to, and whether the inherited
//! self-updater is allowed to run.
//!
//! # Why this exists
//!
//! Everything else in this crate is upstream's updater. It resolves versions
//! from xAI's channel pointers, the `@xai-official/grok` npm package, or the
//! `xai-org-shared/grok-build` GitHub releases, and then overwrites the running
//! binary with what it downloaded. That is correct for an official Grok Build
//! install and catastrophic for a medley one: the fork would replace itself
//! with the upstream CLI, losing multi-provider support and orphaning the
//! `~/.medley` state directory it had been using ([#71]).
//!
//! # The rule
//!
//! Fail closed. The inherited updater runs only for a build that can *prove*
//! it belongs to the upstream distribution. Everything else refuses and tells
//! the user how to upgrade by hand. Concretely:
//!
//! | Build stamp (`MEDLEY_CHANNEL`) | Identity                | Self-update |
//! |--------------------------------|-------------------------|-------------|
//! | `medley`                       | [`DistIdentity::Medley`]| refused     |
//! | unset                          | [`DistIdentity::Unstamped`] | refused |
//! | anything else — blank, or even `upstream` | [`DistIdentity::Unknown`] | refused |
//! | *(unstamped + test override)*  | [`DistIdentity::Upstream`]  | allowed |
//!
//! [`DistIdentity::Upstream`] is reachable **only** from the test override on
//! an unstamped build. No value of `MEDLEY_CHANNEL` produces it, so no build
//! of this tree can be compiled back into upstream's updater.
//!
//! A medley build refuses rather than updating because this fork ships through
//! `install.sh` (GitHub releases, SHA-256 verified, launcher + `versions/`
//! layout) — a layout the inherited symlink-swapping installer does not
//! understand. Refusing with guidance is one of the two outcomes [#71] accepts,
//! and the one that cannot corrupt an install.
//!
//! # The test override
//!
//! The upstream update suites still exercise the orchestration code in
//! `auto_update`, so a build with **no** stamp honours
//! [`TEST_DIST_CHANNEL_ENV`] to select an identity. A build that carries a
//! stamp ignores that variable completely, so no published medley binary can be
//! talked into self-updating by its environment. This is strictly narrower than
//! the pre-existing `GROK_TEST_VERSION` override, which every build honours.
//!
//! [#71]: https://github.com/ImL1s/medley/issues/71

use xai_grok_version::{DIST_CHANNEL_ENV, DIST_CHANNEL_STAMP, TEST_DIST_CHANNEL_ENV};

/// Stamp this fork's release workflow bakes into published binaries.
pub const MEDLEY_CHANNEL: &str = "medley";

/// Identity that selects the inherited upstream update channel.
///
/// Valid **only** as a [`TEST_DIST_CHANNEL_ENV`] value on an unstamped build.
/// It is not a valid build stamp: `MEDLEY_CHANNEL=upstream` resolves to
/// [`DistIdentity::Unknown`] and refuses, like any other unrecognised stamp.
pub const UPSTREAM_CHANNEL: &str = "upstream";

/// Repository the installer and releases live in.
pub const MEDLEY_REPO_SLUG: &str = "ImL1s/medley";

/// Git ref a **dev** build points at, having no tag of its own to name.
pub const MEDLEY_DEV_INSTALL_REF: &str = "providers";

/// One-liner that installs or upgrades medley, pinned to the ref this binary
/// was built from.
///
/// A published build names its own tag. Pointing at the `providers` branch
/// instead would mean a binary released today instructs its users to pipe
/// whatever that branch contains *whenever they get round to upgrading* into
/// `sh` — the instruction is compiled in and cannot be corrected afterwards,
/// so it must not resolve to moving content. The tag is immutable and its
/// installer still resolves the newest release, so pinning costs nothing.
///
/// Unstamped builds keep `providers`: they have no tag, and naming a branch is
/// the honest answer rather than a guess.
pub fn medley_install_command() -> String {
    install_command_for(&identity(), xai_grok_version::VERSION)
}

/// Pure policy behind [`medley_install_command`].
///
/// Split out because both inputs are compile-time state: a test binary is
/// always unstamped, so the published-build branch — the one that matters —
/// would be unreachable from a test that called the wrapper.
pub(crate) fn install_command_for(identity: &DistIdentity, version: &str) -> String {
    format!(
        "curl -fsSL https://raw.githubusercontent.com/{}/{}/install.sh | sh",
        MEDLEY_REPO_SLUG,
        install_ref_for(identity, version)
    )
}

/// The ref to pin: this build's tag when it is a stamped medley release,
/// otherwise the development branch.
fn install_ref_for(identity: &DistIdentity, version: &str) -> String {
    match identity {
        // `GROK_VERSION` is set from the tag with its leading `v` stripped, so
        // the tag is recoverable by putting it back. A raw.githubusercontent
        // path takes the `+` in `v1.2.3+providers.4` literally — verified
        // against GitHub, which serves both the bare and percent-encoded form.
        //
        // Gated on the version actually looking like a release, because a
        // stamp alone does not make one: the docs tell developers to build
        // with `MEDLEY_CHANNEL=medley cargo build --release`, and such a build
        // has no `GROK_VERSION`, so it falls back to the Cargo package version.
        // Deriving a ref from that would name `v0.2.117` — a tag this
        // repository never publishes, since the release scheme requires
        // `+providers.<N>`. That 404s, or worse, resolves an unrelated tag
        // inherited from upstream.
        DistIdentity::Medley if is_fork_release_version(version) => format!("v{version}"),
        _ => MEDLEY_DEV_INSTALL_REF.to_string(),
    }
}

/// Whether a version came from a tag this repository publishes.
///
/// Mirrors the scheme the release workflow enforces: `<upstream>+providers.<N>`
/// with a numeric, non-empty counter.
fn is_fork_release_version(version: &str) -> bool {
    let Some((upstream, counter)) = version.split_once("+providers.") else {
        return false;
    };
    !upstream.is_empty() && !counter.is_empty() && counter.bytes().all(|b| b.is_ascii_digit())
}

/// Where medley's release artifacts and checksums are published.
pub const MEDLEY_RELEASES_URL: &str = "https://github.com/ImL1s/medley/releases";

/// Distribution a binary belongs to, as far as it can prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistIdentity {
    /// Built by this fork's release pipeline and shipped as `medley`.
    Medley,
    /// Official upstream distribution semantics.
    Upstream,
    /// No stamp at all: a local `cargo build`, or a rebuild by someone who did
    /// not set [`DIST_CHANNEL_ENV`].
    Unstamped,
    /// Stamped with a value this binary does not recognise — a newer or
    /// third-party distribution. Ambiguous, and treated as such.
    Unknown(String),
}

impl DistIdentity {
    /// Short machine-readable name, for `version --json` and diagnostics.
    ///
    /// A blank stamp reports `unknown` rather than an empty string: the field
    /// is meant to be readable by a human debugging a build, and `""` says
    /// nothing that `unknown` does not say better.
    pub fn name(&self) -> &str {
        match self {
            Self::Medley => MEDLEY_CHANNEL,
            Self::Upstream => UPSTREAM_CHANNEL,
            Self::Unstamped => "unknown",
            Self::Unknown(raw) if raw.trim().is_empty() => "unknown",
            Self::Unknown(raw) => raw,
        }
    }
}

/// Why a build refuses to run the inherited self-updater.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalReason {
    /// A medley build: upstream's channel is the wrong source, and this fork
    /// distributes through `install.sh` rather than an in-binary updater.
    ForkBuild,
    /// The build could not prove which distribution it belongs to.
    AmbiguousIdentity {
        /// The unrecognised stamp, when there was one.
        stamp: Option<String>,
    },
}

/// Whether the inherited self-updater may run in this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfUpdate {
    Allowed,
    Refused(RefusalReason),
}

/// Pure identity resolution.
///
/// `stamp` is the compile-time [`DIST_CHANNEL_STAMP`]; `test_override` is the
/// run-time [`TEST_DIST_CHANNEL_ENV`] value. **Any stamp at all wins** — that
/// asymmetry is what keeps a published binary's identity out of the
/// environment's reach.
///
/// "Any stamp at all" is deliberately the raw `Option`, not a trimmed one. A
/// build that set `MEDLEY_CHANNEL=""` did try to declare an identity and got it
/// wrong; treating that as *unstamped* would hand the environment a way to
/// choose upstream on a build the pipeline had touched. A blank stamp is
/// therefore ambiguous, and ambiguous never consults the override.
pub fn resolve_identity(stamp: Option<&str>, test_override: Option<&str>) -> DistIdentity {
    match stamp {
        Some(raw) => match non_empty(Some(raw)) {
            // A build stamp can only ever declare *this* fork. `upstream` is
            // deliberately not accepted here: a build asserting it is upstream
            // proves nothing, and honouring it would turn a compile-time flag
            // into a switch that re-enables the updater this module exists to
            // disable. Anything that is not `medley`, blank included, is
            // ambiguous.
            Some(MEDLEY_CHANNEL) => DistIdentity::Medley,
            _ => DistIdentity::Unknown(raw.to_string()),
        },
        // Genuinely unstamped builds only: a dev build running the inherited
        // update suites, or a developer reproducing a distribution's behaviour.
        // This is the one path that can reach `Upstream`.
        None => match non_empty(test_override) {
            Some(MEDLEY_CHANNEL) => DistIdentity::Medley,
            Some(UPSTREAM_CHANNEL) => DistIdentity::Upstream,
            Some(other) => DistIdentity::Unknown(other.to_string()),
            None => DistIdentity::Unstamped,
        },
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// This binary's distribution identity.
pub fn identity() -> DistIdentity {
    let test_override = std::env::var(TEST_DIST_CHANNEL_ENV).ok();
    resolve_identity(DIST_CHANNEL_STAMP, test_override.as_deref())
}

/// Pure policy: map an identity onto a self-update decision.
pub fn decide(identity: &DistIdentity) -> SelfUpdate {
    match identity {
        DistIdentity::Upstream => SelfUpdate::Allowed,
        DistIdentity::Medley => SelfUpdate::Refused(RefusalReason::ForkBuild),
        DistIdentity::Unstamped => {
            SelfUpdate::Refused(RefusalReason::AmbiguousIdentity { stamp: None })
        }
        // A blank stamp is reported as "no marker" rather than echoed: quoting
        // an empty string back at the user explains nothing.
        DistIdentity::Unknown(raw) if raw.trim().is_empty() => {
            SelfUpdate::Refused(RefusalReason::AmbiguousIdentity { stamp: None })
        }
        DistIdentity::Unknown(raw) => SelfUpdate::Refused(RefusalReason::AmbiguousIdentity {
            stamp: Some(raw.clone()),
        }),
    }
}

/// User-facing explanation of a refusal, ending with how to upgrade instead.
pub fn refusal_message(reason: &RefusalReason) -> String {
    let install_command = medley_install_command();
    match reason {
        RefusalReason::ForkBuild => format!(
            "medley does not self-update.\n\n\
             This is a medley build — a community fork of Grok Build. The bundled \
             updater installs official Grok Build releases, which would replace \
             medley with the upstream CLI and leave its state directory behind.\n\n\
             To upgrade, re-run the medley installer:\n  {install_command}\n\n\
             Releases and checksums: {MEDLEY_RELEASES_URL}"
        ),
        RefusalReason::AmbiguousIdentity { stamp } => {
            let detail = match stamp {
                Some(raw) => format!(
                    "This build is stamped `{raw}`, which it does not recognise, so it \
                     cannot tell whose releases are the right ones to install."
                ),
                None => format!(
                    "This build carries no `{DIST_CHANNEL_ENV}` marker, so it cannot prove \
                     which distribution it belongs to or whose releases are the right ones \
                     to install."
                ),
            };
            format!(
                "Self-update is disabled: unverified build identity.\n\n\
                 {detail}\n\n\
                 Refusing rather than risk replacing it with a binary from another \
                 distribution. To install a published medley build:\n  \
                 {install_command}\n\n\
                 Releases and checksums: {MEDLEY_RELEASES_URL}"
            )
        }
    }
}

/// `None` when the inherited self-updater may run; otherwise the message to
/// show the user. Every install and download path in `auto_update` consults
/// this before touching the network.
pub fn self_update_refusal() -> Option<String> {
    match decide(&identity()) {
        SelfUpdate::Allowed => None,
        SelfUpdate::Refused(reason) => Some(refusal_message(&reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stamp → identity → decision matrix, including the acceptance
    /// criterion from #71: an ambiguous identity must refuse to update.
    #[test]
    fn identity_matrix_fails_closed() {
        // (stamp, test_override, identity, self-update decision)
        let cases: &[(Option<&str>, Option<&str>, DistIdentity, SelfUpdate)] = &[
            // Released medley binary: refuses, and no environment can change it.
            (
                Some("medley"),
                None,
                DistIdentity::Medley,
                SelfUpdate::Refused(RefusalReason::ForkBuild),
            ),
            // Unstamped local build with nothing set: ambiguous ⇒ refuse.
            (
                None,
                None,
                DistIdentity::Unstamped,
                SelfUpdate::Refused(RefusalReason::AmbiguousIdentity { stamp: None }),
            ),
            // A blank stamp is a botched declaration, not the absence of one:
            // ambiguous, and it must not fall through to the override.
            (
                Some(""),
                None,
                DistIdentity::Unknown(String::new()),
                SelfUpdate::Refused(RefusalReason::AmbiguousIdentity { stamp: None }),
            ),
            (
                Some("   "),
                None,
                DistIdentity::Unknown("   ".to_string()),
                SelfUpdate::Refused(RefusalReason::AmbiguousIdentity { stamp: None }),
            ),
            (
                Some(""),
                Some(UPSTREAM_CHANNEL),
                DistIdentity::Unknown(String::new()),
                SelfUpdate::Refused(RefusalReason::AmbiguousIdentity { stamp: None }),
            ),
            (
                Some("   "),
                Some(UPSTREAM_CHANNEL),
                DistIdentity::Unknown("   ".to_string()),
                SelfUpdate::Refused(RefusalReason::AmbiguousIdentity { stamp: None }),
            ),
            // A stamp from some other distribution is ambiguous, not trusted.
            (
                Some("acme-grok"),
                None,
                DistIdentity::Unknown("acme-grok".to_string()),
                SelfUpdate::Refused(RefusalReason::AmbiguousIdentity {
                    stamp: Some("acme-grok".to_string()),
                }),
            ),
            // `upstream` is not a build stamp. Honouring it would make
            // `MEDLEY_CHANNEL=upstream` a compile-time switch that turns the
            // inherited updater back on, which is the whole thing this module
            // prevents.
            (
                Some(UPSTREAM_CHANNEL),
                None,
                DistIdentity::Unknown("upstream".to_string()),
                SelfUpdate::Refused(RefusalReason::AmbiguousIdentity {
                    stamp: Some("upstream".to_string()),
                }),
            ),
            (
                Some(UPSTREAM_CHANNEL),
                Some(UPSTREAM_CHANNEL),
                DistIdentity::Unknown("upstream".to_string()),
                SelfUpdate::Refused(RefusalReason::AmbiguousIdentity {
                    stamp: Some("upstream".to_string()),
                }),
            ),
            // Surrounding whitespace does not create a new distribution.
            (
                Some(" medley\n"),
                None,
                DistIdentity::Medley,
                SelfUpdate::Refused(RefusalReason::ForkBuild),
            ),
            // Only an unstamped build reads the override.
            (
                None,
                Some("upstream"),
                DistIdentity::Upstream,
                SelfUpdate::Allowed,
            ),
            (
                None,
                Some("medley"),
                DistIdentity::Medley,
                SelfUpdate::Refused(RefusalReason::ForkBuild),
            ),
            (
                None,
                Some(""),
                DistIdentity::Unstamped,
                SelfUpdate::Refused(RefusalReason::AmbiguousIdentity { stamp: None }),
            ),
        ];

        for (stamp, test_override, expected_identity, expected_decision) in cases {
            let identity = resolve_identity(*stamp, *test_override);
            assert_eq!(
                identity, *expected_identity,
                "resolve_identity({stamp:?}, {test_override:?})"
            );
            assert_eq!(
                decide(&identity),
                *expected_decision,
                "decide() for stamp {stamp:?} override {test_override:?}"
            );
        }
    }

    /// The load-bearing asymmetry: a stamped binary ignores the environment, so
    /// a shipped medley build cannot be unlocked into upstream's channel.
    #[test]
    fn build_stamp_beats_environment_override() {
        for attempt in ["upstream", "UPSTREAM", "medley", "", "anything"] {
            let identity = resolve_identity(Some(MEDLEY_CHANNEL), Some(attempt));
            assert_eq!(
                identity,
                DistIdentity::Medley,
                "stamped build must ignore override {attempt:?}"
            );
            assert!(
                matches!(decide(&identity), SelfUpdate::Refused(_)),
                "stamped medley build must refuse to self-update (override {attempt:?})"
            );
        }

        // Every stamp that is not literally absent must resist the override —
        // a malformed one most of all, since that is the case an attacker or a
        // broken pipeline can most easily produce.
        for stamp in [
            "acme-grok",
            "",
            "   ",
            "MEDLEY",
            "medley\u{0}",
            UPSTREAM_CHANNEL,
        ] {
            let identity = resolve_identity(Some(stamp), Some(UPSTREAM_CHANNEL));
            assert_ne!(
                identity,
                DistIdentity::Upstream,
                "stamp {stamp:?} must not be overridable into upstream"
            );
            assert!(
                matches!(decide(&identity), SelfUpdate::Refused(_)),
                "stamp {stamp:?} with an upstream override must still refuse"
            );
        }
    }

    /// `Upstream` — the only identity that permits self-update — must be
    /// unreachable from a build stamp. If this ever fails, `MEDLEY_CHANNEL`
    /// has become a way to compile the inherited updater back on.
    #[test]
    fn no_build_stamp_can_select_upstream() {
        for stamp in [
            UPSTREAM_CHANNEL,
            "Upstream",
            " upstream ",
            "upstream-channel",
            MEDLEY_CHANNEL,
            "",
        ] {
            for override_value in [None, Some(UPSTREAM_CHANNEL), Some("")] {
                let identity = resolve_identity(Some(stamp), override_value);
                assert_ne!(
                    identity,
                    DistIdentity::Upstream,
                    "stamp {stamp:?} with override {override_value:?} reached Upstream"
                );
                assert!(
                    matches!(decide(&identity), SelfUpdate::Refused(_)),
                    "stamp {stamp:?} with override {override_value:?} must refuse"
                );
            }
        }

        // The override on an unstamped build remains the single door to it.
        assert_eq!(
            resolve_identity(None, Some(UPSTREAM_CHANNEL)),
            DistIdentity::Upstream
        );
        assert_eq!(
            decide(&resolve_identity(None, Some(UPSTREAM_CHANNEL))),
            SelfUpdate::Allowed
        );
    }

    /// Channel names are matched exactly; casing is not a distribution.
    #[test]
    fn channel_matching_is_case_sensitive() {
        assert_eq!(
            resolve_identity(Some("Medley"), None),
            DistIdentity::Unknown("Medley".to_string())
        );
        assert_eq!(
            resolve_identity(None, Some("Upstream")),
            DistIdentity::Unknown("Upstream".to_string())
        );
    }

    /// Every refusal has to be actionable: name the installer, and never point
    /// a fork user at an upstream install command.
    #[test]
    fn published_builds_pin_the_installer_to_their_own_tag() {
        // The whole point: a published binary must not send its users to a
        // moving branch. The instruction is compiled in and cannot be
        // corrected after the tag, so whatever it names has to be immutable.
        let published = install_command_for(&DistIdentity::Medley, "0.2.117+providers.1");
        assert!(
            published.contains("/v0.2.117+providers.1/install.sh"),
            "a stamped build must pin its own tag: {published}"
        );
        assert!(
            !published.contains("/providers/install.sh"),
            "a stamped build must not point at the branch: {published}"
        );

        // Everything that cannot prove it is a published build has no tag to
        // name, so the branch is the honest answer rather than a guess.
        for identity in [
            DistIdentity::Unstamped,
            DistIdentity::Upstream,
            DistIdentity::Unknown("acme-grok".to_string()),
        ] {
            let dev = install_command_for(&identity, "0.1.220-alpha.4");
            assert!(
                dev.contains("/providers/install.sh"),
                "{identity:?} has no tag, so it must name the branch: {dev}"
            );
            assert!(
                !dev.contains("0.1.220-alpha.4"),
                "{identity:?} must not fabricate a tag from a dev version: {dev}"
            );
        }
    }

    /// A stamp is not a release. The docs tell developers to build with
    /// `MEDLEY_CHANNEL=medley cargo build --release`, and such a build has no
    /// `GROK_VERSION` — so its version is the Cargo package version, and
    /// deriving a ref from it would name a tag this repository never
    /// publishes. That 404s, or resolves an unrelated tag inherited from
    /// upstream, and it is baked into the binary either way.
    #[test]
    fn a_stamp_without_a_release_version_still_names_the_branch() {
        for version in [
            // What a local `MEDLEY_CHANNEL=medley` build actually reports.
            "0.2.117",
            "0.1.220-alpha.4",
            // Shapes that resemble the scheme without matching it.
            "0.2.117+providers",
            "0.2.117+providers.",
            "0.2.117+providers.x",
            "0.2.117+providers.1a",
            "+providers.1",
        ] {
            let cmd = install_command_for(&DistIdentity::Medley, version);
            assert!(
                cmd.contains("/providers/install.sh"),
                "{version:?} is not a release version, so it must name the branch: {cmd}"
            );
        }

        // And the real thing still pins.
        for version in ["0.2.117+providers.1", "1.2.3+providers.42"] {
            let cmd = install_command_for(&DistIdentity::Medley, version);
            assert!(
                cmd.contains(&format!("/v{version}/install.sh")),
                "{version:?} follows the release scheme and must pin: {cmd}"
            );
        }
    }

    #[test]
    fn refusal_messages_point_at_the_medley_installer() {
        let reasons = [
            RefusalReason::ForkBuild,
            RefusalReason::AmbiguousIdentity { stamp: None },
            RefusalReason::AmbiguousIdentity {
                stamp: Some("acme-grok".to_string()),
            },
        ];
        for reason in reasons {
            let message = refusal_message(&reason);
            assert!(
                message.contains("install.sh"),
                "refusal must name the installer: {message}"
            );
            assert!(
                message.contains(MEDLEY_RELEASES_URL),
                "refusal must link the fork's releases: {message}"
            );
            // Asserted as a literal, not via the constants above: the message
            // is *built* from those constants, so comparing against them
            // proves only that the format string interpolated. Reverting both
            // constants to the pre-rename slug would still pass that check,
            // and these URLs are compiled into every published binary — an
            // undetected revert ships permanently.
            assert!(
                message.contains("ImL1s/medley"),
                "refusal must name the canonical repository: {message}"
            );
            for upstream in [
                "x.ai/cli",
                "@xai-official/grok",
                "xai-org-shared/grok-build",
                // The pre-rename slug. Redirects keep it working, which is
                // exactly why a regression here would go unnoticed.
                "ImL1s/grok-build",
            ] {
                assert!(
                    !message.contains(upstream),
                    "refusal must not send users to {upstream}: {message}"
                );
            }
        }
        assert!(
            refusal_message(&RefusalReason::AmbiguousIdentity {
                stamp: Some("acme-grok".to_string()),
            })
            .contains("acme-grok"),
            "an unrecognised stamp should be echoed so the user can see it"
        );
    }

    /// `name()` is what `version --json` reports; an unstamped build must not
    /// claim a distribution it cannot prove.
    #[test]
    fn identity_names_are_stable() {
        assert_eq!(DistIdentity::Medley.name(), "medley");
        assert_eq!(DistIdentity::Upstream.name(), "upstream");
        assert_eq!(DistIdentity::Unstamped.name(), "unknown");
        assert_eq!(DistIdentity::Unknown("acme".to_string()).name(), "acme");
        assert_eq!(DistIdentity::Unknown(String::new()).name(), "unknown");
        assert_eq!(DistIdentity::Unknown("  ".to_string()).name(), "unknown");
    }
}
