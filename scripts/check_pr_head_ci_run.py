#!/usr/bin/env python3
"""Fail closed when a target branch head has no successful CI run (issue #202).

Why this exists:
- A push can land on the remote with no workflow run created at all.
- `gh pr checks` prints "no checks reported" in that state, which has been
  treated as success/absent rather than fail-closed.
- Queued/in-progress, skipped, and zero-run are different states; only the
  last one is `absent`.
- For pull_request runs, GitHub's SHA fields are inconsistent in practice:
  sometimes `head_sha` is the PR head, sometimes it points at a merge commit,
  and `pull_requests[].head.sha` may be missing.
- The reliable question is branch-head based: compare `git ls-remote` for the
  branch against CI runs that actually target that branch head.

This script is a pre-merge guard meant to run on a developer machine (or merge
orchestrator), not inside CI itself. `--evaluate-pr-checks` is the fail-closed
wrapper for `gh pr checks --json` used by `scripts/merge-pr.sh`.
`--report-pr-heads` reconciles every commit in a PR with its check runs so a
cancelled historical run cannot be mistaken for a still-pending current run.
"""

from __future__ import annotations

import argparse
import json
import string
import subprocess
import sys
from typing import Any, TextIO

CI_WORKFLOW_PATH = ".github/workflows/ci.yml"
RUN_LIST_FIELDS = "databaseId,headSha,status,conclusion,url,displayTitle,event"
PROVIDERS_BRANCH = "providers"

# Structured verdicts so "no run created" is never confused with queued, skipped,
# or failed. Every non-success verdict is fail-closed (exit 1).
VERDICT_SUCCESS = "success"
VERDICT_ABSENT = "absent"
VERDICT_IN_PROGRESS = "in_progress"
VERDICT_SKIPPED = "skipped"
VERDICT_FAILED = "failed"
VERDICT_IDENTITY_REJECTED = "identity_rejected"

IN_PROGRESS_STATUSES = frozenset(
    {"queued", "in_progress", "waiting", "requested", "pending"}
)
SKIPPED_CONCLUSIONS = frozenset({"skipped", "neutral", "cancelled"})
PR_CHECK_PASS_BUCKETS = frozenset({"pass"})
PR_CHECK_SKIP_BUCKETS = frozenset({"skipping", "skip", "skipped"})
PR_CHECK_PENDING_BUCKETS = frozenset({"pending"})


class CiHeadGateError(RuntimeError):
    """Raised when the guard cannot gather trustworthy inputs."""


def run_command(command: list[str], *, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        command,
        check=False,
        text=True,
        capture_output=True,
    )
    if check and result.returncode != 0:
        stderr = (result.stderr or "").strip()
        stdout = (result.stdout or "").strip()
        detail = stderr or stdout or f"exit code {result.returncode}"
        raise CiHeadGateError(f"`{' '.join(command)}` failed: {detail}")
    return result


def gh_json(args: list[str]) -> Any:
    result = run_command(["gh", *args])
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise CiHeadGateError(
            f"`gh {' '.join(args)}` did not return valid JSON"
        ) from exc


def gh_paginated_json(endpoint: str) -> Any:
    """Load every REST page while preserving page boundaries for validation."""

    result = run_command(["gh", "api", "--paginate", "--slurp", endpoint])
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise CiHeadGateError(
            f"`gh api --paginate --slurp {endpoint}` did not return valid JSON"
        ) from exc


def _is_sha(value: str) -> bool:
    return len(value) == 40 and all(c in string.hexdigits for c in value)


def load_pr_head(repo: str, pr_number: int) -> tuple[str, str, str]:
    data = gh_json(
        [
            "pr",
            "view",
            str(pr_number),
            "--repo",
            repo,
            "--json",
            "headRefName,headRefOid,url",
        ]
    )
    if not isinstance(data, dict):
        raise CiHeadGateError("`gh pr view` returned a non-object response")
    branch = data.get("headRefName")
    head_sha = data.get("headRefOid")
    pr_url = data.get("url")
    if not isinstance(branch, str) or not branch:
        raise CiHeadGateError("PR head branch is missing from `gh pr view`")
    if not isinstance(head_sha, str) or not _is_sha(head_sha):
        raise CiHeadGateError("PR head SHA is missing or malformed")
    if not isinstance(pr_url, str) or not pr_url:
        raise CiHeadGateError("PR URL is missing from `gh pr view`")
    return branch, head_sha.lower(), pr_url


def list_pr_commit_shas(repo: str, pr_number: int) -> list[str]:
    endpoint = f"repos/{repo}/pulls/{pr_number}/commits?per_page=100"
    pages = gh_paginated_json(endpoint)
    if not isinstance(pages, list):
        raise CiHeadGateError("PR commits API returned a non-array page list")

    shas: list[str] = []
    seen: set[str] = set()
    for page in pages:
        if not isinstance(page, list):
            raise CiHeadGateError("PR commits API returned a non-array page")
        for row in page:
            if not isinstance(row, dict):
                raise CiHeadGateError("PR commits API returned a non-object commit")
            sha = row.get("sha")
            if not isinstance(sha, str) or not _is_sha(sha):
                raise CiHeadGateError("PR commits API returned a malformed commit SHA")
            sha = sha.lower()
            if sha in seen:
                raise CiHeadGateError(f"PR commits API repeated commit SHA {sha}")
            seen.add(sha)
            shas.append(sha)

    if not shas:
        raise CiHeadGateError("PR commits API returned no commits")
    return shas


def _check_run_state(row: dict[str, Any]) -> str:
    status = row.get("status")
    if not isinstance(status, str) or not status:
        raise CiHeadGateError("check-runs API returned a malformed status")
    status = status.lower()
    if status != "completed":
        return status

    conclusion = row.get("conclusion")
    if not isinstance(conclusion, str) or not conclusion:
        raise CiHeadGateError(
            "check-runs API returned a completed run without a conclusion"
        )
    return conclusion.lower()


def list_commit_check_runs(repo: str, sha: str) -> list[dict[str, Any]]:
    endpoint = f"repos/{repo}/commits/{sha}/check-runs?filter=all&per_page=100"
    pages = gh_paginated_json(endpoint)
    if not isinstance(pages, list):
        raise CiHeadGateError("check-runs API returned a non-array page list")

    rows: list[dict[str, Any]] = []
    seen_ids: set[int] = set()
    total_count: int | None = None
    for page in pages:
        if not isinstance(page, dict):
            raise CiHeadGateError("check-runs API returned a non-object page")
        page_total = page.get("total_count")
        page_rows = page.get("check_runs")
        if not isinstance(page_total, int) or page_total < 0:
            raise CiHeadGateError("check-runs API returned a malformed total_count")
        if total_count is None:
            total_count = page_total
        elif page_total != total_count:
            raise CiHeadGateError("check-runs API returned inconsistent total_count values")
        if not isinstance(page_rows, list):
            raise CiHeadGateError("check-runs API returned a non-array check_runs value")

        for row in page_rows:
            if not isinstance(row, dict):
                raise CiHeadGateError("check-runs API returned a non-object check run")
            run_id = row.get("id")
            name = row.get("name")
            run_sha = row.get("head_sha")
            if not isinstance(run_id, int) or run_id <= 0:
                raise CiHeadGateError("check-runs API returned a malformed check run ID")
            if run_id in seen_ids:
                raise CiHeadGateError(f"check-runs API repeated check run ID {run_id}")
            if not isinstance(name, str) or not name:
                raise CiHeadGateError("check-runs API returned a nameless check run")
            if not isinstance(run_sha, str) or not _is_sha(run_sha):
                raise CiHeadGateError("check-runs API returned a malformed head SHA")
            if run_sha.lower() != sha:
                raise CiHeadGateError(
                    f"check run {run_id} belongs to a different head SHA"
                )
            _check_run_state(row)
            seen_ids.add(run_id)
            rows.append(row)

    if total_count is None:
        raise CiHeadGateError("check-runs API returned no pages")
    if len(rows) != total_count:
        raise CiHeadGateError(
            "check-runs API total_count does not match the paginated rows "
            f"({total_count} != {len(rows)})"
        )
    return rows


def report_pr_head_history(
    *, repo: str, pr_number: int, stream: TextIO
) -> int:
    """Print check-run states for every commit without gating on old states."""

    _branch, current_head, pr_url = load_pr_head(repo, pr_number)
    commit_shas = list_pr_commit_shas(repo, pr_number)
    if current_head not in commit_shas:
        raise CiHeadGateError("PR commit history omits the current head")
    if commit_shas[-1] != current_head:
        raise CiHeadGateError("PR commit history does not end at the current head")

    print(f"PR #{pr_number} head history: {pr_url}", file=stream)
    for sha in commit_shas:
        current = " (current)" if sha == current_head else ""
        label = f"head {_short_sha(sha)}{current}"
        runs = list_commit_check_runs(repo, sha)
        if not runs:
            print(f"{label}: absent", file=stream)
            continue

        print(label, file=stream)
        attempts_by_name: dict[str, list[dict[str, Any]]] = {}
        for row in runs:
            attempts_by_name.setdefault(str(row["name"]), []).append(row)
        for name in sorted(attempts_by_name):
            attempts = sorted(attempts_by_name[name], key=lambda row: int(row["id"]))
            states = [_check_run_state(row) for row in attempts]
            suffix = ""
            if len(states) > 1:
                suffix = f" (attempts: {' -> '.join(states)})"
            print(f"  {name}: {states[-1]}{suffix}", file=stream)

    print(
        "note: historical states are report-only; the exact current-head gates "
        "below remain authoritative",
        file=stream,
    )
    return 0


def remote_head_sha(remote: str, branch: str) -> str:
    result = run_command(["git", "ls-remote", remote, f"refs/heads/{branch}"])
    line = result.stdout.strip()
    if not line:
        raise CiHeadGateError(
            f"`git ls-remote` found no remote branch `{branch}` on `{remote}`"
        )
    sha = line.split()[0].strip().lower()
    if not _is_sha(sha):
        raise CiHeadGateError(f"`git ls-remote` returned malformed SHA: {sha!r}")
    return sha


def list_branch_runs(repo: str, branch: str, event: str, limit: int) -> list[dict[str, Any]]:
    data = gh_json(
        [
            "run",
            "list",
            "--repo",
            repo,
            "--workflow",
            "ci.yml",
            "--branch",
            branch,
            "--event",
            event,
            "--limit",
            str(limit),
            "--json",
            RUN_LIST_FIELDS,
        ]
    )
    if not isinstance(data, list):
        raise CiHeadGateError("`gh run list` returned a non-array response")
    rows: list[dict[str, Any]] = []
    for row in data:
        if not isinstance(row, dict):
            raise CiHeadGateError("`gh run list` returned a non-object run entry")
        rows.append(row)
    return rows


def run_detail(repo: str, run_id: int) -> dict[str, Any]:
    data = gh_json(["api", f"repos/{repo}/actions/runs/{run_id}"])
    if not isinstance(data, dict):
        raise CiHeadGateError(f"run detail for {run_id} is not an object")
    return data


def matches_release_gate_shape(detail: dict[str, Any], *, sha: str, branch: str) -> bool:
    """Mirror release.yml's CI-run identity checks for this SHA."""

    return (
        detail.get("head_sha") == sha
        and detail.get("event") == "push"
        and detail.get("head_branch") == branch
        and detail.get("path") == CI_WORKFLOW_PATH
        and detail.get("status") == "completed"
        and detail.get("conclusion") == "success"
    )


def pull_request_head_sha(detail: dict[str, Any], branch: str) -> str | None:
    pull_requests = detail.get("pull_requests")
    if not isinstance(pull_requests, list):
        return None
    for pr in pull_requests:
        if not isinstance(pr, dict):
            continue
        head = pr.get("head")
        if not isinstance(head, dict):
            continue
        ref = head.get("ref")
        sha = head.get("sha")
        if ref != branch:
            continue
        if isinstance(sha, str) and _is_sha(sha):
            return sha.lower()
    return None


def pull_request_candidate_shas(detail: dict[str, Any], branch: str) -> set[str]:
    """Collect every SHA that can identify this pull_request run's branch head."""

    candidates: set[str] = set()
    head_sha = detail.get("head_sha")
    if isinstance(head_sha, str) and _is_sha(head_sha):
        candidates.add(head_sha.lower())

    pr_head_sha = pull_request_head_sha(detail, branch)
    if pr_head_sha is not None:
        candidates.add(pr_head_sha)
    return candidates


def matches_pull_request_gate_shape(detail: dict[str, Any], *, sha: str, branch: str) -> bool:
    return (
        detail.get("event") == "pull_request"
        and detail.get("head_branch") == branch
        and detail.get("path") == CI_WORKFLOW_PATH
        and detail.get("status") == "completed"
        and detail.get("conclusion") == "success"
        and sha in pull_request_candidate_shas(detail, branch)
    )


def expected_ci_event(branch: str) -> str:
    return "push" if branch == PROVIDERS_BRANCH else "pull_request"


def _short_sha(sha: str) -> str:
    return sha[:8]


def _row_status(row: dict[str, Any]) -> str:
    status = row.get("status")
    return status.lower() if isinstance(status, str) else ""


def _row_conclusion(row: dict[str, Any]) -> str:
    conclusion = row.get("conclusion")
    return conclusion.lower() if isinstance(conclusion, str) else ""


def row_is_in_progress(row: dict[str, Any]) -> bool:
    return _row_status(row) in IN_PROGRESS_STATUSES


def row_is_skipped(row: dict[str, Any]) -> bool:
    return _row_status(row) == "completed" and _row_conclusion(row) in SKIPPED_CONCLUSIONS


def classify_head_ci(
    head_runs: list[dict[str, Any]],
    *,
    verified_success: list[dict[str, Any]],
    rejected_success: list[int],
) -> str:
    """Classify this head's CI state.

    `absent` is only the empty-head-run case (dropped webhook / never created).
    A skipped, queued, or failed run is a different verdict, still fail-closed.
    """

    if verified_success:
        return VERDICT_SUCCESS
    if not head_runs:
        return VERDICT_ABSENT
    if any(row_is_in_progress(row) for row in head_runs):
        return VERDICT_IN_PROGRESS
    if rejected_success:
        return VERDICT_IDENTITY_REJECTED
    if all(row_is_skipped(row) for row in head_runs):
        return VERDICT_SKIPPED
    return VERDICT_FAILED


def _check_name(row: dict[str, Any]) -> str:
    name = row.get("name")
    return name if isinstance(name, str) and name else "<unnamed>"


def _check_bucket(row: dict[str, Any]) -> str:
    bucket = row.get("bucket")
    return bucket.lower() if isinstance(bucket, str) else ""


def evaluate_pr_checks(rows: Any) -> tuple[str, int, str]:
    """Fail-closed evaluator for `gh pr checks --json name,state,bucket`.

    `gh pr checks` prints "no checks reported" for an empty list, which has
    been read as a pass. Empty is `absent`. Pending is `in_progress`, not
    absent. Skip-only (no pass) is `skipped`, not success.
    """

    if not isinstance(rows, list):
        raise CiHeadGateError("`gh pr checks` returned a non-array response")
    for row in rows:
        if not isinstance(row, dict):
            raise CiHeadGateError("`gh pr checks` returned a non-object check entry")

    if not rows:
        return (
            VERDICT_ABSENT,
            1,
            "the PR reports no checks at all (`gh pr checks` calls this "
            '"no checks reported", which is not a pass)',
        )

    failed = [
        _check_name(row)
        for row in rows
        if _check_bucket(row)
        not in PR_CHECK_PASS_BUCKETS | PR_CHECK_SKIP_BUCKETS | PR_CHECK_PENDING_BUCKETS
    ]
    pending = [
        _check_name(row)
        for row in rows
        if _check_bucket(row) in PR_CHECK_PENDING_BUCKETS
    ]
    passing = [row for row in rows if _check_bucket(row) in PR_CHECK_PASS_BUCKETS]
    skipped = [
        _check_name(row)
        for row in rows
        if _check_bucket(row) in PR_CHECK_SKIP_BUCKETS
    ]

    if failed:
        return (
            VERDICT_FAILED,
            1,
            "not concluded successfully: " + ", ".join(failed),
        )
    if pending:
        return (
            VERDICT_IN_PROGRESS,
            1,
            "checks still queued/in progress: "
            + ", ".join(pending)
            + " (this is not the absent/\"no checks reported\" case)",
        )
    if not passing:
        skipped_note = ", ".join(skipped) if skipped else "none named"
        return (
            VERDICT_SKIPPED,
            1,
            "checks exist but none passed (only skipped: "
            + skipped_note
            + "); this is not the absent/\"no checks reported\" case",
        )
    return (
        VERDICT_SUCCESS,
        0,
        f"{len(rows)} checks, all green",
    )


def run_evaluate_pr_checks(stdin: TextIO, stdout: TextIO, stderr: TextIO) -> int:
    raw = stdin.read()
    try:
        rows = json.loads(raw) if raw.strip() else []
    except json.JSONDecodeError as exc:
        raise CiHeadGateError("`gh pr checks` did not return valid JSON") from exc
    verdict, code, message = evaluate_pr_checks(rows)
    print(f"verdict: {verdict}", file=stdout)
    if code == 0:
        print(f"    {message}", file=stdout)
    else:
        print(f"merge-pr: {message}", file=stderr)
    return code


def check_branch_head_ci(
    *,
    repo: str,
    branch: str,
    head_sha: str,
    limit: int,
    stream: TextIO,
    head_source: str = "explicit --head-sha",
) -> int:
    if not _is_sha(head_sha):
        raise CiHeadGateError("target head SHA is missing or malformed")

    event = expected_ci_event(branch)
    print(
        f"Branch `{branch}` target head: {_short_sha(head_sha)} ({head_source})",
        file=stream,
    )

    runs = list_branch_runs(repo, branch, event=event, limit=limit)
    details_by_id: dict[int, dict[str, Any]] = {}

    def detail_for(run_id: int) -> dict[str, Any]:
        detail = details_by_id.get(run_id)
        if detail is None:
            detail = run_detail(repo, run_id)
            details_by_id[run_id] = detail
        return detail

    head_runs: list[dict[str, Any]] = []
    if event == "push":
        head_runs = [row for row in runs if row.get("headSha") == head_sha]
    else:
        for row in runs:
            run_id = row.get("databaseId")
            if not isinstance(run_id, int):
                continue
            detail = detail_for(run_id)
            if head_sha in pull_request_candidate_shas(detail, branch):
                head_runs.append(row)

    candidate_success = [
        row
        for row in head_runs
        if row.get("status") == "completed" and row.get("conclusion") == "success"
    ]

    verified_success: list[dict[str, Any]] = []
    rejected_success: list[int] = []
    for row in candidate_success:
        run_id = row.get("databaseId")
        if not isinstance(run_id, int):
            continue
        detail = detail_for(run_id)
        if event == "push":
            is_match = matches_release_gate_shape(detail, sha=head_sha, branch=branch)
        else:
            is_match = matches_pull_request_gate_shape(detail, sha=head_sha, branch=branch)
        if is_match:
            verified_success.append(row)
        else:
            rejected_success.append(run_id)

    verdict = classify_head_ci(
        head_runs,
        verified_success=verified_success,
        rejected_success=rejected_success,
    )
    print(f"verdict: {verdict}", file=stream)

    if verdict == VERDICT_SUCCESS:
        print(
            f"ok: found {len(verified_success)} completed successful {event} run(s) of "
            f"{CI_WORKFLOW_PATH} for {_short_sha(head_sha)}",
            file=stream,
        )
        for row in verified_success:
            run_id = row.get("databaseId")
            url = row.get("url", "")
            print(f"  run {run_id}: {url}", file=stream)
        return 0

    print(
        f"error: No completed, successful {event} run of "
        f"{CI_WORKFLOW_PATH} exists for {_short_sha(head_sha)} on `{branch}`.",
        file=stream,
    )
    states = sorted(
        {
            f"status={row.get('status')} conclusion={row.get('conclusion')}"
            for row in head_runs
        }
    )
    if verdict == VERDICT_ABSENT:
        print(
            f"       This head SHA has no {event} CI run at all (the dropped-webhook case).",
            file=stream,
        )
        print(
            "       This is not a queued or in-progress run; no ci.yml run exists "
            "for this head.",
            file=stream,
        )
        if not runs:
            print(
                "       `gh run list` returned 0 rows for this branch/event "
                "(not merely an older SHA).",
                file=stream,
            )
    elif verdict == VERDICT_IN_PROGRESS:
        print(
            f"       Found {len(head_runs)} {event} run(s) for this head still "
            f"queued/in progress: {states}",
            file=stream,
        )
        print(
            "       This is not the absent/dropped-webhook case.",
            file=stream,
        )
    elif verdict == VERDICT_SKIPPED:
        print(
            f"       Found {len(head_runs)} {event} run(s) for this head, but they "
            f"completed as skipped/cancelled, not success: {states}",
            file=stream,
        )
        print(
            "       This is not the absent/dropped-webhook case.",
            file=stream,
        )
    elif verdict == VERDICT_IDENTITY_REJECTED:
        print(
            "       Found successful branch-head run(s), but identity checks "
            f"rejected them (run ids: {rejected_success}).",
            file=stream,
        )
    else:
        print(
            f"       Found {len(head_runs)} {event} run(s) for this head, but none "
            f"completed successfully: {states}",
            file=stream,
        )

    print(
        "       Probe rationale: uses branch head + `gh run list --branch`; "
        "it intentionally avoids `gh pr checks`.",
        file=stream,
    )
    if runs:
        print("       Recent branch runs:", file=stream)
        for row in runs[:5]:
            run_id = row.get("databaseId")
            head_sha = str(row.get("headSha", ""))[:8]
            status = row.get("status")
            conclusion = row.get("conclusion")
            listed_event = row.get("event")
            print(
                f"         - {run_id} head={head_sha} status={status} "
                f"conclusion={conclusion} event={listed_event}",
                file=stream,
            )
    return 1


def check_pr_head_ci(
    *,
    repo: str,
    pr_number: int,
    remote: str,
    limit: int,
    stream: TextIO,
) -> int:
    branch, pr_head_sha, pr_url = load_pr_head(repo, pr_number)
    remote_sha = remote_head_sha(remote, branch)
    print(f"PR #{pr_number}: {pr_url}", file=stream)
    if remote_sha != pr_head_sha:
        print(
            f"note: PR reports {_short_sha(pr_head_sha)} but remote is "
            f"{_short_sha(remote_sha)}; using remote SHA for gating",
            file=stream,
        )
    return check_branch_head_ci(
        repo=repo,
        branch=branch,
        head_sha=remote_sha,
        limit=limit,
        stream=stream,
        head_source=f"from git ls-remote {remote} refs/heads/{branch}",
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    target = parser.add_mutually_exclusive_group(required=True)
    target.add_argument("--pr", type=int, help="Pull request number")
    target.add_argument("--branch", help="Branch name to probe directly")
    target.add_argument(
        "--report-pr-heads",
        type=int,
        metavar="PR",
        help="Report check-run states for every commit in a pull request",
    )
    target.add_argument(
        "--evaluate-pr-checks",
        action="store_true",
        help=(
            "Read `gh pr checks --json name,state,bucket` from stdin and "
            "fail closed on empty (\"no checks reported\"), pending, or skip-only"
        ),
    )
    parser.add_argument(
        "--repo",
        default="ImL1s/medley",
        help="Repository in OWNER/REPO form (default: ImL1s/medley)",
    )
    parser.add_argument(
        "--remote",
        default="origin",
        help="git remote used for ls-remote head lookup (default: origin)",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=100,
        help="Maximum branch runs to inspect from `gh run list` (default: 100)",
    )
    parser.add_argument(
        "--head-sha",
        help=(
            "Explicit branch head SHA to probe directly (only with --branch). "
            "Without this, the guard reads the branch head from git ls-remote."
        ),
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.evaluate_pr_checks:
        if args.head_sha is not None:
            raise CiHeadGateError("--head-sha cannot be used with --evaluate-pr-checks")
        return run_evaluate_pr_checks(sys.stdin, sys.stdout, sys.stderr)
    if args.report_pr_heads is not None:
        if args.head_sha is not None:
            raise CiHeadGateError("--head-sha cannot be used with --report-pr-heads")
        if args.report_pr_heads <= 0:
            raise CiHeadGateError("--report-pr-heads must be a positive integer")
        return report_pr_head_history(
            repo=args.repo,
            pr_number=args.report_pr_heads,
            stream=sys.stdout,
        )
    if args.limit <= 0:
        raise CiHeadGateError("--limit must be a positive integer")
    if args.pr is not None and args.head_sha is not None:
        raise CiHeadGateError("--head-sha can only be used with --branch")
    if args.pr is not None:
        if args.pr <= 0:
            raise CiHeadGateError("--pr must be a positive integer")
        return check_pr_head_ci(
            repo=args.repo,
            pr_number=args.pr,
            remote=args.remote,
            limit=args.limit,
            stream=sys.stdout,
        )

    if args.head_sha is not None:
        head_sha = args.head_sha.lower()
        if not _is_sha(head_sha):
            raise CiHeadGateError("--head-sha must be a full 40-character hex SHA")
        head_source = "from explicit --head-sha"
    else:
        head_sha = remote_head_sha(args.remote, args.branch)
        head_source = f"from git ls-remote {args.remote} refs/heads/{args.branch}"

    return check_branch_head_ci(
        repo=args.repo,
        branch=args.branch,
        head_sha=head_sha,
        limit=args.limit,
        stream=sys.stdout,
        head_source=head_source,
    )


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except CiHeadGateError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(2)
