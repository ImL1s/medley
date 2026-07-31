#!/usr/bin/env python3
"""Publish the audited Grok-build backlog to GitHub Issues.

The checked-in manifest is plain JSON so reviewers and CI can inspect it directly.
Dry-run is the default. GitHub mutations require an explicit ``--apply``.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
from datetime import date
from pathlib import Path
from typing import Any


DEFAULT_MANIFEST = Path(".github/audit/2026-07-31-issues.json")
ISSUE_ID_RE = re.compile(r"^GB-[0-9]{3}$")
REPOSITORY_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9_.-]+$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
LABEL_COLOR_RE = re.compile(r"^[0-9a-fA-F]{6}$")
AUDIT_DATE_RE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")
AUDIT_MARKER_RE = re.compile(r"<!--\s*grok-build-audit-id:\s*(GB-[0-9]{3})\s*-->")
MANIFEST_KEYS = {
    "schema_version",
    "repository",
    "audit_date",
    "audited_branch",
    "audited_commit",
    "upstream_audit_target",
    "source_path",
    "notes",
    "labels",
    "issues",
}


class PublishError(RuntimeError):
    """Raised when validation or a GitHub operation fails."""


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


def _required_string(data: dict[str, Any], key: str) -> str:
    value = data.get(key)
    if not isinstance(value, str) or not value.strip():
        raise PublishError(f"Manifest `{key}` must be a non-empty string")
    if value != value.strip():
        raise PublishError(f"Manifest `{key}` must not have surrounding whitespace")
    return value


def validate_repository(repo: object) -> str:
    if not isinstance(repo, str) or REPOSITORY_RE.fullmatch(repo) is None:
        raise PublishError("Repository must be exactly OWNER/REPO")
    return repo


def load_manifest(path: Path) -> dict[str, Any]:
    try:
        raw = path.read_text(encoding="utf-8")
    except FileNotFoundError as exc:
        raise PublishError(f"Manifest not found: {path}") from exc
    except UnicodeDecodeError as exc:
        raise PublishError(f"Manifest is not UTF-8: {path}") from exc

    try:
        data = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise PublishError(f"Invalid JSON in {path}: {exc}") from exc
    if not isinstance(data, dict):
        raise PublishError("Manifest root must be an object")
    if data.get("schema_version") != 1:
        raise PublishError(
            f"Unsupported schema_version: {data.get('schema_version')!r}; expected 1"
        )
    unknown_root_keys = sorted(set(data) - MANIFEST_KEYS)
    if unknown_root_keys:
        raise PublishError(f"Manifest has unknown keys: {unknown_root_keys}")

    validate_repository(_required_string(data, "repository"))
    audit_date = _required_string(data, "audit_date")
    if AUDIT_DATE_RE.fullmatch(audit_date) is None:
        raise PublishError("Manifest `audit_date` must use YYYY-MM-DD")
    try:
        date.fromisoformat(audit_date)
    except ValueError as exc:
        raise PublishError("Manifest `audit_date` must use YYYY-MM-DD") from exc

    _required_string(data, "audited_branch")
    audited_commit = _required_string(data, "audited_commit")
    if COMMIT_RE.fullmatch(audited_commit) is None:
        raise PublishError("Manifest `audited_commit` must be a 40-character SHA")

    upstream_target = data.get("upstream_audit_target")
    if upstream_target is not None and (
        not isinstance(upstream_target, str)
        or COMMIT_RE.fullmatch(upstream_target) is None
    ):
        raise PublishError(
            "Manifest `upstream_audit_target` must be a 40-character SHA"
        )

    source_path = _required_string(data, "source_path")
    source = Path(source_path)
    if source.is_absolute() or ".." in source.parts:
        raise PublishError("Manifest `source_path` must be a repository-relative path")

    notes = data.get("notes", [])
    if not isinstance(notes, list) or not all(
        isinstance(note, str) and bool(note.strip()) for note in notes
    ):
        raise PublishError("Manifest `notes` must be an array of non-empty strings")

    labels = data.get("labels")
    issues = data.get("issues")
    if not isinstance(labels, dict) or not labels:
        raise PublishError("Manifest `labels` must be a non-empty object")
    if not isinstance(issues, list) or not issues:
        raise PublishError("Manifest `issues` must be a non-empty array")

    for name, spec in labels.items():
        if not isinstance(name, str) or not name.strip() or name != name.strip():
            raise PublishError("Every label name must be a non-empty trimmed string")
        if not isinstance(spec, dict):
            raise PublishError(f"Label {name!r} definition must be an object")
        unknown_keys = sorted(set(spec) - {"color", "description"})
        if unknown_keys:
            raise PublishError(f"Label {name!r} has unknown keys: {unknown_keys}")
        color = spec.get("color")
        description = spec.get("description")
        if not isinstance(color, str) or LABEL_COLOR_RE.fullmatch(color) is None:
            raise PublishError(
                f"Label {name!r} color must be exactly six hexadecimal characters"
            )
        if not isinstance(description, str):
            raise PublishError(f"Label {name!r} description must be a string")

    seen_ids: set[str] = set()
    seen_titles: set[str] = set()
    for index, issue in enumerate(issues, start=1):
        if not isinstance(issue, dict):
            raise PublishError(f"Issue #{index} is not an object")
        unknown_keys = sorted(set(issue) - {"id", "title", "body", "labels"})
        if unknown_keys:
            raise PublishError(f"Issue #{index} has unknown keys: {unknown_keys}")

        issue_id = issue.get("id")
        title = issue.get("title")
        body = issue.get("body")
        issue_labels = issue.get("labels")
        if not isinstance(issue_id, str) or ISSUE_ID_RE.fullmatch(issue_id) is None:
            raise PublishError(f"Issue #{index} `id` must match GB-NNN")
        if not isinstance(title, str) or not title.strip() or title != title.strip():
            raise PublishError(f"{issue_id} has no valid trimmed `title`")
        if not isinstance(body, str) or not body.strip():
            raise PublishError(f"{issue_id} has no valid `body`")
        if "grok-build-audit-id" in body:
            raise PublishError(
                f"{issue_id} body contains the publisher-reserved audit marker"
            )
        if not isinstance(issue_labels, list) or not issue_labels:
            raise PublishError(f"{issue_id} has invalid `labels`")
        if not all(
            isinstance(label, str) and bool(label.strip()) and label == label.strip()
            for label in issue_labels
        ):
            raise PublishError(f"{issue_id} labels must be non-empty trimmed strings")
        if len(set(issue_labels)) != len(issue_labels):
            raise PublishError(f"{issue_id} contains duplicate labels")
        if issue_id in seen_ids:
            raise PublishError(f"Duplicate issue id: {issue_id}")
        if title in seen_titles:
            raise PublishError(f"Duplicate issue title: {title}")
        seen_ids.add(issue_id)
        seen_titles.add(title)
        unknown_labels = sorted(set(issue_labels) - set(labels))
        if unknown_labels:
            raise PublishError(
                f"{issue_id} references undefined labels: {unknown_labels}"
            )

    ordered_ids = [issue["id"] for issue in issues]
    if ordered_ids != sorted(ordered_ids):
        raise PublishError("Manifest issues must be ordered by audit ID")

    return data


def audit_marker(issue_id: str) -> str:
    if ISSUE_ID_RE.fullmatch(issue_id) is None:
        raise PublishError(f"Invalid audit issue id for marker: {issue_id!r}")
    return f"<!-- grok-build-audit-id: {issue_id} -->"


def format_issue_body(manifest: dict[str, Any], issue: dict[str, Any]) -> str:
    """Append stable, managed audit metadata to an issue body."""

    footer = (
        f"_Audit source: `{issue['id']}` in `{manifest['source_path']}`, reviewed at "
        f"`{manifest['audited_branch']}@{manifest['audited_commit']}`._"
    )
    return (
        f"{issue['body'].rstrip()}\n\n---\n\n{footer}\n\n{audit_marker(issue['id'])}\n"
    )


def render_markdown(manifest: dict[str, Any]) -> str:
    lines = [
        f"# Grok-build issue backlog — {manifest['audit_date']}",
        "",
        f"Repository: `{manifest['repository']}`  ",
        f"Audited branch: `{manifest['audited_branch']}`  ",
        f"Audited commit: `{manifest['audited_commit']}`",
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
                format_issue_body(manifest, issue).rstrip(),
                "",
                "---",
                "",
            ]
        )
    return "\n".join(lines).rstrip() + "\n"


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Create or update the audited Grok-build labels and issues. "
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
        help=(
            "GitHub repository in OWNER/REPO form. If supplied, it must equal "
            "manifest.repository."
        ),
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="Perform mutations. Without this flag, print the plan only.",
    )
    parser.add_argument(
        "--validate-only",
        action="store_true",
        help="Validate the manifest without writing files or contacting GitHub.",
    )
    parser.add_argument(
        "--update-existing",
        action="store_true",
        help=(
            "Update title, body, and labels when a stable audit marker exists. "
            "Default is to leave marker-managed existing issues unchanged."
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
        help="Do not create or update label definitions.",
    )
    parser.add_argument(
        "--dump-markdown",
        type=Path,
        metavar="PATH",
        help="Write a human-readable copy of every managed issue body.",
    )
    return parser.parse_args(argv)


def require_gh_auth() -> None:
    if shutil.which("gh") is None:
        raise PublishError(
            "`gh` was not found. Install GitHub CLI and run `gh auth login` first."
        )
    run_gh(["auth", "status"])


def repository_has_issues(repo: str) -> bool:
    result = run_gh(["api", f"repos/{repo}", "--jq", ".has_issues"])
    value = result.stdout.strip().lower()
    if value not in {"true", "false"}:
        raise PublishError(
            f"Repository API returned invalid has_issues value: {value!r}"
        )
    return value == "true"


def sync_labels(
    repo: str,
    label_defs: dict[str, Any],
    *,
    apply: bool,
) -> None:
    for name, spec in label_defs.items():
        color = spec["color"]
        description = spec["description"]
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


def list_existing_issues(repo: str) -> list[dict[str, Any]]:
    """Read every issue through REST pagination and normalize identity fields."""

    result = run_gh(
        [
            "api",
            "--paginate",
            "--slurp",
            f"repos/{repo}/issues?state=all&per_page=100",
        ]
    )
    try:
        pages = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise PublishError(
            f"Could not parse paginated GitHub Issues JSON: {exc}"
        ) from exc
    if not isinstance(pages, list):
        raise PublishError("Paginated GitHub Issues response is not an array")

    rows: list[dict[str, Any]] = []
    for page_number, page in enumerate(pages, start=1):
        if not isinstance(page, list):
            raise PublishError(f"GitHub Issues page #{page_number} is not an array")
        for item_number, item in enumerate(page, start=1):
            if not isinstance(item, dict):
                raise PublishError(
                    f"GitHub Issues page #{page_number} item #{item_number} "
                    "is not an object"
                )
            # The REST issues endpoint also returns pull requests.
            if "pull_request" in item:
                continue

            raw_labels = item.get("labels")
            if not isinstance(raw_labels, list):
                raise PublishError(
                    f"GitHub issue #{item.get('number', '?')} has invalid labels"
                )
            labels: list[str] = []
            for raw_label in raw_labels:
                name = (
                    raw_label.get("name") if isinstance(raw_label, dict) else raw_label
                )
                if not isinstance(name, str) or not name.strip():
                    raise PublishError(
                        f"GitHub issue #{item.get('number', '?')} has invalid labels"
                    )
                labels.append(name)

            state = item.get("state")
            rows.append(
                {
                    "number": item.get("number"),
                    "title": item.get("title"),
                    "url": item.get("html_url"),
                    "body": item.get("body") or "",
                    "state": state.upper() if isinstance(state, str) else state,
                    "labels": labels,
                }
            )
    return rows


def extract_audit_id(body: object) -> str | None:
    if not isinstance(body, str):
        return None
    markers = AUDIT_MARKER_RE.findall(body)
    if len(markers) > 1:
        raise PublishError("Existing issue contains multiple grok-build audit markers")
    return markers[0] if markers else None


def index_existing_issues(
    rows: list[dict[str, Any]],
) -> tuple[dict[str, dict[str, Any]], dict[str, list[dict[str, Any]]]]:
    """Index remote issues by stable audit ID and exact title.

    Exact-title lookup exists only to detect collisions with unmanaged issues.
    Ambiguous remote state fails closed.
    """

    by_id: dict[str, dict[str, Any]] = {}
    by_title: dict[str, list[dict[str, Any]]] = {}
    for index, row in enumerate(rows, start=1):
        if not isinstance(row, dict):
            raise PublishError(f"Existing issue row #{index} is not an object")
        title = row.get("title")
        number = row.get("number")
        url = row.get("url")
        labels = row.get("labels")
        if (
            not isinstance(title, str)
            or not isinstance(number, int)
            or not isinstance(url, str)
            or not isinstance(labels, list)
            or not all(isinstance(label, str) for label in labels)
        ):
            raise PublishError(f"Existing issue row #{index} is missing fields")
        by_title.setdefault(title, []).append(row)

        issue_id = extract_audit_id(row.get("body"))
        if issue_id is not None:
            if issue_id in by_id:
                raise PublishError(f"Multiple existing issues claim marker {issue_id}")
            by_id[issue_id] = row
    return by_id, by_title


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


def create_issue(
    repo: str,
    issue: dict[str, Any],
    body: str,
) -> str:
    body_file = write_body_file(body)
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


def update_issue(
    repo: str,
    number: int,
    issue: dict[str, Any],
    body: str,
    existing_labels: list[str],
) -> None:
    body_file = write_body_file(body)
    try:
        args = [
            "issue",
            "edit",
            str(number),
            "--repo",
            repo,
            "--title",
            issue["title"],
            "--body-file",
            str(body_file),
        ]
        desired_labels = set(issue["labels"])
        current_labels = set(existing_labels)
        for label in sorted(desired_labels - current_labels):
            args.extend(["--add-label", label])
        for label in sorted(current_labels - desired_labels):
            args.extend(["--remove-label", label])
        run_gh(args)
    finally:
        body_file.unlink(missing_ok=True)


def select_issues(
    all_issues: list[dict[str, Any]], only_ids: list[str]
) -> list[dict[str, Any]]:
    if not only_ids:
        return all_issues
    invalid = sorted(
        {issue_id for issue_id in only_ids if not ISSUE_ID_RE.fullmatch(issue_id)}
    )
    if invalid:
        raise PublishError(f"Invalid --only issue IDs: {', '.join(invalid)}")
    wanted = set(only_ids)
    selected = [issue for issue in all_issues if issue["id"] in wanted]
    missing = sorted(wanted - {issue["id"] for issue in selected})
    if missing:
        raise PublishError(f"Unknown --only issue IDs: {', '.join(missing)}")
    return selected


def _match_existing_issue(
    issue: dict[str, Any],
    by_id: dict[str, dict[str, Any]],
    by_title: dict[str, list[dict[str, Any]]],
) -> tuple[dict[str, Any] | None, str | None]:
    marker_match = by_id.get(issue["id"])
    title_matches = by_title.get(issue["title"], [])
    if marker_match is not None and any(
        marker_match["number"] != row["number"] for row in title_matches
    ):
        raise PublishError(
            f"{issue['id']} marker and exact title identify different issues"
        )
    if marker_match is not None:
        return marker_match, "marker"
    if title_matches:
        numbers = ", ".join(f"#{row['number']}" for row in title_matches)
        raise PublishError(
            f"{issue['id']} exact title collides with existing issue(s) {numbers}; "
            "refusing to adopt them"
        )
    return None, None


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.apply and args.validate_only:
        raise PublishError("--apply and --validate-only cannot be combined")
    manifest = load_manifest(args.manifest)

    manifest_repo = validate_repository(manifest["repository"])
    repo = validate_repository(args.repo) if args.repo is not None else manifest_repo
    if repo != manifest_repo:
        raise PublishError(
            f"--repo {repo!r} does not match manifest.repository {manifest_repo!r}"
        )
    selected = select_issues(manifest["issues"], args.only)

    if args.validate_only:
        print(
            f"Validated {args.manifest}: {len(manifest['labels'])} labels, "
            f"{len(manifest['issues'])} issues; selected {len(selected)}"
        )
        return 0

    if args.dump_markdown:
        args.dump_markdown.parent.mkdir(parents=True, exist_ok=True)
        args.dump_markdown.write_text(render_markdown(manifest), encoding="utf-8")
        print(f"Wrote {args.dump_markdown}")
    print(f"Manifest: {args.manifest}")
    print(f"Repository: {repo}")
    print(f"Mode: {'APPLY' if args.apply else 'DRY RUN'}")
    print(f"Selected issues: {len(selected)} / {len(manifest['issues'])}")

    if not args.apply:
        if not args.skip_labels:
            sync_labels(repo, manifest["labels"], apply=False)
        for issue in selected:
            print(
                f"[dry-run] {issue['id']} create/update by stable marker "
                f"{issue['title']!r} labels={issue['labels']}"
            )
        print("\nNo mutations performed. Re-run with --apply after reviewing the plan.")
        return 0

    # The complete local schema gate above runs before authentication or any remote
    # mutation. GitHub operations are fail-fast but not transactional; stable markers
    # make a corrected rerun converge without duplicating already-created issues.
    require_gh_auth()
    if not repository_has_issues(repo):
        raise PublishError(
            "GitHub Issues is disabled. Enable it in repository settings before applying."
        )
    rows = list_existing_issues(repo)
    by_id, by_title = index_existing_issues(rows)
    planned_matches: dict[str, tuple[dict[str, Any] | None, str | None]] = {}
    for issue in selected:
        planned_matches[issue["id"]] = _match_existing_issue(issue, by_id, by_title)

    if not args.skip_labels:
        sync_labels(repo, manifest["labels"], apply=True)
    created: list[tuple[str, str]] = []
    updated: list[tuple[str, str]] = []
    skipped: list[tuple[str, str]] = []

    for issue in selected:
        row, match_kind = planned_matches[issue["id"]]
        body = format_issue_body(manifest, issue)
        if row is not None:
            if args.update_existing:
                print(
                    f"[apply] {issue['id']} update existing #{row['number']} "
                    f"matched by {match_kind}"
                )
                update_issue(repo, int(row["number"]), issue, body, row["labels"])
                updated.append((issue["id"], row["url"]))
            else:
                print(
                    f"[skip] {issue['id']} existing #{row['number']} "
                    f"matched by {match_kind}: {row['url']}"
                )
                skipped.append((issue["id"], row["url"]))
            continue

        print(f"[apply] {issue['id']} create {issue['title']}")
        url = create_issue(repo, issue, body)
        created.append((issue["id"], url))

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
