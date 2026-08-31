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
import subprocess
import sys
import tempfile
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


def cargo_test_names(files: dict[str, str]) -> set[str]:
    """Compile a dependency-free Rust fixture and return Cargo's test list."""

    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        for relative, text in files.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(src(text))
        result = subprocess.run(
            ["cargo", "test", "--quiet", "--", "--list"],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
            timeout=60,
        )
    return {
        line.removesuffix(": test")
        for line in result.stdout.splitlines()
        if line.endswith(": test")
    }


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

    def test_unicode_static_in_contiguous_block_is_registered(self):
        """ASCII-only static names used to end the block before `ÉTAT`
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            static ÉTAT: AtomicU64 = AtomicU64::new(0);
            """
        )
        items, errors = guard.find_registry([(Path("f.rs"), text)])
        self.assertEqual(errors, [])
        self.assertEqual(items[0].identifiers, ("COUNTER", "ÉTAT"))

    def test_unicode_static_touchers_are_members(self):
        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            static ÉTAT: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn first_untagged() {
                ÉTAT.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn second_untagged() {
                ÉTAT.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

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

    def test_let_binding_of_registered_name_is_not_a_touch(self):
        """`let COUNTER = 1;` binds a local; the declaration is not a
        use of the registered static (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn first_clean() {
                let COUNTER = 1;
                let _ = COUNTER;
            }

            #[test]
            fn second_clean() {
                let COUNTER = 1;
                let _ = COUNTER;
            }
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        self.assertEqual(names, set())
        findings, errors, _membership = guard.analyze(
            [(Path("src/lib.rs"), text)], scan_root=Path(".")
        )
        self.assertEqual(errors, [])
        self.assertEqual(findings, [])

    def test_untagged_toucher_is_a_finding(self):
        findings = guard.scan_source(DIRECT_TOUCH)
        names = {f.name for f in findings}
        self.assertIn("untagged_direct_toucher", names)

    def test_cfg_attr_test_is_a_test(self):
        """`#[cfg_attr(test, test)]` is a live test under cargo test
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[cfg_attr(test, test)]
            fn first_untagged() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[cfg_attr(test, test)]
            fn second_untagged() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_composite_cfg_attr_test_is_a_test(self):
        """`#[cfg_attr(all(test, unix), test)]` is a harness test on
        Unix CI (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[cfg_attr(all(test, unix), test)]
            fn first_untagged() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[cfg_attr(all(test, unix), test)]
            fn second_untagged() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        if sys.platform == "win32":
            self.assertEqual(names, set())
        else:
            self.assertEqual(names, {"first_untagged", "second_untagged"})

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

    def test_uppercase_bare_call_is_a_member(self):
        """`Bump()` is a Rust identifier call (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[allow(non_snake_case)]
            fn Bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn first_untagged() {
                Bump();
            }

            #[test]
            fn second_untagged() {
                Bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_uppercase_qualified_call_is_a_member(self):
        """`helper::Bump()` is a module-qualified identifier call
        (#516 review)."""

        sources = [
            (
                Path("src/helper.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[allow(non_snake_case)]
                    pub fn Bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/lib.rs"),
                src(
                    """\
                    mod helper;

                    #[test]
                    fn first_untagged() {
                        helper::Bump();
                    }

                    #[test]
                    fn second_untagged() {
                        helper::Bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_unused_nested_fn_body_is_not_a_direct_touch(self):
        """An unused nested `fn helper() { COUNTER... }` must not mark
        the enclosing test as a Stage-1 toucher (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn clean_with_unused_helper() {
                fn helper() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn also_clean_with_unused_helper() {
                fn helper() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, set())

    def test_called_nested_fn_still_propagates(self):
        """A nested helper that is actually called still reaches the
        enclosing test through the call graph (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn calls_nested_helper_untagged() {
                fn helper() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
                helper();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"calls_nested_helper_untagged"})

    def test_sibling_nested_helpers_do_not_share_keys(self):
        """A touching nested `fn helper` must not taint a sibling test's
        clean nested `helper` (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn dirty_nested_helper_untagged() {
                fn helper() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
                helper();
            }

            #[test]
            fn clean_nested_helper_untagged() {
                fn helper() {}
                helper();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"dirty_nested_helper_untagged"})

    def test_unused_nested_impl_body_is_not_a_direct_touch(self):
        """An unused nested `impl { fn helper() { COUNTER... } }` must
        not mark the enclosing test as a Stage-1 toucher (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn clean_with_unused_impl() {
                struct Local;
                impl Local {
                    fn helper() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, set())

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

    def test_unicode_ident_macro_invocation_is_a_member(self):
        """`make!(prémier)` is a `$name:ident` invocation (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! make {
                ($name:ident) => {
                    #[test]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            make!(prémier);
            make!(deuxième);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertTrue(any(n.startswith("make!") for n in names), names)
        self.assertEqual(len(names), 2, names)

    def test_wrapper_macro_delegates_to_test_generating_macro(self):
        """`wrapper!($name)` -> `make_test!($name)` still registers
        generated tests (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! make_test {
                ($name:ident) => {
                    #[test]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }
            macro_rules! wrapper {
                ($name:ident) => {
                    make_test!($name);
                };
            }

            wrapper!(first_untagged);
            wrapper!(second_untagged);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(len(names), 2, names)
        self.assertTrue(all(n.startswith("wrapper!") for n in names), names)

    def test_function_local_macros_do_not_share_keys(self):
        """Same-named `macro_rules!` in two functions stay distinct
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn dirty_untagged() {
                macro_rules! act {
                    () => {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    };
                }
                act!();
            }

            #[test]
            fn clean_untagged() {
                macro_rules! act {
                    () => {};
                }
                act!();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertIn("dirty_untagged", names)
        self.assertNotIn("clean_untagged", names)

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

    def test_generated_serial_attr_metavars_are_substituted(self):
        """`#[$guard]` with `$guard = serial(demo_key)` holds that key
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                ($guard:meta, $name:ident) => {
                    #[$guard]
                    #[test]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            cases!(serial(demo_key), first);
            cases!(serial(demo_key), second);
            """
        )
        findings, errors, membership = guard.analyze(
            [(Path("src/lib.rs"), text)], scan_root=Path(".")
        )
        self.assertEqual(errors, [])
        self.assertEqual(len(membership["demo_key"]), 2)
        self.assertEqual({f.name for f in findings}, set())

    def test_async_generated_tokio_test_is_a_member(self):
        """`#[tokio::test] async fn $name` still has the test attr
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                ($name:ident) => {
                    #[tokio::test]
                    async fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            cases!(one);
            cases!(two);
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 2, names)

    def test_aliased_test_attr_is_a_harness_member(self):
        """`use tokio::test as async_case; #[async_case]` is a test
        (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    use tokio::test as async_case;

                    #[async_case]
                    async fn first_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[async_case]
                    async fn second_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_aliased_serial_attr_holds_the_key(self):
        """`use serial_test::serial as isolated; #[isolated(demo_key)]`
        still holds the key (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    use serial_test::serial as isolated;

                    #[isolated(demo_key)]
                    #[test]
                    fn first() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[isolated(demo_key)]
                    #[test]
                    fn second() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        findings, errors, _membership = guard.analyze(
            sources, scan_root=Path(".")
        )
        self.assertEqual(errors, [])
        self.assertEqual(findings, [])

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

    def test_repeated_macro_args_with_semicolon_separator_are_distinct(self):
        """`cases!(one; two)` with `$($name:ident);*` is two tests
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                ($($name:ident);*) => {
                    $(
                        #[test]
                        fn $name() {
                            COUNTER.fetch_add(1, Ordering::SeqCst);
                        }
                    )*
                };
            }

            cases!(one; two);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 2, names)
        findings = guard.scan_source(text)
        self.assertGreaterEqual(
            len(findings),
            1,
            "semicolon-separated generated tests must not collapse into a sole-member exemption",
        )

    def test_repetition_embedded_in_a_larger_matcher_still_expands(self):
        """`(prefix; $($name:ident),*)` plus `cases!(prefix; one, two)`
        must still select the arm and count two tests (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                (prefix; $($name:ident),*) => {
                    $(
                        #[test]
                        fn $name() {
                            COUNTER.fetch_add(1, Ordering::SeqCst);
                        }
                    )*
                };
            }

            cases!(prefix; one, two);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 2, names)
        findings = guard.scan_source(text)
        self.assertGreaterEqual(
            len(findings),
            1,
            "embedded $(...)* generated tests must not collapse into a sole-member exemption",
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

    def test_generated_test_path_metavar_is_substituted(self):
        """`$action:path` in a generated body is the invoked function
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn bump() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            macro_rules! case {
                ($name:ident, $action:path) => {
                    #[test]
                    fn $name() {
                        $action();
                    }
                };
            }

            case!(one, bump);
            case!(two, bump);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("case!")}
        self.assertEqual(len(generated), 2, names)
        findings = guard.scan_source(text)
        self.assertGreaterEqual(
            len(findings),
            1,
            "two generated tests that call a toucher must not be invisible",
        )

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

    def test_child_macro_export_is_resolved_at_crate_root(self):
        lib = src(
            """\
            use std::sync::atomic::AtomicU64;

            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            mod exported;

            #[cfg(test)]
            crate::exported_cases!();

            #[cfg(test)]
            mod tests {
                #[test]
                fn first_child_export_untagged() {
                    crate::touch!();
                }

                #[test]
                fn second_child_export_untagged() {
                    crate::touch!();
                }
            }
            """
        )
        exported = src(
            """\
            #[macro_export]
            macro_rules! touch {
                () => { $crate::COUNTER.fetch_add(
                    1, ::std::sync::atomic::Ordering::SeqCst
                ); };
            }

            #[macro_export]
            macro_rules! exported_cases {
                () => {
                    #[test]
                    fn first_exported_case() {
                        $crate::COUNTER.fetch_add(
                            1, ::std::sync::atomic::Ordering::SeqCst
                        );
                    }

                    #[test]
                    fn second_exported_case() {
                        $crate::COUNTER.fetch_add(
                            1, ::std::sync::atomic::Ordering::SeqCst
                        );
                    }
                };
            }
            """
        )
        oracle = cargo_test_names(
            {
                "Cargo.toml": """
                    [package]
                    name = "child-macro-export"
                    version = "0.1.0"
                    edition = "2021"
                """,
                "src/lib.rs": lib,
                "src/exported.rs": exported,
            }
        )
        self.assertEqual(
            oracle,
            {
                "first_exported_case",
                "second_exported_case",
                "tests::first_child_export_untagged",
                "tests::second_child_export_untagged",
            },
        )
        names = derived_names(
            [(Path("src/lib.rs"), lib), (Path("src/exported.rs"), exported)],
            "demo_key",
        )
        self.assertTrue(
            {"first_child_export_untagged", "second_child_export_untagged"}
            <= names,
            names,
        )
        self.assertEqual(
            len({name for name in names if name.startswith("exported_cases!")}),
            2,
            names,
        )

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

    def test_generated_tests_from_a_reexported_macro_are_derived_members(self):
        """Generated-test discovery must use the same re-export graph as
        invocations inside ordinary functions (#516 review)."""

        sources = [
            (
                Path("src/inner.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    macro_rules! cases {
                        () => {
                            #[test]
                            fn first() {
                                COUNTER.fetch_add(1, Ordering::SeqCst);
                            }

                            #[test]
                            fn second() {
                                COUNTER.fetch_add(1, Ordering::SeqCst);
                            }
                        };
                    }
                    pub(crate) use cases;
                    """
                ),
            ),
            (
                Path("src/bridge.rs"),
                src("pub(crate) use crate::inner::cases as relay;\n"),
            ),
            (
                Path("src/lib.rs"),
                src("crate::bridge::relay!();\n"),
            ),
        ]
        names = derived_names(sources, "demo_key")
        generated = {name for name in names if name.startswith("relay!")}
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

    def test_macro_invoke_rejects_number_for_ident_fragment(self):
        """`$kind:ident` must not accept `42`; the literal arm is the
        one Rust selects (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                ($kind:ident, $name:ident) => {
                    #[test]
                    fn $name() {}
                };
                (42, $name:ident) => {
                    #[test]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            cases!(42, one);
            cases!(42, two);
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 2, names)

    def test_macro_invoke_keeps_string_literals_intact(self):
        """`($label:literal, $name:ident)` must match `cases!(\"one\", one)`
        without splitting the string into quote tokens (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                ($label:literal, $name:ident) => {
                    #[test]
                    fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            cases!("one", one);
            cases!("two", two);
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        generated = {n for n in names if n.startswith("cases!")}
        self.assertEqual(len(generated), 2, names)

    def test_macro_invoke_matches_literals_in_repetition_arms(self):
        """`($(clean $name:ident),*)` must not accept `touch one, touch two`
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! cases {
                ($(clean $name:ident),*) => {
                    $(
                        #[test]
                        fn $name() {}
                    )*
                };
                ($(touch $name:ident),*) => {
                    $(
                        #[test]
                        fn $name() {
                            COUNTER.fetch_add(1, Ordering::SeqCst);
                        }
                    )*
                };
            }

            cases!(touch one, touch two);
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

    def test_macro_arm_call_inherits_callee_keys(self):
        """`(touch) => { helper(); }` must inherit `helper`'s keys after
        call-graph closure (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn helper() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            macro_rules! act {
                (touch) => {
                    helper();
                };
                (clean) => {};
            }

            #[test]
            fn first_untagged() {
                act!(touch);
            }

            #[test]
            fn second_untagged() {
                act!(touch);
            }
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_imported_nested_macro_selects_transitive_arm_keys(self):
        """An imported macro invocation inside another selected arm must
        keep the imported macro's arm selection after closure (#516 review)."""

        sources = [
            (
                Path("src/inner.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    fn helper() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    macro_rules! inner {
                        (touch) => { helper(); };
                        (clean) => {};
                    }
                    """
                ),
            ),
            (
                Path("src/outer.rs"),
                src(
                    """\
                    use crate::inner::inner;

                    macro_rules! outer {
                        (touch) => { inner!(touch); };
                        (clean) => { inner!(clean); };
                    }
                    """
                ),
            ),
            (
                Path("src/lib.rs"),
                src(
                    """\
                    use crate::outer::outer;

                    #[test]
                    fn first_clean() {
                        outer!(clean);
                    }

                    #[test]
                    fn second_clean() {
                        outer!(clean);
                    }

                    #[test]
                    fn first_touching() {
                        outer!(touch);
                    }

                    #[test]
                    fn second_touching() {
                        outer!(touch);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_touching", "second_touching"})

    def test_reexported_macro_keeps_selected_arm_keys(self):
        """A macro imported through `pub use` must not fall back to the
        definition's macro-wide dirty-arm union (#516 review)."""

        sources = [
            (
                Path("src/inner.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    macro_rules! inner {
                        (touch) => { COUNTER.fetch_add(1, Ordering::SeqCst); };
                        (clean) => {};
                    }
                    """
                ),
            ),
            (
                Path("src/bridge.rs"),
                src("pub use crate::inner::inner as relay;\n"),
            ),
            (
                Path("src/lib.rs"),
                src(
                    """\
                    use crate::bridge::relay;

                    #[test]
                    fn first_clean() { relay!(clean); }

                    #[test]
                    fn second_clean() { relay!(clean); }

                    #[test]
                    fn first_touching() { relay!(touch); }

                    #[test]
                    fn second_touching() { relay!(touch); }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_touching", "second_touching"})

    def test_qualified_macro_paths_keep_selected_arm_keys(self):
        """Qualified macro calls resolve their module/re-export before arm
        selection instead of falling back to a macro-wide key union."""

        sources = [
            (
                Path("src/inner.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    pub(crate) static COUNTER: AtomicU64 = AtomicU64::new(0);

                    macro_rules! inner {
                        (touch) => { $crate::inner::COUNTER.fetch_add(
                            1, std::sync::atomic::Ordering::SeqCst
                        ); };
                        (clean) => {};
                    }
                    pub(crate) use inner;
                    """
                ),
            ),
            (
                Path("src/bridge.rs"),
                src("pub(crate) use crate::inner::inner as relay;\n"),
            ),
            (
                Path("src/lib.rs"),
                src(
                    """\
                    #[test]
                    fn direct_clean() { crate::inner::inner!(clean); }
                    #[test]
                    fn direct_touch() { crate::inner::inner!(touch); }
                    #[test]
                    fn relay_clean() { crate::bridge::relay!(clean); }
                    #[test]
                    fn relay_touch() { crate::bridge::relay!(touch); }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"direct_touch", "relay_touch"})

    def test_macro_arm_local_glob_keeps_nested_macro_arm_precision(self):
        """A macro arm's lexical glob resolves macros in that module and
        still selects only the invoked nested arm."""

        sources = [
            (
                Path("src/inner.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    pub(crate) static COUNTER: AtomicU64 = AtomicU64::new(0);

                    macro_rules! relay {
                        (touch) => { $crate::inner::COUNTER.fetch_add(
                            1, std::sync::atomic::Ordering::SeqCst
                        ); };
                        (clean) => {};
                    }
                    pub(crate) use relay;
                    """
                ),
            ),
            (
                Path("src/lib.rs"),
                src(
                    """\
                    macro_rules! outer {
                        (touch) => {{ use crate::inner::*; relay!(touch); }};
                        (clean) => {{ use crate::inner::*; relay!(clean); }};
                    }

                    #[test]
                    fn first_clean() { outer!(clean); }
                    #[test]
                    fn second_clean() { outer!(clean); }
                    #[test]
                    fn first_touch() { outer!(touch); }
                    #[test]
                    fn second_touch() { outer!(touch); }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_touch", "second_touch"})

    def test_macro_arm_local_imports_reach_helper_keys(self):
        """Selected macro arms carry their own named and glob imports into
        call-graph closure (#516 review)."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn helper() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/lib.rs"),
                src(
                    """\
                    macro_rules! named {
                        (touch) => {{
                            use crate::a::helper;
                            helper();
                        }};
                    }

                    macro_rules! globbed {
                        (touch) => {{
                            use crate::a::*;
                            helper();
                        }};
                    }

                    #[test]
                    fn first_named() { named!(touch); }

                    #[test]
                    fn second_named() { named!(touch); }

                    #[test]
                    fn first_globbed() { globbed!(touch); }

                    #[test]
                    fn second_globbed() { globbed!(touch); }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(
            names,
            {"first_named", "second_named", "first_globbed", "second_globbed"},
        )

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

    def test_nested_use_does_not_rewrite_outer_same_file_calls(self):
        """Outer `bump()` is the same-file helper; a nested
        `use crate::b::bump` must not steal it (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    fn first_untagged() {
                        bump();
                        {
                            use crate::b::bump;
                        }
                    }

                    #[test]
                    fn second_untagged() {
                        bump();
                        {
                            use crate::b::bump;
                        }
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
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

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

    def test_function_local_import_applies_before_its_declaration(self):
        """Rust `use` items apply to their whole lexical block, including
        calls textually before the declaration (#516 review)."""

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
                    #[test]
                    fn first_untagged() {
                        bump();
                        use crate::a::bump;
                    }

                    #[test]
                    fn second_untagged() {
                        bump();
                        use crate::a::bump;
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_function_local_glob_precedes_same_file_helper_before_declaration(self):
        """A lexical glob applies before its declaration and must resolve
        before a clean same-file helper (#516 review)."""

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
                    fn first_untagged() {
                        bump();
                        use crate::a::*;
                    }

                    #[test]
                    fn second_untagged() {
                        bump();
                        use crate::a::*;
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_nested_function_local_glob_does_not_affect_outer_calls(self):
        """A glob in a nested block must not rewrite calls in the outer
        function block (#516 review)."""

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
                    fn first_clean() {
                        bump();
                        { use crate::a::*; }
                    }

                    #[test]
                    fn second_clean() {
                        bump();
                        { use crate::a::*; }
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, set())

    def test_inner_local_glob_precedes_outer_named_imports(self):
        """An innermost local glob wins over both module-level and outer-block
        named imports of the same function (#516 review)."""

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
            (Path("src/b.rs"), src("pub fn bump() {}\n")),
            (
                Path("src/t.rs"),
                src(
                    """\
                    use crate::b::bump;

                    #[test]
                    fn module_named_untagged() {
                        use crate::a::*;
                        bump();
                    }

                    #[test]
                    fn outer_local_named_untagged() {
                        use crate::b::bump;
                        {
                            use crate::a::*;
                            bump();
                        }
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(
            names, {"module_named_untagged", "outer_local_named_untagged"}
        )

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

    def test_nested_block_static_alias_does_not_escape_its_scope(self):
        """A local alias from a nested block must not turn an outer local
        variable with the same name into a registered-static touch."""

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
                    fn first_clean() {
                        let C = local_counter();
                        C.load(Ordering::SeqCst);
                        { use crate::a::COUNTER as C; }
                    }

                    #[test]
                    fn second_clean() {
                        let C = local_counter();
                        C.load(Ordering::SeqCst);
                        { use crate::a::COUNTER as C; }
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, set())

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

    def test_glob_reexport_resolves_through_the_exporting_module(self):
        """`pub use crate::a::*;` in `b.rs` must make `crate::b::bump()`
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
                    pub use crate::a::*;
                    """
                ),
            ),
            (
                Path("src/c.rs"),
                src(
                    """\
                    #[test]
                    fn calls_glob_reexported_bump_untagged() {
                        crate::b::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_glob_reexported_bump_untagged"})

    def test_reexported_static_import_is_a_direct_touch(self):
        """`pub use crate::a::COUNTER` must make `use crate::b::COUNTER`
        a touch of the registered static (#516 review)."""

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
                    pub use crate::a::COUNTER;
                    """
                ),
            ),
            (
                Path("src/c.rs"),
                src(
                    """\
                    use crate::b::COUNTER;

                    #[test]
                    fn first_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    fn second_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_local_static_imports_follow_reexports_and_aliases(self):
        """Function-local named, aliased, and nested-block imports are
        checked through their re-export source path."""

        sources = [
            (
                Path("src/a.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    pub static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("src/bridge.rs"),
                src("pub use crate::a::COUNTER as EXPORTED;\n"),
            ),
            (
                Path("src/t.rs"),
                src(
                    """\
                    #[test]
                    fn named() {
                        use crate::bridge::EXPORTED;
                        EXPORTED.load(Ordering::SeqCst);
                    }
                    #[test]
                    fn aliased() {
                        use crate::bridge::EXPORTED as LOCAL;
                        LOCAL.load(Ordering::SeqCst);
                    }
                    #[test]
                    fn nested() {
                        { use crate::bridge::EXPORTED as LOCAL; LOCAL.load(Ordering::SeqCst); }
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"named", "aliased", "nested"})

    def test_reexport_search_explores_all_matching_edges_and_cycles(self):
        """A dead/cyclic first edge must not hide a later path to the static."""

        previous = (guard._REEXPORTS, guard._REEXPORT_EXACT, guard._REEXPORT_GLOBS)
        try:
            guard._REEXPORTS = [
                (("bridge",), "EXPORTED", ("dead",), "LOOP"),
                (("bridge",), "EXPORTED", ("a",), "COUNTER"),
                (("dead",), "LOOP", ("bridge",), "EXPORTED"),
            ]
            guard._REEXPORT_EXACT = {
                (("bridge",), "EXPORTED"): [
                    (("dead",), "LOOP"),
                    (("a",), "COUNTER"),
                ],
                (("dead",), "LOOP"): [(("bridge",), "EXPORTED")],
            }
            guard._REEXPORT_GLOBS = {}
            self.assertTrue(
                guard._reexport_reaches(
                    ("bridge",), "EXPORTED", ("a",), ("COUNTER",)
                )
            )
            self.assertFalse(
                guard._reexport_reaches(
                    ("bridge",), "EXPORTED", ("missing",), ("COUNTER",)
                )
            )
        finally:
            (
                guard._REEXPORTS,
                guard._REEXPORT_EXACT,
                guard._REEXPORT_GLOBS,
            ) = previous

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

    def test_use_consumes_every_leading_super(self):
        """`use super::super::a::bump` must not drop the leaf because
        remaining segments start with `super` (#516 review)."""

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
                Path("src/b/c.rs"),
                src(
                    """\
                    use super::super::a::bump;

                    #[test]
                    fn calls_double_super_import_untagged() {
                        bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_double_super_import_untagged"})

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

    def test_cfg_gated_same_name_macros_are_all_candidates(self):
        """A clean cfg twin must not overwrite a touching macro definition."""

        text = src(
            """\
            use std::sync::atomic::{AtomicU64, Ordering};

            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[cfg(unix)]
            macro_rules! act {
                (go) => { COUNTER.fetch_add(1, Ordering::SeqCst); };
            }

            #[cfg(not(unix))]
            macro_rules! act {
                (go) => {};
            }

            #[test]
            fn first_cfg_call_untagged() {
                act!(go);
            }

            #[test]
            fn second_cfg_call_untagged() {
                act!(go);
            }
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        self.assertEqual(
            names, {"first_cfg_call_untagged", "second_cfg_call_untagged"}
        )

    def test_unicode_and_raw_macro_identifiers_reach_registered_state(self):
        sources = [
            (
                Path("src/動作.rs"),
                src(
                    """\
                    use std::sync::atomic::AtomicU64;

                    // SERIAL-GROUP: demo_key
                    pub(crate) static COUNTER: AtomicU64 = AtomicU64::new(0);

                    macro_rules! 觸碰 {
                        () => { $crate::動作::COUNTER.fetch_add(
                            1, ::std::sync::atomic::Ordering::SeqCst
                        ); };
                    }
                    pub(crate) use 觸碰;
                    """
                ),
            ),
            (
                Path("src/lib.rs"),
                src(
                    """\
                    #[path = "動作.rs"]
                    mod 動作;

                    macro_rules! r#type {
                        () => { crate::動作::COUNTER.fetch_add(
                            1, ::std::sync::atomic::Ordering::SeqCst
                        ); };
                    }

                    #[test]
                    fn first_unicode_qualified_untagged() {
                        crate::動作::觸碰!();
                    }

                    #[test]
                    fn second_unicode_qualified_untagged() {
                        crate::動作::觸碰!();
                    }

                    #[test]
                    fn first_raw_untagged() {
                        r#type!();
                    }

                    #[test]
                    fn second_raw_untagged() {
                        r#type!();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(
            names,
            {
                "first_unicode_qualified_untagged",
                "second_unicode_qualified_untagged",
                "first_raw_untagged",
                "second_raw_untagged",
            },
        )

    def test_unicode_path_module_name_can_differ_from_filename(self):
        lib = src(
            """\
            #[path = "impls.rs"]
            mod 動作;

            #[cfg(test)]
            mod tests {
                #[test]
                fn first_unicode_path_untagged() {
                    crate::動作::觸碰!();
                }

                #[test]
                fn second_unicode_path_untagged() {
                    crate::動作::觸碰!();
                }
            }
            """
        )
        impls = src(
            """\
            use std::sync::atomic::AtomicU64;

            // SERIAL-GROUP: demo_key
            pub(crate) static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! 觸碰 {
                () => { $crate::動作::COUNTER.fetch_add(
                    1, ::std::sync::atomic::Ordering::SeqCst
                ); };
            }
            pub(crate) use 觸碰;
            """
        )
        oracle = cargo_test_names(
            {
                "Cargo.toml": """
                    [package]
                    name = "unicode-path-module"
                    version = "0.1.0"
                    edition = "2021"
                """,
                "src/lib.rs": lib,
                "src/impls.rs": impls,
            }
        )
        self.assertEqual(
            oracle,
            {
                "tests::first_unicode_path_untagged",
                "tests::second_unicode_path_untagged",
            },
        )
        self.assertEqual(
            derived_names(
                [(Path("src/lib.rs"), lib), (Path("src/impls.rs"), impls)],
                "demo_key",
            ),
            {"first_unicode_path_untagged", "second_unicode_path_untagged"},
        )

    def test_decomposed_unicode_macro_identifier_is_one_xid_token(self):
        text = src(
            """\
            use std::sync::atomic::AtomicU64;

            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! café {
                () => { crate::COUNTER.fetch_add(
                    1, ::std::sync::atomic::Ordering::SeqCst
                ); };
            }

            #[test]
            fn first_decomposed_xid_untagged() {
                café!();
            }

            #[test]
            fn second_decomposed_xid_untagged() {
                café!();
            }
            """
        )
        oracle = cargo_test_names(
            {
                "Cargo.toml": """
                    [package]
                    name = "decomposed-xid-macro"
                    version = "0.1.0"
                    edition = "2021"
                """,
                "src/lib.rs": text,
            }
        )
        self.assertEqual(
            oracle,
            {"first_decomposed_xid_untagged", "second_decomposed_xid_untagged"},
        )
        self.assertEqual(
            derived_names([(Path("src/lib.rs"), text)], "demo_key"),
            {"first_decomposed_xid_untagged", "second_decomposed_xid_untagged"},
        )

    def test_raw_string_path_attributes_resolve_logical_modules(self):
        lib = src(
            '''\
            use std::sync::atomic::AtomicU64;

            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[path = r"impls.rs"]
            mod raw_logic;
            #[path = r##"hash_impls.rs"##]
            mod hash_logic;

            #[test]
            fn first_raw_path_untagged() {
                crate::raw_logic::touch!();
            }

            #[test]
            fn second_hash_raw_path_untagged() {
                crate::hash_logic::touch!();
            }
            '''
        )
        raw_impl = src(
            """\
            macro_rules! touch {
                () => { crate::COUNTER.fetch_add(
                    1, ::std::sync::atomic::Ordering::SeqCst
                ); };
            }
            pub(crate) use touch;
            """
        )
        hash_impl = raw_impl
        files = {
            "Cargo.toml": """
                [package]
                name = "raw-path-modules"
                version = "0.1.0"
                edition = "2021"
            """,
            "src/lib.rs": lib,
            "src/impls.rs": raw_impl,
            "src/hash_impls.rs": hash_impl,
        }
        self.assertEqual(
            cargo_test_names(files),
            {"first_raw_path_untagged", "second_hash_raw_path_untagged"},
        )
        self.assertEqual(
            derived_names(
                [
                    (Path("src/lib.rs"), lib),
                    (Path("src/impls.rs"), raw_impl),
                    (Path("src/hash_impls.rs"), hash_impl),
                ],
                "demo_key",
            ),
            {"first_raw_path_untagged", "second_hash_raw_path_untagged"},
        )

    def test_qualified_generated_macro_ignores_same_name_bare_import(self):
        lib = src(
            """\
            use std::sync::atomic::AtomicU64;

            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[cfg(test)]
            mod touching;
            #[cfg(test)]
            mod clean;

            #[cfg(test)]
            use crate::touching::cases;

            #[cfg(test)]
            crate::clean::cases!();
            """
        )
        touching = src(
            """\
            macro_rules! cases {
                () => {
                    #[test]
                    fn touching_generated() {
                        crate::COUNTER.fetch_add(
                            1, ::std::sync::atomic::Ordering::SeqCst
                        );
                    }
                };
            }
            pub(crate) use cases;
            """
        )
        clean = src(
            """\
            macro_rules! cases {
                () => {
                    #[test]
                    fn clean_generated() {}
                };
            }
            pub(crate) use cases;
            """
        )
        oracle = cargo_test_names(
            {
                "Cargo.toml": """
                    [package]
                    name = "qualified-generated-macro"
                    version = "0.1.0"
                    edition = "2021"
                """,
                "src/lib.rs": lib,
                "src/touching.rs": touching,
                "src/clean.rs": clean,
            }
        )
        self.assertEqual(oracle, {"clean_generated"})
        self.assertEqual(
            derived_names(
                [
                    (Path("src/lib.rs"), lib),
                    (Path("src/touching.rs"), touching),
                    (Path("src/clean.rs"), clean),
                ],
                "demo_key",
            ),
            set(),
        )

    def test_generated_macro_resolves_qualified_module_alias(self):
        lib = src(
            """\
            use std::sync::atomic::AtomicU64;

            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            mod touching;
            use crate::touching as aliased;
            aliased::cases!();
            """
        )
        touching = src(
            """\
            macro_rules! cases {
                () => {
                    #[test]
                    fn first_alias_generated() {
                        crate::COUNTER.fetch_add(
                            1, ::std::sync::atomic::Ordering::SeqCst
                        );
                    }

                    #[test]
                    fn second_alias_generated() {
                        crate::COUNTER.fetch_add(
                            1, ::std::sync::atomic::Ordering::SeqCst
                        );
                    }
                };
            }
            pub(crate) use cases;
            """
        )
        self.assertEqual(
            cargo_test_names(
                {
                    "Cargo.toml": """
                        [package]
                        name = "qualified-macro-alias"
                        version = "0.1.0"
                        edition = "2021"
                    """,
                    "src/lib.rs": lib,
                    "src/touching.rs": touching,
                }
            ),
            {"first_alias_generated", "second_alias_generated"},
        )
        names = derived_names(
            [(Path("src/lib.rs"), lib), (Path("src/touching.rs"), touching)],
            "demo_key",
        )
        self.assertEqual(
            len({name for name in names if name.startswith("cases!")}), 2, names
        )

    def test_unicode_macro_reexport_reaches_registered_state(self):
        sources = [
            (
                Path("src/inner.rs"),
                src(
                    """\
                    use std::sync::atomic::AtomicU64;

                    // SERIAL-GROUP: demo_key
                    pub(crate) static COUNTER: AtomicU64 = AtomicU64::new(0);

                    macro_rules! 觸碰 {
                        () => { $crate::inner::COUNTER.fetch_add(
                            1, ::std::sync::atomic::Ordering::SeqCst
                        ); };
                    }
                    pub(crate) use 觸碰;
                    """
                ),
            ),
            (
                Path("src/bridge.rs"),
                src("pub(crate) use crate::inner::觸碰 as 轉送;\n"),
            ),
            (
                Path("src/lib.rs"),
                src(
                    """\
                    #[test]
                    fn first_unicode_reexport_untagged() {
                        crate::bridge::轉送!();
                    }

                    #[test]
                    fn second_unicode_reexport_untagged() {
                        crate::bridge::轉送!();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(
            names,
            {
                "first_unicode_reexport_untagged",
                "second_unicode_reexport_untagged",
            },
        )

    def test_absolute_macro_path_reaches_registered_state(self):
        text = src(
            """\
            extern crate self as xai_grok_shell;

            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            mod inner {
                macro_rules! act {
                    () => { crate::COUNTER.fetch_add(
                        1, ::std::sync::atomic::Ordering::SeqCst
                    ); };
                }
                pub(crate) use act;
            }

            #[test]
            fn first_absolute_macro_untagged() {
                ::xai_grok_shell::inner::act!();
            }

            #[test]
            fn second_absolute_macro_untagged() {
                ::xai_grok_shell::inner::act!();
            }
            """
        )
        names = derived_names(
            [(Path("crates/codegen/xai-grok-shell/src/lib.rs"), text)],
            "demo_key",
        )
        self.assertEqual(
            names,
            {
                "first_absolute_macro_untagged",
                "second_absolute_macro_untagged",
            },
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

    def test_angle_bracket_type_assoc_without_as_trait_resolves(self):
        """`<State>::bump()` is a valid associated call with no `as Trait`
        clause (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct State;

            impl State {
                fn bump() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn first_untagged() {
                <State>::bump();
            }

            #[test]
            fn second_untagged() {
                <State>::bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_type_assoc_is_namespaced_by_cargo_target(self):
        """Library `State::bump` must not tag an integration-local
        `State::bump` (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
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
                Path("tests/a.rs"),
                src(
                    """\
                    struct State;

                    impl State {
                        fn bump() {}
                    }

                    #[test]
                    fn local_state_bump_is_clean() {
                        State::bump();
                    }

                    #[test]
                    fn also_clean() {
                        State::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, set())

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

    def test_local_type_alias_assoc_call_resolves(self):
        """`type S = State; S::bump()` must resolve through the alias
        before `by_type` (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct State;

            impl State {
                fn bump() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            type S = State;

            #[test]
            fn first_untagged() {
                S::bump();
            }

            #[test]
            fn second_untagged() {
                S::bump();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_lowercase_type_assoc_call_is_a_touch(self):
        """`struct worker; worker::bump()` is a type-method call
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct worker;

            impl worker {
                fn bump() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            #[test]
            fn first_untagged() {
                worker::bump();
            }

            #[test]
            fn second_untagged() {
                worker::bump();
            }
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_ufcs_trait_call_resolves_like_type_assoc(self):
        """`<Type as Trait>::method(` does not match QUALIFIED_CALL or
        TYPE_ASSOC_CALL; without a dedicated pattern the call never
        derived a key (#516 review)."""

        impl_file = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            pub(super) struct Snapshot(u64);

            pub(super) trait Probe {
                fn now() -> Self;
            }

            impl Probe for Snapshot {
                fn now() -> Self {
                    Self(COUNTER.load(Ordering::SeqCst))
                }
            }
            """
        )
        test_file = src(
            """\
            #[test]
            fn calls_ufcs_trait_untagged() {
                let _s = <inner::Snapshot as inner::Probe>::now();
            }
            """
        )
        sources = [
            (Path("crates/codegen/demo/src/inner.rs"), impl_file),
            (Path("crates/codegen/demo/src/caller.rs"), test_file),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"calls_ufcs_trait_untagged"})

    def test_ufcs_named_trait_does_not_inherit_sibling_impl(self):
        """`<S as Clean>::act()` must not inherit `Touch::act` keys
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct S;

            trait Touch {
                fn act();
            }
            trait Clean {
                fn act();
            }

            impl Touch for S {
                fn act() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }
            impl Clean for S {
                fn act() {}
            }

            #[test]
            fn first_untagged() {
                <S as Clean>::act();
            }

            #[test]
            fn second_untagged() {
                <S as Clean>::act();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, set())

    def test_ufcs_trait_lookup_is_type_and_trait(self):
        """`<Clean as Action>::act()` must not inherit Dirty::act
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            struct Dirty;
            struct Clean;
            trait Action {
                fn act();
            }

            impl Action for Dirty {
                fn act() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }
            impl Action for Clean {
                fn act() {}
            }

            #[test]
            fn first_untagged() {
                <Clean as Action>::act();
            }

            #[test]
            fn second_untagged() {
                <Clean as Action>::act();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, set())

    def test_cfg_disabled_test_is_not_a_member(self):
        """`#[cfg(windows)] #[test]` is not a harness test on Unix
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[cfg(unix)]
            #[test]
            fn unix_untagged() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[cfg(windows)]
            #[test]
            fn windows_untagged() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        if sys.platform == "win32":
            self.assertEqual(names, {"windows_untagged"})
        else:
            self.assertEqual(names, {"unix_untagged"})

    def test_empty_vis_fragment_selects_the_macro_arm(self):
        """`$visibility:vis fn $name` accepts `case!(fn one)` (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            macro_rules! case {
                ($visibility:vis fn $name:ident) => {
                    #[test]
                    $visibility fn $name() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                };
            }

            case!(fn first_untagged);
            case!(fn second_untagged);
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertTrue(any(n.startswith("case!") for n in names), names)
        self.assertEqual(len(names), 2, names)
        findings = guard.scan_source(text)
        self.assertGreaterEqual(
            len(findings),
            1,
            "empty vis must not drop generated untagged touchers",
        )

    def test_cfg_disabled_enclosing_module_is_not_a_member(self):
        """`#[cfg(windows)] mod { #[test] ... }` is off on Unix
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[cfg(windows)]
            mod windows_tests {
                #[test]
                fn first_untagged() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
                #[test]
                fn second_untagged() {
                    COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        if sys.platform == "win32":
            self.assertEqual(names, {"first_untagged", "second_untagged"})
        else:
            self.assertEqual(names, set())

    def test_cfg_disabled_out_of_line_module_is_not_a_member(self):
        """`#[cfg(windows)] mod windows_tests;` is off on Unix
        (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[cfg(windows)]
                    mod windows_tests;

                    #[test]
                    fn only_active() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/windows_tests.rs"),
                src(
                    """\
                    #[test]
                    fn windows_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        if sys.platform == "win32":
            self.assertEqual(names, {"only_active", "windows_untagged"})
        else:
            self.assertEqual(names, {"only_active"})
            findings, errors, _membership = guard.analyze(sources, scan_root=Path("."))
            self.assertEqual(errors, [])
            self.assertEqual(findings, [])

    def test_cfg_disabled_helper_does_not_taint_active_twin(self):
        """`#[cfg(windows)] fn helper()` must not tag unix tests that
        call the clean `#[cfg(unix)]` twin (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[cfg(windows)]
            fn helper() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[cfg(unix)]
            fn helper() {}

            #[test]
            fn first_untagged() {
                helper();
            }

            #[test]
            fn second_untagged() {
                helper();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        if sys.platform == "win32":
            self.assertEqual(names, {"first_untagged", "second_untagged"})
        else:
            self.assertEqual(names, set())

    def test_crate_level_cfg_skips_the_whole_file(self):
        """`#![cfg(windows)]` on an integration file is empty on Linux
        (#516 review)."""

        text = src(
            """\
            #![cfg(windows)]
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn first_untagged() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn second_untagged() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        names = derived_names([(Path("tests/win.rs"), text)], "demo_key")
        if sys.platform == "win32":
            self.assertEqual(names, {"first_untagged", "second_untagged"})
        else:
            self.assertEqual(names, set())

    def test_local_closure_binding_shadows_module_helper(self):
        """`let helper = || {}; helper();` must not inherit the module
        helper's keys (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn helper() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn first_untagged() {
                let helper = || {};
                helper();
            }

            #[test]
            fn second_untagged() {
                let helper = || {};
                helper();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, set())

    def test_block_local_const_does_not_touch_registered_static(self):
        """`const COUNTER` inside a test is not the registered static
        (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn first_untagged() {
                const COUNTER: u64 = 1;
                let _ = COUNTER;
            }

            #[test]
            fn second_untagged() {
                const COUNTER: u64 = 1;
                let _ = COUNTER;
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, set())

    def test_nested_block_let_does_not_shadow_outer_calls(self):
        """`{ let helper = || {}; } helper();` still reaches the module
        function (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            fn helper() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn first_untagged() {
                {
                    let helper = || {};
                    helper();
                }
                helper();
            }

            #[test]
            fn second_untagged() {
                {
                    let helper = || {};
                    helper();
                }
                helper();
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_unicode_qualified_static_path_is_a_touch(self):
        """`xai_grok_shell::動作::COUNTER` is the library static
        (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    pub mod 動作 {
                        // SERIAL-GROUP: demo_key
                        pub static COUNTER: AtomicU64 = AtomicU64::new(0);
                    }
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    #[test]
                    fn first_untagged() {
                        xai_grok_shell::動作::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    fn second_untagged() {
                        xai_grok_shell::動作::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_unicode_identifier_tests_are_members(self):
        """`fn prémier` / `fn deuxième` are Stage-1 touchers (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[test]
            fn prémier() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[test]
            fn deuxième() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        self.assertEqual(names, {"prémier", "deuxième"})

    def test_imported_library_static_counts_in_integration_tests(self):
        """`use xai_grok_shell::COUNTER; COUNTER.load` in tests/ is
        the library static (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    pub static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    use xai_grok_shell::COUNTER;

                    #[test]
                    fn first_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    fn second_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_local_module_use_is_resolved(self):
        """`mod a; use a::bump;` reaches the local module (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            mod a {
                pub fn bump() {
                    super::COUNTER.fetch_add(1, Ordering::SeqCst);
                }
            }

            use a::bump;

            #[test]
            fn first_untagged() {
                bump();
            }

            #[test]
            fn second_untagged() {
                bump();
            }
            """
        )
        names = derived_names([(Path("src/lib.rs"), text)], "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_integration_nested_mod_shares_process_group(self):
        """`mod support;` then `mod nested;` stay in the race binary
        (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    pub static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    mod support;

                    #[test]
                    fn first_untagged() {
                        xai_grok_shell::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("tests/support.rs"),
                src(
                    """\
                    mod nested;
                    """
                ),
            ),
            (
                Path("tests/nested.rs"),
                src(
                    """\
                    #[test]
                    fn second_untagged() {
                        xai_grok_shell::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})
        findings, errors, _membership = guard.analyze(sources, scan_root=Path("."))
        self.assertEqual(errors, [])
        self.assertGreaterEqual(
            len(findings),
            1,
            "nested integration mods must share the race process group",
        )

    def test_undeclared_integration_descendant_is_not_in_binary_group(self):
        """`mod support;` compiles `support/mod.rs`, not `support/orphan.rs`
        (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    pub static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    mod support;

                    #[test]
                    fn only_race() {
                        xai_grok_shell::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("tests/support/mod.rs"),
                src(
                    """\
                    mod nested;
                    """
                ),
            ),
            (
                Path("tests/support/nested.rs"),
                src(
                    """\
                    pub fn helper() {}
                    """
                ),
            ),
            (
                Path("tests/support/orphan.rs"),
                src(
                    """\
                    #[test]
                    fn orphan_untagged() {
                        xai_grok_shell::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        findings, errors, membership = guard.analyze(sources, scan_root=Path("."))
        self.assertEqual(errors, [])
        self.assertEqual(
            findings,
            [],
            "an undeclared tests/support/orphan.rs test must not defeat "
            "the sole-member exemption on tests/race.rs",
        )
        names = {name for _path, _line, name in membership.get("demo_key", [])}
        self.assertIn("only_race", names)

    def test_imported_library_type_assoc_counts_in_integration_tests(self):
        """`use xai_grok_shell::State; State::bump()` in tests/ is
        the library impl (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
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
                Path("tests/race.rs"),
                src(
                    """\
                    use xai_grok_shell::State;

                    #[test]
                    fn first_untagged() {
                        State::bump();
                    }

                    #[test]
                    fn second_untagged() {
                        State::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_target_os_key_value_cfg_is_evaluated(self):
        """`#[cfg(target_os = \"windows\")]` is off on Unix (#516 review)."""

        text = src(
            """\
            // SERIAL-GROUP: demo_key
            static COUNTER: AtomicU64 = AtomicU64::new(0);

            #[cfg(target_os = "windows")]
            #[test]
            fn first_untagged() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }

            #[cfg(target_os = "windows")]
            #[test]
            fn second_untagged() {
                COUNTER.fetch_add(1, Ordering::SeqCst);
            }
            """
        )
        names = derived_names([(Path("f.rs"), text)], "demo_key")
        if sys.platform == "win32":
            self.assertEqual(names, {"first_untagged", "second_untagged"})
        else:
            self.assertEqual(names, set())

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


class DefaultScanRoots(unittest.TestCase):
    def test_default_collect_includes_integration_test_files(self):
        """CI default must include `tests/*.rs` beside `src` (#516 review)."""

        with tempfile.TemporaryDirectory() as d:
            repo = Path(d)
            src = repo / "crates" / "codegen" / "xai-grok-shell" / "src"
            tests = repo / "crates" / "codegen" / "xai-grok-shell" / "tests"
            src.mkdir(parents=True)
            tests.mkdir(parents=True)
            (src / "lib.rs").write_text("fn lib() {}\n", encoding="utf-8")
            (tests / "it.rs").write_text("#[test]\nfn it() {}\n", encoding="utf-8")
            roots = [repo / path for path in guard.DEFAULT_SCAN_ROOTS]
            files = {rel.as_posix() for rel, _text in guard.collect_sources(repo, roots)}
            self.assertIn("crates/codegen/xai-grok-shell/src/lib.rs", files)
            self.assertIn("crates/codegen/xai-grok-shell/tests/it.rs", files)

    def test_integration_test_library_import_reaches_registered_state(self):
        """`use xai_grok_shell::bump` in `tests/*.rs` is the library
        helper, not a same-crate path (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
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
                Path("tests/race.rs"),
                src(
                    """\
                    use xai_grok_shell::bump;

                    #[test]
                    fn first_untagged() {
                        bump();
                    }

                    #[test]
                    fn second_untagged() {
                        bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_integration_library_crate_alias_reaches_registered_state(self):
        """`use xai_grok_shell as shell; shell::bump()` is the library
        helper (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
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
                Path("tests/race.rs"),
                src(
                    """\
                    use xai_grok_shell as shell;

                    #[test]
                    fn first_untagged() {
                        shell::bump();
                    }

                    #[test]
                    fn second_untagged() {
                        shell::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_integration_extern_crate_alias_reaches_registered_state(self):
        """`extern crate xai_grok_shell as shell;` is the library root
        (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
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
                Path("tests/race.rs"),
                src(
                    """\
                    extern crate xai_grok_shell as shell;

                    #[test]
                    fn first_untagged() {
                        shell::bump();
                    }

                    #[test]
                    fn second_untagged() {
                        shell::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_integration_test_library_static_path_is_crate_rooted(self):
        """`xai_grok_shell::a::COUNTER` in `tests/*.rs` is the library
        static, not a path under the integration module (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    pub mod a {
                        // SERIAL-GROUP: demo_key
                        pub static COUNTER: AtomicU64 = AtomicU64::new(0);
                    }
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    #[test]
                    fn first_untagged() {
                        xai_grok_shell::a::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    fn second_untagged() {
                        xai_grok_shell::a::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_integration_local_static_does_not_inherit_library_key(self):
        """Bare COUNTER in `tests/*.rs` is that target's static, not the
        library crate-root static of the same name (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: lib_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    // SERIAL-GROUP: int_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[test]
                    fn first_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    fn second_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        self.assertEqual(derived_names(sources, "lib_key"), set())
        self.assertEqual(
            derived_names(sources, "int_key"),
            {"first_untagged", "second_untagged"},
        )

    def test_integration_support_module_helper_reaches_registered_state(self):
        """`common::helper()` in `tests/*.rs` must resolve helpers under
        `tests/common/` (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
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
                Path("tests/common/mod.rs"),
                src(
                    """\
                    pub fn helper() {
                        xai_grok_shell::bump();
                    }
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    mod common;

                    #[test]
                    fn first_untagged() {
                        common::helper();
                    }

                    #[test]
                    fn second_untagged() {
                        common::helper();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_support_module_tests_share_the_integration_binary(self):
        """Tests in `tests/common/mod.rs` run in each binary that
        `mod common;`s them, not in a fictitious `tests/common` process
        (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
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
                Path("tests/common/mod.rs"),
                src(
                    """\
                    #[test]
                    fn support_toucher() {
                        xai_grok_shell::bump();
                    }
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    mod common;

                    #[test]
                    fn root_toucher() {
                        xai_grok_shell::bump();
                    }
                    """
                ),
            ),
        ]
        findings, errors, _membership = guard.analyze(sources, scan_root=Path("."))
        self.assertEqual(errors, [])
        names = {f.name for f in findings}
        self.assertEqual(names, {"support_toucher", "root_toucher"})

    def test_path_attr_integration_module_shares_the_binary(self):
        """`#[path = \"shared.rs\"] mod support;` in an integration root
        compiles `shared.rs` into that binary (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
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
                Path("tests/shared.rs"),
                src(
                    """\
                    #[test]
                    fn path_toucher() {
                        xai_grok_shell::bump();
                    }
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    #[path = "shared.rs"]
                    mod support;

                    #[test]
                    fn root_toucher() {
                        xai_grok_shell::bump();
                    }
                    """
                ),
            ),
        ]
        findings, errors, _membership = guard.analyze(sources, scan_root=Path("."))
        self.assertEqual(errors, [])
        names = {f.name for f in findings}
        self.assertEqual(names, {"path_toucher", "root_toucher"})

    def test_path_mod_rs_does_not_pull_in_undeclared_orphans(self):
        """`#[path = \"support/mod.rs\"]` compiles declared children,
        not `support/orphan.rs` (#516 review)."""

        sources = [
            (
                Path("tests/race.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[path = "support/mod.rs"]
                    mod support;

                    #[test]
                    fn only_real() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("tests/support/mod.rs"),
                src(
                    """\
                    pub fn helper() {}
                    """
                ),
            ),
            (
                Path("tests/support/orphan.rs"),
                src(
                    """\
                    #[test]
                    fn orphan_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"only_real"})
        findings, errors, _membership = guard.analyze(sources, scan_root=Path("."))
        self.assertEqual(errors, [])
        self.assertEqual(findings, [])

    def test_undeclared_src_file_is_not_collected(self):
        """A `.rs` file not reached from crate roots is not a Cargo
        source and must not join the library process (#516 review)."""

        with tempfile.TemporaryDirectory() as d:
            repo = Path(d)
            src_dir = repo / "src"
            src_dir.mkdir()
            (src_dir / "lib.rs").write_text(
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    pub static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
                encoding="utf-8",
            )
            (src_dir / "orphan.rs").write_text(
                src(
                    """\
                    #[test]
                    fn first_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    #[test]
                    fn second_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
                encoding="utf-8",
            )
            sources = guard.collect_sources(repo, [src_dir])
            self.assertEqual(
                [path.as_posix() for path, _text in sources],
                ["src/lib.rs"],
            )
            findings, errors, _membership = guard.analyze(
                sources, scan_root=src_dir
            )
            self.assertEqual(errors, [])
            self.assertEqual(findings, [])

    def test_src_bin_roots_are_collected(self):
        """`src/bin/*.rs` and `src/bin/*/main.rs` are crate roots
        (#516 review)."""

        with tempfile.TemporaryDirectory() as d:
            repo = Path(d)
            src_dir = repo / "src"
            (src_dir / "bin" / "nested").mkdir(parents=True)
            (src_dir / "lib.rs").write_text("pub fn f() {}\n", encoding="utf-8")
            (src_dir / "bin" / "tool.rs").write_text(
                "fn main() {}\n", encoding="utf-8"
            )
            (src_dir / "bin" / "nested" / "main.rs").write_text(
                "fn main() {}\n", encoding="utf-8"
            )
            (src_dir / "bin" / "nested" / "orphan.rs").write_text(
                "fn unused() {}\n", encoding="utf-8"
            )
            sources = guard.collect_sources(repo, [src_dir])
            self.assertEqual(
                sorted(path.as_posix() for path, _text in sources),
                ["src/bin/nested/main.rs", "src/bin/tool.rs", "src/lib.rs"],
            )

    def test_inline_module_children_use_inline_directory(self):
        """`mod outer { mod nested; }` loads `src/outer/nested.rs`
        (#516 review)."""

        with tempfile.TemporaryDirectory() as d:
            repo = Path(d)
            src_dir = repo / "src"
            (src_dir / "outer").mkdir(parents=True)
            (src_dir / "lib.rs").write_text(
                "mod outer { mod nested; }\n", encoding="utf-8"
            )
            (src_dir / "outer" / "nested.rs").write_text(
                "pub fn x() {}\n", encoding="utf-8"
            )
            (src_dir / "nested.rs").write_text(
                "pub fn sibling() {}\n", encoding="utf-8"
            )
            sources = guard.collect_sources(repo, [src_dir])
            self.assertEqual(
                sorted(path.as_posix() for path, _text in sources),
                ["src/lib.rs", "src/outer/nested.rs"],
            )

    def test_unicode_out_of_line_module_is_collected(self):
        """`mod 動作;` loads `動作.rs` (#516 review)."""

        with tempfile.TemporaryDirectory() as d:
            repo = Path(d)
            src_dir = repo / "src"
            src_dir.mkdir()
            (src_dir / "lib.rs").write_text("mod 動作;\n", encoding="utf-8")
            (src_dir / "動作.rs").write_text("pub fn x() {}\n", encoding="utf-8")
            sources = guard.collect_sources(repo, [src_dir])
            self.assertEqual(
                sorted(path.as_posix() for path, _text in sources),
                ["src/lib.rs", "src/動作.rs"],
            )

    def test_unused_macro_mod_is_not_collected(self):
        """`macro_rules! unused { () => { mod orphan; } }` does not
        compile `orphan.rs` (#516 review)."""

        with tempfile.TemporaryDirectory() as d:
            repo = Path(d)
            src_dir = repo / "src"
            src_dir.mkdir()
            (src_dir / "lib.rs").write_text(
                "macro_rules! unused { () => { mod orphan; } }\n",
                encoding="utf-8",
            )
            (src_dir / "orphan.rs").write_text("pub fn x() {}\n", encoding="utf-8")
            sources = guard.collect_sources(repo, [src_dir])
            self.assertEqual(
                [path.as_posix() for path, _text in sources],
                ["src/lib.rs"],
            )

    def test_file_module_children_live_in_the_stem_directory(self):
        """`mod bar;` in `src/foo.rs` loads `src/foo/bar.rs`, not
        `src/bar.rs` (#516 review)."""

        with tempfile.TemporaryDirectory() as d:
            repo = Path(d)
            src_dir = repo / "src"
            (src_dir / "foo").mkdir(parents=True)
            (src_dir / "lib.rs").write_text("mod foo;\n", encoding="utf-8")
            (src_dir / "foo.rs").write_text("mod bar;\n", encoding="utf-8")
            (src_dir / "foo" / "bar.rs").write_text(
                "pub fn nested() {}\n", encoding="utf-8"
            )
            (src_dir / "bar.rs").write_text("pub fn sibling() {}\n", encoding="utf-8")
            sources = guard.collect_sources(repo, [src_dir])
            self.assertEqual(
                sorted(path.as_posix() for path, _text in sources),
                ["src/foo.rs", "src/foo/bar.rs", "src/lib.rs"],
            )

    def test_same_path_file_keeps_every_module_alias(self):
        """`#[path = \"shared.rs\"] mod support` and `mod common` both
        reach the helper (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    pub static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("tests/shared.rs"),
                src(
                    """\
                    pub fn bump() {
                        xai_grok_shell::COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("tests/a.rs"),
                src(
                    """\
                    #[path = "shared.rs"]
                    mod support;

                    #[test]
                    fn first_untagged() {
                        support::bump();
                    }
                    """
                ),
            ),
            (
                Path("tests/b.rs"),
                src(
                    """\
                    #[path = "shared.rs"]
                    mod common;

                    #[test]
                    fn second_untagged() {
                        common::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_glob_import_of_library_static_is_a_touch(self):
        """`use xai_grok_shell::*; COUNTER` in an integration test is
        the library static (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    pub static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    use xai_grok_shell::*;

                    #[test]
                    fn first_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    fn second_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_cfg_disabled_mod_edge_does_not_join_the_binary(self):
        """`#[cfg(windows)] mod common;` does not put `common.rs` in
        that integration binary on Linux (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    pub static COUNTER: AtomicU64 = AtomicU64::new(0);
                    """
                ),
            ),
            (
                Path("tests/first.rs"),
                src(
                    """\
                    use xai_grok_shell::COUNTER;
                    mod common;

                    #[test]
                    fn first_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("tests/second.rs"),
                src(
                    """\
                    use xai_grok_shell::COUNTER;
                    #[cfg(windows)]
                    mod common;

                    #[test]
                    fn second_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("tests/common.rs"),
                src(
                    """\
                    use xai_grok_shell::COUNTER;

                    #[test]
                    fn common_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        findings, errors, _membership = guard.analyze(sources, scan_root=Path("."))
        self.assertEqual(errors, [])
        found = {f.name for f in findings}
        self.assertIn("first_untagged", found)
        self.assertIn("common_untagged", found)
        if sys.platform == "win32":
            self.assertIn("second_untagged", found)
        else:
            self.assertNotIn("second_untagged", found)

    def test_src_bin_crate_qualified_call_reaches_registered_state(self):
        """`crate::bump()` in `src/bin/tool.rs` is that binary's crate
        root, not `bin::tool` (#516 review)."""

        sources = [
            (
                Path("src/bin/tool.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    fn bump() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    fn first_untagged() {
                        crate::bump();
                    }

                    #[test]
                    fn second_untagged() {
                        crate::bump();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_src_bin_unit_tests_do_not_race_library_unit_tests(self):
        """Each `src/bin` target is its own test process (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[test]
                    fn lib_toucher() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/bin/tool.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[test]
                    fn bin_toucher() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        findings, errors, _membership = guard.analyze(sources, scan_root=Path("."))
        self.assertEqual(errors, [])
        self.assertEqual(findings, [])

    def test_test_const_fn_is_a_harness_member(self):
        """`#[test] const fn` still belongs to the preceding test attr
        (#516 review). `FN_DEF` starts at `fn`, so attributes must be
        collected from before `const`."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[test]
                    const fn first_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[test]
                    const fn second_untagged() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_src_main_unit_tests_do_not_race_library_unit_tests(self):
        """`src/main.rs` is its own test process, not the library
        (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[test]
                    fn lib_toucher() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("src/main.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    #[test]
                    fn main_toucher() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
        ]
        findings, errors, _membership = guard.analyze(sources, scan_root=Path("."))
        self.assertEqual(errors, [])
        self.assertEqual(findings, [])

    def test_aliased_macro_invoke_reaches_registered_state(self):
        """`use crate::act as do_it; do_it!()` is `act!` (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    macro_rules! act {
                        () => {
                            COUNTER.fetch_add(1, Ordering::SeqCst)
                        };
                    }
                    """
                ),
            ),
            (
                Path("src/a.rs"),
                src(
                    """\
                    use crate::act as do_it;

                    #[test]
                    fn first_untagged() {
                        do_it!();
                    }

                    #[test]
                    fn second_untagged() {
                        do_it!();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_path_attribute_module_resolves_super_to_the_declaring_parent(self):
        """`#[path = "managed_tests.rs"] mod tests;` is `managed::tests`,
        not a crate-root module named after the file stem (#516 review)."""

        sources = [
            (
                Path("src/managed.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn helper() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[path = "managed_tests.rs"]
                    mod tests;
                    """
                ),
            ),
            (
                Path("src/managed_tests.rs"),
                src(
                    """\
                    #[test]
                    fn first_untagged() {
                        super::helper();
                    }

                    #[test]
                    fn second_untagged() {
                        super::helper();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_nested_path_attribute_inherits_the_parent_override(self):
        """A `#[path]` file that itself `#[path]`s tests must keep the
        first override's module, not the physical directory (#516 review)."""

        sources = [
            (
                Path("src/managed.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn helper() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }

                    #[path = "impl/mid.rs"]
                    mod mid;
                    """
                ),
            ),
            (
                Path("src/impl/mid.rs"),
                src(
                    """\
                    pub fn relay() {
                        super::helper();
                    }

                    #[path = "mid_tests.rs"]
                    mod tests;
                    """
                ),
            ),
            (
                Path("src/impl/mid_tests.rs"),
                src(
                    """\
                    #[test]
                    fn first_untagged() {
                        super::relay();
                    }

                    #[test]
                    fn second_untagged() {
                        super::relay();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, {"first_untagged", "second_untagged"})

    def test_integration_crate_path_does_not_inherit_library_helpers(self):
        """`crate::helper()` in `tests/*.rs` is that binary, not
        `src/lib.rs` (#516 review)."""

        sources = [
            (
                Path("src/lib.rs"),
                src(
                    """\
                    // SERIAL-GROUP: demo_key
                    static COUNTER: AtomicU64 = AtomicU64::new(0);

                    pub fn helper() {
                        COUNTER.fetch_add(1, Ordering::SeqCst);
                    }
                    """
                ),
            ),
            (
                Path("tests/race.rs"),
                src(
                    """\
                    #[test]
                    fn first_untagged() {
                        crate::helper();
                    }

                    #[test]
                    fn second_untagged() {
                        crate::helper();
                    }
                    """
                ),
            ),
        ]
        names = derived_names(sources, "demo_key")
        self.assertEqual(names, set())


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
