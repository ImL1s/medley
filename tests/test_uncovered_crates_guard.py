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
    evaluate,
    load_allowlist,
    main,
    named_tokens,
    package_name,
    read_reasons,
    src_has_tests,
    write_allowlist,
)


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

    def test_a_comment_on_the_package_header_does_not_hide_the_table(self):
        # `[package] # metadata` is valid TOML. An exact `== "[package]"`
        # never enters the table, and the name goes missing -- which
        # `check_test_filter_coverage.workspace_members()` turns into the
        # directory basename, a wrong package name rather than no name.
        self.assertEqual(
            package_name('[package] # metadata\nname = "foo"\n'), "foo"
        )

    def test_a_comment_on_a_later_header_still_ends_the_package_table(self):
        # The worse direction of the same miss: an unrecognised header does
        # not *close* `[package]` either, so a `name` belonging to a later
        # table is returned as the package name. A confident wrong answer,
        # not a missing one.
        self.assertIsNone(
            package_name(
                '[package]\nversion = "1"\n[features] # x\nname = "wrong"\n'
            )
        )

    def test_a_bare_bin_table_still_ends_it(self):
        # The uncommented form must keep working: this is the case the
        # section scoping exists for.
        self.assertIsNone(
            package_name('[package]\nversion = "1"\n[[bin]]\nname = "b"\n')
        )


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
