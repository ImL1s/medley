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
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CLAUDE_MD = ROOT / "CLAUDE.md"
_CRATE_ROOTS = ("crates", "prod", "third_party")

_TEST_ATTR = re.compile(r"^\s*#\[(?:tokio::)?test\b")
_FN = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)
_MOD_OPEN = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{"
)
_MOD_SEMI = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;"
)
_PATH_ATTR = re.compile(r'#\[path\s*=\s*"([^"]+)"\]')

_HOT_PATH_MARKER = "CI's hot path is exactly this suite"
_DOC_ENTRY = re.compile(r"`([a-z][a-z0-9_]*)`\s*\((\d+)\)")

# The seven names the hot-path paragraph must keep enumerating. Counts
# still come from CLAUDE.md; this set is the ratchet that a deleted
# named entry cannot silently drop out of the loop (#507 review).
_REQUIRED_HOT_PATH_ENTRIES = frozenset(
    {
        "is_secret_free_",
        "omits_xai_identity",
        "hostile_injector",
        "none_auth_scheme_",
        "sampler_request_logs_never_emit_credential_bytes",
        "transport_failure_never_emits_query_credential_bytes",
        "subagent_resolution_diagnostics_never_emit_parent_or_child_credentials",
    }
)


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


def _crate_source_rel(rs: Path) -> tuple[str, list[str]] | None:
    """(`src`|`tests`, remaining components including the filename).

    Anchored at the crate source root, not the last `src`/`tests` in the
    path. `src/agent/subagent/tests/rest.rs` is `agent::subagent::tests::rest`,
    not `rest` -- libtest uses the crate-root-relative module path
    (#507 review). Mirrors `check_new_tests_are_filtered.py`'s `_crate_split`.
    """
    parts = list(rs.parts)
    if "crates" in parts:
        i = parts.index("crates")
        # crates/<group>/<crate>/{src,tests}/... in the real tree;
        # crates/<crate>/{src,tests}/... in this file's unit fixtures.
        for crate_end in (i + 3, i + 2):
            if len(parts) > crate_end and parts[crate_end] in ("src", "tests"):
                return parts[crate_end], list(parts[crate_end + 1 :])
        return None
    if "prod" in parts or "third_party" in parts:
        marker = "prod" if "prod" in parts else "third_party"
        i = parts.index(marker)
        for j in range(i + 1, len(parts)):
            if parts[j] in ("src", "tests"):
                return parts[j], list(parts[j + 1 :])
        return None
    return None


def _path_module_prefix(rs: Path) -> list[str]:
    """Module path implied by a file under `src/` or `tests/`.

    `mod name;` in a parent file loads `name.rs` (or `name/mod.rs`);
    libtest then reports `name::fn`, not a bare `fn`. Walking only
    inline `mod X {` blocks misses that prefix (#507 review).

    Integration targets `tests/foo.rs` are a cargo test binary named
    after the stem: libtest reports `fn`, not `foo::fn`. Prefixing the
    stem would inflate CLAUDE.md counts (#507 review).
    """
    split = _crate_source_rel(rs)
    if split is None:
        return []
    marker, rest = split
    if not rest:
        return []
    if marker == "tests" and len(rest) == 1:
        return []
    rest = list(rest)
    rest[-1] = Path(rest[-1]).stem
    if rest[-1] in ("lib", "main", "mod"):
        rest.pop()
    if marker == "tests" and rest:
        # `tests/foo/bar.rs`: `foo` is the integration crate, not a module.
        rest = rest[1:]
    return list(rest)


def _declared_module_overrides(root: Path) -> dict[Path, list[str]]:
    """`#[path = "..."] mod name;` maps the target to the declaring
    file's module prefix plus `name`.

    The declared module is a child of the declaring file, not a sibling
    of the target's directory. `session/acp_session.rs` with
    `#[path = "acp_session_tests/auth_error_no_retry_tests.rs"] mod
    auth_error_no_retry_tests;` is
    `session::acp_session::auth_error_no_retry_tests`, not
    `session::acp_session_tests::...` (#507 review).
    """
    overrides: dict[Path, list[str]] = {}
    for base in _CRATE_ROOTS:
        base_dir = root / base
        if not base_dir.is_dir():
            continue
        for rs in base_dir.rglob("*.rs"):
            try:
                text = rs.read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError):
                continue
            pending_path: str | None = None
            for raw in text.splitlines():
                line = _strip_line_comment(raw).strip()
                path_match = _PATH_ATTR.search(line)
                if path_match:
                    pending_path = path_match.group(1)
                    continue
                semi = _MOD_SEMI.match(line)
                if semi and pending_path:
                    child = (rs.parent / pending_path).resolve()
                    overrides[child] = _path_module_prefix(rs) + [semi.group(1)]
                    pending_path = None
                    continue
                if line:
                    pending_path = None
    return overrides


def _qualified_test_names(root: Path) -> list[str]:
    """Every `#[test]`/`#[tokio::test]` function's qualified name under
    `crates/`/`prod/` -- `src/` and `tests/` alike -- prefixed with its
    file-path module plus in-file `mod X { ... }` blocks.

    File-path prefix covers the common `mod name;` / `name.rs` shape
    libtest uses (#507 review). `#[path = "..."] mod name;` replaces the
    target file's path prefix with the declaring file's prefix plus
    `name`. Cross-file `mod x;`
    whose file is not `x.rs` and has no `#[path]` is still a shorter
    name than cargo would report -- conservative, same direction as
    the defect this guard exists to catch.
    """
    overrides = _declared_module_overrides(root)
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
            file_mods = _path_module_prefix(rs)
            key = rs.resolve()
            if key in overrides:
                file_mods = list(overrides[key])
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
                            prefix_parts = file_mods + [name for _, name in mod_stack]
                            prefix = "::".join(prefix_parts)
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

    def test_claude_md_still_documents_all_required_hot_path_entries(self):
        # A deleted named entry must not drop out of the count loop
        # because this assertion only listed the four patterns (#507 review).
        self.assertEqual(set(self.documented), _REQUIRED_HOT_PATH_ENTRIES)

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


class ExternalModulePrefix(unittest.TestCase):
    def test_mod_decl_file_uses_the_file_stem_as_libtest_prefix(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text("mod none_auth_scheme_regressions;\n")
            (src / "none_auth_scheme_regressions.rs").write_text(
                "#[test]\nfn works() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_regressions::works", names)

    def test_path_attr_overrides_the_file_stem(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                '#[path = "elsewhere.rs"]\nmod none_auth_scheme_regressions;\n'
            )
            (src / "elsewhere.rs").write_text("#[test]\nfn works() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_regressions::works", names)

    def test_nested_src_tests_dir_keeps_the_crate_root_prefix(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            nested = root / "crates" / "codegen" / "demo" / "src" / "agent" / "subagent" / "tests"
            nested.mkdir(parents=True)
            (nested.parent.parent.parent / "lib.rs").write_text("")
            (nested / "rest.rs").write_text("#[test]\nfn works() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("agent::subagent::tests::rest::works", names)
            self.assertNotIn("rest::works", names)

    def test_path_attr_keeps_the_declaring_file_prefix(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            session = root / "crates" / "codegen" / "demo" / "src" / "session"
            session.mkdir(parents=True)
            (session.parent / "lib.rs").write_text("mod session;\n")
            (session / "mod.rs").write_text("mod acp_session;\n")
            (session / "acp_session.rs").write_text(
                '#[path = "acp_session_tests/auth_error_no_retry_tests.rs"]\n'
                "mod auth_error_no_retry_tests;\n"
            )
            tests_dir = session / "acp_session_tests"
            tests_dir.mkdir()
            (tests_dir / "auth_error_no_retry_tests.rs").write_text(
                "#[test]\nfn works() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn(
                "session::acp_session::auth_error_no_retry_tests::works", names
            )
            self.assertNotIn(
                "session::acp_session_tests::auth_error_no_retry_tests::works",
                names,
            )

    def test_integration_target_has_no_file_stem_prefix(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            tests = root / "crates" / "codegen" / "demo" / "tests"
            tests.mkdir(parents=True)
            (tests / "shared_http_wire.rs").write_text(
                "#[test]\nfn none_auth_scheme_sends() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_sends", names)
            self.assertNotIn(
                "shared_http_wire::none_auth_scheme_sends", names
            )

    def test_third_party_crate_is_scanned(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "third_party" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text("mod none_auth_scheme_regressions;\n")
            (src / "none_auth_scheme_regressions.rs").write_text(
                "#[test]\nfn works() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_regressions::works", names)


if __name__ == "__main__":
    unittest.main()
