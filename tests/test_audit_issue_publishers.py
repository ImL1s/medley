from __future__ import annotations

import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from copy import deepcopy
from pathlib import Path
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[1]
PUBLISHER_PATH = REPO_ROOT / "scripts/publish-audit-issues.py"


def load_publisher():
    spec = importlib.util.spec_from_file_location(
        "audit_issue_publisher", PUBLISHER_PATH
    )
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {PUBLISHER_PATH}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


publisher = load_publisher()


def valid_manifest(issue_count: int = 1) -> dict:
    issues = []
    for number in range(1, issue_count + 1):
        issues.append(
            {
                "id": f"GB-{number:03d}",
                "title": f"Issue {number}",
                "body": f"Expected body {number}.\n",
                "labels": ["type:bug"],
            }
        )
    return {
        "schema_version": 1,
        "repository": "ImL1s/grok-build",
        "audit_date": "2026-07-28",
        "audited_branch": "providers",
        "audited_commit": "a" * 40,
        "source_path": "docs/audits/2026-07-28-grok-build-audit.md",
        "labels": {
            "type:bug": {"color": "D73A4A", "description": "A bug"},
        },
        "issues": issues,
    }


class ManifestTests(unittest.TestCase):
    def write_manifest(self, manifest: dict) -> tempfile.TemporaryDirectory:
        directory = tempfile.TemporaryDirectory()
        path = Path(directory.name) / "issues.json"
        path.write_text(json.dumps(manifest), encoding="utf-8")
        directory.path = path  # type: ignore[attr-defined]
        return directory

    def assert_rejected(self, manifest: dict, message: str) -> None:
        with self.write_manifest(manifest) as directory:
            path = Path(directory) / "issues.json"
            with self.assertRaisesRegex(publisher.PublishError, message):
                publisher.load_manifest(path)

    def test_default_manifest_is_plain_json_and_loads(self) -> None:
        self.assertEqual(publisher.DEFAULT_MANIFEST.suffix, ".json")
        manifest = publisher.load_manifest(REPO_ROOT / publisher.DEFAULT_MANIFEST)
        self.assertEqual(manifest["schema_version"], 1)
        self.assertTrue(manifest["issues"])

    def test_load_manifest_rejects_unsupported_schema_version(self) -> None:
        manifest = valid_manifest()
        manifest["schema_version"] = 2
        self.assert_rejected(manifest, "schema_version")

    def test_load_manifest_rejects_unknown_root_key(self) -> None:
        manifest = valid_manifest()
        manifest["repository_typo"] = manifest["repository"]
        self.assert_rejected(manifest, "unknown keys")

    def test_load_manifest_rejects_missing_audit_metadata(self) -> None:
        for field in ("repository", "audit_date", "audited_branch", "audited_commit"):
            with self.subTest(field=field):
                manifest = valid_manifest()
                del manifest[field]
                self.assert_rejected(manifest, field)

    def test_load_manifest_rejects_malformed_issue_id(self) -> None:
        for issue_id in ("GB-1", "GB-0001", "XX-001", " "):
            with self.subTest(issue_id=issue_id):
                manifest = valid_manifest()
                manifest["issues"][0]["id"] = issue_id
                self.assert_rejected(manifest, "id")

    def test_load_manifest_rejects_duplicate_issue_id(self) -> None:
        manifest = valid_manifest(2)
        manifest["issues"][1]["id"] = manifest["issues"][0]["id"]
        self.assert_rejected(manifest, "Duplicate issue id")

    def test_load_manifest_rejects_duplicate_issue_title(self) -> None:
        manifest = valid_manifest(2)
        manifest["issues"][1]["title"] = manifest["issues"][0]["title"]
        self.assert_rejected(manifest, "Duplicate issue title")

    def test_load_manifest_rejects_undefined_issue_label(self) -> None:
        manifest = valid_manifest()
        manifest["issues"][0]["labels"] = ["missing"]
        self.assert_rejected(manifest, "undefined labels")

    def test_load_manifest_rejects_malformed_repository(self) -> None:
        for repository in (
            "/repo",
            "owner/",
            "owner/repo/extra",
            "owner repo/name",
            "owner/name with space",
        ):
            with self.subTest(repository=repository):
                manifest = valid_manifest()
                manifest["repository"] = repository
                self.assert_rejected(manifest, "OWNER/REPO")

    def test_load_manifest_rejects_malformed_label_definition(self) -> None:
        invalid_definitions = (
            "not an object",
            {"color": "12345", "description": "A bug"},
            {"color": "nothex", "description": "A bug"},
            {"color": "D73A4A", "description": 42},
        )
        for definition in invalid_definitions:
            with self.subTest(definition=definition):
                manifest = valid_manifest()
                manifest["labels"]["type:bug"] = definition
                self.assert_rejected(manifest, "Label")

    def test_load_manifest_rejects_out_of_order_ids_and_duplicate_labels(self) -> None:
        manifest = valid_manifest(2)
        manifest["issues"].reverse()
        self.assert_rejected(manifest, "ordered by audit ID")

        manifest = valid_manifest()
        manifest["issues"][0]["labels"] = ["type:bug", "type:bug"]
        self.assert_rejected(manifest, "duplicate labels")

    def test_load_manifest_rejects_reserved_marker_in_source_body(self) -> None:
        manifest = valid_manifest()
        manifest["issues"][0]["body"] += "\n<!-- grok-build-audit-id: GB-001 -->\n"
        self.assert_rejected(manifest, "publisher-reserved audit marker")

    def test_format_issue_body_preserves_body_and_adds_stable_marker(self) -> None:
        manifest = valid_manifest()
        issue = manifest["issues"][0]
        formatted_once = publisher.format_issue_body(manifest, issue)
        formatted_twice = publisher.format_issue_body(manifest, issue)
        self.assertEqual(formatted_once, formatted_twice)
        self.assertIn(issue["body"].strip(), formatted_once)
        self.assertIn(issue["id"], formatted_once)
        self.assertIn("<!--", formatted_once)
        self.assertIn("-->", formatted_once)


class PublisherBehaviorTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.manifest_path = Path(self.temporary_directory.name) / "issues.json"

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def write_manifest(self, manifest: dict) -> None:
        self.manifest_path.write_text(json.dumps(manifest), encoding="utf-8")

    def run_main(self, manifest: dict, *arguments: str, existing=None, create=None):
        self.write_manifest(manifest)
        argv = [
            str(PUBLISHER_PATH),
            "--manifest",
            str(self.manifest_path),
            *arguments,
        ]
        existing_rows = [] if existing is None else existing
        create_mock = mock.Mock(return_value="https://example.test/issues/1")
        if create is not None:
            create_mock.side_effect = create
        with (
            mock.patch.object(sys, "argv", argv),
            mock.patch.object(publisher, "require_gh_auth") as auth,
            mock.patch.object(publisher, "repository_has_issues", return_value=True),
            mock.patch.object(publisher, "sync_labels") as labels,
            mock.patch.object(
                publisher, "list_existing_issues", return_value=existing_rows
            ),
            mock.patch.object(publisher, "create_issue", create_mock),
            mock.patch.object(publisher, "update_issue") as update,
            redirect_stdout(io.StringIO()) as output,
        ):
            result = publisher.main()
        return result, output.getvalue(), auth, labels, create_mock, update

    def issue_row(self, manifest: dict, issue_index: int = 0, *, title=None, body=None):
        issue = manifest["issues"][issue_index]
        return {
            "number": issue_index + 10,
            "title": issue["title"] if title is None else title,
            "url": f"https://example.test/issues/{issue_index + 10}",
            "body": (
                publisher.format_issue_body(manifest, issue) if body is None else body
            ),
            "state": "OPEN",
            "labels": list(issue["labels"]),
        }

    @staticmethod
    def created_ids(create_mock: mock.Mock) -> list[str]:
        ids = []
        for call in create_mock.call_args_list:
            issue = next(
                arg for arg in call.args if isinstance(arg, dict) and "id" in arg
            )
            ids.append(issue["id"])
        return ids

    def test_dry_run_does_not_authenticate_or_call_github(self) -> None:
        manifest = valid_manifest()
        self.write_manifest(manifest)
        argv = [str(PUBLISHER_PATH), "--manifest", str(self.manifest_path)]
        with (
            mock.patch.object(sys, "argv", argv),
            mock.patch.object(publisher, "run_gh") as run_gh,
            mock.patch.object(publisher, "require_gh_auth") as auth,
            redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(publisher.main(), 0)
        run_gh.assert_not_called()
        auth.assert_not_called()

    def test_repo_override_must_match_manifest_repository(self) -> None:
        manifest = valid_manifest()
        self.write_manifest(manifest)
        argv = [
            str(PUBLISHER_PATH),
            "--manifest",
            str(self.manifest_path),
            "--repo",
            "other/repository",
        ]
        with mock.patch.object(sys, "argv", argv):
            with self.assertRaisesRegex(publisher.PublishError, "repository"):
                publisher.main()

    def test_marker_match_skips_issue_even_when_title_changed(self) -> None:
        manifest = valid_manifest()
        row = self.issue_row(manifest, title="Title edited on GitHub")
        result = self.run_main(manifest, "--apply", "--skip-labels", existing=[row])
        self.assertEqual(result[0], 0)
        result[4].assert_not_called()
        result[5].assert_not_called()

    def test_update_existing_receives_the_remote_label_set(self) -> None:
        manifest = valid_manifest()
        row = self.issue_row(manifest, title="Title edited on GitHub")
        row["labels"] = ["stale-label"]
        result = self.run_main(
            manifest,
            "--apply",
            "--skip-labels",
            "--update-existing",
            existing=[row],
        )
        self.assertEqual(result[0], 0)
        result[4].assert_not_called()
        result[5].assert_called_once()
        self.assertEqual(result[5].call_args.args[-1], ["stale-label"])

    def test_unmanaged_exact_title_collision_fails_closed(self) -> None:
        manifest = valid_manifest()
        row = self.issue_row(manifest, body="Legacy issue without a marker")
        self.write_manifest(manifest)
        with (
            mock.patch.object(publisher, "require_gh_auth"),
            mock.patch.object(publisher, "repository_has_issues", return_value=True),
            mock.patch.object(publisher, "list_existing_issues", return_value=[row]),
            mock.patch.object(publisher, "sync_labels") as labels,
            mock.patch.object(publisher, "create_issue") as create,
            mock.patch.object(publisher, "update_issue") as update,
            redirect_stdout(io.StringIO()),
        ):
            with self.assertRaisesRegex(publisher.PublishError, "refusing to adopt"):
                publisher.main(["--manifest", str(self.manifest_path), "--apply"])
        labels.assert_not_called()
        create.assert_not_called()
        update.assert_not_called()

    def test_validate_only_does_not_write_or_contact_github(self) -> None:
        manifest = valid_manifest()
        self.write_manifest(manifest)
        markdown_path = Path(self.temporary_directory.name) / "must-not-exist.md"
        with (
            mock.patch.object(publisher, "run_gh") as run_gh,
            mock.patch.object(publisher, "require_gh_auth") as auth,
            mock.patch.object(publisher, "sync_labels") as labels,
            mock.patch.object(publisher, "create_issue") as create,
            mock.patch.object(publisher, "update_issue") as update,
            redirect_stdout(io.StringIO()) as output,
        ):
            result = publisher.main(
                [
                    "--manifest",
                    str(self.manifest_path),
                    "--validate-only",
                    "--dump-markdown",
                    str(markdown_path),
                ]
            )
        self.assertEqual(result, 0)
        self.assertIn("Validated", output.getvalue())
        self.assertFalse(markdown_path.exists())
        run_gh.assert_not_called()
        auth.assert_not_called()
        labels.assert_not_called()
        create.assert_not_called()
        update.assert_not_called()

    def test_duplicate_remote_marker_fails_before_label_mutation(self) -> None:
        manifest = valid_manifest()
        first = self.issue_row(manifest, title="Remote title one")
        second = dict(
            first,
            number=11,
            title="Remote title two",
            url="https://example.test/issues/11",
        )
        self.write_manifest(manifest)
        with (
            mock.patch.object(publisher, "require_gh_auth"),
            mock.patch.object(publisher, "repository_has_issues", return_value=True),
            mock.patch.object(
                publisher, "list_existing_issues", return_value=[first, second]
            ),
            mock.patch.object(publisher, "sync_labels") as labels,
            mock.patch.object(publisher, "create_issue") as create,
            mock.patch.object(publisher, "update_issue") as update,
            redirect_stdout(io.StringIO()),
        ):
            with self.assertRaisesRegex(
                publisher.PublishError, "Multiple existing issues claim marker"
            ):
                publisher.main(["--manifest", str(self.manifest_path), "--apply"])
        labels.assert_not_called()
        create.assert_not_called()
        update.assert_not_called()

    def test_unrelated_duplicate_titles_do_not_block_publication(self) -> None:
        manifest = valid_manifest()
        unrelated = [
            {
                "number": number,
                "title": "Unrelated duplicate",
                "url": f"https://example.test/issues/{number}",
                "body": "No audit marker",
                "state": "OPEN",
                "labels": [],
            }
            for number in (40, 41)
        ]
        result = self.run_main(
            manifest,
            "--apply",
            "--skip-labels",
            existing=unrelated,
        )
        self.assertEqual(result[0], 0)
        self.assertEqual(self.created_ids(result[4]), ["GB-001"])

    def test_retry_after_partial_failure_creates_only_remaining_issue(self) -> None:
        manifest = valid_manifest(2)
        self.write_manifest(manifest)
        first_run_create = mock.Mock(
            side_effect=[
                "https://example.test/issues/1",
                publisher.PublishError("boom"),
            ]
        )
        argv = [
            str(PUBLISHER_PATH),
            "--manifest",
            str(self.manifest_path),
            "--apply",
            "--skip-labels",
        ]
        with (
            mock.patch.object(sys, "argv", argv),
            mock.patch.object(publisher, "require_gh_auth"),
            mock.patch.object(publisher, "repository_has_issues", return_value=True),
            mock.patch.object(publisher, "list_existing_issues", return_value=[]),
            mock.patch.object(publisher, "create_issue", first_run_create),
            redirect_stdout(io.StringIO()),
        ):
            with self.assertRaisesRegex(publisher.PublishError, "boom"):
                publisher.main()
        self.assertEqual(self.created_ids(first_run_create), ["GB-001", "GB-002"])

        first_row = self.issue_row(manifest, 0)
        retry = self.run_main(
            manifest,
            "--apply",
            "--skip-labels",
            existing=[first_row],
        )
        self.assertEqual(self.created_ids(retry[4]), ["GB-002"])


class GitHubCommandTests(unittest.TestCase):
    def completed(self, stdout: str = "") -> subprocess.CompletedProcess[str]:
        return subprocess.CompletedProcess(["gh"], 0, stdout=stdout, stderr="")

    def test_sync_labels_constructs_force_create_command(self) -> None:
        with (
            mock.patch.object(publisher, "run_gh") as run_gh,
            redirect_stdout(io.StringIO()),
        ):
            publisher.sync_labels(
                "ImL1s/grok-build", valid_manifest()["labels"], apply=True
            )
        run_gh.assert_called_once_with(
            [
                "label",
                "create",
                "type:bug",
                "--repo",
                "ImL1s/grok-build",
                "--color",
                "D73A4A",
                "--description",
                "A bug",
                "--force",
            ]
        )

    def test_repository_check_constructs_api_command(self) -> None:
        with mock.patch.object(
            publisher, "run_gh", return_value=self.completed("true\n")
        ) as run_gh:
            self.assertTrue(publisher.repository_has_issues("ImL1s/grok-build"))
        run_gh.assert_called_once_with(
            ["api", "repos/ImL1s/grok-build", "--jq", ".has_issues"]
        )

    def test_list_existing_paginates_and_filters_pull_requests(self) -> None:
        pages = [
            [
                {
                    "number": 3,
                    "title": "Issue three",
                    "html_url": "https://example.test/issues/3",
                    "body": "Body three",
                    "state": "open",
                    "labels": [{"name": "type:bug"}],
                },
                {
                    "number": 4,
                    "title": "Pull request four",
                    "html_url": "https://example.test/pull/4",
                    "body": "PR",
                    "state": "open",
                    "labels": [],
                    "pull_request": {"url": "https://api.example.test/pulls/4"},
                },
            ],
            [
                {
                    "number": 5,
                    "title": "Issue five",
                    "html_url": "https://example.test/issues/5",
                    "body": None,
                    "state": "closed",
                    "labels": ["priority:p1"],
                }
            ],
        ]
        with mock.patch.object(
            publisher, "run_gh", return_value=self.completed(json.dumps(pages))
        ) as run_gh:
            rows = publisher.list_existing_issues("ImL1s/grok-build")
        self.assertEqual([row["number"] for row in rows], [3, 5])
        self.assertEqual(rows[0]["labels"], ["type:bug"])
        self.assertEqual(rows[1]["body"], "")
        self.assertEqual(rows[1]["state"], "CLOSED")
        run_gh.assert_called_once_with(
            [
                "api",
                "--paginate",
                "--slurp",
                "repos/ImL1s/grok-build/issues?state=all&per_page=100",
            ]
        )

    def test_create_issue_constructs_body_and_label_arguments(self) -> None:
        issue = deepcopy(valid_manifest()["issues"][0])
        observed = {}

        def capture(arguments):
            body_path = Path(arguments[arguments.index("--body-file") + 1])
            observed["arguments"] = arguments
            observed["body"] = body_path.read_text(encoding="utf-8")
            observed["path"] = body_path
            return self.completed("https://example.test/issues/1\n")

        with mock.patch.object(publisher, "run_gh", side_effect=capture):
            self.assertEqual(
                publisher.create_issue("ImL1s/grok-build", issue, issue["body"]),
                "https://example.test/issues/1",
            )
        self.assertEqual(observed["body"], issue["body"])
        self.assertFalse(observed["path"].exists())
        self.assertEqual(
            observed["arguments"][:6],
            [
                "issue",
                "create",
                "--repo",
                "ImL1s/grok-build",
                "--title",
                issue["title"],
            ],
        )
        self.assertEqual(observed["arguments"][-2:], ["--label", "type:bug"])

    def test_update_issue_converges_title_body_and_exact_label_set(self) -> None:
        issue = deepcopy(valid_manifest()["issues"][0])
        body = "Managed body\n\n<!-- grok-build-audit-id: GB-001 -->\n"
        observed = {}

        def capture(arguments):
            body_path = Path(arguments[arguments.index("--body-file") + 1])
            observed["arguments"] = arguments
            observed["body"] = body_path.read_text(encoding="utf-8")
            observed["path"] = body_path
            return self.completed()

        with mock.patch.object(publisher, "run_gh", side_effect=capture):
            publisher.update_issue(
                "ImL1s/grok-build",
                42,
                issue,
                body,
                ["stale-label", "already-present"],
            )
        self.assertEqual(observed["body"], body)
        self.assertFalse(observed["path"].exists())
        self.assertEqual(
            observed["arguments"][:8],
            [
                "issue",
                "edit",
                "42",
                "--repo",
                "ImL1s/grok-build",
                "--title",
                issue["title"],
                "--body-file",
            ],
        )
        self.assertEqual(
            observed["arguments"][-6:],
            [
                "--add-label",
                "type:bug",
                "--remove-label",
                "already-present",
                "--remove-label",
                "stale-label",
            ],
        )


if __name__ == "__main__":
    unittest.main()
