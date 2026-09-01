#!/usr/bin/env python3
"""Fail closed when a target branch head has no successful CI run (issue #202).

Why this exists:
- A push can land on the remote with no workflow run created at all.
- `gh pr checks` prints "no checks reported" in that state, which has been
  treated as success/absent rather than fail-closed.
- Queued/in-progress, skipped, and zero-run are different states; only the
  last one is `absent`.
- For pull_request runs, GitHub's SHA fields are inconsistent in practice:
  sometimes `head_sha` is the PR head, sometimes it is the synthetic merge
  commit Actions checked out. `pull_requests[].head.sha` is rewritten to the
  live PR tip and is not a receipt.
- The reliable question is: does this branch have a ci.yml run whose recorded
  `head_sha` is the requested commit, or a merge commit whose git parents
  include that commit.

This script is a pre-merge guard meant to run on a developer machine (or merge
orchestrator), not inside CI itself. `--evaluate-pr-checks` is the fail-closed
wrapper for `gh pr checks --json` used by `scripts/merge-pr.sh`.
`--report-pr-heads` reconciles every commit in a PR with its check runs so a
cancelled historical run cannot be mistaken for a still-pending current run.
"""

from __future__ import annotations

import argparse
import json
import re
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
CHECK_RUN_ACTIVE_STATUSES = frozenset(
    {"queued", "in_progress", "waiting", "requested", "pending"}
)
CHECK_RUN_CONCLUSIONS = frozenset(
    {
        "action_required",
        "cancelled",
        "failure",
        "neutral",
        "skipped",
        "stale",
        "startup_failure",
        "success",
        "timed_out",
    }
)


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
    try:
        owner, name = repo.split("/", 1)
    except ValueError as exc:
        raise CiHeadGateError("repository must use OWNER/REPO form") from exc
    if not _printable_label(owner) or not _printable_label(name) or "/" in name:
        raise CiHeadGateError("repository must use OWNER/REPO form")

    # The REST `pulls/{number}/commits` endpoint has a hard 250-commit cap even
    # when pagination is requested. The GraphQL commits connection is cursor
    # paginated, so large synchronization PRs still include their current head.
    query = """
query($owner:String!,$name:String!,$number:Int!,$endCursor:String) {
  repository(owner:$owner,name:$name) {
    pullRequest(number:$number) {
      commits(first:100,after:$endCursor) {
        totalCount
        nodes { commit { oid } }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
""".strip()
    result = run_command(
        [
            "gh",
            "api",
            "graphql",
            "--paginate",
            "--slurp",
            "-F",
            f"owner={owner}",
            "-F",
            f"name={name}",
            "-F",
            f"number={pr_number}",
            "-f",
            f"query={query}",
        ]
    )
    try:
        pages = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise CiHeadGateError(
            "`gh api graphql --paginate --slurp` did not return valid JSON"
        ) from exc
    if not isinstance(pages, list):
        raise CiHeadGateError("PR commits GraphQL API returned a non-array page list")
    if not pages:
        raise CiHeadGateError("PR commits GraphQL API returned no pages")

    shas: list[str] = []
    seen: set[str] = set()
    total_count: int | None = None
    for page_index, page in enumerate(pages):
        try:
            pull_request = page["data"]["repository"]["pullRequest"]
            connection = pull_request["commits"]
            nodes = connection["nodes"]
            page_info = connection["pageInfo"]
            page_total = connection["totalCount"]
        except (KeyError, TypeError) as exc:
            raise CiHeadGateError(
                "PR commits GraphQL API returned a malformed page"
            ) from exc
        if not isinstance(nodes, list) or not isinstance(page_info, dict):
            raise CiHeadGateError("PR commits GraphQL API returned a malformed page")
        if (
            not isinstance(page_total, int)
            or isinstance(page_total, bool)
            or page_total < 0
        ):
            raise CiHeadGateError(
                "PR commits GraphQL API returned a malformed totalCount"
            )
        if total_count is None:
            total_count = page_total
        elif page_total != total_count:
            raise CiHeadGateError(
                "PR commits GraphQL API returned inconsistent totalCount values"
            )
        has_next_page = page_info.get("hasNextPage")
        end_cursor = page_info.get("endCursor")
        if not isinstance(has_next_page, bool):
            raise CiHeadGateError(
                "PR commits GraphQL API returned malformed pageInfo"
            )
        is_last_page = page_index == len(pages) - 1
        if has_next_page == is_last_page:
            raise CiHeadGateError(
                "PR commits GraphQL pagination ended at an inconsistent page"
            )
        if has_next_page and not _printable_label(end_cursor):
            raise CiHeadGateError(
                "PR commits GraphQL API returned a malformed endCursor"
            )
        for row in nodes:
            if not isinstance(row, dict) or not isinstance(row.get("commit"), dict):
                raise CiHeadGateError(
                    "PR commits GraphQL API returned a non-object commit"
                )
            sha = row["commit"].get("oid")
            if not isinstance(sha, str) or not _is_sha(sha):
                raise CiHeadGateError(
                    "PR commits GraphQL API returned a malformed commit SHA"
                )
            sha = sha.lower()
            if sha in seen:
                raise CiHeadGateError(
                    f"PR commits GraphQL API repeated commit SHA {sha}"
                )
            seen.add(sha)
            shas.append(sha)

    if not shas:
        raise CiHeadGateError("PR commits GraphQL API returned no commits")
    if total_count != len(shas):
        raise CiHeadGateError(
            "PR commits GraphQL totalCount does not match the paginated commits "
            f"({total_count} != {len(shas)})"
        )
    return shas


def _check_run_state(row: dict[str, Any]) -> str:
    status = row.get("status")
    if not isinstance(status, str) or status != status.strip():
        raise CiHeadGateError("check-runs API returned a malformed status")
    status = status.lower()
    if status != "completed":
        if status not in CHECK_RUN_ACTIVE_STATUSES:
            raise CiHeadGateError(
                f"check-runs API returned an unknown status {status!r}"
            )
        return status

    conclusion = row.get("conclusion")
    if not isinstance(conclusion, str) or conclusion != conclusion.strip():
        raise CiHeadGateError(
            "check-runs API returned a completed run without a conclusion"
        )
    conclusion = conclusion.lower()
    if conclusion not in CHECK_RUN_CONCLUSIONS:
        raise CiHeadGateError(
            f"check-runs API returned an unknown conclusion {conclusion!r}"
        )
    return conclusion


def _plain_positive_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def _printable_label(value: Any) -> bool:
    return (
        isinstance(value, str)
        and bool(value)
        and value == value.strip()
        and value.isprintable()
    )


def _check_run_identity(row: dict[str, Any]) -> tuple[str, int, int]:
    """Stable identity shared by reruns of one check-suite check."""

    name = row.get("name")
    app = row.get("app")
    suite = row.get("check_suite")
    if not _printable_label(name):
        raise CiHeadGateError("check-runs API returned an unsafe check run name")
    if not isinstance(app, dict) or not _plain_positive_int(app.get("id")):
        raise CiHeadGateError("check-runs API returned a malformed app identity")
    if not _printable_label(app.get("slug")):
        raise CiHeadGateError("check-runs API returned an unsafe app slug")
    if not isinstance(suite, dict) or not _plain_positive_int(suite.get("id")):
        raise CiHeadGateError(
            "check-runs API returned a malformed check-suite identity"
        )
    return str(name), int(app["id"]), int(suite["id"])


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
        if (
            not isinstance(page_total, int)
            or isinstance(page_total, bool)
            or page_total < 0
        ):
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
            run_sha = row.get("head_sha")
            if not _plain_positive_int(run_id):
                raise CiHeadGateError("check-runs API returned a malformed check run ID")
            if run_id in seen_ids:
                raise CiHeadGateError(f"check-runs API repeated check run ID {run_id}")
            if not isinstance(run_sha, str) or not _is_sha(run_sha):
                raise CiHeadGateError("check-runs API returned a malformed head SHA")
            if run_sha.lower() != sha:
                raise CiHeadGateError(
                    f"check run {run_id} belongs to a different head SHA"
                )
            _check_run_identity(row)
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
        attempts_by_identity: dict[
            tuple[str, int, int], list[dict[str, Any]]
        ] = {}
        for row in runs:
            identity = _check_run_identity(row)
            attempts_by_identity.setdefault(identity, []).append(row)
        for name, app_id, suite_id in sorted(attempts_by_identity):
            attempts = sorted(
                attempts_by_identity[(name, app_id, suite_id)],
                key=lambda row: int(row["id"]),
            )
            states = [_check_run_state(row) for row in attempts]
            run_ids = [int(row["id"]) for row in attempts]
            suffix = f"run={run_ids[-1]}"
            if len(states) > 1:
                chain = " -> ".join(
                    f"{run_id}={state}"
                    for run_id, state in zip(run_ids, states, strict=True)
                )
                suffix += f" attempts: {chain}"
            app_slug = str(attempts[-1]["app"]["slug"])
            print(
                f"  {name}: {states[-1]} "
                f"[{suffix}; app={app_slug}#{app_id}; suite={suite_id}]",
                file=stream,
            )

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


def list_ci_runs(
    repo: str,
    event: str,
    limit: int,
    *,
    branch: str | None = None,
    commit: str | None = None,
) -> list[dict[str, Any]]:
    """List ci.yml runs for a branch or an exact commit.

    Pull-request CI in this repository is recorded against the synthetic
    merge commit, so `--commit $PR_HEAD` returns no run. Callers list
    pull_request rows by `--branch` and then associate each row with the
    requested head via immutable `headSha` / git parents. Do not treat
    `pull_requests[].head.sha` as a receipt: GitHub rewrites it to the
    live tip.
    """

    args = [
        "run",
        "list",
        "--repo",
        repo,
        "--workflow",
        "ci.yml",
        "--event",
        event,
        "--limit",
        str(limit),
        "--json",
        RUN_LIST_FIELDS,
    ]
    if commit is not None:
        if not _is_sha(commit):
            raise CiHeadGateError("commit SHA is missing or malformed")
        args.extend(["--commit", commit])
    elif branch:
        args.extend(["--branch", branch])
    else:
        raise CiHeadGateError("list_ci_runs requires a branch or commit")
    data = gh_json(args)
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


def commit_parent_shas(repo: str, sha: str) -> set[str]:
    """Direct git parents of `sha`. Merge-commit CI uses these, not `--commit`.

    Actions sometimes records a synthetic merge SHA that GitHub later GC's.
    Treat a missing commit as "no parents" so historical probes stay
    fail-closed on identity without aborting the whole gate.
    """

    return commit_parents_and_message(repo, sha)[0]


def commit_parents_and_message(repo: str, sha: str) -> tuple[set[str], str]:
    """`(parents, first-line message)` for `sha`."""

    parents, message, _github = commit_parents_message_and_github_proof(repo, sha)
    return parents, message


def commit_is_github_synthetic_merge_proof(data: dict[str, Any]) -> bool:
    """True when GitHub itself authored a verified Actions synthetic merge.

    Message + parent shape alone is forgeable (#530 review). Require the
    commit API's GitHub-verified signature and the `GitHub` /
    `noreply@github.com` committer identity Actions uses for
    `Merge <head> into <base>` checkout commits.
    """

    commit = data.get("commit")
    if not isinstance(commit, dict):
        return False
    verification = commit.get("verification")
    if not isinstance(verification, dict) or verification.get("verified") is not True:
        return False
    committer = commit.get("committer")
    if not isinstance(committer, dict):
        return False
    name = committer.get("name")
    email = committer.get("email")
    if not isinstance(name, str) or not isinstance(email, str):
        return False
    return name == "GitHub" and email.lower() == "noreply@github.com"


def commit_parents_message_and_github_proof(
    repo: str, sha: str
) -> tuple[set[str], str, bool]:
    """`(parents, first-line message, github_synthetic_proof)` for `sha`."""

    if not _is_sha(sha):
        raise CiHeadGateError("commit SHA is missing or malformed")
    result = run_command(
        ["gh", "api", f"repos/{repo}/commits/{sha}"],
        check=False,
    )
    if result.returncode != 0:
        detail = ((result.stderr or result.stdout) or "").lower()
        if "http 404" in detail or "http 422" in detail or "no commit found" in detail:
            return set(), "", False
        raise CiHeadGateError(
            f"`gh api repos/{repo}/commits/{sha}` failed: "
            f"{(result.stderr or result.stdout or f'exit {result.returncode}').strip()}"
        )
    try:
        data = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise CiHeadGateError(
            f"commit {sha} response is not valid JSON"
        ) from exc
    if not isinstance(data, dict):
        raise CiHeadGateError(f"commit {sha} is not an object")
    parents_raw = data.get("parents")
    out: set[str] = set()
    if isinstance(parents_raw, list):
        for parent in parents_raw:
            if not isinstance(parent, dict):
                continue
            parent_sha = parent.get("sha")
            if isinstance(parent_sha, str) and _is_sha(parent_sha):
                out.add(parent_sha.lower())
    message = ""
    commit = data.get("commit")
    if isinstance(commit, dict):
        raw_msg = commit.get("message")
        if isinstance(raw_msg, str) and raw_msg:
            message = raw_msg.splitlines()[0].strip()
    return out, message, commit_is_github_synthetic_merge_proof(data)


_ACTIONS_SYNTHETIC_MERGE = re.compile(
    r"(?i)^Merge ([0-9a-f]{40}) into ([0-9a-f]{40})\s*$"
)


def run_head_matches_requested(
    *,
    repo: str,
    recorded_sha: object,
    requested_sha: str,
    parent_cache: dict[str, set[str]],
    message_cache: dict[str, str] | None = None,
    github_proof_cache: dict[str, bool] | None = None,
    pull_merge_sha: str | None = None,
    require_pull_merge: bool = False,
) -> bool:
    """True when a run's immutable head is `requested_sha` or merges it.

    Exact `head_sha` match covers runs recorded against the PR head.
    Parent association covers only the live Actions synthetic merge at
    `refs/pull/<n>/merge` (`Merge <head> into <base>`, two parents, GitHub
    verified). When `require_pull_merge` is set (merge-pr / `--pr` probes),
    an unavailable or absent merge ref fails closed for parent association
    rather than skipping the binding (#530 review).
    """

    if not isinstance(recorded_sha, str) or not _is_sha(recorded_sha):
        return False
    recorded = recorded_sha.lower()
    requested = requested_sha.lower()
    if recorded == requested:
        return True
    if require_pull_merge:
        if (
            pull_merge_sha is None
            or not _is_sha(pull_merge_sha)
            or recorded != pull_merge_sha.lower()
        ):
            return False
    elif pull_merge_sha is not None:
        if not _is_sha(pull_merge_sha) or recorded != pull_merge_sha.lower():
            return False
    parents = parent_cache.get(recorded)
    messages = message_cache if message_cache is not None else {}
    proofs = github_proof_cache if github_proof_cache is not None else {}
    message = messages.get(recorded)
    proof = proofs.get(recorded)
    if parents is None or message is None or proof is None:
        fetched_parents, fetched_message, fetched_proof = (
            commit_parents_message_and_github_proof(repo, recorded)
        )
        if parents is None:
            parents = fetched_parents
            parent_cache[recorded] = parents
        if message is None:
            message = fetched_message
            messages[recorded] = message
        if proof is None:
            proof = fetched_proof
            proofs[recorded] = proof
    if not proof:
        return False
    if requested not in parents or len(parents) != 2:
        return False
    match = _ACTIONS_SYNTHETIC_MERGE.match(message or "")
    if match is None:
        return False
    if match.group(1).lower() != requested:
        return False
    base = match.group(2).lower()
    return base in parents


def pull_request_merge_sha(remote: str, pr_number: int) -> str | None:
    """Current `refs/pull/<n>/merge` object, or None when the ref is absent.

    A failed `git ls-remote` is not treated as absence — it raises so the
    merge gate cannot silently drop the synthetic-merge binding (#530).
    """

    if pr_number <= 0:
        raise CiHeadGateError("--pr must be a positive integer")
    result = run_command(
        ["git", "ls-remote", remote, f"refs/pull/{pr_number}/merge"],
        check=False,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or f"exit {result.returncode}").strip()
        raise CiHeadGateError(
            f"`git ls-remote {remote} refs/pull/{pr_number}/merge` failed: {detail}"
        )
    line = (result.stdout or "").splitlines()[:1]
    if not line:
        return None
    sha = line[0].split("\t", 1)[0].strip().lower()
    return sha if _is_sha(sha) else None

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
    """Immutable SHA identity of a pull_request run.

    `head_sha` is frozen at creation (PR head or the merge commit CI
    checked out). `pull_requests[].head.sha` is rewritten to the live PR
    tip and must not be used as a receipt for a later head.
    """

    candidates: set[str] = set()
    head_sha = detail.get("head_sha")
    if isinstance(head_sha, str) and _is_sha(head_sha):
        candidates.add(head_sha.lower())
    _ = branch
    return candidates


def matches_pull_request_gate_shape(detail: dict[str, Any], *, branch: str) -> bool:
    return (
        detail.get("event") == "pull_request"
        and detail.get("head_branch") == branch
        and detail.get("path") == CI_WORKFLOW_PATH
        and detail.get("status") == "completed"
        and detail.get("conclusion") == "success"
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
    pull_merge_sha: str | None = None,
    require_pull_merge: bool = False,
) -> int:
    if not _is_sha(head_sha):
        raise CiHeadGateError("target head SHA is missing or malformed")

    event = expected_ci_event(branch)
    print(
        f"Branch `{branch}` target head: {_short_sha(head_sha)} ({head_source})",
        file=stream,
    )

    runs = list_ci_runs(repo, event, limit, branch=branch)
    details_by_id: dict[int, dict[str, Any]] = {}
    parent_cache: dict[str, set[str]] = {}
    message_cache: dict[str, str] = {}
    github_proof_cache: dict[str, bool] = {}

    def detail_for(run_id: int) -> dict[str, Any]:
        detail = details_by_id.get(run_id)
        if detail is None:
            detail = run_detail(repo, run_id)
            details_by_id[run_id] = detail
        return detail

    # Prefer exact headSha matches first so a current successful receipt does
    # not pay for parent lookups on up to `--limit` historical merge SHAs
    # (#530 review). Only fall through to parent association when needed.
    head_runs: list[dict[str, Any]] = []
    verified_success: list[dict[str, Any]] = []
    rejected_success: list[int] = []

    def verify_row(row: dict[str, Any]) -> bool | None:
        """Return True/False once verified, or None when the row is not a success candidate."""
        if row.get("status") != "completed" or row.get("conclusion") != "success":
            return None
        run_id = row.get("databaseId")
        if not isinstance(run_id, int):
            return None
        detail = detail_for(run_id)
        if event == "push":
            is_match = matches_release_gate_shape(detail, sha=head_sha, branch=branch)
        else:
            is_match = matches_pull_request_gate_shape(
                detail, branch=branch
            ) and run_head_matches_requested(
                repo=repo,
                recorded_sha=detail.get("head_sha"),
                requested_sha=head_sha,
                parent_cache=parent_cache,
                message_cache=message_cache,
                github_proof_cache=github_proof_cache,
                pull_merge_sha=pull_merge_sha,
                require_pull_merge=require_pull_merge,
            )
        if is_match:
            verified_success.append(row)
            return True
        rejected_success.append(run_id)
        return False

    if event == "push":
        head_runs = [row for row in runs if row.get("headSha") == head_sha]
        for row in head_runs:
            verify_row(row)
    else:
        exact_rows = [row for row in runs if row.get("headSha") == head_sha]
        head_runs.extend(exact_rows)
        for row in exact_rows:
            if verify_row(row) is True:
                break
        if not verified_success:
            for row in runs:
                if row.get("headSha") == head_sha:
                    continue
                if not run_head_matches_requested(
                    repo=repo,
                    recorded_sha=row.get("headSha"),
                    requested_sha=head_sha,
                    parent_cache=parent_cache,
                    message_cache=message_cache,
                    github_proof_cache=github_proof_cache,
                ):
                    continue
                head_runs.append(row)
                if verify_row(row) is True:
                    break

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
        "       Probe rationale: uses `gh run list --branch` plus immutable "
        "head_sha / merge-commit parents for pull_request heads, and "
        "`--branch` for providers push; it intentionally avoids "
        "`gh pr checks` and rewritten `pull_requests[].head.sha`.",
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
        pull_merge_sha=pull_request_merge_sha(remote, pr_number),
        require_pull_merge=True,
    )


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    # `--pr` may accompany `--branch --head-sha` so merge-pr can bind
    # `refs/pull/<n>/merge` while probing an explicit tip (#530). Keep the
    # other modes exclusive of each other.
    parser.add_argument(
        "--pr",
        type=int,
        help=(
            "Pull request number. Alone: probe that PR's head. "
            "With --branch: also bind refs/pull/<n>/merge for parent CI."
        ),
    )
    target = parser.add_mutually_exclusive_group(required=False)
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
    mode_count = sum(
        [
            bool(args.evaluate_pr_checks),
            args.report_pr_heads is not None,
            args.branch is not None,
            args.pr is not None
            and args.branch is None
            and args.report_pr_heads is None
            and not args.evaluate_pr_checks,
        ]
    )
    if mode_count != 1:
        raise CiHeadGateError(
            "specify exactly one of --pr (alone), --branch, "
            "--report-pr-heads, or --evaluate-pr-checks"
        )
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
    if args.pr is not None and args.head_sha is not None and args.branch is None:
        raise CiHeadGateError("--head-sha with --pr also requires --branch")
    if args.pr is not None and args.head_sha is None and args.branch is None:
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

    pull_merge_sha = None
    require_pull_merge = False
    if args.pr is not None:
        if args.pr <= 0:
            raise CiHeadGateError("--pr must be a positive integer")
        pull_merge_sha = pull_request_merge_sha(args.remote, args.pr)
        require_pull_merge = True

    return check_branch_head_ci(
        repo=args.repo,
        branch=args.branch,
        head_sha=head_sha,
        limit=args.limit,
        stream=sys.stdout,
        head_source=head_source,
        pull_merge_sha=pull_merge_sha,
        require_pull_merge=require_pull_merge,
    )


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except CiHeadGateError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(2)
