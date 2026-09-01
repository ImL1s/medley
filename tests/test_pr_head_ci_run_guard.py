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
                self.assertIn("--branch", command)
                self.assertEqual(command[command.index("--branch") + 1], branch)
                self.assertNotIn("--commit", command)
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
                endpoint = command[2]
                self.assertNotIn(
                    "/commits/",
                    endpoint,
                    f"exact PR-head match must not fetch merge parents: {command}",
                )
                self.assertEqual(endpoint, "repos/ImL1s/medley/actions/runs/31182413606")
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
        self.assertIn("--branch", run_list)
        self.assertNotIn("--commit", run_list)

    def test_feature_branch_uses_pull_request_probe_and_passes_for_pr_head(self) -> None:
        branch = "fix/54-switch-transaction-boundaries"
        pr_head_sha = "960d816ecb19a52bdea9288b7bf66c6095276036"
        merge_sha = "e9eca323281b192e3a14f670c490ec9fb8d64a8f"
        base_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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
                ref = command[3] if len(command) > 3 else ""
                if ref == "refs/pull/202/merge":
                    return completed(
                        command, stdout=f"{merge_sha}\trefs/pull/202/merge\n"
                    )
                return completed(command, stdout=f"{pr_head_sha}\trefs/heads/{branch}\n")
            if command[:3] == ["gh", "run", "list"]:
                self.assertEqual(command[command.index("--event") + 1], "pull_request")
                # `--commit $PR_HEAD` does not return the merge-commit run.
                # A fake that ignored that filter used to mask the production miss.
                if "--commit" in command:
                    self.assertEqual(
                        command[command.index("--commit") + 1], pr_head_sha
                    )
                    return completed(command, stdout=json.dumps([]))
                self.assertIn("--branch", command)
                self.assertEqual(command[command.index("--branch") + 1], branch)
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
                endpoint = command[2]
                if endpoint == f"repos/ImL1s/medley/commits/{merge_sha}":
                    return completed(
                        command,
                        stdout=json.dumps(
                            {
                                "sha": merge_sha,
                                "parents": [
                                    {"sha": base_sha},
                                    {"sha": pr_head_sha},
                                ],
                                "commit": {
                                    "committer": {
                                        "name": "GitHub",
                                        "email": "noreply@github.com",
                                    },
                                    "verification": {"verified": True},
                                    "message": f"Merge {pr_head_sha} into {base_sha}",
                                },
                            }
                        ),
                    )
                self.assertEqual(endpoint, "repos/ImL1s/medley/actions/runs/31178074734")
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
        self.assertNotIn("--commit", run_list)
        self.assertIn("--branch", run_list)
        self.assertEqual(run_list[run_list.index("--branch") + 1], branch)
        self.assertIn("--event", run_list)
        self.assertEqual(run_list[run_list.index("--event") + 1], "pull_request")
        self.assertFalse(any(cmd[:3] == ["gh", "pr", "checks"] for cmd in commands))
        self.assertTrue(
            any(
                cmd[:2] == ["gh", "api"]
                and cmd[2] == f"repos/ImL1s/medley/commits/{merge_sha}"
                for cmd in commands
            ),
            commands,
        )

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
                self.assertIn("--branch", command)
                self.assertNotIn("--commit", command)
                return completed(command, stdout=json.dumps([]))
            if command[:2] == ["gh", "api"]:
                raise AssertionError("absent commit list must not fetch run detail")
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

    def test_rewritten_pr_head_on_old_run_is_not_a_receipt(self) -> None:
        """GitHub rewrites pull_requests[].head.sha to the live tip.

        An older successful run must not verify a later head (#530).
        """

        branch = "fix/513-negated-close-guard"
        current = "ff40de1f6a6c56a4884f609a98a206264924103b"
        old = "fb4ffbacb3bce9be58980b81366428c2b632857f"
        old_merge = "cccccccccccccccccccccccccccccccccccccccc"
        current_merge = "dddddddddddddddddddddddddddddddddddddddd"
        base_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

        def fake_run(command, **kwargs):
            if command[:3] == ["gh", "run", "list"]:
                self.assertIn("--branch", command)
                self.assertEqual(command[command.index("--branch") + 1], branch)
                self.assertNotIn("--commit", command)
                return completed(
                    command,
                    stdout=json.dumps(
                        [
                            {
                                "databaseId": 33365597354,
                                "headSha": old_merge,
                                "status": "completed",
                                "conclusion": "success",
                                "url": "https://github.com/ImL1s/medley/actions/runs/33365597354",
                                "displayTitle": "CI",
                                "event": "pull_request",
                            },
                            {
                                "databaseId": 33383438551,
                                "headSha": current_merge,
                                "status": "in_progress",
                                "conclusion": "",
                                "url": "https://github.com/ImL1s/medley/actions/runs/33383438551",
                                "displayTitle": "CI",
                                "event": "pull_request",
                            },
                        ]
                    ),
                )
            if command[:2] == ["gh", "api"]:
                endpoint = command[2]
                if endpoint == f"repos/ImL1s/medley/commits/{old_merge}":
                    return completed(
                        command,
                        stdout=json.dumps(
                            {
                                "sha": old_merge,
                                "parents": [
                                    {"sha": base_sha},
                                    {"sha": old},
                                ],
                                "commit": {
                                    "committer": {
                                        "name": "GitHub",
                                        "email": "noreply@github.com",
                                    },
                                    "verification": {"verified": True},
                                    "message": f"Merge {old} into {base_sha}",
                                },
                            }
                        ),
                    )
                if endpoint == f"repos/ImL1s/medley/commits/{current_merge}":
                    return completed(
                        command,
                        stdout=json.dumps(
                            {
                                "sha": current_merge,
                                "parents": [
                                    {"sha": base_sha},
                                    {"sha": current},
                                ],
                                "commit": {
                                    "committer": {
                                        "name": "GitHub",
                                        "email": "noreply@github.com",
                                    },
                                    "verification": {"verified": True},
                                    "message": f"Merge {current} into {base_sha}",
                                },
                            }
                        ),
                    )
                raise AssertionError(
                    "in-progress current-head list must not treat rewritten "
                    f"pull_requests[].head.sha on old run as a receipt: {command}"
                )
            raise AssertionError(f"unexpected command: {command}")

        with mock.patch.object(guard.subprocess, "run", side_effect=fake_run):
            out = io.StringIO()
            rc = guard.check_branch_head_ci(
                repo="ImL1s/medley",
                branch=branch,
                head_sha=current,
                limit=20,
                stream=out,
            )
        self.assertEqual(rc, 1, out.getvalue())
        self.assertIn("verdict: in_progress", out.getvalue())

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

    def test_one_parent_successor_is_not_a_ci_receipt(self) -> None:
        """A green child of tip H must not satisfy a later force-push of H
        (#530 review). Only Actions synthetic merges inherit via parent
        association."""

        tip = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        child = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        merge = "cccccccccccccccccccccccccccccccccccccccc"
        ordinary = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        forged = "ffffffffffffffffffffffffffffffffffffffff"
        base = "dddddddddddddddddddddddddddddddddddddddd"
        cache: dict[str, set[str]] = {
            child: {tip},
            merge: {base, tip},
            ordinary: {base, tip},
            forged: {base, tip},
        }
        messages = {
            child: "fix something",
            merge: f"Merge {tip} into {base}",
            ordinary: "Merge branch 'feature' into providers",
            forged: f"Merge {tip} into {base}",
        }
        proofs = {
            child: False,
            merge: True,
            ordinary: False,
            forged: False,
        }
        self.assertFalse(
            guard.run_head_matches_requested(
                repo="ImL1s/medley",
                recorded_sha=child,
                requested_sha=tip,
                parent_cache=cache,
                message_cache=messages,
                github_proof_cache=proofs,
            )
        )
        self.assertFalse(
            guard.run_head_matches_requested(
                repo="ImL1s/medley",
                recorded_sha=ordinary,
                requested_sha=tip,
                parent_cache=cache,
                message_cache=messages,
                github_proof_cache=proofs,
            )
        )
        # Matching message + parents without GitHub verification is forgeable.
        self.assertFalse(
            guard.run_head_matches_requested(
                repo="ImL1s/medley",
                recorded_sha=forged,
                requested_sha=tip,
                parent_cache=cache,
                message_cache=messages,
                github_proof_cache=proofs,
            )
        )
        # GitHub-signed ordinary merges can forge the same subject; bind to
        # the live refs/pull/N/merge identity (#530 review).
        self.assertFalse(
            guard.run_head_matches_requested(
                repo="ImL1s/medley",
                recorded_sha=merge,
                requested_sha=tip,
                parent_cache=cache,
                message_cache=messages,
                github_proof_cache=proofs,
                pull_merge_sha=forged,
                require_pull_merge=True,
            )
        )
        self.assertFalse(
            guard.run_head_matches_requested(
                repo="ImL1s/medley",
                recorded_sha=merge,
                requested_sha=tip,
                parent_cache=cache,
                message_cache=messages,
                github_proof_cache=proofs,
                pull_merge_sha=None,
                require_pull_merge=True,
            )
        )
        self.assertTrue(
            guard.run_head_matches_requested(
                repo="ImL1s/medley",
                recorded_sha=merge,
                requested_sha=tip,
                parent_cache=cache,
                message_cache=messages,
                github_proof_cache=proofs,
                pull_merge_sha=merge,
                require_pull_merge=True,
            )
        )
        self.assertTrue(
            guard.run_head_matches_requested(
                repo="ImL1s/medley",
                recorded_sha=tip,
                requested_sha=tip,
                parent_cache=cache,
                message_cache=messages,
                github_proof_cache=proofs,
                pull_merge_sha=merge,
            )
        )

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
        self.assertIn("--report-pr-heads", text)
        self.assertIn("gh pr checks", text)
        self.assertNotIn("if not rows:", text)

    def test_fork_md_describes_immutable_pr_receipt(self) -> None:
        text = (REPO / "FORK.md").read_text(encoding="utf-8")
        self.assertNotIn(
            "matches the target commit against the run detail's\n"
            "`pull_requests[].head.sha`",
            text,
        )
        self.assertIn("immutable `head_sha`", text)
        self.assertIn("synthetic merge commit whose git", text)
        self.assertIn("parents include that PR head", text)


class PrHeadHistoryReportTests(unittest.TestCase):
    SHA_1 = "1111111111111111111111111111111111111111"
    SHA_2 = "2222222222222222222222222222222222222222"

    @staticmethod
    def check_run(
        run_id: int,
        name: str,
        sha: str,
        *,
        status: str = "completed",
        conclusion: str | None = "success",
        app_id: int = 15368,
        app_slug: str = "github-actions",
        suite_id: int = 100,
    ) -> dict:
        return {
            "id": run_id,
            "name": name,
            "head_sha": sha,
            "status": status,
            "conclusion": conclusion,
            "app": {"id": app_id, "slug": app_slug},
            "check_suite": {"id": suite_id},
        }

    def fake_api(self, responses: dict[str, object]):
        def fake_run(command, **kwargs):
            if command[:3] == ["gh", "pr", "view"]:
                return completed(
                    command,
                    stdout=json.dumps(
                        {
                            "headRefName": "fix/history",
                            "headRefOid": self.SHA_2,
                            "url": "https://github.com/ImL1s/medley/pull/506",
                        }
                    ),
                )
            if command[:5] == ["gh", "api", "graphql", "--paginate", "--slurp"]:
                pages = responses.get("pr_commits")
                if pages is None:
                    raise AssertionError("unexpected GraphQL PR commits request")
                total_count = sum(len(page) for page in pages)
                wrapped = []
                for index, page in enumerate(pages):
                    has_next_page = index < len(pages) - 1
                    wrapped.append(
                        {
                            "data": {
                                "repository": {
                                    "pullRequest": {
                                        "commits": {
                                            "totalCount": total_count,
                                            "nodes": [
                                                {"commit": {"oid": row["sha"]}}
                                                for row in page
                                            ],
                                            "pageInfo": {
                                                "hasNextPage": has_next_page,
                                                "endCursor": (
                                                    f"cursor-{index}"
                                                    if has_next_page
                                                    else None
                                                ),
                                            },
                                        }
                                    }
                                }
                            }
                        }
                    )
                return completed(command, stdout=json.dumps(wrapped))
            if command[:4] != ["gh", "api", "--paginate", "--slurp"]:
                raise AssertionError(f"unexpected command: {command}")
            endpoint = command[4]
            if endpoint not in responses:
                raise AssertionError(f"unexpected endpoint: {endpoint}")
            return completed(command, stdout=json.dumps(responses[endpoint]))

        return fake_run

    def test_commit_history_uses_graphql_connection_beyond_rest_cap(self) -> None:
        shas = [f"{index:040x}" for index in range(1, 252)]
        responses = {
            "pr_commits": [
                [{"sha": sha} for sha in shas[:100]],
                [{"sha": sha} for sha in shas[100:200]],
                [{"sha": sha} for sha in shas[200:]],
            ]
        }
        commands: list[list[str]] = []

        def recording_api(command, **kwargs):
            commands.append(command)
            return self.fake_api(responses)(command, **kwargs)

        with mock.patch.object(guard.subprocess, "run", side_effect=recording_api):
            actual = guard.list_pr_commit_shas("ImL1s/medley", 506)

        self.assertEqual(actual, shas)
        self.assertEqual(len(commands), 1)
        self.assertEqual(commands[0][:5], ["gh", "api", "graphql", "--paginate", "--slurp"])
        self.assertNotIn("pulls/506/commits", " ".join(commands[0]))

    def test_report_distinguishes_cancelled_queued_and_success_without_blocking(
        self,
    ) -> None:
        responses = {
            "pr_commits": [
                [{"sha": self.SHA_1}, {"sha": self.SHA_2}]
            ],
            (
                f"repos/ImL1s/medley/commits/{self.SHA_1}/check-runs"
                "?filter=all&per_page=100"
            ): [
                {
                    "total_count": 2,
                    "check_runs": [
                        self.check_run(
                            1,
                            "Compile every test target",
                            self.SHA_1,
                            conclusion="cancelled",
                        ),
                        self.check_run(
                            2,
                            "Format",
                            self.SHA_1,
                        ),
                    ],
                }
            ],
            (
                f"repos/ImL1s/medley/commits/{self.SHA_2}/check-runs"
                "?filter=all&per_page=100"
            ): [
                {
                    "total_count": 2,
                    "check_runs": [
                        self.check_run(
                            3,
                            "Compile every test target",
                            self.SHA_2,
                            status="queued",
                            conclusion=None,
                        ),
                        self.check_run(4, "Format", self.SHA_2),
                    ],
                }
            ],
        }
        with mock.patch.object(
            guard.subprocess, "run", side_effect=self.fake_api(responses)
        ):
            out = io.StringIO()
            rc = guard.report_pr_head_history(
                repo="ImL1s/medley", pr_number=506, stream=out
            )

        text = out.getvalue()
        self.assertEqual(rc, 0, text)
        self.assertIn(f"head {self.SHA_1[:8]}", text)
        self.assertIn("Compile every test target: cancelled", text)
        self.assertIn(f"head {self.SHA_2[:8]} (current)", text)
        self.assertIn("Compile every test target: queued", text)
        self.assertIn("Format: success", text)
        self.assertIn("historical states are report-only", text)

    def test_report_paginates_and_shows_superseded_rerun_attempts(self) -> None:
        responses = {
            "pr_commits": [
                [{"sha": self.SHA_1}],
                [{"sha": self.SHA_2}],
            ],
            (
                f"repos/ImL1s/medley/commits/{self.SHA_1}/check-runs"
                "?filter=all&per_page=100"
            ): [
                {
                    "total_count": 2,
                    "check_runs": [
                        self.check_run(
                            10,
                            "Tests (providers hot path)",
                            self.SHA_1,
                            conclusion="cancelled",
                        )
                    ],
                },
                {
                    "total_count": 2,
                    "check_runs": [
                        self.check_run(
                            11, "Tests (providers hot path)", self.SHA_1
                        )
                    ],
                },
            ],
            (
                f"repos/ImL1s/medley/commits/{self.SHA_2}/check-runs"
                "?filter=all&per_page=100"
            ): [{"total_count": 0, "check_runs": []}],
        }
        with mock.patch.object(
            guard.subprocess, "run", side_effect=self.fake_api(responses)
        ):
            out = io.StringIO()
            rc = guard.report_pr_head_history(
                repo="ImL1s/medley", pr_number=506, stream=out
            )

        text = out.getvalue()
        self.assertEqual(rc, 0, text)
        self.assertIn(
            "Tests (providers hot path): success "
            "[run=11 attempts: 10=cancelled -> 11=success;",
            text,
        )
        self.assertIn(f"head {self.SHA_2[:8]} (current): absent", text)

    def test_report_does_not_collapse_same_name_checks_from_other_suites(
        self,
    ) -> None:
        responses = {
            "pr_commits": [
                [{"sha": self.SHA_2}]
            ],
            (
                f"repos/ImL1s/medley/commits/{self.SHA_2}/check-runs"
                "?filter=all&per_page=100"
            ): [
                {
                    "total_count": 2,
                    "check_runs": [
                        self.check_run(
                            10,
                            "CI",
                            self.SHA_2,
                            conclusion="cancelled",
                            app_id=1,
                            app_slug="first-app",
                            suite_id=10,
                        ),
                        self.check_run(
                            11,
                            "CI",
                            self.SHA_2,
                            app_id=2,
                            app_slug="second-app",
                            suite_id=20,
                        ),
                    ],
                }
            ],
        }
        with mock.patch.object(
            guard.subprocess, "run", side_effect=self.fake_api(responses)
        ):
            out = io.StringIO()
            rc = guard.report_pr_head_history(
                repo="ImL1s/medley", pr_number=506, stream=out
            )

        text = out.getvalue()
        self.assertEqual(rc, 0, text)
        self.assertEqual(text.count("  CI:"), 2)
        self.assertNotIn("attempts:", text)
        self.assertIn("app=first-app#1; suite=10", text)
        self.assertIn("app=second-app#2; suite=20", text)

    def test_report_rejects_unknown_states_and_unsafe_names(self) -> None:
        bad_rows = [
            self.check_run(1, "CI", self.SHA_2, status="banana", conclusion=None),
            self.check_run(1, "CI", self.SHA_2, conclusion="banana"),
            self.check_run(1, "Format\nhead deadbeef", self.SHA_2),
            self.check_run(1, "\x1b[31mFormat", self.SHA_2),
        ]
        for bad_row in bad_rows:
            with self.subTest(row=bad_row):
                responses = {
                    "pr_commits": [
                        [{"sha": self.SHA_2}]
                    ],
                    (
                        f"repos/ImL1s/medley/commits/{self.SHA_2}/check-runs"
                        "?filter=all&per_page=100"
                    ): [{"total_count": 1, "check_runs": [bad_row]}],
                }
                with mock.patch.object(
                    guard.subprocess,
                    "run",
                    side_effect=self.fake_api(responses),
                ):
                    with self.assertRaises(guard.CiHeadGateError):
                        guard.report_pr_head_history(
                            repo="ImL1s/medley",
                            pr_number=506,
                            stream=io.StringIO(),
                        )

    def test_report_rejects_check_run_for_a_different_head(self) -> None:
        responses = {
            "pr_commits": [
                [{"sha": self.SHA_2}]
            ],
            (
                f"repos/ImL1s/medley/commits/{self.SHA_2}/check-runs"
                "?filter=all&per_page=100"
            ): [
                {
                    "total_count": 1,
                    "check_runs": [
                        self.check_run(1, "CI", self.SHA_1)
                    ],
                }
            ],
        }
        with mock.patch.object(
            guard.subprocess, "run", side_effect=self.fake_api(responses)
        ):
            with self.assertRaisesRegex(guard.CiHeadGateError, "different head SHA"):
                guard.report_pr_head_history(
                    repo="ImL1s/medley", pr_number=506, stream=io.StringIO()
                )

    def test_report_rejects_commit_pages_that_omit_the_current_head(self) -> None:
        responses = {
            "pr_commits": [
                [{"sha": self.SHA_1}]
            ]
        }
        with mock.patch.object(
            guard.subprocess, "run", side_effect=self.fake_api(responses)
        ):
            with self.assertRaisesRegex(guard.CiHeadGateError, "current head"):
                guard.report_pr_head_history(
                    repo="ImL1s/medley", pr_number=506, stream=io.StringIO()
                )


if __name__ == "__main__":
    unittest.main()
