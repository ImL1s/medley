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
    evaluate,
    load_allowlist,
    main,
    named_tokens,
    package_name,
    src_has_tests,
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


class NamedTokens(unittest.TestCase):
    def test_extracts_p_and_manifest_path(self):
        wf = textwrap.dedent(
            """\
            run_nonzero -p covered --lib foo -- --nocapture
            cargo clippy --manifest-path crates/codegen/also-named/Cargo.toml
            """
        )
        self.assertEqual(named_tokens(wf), {"covered", "also-named"})

    def test_package_long_flag(self):
        self.assertEqual(named_tokens("cargo test --package boxed --lib\n"), {"boxed"})


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
                allowlisted=set(),
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
                allowlisted={"gap", "prod-mc-proxy", "covered"},
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
                allowlisted=set(),
            )
        self.assertFalse(report.new_gaps)
        self.assertIn("prod-mc-proxy", report.has_tests)


class AllowlistFile(unittest.TestCase):
    def test_skips_comments_and_blanks(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "allowlist"
            path.write_text(
                "# First burn-down targets: xai-grok-secrets and xai-chat-state.\n"
                "\n"
                "gap\n"
                "# ignored\n",
                encoding="utf-8",
            )
            self.assertEqual(load_allowlist(path), {"gap"})


class Main(unittest.TestCase):
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
        self.assertEqual(
            main(
                [
                    "--workflow",
                    str(REPO / ".github" / "workflows" / "ci.yml"),
                    "--root",
                    str(REPO),
                    "--allowlist",
                    str(REPO / "tests" / "ci" / "uncovered-crates.allowlist"),
                ]
            ),
            0,
        )


if __name__ == "__main__":
    unittest.main()
