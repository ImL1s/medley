"""Tests for the #171 guards in `scripts/`.

The scripts exist because a test that no `ci.yml` filter selects runs nowhere
and still shows green. Shipping them without tests of their own would be the
same joke told twice, so the cases below are the ones where a wrong answer
would be silent: a filter parsed as covering more than it does, an added `fn`
counted as a test when its attribute was already there, and a module-path
filter that cannot match a bare function name.
"""

import subprocess
import sys
import textwrap
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "scripts"))

from check_new_tests_are_filtered import added_tests, crate_of, selected, target_of  # noqa: E402
from check_test_filter_coverage import parse_workflow, uncovered  # noqa: E402


class ParseWorkflow(unittest.TestCase):
    def test_extracts_the_positional_filter_not_the_flag_values(self):
        wf = "          run_nonzero -p xai-grok-shell --lib auth_scheme -- --nocapture\n"
        self.assertEqual(parse_workflow(wf), {"xai-grok-shell": {"lib": {"auth_scheme"}}})

    def test_joins_line_continuations(self):
        """`ci.yml` wraps long invocations, and the filter is on the second line."""
        wf = (
            "          run_nonzero -p xai-grok-sampler --lib \\\n"
            "            hostile_injector -- --nocapture\n"
        )
        self.assertEqual(parse_workflow(wf), {"xai-grok-sampler": {"lib": {"hostile_injector"}}})

    def test_libtest_args_after_the_separator_are_not_filters(self):
        wf = "          run_nonzero -p xai-grok-shell --lib foo -- --skip bar --nocapture\n"
        self.assertEqual(parse_workflow(wf), {"xai-grok-shell": {"lib": {"foo"}}})

    def test_manifest_path_resolves_to_a_crate_name(self):
        wf = (
            "          cargo test --manifest-path crates/codegen/xai-grok-sampler/Cargo.toml \\\n"
            "            --lib none_scheme_ -- --nocapture\n"
        )
        self.assertEqual(parse_workflow(wf), {"xai-grok-sampler": {"lib": {"none_scheme_"}}})

    def test_unfiltered_invocation_covers_everything(self):
        wf = "          run_nonzero -p xai-grok-update --lib -- --nocapture\n"
        self.assertEqual(parse_workflow(wf), {"xai-grok-update": {"lib": {""}}})
        self.assertEqual(uncovered(["anything::at::all"], {""}), [])

    def test_target_scoped_invocation_does_not_blanket_cover_the_lib(self):
        """`--test <target>` restricts to one integration target.

        Treating it as "covers the crate" would silently mark every lib test
        as covered -- the exact false green this whole check exists to stop.
        """
        wf = "          run_nonzero -p xai-grok-update --test test_dist_channel_gate -- --nocapture\n"
        parsed = parse_workflow(wf)
        self.assertEqual(parsed, {"xai-grok-update": {"test:test_dist_channel_gate": {""}}})
        target_filters = parsed.get("xai-grok-update", {})
        filters = target_filters.get("lib", set()) | target_filters.get("*", set())
        self.assertEqual(
            uncovered(["some::lib::test"], filters),
            ["some::lib::test"],
        )

    def test_ignores_shell_variable_filters_and_warns(self):
        wf = "          run_nonzero -p xai-grok-shell --lib $filter -- --nocapture\n"
        import io
        import contextlib
        f = io.StringIO()
        with contextlib.redirect_stderr(f):
            res = parse_workflow(wf)
        self.assertEqual(res, {})
        self.assertIn("warning: ignoring filter token '$filter'", f.getvalue())


class AddedTests(unittest.TestCase):
    def _diff(self, body: str) -> str:
        return "+++ b/crates/codegen/xai-grok-shell/src/a.rs\n" + textwrap.dedent(body)

    def test_finds_test_and_tokio_test(self):
        diff = self._diff(
            """\
            +#[test]
            +fn plain_one() {}
            +#[tokio::test]
            +async fn async_one() {}
            """
        )
        self.assertEqual(
            [fn for _, fn in added_tests(diff)], ["plain_one", "async_one"]
        )

    def test_skips_intervening_attributes_and_doc_comments(self):
        diff = self._diff(
            """\
            +#[test]
            +#[serial]
            +/// why this exists
            +fn still_a_test() {}
            """
        )
        self.assertEqual([fn for _, fn in added_tests(diff)], ["still_a_test"])

    def test_a_fn_whose_attribute_was_not_added_is_not_new(self):
        """A reindent under an existing `#[test]` must not count as a new test."""
        diff = self._diff(
            """\
             #[test]
            +fn moved_but_not_new() {}
            """
        )
        self.assertEqual(added_tests(diff), [])

    def test_a_plain_added_fn_is_not_a_test(self):
        diff = self._diff("+fn just_a_helper() {}\n")
        self.assertEqual(added_tests(diff), [])

    def test_attributes_the_file_they_came_from(self):
        diff = (
            "+++ b/crates/codegen/xai-grok-pager/src/x.rs\n"
            "+#[test]\n+fn one() {}\n"
            "+++ b/crates/codegen/xai-grok-shell/src/y.rs\n"
            "+#[test]\n+fn two() {}\n"
        )
        self.assertEqual(
            added_tests(diff),
            [
                ("crates/codegen/xai-grok-pager/src/x.rs", "one"),
                ("crates/codegen/xai-grok-shell/src/y.rs", "two"),
            ],
        )

    def test_plain_comment_keeps_attribute(self):
        diff = self._diff(
            """\
            +#[test]
            +// note
            +fn with_comment() {}
            """
        )
        self.assertEqual([fn for _, fn in added_tests(diff)], ["with_comment"])



class CrateOf(unittest.TestCase):
    def test_reads_the_crate_directory(self):
        self.assertEqual(
            crate_of("crates/codegen/xai-grok-shell/src/agent/mod.rs"), "xai-grok-shell"
        )

    def test_returns_none_outside_crates(self):
        self.assertIsNone(crate_of("scripts/whatever.py"))


class Selected(unittest.TestCase):
    P = "crates/codegen/xai-grok-pager/src/slash/commands/model.rs"

    def test_substring_of_the_function_name(self):
        self.assertTrue(selected("model_not_ready_reason_catalog_miss", self.P, {"catalog_miss"}))

    def test_unrelated_filter_does_not_match(self):
        self.assertFalse(selected("something_else_entirely", self.P, {"catalog_miss"}))

    def test_module_path_filter_matches_via_the_file_path(self):
        """`slash::commands::model::` cannot match a bare fn name.

        Cargo would select it by module path; from a diff we only have the
        file, so the path is the stand-in. Without this the check would report
        false positives for every module-path filter in `ci.yml`.
        """
        self.assertTrue(selected("anything", self.P, {"slash::commands::model::"}))

    def test_module_path_filter_does_not_match_another_file(self):
        self.assertFalse(
            selected("anything", "crates/codegen/xai-grok-pager/src/views/picker.rs",
                     {"slash::commands::model::"})
        )


class EndToEnd(unittest.TestCase):
    SCRIPT = REPO / "scripts" / "check_new_tests_are_filtered.py"
    WORKFLOW = REPO / ".github" / "workflows" / "ci.yml"

    def _run(self, diff: str) -> subprocess.CompletedProcess:
        import tempfile
        with tempfile.NamedTemporaryFile("w", suffix=".diff", delete=False) as f:
            f.write(diff)
            path = f.name
        return subprocess.run(
            [sys.executable, str(self.SCRIPT), "--workflow", str(self.WORKFLOW),
             "--diff-file", path],
            capture_output=True, text=True,
        )

    def test_exits_nonzero_and_names_an_unselected_test(self):
        proc = self._run(
            "+++ b/crates/codegen/xai-grok-shell/src/agent/mod.rs\n"
            "+#[test]\n+fn a_name_no_filter_selects_zzz() {}\n"
        )
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("a_name_no_filter_selects_zzz", proc.stdout)
        self.assertIn("will not run in CI", proc.stdout)

    def test_exits_zero_when_the_filter_selects_it(self):
        """`auth_scheme` is a real `xai-grok-shell` filter in `ci.yml`."""
        proc = self._run(
            "+++ b/crates/codegen/xai-grok-shell/src/agent/mod.rs\n"
            "+#[test]\n+fn invalid_auth_scheme_is_rejected() {}\n"
        )
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_exits_zero_when_the_diff_adds_no_tests(self):
        proc = self._run(
            "+++ b/crates/codegen/xai-grok-shell/src/agent/mod.rs\n+fn helper() {}\n"
        )
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("no new tests", proc.stdout)

    def test_target_scoped_filter_regression(self):
        # Reaches: main() in check_new_tests_are_filtered.py
        # Mutation: in check_new_tests_are_filtered.py, change:
        #   filters = target_filters.get(t, set()) | target_filters.get("*", set())
        # to:
        #   filters = set().union(*target_filters.values())
        diff = (
            "+++ b/crates/codegen/xai-grok-sampler/src/auth.rs\n"
            "+#[test]\n+fn none_auth_scheme_lib_regression_for_171() {}\n"
        )
        proc = self._run(diff)
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("none_auth_scheme_lib_regression_for_171", proc.stdout)


class TargetOf(unittest.TestCase):
    def test_target_of_paths(self):
        self.assertEqual(target_of("crates/codegen/xai-grok-sampler/src/auth.rs", "xai-grok-sampler"), "lib")
        self.assertEqual(target_of("crates/codegen/xai-grok-update/tests/test_dist_channel_gate.rs", "xai-grok-update"), "test:test_dist_channel_gate")
        self.assertEqual(target_of("crates/codegen/xai-grok-update/tests/test_dist_channel_gate/main.rs", "xai-grok-update"), "test:test_dist_channel_gate")
        self.assertEqual(target_of("crates/codegen/xai-grok-pager-bin/src/main.rs", "xai-grok-pager-bin"), "bin:xai-grok-pager-bin")
        self.assertEqual(target_of("crates/codegen/xai-grok-pager-bin/src/bin/xai-grok-pager.rs", "xai-grok-pager-bin"), "bin:xai-grok-pager")


if __name__ == "__main__":
    unittest.main()
