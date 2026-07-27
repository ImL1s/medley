#!/usr/bin/env python3
"""Publish the audited Grok-build backlog to GitHub Issues.

Dry-run is the default. Mutations require --apply.
The manifest may be plain JSON or a base64-encoded gzip payload.
The script is dependency-free except for an authenticated `gh` CLI when applying.
"""

from __future__ import annotations

import argparse
import base64
import gzip
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any


DEFAULT_MANIFEST = Path(".github/audit/2026-07-28-issues.json.gz.b64")


class PublishError(RuntimeError):
    pass


def run_gh(
    args: list[str],
    *,
    check: bool = True,
    capture_output: bool = True,
) -> subprocess.CompletedProcess[str]:
    command = ["gh", *args]
    result = subprocess.run(
        command,
        check=False,
        text=True,
        capture_output=capture_output,
    )
    if check and result.returncode != 0:
        stderr = (result.stderr or "").strip()
        stdout = (result.stdout or "").strip()
        detail = stderr or stdout or f"exit code {result.returncode}"
        raise PublishError(f"`{' '.join(command)}` failed: {detail}")
    return result


def read_manifest_bytes(path: Path) -> bytes:
    try:
        raw = path.read_bytes()
    except FileNotFoundError as exc:
        raise PublishError(f"Manifest not found: {path}") from exc

    if path.name.endswith(".gz.b64"):
        try:
            return gzip.decompress(base64.b64decode(raw, validate=True))
        except (ValueError, gzip.BadGzipFile) as exc:
            raise PublishError(f"Invalid base64/gzip manifest {path}: {exc}") from exc
    return raw


def load_manifest(path: Path) -> dict[str, Any]:
    try:
        data = json.loads(read_manifest_bytes(path).decode("utf-8"))
    except UnicodeDecodeError as exc:
        raise PublishError(f"Manifest is not UTF-8 after decoding: {path}") from exc
    except json.JSONDecodeError as exc:
        raise PublishError(f"Invalid JSON in {path}: {exc}") from exc

    if data.get("schema_version") != 1:
        raise PublishError(
            f"Unsupported schema_version: {data.get('schema_version')!r}; expected 1"
        )

    labels = data.get("labels")
    issues = data.get("issues")
    if not isinstance(labels, dict) or not isinstance(issues, list):
        raise PublishError("Manifest must contain object `labels` and array `issues`")

    seen_ids: set[str] = set()
    seen_titles: set[str] = set()
    for index, issue in enumerate(issues, start=1):
        if not isinstance(issue, dict):
            raise PublishError(f"Issue #{index} is not an object")
        issue_id = issue.get("id")
        title = issue.get("title")
        body = issue.get("body")
        issue_labels = issue.get("labels")
        if not isinstance(issue_id, str) or not issue_id:
            raise PublishError(f"Issue #{index} has no valid `id`")
        if not isinstance(title, str) or not title:
            raise PublishError(f"{issue_id} has no valid `title`")
        if not isinstance(body, str) or not body.strip():
            raise PublishError(f"{issue_id} has no valid `body`")
        if not isinstance(issue_labels, list) or not all(
            isinstance(label, str) for label in issue_labels
        ):
            raise PublishError(f"{issue_id} has invalid `labels`")
        if issue_id in seen_ids:
            raise PublishError(f"Duplicate issue id: {issue_id}")
        if title in seen_titles:
            raise PublishError(f"Duplicate issue title: {title}")
        seen_ids.add(issue_id)
        seen_titles.add(title)
        unknown = sorted(set(issue_labels) - set(labels))
        if unknown:
            raise PublishError(f"{issue_id} references undefined labels: {unknown}")

    return data


def render_markdown(manifest: dict[str, Any]) -> str:
    lines = [
        f"# Grok-build issue backlog — {manifest.get('audit_date', 'unknown date')}",
        "",
        f"Repository: `{manifest.get('repository', '')}`  ",
        f"Audited branch: `{manifest.get('audited_branch', '')}`  ",
        f"Audited commit: `{manifest.get('audited_commit', '')}`",
        "",
    ]
    for issue in manifest["issues"]:
        labels = ", ".join(f"`{label}`" for label in issue["labels"])
        lines.extend(
            [
                f"## {issue['id']} — {issue['title']}",
                "",
                f"**Labels:** {labels}",
                "",
                issue["body"].rstrip(),
                "",
                "---",
                "",
            ]
        )
    return "\n".join(lines).rstrip() + "\n"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Create/update the audited Grok-build labels and issues. "
            "Dry-run unless --apply is supplied."
        )
    )
    parser.add_argument(
        "--manifest",
        type=Path,
        default=DEFAULT_MANIFEST,
        help=f"Manifest path (default: {DEFAULT_MANIFEST})",
    )
    parser.add_argument(
        "--repo",
        help="GitHub repository in OWNER/REPO form; defaults to manifest.repository",
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="Perform mutations. Without this flag, print the plan only.",
    )
    parser.add_argument(
        "--enable-issues",
        action="store_true",
        help="Enable the repository Issues feature before publishing (admin required).",
    )
    parser.add_argument(
        "--update-existing",
        action="store_true",
        help=(
            "Replace the body and add missing manifest labels when an exact-title "
            "issue already exists. Default is to leave existing issues unchanged."
        ),
    )
    parser.add_argument(
        "--only",
        action="append",
        default=[],
        metavar="GB-NNN",
        help="Publish only the given audit ID; repeatable.",
    )
    parser.add_argument(
        "--skip-labels",
        action="store_true",
        help="Do not create/update label definitions.",
    )
    parser.add_argument(
        "--dump-markdown",
        type=Path,
        metavar="PATH",
        help="Write a human-readable copy of every issue body before doing anything else.",
    )
    parser.add_argument(
        "--dump-json",
        type=Path,
        metavar="PATH",
        help="Write the decoded canonical JSON manifest before doing anything else.",
    )
    return parser.parse_args()


def require_gh_auth() -> None:
    if shutil.which("gh") is None:
        raise PublishError(
            "`gh` was not found. Install GitHub CLI and run `gh auth login` first."
        )
    run_gh(["auth", "status"])


def repository_has_issues(repo: str) -> bool:
    result = run_gh(["api", f"repos/{repo}", "--jq", ".has_issues"])
    return result.stdout.strip().lower() == "true"


def enable_issues(repo: str, *, apply: bool) -> None:
    print(f"{'[apply]' if apply else '[dry-run]'} enable Issues on {repo}")
    if not apply:
        return
    run_gh(
        [
            "api",
            "-X",
            "PATCH",
            f"repos/{repo}",
            "-F",
            "has_issues=true",
            "--silent",
        ]
    )
    if not repository_has_issues(repo):
        raise PublishError("Repository API did not report has_issues=true after PATCH")


def sync_labels(
    repo: str,
    label_defs: dict[str, Any],
    *,
    apply: bool,
) -> None:
    for name, spec in label_defs.items():
        color = str(spec.get("color", "")).lstrip("#")
        description = str(spec.get("description", ""))
        print(
            f"{'[apply]' if apply else '[dry-run]'} label {name!r} "
            f"color={color} description={description!r}"
        )
        if not apply:
            continue
        run_gh(
            [
                "label",
                "create",
                name,
                "--repo",
                repo,
                "--color",
                color,
                "--description",
                description,
                "--force",
            ]
        )


def list_existing_issues(repo: str) -> dict[str, dict[str, Any]]:
    result = run_gh(
        [
            "issue",
            "list",
            "--repo",
            repo,
            "--state",
            "all",
            "--limit",
            "1000",
            "--json",
            "number,title,url",
        ]
    )
    try:
        rows = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise PublishError(f"Could not parse `gh issue list` JSON: {exc}") from exc
    return {row["title"]: row for row in rows}


def write_body_file(body: str) -> Path:
    handle = tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        suffix=".md",
        prefix="grok-build-issue-",
        delete=False,
    )
    try:
        handle.write(body)
        handle.flush()
        return Path(handle.name)
    finally:
        handle.close()


def create_issue(repo: str, issue: dict[str, Any]) -> str:
    body_file = write_body_file(issue["body"])
    try:
        args = [
            "issue",
            "create",
            "--repo",
            repo,
            "--title",
            issue["title"],
            "--body-file",
            str(body_file),
        ]
        for label in issue["labels"]:
            args.extend(["--label", label])
        result = run_gh(args)
        return result.stdout.strip()
    finally:
        body_file.unlink(missing_ok=True)


def update_issue(repo: str, number: int, issue: dict[str, Any]) -> None:
    body_file = write_body_file(issue["body"])
    try:
        args = [
            "issue",
            "edit",
            str(number),
            "--repo",
            repo,
            "--body-file",
            str(body_file),
        ]
        for label in issue["labels"]:
            args.extend(["--add-label", label])
        run_gh(args)
    finally:
        body_file.unlink(missing_ok=True)


def select_issues(
    all_issues: list[dict[str, Any]], only_ids: list[str]
) -> list[dict[str, Any]]:
    if not only_ids:
        return all_issues
    wanted = set(only_ids)
    selected = [issue for issue in all_issues if issue["id"] in wanted]
    missing = sorted(wanted - {issue["id"] for issue in selected})
    if missing:
        raise PublishError(f"Unknown --only issue IDs: {', '.join(missing)}")
    return selected


def main() -> int:
    args = parse_args()
    manifest = load_manifest(args.manifest)

    if args.dump_markdown:
        args.dump_markdown.parent.mkdir(parents=True, exist_ok=True)
        args.dump_markdown.write_text(render_markdown(manifest), encoding="utf-8")
        print(f"Wrote {args.dump_markdown}")
    if args.dump_json:
        args.dump_json.parent.mkdir(parents=True, exist_ok=True)
        args.dump_json.write_text(
            json.dumps(manifest, indent=2, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )
        print(f"Wrote {args.dump_json}")

    repo = args.repo or manifest.get("repository")
    if not isinstance(repo, str) or "/" not in repo:
        raise PublishError("Repository must be supplied as OWNER/REPO")

    selected = select_issues(manifest["issues"], args.only)

    print(f"Manifest: {args.manifest}")
    print(f"Repository: {repo}")
    print(f"Mode: {'APPLY' if args.apply else 'DRY RUN'}")
    print(f"Selected issues: {len(selected)} / {len(manifest['issues'])}")

    if not args.apply:
        if args.enable_issues:
            enable_issues(repo, apply=False)
        if not args.skip_labels:
            sync_labels(repo, manifest["labels"], apply=False)
        for issue in selected:
            print(
                f"[dry-run] {issue['id']} create/update exact title "
                f"{issue['title']!r} labels={issue['labels']}"
            )
        print("\nNo mutations performed. Re-run with --apply after reviewing the plan.")
        return 0

    require_gh_auth()

    if args.enable_issues:
        enable_issues(repo, apply=True)
    elif not repository_has_issues(repo):
        raise PublishError(
            "GitHub Issues is disabled. Re-run with --enable-issues --apply "
            "from a repository administrator account."
        )

    if not args.skip_labels:
        sync_labels(repo, manifest["labels"], apply=True)

    existing = list_existing_issues(repo)
    created: list[tuple[str, str]] = []
    updated: list[tuple[str, str]] = []
    skipped: list[tuple[str, str]] = []

    for issue in selected:
        row = existing.get(issue["title"])
        if row is not None:
            if args.update_existing:
                print(
                    f"[apply] {issue['id']} update existing "
                    f"#{row['number']} {issue['title']}"
                )
                update_issue(repo, int(row["number"]), issue)
                updated.append((issue["id"], row["url"]))
            else:
                print(
                    f"[skip] {issue['id']} exact-title issue already exists: "
                    f"#{row['number']} {row['url']}"
                )
                skipped.append((issue["id"], row["url"]))
            continue

        print(f"[apply] {issue['id']} create {issue['title']}")
        url = create_issue(repo, issue)
        created.append((issue["id"], url))
        existing[issue["title"]] = {"number": -1, "title": issue["title"], "url": url}

    print("\nResult")
    print(f"  created: {len(created)}")
    print(f"  updated: {len(updated)}")
    print(f"  skipped: {len(skipped)}")
    for issue_id, url in [*created, *updated, *skipped]:
        print(f"  {issue_id}: {url}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except PublishError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(2)
