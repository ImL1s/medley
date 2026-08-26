"""Pins CLAUDE.md's credential hot-path counts (#487).

CLAUDE.md names five patterns/tests as CI's credential hot path -- "if a
change is going to break something, that is where it shows." One of the
five was a literal substring, `never_emit_credential_bytes`, that read as
a family name but selected only one of its own three tests. Both of the
missed tests run in CI today via unrelated filters, so nothing was
actually uncovered -- but a report of "1 passed" for this filter reads as
"the suite is green" with no way to tell it is one third of the family.
`run_nonzero`'s own zero-match guard cannot catch this: one match is not
zero.

This guard checks two things CLAUDE.md's prose alone cannot self-verify:

1. That the counts recorded in CLAUDE.md still match what each
   pattern/name actually selects. Enumerated independently of `cargo
   test`'s own filter mechanism: a source scan for
   `#[test]`/`#[tokio::test]`-attributed function names under every real
   crate's `src/` AND `tests/` directories, qualified with their in-file
   module prefix. Both scope dimensions matter, and this guard's own first
   version got each wrong once, caught by review both times:

   - `--lib`-only scope missed 3 of the 5 documented counts (#487's own
     investigation) -- `xai-grok-sampler/tests/shared_http_wire.rs` is a
     separate integration-test target, invisible to a `--lib`-only
     listing.
   - fn-name-only classification (no module prefix) reproduces the exact
     defect this guard exists to catch (#507 review): libtest matches a
     substring filter against the full `module::path::fn` name, so a
     generic test added under a module whose name matches a pattern
     (`mod none_auth_scheme_regressions { fn works() {} }`) is selected by
     the real filter but invisible to a fn-name-only scan.

2. That CLAUDE.md's counts are the thing actually checked, not a value
   duplicated in this file (#507 review): a guard that pins its own copy
   of "3", not the "3" written in CLAUDE.md, goes green forever no matter
   what CLAUDE.md is edited to say. This module parses the counts straight
   out of the documented paragraph and treats a parse failure as a loud
   error, never as a silent fallback to some other expected value.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CLAUDE_MD = ROOT / "CLAUDE.md"
_CRATE_ROOTS = ("crates", "prod")

_TEST_ATTR = re.compile(r"^\s*#\[(?:tokio::)?test\b")
_FN = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)
_MOD_OPEN = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{"
)

_HOT_PATH_MARKER = "CI's hot path is exactly this suite"
_DOC_ENTRY = re.compile(r"`([a-z][a-z0-9_]*)`\s*\((\d+)\)")


def _strip_line_comment(line: str) -> str:
    # Good enough for brace-depth counting: does not account for `//`
    # inside a string or char literal, which this repo's test modules do
    # not do at a `mod`/brace boundary. A noted limitation, not a silent
    # one.
    idx = line.find("//")
    return line if idx == -1 else line[:idx]


def parse_documented_hot_path(text: str) -> dict[str, int]:
    """CLAUDE.md's own `` `pattern` (N) `` entries in its hot-path
    paragraph -- the guard's source of truth, not a value copied into
    this file (#507 review). Raises `AssertionError` on any parse
    failure: a guard that cannot read its own source of truth must fail
    loudly, never fall back to an expectation nothing here checked.
    """
    start = text.find(_HOT_PATH_MARKER)
    if start == -1:
        raise AssertionError(
            f"could not find {_HOT_PATH_MARKER!r} in CLAUDE.md -- has the "
            "wording moved?"
        )
    end = text.find("\n\n", start)
    paragraph = text[start : end if end != -1 else len(text)]
    entries = dict(
        (name, int(count)) for name, count in _DOC_ENTRY.findall(paragraph)
    )
    if not entries:
        raise AssertionError(
            "parsed zero `pattern` (N) entries from CLAUDE.md's hot-path "
            "paragraph -- has the formatting moved?"
        )
    return entries


def _qualified_test_names(root: Path) -> list[str]:
    """Every `#[test]`/`#[tokio::test]` function's qualified name under
    `crates/`/`prod/` -- `src/` and `tests/` alike -- prefixed with its
    in-file module path via brace-depth tracking of `mod X { ... }`
    blocks.

    Exact for the common case this repo overwhelmingly uses --
    `#[cfg(test)] mod tests { ... }` and similar inline nesting -- since
    the actual source text is walked, not approximated from a file path.
    Does NOT resolve a cross-file `mod x;` / `#[path = ...] mod x;`
    declaration into the *including* file's own module prefix (unlike
    `check_new_tests_are_filtered.py`'s `selected()`, which approximates
    that case from the file's path but does not track inline `mod`
    blocks at all -- neither approximation is a superset of the other).
    A test in a file only reachable through such a declaration is
    enumerated with a shorter qualified name than `cargo test` would
    report; this can only make the guard MISS a module-prefix match, the
    same direction as the defect it exists to catch, so it is a known
    conservative gap rather than a silent wrong answer in the other
    direction.
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
            mod_stack: list[tuple[int, str]] = []
            depth = 0
            n = len(lines)
            for i in range(n):
                raw = lines[i]

                if _TEST_ATTR.match(raw):
                    for follow_raw in lines[i + 1 :]:
                        follow = follow_raw.strip()
                        if follow.startswith("#[") or follow.startswith("//"):
                            continue
                        if not follow:
                            continue
                        m = _FN.match(follow_raw)
                        if m:
                            prefix = "::".join(name for _, name in mod_stack)
                            names.append(
                                f"{prefix}::{m.group(1)}" if prefix else m.group(1)
                            )
                        break

                line = _strip_line_comment(raw)
                mod_match = _MOD_OPEN.match(line)
                if mod_match:
                    mod_stack.append((depth, mod_match.group(1)))
                depth += line.count("{") - line.count("}")
                while mod_stack and depth <= mod_stack[-1][0]:
                    mod_stack.pop()
    return names


class CredentialHotPathCorpus(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.documented = parse_documented_hot_path(
            CLAUDE_MD.read_text(encoding="utf-8")
        )
        cls.names = _qualified_test_names(ROOT)

    def test_the_corpus_is_not_empty(self):
        # A scan that silently finds nothing satisfies every assertion
        # below while checking nothing at all.
        self.assertGreater(len(self.names), 1000, len(self.names))

    def test_claude_md_still_documents_at_least_the_five_known_entries(self):
        # Guards against the parse silently degrading to zero real
        # entries while still finding an unrelated `` `x` (N) `` span
        # elsewhere in the file.
        known = {
            "is_secret_free_",
            "omits_xai_identity",
            "hostile_injector",
            "none_auth_scheme_",
        }
        self.assertLessEqual(known, set(self.documented))

    def test_each_documented_entry_selects_its_documented_count(self):
        # The counterexample CLAUDE.md's own commit history should never
        # reproduce (#507 review): editing only this file's number, with
        # nothing about the source changing, must turn this test red --
        # the guard checks CLAUDE.md's count against source, not a copy
        # of the count against itself.
        wrong = {}
        for pattern, expected in self.documented.items():
            matched = [n for n in self.names if pattern in n]
            if len(matched) != expected:
                wrong[pattern] = (len(matched), expected, matched)
        self.assertEqual(
            wrong,
            {},
            f"CLAUDE.md's documented count does not match source (got, "
            f"documented, matches): {wrong}",
        )


if __name__ == "__main__":
    unittest.main()
