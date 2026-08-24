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

    def test_keyed_serial_alone_is_sound_when_the_key_is_consistent(self):
        # Contract change (#446), deliberate: "keyed serial is insufficient"
        # was a global verdict, and it is not one. A keyed `#[serial]`
        # serialises every test sharing that key, so it is sound exactly when
        # every test mutating the variable agrees on one key for it. Measured,
        # `xai-grok-sandbox`'s 12 keyed tests all share `bwrap_env` and cannot
        # race each other; calling them violations was noise. The insufficient
        # case is a key CLASH, pinned in the test below.
        self.assertEqual(guard.scan_source(KEYED_SERIAL), [])

    def test_keyed_serial_is_a_violation_when_one_var_has_two_keys(self):
        source = textwrap.dedent(
            """\
            #[test]
            #[serial_test::serial(home_a)]
            fn one_key() {
                unsafe { std::env::set_var("HOME", "/tmp/a") };
            }

            #[test]
            #[serial_test::serial(home_b)]
            fn other_key() {
                unsafe { std::env::set_var("HOME", "/tmp/b") };
            }
            """
        )
        found = guard.scan_source(source)
        self.assertEqual(
            sorted(item.name for item in found), ["one_key", "other_key"]
        )
        self.assertIn("HOME", found[0].reason)

    def test_keyed_serial_clashing_with_an_unkeyed_mutation_is_a_violation(self):
        source = textwrap.dedent(
            """\
            #[test]
            #[serial_test::serial(home_a)]
            fn keyed_one() {
                unsafe { std::env::set_var("HOME", "/tmp/a") };
            }

            #[test]
            fn not_serialised_at_all() {
                unsafe { std::env::set_var("HOME", "/tmp/b") };
            }
            """
        )
        found = guard.scan_source(source)
        self.assertEqual(
            sorted(item.name for item in found), ["keyed_one", "not_serialised_at_all"]
        )

    def test_sole_test_in_its_own_integration_binary_is_sound(self):
        # No in-process sibling to corrupt. Detected from the path shape:
        # `…/<crate>/tests/<file>.rs` is its own binary.
        found = guard.scan_source(
            VIOLATION, relpath=Path("crates/codegen/x/tests/only_one.rs")
        )
        self.assertEqual(found, [])

    def test_tests_module_under_src_is_not_an_integration_binary(self):
        # `src/app/dispatch/tests/cta_e2e.rs` shares the lib's process; the
        # path contains "tests" but it is a module, not a target.
        found = guard.scan_source(
            VIOLATION, relpath=Path("crates/codegen/x/src/app/tests/cta_e2e.rs")
        )
        self.assertEqual([item.name for item in found], ["mutates_env_without_serial"])

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
        new, stale, outside = guard.evaluate(
            findings, ["src/fake.rs::mutates_env_without_serial"]
        )
        self.assertEqual(new, [])
        self.assertEqual(stale, [])
        self.assertEqual(outside, [])

    def test_new_violation_is_reported(self):
        findings = guard.scan_source(VIOLATION, relpath=Path("src/fake.rs"))
        new, stale, outside = guard.evaluate(findings, [])
        self.assertEqual([item.name for item in new], ["mutates_env_without_serial"])
        self.assertEqual(stale, [])
        self.assertEqual(outside, [])

    def test_stale_allowlist_entry_is_reported(self):
        findings = guard.scan_source(SERIAL_OK, relpath=Path("src/fake.rs"))
        new, stale, outside = guard.evaluate(findings, ["src/fake.rs::already_fixed"])
        self.assertEqual(new, [])
        self.assertEqual(stale, ["src/fake.rs::already_fixed"])
        self.assertEqual(outside, [])

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
            new, stale, _ = guard.evaluate(findings, guard.load_allowlist(allow))
            self.assertEqual(new, [])
            self.assertEqual(stale, [])

            allow.write_text("", encoding="utf-8")
            new, stale, _ = guard.evaluate(findings, guard.load_allowlist(allow))
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
        # Pass the scanned scope, as `main` does: without it every allowlist
        # entry from an unscanned crate reads as stale, which is the #446 bug.
        new, stale, _ = guard.evaluate(
            findings, allowlist, scan_rel=SHELL_SRC.relative_to(REPO).as_posix()
        )
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



# ── #446: the guard used to find its subject by NAME ────────────────────────

ALIASED_GUARD = textwrap.dedent(
    """\
    struct EnvVarGuard { prev: Option<String> }
    impl EnvVarGuard {
        fn set(k: &str, v: &str) -> Self {
            let prev = std::env::var(k).ok();
            unsafe { std::env::set_var(k, v) };
            Self { prev }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) { unsafe { std::env::remove_var("K") } }
    }

    #[test]
    fn uses_a_guard_that_is_not_called_EnvGuard() {
        let _g = EnvVarGuard::set("GROK_HOME", "/tmp");
    }
    """
)

SELF_LOCKING_GUARD = textwrap.dedent(
    """\
    struct EnvVarGuard { _lock: std::sync::MutexGuard<'static, ()> }
    impl EnvVarGuard {
        fn set(k: &str, v: &str) -> Self {
            let _lock = ENV_LOCK.lock().unwrap();
            unsafe { std::env::set_var(k, v) };
            Self { _lock }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) { unsafe { std::env::remove_var("K") } }
    }

    #[test]
    fn guard_takes_the_lock_itself() {
        let _g = EnvVarGuard::set("GROK_HOME", "/tmp");
    }
    """
)

LOCK_IN_BODY = textwrap.dedent(
    """\
    #[test]
    fn body_holds_the_crate_lock() {
        let _lock = ENV_TEST_LOCK.lock().unwrap();
        unsafe { std::env::set_var("GROK_HOME", "/tmp") };
    }
    """
)

WRAPPER_OWNING_A_GUARD = textwrap.dedent(
    """\
    struct TestEnvGuard { prev: Option<String> }
    impl TestEnvGuard {
        fn set(k: &str, v: &str) -> Self {
            let prev = None;
            unsafe { std::env::set_var(k, v) };
            Self { prev }
        }
    }
    impl Drop for TestEnvGuard {
        fn drop(&mut self) { unsafe { std::env::remove_var("K") } }
    }

    struct LockedTestEnv {
        _env: Vec<TestEnvGuard>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl LockedTestEnv {
        fn lock() -> Self {
            Self { _env: Vec::new(), _lock: ENV_TEST_LOCK.lock().unwrap() }
        }
        fn set(mut self, k: &str, v: &str) -> Self {
            self._env.push(TestEnvGuard::set(k, v));
            self
        }
    }

    #[test]
    fn wrapper_is_sound_because_it_locks() {
        let _env = LockedTestEnv::lock().set("HOME", "/tmp");
    }
    """
)

NON_LOCKING_GUARD_FOLLOWED_BY_A_LOCK = textwrap.dedent(
    """\
    struct TestEnvGuard { prev: Option<String> }
    impl TestEnvGuard {
        fn set(k: &str, v: &str) -> Self {
            unsafe { std::env::set_var(k, v) };
            Self { prev: None }
        }
    }
    impl Drop for TestEnvGuard {
        fn drop(&mut self) { unsafe { std::env::remove_var("K") } }
    }

    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn guard_does_not_lock_so_this_is_a_violation() {
        let _g = TestEnvGuard::set("GROK_HOME", "/tmp");
    }
    """
)


class NameIndependentDetection(unittest.TestCase):
    """#446: a guard is found by what it does, not by what it is called."""

    def test_guard_not_named_EnvGuard_is_still_detected(self):
        found = guard.scan_source(ALIASED_GUARD)
        self.assertEqual(
            [item.name for item in found], ["uses_a_guard_that_is_not_called_EnvGuard"]
        )

    def test_name_fallback_has_no_mandatory_prefix_character(self):
        # The old matcher was `\bEnvGuard\s*::`, which cannot match
        # `EnvVarGuard::`; a `[A-Za-z_][A-Za-z0-9_]*` prefix has the same bug
        # because it demands at least one character before `Env`.
        self.assertTrue(guard.ENV_GUARD_NAME.search("EnvVarGuard::set"))
        self.assertTrue(guard.ENV_GUARD_NAME.search("TestEnvGuard::set"))
        self.assertTrue(guard.ENV_GUARD_NAME.search("EnvGuard::set"))

    def test_guard_that_takes_the_lock_itself_is_not_a_violation(self):
        self.assertEqual(guard.scan_source(SELF_LOCKING_GUARD), [])

    def test_lock_held_in_the_test_body_is_not_a_violation(self):
        self.assertEqual(guard.scan_source(LOCK_IN_BODY), [])

    def test_wrapper_owning_a_guard_and_a_lock_is_not_a_violation(self):
        self.assertEqual(guard.scan_source(WRAPPER_OWNING_A_GUARD), [])

    def test_lock_after_the_impl_block_does_not_vouch_for_the_guard(self):
        # Regression: reading a fixed window past `impl` swept in the NEXT
        # item's lock and called the guard self-locking — a silent pass, which
        # is the failure mode this guard exists to prevent. Block-scoped now.
        found = guard.scan_source(NON_LOCKING_GUARD_FOLLOWED_BY_A_LOCK)
        self.assertEqual(
            [item.name for item in found],
            ["guard_does_not_lock_so_this_is_a_violation"],
        )


class ScanRootVerdict(unittest.TestCase):
    """#446: "outside the scan root" is not "stale"."""

    def test_entry_outside_the_scan_root_is_not_stale(self):
        findings = guard.scan_source(SERIAL_OK, relpath=Path("crates/a/src/ok.rs"))
        new, stale, outside = guard.evaluate(
            findings,
            ["crates/b/src/other.rs::some_test"],
            scan_rel="crates/a/src",
        )
        self.assertEqual(new, [])
        self.assertEqual(stale, [], "an unscanned file cannot be judged stale")
        self.assertEqual(outside, ["crates/b/src/other.rs::some_test"])

    def test_entry_inside_the_scan_root_with_no_finding_is_stale(self):
        findings = guard.scan_source(SERIAL_OK, relpath=Path("crates/a/src/ok.rs"))
        new, stale, outside = guard.evaluate(
            findings, ["crates/a/src/ok.rs::already_fixed"], scan_rel="crates/a/src"
        )
        self.assertEqual(stale, ["crates/a/src/ok.rs::already_fixed"])
        self.assertEqual(outside, [])

    def test_report_names_the_scanned_scope(self):
        text = guard.format_report(
            [], [], finding_count=0, allowlist_count=0, scan_rel="crates/x/src"
        )
        self.assertIn("crates/x/src", text)

if __name__ == "__main__":
    unittest.main()
