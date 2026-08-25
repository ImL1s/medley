"""Tests for the #496 shared-state serial-group guard.

Its examples are hand-written, and so are its patterns -- one mental model
produces both, so a wrong one produces a matching wrong test. This file
checks the same patterns the guard's own regexes implement, against
synthetic Rust fixtures built independently of the implementation's own
internals -- the same discipline `test_new_test_filter_corpus.py`'s own
docstring names for its sibling guard (#455/#458).
"""

from __future__ import annotations

import importlib.util
import sys
import textwrap
import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO / "scripts" / "check_shared_state_serial.py"


def load_guard():
    spec = importlib.util.spec_from_file_location("check_shared_state_serial", SCRIPT_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {SCRIPT_PATH}")
    module = importlib.util.module_from_spec(spec)
    # dataclass + `from __future__ import annotations` needs the module in
    # sys.modules before exec_module (Python 3.14) -- same fix
    # `test_envguard_serial_guard.py` uses for its own sibling script.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


guard = load_guard()


def src(text: str) -> str:
    return textwrap.dedent(text)


def derived_names(sources: list[tuple[Path, str]], key: str) -> set[str]:
    """Derived membership for `key`, independent of the solitary-process
    exception `findings` applies -- a fixture with exactly one toucher would
    otherwise need a second, unrelated one just to exercise resolution."""

    _findings, errors, membership = guard.analyze(sources, scan_root=Path("."))
    assert not errors, errors
    return {name for _path, _line, name in membership.get(key, [])}


class RegistryDiscovery(unittest.TestCase):
    def test_single_static_is_registered(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(len(items), 1)
        self.assertEqual(items[0].key, "demo_key")
        self.assertEqual(items[0].identifiers, ("COUNTER",))

    def test_contiguous_block_all_claimed(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static A: AtomicU64 = AtomicU64::new(0);
            static B: AtomicBool = AtomicBool::new(false);
            static C: Mutex<Option<PathBuf>> = Mutex::new(None);

            const UNRELATED: &str = "not part of the block";
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items[0].identifiers, ("A", "B", "C"))

    def test_doc_comment_and_attr_between_marker_and_static_are_skipped(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            /// A doc comment sits between the marker and the static.
            #[allow(dead_code)]
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items[0].identifiers, ("COUNTER",))

    def test_marker_naming_no_static_is_a_hard_error(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            const NOT_A_STATIC: u64 = 0;
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(items, [])
        self.assertEqual(len(errors), 1)
        self.assertIn("demo_key", errors[0])
        self.assertIn("f.rs", errors[0])

    def test_marker_at_end_of_file_is_a_hard_error(self):
        text = src("// SERIAL-GROUP: demo_key\n")
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(items, [])
        self.assertEqual(len(errors), 1)

    def test_second_group_after_unrelated_code_is_separate(self):
        text = src(
            """\
            // SERIAL-GROUP: first_key
            static FIRST: AtomicU64 = AtomicU64::new(0);

            fn unrelated() {}

            // SERIAL-GROUP: second_key
            static SECOND: AtomicU64 = AtomicU64::new(0);
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual({i.key: i.identifiers for i in items}, {
            "first_key": ("FIRST",),
            "second_key": ("SECOND",),
        })

    def test_registry_error_short_circuits_analysis(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            const NOT_A_STATIC: u64 = 0;

            #[test]
            fn touches_nothing() {}
            """
        )
        findings, errors, membership = guard.analyze([(Path("f.rs"), text)], scan_root=Path("."))
        self.assertEqual(findings, [])
        self.assertEqual(len(errors), 1)
        self.assertEqual(membership, {})


DIRECT_TOUCH = src(
    """\
    // SERIAL-GROUP: demo_key
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    #[serial(demo_key)]
    fn tagged_direct_toucher() {
        COUNTER.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn untagged_direct_toucher() {
        COUNTER.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn unrelated_test_never_mentions_counter() {
        assert_eq!(1 + 1, 2);
    }
    """
)


class DirectReference(unittest.TestCase):
    def test_tagged_toucher_is_not_a_finding(self):
        findings = guard.scan_source(DIRECT_TOUCH)
        names = {f.name for f in findings}
        self.assertNotIn("tagged_direct_toucher", names)

    def test_untagged_toucher_is_a_finding(self):
        findings = guard.scan_source(DIRECT_TOUCH)
        names = {f.name for f in findings}
        self.assertIn("untagged_direct_toucher", names)

    def test_unrelated_test_is_not_a_finding(self):
        findings = guard.scan_source(DIRECT_TOUCH)
        names = {f.name for f in findings}
        self.assertNotIn("unrelated_test_never_mentions_counter", names)

    def test_finding_names_the_key_and_reason(self):
        findings = guard.scan_source(DIRECT_TOUCH)
        finding = next(f for f in findings if f.name == "untagged_direct_toucher")
        self.assertEqual(finding.key, "demo_key")
        self.assertIn("demo_key", finding.reason)

    def test_comment_and_string_mentions_do_not_count(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn only_mentions_in_comment_and_string() {
                // COUNTER.fetch_add(1, Ordering::SeqCst);
                let _s = "COUNTER";
            }
            """
        )
        findings = guard.scan_source(text)
        self.assertEqual(findings, [])


class TransitiveClosure(unittest.TestCase):
    def test_one_hop_same_file_bare_call(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn calls_bump_untagged() {
                bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_bump_untagged"})

    def test_two_hops_same_file_is_still_derived(self):
        """The real #492 shape: test -> `quarantined_after` -> `heal_unusable`,
        all same-file, neither intermediate call textually mentioning the
        identifier itself."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            fn wrapper() -> bool {
                bump();
                true
            }

            #[test]
            fn calls_wrapper_untagged() {
                assert!(wrapper());
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_wrapper_untagged"})

    def test_five_hops_still_converges_within_max_rounds(self):
        chain = "\n".join(f"fn hop{i}() {{ hop{i + 1}(); }}" for i in range(5))
        text = src(
            f"""\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn hop5() {{
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }}

            {chain}

            #[test]
            fn calls_hop0_untagged() {{
                hop0();
            }}
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_hop0_untagged"})

    def test_function_pointer_as_value_is_not_a_call(self):
        """Measured false-positive risk named in the module docstring: a
        toucher's name passed as a bare argument (never invoked with `()`)
        must not propagate."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            fn wire_up(hook: fn()) -> fn() {
                hook
            }

            #[test]
            fn only_wires_the_pointer_never_calls_it() {
                let _hook = wire_up(bump);
            }
            """
        )
        findings = guard.scan_source(text)
        self.assertEqual(findings, [])

    def test_generic_fn_with_no_space_before_the_bracket_is_still_indexed(self):
        """Regression for a real miss: `fn with_index<R>(` (no space before
        `<`) was silently unparseable -- `_fn_body` tried to skip the
        generic via `_balanced_end`, whose `pairs` dict does not include
        `<`/`>`, so it returned the position unchanged and the following
        `!= "("` check failed, dropping the WHOLE function from the index.
        Found via a dry run against #492's real `with_index<R>`, which
        made `test_malformed_db_file_is_quarantined_and_recreated`
        undetectable -- exactly the test #496 exists to catch, hidden by a
        parser bug rather than a resolution-depth limit."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            pub fn with_index<R>(op: impl Fn() -> R) -> R {
                COUNTER.fetch_add(1, Ordering::SeqCst);
                op()
            }

            #[test]
            fn calls_generic_fn_untagged() {
                with_index(|| 1);
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_generic_fn_untagged"})

    def test_generic_fn_with_trait_bound_arrow_does_not_confuse_the_depth_count(self):
        """The one real ambiguity `_skip_generic_params` guards: a `->`
        arrow inside a trait-bound generic contains a `>` that is not a
        close. Without the guard, `Fn() -> R` inside `<F: Fn() -> R>` would
        prematurely end the generic at its own `>`, then fail the
        following `!= "("` check against the `R` that's left over."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            pub fn with_bound<F: Fn() -> u64>(op: F) -> u64 {
                COUNTER.fetch_add(1, Ordering::SeqCst);
                op()
            }

            #[test]
            fn calls_bound_fn_untagged() {
                with_bound(|| 1);
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_bound_fn_untagged"})


class CrateQualifiedResolution(unittest.TestCase):
    def test_crate_qualified_call_resolves_by_module_path(self):
        toucher_file = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            pub(super) fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        test_file = src(
            """\
            #[test]
            fn calls_crate_qualified_untagged() {
                crate::inner::bump();
            }
            """
        )
        sources = [
            (Path("crates/codegen/demo/src/inner.rs"), toucher_file),
            (Path("crates/codegen/demo/src/caller.rs"), test_file),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_crate_qualified_untagged"})

    def test_leaf_module_call_resolves_without_crate_prefix(self):
        """The real #492 shape: `search_recovery::heal_unusable(...)`, no
        `crate::` prefix at all -- resolved by the callee file's own module
        leaf (its filename stem)."""

        toucher_file = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            pub(super) fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        test_file = src(
            """\
            #[test]
            fn calls_leaf_qualified_untagged() {
                inner::bump();
            }
            """
        )
        sources = [
            (Path("crates/codegen/demo/src/inner.rs"), toucher_file),
            (Path("crates/codegen/demo/src/caller.rs"), test_file),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_leaf_qualified_untagged"})


class TypeAssociatedResolution(unittest.TestCase):
    def test_same_file_type_assoc_call_resolves(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct Snapshot(u64);

            impl Snapshot {
                fn now() -> Self {
                    Self(COUNTER.load(Ordering::SeqCst))
                }
            }

            #[test]
            fn calls_type_assoc_untagged() {
                let _s = Snapshot::now();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_type_assoc_untagged"})

    def test_cross_file_type_assoc_call_resolves_crate_wide(self):
        """The real #492 shape: `search_recovery::CacheEpoch::now()`, called
        from a DIFFERENT file than `CacheEpoch`'s own `impl` block."""

        impl_file = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            pub(super) struct Snapshot(u64);

            impl Snapshot {
                pub(super) fn now() -> Self {
                    Self(COUNTER.load(Ordering::SeqCst))
                }
            }
            """
        )
        test_file = src(
            """\
            #[test]
            fn calls_qualified_type_assoc_untagged() {
                let _s = inner::Snapshot::now();
            }
            """
        )
        sources = [
            (Path("crates/codegen/demo/src/inner.rs"), impl_file),
            (Path("crates/codegen/demo/src/caller.rs"), test_file),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_qualified_type_assoc_untagged"})

    def test_instance_method_call_is_not_resolved(self):
        """Named, measured gap: `.changed()`-shaped instance calls are not
        followed -- there is no cheap way to know a bare variable's type."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct Snapshot(u64);

            impl Snapshot {
                fn changed(&self) -> bool {
                    COUNTER.load(Ordering::SeqCst) != self.0
                }
            }

            #[test]
            fn only_calls_the_instance_method() {
                let snap = Snapshot(0);
                let _ = snap.changed();
            }
            """
        )
        findings = guard.scan_source(text)
        self.assertEqual(findings, [])


SERIAL_KEY_SHAPES = src(
    """\
    // SERIAL-GROUP: demo_key
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    #[serial(demo_key)]
    fn exact_key_clears(){ COUNTER.fetch_add(1, Ordering::SeqCst); }

    #[test]
    #[serial]
    fn unkeyed_serial_does_not_clear(){ COUNTER.fetch_add(1, Ordering::SeqCst); }

    #[test]
    #[serial(other_key)]
    fn wrong_key_does_not_clear(){ COUNTER.fetch_add(1, Ordering::SeqCst); }

    #[test]
    #[serial(other_key, demo_key)]
    fn multi_key_containing_it_clears(){ COUNTER.fetch_add(1, Ordering::SeqCst); }

    #[test]
    fn no_attribute_at_all_does_not_clear(){ COUNTER.fetch_add(1, Ordering::SeqCst); }
    """
)


class SerialKeyMatching(unittest.TestCase):
    def test_exact_key_clears_the_finding(self):
        findings = guard.scan_source(SERIAL_KEY_SHAPES)
        names = {f.name for f in findings}
        self.assertNotIn("exact_key_clears", names)

    def test_unkeyed_serial_is_a_different_lock(self):
        findings = guard.scan_source(SERIAL_KEY_SHAPES)
        finding = next(f for f in findings if f.name == "unkeyed_serial_does_not_clear")
        self.assertIn("unkeyed", finding.reason)
        self.assertIn("DIFFERENT lock", finding.reason)

    def test_wrong_key_is_named_as_held_but_insufficient(self):
        findings = guard.scan_source(SERIAL_KEY_SHAPES)
        finding = next(f for f in findings if f.name == "wrong_key_does_not_clear")
        self.assertIn("other_key", finding.reason)
        self.assertIn("demo_key", finding.reason)

    def test_multi_key_attribute_containing_the_required_key_clears(self):
        findings = guard.scan_source(SERIAL_KEY_SHAPES)
        names = {f.name for f in findings}
        self.assertNotIn("multi_key_containing_it_clears", names)

    def test_no_attribute_is_a_finding(self):
        findings = guard.scan_source(SERIAL_KEY_SHAPES)
        names = {f.name for f in findings}
        self.assertIn("no_attribute_at_all_does_not_clear", names)


class SolitaryProcessException(unittest.TestCase):
    def test_lone_toucher_in_its_process_needs_no_tag(self):
        """Mirrors `check_envguard_serial.py`'s "sole test in its own
        integration binary" regime: nothing else in that process can race a
        test that is the only member of its key there."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn the_only_toucher_in_this_binary() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        findings = guard.scan_source(
            text, path="crates/codegen/demo/tests/solo_integration_test.rs"
        )
        self.assertEqual(findings, [])

    def test_two_touchers_in_the_same_process_both_need_tags(self):
        findings = guard.scan_source(DIRECT_TOUCH, path="crates/codegen/demo/src/lib_tests.rs")
        names = {f.name for f in findings}
        self.assertIn("untagged_direct_toucher", names)


class ReportFormatting(unittest.TestCase):
    def test_format_report_with_no_findings(self):
        report = guard.format_report([], [])
        self.assertIn("0 violation(s)", report)
        self.assertIn("0 registry error(s)", report)

    def test_format_report_lists_each_finding(self):
        findings = guard.scan_source(DIRECT_TOUCH)
        report = guard.format_report(findings, [])
        self.assertIn("untagged_direct_toucher", report)
        self.assertIn("demo_key", report)

    def test_format_report_lists_registry_errors(self):
        report = guard.format_report([], ["f.rs:1: SERIAL-GROUP(x) names no static"])
        self.assertIn("SERIAL-GROUP(x)", report)


class DumpMode(unittest.TestCase):
    def test_dump_lists_identifiers_and_members(self):
        sources = [(Path("f.rs"), DIRECT_TOUCH)]
        _findings, _errors, membership = guard.analyze(sources, scan_root=Path("."))
        output = guard.format_dump(sources, membership)
        self.assertIn("demo_key", output)
        self.assertIn("COUNTER", output)
        self.assertIn("untagged_direct_toucher", output)
        self.assertIn("tagged_direct_toucher", output)


if __name__ == "__main__":
    unittest.main()
