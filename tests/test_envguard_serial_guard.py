"""Tests for the #319 EnvGuard / unkeyed-serial static guard."""

from __future__ import annotations

import importlib.util
import re
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
        # Same shape, second regex: the env-lock matcher must not require a
        # character before `ENV` either.
        import re as _re

        for lock in ("ENV_LOCK", "ENV_TEST_LOCK", "TOOL_STATE_ENV_LOCK"):
            self.assertTrue(_re.search(guard.ENV_LOCK_NAME, lock), lock)

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


# ── #449 review: three false negatives, each pinned in both directions ──────


def _guard_src(name, *, locks, test_name, use=""):
    field = "_lock: std::sync::MutexGuard<'static, ()>," if locks else "prev: Option<String>,"
    build = "let _l = SOME_LOCK.lock().unwrap();" if locks else ""
    return textwrap.dedent(
        f"""\
        struct {name} {{ {field} }}
        impl {name} {{
            fn set(k: &str, v: &str) -> Self {{
                {build}
                unsafe {{ std::env::set_var(k, v) }};
                todo!()
            }}
        }}
        impl Drop for {name} {{
            fn drop(&mut self) {{ unsafe {{ std::env::remove_var("K") }} }}
        }}

        {use}
        #[test]
        fn {test_name}() {{
            let _g = {name}::set("GROK_HOME", "/tmp");
        }}
        """
    )


class SameNameDifferentGuards(unittest.TestCase):
    """Finding 1: two guards may share a name and disagree about locking."""

    def test_two_definitions_do_not_vouch_for_each_other(self):
        locking = _guard_src("EnvVarGuard", locks=True, test_name="uses_locking_guard")
        plain = _guard_src("EnvVarGuard", locks=False, test_name="uses_plain_guard")
        mutators = guard.index_env_mutators(
            [
                (Path("crates/codegen/a/src/locking.rs"), locking),
                (Path("crates/codegen/b/src/plain.rs"), plain),
            ]
        )
        # Resolved per definition site, not merged by name.
        self.assertTrue(
            mutators.self_locks("EnvVarGuard", "a", "crates/codegen/a/src/locking.rs")
        )
        self.assertFalse(
            mutators.self_locks("EnvVarGuard", "b", "crates/codegen/b/src/plain.rs")
        )
        # And the non-locking one's user is still reported.
        found = guard.scan_source(
            plain, relpath=Path("crates/codegen/b/src/plain.rs"), mutators=mutators
        )
        self.assertEqual([f.name for f in found], ["uses_plain_guard"])
        self.assertEqual(
            guard.scan_source(
                locking,
                relpath=Path("crates/codegen/a/src/locking.rs"),
                mutators=mutators,
            ),
            [],
        )


class LockMustBeLive(unittest.TestCase):
    """Finding 2: the token appearing is not the guard being held."""

    def _src(self, stmt):
        return textwrap.dedent(
            f"""\
            #[test]
            fn t() {{
                {stmt}
                unsafe {{ std::env::set_var("GROK_HOME", "/tmp") }};
            }}
            """
        )

    def test_discarded_lock_guard_is_not_serialisation(self):
        # `let _ = ...` drops at the end of the statement.
        found = guard.scan_source(self._src("let _ = ENV_TEST_LOCK.lock().unwrap();"))
        self.assertEqual([f.name for f in found], ["t"])

    def test_explicitly_dropped_lock_is_not_serialisation(self):
        src = textwrap.dedent(
            """\
            #[test]
            fn t() {
                let lock = ENV_TEST_LOCK.lock().unwrap();
                drop(lock);
                unsafe { std::env::set_var("GROK_HOME", "/tmp") };
            }
            """
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])

    def test_unrelated_mutex_is_not_the_env_lock(self):
        found = guard.scan_source(self._src("let _m = RENDER_CACHE.lock().unwrap();"))
        self.assertEqual([f.name for f in found], ["t"])

    def test_live_env_lock_guard_is_serialisation(self):
        self.assertEqual(
            guard.scan_source(self._src("let _lock = ENV_TEST_LOCK.lock().unwrap();")), []
        )


class UnkeyedCountsInTheKeyMap(unittest.TestCase):
    """Finding 3: unkeyed `#[serial]` and `#[serial(k)]` take different locks."""

    def test_keyed_test_clashing_with_an_unkeyed_serial_is_reported(self):
        src = textwrap.dedent(
            """\
            #[test]
            #[serial_test::serial]
            fn unkeyed_one() {
                unsafe { std::env::set_var("HOME", "/tmp/a") };
            }

            #[test]
            #[serial_test::serial(home)]
            fn keyed_one() {
                unsafe { std::env::set_var("HOME", "/tmp/b") };
            }
            """
        )
        found = guard.scan_source(src)
        # The unkeyed one is sound on its own terms; the keyed one is not
        # protected from it and must be reported.
        self.assertEqual([f.name for f in found], ["keyed_one"])
        self.assertIn("HOME", found[0].reason)


class KeyMapIsScopedPerBinary(unittest.TestCase):
    def test_two_crates_do_not_clash(self):
        # Different crates never share a process, so their keys cannot collide.
        one = textwrap.dedent(
            """\
            #[test]
            #[serial_test::serial(home_a)]
            fn a() { unsafe { std::env::set_var("HOME", "/x") }; }
            """
        )
        two = textwrap.dedent(
            """\
            #[test]
            #[serial_test::serial(home_b)]
            fn b() { unsafe { std::env::set_var("HOME", "/y") }; }
            """
        )
        cands = guard.analyze_source(one, relpath=Path("crates/codegen/a/src/x.rs"))
        cands += guard.analyze_source(two, relpath=Path("crates/codegen/b/src/y.rs"))
        self.assertEqual(guard.judge(cands, guard.key_map(cands)), [])


class RegexesAgreeWithTheTree(unittest.TestCase):
    """Corpus from the repo, not from imagination.

    Every off-by-one in this file — three of them — came from the same place:
    a regex hand-written against imagined names, checked against hand-written
    examples. Both sides come from one head, so a wrong mental model produces a
    matching wrong test, which is why the regression test added for the first
    instance did not stop the third being written in the same commit.

    These enumerate the real identifiers by a DIFFERENT mechanism — tokenize,
    then plain substring predicates — and assert the regex under test agrees on
    each. The inputs are not chosen by whoever wrote the pattern.
    """

    TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")

    @classmethod
    def setUpClass(cls):
        locks: set[str] = set()
        guards: set[str] = set()
        for path in guard.rust_files(REPO / "crates"):
            for token in cls.TOKEN.findall(path.read_text(encoding="utf-8")):
                if token.isupper() and "ENV" in token and token.endswith("_LOCK"):
                    locks.add(token)
                if "Env" in token and token.endswith("Guard"):
                    guards.add(token)
        cls.locks = sorted(locks)
        cls.guards = sorted(guards)

    def test_the_corpus_is_not_empty(self):
        # A corpus scan that silently finds nothing passes every assertion
        # below while checking nothing at all.
        self.assertGreaterEqual(len(self.locks), 5, self.locks)
        self.assertGreaterEqual(len(self.guards), 3, self.guards)

    def test_env_lock_name_matches_every_env_lock_in_the_tree(self):
        missed = [name for name in self.locks if not re.search(guard.ENV_LOCK_NAME, name)]
        self.assertEqual(missed, [], f"ENV_LOCK_NAME misses real locks: {missed}")

    def test_env_guard_name_matches_every_env_guard_type_in_the_tree(self):
        missed = [n for n in self.guards if not guard.ENV_GUARD_NAME.search(f"{n}::set")]
        self.assertEqual(missed, [], f"ENV_GUARD_NAME misses real guards: {missed}")

    def test_env_lock_name_still_discriminates(self):
        # The pattern must not have widened into "any lock".
        for unrelated in ("PASTEBOARD_LOCK", "SAVE_LOCK", "DISMISS_LOCK", "HEAL_LOCK"):
            self.assertIsNone(
                re.search(guard.ENV_LOCK_NAME, unrelated),
                f"{unrelated} is not an env lock",
            )


_GUARD_DEF = textwrap.dedent(
    """\
    struct EnvVarGuard { _lock: std::sync::MutexGuard<'static, ()> }
    impl EnvVarGuard {
        fn set(k: &str, v: &str) -> Self {
            let l = SOME_LOCK.lock().unwrap();
            unsafe { std::env::set_var(k, v) };
            Self { _lock: l }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) { unsafe { std::env::remove_var("K") } }
    }

    """
)


class ProtectionIsPositional(unittest.TestCase):
    """#449 round 2: a lock that exists is not a lock that covers."""

    def test_lock_acquired_after_the_mutation_does_not_protect_it(self):
        src = textwrap.dedent(
            """\
            #[test]
            fn t() {
                unsafe { std::env::set_var("HOME", "/tmp") };
                let _lock = ENV_TEST_LOCK.lock().unwrap();
            }
            """
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])

    def test_lock_acquired_before_the_mutation_does_protect_it(self):
        src = textwrap.dedent(
            """\
            #[test]
            fn t() {
                let _lock = ENV_TEST_LOCK.lock().unwrap();
                unsafe { std::env::set_var("HOME", "/tmp") };
            }
            """
        )
        self.assertEqual(guard.scan_source(src), [])

    def test_unbound_guard_temporary_does_not_cover_a_later_mutation(self):
        # `EnvVarGuard::set(..);` drops at that semicolon, so the raw mutation
        # on the next line runs with no lock held.
        src = _GUARD_DEF + textwrap.dedent(
            """\
            #[test]
            fn t() {
                EnvVarGuard::set("A", "1");
                unsafe { std::env::set_var("HOME", "/tmp") };
            }
            """
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])

    def test_bound_self_locking_guard_covers_the_rest_of_the_body(self):
        src = _GUARD_DEF + textwrap.dedent(
            """\
            #[test]
            fn t() {
                let _g = EnvVarGuard::set("A", "1");
                unsafe { std::env::set_var("HOME", "/tmp") };
            }
            """
        )
        self.assertEqual(guard.scan_source(src), [])

    def test_dropping_the_guard_ends_its_protection(self):
        src = _GUARD_DEF + textwrap.dedent(
            """\
            #[test]
            fn t() {
                let g = EnvVarGuard::set("A", "1");
                drop(g);
                unsafe { std::env::set_var("HOME", "/tmp") };
            }
            """
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])


class HelperVariablesArePropagated(unittest.TestCase):
    """#449 round 2: the variable may be mutated inside the helper."""

    SRC = textwrap.dedent(
        """\
        fn set_home(v: &str) {
            unsafe { std::env::set_var("HOME", v) };
        }

        #[test]
        #[serial_test::serial(a)]
        fn keyed_a() { set_home("/a"); }

        #[test]
        #[serial_test::serial(b)]
        fn keyed_b() { set_home("/b"); }
        """
    )

    def test_two_keys_reaching_one_variable_through_a_helper_clash(self):
        found = guard.scan_source(self.SRC)
        self.assertEqual(sorted(f.name for f in found), ["keyed_a", "keyed_b"])
        self.assertIn("HOME", found[0].reason)

    def test_helper_variables_reach_the_candidate(self):
        cands = guard.analyze_source(self.SRC)
        by_name = {c.name: c for c in cands}
        self.assertIn("HOME", by_name["keyed_a"].variables)


class UnknownVariableIsNotSafe(unittest.TestCase):
    def test_keyed_test_with_undeterminable_variable_is_reported(self):
        src = textwrap.dedent(
            """\
            #[test]
            #[serial_test::serial(some_key)]
            fn t() {
                let name = compute_name();
                unsafe { std::env::set_var(name, "1") };
            }
            """
        )
        found = guard.scan_source(src)
        self.assertEqual([f.name for f in found], ["t"])
        self.assertIn("could not be determined", found[0].reason)


class ProtectorMustReallyOwnALock(unittest.TestCase):
    """#449 round 3: owning a lock, not mentioning one."""

    def test_identifier_containing_env_lock_is_not_a_lock_owner(self):
        src = textwrap.dedent(
            """\
            #[test]
            fn t() {
                let cfg = read_env_lock_setting();
                unsafe { std::env::set_var("HOME", "/tmp") };
            }
            """
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])

    def test_actual_lock_acquisition_is_a_lock_owner(self):
        src = textwrap.dedent(
            """\
            #[test]
            fn t() {
                let cfg = ENV_TEST_LOCK.lock().unwrap();
                unsafe { std::env::set_var("HOME", "/tmp") };
            }
            """
        )
        self.assertEqual(guard.scan_source(src), [])

    def test_helper_that_locks_but_returns_unit_does_not_protect_its_caller(self):
        src = textwrap.dedent(
            """\
            fn set_a() {
                let _l = ENV_TEST_LOCK.lock().unwrap();
                unsafe { std::env::set_var("A", "1") };
            }

            #[test]
            fn t() {
                let result = set_a();
                unsafe { std::env::set_var("HOME", "/tmp") };
            }
            """
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])

    def test_helper_that_returns_the_guard_does_protect_its_caller(self):
        src = textwrap.dedent(
            """\
            fn locked() -> std::sync::MutexGuard<'static, ()> {
                let g = ENV_TEST_LOCK.lock().unwrap();
                unsafe { std::env::set_var("A", "1") };
                g
            }

            #[test]
            fn t() {
                let _held = locked();
                unsafe { std::env::set_var("HOME", "/tmp") };
            }
            """
        )
        self.assertEqual(guard.scan_source(src), [])

    def test_lock_taken_in_a_nested_block_does_not_protect_after_it(self):
        # Rust drops the guard at the inner `}`.
        src = textwrap.dedent(
            """\
            #[test]
            fn t() {
                {
                    let _lock = ENV_TEST_LOCK.lock().unwrap();
                    unsafe { std::env::set_var("A", "1") };
                }
                unsafe { std::env::set_var("HOME", "/tmp") };
            }
            """
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])


class MultipleKeysAreHeldJointly(unittest.TestCase):
    def test_a_lone_test_with_two_keys_does_not_conflict_with_itself(self):
        src = textwrap.dedent(
            """\
            #[test]
            #[serial_test::serial(a)]
            #[serial_test::serial(b)]
            fn t() {
                unsafe { std::env::set_var("A", "1") };
            }
            """
        )
        self.assertEqual(guard.scan_source(src), [])

    def test_two_tests_sharing_one_of_their_keys_do_not_clash(self):
        src = textwrap.dedent(
            """\
            #[test]
            #[serial_test::serial(shared)]
            #[serial_test::serial(a)]
            fn one() { unsafe { std::env::set_var("A", "1") }; }

            #[test]
            #[serial_test::serial(shared)]
            fn two() { unsafe { std::env::set_var("A", "2") }; }
            """
        )
        self.assertEqual(guard.scan_source(src), [])

    def test_two_tests_with_no_key_in_common_still_clash(self):
        src = textwrap.dedent(
            """\
            #[test]
            #[serial_test::serial(a)]
            fn one() { unsafe { std::env::set_var("A", "1") }; }

            #[test]
            #[serial_test::serial(b)]
            fn two() { unsafe { std::env::set_var("A", "2") }; }
            """
        )
        self.assertEqual(sorted(f.name for f in guard.scan_source(src)), ["one", "two"])


class HelperLockMustPrecedeItsMutation(unittest.TestCase):
    """#449 round 4: the same positional rule, applied to helpers."""

    def _src(self, helper_body):
        return textwrap.dedent(
            f"""\
            fn prepare() {{
            {helper_body}
            }}

            #[test]
            fn t() {{
                prepare();
            }}
            """
        )

    def test_helper_locking_after_its_mutation_does_not_serialise_it(self):
        src = self._src(
            '    unsafe { std::env::set_var("HOME", "/tmp") };\n'
            "    let _lock = ENV_TEST_LOCK.lock().unwrap();"
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])

    def test_helper_locking_before_its_mutation_does_serialise_it(self):
        src = self._src(
            "    let _lock = ENV_TEST_LOCK.lock().unwrap();\n"
            '    unsafe { std::env::set_var("HOME", "/tmp") };'
        )
        self.assertEqual(guard.scan_source(src), [])

    def test_helper_releasing_before_its_last_mutation_does_not_serialise_it(self):
        src = self._src(
            "    let lock = ENV_TEST_LOCK.lock().unwrap();\n"
            '    unsafe { std::env::set_var("A", "1") };\n'
            "    drop(lock);\n"
            '    unsafe { std::env::set_var("HOME", "/tmp") };'
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])


class ConstantsResolveToTheirVariable(unittest.TestCase):
    """#449 round 5: `HOME_VAR` and `"HOME"` are the same process-global."""

    SRC = textwrap.dedent(
        """\
        const HOME_VAR: &str = "HOME";

        #[test]
        #[serial_test::serial(a)]
        fn via_const() { unsafe { std::env::set_var(HOME_VAR, "/a") }; }

        #[test]
        #[serial_test::serial(b)]
        fn via_literal() { unsafe { std::env::set_var("HOME", "/b") }; }
        """
    )

    def test_const_and_literal_are_compared_as_one_variable(self):
        found = guard.scan_source(self.SRC)
        self.assertEqual(sorted(f.name for f in found), ["via_const", "via_literal"])

    def test_the_constant_resolves_in_the_candidate(self):
        by_name = {c.name: c for c in guard.analyze_source(self.SRC)}
        self.assertIn("HOME", by_name["via_const"].variables)
        self.assertNotIn("HOME_VAR", by_name["via_const"].variables)


class SoleTestRegimeNeedsTheWholeBinary(unittest.TestCase):
    """#449 round 5: an integration root can pull in more tests."""

    BODY = textwrap.dedent(
        """\
        #[test]
        fn only_local_test() {
            unsafe { std::env::set_var("HOME", "/tmp") };
        }
        """
    )

    def test_a_single_file_target_is_still_isolated(self):
        found = guard.scan_source(
            self.BODY, relpath=Path("crates/codegen/x/tests/solo.rs")
        )
        self.assertEqual(found, [])

    def test_a_target_declaring_modules_is_not_assumed_isolated(self):
        # `mod common;` compiles its tests into the same binary, so "one test
        # in this file" says nothing about how many share the process.
        found = guard.scan_source(
            "mod common;\n" + self.BODY,
            relpath=Path("crates/codegen/x/tests/root.rs"),
        )
        self.assertEqual([f.name for f in found], ["only_local_test"])


class GuardDefinitionNamesTheVariable(unittest.TestCase):
    """#449: a setter's first argument may be a VALUE, not a variable name."""

    def _src(self, locking):
        field = "_lock: std::sync::MutexGuard<'static, ()>" if locking else "prev: Option<String>"
        acquire = "let l = ENV_LOCK.lock().unwrap();" if locking else ""
        init = "Self { _lock: l }" if locking else "Self { prev: None }"
        return textwrap.dedent(
            f"""\
            const FIXED: &str = "HOME";
            struct FixedEnvGuard {{ {field} }}
            impl FixedEnvGuard {{
                fn set(value: &str) -> Self {{
                    {acquire}
                    unsafe {{ std::env::set_var(FIXED, value) }};
                    {init}
                }}
            }}
            impl Drop for FixedEnvGuard {{
                fn drop(&mut self) {{ unsafe {{ std::env::remove_var(FIXED) }} }}
            }}

            #[test]
            #[serial_test::serial(a)]
            fn one() {{ let _g = FixedEnvGuard::set("first"); }}

            #[test]
            #[serial_test::serial(b)]
            fn two() {{ let _g = FixedEnvGuard::set("second"); }}
            """
        )

    def test_the_guards_own_variable_is_recorded_not_the_value(self):
        by_name = {c.name: c for c in guard.analyze_source(self._src(locking=False))}
        self.assertEqual(by_name["one"].variables, ("HOME",))
        self.assertEqual(by_name["two"].variables, ("HOME",))

    def test_two_keys_on_that_variable_clash(self):
        found = guard.scan_source(self._src(locking=False))
        self.assertEqual(sorted(f.name for f in found), ["one", "two"])

    def test_a_self_locking_fixed_guard_is_still_sound(self):
        # Same shape, but the guard holds the lock, so the keys are irrelevant.
        self.assertEqual(guard.scan_source(self._src(locking=True)), [])


class SelfLockingGuardCallIsNotAnUnprotectedSite(unittest.TestCase):
    """#449: the mirror of the self-locking HELPER exclusion, one level over."""

    GUARD = textwrap.dedent(
        """\
        struct G { _lock: std::sync::MutexGuard<'static, ()> }
        impl G {
            fn set(k: &str) -> Self {
                let l = ENV_TEST_LOCK.lock().unwrap();
                unsafe { std::env::set_var(k, "1") };
                Self { _lock: l }
            }
        }
        impl Drop for G { fn drop(&mut self) { unsafe { std::env::remove_var("K") } } }

        """
    )

    def test_an_unbound_self_locking_guard_call_is_not_a_violation(self):
        src = self.GUARD + textwrap.dedent(
            """\
            #[test]
            fn t() {
                G::set("HOME");
            }
            """
        )
        self.assertEqual(guard.scan_source(src), [])

    def test_a_later_raw_mutation_still_needs_its_own_span(self):
        src = self.GUARD + textwrap.dedent(
            """\
            #[test]
            fn t() {
                G::set("HOME");
                unsafe { std::env::set_var("OTHER", "2") };
            }
            """
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])


class HelperReturnTypeMustBeAGuard(unittest.TestCase):
    """#449 round 6: `->` is not "hands back a guard"."""

    def _src(self, ret, tail):
        return textwrap.dedent(
            f"""\
            fn prepare() {ret} {{
                let l = ENV_TEST_LOCK.lock().unwrap();
                unsafe {{ std::env::set_var("A", "1") }};
                {tail}
            }}

            #[test]
            fn t() {{
                let held = prepare();
                unsafe {{ std::env::set_var("HOME", "/tmp") }};
            }}
            """
        )

    def test_a_bool_return_does_not_protect_the_caller(self):
        found = guard.scan_source(self._src("-> bool", "true"))
        self.assertEqual([f.name for f in found], ["t"])

    def test_a_guard_return_does_protect_the_caller(self):
        src = self._src("-> std::sync::MutexGuard<'static, ()>", "l")
        self.assertEqual(guard.scan_source(src), [])


class HelperLockDiesAtItsLexicalScope(unittest.TestCase):
    """#449 round 6: scope tracking reached `_protected_spans` and not here."""

    def test_helper_locking_in_a_nested_block_does_not_cover_a_later_mutation(self):
        src = textwrap.dedent(
            """\
            fn prepare() {
                {
                    let l = ENV_TEST_LOCK.lock().unwrap();
                    unsafe { std::env::set_var("A", "1") };
                }
                unsafe { std::env::set_var("HOME", "/tmp") };
            }

            #[test]
            fn t() {
                prepare();
            }
            """
        )
        self.assertEqual([f.name for f in guard.scan_source(src)], ["t"])


class HelperChainsPropagate(unittest.TestCase):
    """#449 round 6: a delegating helper has no env call of its own."""

    def test_a_two_hop_chain_is_still_seen(self):
        src = textwrap.dedent(
            """\
            fn inner() {
                unsafe { std::env::set_var("HOME", "/tmp") };
            }

            fn outer() {
                inner();
            }

            #[test]
            fn t() {
                outer();
            }
            """
        )
        found = guard.scan_source(src)
        self.assertEqual([f.name for f in found], ["t"])

    def test_the_chain_carries_the_variable_too(self):
        src = textwrap.dedent(
            """\
            fn inner() { unsafe { std::env::set_var("HOME", "/a") }; }
            fn outer() { inner(); }

            #[test]
            #[serial_test::serial(a)]
            fn one() { outer(); }

            #[test]
            #[serial_test::serial(b)]
            fn two() { unsafe { std::env::set_var("HOME", "/b") }; }
            """
        )
        found = guard.scan_source(src)
        self.assertEqual(sorted(f.name for f in found), ["one", "two"])


class HelperLockNeedsARealAcquisition(unittest.TestCase):
    """#449: `_protected_spans` required an acquisition; the helper path did not."""

    def _src(self, binding):
        return textwrap.dedent(
            f"""\
            fn prepare() {{
                {binding}
                unsafe {{ std::env::set_var("HOME", "/tmp") }};
            }}

            #[test]
            fn t() {{
                prepare();
            }}
            """
        )

    def test_an_env_lock_shaped_identifier_is_not_an_acquisition(self):
        found = guard.scan_source(self._src("let cfg = read_env_lock_setting();"))
        self.assertEqual([f.name for f in found], ["t"])

    def test_a_real_acquisition_still_counts(self):
        self.assertEqual(
            guard.scan_source(self._src("let _l = ENV_TEST_LOCK.lock().unwrap();")), []
        )


class IntegrationRootAndItsModulesAreOneBinary(unittest.TestCase):
    """#449: `tests/root.rs` and `tests/root/child.rs` share a process."""

    def test_root_and_included_module_share_a_group(self):
        root = guard._process_group(Path("crates/codegen/x/tests/root.rs"))
        child = guard._process_group(Path("crates/codegen/x/tests/root/child.rs"))
        self.assertEqual(root, child)

    def test_two_different_roots_do_not_share_a_group(self):
        a = guard._process_group(Path("crates/codegen/x/tests/alpha.rs"))
        b = guard._process_group(Path("crates/codegen/x/tests/beta.rs"))
        self.assertNotEqual(a, b)

    def test_a_tests_module_under_src_is_still_the_lib_binary(self):
        self.assertTrue(
            guard._process_group(Path("crates/codegen/x/src/app/tests/e2e.rs")).startswith("lib:")
        )

if __name__ == "__main__":
    unittest.main()
