//! `MEDLEY_*`-first / `GROK_*`-fallback precedence for the fork's documented
//! user-facing environment variables ([#426]).
//!
//! This is deliberately **not** a startup shim that copies `MEDLEY_X` into
//! `GROK_X` once at process start. Medley spawns user shells and child
//! processes (subagents, MCP servers, hooks); a copied `GROK_X` would leak
//! into an official Grok Build invoked from inside a medley session, which is
//! exactly the collision [`state_dir`] exists to avoid for the state
//! directory. Instead, this module is a read-only precedence check applied at
//! each individual read site, for the specific variables enumerated and
//! justified in #426 — it is not a blanket `GROK_*` → `MEDLEY_*` rename. Most
//! `GROK_*` variables have no `MEDLEY_*` alias and never will; see FORK.md
//! for the enumerated set and why.
//!
//! An exported-but-blank value on either side counts as unset, matching
//! [`state_dir::nonempty`]'s house rule — `05-configuration.md` already
//! documents this as the general contract for environment-variable
//! overrides, so this module keeps that contract uniform rather than
//! special-casing itself.
//!
//! The fallback to `GROK_*` is permanent, not a migration window (per #426's
//! own framing of the parent issue #49). [`legacy_notice`] exists to tell a
//! user their `GROK_*` setting is still honored and name the `MEDLEY_*`
//! alternative — never to warn that support is going away.
//!
//! [#426]: https://github.com/ImL1s/medley/issues/426
//! [`state_dir`]: crate::state_dir

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::sync::Mutex;

/// `GROK_*` names that supplied a value in this process because their
/// `MEDLEY_*` alias was unset or blank. Deduped. Read (not cleared) by
/// [`legacy_notice`], so calling it more than once is safe and keeps
/// returning the full set seen so far.
static LEGACY_HITS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record_legacy_hit(grok_name: &str) {
    let mut hits = LEGACY_HITS.lock().unwrap_or_else(|p| p.into_inner());
    if !hits.iter().any(|h| h == grok_name) {
        hits.push(grok_name.to_string());
    }
}

/// Parse a boolean the same way [`crate::env_bool`] does. Duplicated rather
/// than shared: `env_bool` is an upstream-owned function ([#405]'s
/// upstream-sync-conflict-surface concern applies to editing its body, not
/// just its signature), and five short match arms are cheaper to keep in
/// sync by hand than to risk a body-level conflict on every sync.
///
/// [#405]: https://github.com/ImL1s/medley/issues/405
fn parse_bool_str(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "1" | "true" | "yes" | "on" | "enabled" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

/// An empty or all-whitespace value is treated as unset, mirroring
/// [`state_dir::nonempty`](crate::state_dir).
fn nonempty(value: Option<&OsStr>) -> Option<&OsStr> {
    let value = value?;
    let has_content = value
        .to_str()
        .map_or_else(|| !value.is_empty(), |s| !s.trim().is_empty());
    has_content.then_some(value)
}

/// Pure core: `medley`'s value (already read) wins when non-blank; else
/// `grok`'s value (already read) when non-blank, recording the legacy hit
/// under `grok_name`. Takes both values pre-read so it never touches process
/// state and is directly testable, matching `state_dir::resolve_in`.
fn resolve_os_in(
    medley: Option<&OsStr>,
    grok: Option<&OsStr>,
    grok_name: &str,
) -> Option<OsString> {
    if let Some(v) = nonempty(medley) {
        return Some(v.to_os_string());
    }
    let v = nonempty(grok)?;
    record_legacy_hit(grok_name);
    Some(v.to_os_string())
}

/// Resolve one config value from the live process environment: `MEDLEY_*`
/// first, `GROK_*` after. Reads via `OsStr` so a non-UTF-8 value on either
/// side is not silently dropped — matches call sites that use `var_os` today
/// (e.g. `GROK_LOG_FILE`, read this way specifically so a non-UTF-8 path
/// isn't lost).
pub fn resolve_env_var_os(medley_name: &str, grok_name: &str) -> Option<OsString> {
    resolve_os_in(
        std::env::var_os(medley_name).as_deref(),
        std::env::var_os(grok_name).as_deref(),
        grok_name,
    )
}

/// [`resolve_env_var_os`], decoded to UTF-8. A non-UTF-8 value is dropped —
/// matches the plain `std::env::var` call sites this replaces.
pub fn resolve_env_var(medley_name: &str, grok_name: &str) -> Option<String> {
    resolve_env_var_os(medley_name, grok_name)?
        .into_string()
        .ok()
}

/// [`resolve_env_var`], parsed the same way [`crate::env_bool`] parses a raw
/// value (same accepted spellings as [`parse_bool_str`]).
///
/// A nonblank but unparseable legacy value (`GROK_TELEMETRY_ENABLED=maybe`)
/// is not a hit: nothing was honored, so the notice must not claim it was
/// (#491 review).
pub fn resolve_env_bool(medley_name: &str, grok_name: &str) -> Option<bool> {
    resolve_bool_in(
        nonempty(std::env::var_os(medley_name).as_deref()).and_then(OsStr::to_str),
        nonempty(std::env::var_os(grok_name).as_deref()).and_then(OsStr::to_str),
        grok_name,
    )
}

fn resolve_bool_in(medley: Option<&str>, grok: Option<&str>, grok_name: &str) -> Option<bool> {
    if let Some(v) = medley.filter(|s| !s.trim().is_empty()) {
        return parse_bool_str(v);
    }
    let v = grok.filter(|s| !s.trim().is_empty())?;
    let parsed = parse_bool_str(v)?;
    record_legacy_hit(grok_name);
    Some(parsed)
}

/// [`resolve_env_var`] against an already-collected environment snapshot (a
/// `HashMap`, not live `std::env`) — used by theme/appearance detection,
/// which reads a snapshot so it stays testable without mutating process
/// state.
pub fn resolve_from_map<'a>(
    env: &'a HashMap<String, String>,
    medley_name: &str,
    grok_name: &str,
) -> Option<&'a str> {
    let nonempty_str =
        |v: Option<&'a String>| v.map(String::as_str).filter(|s| !s.trim().is_empty());
    if let Some(v) = nonempty_str(env.get(medley_name)) {
        return Some(v);
    }
    let v = nonempty_str(env.get(grok_name))?;
    record_legacy_hit(grok_name);
    Some(v)
}

/// Record that `grok_name` supplied a value because a `MEDLEY_*` alias was
/// unset, blank, or (for a cascade with more than two candidates, e.g. the
/// theme loop trying `MEDLEY_THEME` → `GROK_THEME` → `LC_GROK_THEME` with
/// per-candidate validity checks) simply not the one that won. For call
/// sites whose cascade can't be expressed as a single [`resolve_env_var`] /
/// [`resolve_from_map`] pair — [`resolve_env_var`] and [`resolve_from_map`]
/// already call this internally and callers using them should not call it
/// again.
pub fn note_legacy_hit(grok_name: &str) {
    record_legacy_hit(grok_name);
}

/// At most one line, naming every legacy `GROK_*` var this process has
/// actually fallen back to so far (`None` if none has). Safe to call more
/// than once — callers are expected to print it at most once, but the
/// function itself is a pure query of the registry, not a one-shot drain.
///
/// The fallback is permanent (#426) — this must never read as a removal
/// warning.
pub fn legacy_notice() -> Option<String> {
    let hits = LEGACY_HITS.lock().unwrap_or_else(|p| p.into_inner());
    if hits.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = hits.iter().map(String::as_str).collect();
    names.sort_unstable();
    Some(format!(
        "medley: honoring legacy env var(s) {} — the MEDLEY_* equivalent is preferred; \
         GROK_* keeps working indefinitely.",
        names.join(", ")
    ))
}

/// Test-only escape hatch: [`LEGACY_HITS`] is process-global, so a test that
/// asserts on [`legacy_notice`] must reset it first or it will see hits left
/// by whichever test ran earlier in the same process. Not exposed outside
/// this crate's own tests (no `test-support` feature) — every call-site test
/// elsewhere in the workspace asserts on the *resolved value*, not on the
/// notice, so no downstream crate needs this.
#[cfg(test)]
fn clear_legacy_hits_for_test() {
    LEGACY_HITS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn os(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn medley_wins_over_grok() {
        assert_eq!(
            resolve_os_in(Some(os("m")), Some(os("g")), "GROK_X"),
            Some(OsString::from("m"))
        );
    }

    // Reaches `LEGACY_HITS` (via `record_legacy_hit`, since `medley` is unset
    // here so `resolve_os_in` falls through to the grok branch that records
    // the hit) — serialized against every other test in this file that does,
    // named rather than unkeyed so it doesn't also serialize against this
    // crate's unrelated env-mutating tests elsewhere.
    #[test]
    #[serial(legacy_hits)]
    fn grok_used_when_medley_unset() {
        assert_eq!(
            resolve_os_in(None, Some(os("g")), "GROK_X"),
            Some(OsString::from("g"))
        );
    }

    // Reaches `LEGACY_HITS` — blank `medley` still falls through to the grok
    // branch that records the hit.
    #[test]
    #[serial(legacy_hits)]
    fn blank_medley_falls_through_to_grok() {
        assert_eq!(
            resolve_os_in(Some(os("   ")), Some(os("g")), "GROK_X"),
            Some(OsString::from("g"))
        );
    }

    #[test]
    fn neither_set_is_none() {
        assert_eq!(resolve_os_in(None, None, "GROK_X"), None);
    }

    #[test]
    fn blank_grok_is_unset_too() {
        assert_eq!(resolve_os_in(None, Some(os("  ")), "GROK_X"), None);
    }

    #[test]
    fn empty_medley_and_empty_grok_is_none() {
        assert_eq!(resolve_os_in(Some(os("")), Some(os("")), "GROK_X"), None);
    }

    #[test]
    fn bool_parsing_matches_env_bool_spellings() {
        for (raw, expected) in [
            ("1", Some(true)),
            ("true", Some(true)),
            ("YES", Some(true)),
            ("on", Some(true)),
            ("enabled", Some(true)),
            ("0", Some(false)),
            ("false", Some(false)),
            ("No", Some(false)),
            ("off", Some(false)),
            ("disabled", Some(false)),
            ("", None),
            ("maybe", None),
        ] {
            assert_eq!(parse_bool_str(raw), expected, "input {raw:?}");
        }
    }

    #[test]
    #[serial(legacy_hits)]
    fn invalid_legacy_bool_is_not_recorded_as_a_hit() {
        clear_legacy_hits_for_test();
        assert_eq!(
            resolve_bool_in(None, Some("maybe"), "GROK_TELEMETRY_ENABLED"),
            None
        );
        assert_eq!(
            legacy_notice(),
            None,
            "an unparseable GROK_* bool must not claim to have been honored"
        );
    }

    #[test]
    #[serial(legacy_hits)]
    fn valid_legacy_bool_is_recorded_as_a_hit() {
        clear_legacy_hits_for_test();
        assert_eq!(
            resolve_bool_in(None, Some("1"), "GROK_TELEMETRY_ENABLED"),
            Some(true)
        );
        assert!(
            legacy_notice()
                .as_deref()
                .is_some_and(|n| n.contains("GROK_TELEMETRY_ENABLED")),
        );
    }

    // Reaches `LEGACY_HITS` directly: clears it, records through it
    // (`resolve_from_map`), and reads it (`legacy_notice`).
    #[test]
    #[serial(legacy_hits)]
    fn resolve_from_map_prefers_medley_and_records_legacy_hit_on_grok() {
        clear_legacy_hits_for_test();
        let mut env = HashMap::new();
        env.insert("GROK_THEME".to_string(), "grokday".to_string());
        assert_eq!(
            resolve_from_map(&env, "MEDLEY_THEME", "GROK_THEME"),
            Some("grokday")
        );
        assert_eq!(
            legacy_notice().as_deref(),
            Some(
                "medley: honoring legacy env var(s) GROK_THEME — the MEDLEY_* equivalent is preferred; \
             GROK_* keeps working indefinitely."
            )
        );

        clear_legacy_hits_for_test();
        env.insert("MEDLEY_THEME".to_string(), "tokyonight".to_string());
        assert_eq!(
            resolve_from_map(&env, "MEDLEY_THEME", "GROK_THEME"),
            Some("tokyonight")
        );
        assert_eq!(
            legacy_notice(),
            None,
            "MEDLEY_THEME winning must not record a legacy hit"
        );
    }

    // Reaches `LEGACY_HITS` directly (clear + read).
    #[test]
    #[serial(legacy_hits)]
    fn legacy_notice_is_none_when_nothing_has_fired() {
        clear_legacy_hits_for_test();
        assert_eq!(legacy_notice(), None);
    }

    // Reaches `LEGACY_HITS` directly (clear + record x3 + read).
    #[test]
    #[serial(legacy_hits)]
    fn legacy_notice_names_multiple_distinct_vars_sorted_and_deduped() {
        clear_legacy_hits_for_test();
        record_legacy_hit("GROK_WORKFLOWS");
        record_legacy_hit("GROK_MEMORY");
        record_legacy_hit("GROK_WORKFLOWS");
        assert_eq!(
            legacy_notice().as_deref(),
            Some(
                "medley: honoring legacy env var(s) GROK_MEMORY, GROK_WORKFLOWS — the MEDLEY_* \
                 equivalent is preferred; GROK_* keeps working indefinitely."
            )
        );
    }

    // Reaches `LEGACY_HITS` directly (clear + record + read).
    #[test]
    #[serial(legacy_hits)]
    fn legacy_notice_never_promises_removal() {
        clear_legacy_hits_for_test();
        record_legacy_hit("GROK_X");
        let notice = legacy_notice().unwrap();
        for banned in [
            "remove",
            "removed",
            "removal",
            "deprecated in a future",
            "will stop",
        ] {
            assert!(
                !notice.to_ascii_lowercase().contains(banned),
                "notice must not threaten removal: {notice}"
            );
        }
    }
}
