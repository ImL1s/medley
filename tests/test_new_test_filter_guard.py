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

from check_new_tests_are_filtered import (  # noqa: E402
    added_tests,
    crate_of,
    selected,
    target_of,
    targets_of,
)
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

    def test_parses_env_prefixed_run_nonzero_invocations(self):
        wf = (
            '          GROK_TOOLS_BUNDLE_FD_PATH="$PWD/crates/codegen/xai-grok-tools/tests/fd-override-fixture.sh" \\\n'
            "            run_nonzero -p xai-grok-tools --features pi --lib \\\n"
            "              bundled_fd_override_artifact_extracts_exact_and_executes -- --nocapture\n"
        )
        self.assertEqual(
            parse_workflow(wf),
            {
                "xai-grok-tools": {
                    "lib": {"bundled_fd_override_artifact_extracts_exact_and_executes"}
                }
            },
        )

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
    # Every path these assertions classify, so one test can check they all exist.
    REAL_PATHS = (
        "crates/codegen/xai-grok-sampler/src/client.rs",
        "crates/codegen/xai-grok-shell/src/agent/subagent/tests/rest.rs",
        "crates/codegen/xai-grok-pager/src/app/dispatch/tests/session/load.rs",
        "crates/codegen/xai-grok-pager-bin/src/main.rs",
        "crates/codegen/xai-grok-pager/src/bin/mouse_events_playground.rs",
        "crates/codegen/xai-grok-pager/tests/pty_e2e/minimal/minimal_quit_resets_bracketed_paste.rs",
        "crates/codegen/xai-grok-pager/tests/pty_e2e/queue_reorder_moves_row_up.rs",
        "crates/codegen/xai-grok-pager/tests/pty_e2e/common.rs",
    )

    def test_every_classified_path_exists_on_disk(self):
        """A classification asserted against a path that does not exist proves nothing.

        Two assertions here did exactly that and were green throughout: one named
        `pager-bin/src/bin/xai-grok-pager.rs` (the crate has only `main.rs`) and one
        named `acp_session_tests/mod.rs` (that directory is `#[path=...]` includes, not
        a module tree). The second was worse than useless -- its path component is
        `acp_session_tests`, not `tests`, so the *old* classifier returned `lib` for it
        too. It passed before and after the fix it was added to prove, and a reader
        counting three cases had been told about coverage that was not there.

        Nothing noticed, because a wrong path fails no assertion. This does.
        """
        for path in self.REAL_PATHS:
            with self.subTest(path=path):
                self.assertTrue((REPO / path).exists(), f"{path} does not exist")

    def test_target_of_paths(self):
        self.assertEqual(target_of("crates/codegen/xai-grok-sampler/src/client.rs", "xai-grok-sampler"), "lib")
        self.assertEqual(target_of("crates/codegen/xai-grok-update/tests/test_dist_channel_gate.rs", "xai-grok-update"), "test:test_dist_channel_gate")
        self.assertEqual(target_of("crates/codegen/xai-grok-update/tests/test_dist_channel_gate/main.rs", "xai-grok-update"), "test:test_dist_channel_gate")

    def test_a_tests_module_under_src_is_lib_not_an_integration_target(self):
        """`tests` below `src/` is a module named `tests`, not a `tests/` target.

        Matching the component at any depth classified these as `test:<parent>` -- a
        target that does not exist, so no filter could ever match and every test in
        those modules was reported as running nowhere. Three PRs failed the gate on
        this, which is the inverse of the failure the gate exists to catch and just as
        corrosive: false positives teach people to route around it.
        """
        for path in (
            "crates/codegen/xai-grok-shell/src/agent/subagent/tests/rest.rs",
            "crates/codegen/xai-grok-pager/src/app/dispatch/tests/session/load.rs",
        ):
            with self.subTest(path=path):
                self.assertEqual(target_of(path, crate_of(path)), "lib")

    def test_a_tests_subdirectory_without_main_rs_is_not_a_target(self):
        """`tests/pty_e2e/` names no target; its cases compile into nine roots.

        Cargo's integration-test targets are the `.rs` files directly under
        `tests/` (and `tests/<dir>/main.rs`). This repository splits one PTY
        suite across nine roots so the families schedule separately, pulling
        every case out of `tests/pty_e2e/` with `#[path]`.

        Reporting the directory was how `ci.yml` came to carry
        `--test pty_e2e`: `cargo test` errors on that target, so the invocation
        never produced a count, and this classifier agreed with it, so the
        guard passed too. Both readers were wrong in the same direction, which
        is worse than either being wrong alone -- it hid that
        `pty_e2e_queue` was named by nothing at all.
        """
        self.assertEqual(
            target_of(
                "crates/codegen/xai-grok-pager/tests/pty_e2e/minimal/"
                "minimal_quit_resets_bracketed_paste.rs",
                "xai-grok-pager",
            ),
            "test:pty_e2e_minimal",
        )
        self.assertEqual(
            target_of(
                "crates/codegen/xai-grok-pager/tests/pty_e2e/queue_reorder_moves_row_up.rs",
                "xai-grok-pager",
            ),
            "test:pty_e2e_queue",
        )

    def test_a_module_shared_by_several_roots_reports_all_of_them(self):
        """`pty_e2e/common.rs` compiles into every root that declares it.

        A filter naming any one of them runs the file, so reporting a single
        root would call a covered test uncovered whenever `ci.yml` named a
        different sibling.
        """
        targets = targets_of(
            "crates/codegen/xai-grok-pager/tests/pty_e2e/common.rs", "xai-grok-pager"
        )
        self.assertGreater(len(targets), 1, targets)
        self.assertIn("test:pty_e2e_minimal", targets)
        self.assertIn("test:pty_e2e_queue", targets)

    def test_bin_target_name_is_read_from_cargo_toml_not_inferred(self):
        """A `[[bin]]` entry can rename either kind of binary, so neither is inferable.

        `src/main.rs` defaults to the package name -- except the crate here is
        `xai-grok-pager-bin` and the target is `xai-grok-pager`, deliberately, because
        renaming the cargo target would churn every upstream sync (CLAUDE.md).

        `src/bin/foo.rs` autobins to the stem -- except this workspace renames five of
        them, and an underscore-to-hyphen change is enough: the filter map is keyed by
        the cargo name, so the lookup finds nothing and a test CI runs is reported as
        running nowhere.

        The first of these was fixed and the second was left inferring from the stem one
        branch away. Both assert against the real manifests on purpose: a fixture would
        stay green while the checked-in `Cargo.toml` said something else.
        """
        self.assertEqual(
            target_of("crates/codegen/xai-grok-pager-bin/src/main.rs", "xai-grok-pager-bin"),
            "bin:xai-grok-pager",
        )
        self.assertEqual(
            target_of("crates/codegen/xai-grok-pager/src/bin/mouse_events_playground.rs", "xai-grok-pager"),
            "bin:mouse-events-playground",
        )


class CIConcurrencyContract(unittest.TestCase):
    WORKFLOW = REPO / ".github" / "workflows" / "ci.yml"

    def test_push_runs_are_sha_scoped_and_only_non_push_cancels(self):
        workflow = self.WORKFLOW.read_text(encoding="utf-8")
        expected_group = "group: ci-${{ github.event_name == 'push' && github.sha || github.ref }}"
        self.assertTrue(
            expected_group in workflow,
            f"missing push SHA-scoped concurrency group: {expected_group}",
        )
        expected_cancel = "cancel-in-progress: ${{ github.event_name != 'push' }}"
        self.assertTrue(
            expected_cancel in workflow,
            f"missing non-push cancellation policy: {expected_cancel}",
        )


if __name__ == "__main__":
    unittest.main()
