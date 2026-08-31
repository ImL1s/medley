"""Tests for the #280 crate-level CI ratchet in `scripts/`.

The checker exists because a crate with tests that no `ci.yml` `-p` names
runs nowhere, and the #171 diff-scoped guard cannot see that. The cases
below are the ones a wrong answer would hide: a new crate treated as
covered, a stale allowlist entry left behind, or `tests/` mistaken for
`src/` tests.
"""

from __future__ import annotations

import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "scripts"))

from check_uncovered_crates import (  # noqa: E402
    AllowlistError,
    _CRATE_ROOTS,
    evaluate,
    iter_crates,
    load_allowlist,
    main,
    named_tokens,
    package_name,
    read_reasons,
    src_has_tests,
    write_allowlist,
)
from check_unlinted_crates import package_name as _unlinted_package_name  # noqa: E402
from check_test_filter_coverage import parse_workflow as _parse_workflow_oracle  # noqa: E402
from toml_package_name import package_name as _toml_package_name  # noqa: E402


def _write(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(body), encoding="utf-8")


def _crate(root: Path, rel: str, name: str, src: str, extra: dict[str, str] | None = None) -> Path:
    crate_dir = root / rel
    _write(crate_dir / "Cargo.toml", f'''\
        [package]
        name = "{name}"
        version = "0.1.0"
        ''')
    _write(crate_dir / "src" / "lib.rs", src)
    if extra:
        for rel_file, contents in extra.items():
            _write(crate_dir / rel_file, contents)
    return crate_dir


class PackageName(unittest.TestCase):
    def test_reads_package_name_not_later_bin_name(self):
        text = textwrap.dedent(
            """\
            [package]
            license = "Apache-2.0"
            name = "prod-mc-cli-chat-proxy-types"
            version = "0.1.0"

            [[bin]]
            name = "something-else"
            """
        )
        self.assertEqual(package_name(text), "prod-mc-cli-chat-proxy-types")

    def test_virtual_manifest_has_no_package(self):
        self.assertIsNone(package_name("[workspace]\nmembers = []\n"))


class NamedTokens(unittest.TestCase):
    def test_extracts_p_from_run_nonzero_and_manifest_path_from_cargo_test(self):
        wf = textwrap.dedent(
            """\
            run_nonzero -p covered --lib foo -- --nocapture
            cargo test --manifest-path crates/codegen/also-covered/Cargo.toml --lib
            """
        )
        self.assertEqual(named_tokens(wf), {"covered", "also-covered"})

    def test_package_long_flag(self):
        self.assertEqual(named_tokens("cargo test --package boxed --lib\n"), {"boxed"})

    def test_clippy_or_build_mention_does_not_count_as_named(self):
        # #437: a crate's tests can be deleted in full while a `cargo clippy`
        # or `cargo build` step still names its manifest path. Crediting that
        # mention made this checker call the crate "covered" with nothing
        # left to run its tests -- the hole neither this guard nor
        # `check_test_filter_coverage.py`'s #280 hand-off caught. Only a line
        # that actually invokes the test binary may name a crate.
        wf = textwrap.dedent(
            """\
            run_nonzero -p covered --lib foo -- --nocapture
            cargo clippy --manifest-path crates/codegen/clippy-only/Cargo.toml --all-targets -- -D warnings
            cargo build --manifest-path crates/codegen/build-only/Cargo.toml -p build-only
            """
        )
        self.assertEqual(named_tokens(wf), {"covered"})

    def test_joins_line_continuations_before_matching(self):
        # `ci.yml` wraps long `run_nonzero` invocations with a trailing
        # backslash; the `-p` flag is on the first line but the test-name
        # continuation line is where the statement actually ends.
        wf = "run_nonzero -p continued --lib \\\n  some_test_name -- --nocapture\n"
        self.assertEqual(named_tokens(wf), {"continued"})


def _naive_package_name(toml_text: str) -> str | None:
    """`[package]` name via plain string ops -- deliberately not `toml_package_name`.

    Only the value-line extraction is reimplemented; the `[section]` walk
    above it is shared structure, not the pattern under test (#455). This is
    what makes the corpus below non-circular: `toml_package_name.package_name`
    cannot pass by construction, because nothing here is built from it.
    """
    in_package = False
    for line in toml_text.splitlines():
        stripped = line.strip()
        if stripped == "[package]":
            in_package = True
            continue
        if stripped.startswith("[") and stripped.endswith("]"):
            if in_package:
                return None
            continue
        if not in_package:
            continue
        key, sep, rest = stripped.partition("=")
        if key.strip() != "name" or not sep:
            continue
        rest = rest.strip()
        if not rest or rest[0] not in ('"', "'"):
            continue
        quote = rest[0]
        end = rest.find(quote, 1)
        if end == -1:
            continue
        value, remainder = rest[1:end], rest[end + 1 :].strip()
        if remainder and not remainder.startswith("#"):
            continue  # trailing junk that is not a comment: not valid TOML
        return value
    return None


def _real_manifests() -> dict[Path, str]:
    """Every real `Cargo.toml` under `_CRATE_ROOTS`, naively-extracted name.

    Enumerated from the SAME roots `iter_crates()` walks, but the value on
    each is read by `_naive_package_name`, not `toml_package_name` -- so this
    corpus is not the thing it is testing.
    """
    found: dict[Path, str] = {}
    for base in _CRATE_ROOTS:
        base_dir = REPO / base
        if not base_dir.is_dir():
            continue
        for manifest in sorted(base_dir.rglob("Cargo.toml")):
            text = manifest.read_text(encoding="utf-8", errors="ignore")
            name = _naive_package_name(text)
            if name is not None:
                found[manifest] = name
    return found


class PackageNameCorpus(unittest.TestCase):
    """`toml_package_name.package_name` against every real manifest.

    #506 moved the tolerant reader into `scripts/toml_package_name.py` so
    the sibling guards cannot drift. These tests pin that (1) the shared
    function still matches a naive walk of the tree, (2) both scripts still
    *re-export that function* rather than growing a private copy, and (3)
    the trailing-comment form #464 fixed is still recognised. A private copy
    would make (2) fail even if (1) happened to agree on today's manifests.
    """

    @classmethod
    def setUpClass(cls):
        cls.manifests = _real_manifests()

    def test_the_corpus_is_not_empty(self):
        # ~78 real manifests under crates/ + prod/ at the time this was
        # written; a scan that silently finds nothing would pass every
        # assertion below while checking nothing at all.
        self.assertGreater(len(self.manifests), 50, len(self.manifests))

    def test_every_real_manifest_name_is_read_correctly(self):
        missed = {
            str(path): (expected, _toml_package_name(path.read_text(encoding="utf-8")))
            for path, expected in self.manifests.items()
            if _toml_package_name(path.read_text(encoding="utf-8")) != expected
        }
        self.assertEqual(missed, {}, missed)

    def test_a_trailing_comment_form_is_recognised(self):
        # Constructed rather than pulled from the tree: no real manifest is
        # spelled this way today (measured), which is the corpus's own limit.
        text = '[package]\nname = "foo" # renamed pending #999\nversion = "0.1.0"\n'
        self.assertEqual(_toml_package_name(text), "foo")

    def test_a_trailing_comment_with_no_space_is_recognised(self):
        text = '[package]\nname = "foo"# no space\n'
        self.assertEqual(_toml_package_name(text), "foo")

    def test_the_pattern_still_discriminates(self):
        # Trailing text that is not a `#` comment is not valid TOML and must
        # not be accepted just because the line starts with a quoted name.
        self.assertIsNone(_toml_package_name('[package]\nname = "foo" extra\n'))
        self.assertIsNone(_toml_package_name("[workspace]\nmembers = []\n"))

    def test_both_scripts_reexport_the_shared_reader(self):
        # The #506 envelope: if a sibling grows a private copy again, this
        # fails even when the copy still agrees on today's manifests.
        self.assertIs(package_name, _toml_package_name)
        self.assertIs(_unlinted_package_name, _toml_package_name)

    def test_agrees_with_the_sibling_script_on_every_real_manifest(self):
        disagreements = {
            str(path): (
                _toml_package_name(text),
                package_name(text),
                _unlinted_package_name(text),
            )
            for path, _ in self.manifests.items()
            for text in [path.read_text(encoding="utf-8")]
            if not (
                _toml_package_name(text)
                == package_name(text)
                == _unlinted_package_name(text)
            )
        }
        self.assertEqual(disagreements, {}, disagreements)

    def test_agrees_with_the_sibling_script_on_the_trailing_comment_form(self):
        text = '[package]\nname = "foo" # renamed pending #999\n'
        self.assertEqual(_toml_package_name(text), _unlinted_package_name(text))
        self.assertEqual(_toml_package_name(text), package_name(text))


class NamedTokensCorpus(unittest.TestCase):
    """`named_tokens()` against real `ci.yml`, enumerated by a different parser.

    `check_test_filter_coverage.parse_workflow()` is `shlex`-based structure
    parsing (#455's own scope excludes it as a corpus target in its own
    right), but it already extracts `-p` / `--package` / `--manifest-path`
    crate identity from the same `run_nonzero` / `cargo test` lines this
    regex does, independently and via a different mechanism. That makes it a
    legitimate oracle for THIS regex's identifiers, even though it is not
    itself corpus-tested here.
    """

    @classmethod
    def setUpClass(cls):
        workflow_text = (REPO / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        cls.tokens = named_tokens(workflow_text)
        cls.oracle_crates: set[str] = set()
        for crate, targets in _parse_workflow_oracle(workflow_text).items():
            if any(targets.values()):
                cls.oracle_crates.add(crate)

    def test_the_corpus_is_not_empty(self):
        self.assertGreater(len(self.oracle_crates), 20, len(self.oracle_crates))

    def test_named_tokens_and_the_oracle_agree_on_every_crate(self):
        # Was one-way (`oracle_crates - tokens`), on the theory that
        # `named_tokens()` reads `--manifest-path` as a strict superset of
        # the oracle's package names. Measured instead of assumed: both
        # sides resolve `--manifest-path` to `Path(...).parent.name` --
        # `check_test_filter_coverage._crate_from_manifest` does the exact
        # same basename read `named_tokens()` does -- so there is no
        # namespace this regex sees that the oracle does not, and nothing
        # justified the missing direction.
        #
        # That missing direction is not academic: `_TEST_INVOCATION` is a
        # bare `re.search`, so a *commented-out* `# cargo test -p phantom
        # --lib` still reads as a real invocation and lands in `tokens`.
        # The old one-way check only ever asked "did `named_tokens()` miss
        # something the oracle has" and had nothing to say about
        # `named_tokens()` inventing a crate the oracle does not -- exactly
        # the direction that lets `check_uncovered_crates.py`'s production
        # `evaluate()` credit a crate whose tests run nowhere (Codex, #508
        # review). `test_a_commented_out_invocation_is_still_a_named_token`
        # below proves this direction has teeth, using that exact example.
        self.assertEqual(self.tokens, self.oracle_crates)

    def test_a_commented_out_invocation_is_still_a_named_token(self):
        # The counter-example from the Codex review verbatim, not a
        # constructed stand-in: proves the divergence this class's main
        # assertion now has to catch is real, not hypothetical. A one-way
        # `oracle_crates - tokens` check would pass on this input --
        # `phantom` is missing from `oracle_crates`, so it is never on the
        # side that check subtracts from -- while the two-way equality
        # above would fail on it immediately.
        wf = "# cargo test -p phantom --lib\ncargo test -p real --lib\n"
        tokens = named_tokens(wf)
        oracle = {
            crate
            for crate, targets in _parse_workflow_oracle(wf).items()
            if any(targets.values())
        }
        self.assertEqual(tokens, {"phantom", "real"})
        self.assertEqual(oracle, {"real"})
        self.assertNotEqual(tokens, oracle)


class SrcHasTests(unittest.TestCase):
    def test_src_attribute_counts_and_tests_dir_does_not(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            lib_only = _crate(root, "lib", "lib", "pub fn f() {}\n")
            with_test = _crate(root, "unit", "unit", "#[test]\nfn t() {}\n")
            tokio = _crate(root, "async", "async", "#[tokio::test]\nasync fn t() {}\n")
            integ = _crate(
                root,
                "integ",
                "integ",
                "pub fn f() {}\n",
                extra={"tests/foo.rs": "#[test]\nfn only_integration() {}\n"},
            )
            comment = _crate(root, "docs", "docs", "/// #[test]\npub fn f() {}\n")
            self.assertFalse(src_has_tests(lib_only))
            self.assertTrue(src_has_tests(with_test))
            self.assertTrue(src_has_tests(tokio))
            self.assertFalse(src_has_tests(integ))
            self.assertFalse(src_has_tests(comment))

    def test_accepts_module_qualified_and_spaced_test_attributes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for rel, attribute in {
                "rstest": "#[rstest::test]",
                "serial": "#[serial_test::test]",
                "spaced_hash": "# [test]",
                "spaced_path": "#[ tokio :: test ]",
                "nested": "#[some :: deeply_nested :: test]",
                "absolute": "#[::tokio::test]",
                "with_args": '#[tokio::test(flavor = "current_thread")]',
                "brace_args": "#[pm::test{}]",
                "bracket_args": "#[pm::test[args]]",
                "raw_module": "#[r#async::test]",
                "raw_test": "#[r#test]",
                "unicode_module": "#[异步::test]",
                "raw_unicode_module": "#[r#异步::test]",
            }.items():
                crate = _crate(root, rel, rel, f"{attribute}\nfn t() {{}}\n")
                with self.subTest(attribute=attribute):
                    self.assertTrue(src_has_tests(crate))

    def test_accepts_hygienic_crate_test_attribute_in_macro(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            crate = _crate(
                root,
                "macro_crate",
                "macro_crate",
                "pub use core::prelude::v1::test;\n"
                "macro_rules! generated_test {\n"
                "    () => {\n"
                "        #[$crate::test]\n"
                "        fn generated() {}\n"
                "    };\n"
                "}\n"
                "generated_test!();\n",
            )
            self.assertTrue(src_has_tests(crate))

    def test_comments_docs_and_strings_do_not_count_as_test_attributes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for rel, source in {
                "line_comment": "// #[rstest::test]\npub fn f() {}\n",
                "doc_comment": "/// # [ serial_test :: test ]\npub fn f() {}\n",
                "block_comment": "/* #[some::test] */\npub fn f() {}\n",
                "string": 'const SPELLING: &str = "#[rstest::test]";\n',
                "multiline_block_comment": (
                    "/* example only:\n#[some::test]\nfn sample() {}\n*/\n"
                ),
                "nested_block_comment": (
                    "/* outer\n/* nested */\n#[some::test]\n*/\npub fn f() {}\n"
                ),
                "raw_multiline_string": (
                    'const EXAMPLE: &str = r##"\n#[some::test]\nfn sample() {}\n"##;\n'
                ),
                "multiline_string": (
                    'const EXAMPLE: &str = "example\n#[some::test]\nstill text";\n'
                ),
                "lifetime_before_ordinary_string": r'''macro_rules! m { ($($tt:tt)*) => {} }
m!('r"fake \"
#[some::test]
");
pub fn f() {}
''',
                "literal_suffix_before_ordinary_string": r'''macro_rules! m { ($($tt:tt)*) => {} }
m!(""r"fake \"
#[some::test]
");
pub fn f() {}
''',
                "raw_literal_suffix_before_ordinary_string": r'''macro_rules! m { ($($tt:tt)*) => {} }
m!(r""r"fake \"
#[some::test]
");
pub fn f() {}
''',
                "char_literal_suffix_before_ordinary_string": r'''macro_rules! m { ($($tt:tt)*) => {} }
m!('x'r"fake \"
#[some::test]
");
pub fn f() {}
''',
                "nonterminal_test_segment": "#[test::fixture]\npub fn f() {}\n",
                "cfg": "#[cfg(test)]\npub fn f() {}\n",
            }.items():
                crate = _crate(root, rel, rel, source)
                with self.subTest(source=source):
                    self.assertFalse(src_has_tests(crate))

    def test_real_attribute_after_multiline_non_code_still_counts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = (
                'const EXAMPLE: &str = r#"\n#[some::test]\n"#;\n'
                "/* another fake:\n#[serial_test::test]\n*/\n"
                "#[::tokio::test]\nasync fn real_test() {}\n"
            )
            crate = _crate(root, "real_after_masked", "real_after_masked", source)
            self.assertTrue(src_has_tests(crate))


class Evaluate(unittest.TestCase):
    def _tree(self) -> tempfile.TemporaryDirectory:
        tmp = tempfile.TemporaryDirectory()
        root = Path(tmp.name)
        _crate(root, "crates/codegen/covered", "covered", "#[test]\nfn t() {}\n")
        _crate(root, "crates/codegen/gap", "gap", "#[test]\nfn t() {}\n")
        _crate(root, "crates/codegen/silent", "silent", "pub fn f() {}\n")
        _crate(
            root,
            "prod/mc/proxy",
            "prod-mc-proxy",
            "#[test]\nfn t() {}\n",
        )
        return tmp

    def test_new_gap_and_counts(self):
        with self._tree() as tmp:
            root = Path(tmp)
            report = evaluate(
                root=root,
                workflow_text="run_nonzero -p covered --lib foo\n",
                allowlisted={},
            )
        self.assertEqual(report.has_tests, frozenset({"covered", "gap", "prod-mc-proxy"}))
        self.assertEqual(report.named, frozenset({"covered"}))
        self.assertEqual(report.new_gaps, frozenset({"gap", "prod-mc-proxy"}))
        self.assertFalse(report.stale)

    def test_allowlisted_gap_is_ok_and_named_crate_is_stale(self):
        with self._tree() as tmp:
            root = Path(tmp)
            report = evaluate(
                root=root,
                workflow_text="run_nonzero -p covered --lib foo\n",
                allowlisted={
                    "gap": "why",
                    "prod-mc-proxy": "why",
                    "covered": "why",
                },
            )
        self.assertFalse(report.new_gaps)
        self.assertEqual(report.stale, frozenset({"covered"}))

    def test_manifest_path_covers_even_when_dir_differs_from_package(self):
        with self._tree() as tmp:
            root = Path(tmp)
            report = evaluate(
                root=root,
                workflow_text=(
                    "cargo test --manifest-path prod/mc/proxy/Cargo.toml --lib\n"
                    "run_nonzero -p covered --lib foo\n"
                    "run_nonzero -p gap --lib foo\n"
                ),
                allowlisted={},
            )
        self.assertFalse(report.new_gaps)
        self.assertIn("prod-mc-proxy", report.has_tests)

    def test_clippy_only_mention_is_a_new_gap_not_coverage(self):
        # #437 in miniature: a crate whose only `ci.yml` mention is a
        # `cargo clippy --manifest-path` is a new gap, because clippy proves
        # nothing about whether the crate's tests run. A single `run_nonzero
        # -p` lane for the same crate restores coverage.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _crate(root, "crates/codegen/tools", "tools", "#[test]\nfn t() {}\n")
            clippy_only = evaluate(
                root=root,
                workflow_text=(
                    "cargo clippy --manifest-path crates/codegen/tools/Cargo.toml "
                    "--all-targets -- -D warnings\n"
                ),
                allowlisted={},
            )
            self.assertEqual(clippy_only.new_gaps, frozenset({"tools"}))

            with_test_lane = evaluate(
                root=root,
                workflow_text=(
                    "cargo clippy --manifest-path crates/codegen/tools/Cargo.toml "
                    "--all-targets -- -D warnings\n"
                    "run_nonzero -p tools --lib some_test -- --nocapture\n"
                ),
                allowlisted={},
            )
            self.assertFalse(with_test_lane.new_gaps)


class AllowlistFile(unittest.TestCase):
    """The allowlist records *what* was decided, not only that someone did.

    Names only, which this file used to be, cannot distinguish "upstream
    crate, its tests are upstream's" from "added in a hurry during a sync to
    get CI green" -- and one of those is fine while the other is not (#504).
    """

    def _write(self, tmp: str, body: str) -> Path:
        path = Path(tmp) / "allowlist"
        path.write_text(textwrap.dedent(body), encoding="utf-8")
        return path

    def test_skips_comments_and_blanks_and_keeps_the_reason(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                tmp,
                """\
                # First burn-down targets: xai-grok-secrets and xai-chat-state.

                gap = upstream-only; no fork commits
                # ignored
                """,
            )
            self.assertEqual(load_allowlist(path), {"gap": "upstream-only; no fork commits"})

    def test_a_bare_name_is_an_error(self):
        # The exact shape of every line this file held before #504.
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, "gap\n")
            with self.assertRaises(AllowlistError) as caught:
                load_allowlist(path)
            self.assertIn("expected `<crate> = <reason>`", str(caught.exception))

    def test_an_empty_reason_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, "gap =   \n")
            with self.assertRaises(AllowlistError) as caught:
                load_allowlist(path)
            self.assertIn("has no reason", str(caught.exception))

    def test_a_reason_may_contain_an_equals_sign(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, "gap = covered by --features x=y elsewhere\n")
            self.assertEqual(
                load_allowlist(path), {"gap": "covered by --features x=y elsewhere"}
            )

    def test_write_allowlist_carries_existing_reasons_forward(self):
        # A `--write-allowlist` run that re-bootstrapped every line would
        # erase the justifications through the tool that maintains the file:
        # the #504 defect returning by the write path instead of the read one.
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "allowlist"
            write_allowlist(path, {"kept", "fresh"}, {"kept": "upstream-only, measured"})
            loaded = load_allowlist(path)
            self.assertEqual(loaded["kept"], "upstream-only, measured")
            self.assertEqual(loaded["fresh"], "tests run in no ci.yml lane; not yet triaged")

    def test_read_reasons_is_lenient_where_load_allowlist_is_strict(self):
        # `--write-allowlist` has to be able to read the file it repairs,
        # including the names-only format it replaces. The verifying path
        # must not gain the same tolerance, or the requirement is decorative.
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, "bare\nwith = a reason\n")
            self.assertEqual(read_reasons(path), {"with": "a reason"})
            with self.assertRaises(AllowlistError):
                load_allowlist(path)


class ThirdPartyRoots(unittest.TestCase):
    """#495: `_CRATE_ROOTS` used to omit `third_party/` entirely, so four
    real workspace members -- including two with real `src/` tests running
    in no `ci.yml` lane -- were invisible to this guard's corpus rather than
    reported as gaps.
    """

    def test_third_party_is_a_crate_root(self):
        self.assertIn("third_party", _CRATE_ROOTS)

    def test_dagre_rust_and_mermaid_to_svg_are_now_visible_with_tests(self):
        crates = {c.name: c for c in iter_crates(REPO)}
        self.assertIn("dagre_rust", crates)
        self.assertIn("mermaid-to-svg", crates)
        self.assertTrue(src_has_tests(crates["dagre_rust"].manifest.parent))
        self.assertTrue(src_has_tests(crates["mermaid-to-svg"].manifest.parent))

    def test_third_party_crates_without_tests_are_visible_but_not_gaps(self):
        # graphlib_rust and ordered_hashmap have no src/ tests -- confirming
        # this keeps the fix honest: the corpus grew, not just the allowlist.
        crates = {c.name: c for c in iter_crates(REPO)}
        self.assertIn("graphlib_rust", crates)
        self.assertIn("ordered_hashmap", crates)
        self.assertFalse(src_has_tests(crates["graphlib_rust"].manifest.parent))
        self.assertFalse(src_has_tests(crates["ordered_hashmap"].manifest.parent))


class Main(unittest.TestCase):
    @staticmethod
    def _real_argv(allowlist: Path) -> list[str]:
        return [
            "--workflow",
            str(REPO / ".github" / "workflows" / "ci.yml"),
            "--root",
            str(REPO),
            "--allowlist",
            str(allowlist),
        ]

    def test_exits_1_on_a_new_gap(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _crate(root, "crates/codegen/covered", "covered", "#[test]\nfn t() {}\n")
            _crate(root, "crates/codegen/gap", "gap", "#[test]\nfn t() {}\n")
            workflow = root / "ci.yml"
            workflow.write_text("run_nonzero -p covered --lib foo\n", encoding="utf-8")
            allowlist = root / "allowlist"
            allowlist.write_text("# empty\n", encoding="utf-8")
            self.assertEqual(
                main(
                    [
                        "--workflow",
                        str(workflow),
                        "--root",
                        str(root),
                        "--allowlist",
                        str(allowlist),
                    ]
                ),
                1,
            )

    def test_checked_in_allowlist_matches_the_repo(self):
        self.assertEqual(main(self._real_argv(_REAL_ALLOWLIST)), 0)


_REAL_ALLOWLIST = REPO / "tests" / "ci" / "uncovered-crates.allowlist"


class CheckedInAllowlist(unittest.TestCase):
    """The real file, against the real tree (#504).

    The assertions below run on today's actual entries rather than on a fixed
    synthetic corpus, because a corpus of stable values agrees with itself
    whatever the file says. Here the file and the tree can disagree, and the
    tests are what makes the disagreement audible.
    """

    def test_every_entry_carries_a_reason(self):
        entries = load_allowlist(_REAL_ALLOWLIST)
        self.assertTrue(entries)
        missing = sorted(name for name, reason in entries.items() if not reason.strip())
        self.assertEqual(missing, [], f"allowlist entries with no reason: {missing}")

    def test_dropping_an_entry_is_not_silent(self):
        # The migration to `<name> = <reason>` had to preserve all 38 names.
        # Nothing has to trust that it did: an entry that goes missing turns
        # its crate back into a new gap, which is red. This runs that, so the
        # count is load-bearing rather than a claim in a commit message.
        entries = load_allowlist(_REAL_ALLOWLIST)
        report = evaluate(
            root=REPO,
            workflow_text=(REPO / ".github" / "workflows" / "ci.yml").read_text(
                encoding="utf-8"
            ),
            allowlisted=entries,
        )
        dropped = sorted(set(entries) & report.has_tests)[0]
        with tempfile.TemporaryDirectory() as tmp:
            trimmed = Path(tmp) / "allowlist"
            trimmed.write_text(
                "".join(
                    f"{name} = {reason}\n"
                    for name, reason in sorted(entries.items())
                    if name != dropped
                ),
                encoding="utf-8",
            )
            self.assertEqual(main(Main._real_argv(trimmed)), 1)

    def test_a_bare_name_in_the_real_file_fails_the_run(self):
        # The pre-#504 spelling of a real line, in the real file's place: the
        # guard must exit non-zero rather than treat the entry as exempt.
        entries = load_allowlist(_REAL_ALLOWLIST)
        with tempfile.TemporaryDirectory() as tmp:
            regressed = Path(tmp) / "allowlist"
            lines = []
            for i, (name, reason) in enumerate(sorted(entries.items())):
                lines.append(f"{name}\n" if i == 0 else f"{name} = {reason}\n")
            regressed.write_text("".join(lines), encoding="utf-8")
            self.assertEqual(main(Main._real_argv(regressed)), 2)


if __name__ == "__main__":
    unittest.main()
