"""Pins CLAUDE.md's credential hot-path counts (#487).

CLAUDE.md names five patterns/tests as CI's credential hot path -- "if a
change is going to break something, that is where it shows." One of the
five was a literal substring, `never_emit_credential_bytes`, that read as
a family name but selected only one of its own three tests:
`sampler_request_logs_never_emit_credential_bytes` matched;
`transport_failure_never_emit**s**_query_credential_bytes` (one letter,
plus an inserted `query_`) and
`subagent_resolution_diagnostics_never_emit_parent_or_child_credentials`
(a different noun) did not. Both of the missed tests run in CI today via
unrelated filters (a dedicated named lane and a module-path sweep), so
nothing was actually uncovered -- but a report of "1 passed" for this
filter reads as "the suite is green" with no way to tell it is one third
of the family. `run_nonzero`'s own zero-match guard cannot catch this: one
match is not zero.

This guard makes the gap visible the other way: it pins what each
name/pattern in CLAUDE.md actually selects, so a rename or a deletion that
shrinks a family reddens CI here, and a new test joining a family without
updating the recorded count also reddens CI here, rather than either
silently changing the meaning of "1 passed".

Enumerated independently of `cargo test`'s own filter mechanism: a source
scan for `#[test]`/`#[tokio::test]`-attributed function names under every
real crate's `src/` AND `tests/` directories. Both matter -- `--lib`-only
scope is exactly the trap this issue's own investigation fell into first:
`xai-grok-sampler/tests/shared_http_wire.rs` is a separate integration-test
target invisible to a `--lib`-only listing, and missing it undercounted
three of these five patterns by measuring against an incomplete universe,
not because the counts themselves were wrong.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
_CRATE_ROOTS = ("crates", "prod")
_TEST_ATTR = re.compile(r"^\s*#\[(?:tokio::)?test\b")
_FN = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)

# The three tests a "correct the pattern" fix would need one substring to
# bridge -- measured (#487) to have no common substring narrower than bare
# `never_emit`, which also selects 6 unrelated tests across the workspace
# (a UI classifier, a chess-syntax test, an argv/gate-flag test among
# them). Named explicitly instead, per CLAUDE.md's own updated entry.
NAMED_HOT_PATH_TESTS = frozenset(
    {
        "sampler_request_logs_never_emit_credential_bytes",
        "transport_failure_never_emits_query_credential_bytes",
        "subagent_resolution_diagnostics_never_emit_parent_or_child_credentials",
    }
)

# Pattern -> the count it must select, exactly. Both directions matter: a
# lower count means a rename or deletion silently shrank the family a
# reader trusts CLAUDE.md's number for; a higher count means a new test
# joined the family and CLAUDE.md's recorded number is now stale.
PATTERN_COUNTS = {
    "is_secret_free_": 3,
    "omits_xai_identity": 4,
    "hostile_injector": 3,
    "none_auth_scheme_": 5,
}

# #487: grepping a bare `never_emit` for "did anything else join this
# family" finds 9 matches today -- the 3 named above, plus these 6, which
# are genuinely unrelated tests that happen to share the verb (a UI
# classifier, chess-syntax, tool-def/argv gate flags, and two other
# `secret`/`credential`-adjacent tests from a different feature entirely).
# This is the measured, permanent shape of that loose net, not a ratchet
# to close -- recorded so a real seventh member is still visible as a
# failure here instead of only in a future full-text review of every
# `never_emit` test in the tree.
KNOWN_UNRELATED_NEVER_EMIT_TESTS = frozenset(
    {
        "callback_payload_debug_never_emits_code_state_or_issuer",
        "issue15_doctor_providers_never_emit_secret_bytes",
        "subagent_classifier_never_emits_needs_input",
        "polarity_safe_never_emits_white_or_black",
        "tool_defs_reemit_gate_flag_off_never_emits_and_records_nothing",
        "to_argv_never_emits_the_enabled_gate",
    }
)


def _test_function_names(root: Path) -> list[str]:
    """Every `#[test]`/`#[tokio::test]`-attributed function name under
    `crates/`/`prod/` -- `src/` and `tests/` alike (see module docstring).

    Independent of `_TEST_ATTR`'s own use elsewhere in this repo's guards:
    this walks forward past intervening attributes/doc-comments to the
    first `fn`, matching how a stacked `#[test]\n#[some_macro]\nfn f()`
    still names one test.
    """
    names: list[str] = []
    for base in _CRATE_ROOTS:
        base_dir = root / base
        if not base_dir.is_dir():
            continue
        for rs in base_dir.rglob("*.rs"):
            try:
                text = rs.read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError):
                continue
            lines = text.splitlines()
            for i, line in enumerate(lines):
                if not _TEST_ATTR.match(line):
                    continue
                for follow in lines[i + 1 :]:
                    stripped = follow.strip()
                    if stripped.startswith("#[") or stripped.startswith("//"):
                        continue
                    if not stripped:
                        continue
                    m = _FN.match(follow)
                    if m:
                        names.append(m.group(1))
                    break
    return names


class CredentialHotPathCorpus(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.names = _test_function_names(ROOT)

    def test_the_corpus_is_not_empty(self):
        # A scan that silently finds nothing satisfies every assertion
        # below while checking nothing at all.
        self.assertGreater(len(self.names), 1000, len(self.names))

    def test_each_pattern_selects_its_recorded_count(self):
        wrong = {}
        for pattern, expected in PATTERN_COUNTS.items():
            matched = [n for n in self.names if pattern in n]
            if len(matched) != expected:
                wrong[pattern] = (len(matched), expected, matched)
        self.assertEqual(
            wrong,
            {},
            f"pattern selects a different count than CLAUDE.md records "
            f"(got, expected, matches): {wrong}",
        )

    def test_the_named_hot_path_tests_all_still_exist(self):
        present = set(self.names)
        missing = NAMED_HOT_PATH_TESTS - present
        self.assertEqual(
            missing, set(), f"named hot-path tests no longer exist: {missing}"
        )

    def test_no_new_test_silently_joined_or_left_the_never_emit_family(self):
        # Both directions: a genuinely new never_emit test (not in either
        # known set) must surface here rather than silently going
        # unnoticed; a known-unrelated test that was renamed or deleted
        # must also surface, so this list does not quietly drift stale.
        loose = {n for n in self.names if "never_emit" in n}
        known = NAMED_HOT_PATH_TESTS | KNOWN_UNRELATED_NEVER_EMIT_TESTS
        unexpected_new = loose - known
        missing_known = known - loose
        self.assertEqual(unexpected_new, set(), f"unrecognised: {unexpected_new}")
        self.assertEqual(missing_known, set(), f"no longer found: {missing_known}")


if __name__ == "__main__":
    unittest.main()
