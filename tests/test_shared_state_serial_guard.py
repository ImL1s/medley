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

    def test_multiline_static_initializer_does_not_end_the_block(self):
        # rustfmt wraps `= make();` onto the next line. That continuation is
        # neither STATIC_DECL nor SKIPPABLE_LINE; treating it as the block
        # boundary would drop every later static (#516 review).
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static B: T =
                make();
            static C: T = make();
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items[0].identifiers, ("B", "C"))

    def test_block_comment_between_statics_does_not_end_the_block(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static A: AtomicU64 = AtomicU64::new(0);
            /* a note between the two claimed statics */
            static B: AtomicU64 = AtomicU64::new(0);
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items[0].identifiers, ("A", "B"))

    def test_multiline_block_comment_between_statics_does_not_end_the_block(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static A: AtomicU64 = AtomicU64::new(0);
            /*
             * still the same group
             */
            static B: AtomicU64 = AtomicU64::new(0);
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items[0].identifiers, ("A", "B"))

    def test_multiline_attr_between_statics_does_not_end_the_block(self):
        # rustfmt wraps `#[cfg(any(...))]` across lines. SKIPPABLE_LINE
        # matches `#[` but only the first line; the continuation is neither
        # a static nor skippable, so the block used to end before B
        # (#516 review).
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static A: AtomicU64 = AtomicU64::new(0);
            #[cfg(any(
                unix,
                windows,
            ))]
            static B: AtomicU64 = AtomicU64::new(0);
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items[0].identifiers, ("A", "B"))

    def test_inner_semicolon_in_static_initializer_does_not_end_the_decl(self):
        # `";" not in line` stops on the inner `;` of
        # `LazyLock::new(|| { let x = 1; x })`, dropping every later static
        # (#516 review).
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static A: T = LazyLock::new(|| {
                let x = 1;
                x
            });
            static B: T = make();
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items[0].identifiers, ("A", "B"))

    def test_nested_block_comment_between_statics_does_not_end_the_block(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static A: AtomicU64 = AtomicU64::new(0);
            /* outer
            /* nested */
            still outer */
            static B: AtomicU64 = AtomicU64::new(0);
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items[0].identifiers, ("A", "B"))

    def test_marker_inside_a_raw_string_is_not_a_registry_entry(self):
        text = src(
            '''\
            const SRC: &str = r#"
            // SERIAL-GROUP: fake_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            "#;
            '''
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items, [])

    def test_marker_inside_a_block_comment_is_not_a_registry_entry(self):
        text = src(
            """\
            /*
            // SERIAL-GROUP: fake_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            */
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items, [])

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

    def test_aliased_static_is_a_direct_touch(self):
        """`use super::COUNTER as C` then `C.fetch_add` is still a touch
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            mod inner {
                use super::COUNTER as C;

                fn helper() {
                    C.fetch_add(1, Ordering::SeqCst);
                }

                #[test]
                fn calls_aliased_helper_untagged() {
                    helper();
                }
            }
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        self.assertIn("calls_aliased_helper_untagged", names)


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

    def test_block_comment_between_signature_and_body_is_not_the_body(self):
        # `fn bump() /* { } */ { COUNTER... }` — taking the comment's braces
        # as the body hides the real toucher (#516 review).
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump() /* { } */ {
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

    def test_macro_invocation_reaches_registered_state_in_the_macro_body(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! bump {
                () => {
                    COUNTER.fetch_add(1, Ordering::SeqCst)
                };
            }

            #[test]
            fn calls_bump_macro_untagged() {
                bump!();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_bump_macro_untagged"})

    def test_inline_module_call_reaches_registered_state(self):
        """`inner::relay()` is not a filename leaf (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            mod inner {
                fn relay() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn calls_inner_relay_untagged() {
                inner::relay();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_inner_relay_untagged"})

    def test_macro_generated_tests_are_derived_members(self):
        """`#[test] fn $name()` expansions are not `FN_DEF` identifiers (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! case {
                ($name:ident) => {
                    #[test]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            case!(a);
            case!(b);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertTrue(any(n.startswith("case!") for n in names), names)
        self.assertEqual(len(names), 2, names)

        fixed = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! case {
                () => {
                    #[test]
                    fn generated() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            mod one {
                case!();
            }
            mod two {
                case!();
            }
            """
        )
        names = derived_names([(Path("f.rs"), fixed)], "demo_key")
        self.assertTrue(any(n.startswith("case!") for n in names), names)
        self.assertEqual(len(names), 2, names)

    def test_comment_between_generated_test_attr_and_fn_is_still_a_test(self):
        """`#[test]` then `// note` then `fn $name` must still count
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! case {
                ($name:ident) => {
                    #[test]
                    // rationale
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            case!(a);
            case!(b);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(len(names), 2, names)

    def test_generated_test_string_mention_is_not_a_touch(self):
        """A generated test that only logs `\"COUNTER\"` is not a toucher
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn real_toucher() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            macro_rules! case {
                ($name:ident) => {
                    #[test]
                    fn $name() {
                        let _s = "COUNTER";
                    }
                };
            }

            case!(a);
            case!(b);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"real_toucher"})

    def test_fixed_name_generated_helper_is_expanded(self):
        """A macro-emitted `fn helper()` called as `helper()` still
        transfers keys (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! bundle {
                ($($name:ident),*) => {
                    fn helper() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    $(
                        #[test]
                        fn $name() {
                            helper();
                        }
                    )*
                };
            }

            bundle!(a, b);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("bundle!")}
        self.assertEqual(len(generated), 2, names)

    def test_macro_generated_tests_inherit_helper_keys(self):
        """A generated `#[test] fn $name() { helper(); }` only acquires the
        key after call-graph closure (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            macro_rules! case {
                ($name:ident) => {
                    #[test]
                    fn $name() {
                        bump();
                    }
                };
            }

            case!(a);
            case!(b);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertTrue(any(n.startswith("case!") for n in names), names)
        self.assertEqual(len(names), 2, names)

        nested = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! case {
                ($name:ident, $relay:ident, $leaf:ident) => {
                    fn $leaf() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    fn $relay() {
                        $leaf();
                    }
                    #[test]
                    fn $name() {
                        $relay();
                    }
                };
            }

            case!(a, a_relay, a_leaf);
            case!(b, b_relay, b_leaf);
            """
        )
        names = derived_names([(Path("f.rs"), nested)], "demo_key")
        self.assertTrue(any(n.startswith("case!") for n in names), names)
        self.assertEqual(len(names), 2, names)

    def test_macro_emitted_serial_is_preserved_on_synthetics(self):
        """A correctly tagged generating macro must not be reported missing
        the key it already emits (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! case {
                ($name:ident) => {
                    #[serial(demo_key)]
                    #[test]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            case!(a);
            case!(b);
            """
        )
        findings = guard.scan_source(text)
        self.assertEqual(findings, [])

    def test_macro_sibling_serial_does_not_cover_an_untagged_generated_test(self):
        """Each generated test keeps its own attributes (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! pair {
                ($name:ident) => {
                    #[test]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    #[serial(demo_key)]
                    #[test]
                    fn $name_locked() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            pair!(a);
            pair!(b);
            """
        )
        findings = guard.scan_source(text)
        untagged = [f.name for f in findings]
        self.assertTrue(
            any(n.startswith("pair!") and n.endswith("#0") for n in untagged),
            untagged,
        )
        self.assertFalse(
            any(n.endswith("#1") for n in untagged),
            untagged,
        )

    def test_repeated_macro_args_are_distinct_members_not_sole_exempt(self):
        """`cases!(one, two)` expands twice from one `fn $name` (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                ($($name:ident),*) => {
                    $(
                        #[test]
                        fn $name() {
                            COUNTER.fetch_add(1, Ordering::SeqCst);
                        }
                    )*
                };
            }

            cases!(one, two);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 2, names)
        findings = guard.scan_source(text)
        self.assertGreaterEqual(
            len(findings),
            1,
            "two generated tests must not collapse into a sole-member exemption",
        )

    def test_fixed_arity_macro_is_not_multiplied_by_argument_count(self):
        """`case!(name, expected)` emits one test, not one per argument (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! case {
                ($name:ident, $expected:expr) => {
                    #[test]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                        let _ = $expected;
                    }
                };
            }

            case!(only, 1);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("case!")}
        self.assertEqual(len(generated), 1, names)
        findings = guard.scan_source(text)
        self.assertEqual(findings, [])

    def test_generated_test_keys_are_per_slot_not_macro_union(self):
        """An expansion with one touching test and one unrelated body must
        not count the unrelated test as a toucher (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! pair {
                ($touch:ident, $skip:ident) => {
                    #[test]
                    fn $touch() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    #[test]
                    fn $skip() {}
                };
            }

            pair!(touch, skip);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("pair!")}
        self.assertEqual(len(generated), 1, names)
        findings = guard.scan_source(text)
        self.assertEqual(findings, [])

    def test_bare_call_resolves_inside_the_caller_inline_module(self):
        """File-wide last `fn bump` must not steal an earlier module (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            mod a {
                fn bump() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }

                #[test]
                fn calls_a() {
                    bump();
                }
            }

            mod b {
                fn bump() {}

                #[test]
                fn calls_b() {
                    bump();
                }
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertIn("calls_a", names)
        self.assertNotIn("calls_b", names)

    def test_generated_helper_fn_without_test_attr_is_not_a_member(self):
        """Only `#[test] fn $ident` slots count as generated tests (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn already_a_member() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            macro_rules! wrap {
                ($name:ident) => {
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    #[test]
                    fn $name_test() {
                        $name();
                    }
                };
            }

            wrap!(helper);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("wrap!")}
        self.assertEqual(len(generated), 1, names)

    def test_imported_macro_invocation_reaches_registered_state(self):
        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[macro_export]
                    macro_rules! bump {
                        () => {
                            COUNTER.fetch_add(1, Ordering::SeqCst)
                        };
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    #[test]
                    fn calls_imported_bump_untagged() {
                        bump!();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertIn("calls_imported_bump_untagged", names)

    def test_path_qualified_macro_invocation_reaches_registered_state(self):
        """`crate::bump!()` is a valid invocation; a lookbehind that
        rejects `:` misses it (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[macro_export]
            macro_rules! bump {
                () => {
                    COUNTER.fetch_add(1, Ordering::SeqCst)
                };
            }

            #[test]
            fn calls_crate_qualified_bump_untagged() {
                crate::bump!();
            }
            """
        )
        names = derived_names(
            [(Path("crates/codegen/demo/src/lib.rs"), text)], "demo_key"
        )
        self.assertIn("calls_crate_qualified_bump_untagged", names)

    def test_generated_tests_from_an_imported_macro_are_derived_members(self):
        """A test-generating macro defined in another file is still the
        definition `pending.file` cannot name (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[macro_export]
                    macro_rules! cases {
                        ($name:ident) => {
                            #[test]
                            fn $name() {
                                COUNTER.fetch_add(1, Ordering::SeqCst);
                            }
                        };
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    cases!(first);
                    cases!(second);
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 2, names)

    def test_generated_helpers_stay_in_their_macro_arm(self):
        """A later arm's `fn helper` must not replace an earlier arm's
        touching helper (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! bundle {
                ($name:ident) => {
                    fn helper() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    #[test]
                    fn $name() {
                        helper();
                    }
                };
                ($name:ident, $x:expr) => {
                    fn helper() {}
                    #[test]
                    fn $name() {
                        helper();
                    }
                };
            }

            bundle!(touching);
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("bundle!")}
        self.assertEqual(len(generated), 1, names)

    def test_macro_invoke_uses_only_the_matching_arm(self):
        """`cases!(clean)` must not inherit a sibling `(touch)` arm
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                (touch) => {
                    #[test]
                    fn generated() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
                (clean) => {
                    #[test]
                    fn generated() {}
                };
            }

            cases!(touch);
            cases!(clean);
            cases!(clean);
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 1, names)

    def test_macro_invoke_matches_bracket_delimited_arms(self):
        """`[touch]` / `[clean]` matchers must select like `(touch)`
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                [touch] => {
                    #[test]
                    fn generated() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
                [clean] => {
                    #[test]
                    fn generated() {}
                };
            }

            cases![touch];
            cases![clean];
            cases![clean];
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 1, names)

    def test_macro_invoke_matches_literals_in_metavar_arms(self):
        """`(clean $name:ident)` and `(touch $name:ident)` share arity;
        `cases!(touch first)` must not take the first arm (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                (clean $name:ident) => {
                    #[test]
                    fn $name() {}
                };
                (touch $name:ident) => {
                    #[test]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            cases!(touch first);
            cases!(touch second);
            cases!(clean x);
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 2, names)

    def test_macro_invoke_consumes_expr_fragments(self):
        """`$value:expr` spans `1 + 2`, not one token (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                ($value:expr, touch) => {
                    #[test]
                    fn generated() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                        let _ = $value;
                    }
                };
                ($value:expr, clean) => {
                    #[test]
                    fn generated() {
                        let _ = $value;
                    }
                };
            }

            cases!(1 + 2, touch);
            cases!(3 + 4, touch);
            cases!(0, clean);
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 2, names)

    def test_macro_attr_metavariable_still_generates_tests(self):
        """`#[$attr] fn $name()` invoked with `test` is still a test
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                ($attr:meta, $name:ident) => {
                    #[$attr]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            cases!(test, first);
            cases!(test, second);
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 2, names)

    def test_same_named_macros_in_different_files_are_not_combined(self):
        """Invoking a local `cases!` must not inherit another file's
        touching template of the same name (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    macro_rules! cases {
                        ($name:ident) => {
                            #[test]
                            fn $name() {
                                COUNTER.fetch_add(1, Ordering::SeqCst);
                            }
                        };
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    macro_rules! cases {
                        ($name:ident) => {
                            #[test]
                            fn $name() {}
                        };
                    }
                    cases!(local);
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertTrue(all(not n.startswith("cases!") for n in names), names)

    def test_aliased_unrelated_static_is_not_a_touch(self):
        """`use crate::b::COUNTER as C` is not the registered `a::COUNTER`
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    use crate::b::COUNTER as C;

                    #[test]
                    fn uses_other_counter_untagged() {
                        C.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, set())

    def test_imported_unrelated_static_without_alias_is_not_a_touch(self):
        """`use crate::b::COUNTER; COUNTER.load` is not `a::COUNTER`
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    use crate::b::COUNTER;

                    #[test]
                    fn uses_other_counter_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, set())

    def test_use_without_access_is_not_a_touch(self):
        """A function-local `use crate::a::COUNTER` is not a read
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    #[test]
                    fn only_imports_untagged() {
                        use crate::a::COUNTER;
                    }

                    #[test]
                    fn also_only_imports_untagged() {
                        use crate::a::COUNTER as C;
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, set())

    def test_imported_module_alias_qualified_static_is_a_touch(self):
        """`use crate::a as state; state::COUNTER` reaches the registered
        static (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    use crate::a as state;

                    #[test]
                    fn uses_aliased_module_untagged() {
                        state::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertIn("uses_aliased_module_untagged", names)

    def test_macro_invoke_does_not_inherit_sibling_arm_keys(self):
        """`act!(clean)` must not inherit a sibling `(touch)` arm
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! act {
                (touch) => {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                };
                (clean) => {};
            }

            #[test]
            fn calls_clean_untagged() {
                act!(clean);
            }

            #[test]
            fn real_toucher() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        self.assertIn("real_toucher", names)
        self.assertNotIn("calls_clean_untagged", names)

    def test_qualified_static_in_another_module_is_not_a_touch(self):
        """`crate::b::COUNTER` is not the registered `a::COUNTER`
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    #[test]
                    fn uses_other_module_counter_untagged() {
                        crate::b::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    fn also_uses_other_module_counter_untagged() {
                        crate::b::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, set())

    def test_qualified_static_in_registered_module_is_a_touch(self):
        """`crate::a::COUNTER` still reaches the registered static."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    #[test]
                    fn uses_registered_counter_untagged() {
                        crate::a::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"uses_registered_counter_untagged"})

    def test_super_path_to_inline_registered_static_is_a_touch(self):
        """A static inside `mod tests` is owned by that inline module;
        `super::COUNTER` from a nested module must still match (#516 review)."""

        text = src(
            """\
            mod tests {
                // SERIAL-GROUP: demo_key
                static COUNTER: AtomicU64 = AtomicU64::new(0);

                mod inner {
                    #[test]
                    fn uses_super_counter_untagged() {
                        super::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    fn also_uses_super_counter_untagged() {
                        super::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        self.assertEqual(
            names,
            {
                "uses_super_counter_untagged",
                "also_uses_super_counter_untagged",
            },
        )

    def test_imported_module_alias_qualified_call_reaches_registered_state(self):
        """`use crate::a as h; h::bump()` must resolve through the alias
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    use crate::a as h;

                    #[test]
                    fn calls_aliased_module_untagged() {
                        h::bump();
                    }

                    #[test]
                    fn also_calls_aliased_module_untagged() {
                        h::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(
            names,
            {
                "calls_aliased_module_untagged",
                "also_calls_aliased_module_untagged",
            },
        )

    def test_glob_import_bare_call_reaches_registered_state(self):
        """`use crate::a::*; bump()` must expand the glob (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    use crate::a::*;

                    #[test]
                    fn calls_glob_imported_bump_untagged() {
                        bump();
                    }

                    #[test]
                    fn also_calls_glob_imported_bump_untagged() {
                        bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(
            names,
            {
                "calls_glob_imported_bump_untagged",
                "also_calls_glob_imported_bump_untagged",
            },
        )

    def test_cross_file_call_into_inline_module_reaches_registered_state(self):
        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    mod inner {
                        fn relay() {
                            COUNTER.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    #[test]
                    fn calls_cross_file_inner_untagged() {
                        crate::a::inner::relay();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertIn("calls_cross_file_inner_untagged", names)

    def test_imported_bare_call_reaches_registered_state(self):
        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    use crate::a::bump;

                    #[test]
                    fn calls_imported_bump_untagged() {
                        bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_imported_bump_untagged"})

    def test_function_local_import_does_not_steal_a_sibling(self):
        """A later `use crate::b::bump` inside another test must not
        rewrite an earlier test's `use crate::a::bump` (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    pub fn bump() {}
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    #[test]
                    fn calls_a_untagged() {
                        use crate::a::bump;
                        bump();
                    }

                    #[test]
                    fn calls_b_untagged() {
                        use crate::b::bump;
                        bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_a_untagged"})

    def test_nested_block_use_does_not_shadow_outer_calls(self):
        """Inner `{ use crate::b::bump }` must not rewrite outer `bump()`
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    pub fn bump() {}
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    #[test]
                    fn calls_outer_untagged() {
                        use crate::a::bump;
                        bump();
                        {
                            use crate::b::bump;
                            bump();
                        }
                        bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertIn("calls_outer_untagged", names)

    def test_brace_self_alias_qualified_call_reaches_registered_state(self):
        """`use crate::a::{self as h}; h::bump()` must resolve (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    use crate::a::{self as h};

                    #[test]
                    fn calls_self_alias_untagged() {
                        h::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertIn("calls_self_alias_untagged", names)

    def test_function_local_import_shadows_same_file_fn(self):
        """`use crate::a::bump; bump()` wins over a same-file `fn bump`
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    fn bump() {}

                    #[test]
                    fn calls_imported_bump_untagged() {
                        use crate::a::bump;
                        bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertIn("calls_imported_bump_untagged", names)

    def test_nested_block_static_use_does_not_shadow_outer_access(self):
        """Outer `use crate::a::COUNTER` then inner `use crate::b::COUNTER`
        must keep the outer access (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    #[test]
                    fn uses_outer_counter_untagged() {
                        use crate::a::COUNTER;
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                        {
                            use crate::b::COUNTER;
                            COUNTER.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertIn("uses_outer_counter_untagged", names)

    def test_reexported_function_resolves_through_the_exporting_module(self):
        """`pub use crate::a::bump` in `b.rs` must make `crate::b::bump()`
        a toucher (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    pub use crate::a::bump;
                    """
                ),
            ),
            (
                Path("src/c.rs"),
                src(
                    """\
                    #[test]
                    fn calls_reexported_bump_untagged() {
                        crate::b::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_reexported_bump_untagged"})

    def test_reexport_chain_resolves_through_each_exporter(self):
        """`pub use` of a `pub use` still has to land on the definition
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    pub use crate::a::bump;
                    """
                ),
            ),
            (
                Path("src/c.rs"),
                src(
                    """\
                    pub use crate::b::bump;
                    """
                ),
            ),
            (
                Path("src/d.rs"),
                src(
                    """\
                    #[test]
                    fn calls_chained_reexport_untagged() {
                        crate::c::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_chained_reexport_untagged"})

    def test_raw_identifier_function_is_indexed_and_called(self):
        """`fn r#match` is a real name; capturing only `r` drops the body
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn r#match() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    #[test]
                    fn calls_raw_ident_untagged() {
                        crate::a::r#match();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_raw_ident_untagged"})

    def test_inline_super_import_resolves_from_the_inline_module(self):
        """`mod tests { use super::helpers::bump }` is `a::helpers::bump`,
        not crate-root `helpers::bump` (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    mod helpers {
                        pub fn bump() {
                            COUNTER.fetch_add(1, Ordering::SeqCst);
                        }
                    }

                    mod tests {
                        use super::helpers::bump;

                        #[test]
                        fn calls_super_helpers_untagged() {
                            bump();
                        }
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_super_helpers_untagged"})

    def test_brace_import_with_nested_path_reaches_registered_state(self):
        """`use crate::{a::bump}` must record `bump`, not the `a` prefix
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    use crate::{a::bump};

                    #[test]
                    fn calls_brace_imported_bump_untagged() {
                        bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_brace_imported_bump_untagged"})

    def test_nested_brace_import_tree_reaches_registered_state(self):
        """`use crate::{a::{bump}}` must parse nested braces (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    use crate::{a::{bump}};

                    #[test]
                    fn calls_nested_brace_imported_bump_untagged() {
                        bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_nested_brace_imported_bump_untagged"})

    def test_cfg_gated_same_name_functions_are_all_candidates(self):
        """A later `#[cfg(not(unix))] fn bump` must not hide an earlier
        touching `#[cfg(unix)] fn bump` (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[cfg(unix)]
            fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[cfg(not(unix))]
            fn bump() {}

            #[test]
            fn calls_cfg_unix_untagged() {
                bump();
            }

            #[test]
            fn calls_cfg_not_unix_untagged() {
                bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(
            names, {"calls_cfg_unix_untagged", "calls_cfg_not_unix_untagged"}
        )

    def test_imported_bare_call_is_scoped_to_the_inline_module(self):
        """A later `mod second { use … as bump }` must not steal `mod first`
        (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/unrelated.rs"),
                src(
                    """\
                    pub fn bump() {}
                    """
                ),
            ),
            (
                Path("src/b.rs"),
                src(
                    """\
                    mod first {
                        use crate::a::bump;

                        #[test]
                        fn calls_first_imported_untagged() {
                            bump();
                        }
                    }

                    mod second {
                        use crate::unrelated::bump as bump;

                        #[test]
                        fn calls_second_imported_untagged() {
                            bump();
                        }
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertIn("calls_first_imported_untagged", names)
        self.assertNotIn("calls_second_imported_untagged", names)

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

    def test_call_graph_that_does_not_converge_is_a_hard_error(self):
        # A chain deeper than the bound must not exit as 0 violations
        # (#516 review). Lower the bound so a 5-hop fixture trips it
        # without emitting 65 functions.
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
        original = guard._MAX_ROUNDS
        guard._MAX_ROUNDS = 2
        try:
            _findings, errors, membership = guard.analyze(
                [(Path("f.rs"), text)], scan_root=Path(".")
            )
        finally:
            guard._MAX_ROUNDS = original
        self.assertTrue(errors, errors)
        self.assertTrue(
            any("converge" in e for e in errors),
            errors,
        )
        self.assertEqual(_findings, [])
        self.assertEqual(membership, {})

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

    def test_const_generic_braces_in_return_type_are_not_the_body(self):
        """`fn bump() -> impl Trait<{ 1 }> { COUNTER... }` must index the
        real body, not the const expression (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump() -> impl Trait<{ 1 }> {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn calls_const_generic_return_untagged() {
                bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_const_generic_return_untagged"})

    def test_const_generic_braces_in_impl_head_are_not_the_body(self):
        """`impl Bump<{ 1 }> for S {` must not treat `{ 1 }` as the impl
        body (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct S;
            trait Bump<const N: u32> {
                fn bump();
            }
            impl Bump<{ 1 }> for S {
                fn bump() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn calls_const_generic_impl_untagged() {
                <S as Bump<{ 1 }>>::bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_const_generic_impl_untagged"})


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

    def test_imported_type_alias_assoc_call_resolves(self):
        """`use crate::a::State as Alias; Alias::bump()` must resolve
        through the type alias before `by_type` (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub struct State;

                    impl State {
                        pub fn bump() {
                            COUNTER.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    """
                ),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    use crate::a::State as Alias;

                    #[test]
                    fn calls_aliased_type_untagged() {
                        Alias::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertIn("calls_aliased_type_untagged", names)

    def test_ufcs_trait_call_resolves_like_type_assoc(self):
        """`<Type as Trait>::method(` does not match QUALIFIED_CALL or
        TYPE_ASSOC_CALL; without a dedicated pattern the call never
        derived a key (#516 review)."""

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
            trait Probe {
                fn now() -> Self;
            }

            #[test]
            fn calls_ufcs_trait_untagged() {
                let _s = <inner::Snapshot as Probe>::now();
            }
            """
        )
        sources = [
            (Path("crates/codegen/demo/src/inner.rs"), impl_file),
            (Path("crates/codegen/demo/src/caller.rs"), test_file),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_ufcs_trait_untagged"})

    def test_ufcs_nested_generic_type_still_resolves(self):
        """`<Box<Vec<u8>> as Bump>::bump()` must survive nested `<>`
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct Box<T>(T);
            trait Bump {
                fn bump();
            }
            impl Bump for Box<Vec<u8>> {
                fn bump() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn calls_nested_generic_ufcs_untagged() {
                <Box<Vec<u8>> as Bump>::bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_nested_generic_ufcs_untagged"})

    def test_trait_qualified_call_resolves_to_the_impl(self):
        """`Bump::bump(&S)` looks up the trait name, not the concrete type
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct S;
            trait Bump {
                fn bump(&self);
            }
            impl Bump for S {
                fn bump(&self) {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn calls_trait_qualified_untagged() {
                Bump::bump(&S);
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_trait_qualified_untagged"})

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

    def test_self_qualified_call_resolves_to_the_enclosing_impl_type(self):
        """Codex, round 3 of #501/#496: `Self::bump()` matched
        `TYPE_ASSOC_CALL` syntactically but resolved by a literal lookup on
        the string `"Self"`, which is never a real registered type name --
        the call silently never resolved. `Self` must resolve to the
        calling function's OWN enclosing impl type."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct Snapshot(u64);

            impl Snapshot {
                fn wrapper() -> Self {
                    Self::bump()
                }

                fn bump() -> Self {
                    Snapshot(COUNTER.fetch_add(1, Ordering::SeqCst))
                }
            }

            #[test]
            fn calls_wrapper_which_calls_self_bump_untagged() {
                let _s = Snapshot::wrapper();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_wrapper_which_calls_self_bump_untagged"})

    def test_generic_impl_indexes_under_the_concrete_type_not_the_generic_param(self):
        """Codex, round 3: `impl<T> Box<T> { fn bump() }` indexed the
        method under the trailing generic parameter `T` (the last
        identifier `IDENT.findall` saw in the impl head), not the actual
        type `Box` -- so `Box::<u64>::bump()` (or even plain
        `Box::bump()`) never matched any registered type."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct Box<T> {
                value: T,
            }

            impl<T> Box<T> {
                fn bump() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn calls_generic_impl_method_untagged() {
                Box::<u64>::bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_generic_impl_method_untagged"})

    def test_generic_trait_impl_indexes_the_implementing_type_after_for(self):
        """The harder shape the same fix must not regress: a generic
        TRAIT impl (`impl<T> Trait<T> for Box<T>`), where the type of
        interest is the one after `for`, not the trait name, and both
        carry their own generic arguments to skip past."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct Box<T> {
                value: T,
            }

            trait Bumper<T> {
                fn bump();
            }

            impl<T> Bumper<T> for Box<T> {
                fn bump() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn calls_generic_trait_impl_method_untagged() {
                Box::<u64>::bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_generic_trait_impl_method_untagged"})

    def test_impl_for_reference_type_is_a_named_residual_not_a_crash(self):
        """`impl Trait for &Foo { .. }` -- a non-path impl type this
        design does not parse. Must be skipped cleanly (no crash, no
        misattribution), not resolved."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct Foo;
            trait Bumper {
                fn bump();
            }

            impl Bumper for &Foo {
                fn bump() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn calls_ref_impl_method_untagged() {
                Foo.bump();
            }
            """
        )
        # Not asserting the derived set here -- only that analysis does not
        # raise. `Foo.bump()` is an instance-method call anyway (the
        # already-named `.changed()`-shaped gap above), so this fixture
        # would not resolve even with a working impl-type parser; it exists
        # to pin "skips cleanly" against the non-path `for &Foo` shape.
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, set())


class RelativeQualifierResolution(unittest.TestCase):
    """Codex, round 3 of #501/#496: `self::`/`super::` matched
    `QUALIFIED_CALL` syntactically but resolved as an ordinary module leaf
    -- `by_leaf.get("self", ...)` / `by_leaf.get("super", ...)` -- which no
    real module is ever named, so both common relative-call forms silently
    never resolved."""

    def test_self_path_call_resolves_same_file(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn calls_self_qualified_untagged() {
                self::bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_self_qualified_untagged"})

    def test_super_path_call_resolves_from_an_inline_test_module(self):
        """The dominant real shape (heap_profile/monitor.rs's own
        `mod tests { use super::*; ... }`): the test's OWN file is
        `_module_path`'s unit of resolution, so `super::` from inside an
        inline `mod tests` block must resolve against the SAME file the
        toucher lives in, not one directory up. A fix that only widens
        `caller_module[:-1]` (the top-level-sibling case below) would
        resolve this to the wrong, one-level-too-high module and still
        miss the shape tests actually use."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            mod tests {
                use super::*;

                #[test]
                fn calls_super_qualified_from_inline_mod_untagged() {
                    super::bump();
                }
            }
            """
        )
        names = derived_names([(Path("crates/codegen/demo/src/inner.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_super_qualified_from_inline_mod_untagged"})

    def test_super_path_call_resolves_from_a_top_level_sibling_file(self):
        """The other real shape (heap_profile/monitor.rs's own top-level
        `super::dump_to_path(..)`, called from CODE, not from `mod tests`):
        a file one directory down calling something in its PARENT
        directory's module via `super::`."""

        parent_file = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            pub(crate) fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        child_file = src(
            """\
            #[test]
            fn calls_super_qualified_from_child_dir_untagged() {
                super::bump();
            }
            """
        )
        sources = [
            (Path("crates/codegen/demo/src/parent.rs"), parent_file),
            (Path("crates/codegen/demo/src/parent/child.rs"), child_file),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_super_qualified_from_child_dir_untagged"})

    def test_double_super_from_doubly_nested_inline_mod_resolves(self):
        """Measured, not assumed: an earlier version of this checker only
        handled a single leading `super`/`self`, on the theory that deeper
        chains were rare enough to name as a residual instead of fixing.
        A real-tree count found otherwise -- `super::super::name(...)`
        (two `super`s, no trailing module) occurs 15 times in
        `xai-grok-shell`. This fixture is that shape: two levels of
        INLINE nesting (`mod inner { mod tests { ... } }`), where
        `super::super` correctly means "back to the top of this same
        file" -- which is exactly where `bump` lives, so this resolves
        via the ascent=0 (same-file) case the general algorithm tries."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            mod inner {
                mod tests {
                    #[test]
                    fn calls_double_super_untagged() {
                        super::super::bump();
                    }
                }
            }
            """
        )
        names = derived_names(
            [(Path("crates/codegen/demo/src/parent/child.rs"), text)], "demo_key"
        )
        self.assertEqual(names, {"calls_double_super_untagged"})

    def test_super_then_a_named_sibling_module_resolves(self):
        """The single most common real relative-qualifier shape in this
        crate: `super::sibling_mod::name(...)` -- a leading `super` (go up
        one level) followed by a NAMED module (not another `super`), then
        the call. Measured at 129 occurrences in `xai-grok-shell`
        (`super::persist::update_config(...)`,
        `super::campaigns::persist_models_default(...)`, and similar). The
        earlier single-segment-only fix (bare `super::name(...)`, nothing
        after it) did not cover this at all."""

        parent_file = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            """
        )
        sibling_file = src(
            """\
            pub(super) fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        child_file = src(
            """\
            #[test]
            fn calls_super_then_sibling_module_untagged() {
                super::sibling::bump();
            }
            """
        )
        sources = [
            (Path("crates/codegen/demo/src/parent.rs"), parent_file),
            (Path("crates/codegen/demo/src/parent/sibling.rs"), sibling_file),
            (Path("crates/codegen/demo/src/parent/child.rs"), child_file),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_super_then_sibling_module_untagged"})


class TurbofishCallResolution(unittest.TestCase):
    """Codex, round 3: `bump::<u64>()` requires `(` immediately after the
    identifier for `FREE_CALL` (and the equivalent for `QUALIFIED_CALL` /
    `TYPE_ASSOC_CALL`); an explicit turbofish sits between the name and the
    `(` and broke every one of them."""

    def test_turbofish_free_call_is_still_resolved(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump<T>() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn calls_turbofish_untagged() {
                bump::<u64>();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_turbofish_untagged"})

    def test_turbofish_with_nested_generic_argument_is_still_resolved(self):
        """The one real ambiguity a naive `::<[^>]*>` strip would get
        wrong: a nested generic (`Vec<u8>`) inside the turbofish has its
        own `>` that is not the turbofish's own close."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump<T>() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn calls_nested_turbofish_untagged() {
                bump::<Vec<u8>>();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_nested_turbofish_untagged"})

    def test_type_associated_turbofish_call_is_still_resolved(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct Box<T> {
                value: T,
            }

            impl<T> Box<T> {
                fn bump() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn calls_type_assoc_turbofish_untagged() {
                Box::<u64>::bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_type_assoc_turbofish_untagged"})


class CrateRootFiles(unittest.TestCase):
    """Codex, round 3, P2: a registered toucher declared directly in
    `src/lib.rs` or `src/main.rs` crashed the whole checker.
    `_module_path` correctly returns `()` (the crate root IS a real,
    meaningful module -- reachable via `crate::name()`), but the indexing
    loop then did `module[-1]` unconditionally to populate `by_leaf`,
    raising `IndexError` on the empty tuple. Only `by_leaf` has no
    meaningful value for the crate root (no sibling ever calls a
    crate-root function as `lib::name()` or `main::name()`); `by_module`
    indexing must still happen."""

    def test_function_in_crate_root_file_does_not_crash_the_indexer(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            pub fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn calls_crate_root_fn_untagged() {
                bump();
            }
            """
        )
        # Must not raise. The path has a real `src` component so it
        # reaches `_module_path`'s crate-root branch -- unlike this file's
        # other fixtures' bare `Path("f.rs")`, which has no `src`
        # component and never exercises this code path at all.
        names = derived_names([(Path("crates/codegen/demo/src/lib.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_crate_root_fn_untagged"})

    def test_crate_qualified_call_to_a_crate_root_function_still_resolves(self):
        """`by_module` indexing for the crate root (module path `()`)
        must still happen even though `by_leaf` is skipped -- a sibling
        file calling `crate::bump()` is the realistic way a crate-root
        function is reached from elsewhere."""

        root_file = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            pub fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        test_file = src(
            """\
            #[test]
            fn calls_crate_qualified_root_fn_untagged() {
                crate::bump();
            }
            """
        )
        sources = [
            (Path("crates/codegen/demo/src/lib.rs"), root_file),
            (Path("crates/codegen/demo/src/caller.rs"), test_file),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_crate_qualified_root_fn_untagged"})


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
