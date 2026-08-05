//! The name this program was invoked as.
//!
//! Lives here, not in the pager, because both `xai-grok-pager` and
//! `xai-grok-shell` need it and the dependency runs pager → shell: an accessor
//! in the pager is unreachable from the shell, which is where the routine
//! 401/expiry copy lives (#117).
//!
//! Why it matters that instructions carry the right name: this fork ships as
//! `medley` and is designed to coexist with an official `grok` on the same
//! machine, with separate state directories. An instruction that says
//! `grok login` when the user ran `medley` **succeeds against the other
//! program** — it writes a credential file this one does not read — and the
//! user returns here still unauthenticated with nothing having errored.

use std::sync::OnceLock;

/// Used when `argv[0]` is absent or not a plain program name.
///
/// This is the upstream name, and it is the least-bad answer in a usage line.
/// In an *instruction* it is actively wrong, which is why
/// [`program_name_for_instruction`] exists.
pub const FALLBACK_PROGRAM_NAME: &str = "grok";

/// Whether `name` is a plain program name, safe to render into output a
/// terminal will interpret.
///
/// `argv[0]` is chosen by whoever execs us and is printed into usage lines and
/// error messages, so a name carrying control characters, escape sequences or
/// newlines could rewrite the surrounding display. Anything that is not a plain
/// name is refused rather than sanitised — truncating or stripping would still
/// put caller-chosen text on screen.
pub fn is_plain_program_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The file name of `argv0`, screened by [`is_plain_program_name`].
///
/// Takes `OsStr`, not `&str`: `std::env::args()` panics on a non-UTF-8
/// argument, and `argv[0]` is chosen by whoever execs us. Since this is now
/// reached from `Display` impls on error types, a panic here would abort the
/// process while it was formatting an error message.
fn program_name_from_argv0(argv0: Option<&std::ffi::OsStr>) -> Option<&str> {
    argv0
        .map(std::path::Path::new)
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .filter(|n| is_plain_program_name(n))
}

/// Both answers, resolved once.
///
/// `argv[0]` cannot change for the life of the process, and deriving the two
/// together is what makes them incapable of disagreeing — an earlier version
/// re-read the environment on every `program_name_for_instruction()` call,
/// which allocated inside `Display` and left a seam where they could differ.
struct Resolved {
    /// Always a plain name: the screened `argv[0]`, or the fallback.
    name: &'static str,
    /// Whether `name` came from `argv[0]` rather than from falling back.
    from_argv0: bool,
}

static RESOLVED: OnceLock<Resolved> = OnceLock::new();

fn resolved() -> &'static Resolved {
    RESOLVED.get_or_init(|| {
        let argv0 = std::env::args_os().next();
        match program_name_from_argv0(argv0.as_deref()) {
            Some(name) => Resolved {
                name: Box::leak(name.to_owned().into_boxed_str()),
                from_argv0: true,
            },
            None => Resolved {
                name: FALLBACK_PROGRAM_NAME,
                from_argv0: false,
            },
        }
    })
}

/// The name this program was invoked as.
///
/// Self-initializing from `argv[0]`, deliberately: `argv[0]` is immutable for
/// the life of the process, so a pull is always correct and needs no ordering
/// guarantee. An earlier version had `parse_cli` *push* the value in, which
/// made every caller's correctness depend on a call order nothing enforced —
/// and left library consumers, and any test that formats a message without
/// parsing arguments, reading the fallback.
pub fn program_name() -> &'static str {
    resolved().name
}

/// The name to use inside an instruction the user is expected to *type*.
///
/// `None` means `argv[0]` gave us nothing usable. Callers must then phrase the
/// message **without naming a command at all** — the fallback is the name of a
/// *different program that may be installed*, so telling the user to run it is
/// the bug this module exists to prevent, and an instruction with the binary
/// amputated ("run `login --provider x`") is worse than one that does not try.
///
/// Reachable in practice, not just in theory: a binary copied to `medley (1)`
/// by a browser's "keep both", renamed to something non-ASCII, or exec'd with a
/// non-UTF-8 `argv[0]`, all fail the screening.
pub fn program_name_for_instruction() -> Option<&'static str> {
    resolved().from_argv0.then_some(resolved().name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn from(argv0: &str) -> Option<&str> {
        program_name_from_argv0(Some(OsStr::new(argv0)))
    }

    #[test]
    fn plain_names_are_taken_whatever_they_are() {
        for (argv0, want) in [
            ("medley", "medley"),
            ("/usr/local/bin/medley", "medley"),
            ("./medley", "medley"),
            ("grok", "grok"),
            ("agent", "agent"),
            ("xai-grok-pager", "xai-grok-pager"),
            // A trailing separator has no file name component of its own.
            ("/usr/bin/", "bin"),
        ] {
            assert_eq!(from(argv0), Some(want), "argv[0] {argv0:?}");
        }
        assert_eq!(
            from(&"m".repeat(64)).map(str::to_owned),
            Some("m".repeat(64))
        );
    }

    /// `argv[0]` is caller-chosen and is printed into a terminal, so anything
    /// that could rewrite the display must be refused rather than echoed.
    #[test]
    fn hostile_or_missing_argv0_is_refused() {
        for argv0 in [
            "",
            "med\u{1b}[2Jley",  // escape sequence: clears the screen
            "med\nUsage: sudo", // newline: forges a second output line
            "med\u{7}ley",      // control character
            "med ley",          // space
            "медley",           // non-ASCII
            "medley (1)",       // a browser's "keep both" copy
        ] {
            assert_eq!(from(argv0), None, "argv[0] {argv0:?} must be refused");
        }
        assert_eq!(program_name_from_argv0(None), None);
        // Refused, not truncated: truncation still prints caller-chosen text.
        assert_eq!(from(&"m".repeat(65)), None);
    }

    /// `std::env::args()` panics on a non-UTF-8 argument. This is reached from
    /// `Display` impls on error types, so that would abort the process while it
    /// was formatting an error message.
    #[test]
    #[cfg(unix)]
    fn non_utf8_argv0_is_refused_rather_than_panicking() {
        use std::os::unix::ffi::OsStrExt;
        let argv0 = OsStr::from_bytes(b"med\xffley");
        assert_eq!(program_name_from_argv0(Some(argv0)), None);
    }

    /// The accessor must be safe to call before anything initialises it, and
    /// must only ever return something a terminal will render literally.
    #[test]
    fn accessor_is_always_a_plain_name() {
        assert!(is_plain_program_name(program_name()));
    }

    /// The two accessors must agree: an instruction may name a command only
    /// when the name came from `argv[0]`.
    #[test]
    fn instruction_name_is_present_exactly_when_argv0_supplied_it() {
        match program_name_for_instruction() {
            Some(name) => {
                assert_eq!(name, program_name(), "the two must not disagree");
                assert!(resolved().from_argv0);
            }
            None => assert_eq!(
                program_name(),
                FALLBACK_PROGRAM_NAME,
                "with no usable argv[0] the plain accessor must read the fallback"
            ),
        }
    }

    /// The fallback is `grok`, which is also a legitimate program name. Falling
    /// back must not be reported as an instruction-safe name, but a program
    /// genuinely invoked as `grok` must be.
    #[test]
    fn the_fallback_string_is_not_confused_with_being_invoked_as_grok() {
        assert_eq!(from("grok"), Some(FALLBACK_PROGRAM_NAME));
        assert_eq!(from("med ley"), None);
        // The discriminator is provenance, not the string: both produce the
        // same `program_name()`, and only one may appear in an instruction.
    }
}
