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
//! | anything else, blank included  | [`DistIdentity::Unknown`]   | refused |
//! | *(unstamped + test override)*  | [`DistIdentity::Upstream`]  | allowed |
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
//! [#71]: https://github.com/ImL1s/grok-build/issues/71

use xai_grok_version::{DIST_CHANNEL_ENV, DIST_CHANNEL_STAMP, TEST_DIST_CHANNEL_ENV};

/// Stamp this fork's release workflow bakes into published binaries.
pub const MEDLEY_CHANNEL: &str = "medley";

/// Identity that selects the inherited upstream update channel. The release
/// workflow never emits it; only an unstamped dev build can select it, via
/// [`TEST_DIST_CHANNEL_ENV`].
pub const UPSTREAM_CHANNEL: &str = "upstream";

/// One-liner that installs or upgrades medley.
pub const MEDLEY_INSTALL_COMMAND: &str =
    "curl -fsSL https://raw.githubusercontent.com/ImL1s/grok-build/providers/install.sh | sh";

/// Where medley's release artifacts and checksums are published.
pub const MEDLEY_RELEASES_URL: &str = "https://github.com/ImL1s/grok-build/releases";

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
            Some(value) => classify(value),
            // Stamped, but with nothing usable in it.
            None => DistIdentity::Unknown(raw.to_string()),
        },
        // Genuinely unstamped builds only: a dev build running the inherited
        // update suites, or a developer reproducing a distribution's behaviour.
        None => match non_empty(test_override) {
            Some(value) => classify(value),
            None => DistIdentity::Unstamped,
        },
    }
}

fn classify(value: &str) -> DistIdentity {
    match value {
        MEDLEY_CHANNEL => DistIdentity::Medley,
        UPSTREAM_CHANNEL => DistIdentity::Upstream,
        other => DistIdentity::Unknown(other.to_string()),
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
    match reason {
        RefusalReason::ForkBuild => format!(
            "medley does not self-update.\n\n\
             This is a medley build — a community fork of Grok Build. The bundled \
             updater installs official Grok Build releases, which would replace \
             medley with the upstream CLI and leave its state directory behind.\n\n\
             To upgrade, re-run the medley installer:\n  {MEDLEY_INSTALL_COMMAND}\n\n\
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
                 {MEDLEY_INSTALL_COMMAND}\n\n\
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
        for stamp in ["acme-grok", "", "   ", "MEDLEY", "medley\u{0}"] {
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
            for upstream in [
                "x.ai/cli",
                "@xai-official/grok",
                "xai-org-shared/grok-build",
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
