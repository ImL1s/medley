from __future__ import annotations

import importlib.util
import io
import json
import subprocess
import unittest
from pathlib import Path
from unittest import mock


REPO = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO / "scripts" / "check_pr_head_ci_run.py"


def load_guard():
    spec = importlib.util.spec_from_file_location("pr_head_ci_run_guard", SCRIPT_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {SCRIPT_PATH}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


guard = load_guard()


def completed(
    command: list[str],
    *,
    stdout: str = "",
    stderr: str = "",
    returncode: int = 0,
) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(command, returncode, stdout=stdout, stderr=stderr)


PROVIDERS_SHA = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"


def listed_run(
    run_id: int,
    sha: str,
    *,
    status: str = "completed",
    conclusion: str | None = "success",
    event: str = "push",
) -> dict:
    return {
        "databaseId": run_id,
        "headSha": sha,
        "status": status,
        "conclusion": conclusion,
        "url": f"https://github.com/ImL1s/medley/actions/runs/{run_id}",
        "displayTitle": "CI",
        "event": event,
    }


def fake_push_list(runs: list[dict]):
    def fake_run(command, **kwargs):
        if command[:3] == ["gh", "pr", "view"]:
            raise AssertionError("direct branch mode must not query PR metadata")
        if command[:2] == ["git", "ls-remote"]:
            raise AssertionError("explicit head-sha mode must not call ls-remote")
        if command[:3] == ["gh", "run", "list"]:
            if "--event" in command:
                assert command[command.index("--event") + 1] == "push"
            if "--workflow" in command:
                assert command[command.index("--workflow") + 1] == "ci.yml"
            return completed(command, stdout=json.dumps(runs))
        if command[:2] == ["gh", "api"]:
            raise AssertionError(
                f"absent/in-progress/skipped list rows must not fetch run detail: {command}"
            )
        raise AssertionError(f"unexpected command: {command}")

    return fake_run


class PrHeadCiRunGuardTests(unittest.TestCase):
    def test_feature_branch_passes_when_run_head_sha_matches_pr_head_and_pull_requests_is_empty(
        self,
    ) -> None:
        branch = "fix/54-switch-transaction-boundaries"
        pr_head_sha = "960d816ecb19a52bdea9288b7bf66c6095276036"
        commands: list[list[str]] = []

        def fake_run(command, **kwargs):
            commands.append(command)
            if command[:3] == ["gh", "pr", "view"]:
                return completed(
                    command,
                    stdout=json.dumps(
                        {
                            "headRefName": branch,
                            "headRefOid": pr_head_sha,
                            "url": "https://github.com/ImL1s/medley/pull/228",
                        }
                    ),
                )
            if command[:2] == ["git", "ls-remote"]:
                return completed(command, stdout=f"{pr_head_sha}\trefs/heads/{branch}\n")
            if command[:3] == ["gh", "run", "list"]:
                self.assertEqual(command[command.index("--event") + 1], "pull_request")
                return completed(
                    command,
                    stdout=json.dumps(
                        [
                            {
                                "databaseId": 31182413606,
                                "headSha": pr_head_sha,
                                "status": "completed",
                                "conclusion": "success",
                                "url": "https://github.com/ImL1s/medley/actions/runs/31182413606",
                                "displayTitle": "CI",
                            }
                        ]
                    ),
                )
            if command[:2] == ["gh", "api"]:
                self.assertEqual(command[2], "repos/ImL1s/medley/actions/runs/31182413606")
                return completed(
                    command,
                    stdout=json.dumps(
                        {
                            "head_sha": pr_head_sha,
                            "event": "pull_request",
                            "head_branch": branch,
                            "path": ".github/workflows/ci.yml",
                            "status": "completed",
                            "conclusion": "success",
                            "pull_requests": [],
                        }
                    ),
                )
            raise AssertionError(f"unexpected command: {command}")

        with mock.patch.object(guard.subprocess, "run", side_effect=fake_run):
            out = io.StringIO()
            rc = guard.check_pr_head_ci(
                repo="ImL1s/medley",
                pr_number=228,
                remote="origin",
                limit=50,
                stream=out,
            )

        self.assertEqual(rc, 0, out.getvalue())
        run_list = next(cmd for cmd in commands if cmd[:3] == ["gh", "run", "list"])
        self.assertEqual(run_list[run_list.index("--event") + 1], "pull_request")

    def test_feature_branch_uses_pull_request_probe_and_passes_for_pr_head(self) -> None:
        branch = "fix/54-switch-transaction-boundaries"
        pr_head_sha = "960d816ecb19a52bdea9288b7bf66c6095276036"
        merge_sha = "e9eca323281b192e3a14f670c490ec9fb8d64a8f"
        commands: list[list[str]] = []

        def fake_run(command, **kwargs):
            commands.append(command)
            if command[:3] == ["gh", "pr", "view"]:
                return completed(
                    command,
                    stdout=json.dumps(
                        {
                            "headRefName": branch,
                            "headRefOid": pr_head_sha,
                            "url": "https://github.com/ImL1s/medley/pull/202",
                        }
                    ),
                )
            if command[:2] == ["git", "ls-remote"]:
                return completed(command, stdout=f"{pr_head_sha}\trefs/heads/{branch}\n")
            if command[:3] == ["gh", "run", "list"]:
                self.assertEqual(command[command.index("--event") + 1], "pull_request")
                return completed(
                    command,
                    stdout=json.dumps(
                        [
                            {
                                "databaseId": 31178074734,
                                "headSha": merge_sha,
                                "status": "completed",
                                "conclusion": "success",
                                "url": "https://github.com/ImL1s/medley/actions/runs/31178074734",
                                "displayTitle": "CI",
                            }
                        ]
                    ),
                )
            if command[:2] == ["gh", "api"]:
                self.assertEqual(command[2], "repos/ImL1s/medley/actions/runs/31178074734")
                return completed(
                    command,
                    stdout=json.dumps(
                        {
                            "head_sha": merge_sha,
                            "event": "pull_request",
                            "head_branch": branch,
                            "path": ".github/workflows/ci.yml",
                            "status": "completed",
                            "conclusion": "success",
                            "pull_requests": [
                                {
                                    "head": {
                                        "ref": branch,
                                        "sha": pr_head_sha,
                                    }
                                }
                            ],
                        }
                    ),
                )
            raise AssertionError(f"unexpected command: {command}")

        with mock.patch.object(guard.subprocess, "run", side_effect=fake_run):
            out = io.StringIO()
            rc = guard.check_pr_head_ci(
                repo="ImL1s/medley",
                pr_number=202,
                remote="origin",
                limit=50,
                stream=out,
            )

        self.assertEqual(rc, 0, out.getvalue())
        run_list = next(cmd for cmd in commands if cmd[:3] == ["gh", "run", "list"])
        self.assertIn("--workflow", run_list)
        self.assertEqual(run_list[run_list.index("--workflow") + 1], "ci.yml")
        self.assertIn("--branch", run_list)
        self.assertEqual(run_list[run_list.index("--branch") + 1], branch)
        self.assertIn("--event", run_list)
        self.assertEqual(run_list[run_list.index("--event") + 1], "pull_request")
        self.assertFalse(any(cmd[:3] == ["gh", "pr", "checks"] for cmd in commands))

    def test_providers_branch_uses_push_probe_and_passes_for_verified_head(self) -> None:
        branch = "providers"
        sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        commands: list[list[str]] = []

        def fake_run(command, **kwargs):
            commands.append(command)
            if command[:3] == ["gh", "pr", "view"]:
                return completed(
                    command,
                    stdout=json.dumps(
                        {
                            "headRefName": branch,
                            "headRefOid": sha,
                            "url": "https://github.com/ImL1s/medley/pull/202",
                        }
                    ),
                )
            if command[:2] == ["git", "ls-remote"]:
                return completed(command, stdout=f"{sha}\trefs/heads/{branch}\n")
            if command[:3] == ["gh", "run", "list"]:
                self.assertEqual(command[command.index("--event") + 1], "push")
                return completed(
                    command,
                    stdout=json.dumps(
                        [
                            {
                                "databaseId": 9001,
                                "headSha": sha,
                                "status": "completed",
                                "conclusion": "success",
                                "url": "https://github.com/ImL1s/medley/actions/runs/9001",
                                "displayTitle": "CI",
                            }
                        ]
                    ),
                )
            if command[:2] == ["gh", "api"]:
                return completed(
                    command,
                    stdout=json.dumps(
                        {
                            "head_sha": sha,
                            "event": "push",
                            "head_branch": branch,
                            "path": ".github/workflows/ci.yml",
                            "status": "completed",
                            "conclusion": "success",
                        }
                    ),
                )
            raise AssertionError(f"unexpected command: {command}")

        with mock.patch.object(guard.subprocess, "run", side_effect=fake_run):
            out = io.StringIO()
            rc = guard.check_pr_head_ci(
                repo="ImL1s/medley",
                pr_number=202,
                remote="origin",
                limit=20,
                stream=out,
            )

        self.assertEqual(rc, 0, out.getvalue())
        run_list = next(cmd for cmd in commands if cmd[:3] == ["gh", "run", "list"])
        self.assertEqual(run_list[run_list.index("--event") + 1], "push")

    def test_direct_branch_sha_probe_fails_when_no_ci_run_matches(self) -> None:
        branch = "fix/54-switch-transaction-boundaries"
        missing_sha = "cccccccccccccccccccccccccccccccccccccccc"

        def fake_run(command, **kwargs):
            if command[:3] == ["gh", "pr", "view"]:
                raise AssertionError("direct branch mode must not query PR metadata")
            if command[:2] == ["git", "ls-remote"]:
                raise AssertionError("explicit head-sha mode must not call ls-remote")
            if command[:3] == ["gh", "run", "list"]:
                self.assertEqual(command[command.index("--event") + 1], "pull_request")
                return completed(
                    command,
                    stdout=json.dumps(
                        [
                            {
                                "databaseId": 31178074734,
                                "headSha": "e9eca323281b192e3a14f670c490ec9fb8d64a8f",
                                "status": "completed",
                                "conclusion": "success",
                                "url": "https://github.com/ImL1s/medley/actions/runs/31178074734",
                                "displayTitle": "CI",
                            }
                        ]
                    ),
                )
            if command[:2] == ["gh", "api"]:
                return completed(
                    command,
                    stdout=json.dumps(
                        {
                            "head_sha": "e9eca323281b192e3a14f670c490ec9fb8d64a8f",
                            "event": "pull_request",
                            "head_branch": branch,
                            "path": ".github/workflows/ci.yml",
                            "status": "completed",
                            "conclusion": "success",
                            "pull_requests": [
                                {
                                    "head": {
                                        "ref": branch,
                                        "sha": "960d816ecb19a52bdea9288b7bf66c6095276036",
                                    }
                                }
                            ],
                        }
                    ),
                )
            raise AssertionError(f"unexpected command: {command}")

        with mock.patch.object(guard.subprocess, "run", side_effect=fake_run):
            out = io.StringIO()
            rc = guard.check_branch_head_ci(
                repo="ImL1s/medley",
                branch=branch,
                head_sha=missing_sha,
                limit=20,
                stream=out,
            )

        self.assertEqual(rc, 1, out.getvalue())
        self.assertIn(
            "No completed, successful pull_request run of .github/workflows/ci.yml",
            out.getvalue(),
        )
        self.assertIn("verdict: absent", out.getvalue())
        self.assertNotIn("`gh run list` returned 0 rows", out.getvalue())

    def test_classify_zero_runs_is_absent_not_skipped(self) -> None:
        self.assertEqual(
            guard.classify_head_ci(
                [],
                verified_success=[],
                rejected_success=[],
            ),
            guard.VERDICT_ABSENT,
        )
        skipped = listed_run(
            1, PROVIDERS_SHA, status="completed", conclusion="skipped"
        )
        self.assertEqual(
            guard.classify_head_ci(
                [skipped],
                verified_success=[],
                rejected_success=[],
            ),
            guard.VERDICT_SKIPPED,
        )
        self.assertNotEqual(guard.VERDICT_ABSENT, guard.VERDICT_SKIPPED)

    def test_classify_failed_and_cancelled_are_not_absent(self) -> None:
        failed = listed_run(
            1, PROVIDERS_SHA, status="completed", conclusion="failure"
        )
        cancelled = listed_run(
            2, PROVIDERS_SHA, status="completed", conclusion="cancelled"
        )
        self.assertEqual(
            guard.classify_head_ci(
                [failed],
                verified_success=[],
                rejected_success=[],
            ),
            guard.VERDICT_FAILED,
        )
        self.assertEqual(
            guard.classify_head_ci(
                [cancelled],
                verified_success=[],
                rejected_success=[],
            ),
            guard.VERDICT_SKIPPED,
        )

    def test_classify_queued_and_in_progress_are_not_absent(self) -> None:
        queued = listed_run(1, PROVIDERS_SHA, status="queued", conclusion=None)
        in_progress = listed_run(
            2, PROVIDERS_SHA, status="in_progress", conclusion=None
        )
        self.assertEqual(
            guard.classify_head_ci(
                [queued],
                verified_success=[],
                rejected_success=[],
            ),
            guard.VERDICT_IN_PROGRESS,
        )
        self.assertEqual(
            guard.classify_head_ci(
                [in_progress],
                verified_success=[],
                rejected_success=[],
            ),
            guard.VERDICT_IN_PROGRESS,
        )

    def test_providers_push_fails_closed_when_zero_runs(self) -> None:
        with mock.patch.object(guard.subprocess, "run", side_effect=fake_push_list([])):
            out = io.StringIO()
            rc = guard.check_branch_head_ci(
                repo="ImL1s/medley",
                branch="providers",
                head_sha=PROVIDERS_SHA,
                limit=20,
                stream=out,
            )

        text = out.getvalue()
        self.assertEqual(rc, 1, text)
        self.assertIn("verdict: absent", text)
        self.assertIn(
            "No completed, successful push run of .github/workflows/ci.yml",
            text,
        )
        self.assertIn("no push CI run at all (the dropped-webhook case)", text)
        self.assertIn("not a queued or in-progress run", text)
        self.assertIn("`gh run list` returned 0 rows for this branch/event", text)
        self.assertNotIn("still queued/in progress", text)
        self.assertNotIn("completed as skipped", text)

    def test_providers_push_in_progress_is_not_absent(self) -> None:
        runs = [
            listed_run(
                9002,
                PROVIDERS_SHA,
                status="in_progress",
                conclusion=None,
            )
        ]
        with mock.patch.object(guard.subprocess, "run", side_effect=fake_push_list(runs)):
            out = io.StringIO()
            rc = guard.check_branch_head_ci(
                repo="ImL1s/medley",
                branch="providers",
                head_sha=PROVIDERS_SHA,
                limit=20,
                stream=out,
            )

        text = out.getvalue()
        self.assertEqual(rc, 1, text)
        self.assertIn("verdict: in_progress", text)
        self.assertIn("still queued/in progress", text)
        self.assertIn("not the absent/dropped-webhook case", text)
        self.assertNotIn("no push CI run at all", text)

    def test_providers_push_queued_is_not_absent(self) -> None:
        runs = [listed_run(9003, PROVIDERS_SHA, status="queued", conclusion=None)]
        with mock.patch.object(guard.subprocess, "run", side_effect=fake_push_list(runs)):
            out = io.StringIO()
            rc = guard.check_branch_head_ci(
                repo="ImL1s/medley",
                branch="providers",
                head_sha=PROVIDERS_SHA,
                limit=20,
                stream=out,
            )

        text = out.getvalue()
        self.assertEqual(rc, 1, text)
        self.assertIn("verdict: in_progress", text)
        self.assertNotIn("no push CI run at all", text)

    def test_providers_push_skipped_is_not_success_and_not_absent(self) -> None:
        runs = [
            listed_run(
                9004,
                PROVIDERS_SHA,
                status="completed",
                conclusion="skipped",
            )
        ]
        with mock.patch.object(guard.subprocess, "run", side_effect=fake_push_list(runs)):
            out = io.StringIO()
            rc = guard.check_branch_head_ci(
                repo="ImL1s/medley",
                branch="providers",
                head_sha=PROVIDERS_SHA,
                limit=20,
                stream=out,
            )

        text = out.getvalue()
        self.assertEqual(rc, 1, text)
        self.assertIn("verdict: skipped", text)
        self.assertIn("completed as skipped/cancelled, not success", text)
        self.assertIn("not the absent/dropped-webhook case", text)
        self.assertNotIn("no push CI run at all", text)

    def test_providers_push_failed_is_not_absent(self) -> None:
        runs = [
            listed_run(
                9005,
                PROVIDERS_SHA,
                status="completed",
                conclusion="failure",
            )
        ]
        with mock.patch.object(guard.subprocess, "run", side_effect=fake_push_list(runs)):
            out = io.StringIO()
            rc = guard.check_branch_head_ci(
                repo="ImL1s/medley",
                branch="providers",
                head_sha=PROVIDERS_SHA,
                limit=20,
                stream=out,
            )

        text = out.getvalue()
        self.assertEqual(rc, 1, text)
        self.assertIn("verdict: failed", text)
        self.assertIn("none completed successfully", text)
        self.assertNotIn("no push CI run at all", text)

    def test_evaluate_pr_checks_empty_is_absent(self) -> None:
        verdict, code, message = guard.evaluate_pr_checks([])
        self.assertEqual(verdict, guard.VERDICT_ABSENT)
        self.assertEqual(code, 1)
        self.assertIn("no checks at all", message)
        self.assertIn("no checks reported", message)

    def test_evaluate_pr_checks_pending_is_in_progress_not_absent(self) -> None:
        verdict, code, message = guard.evaluate_pr_checks(
            [{"name": "CI", "state": "PENDING", "bucket": "pending"}]
        )
        self.assertEqual(verdict, guard.VERDICT_IN_PROGRESS)
        self.assertEqual(code, 1)
        self.assertIn("queued/in progress", message)
        self.assertNotIn("no checks at all", message)

    def test_evaluate_pr_checks_skipped_only_is_not_absent(self) -> None:
        verdict, code, message = guard.evaluate_pr_checks(
            [{"name": "CI", "state": "SKIPPED", "bucket": "skipping"}]
        )
        self.assertEqual(verdict, guard.VERDICT_SKIPPED)
        self.assertEqual(code, 1)
        self.assertIn("none passed", message)
        self.assertNotIn("no checks at all", message)

    def test_evaluate_pr_checks_pass_with_skip_succeeds(self) -> None:
        verdict, code, message = guard.evaluate_pr_checks(
            [
                {"name": "CI", "state": "SUCCESS", "bucket": "pass"},
                {"name": "optional", "state": "SKIPPED", "bucket": "skipping"},
            ]
        )
        self.assertEqual(verdict, guard.VERDICT_SUCCESS)
        self.assertEqual(code, 0)
        self.assertIn("2 checks, all green", message)

    def test_evaluate_pr_checks_failed_is_not_absent(self) -> None:
        verdict, code, message = guard.evaluate_pr_checks(
            [{"name": "CI", "state": "FAILURE", "bucket": "fail"}]
        )
        self.assertEqual(verdict, guard.VERDICT_FAILED)
        self.assertEqual(code, 1)
        self.assertIn("not concluded successfully: CI", message)

    def test_main_evaluate_pr_checks_empty_is_fail_closed(self) -> None:
        stdin = io.StringIO("[]")
        stdout = io.StringIO()
        stderr = io.StringIO()
        with (
            mock.patch.object(guard.sys, "stdin", stdin),
            mock.patch.object(guard.sys, "stdout", stdout),
            mock.patch.object(guard.sys, "stderr", stderr),
        ):
            rc = guard.main(["--evaluate-pr-checks"])
        self.assertEqual(rc, 1)
        self.assertIn("verdict: absent", stdout.getvalue())
        self.assertIn("no checks reported", stderr.getvalue())

    def test_merge_pr_wrapper_uses_evaluate_pr_checks(self) -> None:
        text = (REPO / "scripts" / "merge-pr.sh").read_text(encoding="utf-8")
        self.assertIn("check_pr_head_ci_run.py", text)
        self.assertIn("--evaluate-pr-checks", text)
        self.assertIn("gh pr checks", text)
        self.assertNotIn("if not rows:", text)


if __name__ == "__main__":
    unittest.main()
