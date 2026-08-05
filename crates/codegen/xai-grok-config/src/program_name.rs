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

/// The file name of `argv[0]`, screened by [`is_plain_program_name`].
fn program_name_from_argv0(argv0: Option<&str>) -> String {
    argv0
        .map(std::path::Path::new)
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .filter(|n| is_plain_program_name(n))
        .unwrap_or(FALLBACK_PROGRAM_NAME)
        .to_owned()
}

static PROGRAM_NAME: OnceLock<String> = OnceLock::new();

/// The name this program was invoked as.
///
/// Self-initializing from `argv[0]`, deliberately: `argv[0]` is immutable for
/// the life of the process, so a pull is always correct and needs no ordering
/// guarantee. An earlier version had `parse_cli` *push* the value in, which
/// made every caller's correctness depend on a call order nothing enforced —
/// and left library consumers, and any test that formats a message without
/// parsing arguments, reading the fallback.
pub fn program_name() -> &'static str {
    PROGRAM_NAME.get_or_init(|| program_name_from_argv0(std::env::args().next().as_deref()))
}

/// The name to use inside an instruction the user is expected to *type*.
///
/// Returns `None` when `argv[0]` gave us nothing usable. Callers must then
/// phrase the message without naming a command, because the fallback is the
/// name of a *different program that may be installed* — telling the user to
/// run it is the bug this module exists to prevent, and it is worse than
/// saying nothing.
///
/// Reachable in practice, not just in theory: a binary copied to `medley (1)`
/// by a browser's "keep both", or renamed to something non-ASCII, fails the
/// screening.
pub fn program_name_for_instruction() -> Option<&'static str> {
    let name = program_name();
    (name != FALLBACK_PROGRAM_NAME || argv0_really_was_the_fallback()).then_some(name)
}

/// Distinguishes "we fell back" from "the program really is called `grok`",
/// which matters because only the first is unsafe to put in an instruction.
fn argv0_really_was_the_fallback() -> bool {
    std::env::args()
        .next()
        .as_deref()
        .map(std::path::Path::new)
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == FALLBACK_PROGRAM_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_taken_whatever_they_are() {
        for (argv0, want) in [
            ("medley", "medley"),
            ("/usr/local/bin/medley", "medley"),
            ("./medley", "medley"),
            ("grok", "grok"),
            ("agent", "agent"),
            ("xai-grok-pager", "xai-grok-pager"),
        ] {
            assert_eq!(program_name_from_argv0(Some(argv0)), want);
        }
    }

    /// `argv[0]` is caller-chosen and is printed into a terminal, so anything
    /// that could rewrite the display must fall back rather than be echoed.
    #[test]
    fn hostile_or_missing_argv0_falls_back() {
        for argv0 in [
            "",
            "med\u{1b}[2Jley",  // escape sequence: clears the screen
            "med\nUsage: sudo", // newline: forges a second output line
            "med\u{7}ley",      // control character
            "med ley",          // space
            "медley",           // non-ASCII
            "medley (1)",       // a browser's "keep both" copy
        ] {
            assert_eq!(
                program_name_from_argv0(Some(argv0)),
                FALLBACK_PROGRAM_NAME,
                "argv[0] {argv0:?} must not reach rendered output"
            );
        }
        assert_eq!(program_name_from_argv0(None), FALLBACK_PROGRAM_NAME);
        // A trailing separator has no file name component of its own.
        assert_eq!(program_name_from_argv0(Some("/usr/bin/")), "bin");
        // Refused, not truncated: truncation still prints caller-chosen text.
        assert_eq!(
            program_name_from_argv0(Some(&"m".repeat(65))),
            FALLBACK_PROGRAM_NAME
        );
        assert_eq!(
            program_name_from_argv0(Some(&"m".repeat(64))),
            "m".repeat(64)
        );
    }

    /// The accessor must be safe to call before anything initialises it, and
    /// must only ever return something a terminal will render literally.
    #[test]
    fn accessor_is_always_a_plain_name() {
        assert!(is_plain_program_name(program_name()));
    }
}
