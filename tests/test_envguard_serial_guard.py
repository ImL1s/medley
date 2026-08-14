"""Tests for the #319 EnvGuard / unkeyed-serial static guard."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO / "scripts" / "check_envguard_serial.py"
ALLOWLIST_PATH = REPO / "tests" / "ci" / "envguard-serial-allowlist.txt"
SHELL_SRC = REPO / "crates" / "codegen" / "xai-grok-shell" / "src"


def load_guard():
    spec = importlib.util.spec_from_file_location("check_envguard_serial", SCRIPT_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {SCRIPT_PATH}")
    module = importlib.util.module_from_spec(spec)
    # dataclass + `from __future__ import annotations` needs the module in
    # sys.modules before exec_module (Python 3.14).
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


guard = load_guard()


VIOLATION = textwrap.dedent(
    """\
    #[test]
    fn mutates_env_without_serial() {
        let _g = EnvGuard::set("GROK_HOME", "/tmp");
    }
    """
)

SERIAL_OK = textwrap.dedent(
    """\
    #[test]
    #[serial_test::serial]
    fn mutates_env_with_unkeyed_serial() {
        let _g = EnvGuard::unset("GROK_HOME");
    }

    #[test]
    #[serial]
    fn imported_serial_is_enough() {
        unsafe { std::env::set_var("GROK_HOME", "/tmp") };
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn tokio_test_with_serial() {
        unsafe { std::env::remove_var("GROK_HOME") };
    }
    """
)

KEYED_SERIAL = textwrap.dedent(
    """\
    #[test]
    #[serial_test::serial(force_dark_wake_env)]
    fn keyed_serial_is_not_crate_wide() {
        let _g = EnvGuard::set("GROK_AUTH_FORCE_DARK_WAKE", "1");
    }
    """
)

HELPER_ONLY = textwrap.dedent(
    """\
    fn isolate() -> EnvGuard {
        EnvGuard::unset("XAI_API_KEY")
    }

    #[test]
    fn calls_helper_but_does_not_mention_envguard() {
        let _g = isolate();
    }
    """
)

COMMENTED = textwrap.dedent(
    """\
    #[test]
    fn comment_is_not_a_mention() {
        // EnvGuard::set("GROK_HOME", "/tmp");
        let _s = "std::env::set_var";
    }
    """
)


class ScanSource(unittest.TestCase):
    def test_violation_without_serial(self):
        found = guard.scan_source(VIOLATION)
        self.assertEqual([item.name for item in found], ["mutates_env_without_serial"])
        self.assertIn("EnvGuard::", found[0].reason)

    def test_unkeyed_serial_is_ok(self):
        self.assertEqual(guard.scan_source(SERIAL_OK), [])

    def test_keyed_serial_is_insufficient(self):
        found = guard.scan_source(KEYED_SERIAL)
        self.assertEqual([item.name for item in found], ["keyed_serial_is_not_crate_wide"])
        self.assertIn("keyed", found[0].reason)

    def test_helper_is_not_a_test(self):
        self.assertEqual(guard.scan_source(HELPER_ONLY), [])

    def test_comments_and_strings_are_ignored(self):
        self.assertEqual(guard.scan_source(COMMENTED), [])

    def test_doc_comment_between_attrs_and_fn_does_not_drop_serial(self):
        source = textwrap.dedent(
            """\
            #[test]
            #[serial]
            /// why this exists
            fn still_serial() {
                let _g = EnvGuard::set("K", "v");
            }
            """
        )
        self.assertEqual(guard.scan_source(source), [])

    def test_std_env_set_var_without_serial_is_a_violation(self):
        source = textwrap.dedent(
            """\
            #[test]
            fn raw_set_var() {
                unsafe { std::env::set_var("K", "v") };
            }
            """
        )
        found = guard.scan_source(source)
        self.assertEqual([item.name for item in found], ["raw_set_var"])


class AllowlistEvaluate(unittest.TestCase):
    def test_allowlisted_violation_is_suppressed(self):
        findings = guard.scan_source(VIOLATION, relpath=Path("src/fake.rs"))
        new, stale = guard.evaluate(
            findings, ["src/fake.rs::mutates_env_without_serial"]
        )
        self.assertEqual(new, [])
        self.assertEqual(stale, [])

    def test_new_violation_is_reported(self):
        findings = guard.scan_source(VIOLATION, relpath=Path("src/fake.rs"))
        new, stale = guard.evaluate(findings, [])
        self.assertEqual([item.name for item in new], ["mutates_env_without_serial"])
        self.assertEqual(stale, [])

    def test_stale_allowlist_entry_is_reported(self):
        findings = guard.scan_source(SERIAL_OK, relpath=Path("src/fake.rs"))
        new, stale = guard.evaluate(findings, ["src/fake.rs::already_fixed"])
        self.assertEqual(new, [])
        self.assertEqual(stale, ["src/fake.rs::already_fixed"])

    def test_temp_tree_matches_allowlist_contract(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "crates" / "codegen" / "xai-grok-shell" / "src"
            src.mkdir(parents=True)
            (src / "ok.rs").write_text(SERIAL_OK, encoding="utf-8")
            (src / "bad.rs").write_text(VIOLATION, encoding="utf-8")
            allow = root / "allow.txt"
            allow.write_text(
                "crates/codegen/xai-grok-shell/src/bad.rs::mutates_env_without_serial\n",
                encoding="utf-8",
            )
            findings = guard.scan_tree(src, repo=root)
            new, stale = guard.evaluate(findings, guard.load_allowlist(allow))
            self.assertEqual(new, [])
            self.assertEqual(stale, [])

            allow.write_text("", encoding="utf-8")
            new, stale = guard.evaluate(findings, guard.load_allowlist(allow))
            self.assertEqual(
                [item.allowlist_id for item in new],
                ["crates/codegen/xai-grok-shell/src/bad.rs::mutates_env_without_serial"],
            )


class RepositoryScan(unittest.TestCase):
    def test_allowlist_file_exists(self):
        self.assertTrue(ALLOWLIST_PATH.is_file(), ALLOWLIST_PATH)
        self.assertTrue(SHELL_SRC.is_dir(), SHELL_SRC)

    def test_repository_allowlist_is_exact(self):
        findings = guard.scan_tree(SHELL_SRC, repo=REPO)
        allowlist = guard.load_allowlist(ALLOWLIST_PATH)
        new, stale = guard.evaluate(findings, allowlist)
        self.assertEqual(
            (new, stale),
            ([], []),
            "\n"
            + guard.format_report(
                new,
                stale,
                finding_count=len(findings),
                allowlist_count=len(allowlist),
            ),
        )

    def test_script_main_exits_zero_against_this_tree(self):
        self.assertEqual(guard.main(["--repo", str(REPO)]), 0)


if __name__ == "__main__":
    unittest.main()
