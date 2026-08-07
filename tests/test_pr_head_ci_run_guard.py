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


if __name__ == "__main__":
    unittest.main()
