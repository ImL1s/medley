"""Regression tests for the negated GitHub closing-keyword guard (#513)."""

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "scripts"))

import check_negated_closing_keywords as guard  # noqa: E402


class Detection(unittest.TestCase):
    def test_all_official_keyword_forms_are_case_insensitive(self):
        for keyword in (
            "close",
            "closes",
            "closed",
            "fix",
            "fixes",
            "fixed",
            "resolve",
            "resolves",
            "resolved",
        ):
            with self.subTest(keyword=keyword):
                findings = guard.find_negated_closing_keywords(
                    f"This does NOT {keyword.upper()}: Owner/repo#123"
                )
                self.assertEqual(len(findings), 1, findings)

    def test_clause_boundaries_stop_the_negation_window(self):
        for separator in (". ", "? ", "! ", "; ", "\n"):
            with self.subTest(separator=separator):
                self.assertEqual(
                    guard.find_negated_closing_keywords(
                        f"This does not change X{separator}Closes #9"
                    ),
                    [],
                )

    def test_soft_line_wrap_preserves_negation(self):
        """A wrapped `does not\\nclose #123` is still negated (#513 review)."""

        self.assertEqual(
            len(guard.find_negated_closing_keywords("This does not\nclose #123")),
            1,
        )

    def test_comma_clause_does_not_inherit_negation(self):
        """`not a breaking change, fixes #123` is a positive closer
        (#513 review)."""

        self.assertEqual(
            guard.find_negated_closing_keywords(
                "This is not a breaking change, fixes #123"
            ),
            [],
        )
        self.assertEqual(
            guard.find_negated_closing_keywords(
                "This is not a breaking change, and fixes #123"
            ),
            [],
        )
        self.assertEqual(
            guard.find_negated_closing_keywords(
                "This is not a breaking change, adds tests, and fixes #123"
            ),
            [],
        )
        self.assertEqual(
            guard.find_negated_closing_keywords(
                "This does not change the API, adds tests, fixes #123"
            ),
            [],
        )

    def test_parenthetical_comma_preserves_negation(self):
        """`does not, in fact, close` is still negated (#513 review)."""

        for text in (
            "This does not, in fact, close #123",
            "This does not — in fact — close #123",
            "This does not, despite the title, close #123",
            "This does not, under any circumstances, fix #123",
            "This does not, at present, fix #123",
            "This change does not:\n- fix #123",
            "This change does not:\n- change the API\n- fix #123",
            "Does this fix #123? No.",
            "Does this fix #123? No, it doesn't.",
            "Fixes #123? Not yet.",
            "Fixes #123? No. It only adds tests.",
            "This change fails consistently to fix #123",
            "This fails consistently even now in every environment to fix #123",
            "This does not adequately\nFix #123",
            "This is insufficient to fix #123",
            "This is inadequate to resolve #123",
            "This does not while running offline fix #123",
            "This does not, updates notwithstanding, fix #123",
        ):
            with self.subTest(text=text):
                self.assertEqual(
                    len(guard.find_negated_closing_keywords(text)), 1
                )

    def test_capitalized_wrap_preserves_negation(self):
        """A wrapped closer stays negated even when capitalized
        (#513 review)."""

        for text in (
            "This does not\nClose #123",
            "DOES NOT\nCLOSE #123",
            "This does not really\nClose #123",
            "This never actually\nFixes #123",
        ):
            with self.subTest(text=text):
                self.assertEqual(
                    len(guard.find_negated_closing_keywords(text)), 1
                )

    def test_negation_forms_and_clause_scope(self):
        bad = (
            "Does not close #1",
            "Doesn't fix #2",
            "Never resolved #3",
            "Unlike the earlier patch, this does not resolve #4",
            "Use a reference rather than fixes #5",
            "Cannot close #17",
            "Does not close https://github.com/ImL1s/medley/issues/18",
            "This no longer closes #123",
            "This in no way fixes #123",
            "This does not close #123, but mentions the follow-up",
            "This does not, however, close #123",
            "This does not however close #123",
            "This fails to close #123",
            "Failed to fix #123",
            "This patch still fails to fix #123",
            "This fails to resolve #123",
            "Doesnt close #123",
            "Dont fix #123",
            "Wont resolve #123",
            "This neither fixes #123",
            "This change is unable to close #123",
            "Unable to fix #123",
            "This is impossible to fix #123",
            "This makes it impossible to resolve #123",
            "Nothing in this PR fixes #123",
            "Partially fixes #123",
            "Only partially resolves #123",
            "This hardly fixes #123",
            "This barely closes #123",
            "This scarcely resolves #123",
            "This almost fixes #123",
            "This nearly fixes #123",
            "This mostly fixes #123",
            "This is unlikely to fix #123",
            "This is unable currently to fix #123",
            "This is unlikely ever to resolve #123",
            "This is not complete unless it fixes #123",
            "We still need to fix #123",
            "This remains to fix #123",
            "This change still needs more work to fix #123",
            "This still needs further review to close #123",
            "This still needs considerably more extensive testing and review to fix #123",
            "This never, ever fixes #123",
            "This does not, frankly speaking, fix #123",
            "This change does everything except fix #123",
            "This does everything other than fix #123",
            "This does anything except close #123",
            "TODO: fix #123",
            "We plan to fix #123",
            "There is no fix #123",
            "This provides no fix #123",
            "This change refuses to fix #123",
            "This refuses to close #123",
            "Refused to resolve #123",
            "This change refuses completely to fix #123",
            "This refuses outright to fix #123",
            "This does not yet\nFix #123",
            "No part of this PR fixes #123",
            "This lands without fix #123",
            "This failed completely to fix #123",
            "This does not, e.g., fix #123",
            "This does not (yet) fix #123",
            "This does not, i.e., close #123",
            "Fixes #123 — actually, no",
            "Fixes #123 (not)",
            "Closes #123 — no",
            "Resolves #123 (never)",
            "Fixes #123 — no longer",
            "Fixes #123 — no way",
            "Closes #123 — no longer",
            "This change does anything but fix #123",
            "This does everything but close #123",
            "There is no API fix #123",
            "This does not address the bug and fix #123",
            "This does not (fix #123)",
            "This cannot (close #123)",
            "This does not help and is unlikely to fix #123",
            "This is not unlikely to help and does not fix #123",
        )
        for text in bad:
            with self.subTest(text=text):
                self.assertEqual(len(guard.find_negated_closing_keywords(text)), 1)

        neither_nor = guard.find_negated_closing_keywords(
            "This neither fixes #123 nor closes #124"
        )
        self.assertEqual(
            {(f.keyword, f.reference) for f in neither_nor},
            {("fixes", "#123"), ("closes", "#124")},
            neither_nor,
        )

        # Negation stays active for the whole clause — no fixed token window
        # (#530 review). Long modifiers must not silently drop `not`.
        far = "not " + " ".join(f"word{i}" for i in range(20))
        self.assertEqual(
            len(guard.find_negated_closing_keywords(f"{far} closes #6")), 1
        )
        long_modifier = (
            "This does not in any conceivable manner under any of the "
            "possible production configurations actually fix #123"
        )
        self.assertEqual(
            len(guard.find_negated_closing_keywords(long_modifier)), 1
        )
        # Subordinate word opening a modifier must keep negation (#530).
        for text in (
            "This cannot, because of API limitations, fix #123",
            "This cannot because of API limitations fix #123",
        ):
            with self.subTest(text=text):
                self.assertEqual(
                    len(guard.find_negated_closing_keywords(text)), 1, text
                )

    def test_quotes_and_code_fences_are_not_exempt(self):
        text = "> Does not close #7\n\n```text\nNever fixes #8\n```"
        self.assertEqual(len(guard.find_negated_closing_keywords(text)), 2)

    def test_allow_corpus(self):
        allowed = (
            "This does not change the sampler. Closes #10.",
            "Leaves #11 open for the remaining work.",
            "Refs #12; fixes spelling only.",
            "Notable cleanup closes #13.",
            "The word unresolved is not a closing keyword for #14.",
            "Fixes: https://example.com/#15",
            (
                "fix(docs): name the never_emit_credential_bytes family explicitly, "
                "pin all five hot-path counts (closes #487)"
            ),
            "Document not_close as an example, then fixes #16",
            "This does not change the API, but fixes #123",
            "This does not change the API, however it fixes #123",
            "This does not change the API, however, it fixes #123",
            "This does not change the API — it fixes #123",
            "This does not change the API -- it fixes #123",
            "This is not impossible to fix #123",
            "This is not unable to fix #123",
            "No API changes, fixes #123",
            "This makes no behavioral changes and fixes #123",
            "This not only fixes #123, it adds regression tests",
            "This doesn't just fix #123; it also improves diagnostics",
            "This does not just fix #123, it also adds tests",
            "This does not merely fix #123; it also improves diagnostics",
            "Unlike the old patch, this fixes #123",
            "Removes the TODO and fixes #123",
            "Fixes TODO parsing and closes #123",
            "This does not regress performance and fixes #123",
            "This does not regress performance and also fixes #123",
            "This doesn't change the API and adds tests that fix #123",
            "This does not regress behavior and then fixes #123",
            "This does not regress behavior and subsequently fixes #123",
            "This lands without fixing #123",
            "Merged without closing #123",
            "Fixes #123, not #124",
            "Fixes #123 and does not regress performance",
            "Fixes #123, not the unrelated parser crash",
            "Fixes #123 — no API changes",
            "- No API changes\n- fixes #123",
            "This does not change the API (fixes #123)",
            "This does not change the API (it fixes #123)",
            "This does not change the API (also fixes #123)",
            "This does not change the API (which fixes #123)",
            "This does not change the API (the patch fixes #123)",
            "- Does not change the API\n- Fixes #123",
            "This is not handled by Acme Inc. Fixes #123",
            "This does not address the bug and also fixes #123",
            "This does not change the API because it fixes #123",
            "This never exposes the workaround while it fixes #123",
            "This does not expose secrets when it closes #123",
            "This does not fail to fix #123",
            "This patch never fails to fix #123",
            "This is not unlikely to fix #123",
            "No API changes\n\nfixes #123",
        )
        for text in allowed:
            with self.subTest(text=text):
                self.assertEqual(guard.find_negated_closing_keywords(text), [])


class HistoricalCorpus(unittest.TestCase):
    def test_landed_known_bad_commit_messages_are_detected(self):
        expected = (
            ("a85a9e0c", "Does not close #432", "#432"),
            ("06208096", "Does NOT close #290", "#290"),
            ("bf046848", "Does NOT close #289", "#289"),
            ("5ac32072", "Does NOT close #289", "#289"),
        )
        for commit, message, reference in expected:
            with self.subTest(commit=commit):
                findings = guard.find_negated_closing_keywords(message)
                self.assertTrue(
                    any(f.reference.casefold() == reference for f in findings),
                    findings,
                )


class PayloadAndCli(unittest.TestCase):
    CLEAN = {
        "title": "A focused change",
        "body": "Leaves #1 open.",
        "commits": [{"messageHeadline": "Change it", "messageBody": "Refs #1"}],
    }

    def test_scans_pr_and_every_commit_field(self):
        payload = {
            "title": "Does not close #1",
            "body": "Never fixes #2",
            "commits": [
                {"messageHeadline": "Doesn't resolve #3", "messageBody": "Refs #3"},
                {"messageHeadline": "Clean", "messageBody": "Rather than closes #4"},
            ],
        }
        findings = guard.find_payload_findings(payload)
        self.assertEqual(
            [finding.source for finding in findings],
            ["pr.title", "pr.body", "commit[1]", "commit[2]"],
        )

    def test_commit_headline_and_body_are_one_message(self):
        """GitHub sees headline plus body as one commit message
        (#513 review)."""

        findings = guard.find_payload_findings(
            {
                "title": "safe",
                "body": "Leaves #1 open.",
                "commits": [
                    {
                        "messageHeadline": "This does not fully",
                        "messageBody": "fix #123",
                    }
                ],
            }
        )
        self.assertEqual(len(findings), 1, findings)
        self.assertEqual(findings[0].source, "commit[1]")
        self.assertEqual(findings[0].reference, "#123")

    def test_malformed_payload_is_api_error(self):
        malformed = dict(self.CLEAN, commits=[{"messageHeadline": "missing body"}])
        with self.assertRaises(guard.PayloadError):
            guard.find_payload_findings(malformed)

    def test_empty_commit_metadata_is_api_error(self):
        with self.assertRaises(guard.PayloadError):
            guard.find_payload_findings(dict(self.CLEAN, commits=[]))

    def test_main_exit_codes_and_does_not_echo_untrusted_full_text(self):
        bad_text = "SECRET arbitrary prose Does not close #77 trailing SECRET"
        cases = (
            (self.CLEAN, 0),
            (dict(self.CLEAN, body=bad_text), 1),
            ({"title": 123, "body": "", "commits": []}, 2),
        )
        for payload, expected in cases:
            with self.subTest(expected=expected):
                stdout = io.StringIO()
                stderr = io.StringIO()
                with (
                    mock.patch.object(
                        guard, "_fetch_pr_payload", return_value=payload
                    ),
                    contextlib.redirect_stdout(stdout),
                    contextlib.redirect_stderr(stderr),
                ):
                    actual = guard.main(["--pr", "123", "--repo", "ImL1s/medley"])
                self.assertEqual(
                    actual, expected, stdout.getvalue() + stderr.getvalue()
                )
                self.assertNotIn(
                    "SECRET arbitrary prose", stdout.getvalue() + stderr.getvalue()
                )

    def test_diagnostics_cap_repeated_findings(self):
        payload = dict(
            self.CLEAN,
            body="\n".join(
                f"Does not close #{number}"
                for number in range(1, guard.MAX_REPORTED_FINDINGS + 8)
            ),
        )
        stderr = io.StringIO()
        with (
            mock.patch.object(guard, "_fetch_pr_payload", return_value=payload),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertEqual(
                guard.main(["--pr", "123", "--repo", "ImL1s/medley"]), 1
            )
        lines = stderr.getvalue().splitlines()
        finding_lines = [line for line in lines if "in pr.body" in line]
        self.assertEqual(len(finding_lines), guard.MAX_REPORTED_FINDINGS)
        self.assertIn("7 additional finding(s) omitted", stderr.getvalue())

    def test_gh_failure_returns_two_without_echoing_gh_stderr(self):
        proc = subprocess.CompletedProcess([], 1, "", "SECRET remote response")
        stderr = io.StringIO()
        with (
            mock.patch.object(guard.subprocess, "run", return_value=proc),
            contextlib.redirect_stderr(stderr),
        ):
            actual = guard.main(["--pr", "123", "--repo", "ImL1s/medley"])
        self.assertEqual(actual, 2)
        self.assertNotIn("SECRET", stderr.getvalue())

    def test_commit_metadata_is_paginated_past_one_hundred(self):
        page1_nodes = [
            {
                "commit": {
                    "messageHeadline": f"safe {index}",
                    "messageBody": "",
                }
            }
            for index in range(100)
        ]
        page1 = {
            "data": {
                "repository": {
                    "pullRequest": {
                        "title": "safe",
                        "body": "",
                        "commits": {
                            "pageInfo": {
                                "hasNextPage": True,
                                "endCursor": "cursor-2",
                            },
                            "nodes": page1_nodes,
                        },
                    }
                }
            }
        }
        page2 = {
            "data": {
                "repository": {
                    "pullRequest": {
                        "title": "safe",
                        "body": "",
                        "commits": {
                            "pageInfo": {
                                "hasNextPage": False,
                                "endCursor": None,
                            },
                            "nodes": [
                                {
                                    "commit": {
                                        "messageHeadline": "Does not close #101",
                                        "messageBody": "",
                                    }
                                }
                            ],
                        },
                    }
                }
            }
        }
        responses = [
            subprocess.CompletedProcess([], 0, json.dumps(page1), ""),
            subprocess.CompletedProcess([], 0, json.dumps(page2), ""),
        ]
        with mock.patch.object(guard.subprocess, "run", side_effect=responses):
            payload = guard._fetch_pr_payload("123", "ImL1s/medley")
        self.assertEqual(len(payload["commits"]), 101)
        findings = guard.find_payload_findings(payload)
        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].reference, "#101")

    def test_title_and_body_come_from_the_last_graphql_page(self):
        def page(title, body, *, has_next, cursor, headline="safe"):
            return {
                "data": {
                    "repository": {
                        "pullRequest": {
                            "title": title,
                            "body": body,
                            "commits": {
                                "pageInfo": {
                                    "hasNextPage": has_next,
                                    "endCursor": cursor,
                                },
                                "nodes": [
                                    {
                                        "commit": {
                                            "messageHeadline": headline,
                                            "messageBody": "",
                                        }
                                    }
                                ],
                            },
                        }
                    }
                }
            }

        responses = [
            subprocess.CompletedProcess(
                [], 0, json.dumps(page("safe", "", has_next=True, cursor="c2")), ""
            ),
            subprocess.CompletedProcess(
                [],
                0,
                json.dumps(
                    page(
                        "Does not close #99",
                        "Leaves nothing; still open.",
                        has_next=False,
                        cursor=None,
                    )
                ),
                "",
            ),
        ]
        with mock.patch.object(guard.subprocess, "run", side_effect=responses):
            payload = guard._fetch_pr_payload("123", "ImL1s/medley")
        self.assertEqual(payload["title"], "Does not close #99")
        findings = guard.find_payload_findings(payload)
        self.assertTrue(any(f.reference == "#99" for f in findings), findings)

    def test_malformed_cli_and_json_return_two(self):
        self.assertEqual(
            guard.main(["--pr", "not-a-number", "--repo", "owner/repo"]), 2
        )
        with mock.patch.object(
            guard, "_fetch_pr_payload", side_effect=guard.PayloadError("bad")
        ):
            self.assertEqual(guard.main(["--pr", "1", "--repo", "owner/repo"]), 2)


class Enrollment(unittest.TestCase):
    def test_ci_runs_a_nonzero_test_module(self):
        workflow = (REPO / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "python3 -B -m unittest tests.test_negated_closing_keywords_guard -v",
            workflow,
        )

    def test_merge_wrapper_runs_guard_before_merge(self):
        wrapper = (REPO / "scripts" / "merge-pr.sh").read_text(encoding="utf-8")
        guard_call = wrapper.index(
            'python3 -B "$CLOSING_GUARD" --pr "$PR" --repo "$REPO"'
        )
        final_head_check = wrapper.index('TIP_AGAIN="$(git ls-remote')
        merge_call = wrapper.index('gh pr merge "$PR"')
        self.assertLess(final_head_check, guard_call)
        self.assertLess(guard_call, merge_call)
        self.assertIn('--match-head-commit "$HEAD"', wrapper)
        self.assertLess(
            wrapper.index('--match-head-commit "$HEAD"'),
            wrapper.index("merge_sha="),
        )
        dequeue = wrapper.index("dequeuePullRequest")
        disable_auto = wrapper.index("--disable-auto")
        not_merged = wrapper.index('die "PR was not MERGED')
        self.assertLess(disable_auto, dequeue)
        self.assertLess(dequeue, not_merged)
        self.assertIn("dequeue did not remove the PR from the merge queue", wrapper)
        self.assertIn("could not dequeue merge-queue entry", wrapper)
        self.assertIn("isInMergeQueue", wrapper)
        self.assertLess(
            wrapper.index("Rejecting PRs already in the merge queue"),
            wrapper.index('python3 -B "$GUARD"'),
        )
        self.assertLess(
            wrapper.index("Rejecting merge-queue-required base branches"),
            wrapper.index('python3 -B "$GUARD"'),
        )
        self.assertIn("mergeQueue(branch:", wrapper)
        self.assertNotIn(
            '--json id --jq .id 2>/dev/null || true',
            wrapper,
        )
        self.assertNotIn(
            'pr_node="$(gh pr view "$PR" --repo "$REPO" --json id --jq .id)"',
            wrapper,
        )
        self.assertLess(
            wrapper.index('id=$PR_NODE'),
            dequeue,
        )
        self.assertIn("--admin", wrapper)
        self.assertIn("--print-digest", wrapper)
        self.assertIn("PR title/body changed after the closing-keyword scan", wrapper)
        self.assertIn(
            'if ! merged_json="$(gh pr view "$PR" --repo "$REPO" --json mergeCommit,mergedAt,state,headRefOid)"',
            wrapper,
        )
        self.assertIn(
            'if ! gh pr merge "$PR" --repo "$REPO" --match-head-commit "$HEAD"',
            wrapper,
        )

    def test_merge_wrapper_rejects_repo_override_before_gh(self):
        for flag in ("--repo", "--repo=other/project", "-R", "-Rother/project"):
            with self.subTest(flag=flag):
                proc = subprocess.run(
                    [
                        "bash",
                        str(REPO / "scripts" / "merge-pr.sh"),
                        "1",
                        flag,
                        "SECRET",
                    ],
                    cwd=REPO,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertIn("repository is bound to this checkout", proc.stderr)
                self.assertNotIn("SECRET", proc.stdout + proc.stderr)

    def test_merge_wrapper_rejects_match_head_commit_override_before_gh(self):
        for flag in ("--match-head-commit", "--match-head-commit=deadbeef"):
            with self.subTest(flag=flag):
                proc = subprocess.run(
                    [
                        "bash",
                        str(REPO / "scripts" / "merge-pr.sh"),
                        "1",
                        flag,
                        "SECRET",
                    ],
                    cwd=REPO,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertIn("match-head-commit is bound to the verified head", proc.stderr)
                self.assertNotIn("SECRET", proc.stdout + proc.stderr)

    def test_merge_wrapper_rejects_admin_flag_before_gh(self):
        for flag in ("--admin", "--admin=true"):
            with self.subTest(flag=flag):
                proc = subprocess.run(
                    ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1", flag],
                    cwd=REPO,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertIn("administrator merge is not supported", proc.stderr)

    def test_merge_wrapper_rejects_deferred_auto_merge_before_gh(self):
        for flag in ("--auto", "--auto=true", "--auto=false"):
            with self.subTest(flag=flag):
                proc = subprocess.run(
                    ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1", flag],
                    cwd=REPO,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertIn("deferred auto-merge is not supported", proc.stderr)

    def test_final_metadata_recheck_blocks_a_late_negated_close(self):
        head = "a" * 40
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            fake_bin = tmp / "bin"
            fake_bin.mkdir()
            merge_marker = tmp / "merge-called"
            gh_log = tmp / "gh.log"

            fake_git = fake_bin / "git"
            fake_git.write_text(
                "#!/bin/sh\n"
                'printf "%s\\trefs/heads/test-branch\\n" "$FAKE_HEAD"\n',
                encoding="utf-8",
            )
            fake_git.chmod(0o755)

            fake_gh = fake_bin / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$FAKE_GH_LOG"\n'
                'case "$*" in\n'
                '*"--json headRefName,headRefOid,baseRefName,state,isDraft,autoMergeRequest,id"*)\n'
                '  printf \'{"headRefName":"test-branch","headRefOid":"%s","baseRefName":"providers","state":"OPEN","isDraft":false,"autoMergeRequest":null,"id":"PR_testnode"}\\n\' "$FAKE_HEAD" ;;\n'
                '*mergeQueue*)\n'
                '  printf \'{"data":{"repository":{"mergeQueue":null}}}\n\' ;;\n'
                '*isInMergeQueue*)\n'
                '  printf \'{"data":{"node":{"isInMergeQueue":false}}}\\n\' ;;\n'
                '*graphql*)\n'
                '  printf \'%s\\n\' \'{"data":{"repository":{"pullRequest":{"title":"late edit","body":"Does not close #77","commits":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"commit":{"messageHeadline":"safe","messageBody":"Refs #77"}}]}}}}}\' ;;\n'
                '*"pr checks"*) printf \'[]\\n\' ;;\n'
                '*"pr merge"*) : > "$FAKE_MERGE_MARKER" ;;\n'
                '*) printf \'[]\\n\' ;;\n'
                "esac\n",
                encoding="utf-8",
            )
            fake_gh.chmod(0o755)

            fake_python = fake_bin / "python3"
            fake_python.write_text(
                "#!/bin/sh\n"
                'case "$*" in\n'
                '*check_pr_head_ci_run.py*) cat >/dev/null; exit 0 ;;\n'
                "esac\n"
                f'exec "{sys.executable}" "$@"\n',
                encoding="utf-8",
            )
            fake_python.chmod(0o755)

            env = os.environ.copy()
            env.update(
                PATH=f"{fake_bin}:{env['PATH']}",
                FAKE_HEAD=head,
                FAKE_GH_LOG=str(gh_log),
                FAKE_MERGE_MARKER=str(merge_marker),
                MERGE_PR_REPO="owner/repo",
                MERGE_PR_REMOTE="fake",
            )
            proc = subprocess.run(
                ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1"],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
            self.assertIn("unsafe negated closing keyword", proc.stderr)
            self.assertFalse(merge_marker.exists(), "late metadata edit was merged")
            self.assertIn("graphql", gh_log.read_text())

    def test_merge_wrapper_binds_merge_to_checked_head(self):
        head = "c" * 40
        merge_sha = "d" * 40
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            fake_bin = tmp / "bin"
            fake_bin.mkdir()
            merge_marker = tmp / "merge-called"
            merge_argv = tmp / "merge-argv"
            gh_log = tmp / "gh.log"

            fake_git = fake_bin / "git"
            fake_git.write_text(
                "#!/bin/sh\n"
                'printf "%s\\trefs/heads/test-branch\\n" "$FAKE_HEAD"\n',
                encoding="utf-8",
            )
            fake_git.chmod(0o755)

            fake_gh = fake_bin / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$FAKE_GH_LOG"\n'
                'case "$*" in\n'
                '*"--json headRefName,headRefOid,baseRefName,state,isDraft,autoMergeRequest,id"*)\n'
                '  printf \'{"headRefName":"test-branch","headRefOid":"%s","baseRefName":"providers","state":"OPEN","isDraft":false,"autoMergeRequest":null,"id":"PR_testnode"}\\n\' "$FAKE_HEAD" ;;\n'
                '*mergeQueue*)\n'
                '  printf \'{"data":{"repository":{"mergeQueue":null}}}\n\' ;;\n'
                '*isInMergeQueue*)\n'
                '  printf \'{"data":{"node":{"isInMergeQueue":false}}}\\n\' ;;\n'
                '*graphql*)\n'
                '  printf \'%s\\n\' \'{"data":{"repository":{"pullRequest":{"title":"safe","body":"Leaves #77 open","commits":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"commit":{"messageHeadline":"safe","messageBody":"Refs #77"}}]}}}}}\' ;;\n'
                '*"pr checks"*) printf \'[]\\n\' ;;\n'
                '*"pr merge"*)\n'
                '  printf "%s\\n" "$*" > "$FAKE_MERGE_ARGV"\n'
                '  case "$*" in\n'
                '  *"--match-head-commit $FAKE_HEAD"*) : > "$FAKE_MERGE_MARKER" ;;\n'
                '  *) printf "missing --match-head-commit\\n" >&2; exit 1 ;;\n'
                "  esac ;;\n"
                '*"--json mergeCommit,mergedAt,state,headRefOid"*)\n'
                '  printf \'{"mergeCommit":{"oid":"%s"},"mergedAt":"2026-01-01T00:00:00Z","state":"MERGED","headRefOid":"%s"}\\n\' "$FAKE_MERGE_SHA" "$FAKE_HEAD" ;;\n'
                '*) printf \'[]\\n\' ;;\n'
                "esac\n",
                encoding="utf-8",
            )
            fake_gh.chmod(0o755)

            fake_python = fake_bin / "python3"
            fake_python.write_text(
                "#!/bin/sh\n"
                'case "$*" in\n'
                '*check_pr_head_ci_run.py*) cat >/dev/null; exit 0 ;;\n'
                "esac\n"
                f'exec "{sys.executable}" "$@"\n',
                encoding="utf-8",
            )
            fake_python.chmod(0o755)

            env = os.environ.copy()
            env.update(
                PATH=f"{fake_bin}:{env['PATH']}",
                FAKE_HEAD=head,
                FAKE_GH_LOG=str(gh_log),
                FAKE_MERGE_MARKER=str(merge_marker),
                FAKE_MERGE_ARGV=str(merge_argv),
                FAKE_MERGE_SHA=merge_sha,
                MERGE_PR_REPO="owner/repo",
                MERGE_PR_REMOTE="fake",
            )
            proc = subprocess.run(
                ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1"],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertTrue(merge_marker.exists(), proc.stdout + proc.stderr)
            argv = merge_argv.read_text(encoding="utf-8")
            self.assertIn(f"--match-head-commit {head}", argv)
            self.assertIn("pr merge", argv)
            self.assertIn(f"merge_commit: {merge_sha}", proc.stdout)

    def test_merge_wrapper_rejects_custom_message_flags_before_gh(self):
        for flag in (
            "--subject",
            "--subject=x",
            "--body",
            "--body=x",
            "--body-file=x",
            "-t",
            "-tcustom",
            "-b",
            "-bcustom",
            "-Ffile",
        ):
            with self.subTest(flag=flag):
                proc = subprocess.run(
                    [
                        "bash",
                        str(REPO / "scripts" / "merge-pr.sh"),
                        "1",
                        flag,
                        "SECRET",
                    ],
                    cwd=REPO,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertIn("custom merge messages are not supported", proc.stderr)
                self.assertNotIn("SECRET", proc.stdout + proc.stderr)

    def test_merge_wrapper_rejects_clustered_custom_message_shorthands(self):
        for flag in ("-sbCUSTOM", "-stCUSTOM", "-sFfile"):
            with self.subTest(flag=flag):
                proc = subprocess.run(
                    ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1", flag],
                    cwd=REPO,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertIn("custom merge messages are not supported", proc.stderr)
                self.assertNotIn("CUSTOM", proc.stdout + proc.stderr)

    def test_queued_merge_is_dequeued_before_failure(self):
        """`--disable-auto` is not a dequeue; a still-OPEN PR must be
        removed from the merge queue (#513 review)."""

        head = "e" * 40
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            fake_bin = tmp / "bin"
            fake_bin.mkdir()
            merge_marker = tmp / "merge-called"
            gh_log = tmp / "gh.log"

            fake_git = fake_bin / "git"
            fake_git.write_text(
                "#!/bin/sh\n"
                'printf "%s\\trefs/heads/test-branch\\n" "$FAKE_HEAD"\n',
                encoding="utf-8",
            )
            fake_git.chmod(0o755)

            fake_gh = fake_bin / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$FAKE_GH_LOG"\n'
                'case "$*" in\n'
                '*"--json headRefName,headRefOid,baseRefName,state,isDraft,autoMergeRequest,id"*)\n'
                '  printf \'{"headRefName":"test-branch","headRefOid":"%s","baseRefName":"providers","state":"OPEN","isDraft":false,"autoMergeRequest":null,"id":"PR_testnode"}\\n\' "$FAKE_HEAD" ;;\n'
                '*dequeuePullRequest*)\n'
                '  printf \'{"data":{"dequeuePullRequest":{"clientMutationId":null}}}\\n\' ;;\n'
                '*mergeQueue*)\n'
                '  printf \'{"data":{"repository":{"mergeQueue":null}}}\n\' ;;\n'
                '*isInMergeQueue*)\n'
                '  printf \'{"data":{"node":{"isInMergeQueue":false}}}\\n\' ;;\n'
                '*graphql*)\n'
                '  printf \'%s\\n\' \'{"data":{"repository":{"pullRequest":{"title":"safe","body":"Leaves #77 open","commits":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"commit":{"messageHeadline":"safe","messageBody":"Refs #77"}}]}}}}}\' ;;\n'
                '*"pr checks"*) printf \'[]\\n\' ;;\n'
                '*"--json id"*) printf \'{"id":"PR_testnode"}\\n\' ;;\n'
                '*"pr merge"*) : > "$FAKE_MERGE_MARKER" ;;\n'
                '*"--json mergeCommit,mergedAt,state,headRefOid"*)\n'
                '  printf \'{"mergeCommit":null,"mergedAt":null,"state":"OPEN"}\\n\' ;;\n'
                '*) printf \'[]\\n\' ;;\n'
                "esac\n",
                encoding="utf-8",
            )
            fake_gh.chmod(0o755)

            fake_python = fake_bin / "python3"
            fake_python.write_text(
                "#!/bin/sh\n"
                'case "$*" in\n'
                '*check_pr_head_ci_run.py*) cat >/dev/null; exit 0 ;;\n'
                "esac\n"
                f'exec "{sys.executable}" "$@"\n',
                encoding="utf-8",
            )
            fake_python.chmod(0o755)

            env = os.environ.copy()
            env.update(
                PATH=f"{fake_bin}:{env['PATH']}",
                FAKE_HEAD=head,
                FAKE_GH_LOG=str(gh_log),
                FAKE_MERGE_MARKER=str(merge_marker),
                MERGE_PR_REPO="owner/repo",
                MERGE_PR_REMOTE="fake",
            )
            proc = subprocess.run(
                ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1"],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
            self.assertIn("deferred/queued merge is not supported", proc.stderr)
            self.assertTrue(merge_marker.exists(), proc.stdout + proc.stderr)
            logged = gh_log.read_text(encoding="utf-8")
            self.assertIn("dequeuePullRequest", logged)
            self.assertIn("pullRequestId", logged)
            self.assertIn("isInMergeQueue", logged)
            self.assertIn("--disable-auto", logged)

    def test_post_merge_view_failure_still_dequeues(self):
        """A failed post-merge `gh pr view` must still dequeue a queued
        PR instead of exiting under `set -e` (#530 review)."""

        head = "b" * 40
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            fake_bin = tmp / "bin"
            fake_bin.mkdir()
            merge_marker = tmp / "merge-called"
            gh_log = tmp / "gh.log"

            fake_git = fake_bin / "git"
            fake_git.write_text(
                "#!/bin/sh\n"
                'printf "%s\\trefs/heads/test-branch\\n" "$FAKE_HEAD"\n',
                encoding="utf-8",
            )
            fake_git.chmod(0o755)

            fake_gh = fake_bin / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$FAKE_GH_LOG"\n'
                'case "$*" in\n'
                '*"--json headRefName,headRefOid,baseRefName,state,isDraft,autoMergeRequest,id"*)\n'
                '  printf \'{"headRefName":"test-branch","headRefOid":"%s","baseRefName":"providers","state":"OPEN","isDraft":false,"autoMergeRequest":null,"id":"PR_testnode"}\\n\' "$FAKE_HEAD" ;;\n'
                '*dequeuePullRequest*)\n'
                '  printf \'{"data":{"dequeuePullRequest":{"clientMutationId":null}}}\\n\' ;;\n'
                '*mergeQueue*)\n'
                '  printf \'{"data":{"repository":{"mergeQueue":null}}}\n\' ;;\n'
                '*isInMergeQueue*)\n'
                '  printf \'{"data":{"node":{"isInMergeQueue":false}}}\\n\' ;;\n'
                '*graphql*)\n'
                '  printf \'%s\\n\' \'{"data":{"repository":{"pullRequest":{"title":"safe","body":"Leaves #77 open","commits":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"commit":{"messageHeadline":"safe","messageBody":"Refs #77"}}]}}}}}\' ;;\n'
                '*"pr checks"*) printf \'[]\\n\' ;;\n'
                '*"--json id"*) printf \'{"id":"PR_testnode"}\\n\' ;;\n'
                '*"pr merge"*) : > "$FAKE_MERGE_MARKER" ;;\n'
                '*"--json mergeCommit,mergedAt,state,headRefOid"*)\n'
                '  printf \'view failed\\n\' >&2; exit 1 ;;\n'
                '*) printf \'[]\\n\' ;;\n'
                "esac\n",
                encoding="utf-8",
            )
            fake_gh.chmod(0o755)

            fake_python = fake_bin / "python3"
            fake_python.write_text(
                "#!/bin/sh\n"
                'case "$*" in\n'
                '*check_pr_head_ci_run.py*) cat >/dev/null; exit 0 ;;\n'
                "esac\n"
                f'exec "{sys.executable}" "$@"\n',
                encoding="utf-8",
            )
            fake_python.chmod(0o755)

            env = os.environ.copy()
            env.update(
                PATH=f"{fake_bin}:{env['PATH']}",
                FAKE_HEAD=head,
                FAKE_GH_LOG=str(gh_log),
                FAKE_MERGE_MARKER=str(merge_marker),
                MERGE_PR_REPO="owner/repo",
                MERGE_PR_REMOTE="fake",
            )
            proc = subprocess.run(
                ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1"],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
            self.assertIn("deferred/queued merge is not supported", proc.stderr)
            self.assertTrue(merge_marker.exists(), proc.stdout + proc.stderr)
            logged = gh_log.read_text(encoding="utf-8")
            self.assertIn("dequeuePullRequest", logged)
            self.assertIn("--disable-auto", logged)

    def test_merge_command_error_still_dequeues(self):
        """A nonzero `gh pr merge` after enqueue must still dequeue
        (#530 review)."""

        head = "c" * 40
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            fake_bin = tmp / "bin"
            fake_bin.mkdir()
            merge_marker = tmp / "merge-called"
            gh_log = tmp / "gh.log"

            fake_git = fake_bin / "git"
            fake_git.write_text(
                "#!/bin/sh\n"
                'printf "%s\\trefs/heads/test-branch\\n" "$FAKE_HEAD"\n',
                encoding="utf-8",
            )
            fake_git.chmod(0o755)

            fake_gh = fake_bin / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$FAKE_GH_LOG"\n'
                'case "$*" in\n'
                '*"--json headRefName,headRefOid,baseRefName,state,isDraft,autoMergeRequest,id"*)\n'
                '  printf \'{"headRefName":"test-branch","headRefOid":"%s","baseRefName":"providers","state":"OPEN","isDraft":false,"autoMergeRequest":null,"id":"PR_testnode"}\\n\' "$FAKE_HEAD" ;;\n'
                '*dequeuePullRequest*)\n'
                '  printf \'{"data":{"dequeuePullRequest":{"clientMutationId":null}}}\\n\' ;;\n'
                '*mergeQueue*)\n'
                '  printf \'{"data":{"repository":{"mergeQueue":null}}}\n\' ;;\n'
                '*isInMergeQueue*)\n'
                '  printf \'{"data":{"node":{"isInMergeQueue":false}}}\\n\' ;;\n'
                '*graphql*)\n'
                '  printf \'%s\\n\' \'{"data":{"repository":{"pullRequest":{"title":"safe","body":"Leaves #77 open","commits":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"commit":{"messageHeadline":"safe","messageBody":"Refs #77"}}]}}}}}\' ;;\n'
                '*"pr checks"*) printf \'[]\\n\' ;;\n'
                '*"--json id"*) printf \'{"id":"PR_testnode"}\\n\' ;;\n'
                '*"pr merge"*)\n'
                '  : > "$FAKE_MERGE_MARKER"\n'
                '  printf \'enqueue lost\\n\' >&2; exit 1 ;;\n'
                '*"--json mergeCommit,mergedAt,state,headRefOid"*)\n'
                '  printf \'{"mergeCommit":null,"mergedAt":null,"state":"OPEN"}\\n\' ;;\n'
                '*) printf \'[]\\n\' ;;\n'
                "esac\n",
                encoding="utf-8",
            )
            fake_gh.chmod(0o755)

            fake_python = fake_bin / "python3"
            fake_python.write_text(
                "#!/bin/sh\n"
                'case "$*" in\n'
                '*check_pr_head_ci_run.py*) cat >/dev/null; exit 0 ;;\n'
                "esac\n"
                f'exec "{sys.executable}" "$@"\n',
                encoding="utf-8",
            )
            fake_python.chmod(0o755)

            env = os.environ.copy()
            env.update(
                PATH=f"{fake_bin}:{env['PATH']}",
                FAKE_HEAD=head,
                FAKE_GH_LOG=str(gh_log),
                FAKE_MERGE_MARKER=str(merge_marker),
                MERGE_PR_REPO="owner/repo",
                MERGE_PR_REMOTE="fake",
            )
            proc = subprocess.run(
                ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1"],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
            self.assertIn("deferred/queued merge is not supported", proc.stderr)
            self.assertTrue(merge_marker.exists(), proc.stdout + proc.stderr)
            logged = gh_log.read_text(encoding="utf-8")
            self.assertIn("dequeuePullRequest", logged)
            self.assertIn("--disable-auto", logged)

    def test_merge_wrapper_rejects_pr_already_in_merge_queue(self):
        """A PR already in the merge queue is rejected before CI
        verification (#530 review)."""

        head = "a" * 40
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            fake_bin = tmp / "bin"
            fake_bin.mkdir()
            merge_marker = tmp / "merge-called"
            gh_log = tmp / "gh.log"

            fake_git = fake_bin / "git"
            fake_git.write_text(
                "#!/bin/sh\n"
                'printf "%s\\trefs/heads/test-branch\\n" "$FAKE_HEAD"\n',
                encoding="utf-8",
            )
            fake_git.chmod(0o755)

            fake_gh = fake_bin / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$FAKE_GH_LOG"\n'
                'case "$*" in\n'
                '*"--json headRefName,headRefOid,baseRefName,state,isDraft,autoMergeRequest,id"*)\n'
                '  printf \'{"headRefName":"test-branch","headRefOid":"%s","baseRefName":"providers","state":"OPEN","isDraft":false,"autoMergeRequest":null,"id":"PR_testnode"}\\n\' "$FAKE_HEAD" ;;\n'
                '*mergeQueue*)\n'
                '  printf \'{"data":{"repository":{"mergeQueue":null}}}\\n\' ;;\n'
                '*isInMergeQueue*)\n'
                '  printf \'{"data":{"node":{"isInMergeQueue":true}}}\\n\' ;;\n'
                '*"pr merge"*) : > "$FAKE_MERGE_MARKER" ;;\n'
                '*) printf \'[]\\n\' ;;\n'
                "esac\n",
                encoding="utf-8",
            )
            fake_gh.chmod(0o755)

            fake_python = fake_bin / "python3"
            fake_python.write_text(
                "#!/bin/sh\n"
                f'exec "{sys.executable}" "$@"\n',
                encoding="utf-8",
            )
            fake_python.chmod(0o755)

            env = os.environ.copy()
            env.update(
                PATH=f"{fake_bin}:{env['PATH']}",
                FAKE_HEAD=head,
                FAKE_GH_LOG=str(gh_log),
                FAKE_MERGE_MARKER=str(merge_marker),
                MERGE_PR_REPO="owner/repo",
                MERGE_PR_REMOTE="fake",
            )
            proc = subprocess.run(
                ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1"],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
            self.assertIn("already in the merge queue", proc.stderr)
            self.assertFalse(merge_marker.exists(), proc.stdout + proc.stderr)

    def test_merge_wrapper_rejects_merge_queue_required_base(self):
        """A base branch with a merge queue must not enqueue (#530 review)."""

        head = "b" * 40
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            fake_bin = tmp / "bin"
            fake_bin.mkdir()
            merge_marker = tmp / "merge-called"
            gh_log = tmp / "gh.log"

            fake_git = fake_bin / "git"
            fake_git.write_text(
                "#!/bin/sh\n"
                'printf "%s\\trefs/heads/test-branch\\n" "$FAKE_HEAD"\n',
                encoding="utf-8",
            )
            fake_git.chmod(0o755)

            fake_gh = fake_bin / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$FAKE_GH_LOG"\n'
                'case "$*" in\n'
                '*"--json headRefName,headRefOid,baseRefName,state,isDraft,autoMergeRequest,id"*)\n'
                '  printf \'{"headRefName":"test-branch","headRefOid":"%s","baseRefName":"providers","state":"OPEN","isDraft":false,"autoMergeRequest":null,"id":"PR_testnode"}\\n\' "$FAKE_HEAD" ;;\n'
                '*mergeQueue*)\n'
                '  printf \'{"data":{"repository":{"mergeQueue":{"id":"MQ_1"}}}}\\n\' ;;\n'
                '*isInMergeQueue*)\n'
                '  printf \'{"data":{"node":{"isInMergeQueue":false}}}\\n\' ;;\n'
                '*"pr merge"*) : > "$FAKE_MERGE_MARKER" ;;\n'
                '*) printf \'[]\\n\' ;;\n'
                "esac\n",
                encoding="utf-8",
            )
            fake_gh.chmod(0o755)

            fake_python = fake_bin / "python3"
            fake_python.write_text(
                "#!/bin/sh\n"
                f'exec "{sys.executable}" "$@"\n',
                encoding="utf-8",
            )
            fake_python.chmod(0o755)

            env = os.environ.copy()
            env.update(
                PATH=f"{fake_bin}:{env['PATH']}",
                FAKE_HEAD=head,
                FAKE_GH_LOG=str(gh_log),
                FAKE_MERGE_MARKER=str(merge_marker),
                MERGE_PR_REPO="owner/repo",
                MERGE_PR_REMOTE="fake",
            )
            proc = subprocess.run(
                ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1"],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
            self.assertIn("requires a merge queue", proc.stderr)
            self.assertFalse(merge_marker.exists(), proc.stdout + proc.stderr)
            self.assertIn("mergeQueue", gh_log.read_text(encoding="utf-8"))

    def test_title_body_edit_between_scans_blocks_merge(self):
        """A clean-to-clean title/body edit still changes the digest
        (#513 review)."""

        head = "f" * 40
        with tempfile.TemporaryDirectory() as raw_tmp:
            tmp = Path(raw_tmp)
            fake_bin = tmp / "bin"
            fake_bin.mkdir()
            merge_marker = tmp / "merge-called"
            gh_log = tmp / "gh.log"
            gql_count = tmp / "gql-count"
            gql_count.write_text("0\n", encoding="utf-8")

            fake_git = fake_bin / "git"
            fake_git.write_text(
                "#!/bin/sh\n"
                'printf "%s\\trefs/heads/test-branch\\n" "$FAKE_HEAD"\n',
                encoding="utf-8",
            )
            fake_git.chmod(0o755)

            fake_gh = fake_bin / "gh"
            fake_gh.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$FAKE_GH_LOG"\n'
                'case "$*" in\n'
                '*"--json headRefName,headRefOid,baseRefName,state,isDraft,autoMergeRequest,id"*)\n'
                '  printf \'{"headRefName":"test-branch","headRefOid":"%s","baseRefName":"providers","state":"OPEN","isDraft":false,"autoMergeRequest":null,"id":"PR_testnode"}\\n\' "$FAKE_HEAD" ;;\n'
                '*mergeQueue*)\n'
                '  printf \'{"data":{"repository":{"mergeQueue":null}}}\n\' ;;\n'
                '*isInMergeQueue*)\n'
                '  printf \'{"data":{"node":{"isInMergeQueue":false}}}\\n\' ;;\n'
                '*graphql*)\n'
                "  n=$(($(cat \"$FAKE_GQL_COUNT\") + 1))\n"
                '  printf "%s\\n" "$n" > "$FAKE_GQL_COUNT"\n'
                '  if [ "$n" -eq 1 ]; then body="Leaves #77 open"; else body="Leaves #88 open"; fi\n'
                '  printf \'{"data":{"repository":{"pullRequest":{"title":"safe","body":"%s","commits":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"commit":{"messageHeadline":"safe","messageBody":"Refs #77"}}]}}}}}\\n\' "$body" ;;\n'
                '*"pr checks"*) printf \'[]\\n\' ;;\n'
                '*"pr merge"*) : > "$FAKE_MERGE_MARKER" ;;\n'
                '*) printf \'[]\\n\' ;;\n'
                "esac\n",
                encoding="utf-8",
            )
            fake_gh.chmod(0o755)

            fake_python = fake_bin / "python3"
            fake_python.write_text(
                "#!/bin/sh\n"
                'case "$*" in\n'
                '*check_pr_head_ci_run.py*) cat >/dev/null; exit 0 ;;\n'
                "esac\n"
                f'exec "{sys.executable}" "$@"\n',
                encoding="utf-8",
            )
            fake_python.chmod(0o755)

            env = os.environ.copy()
            env.update(
                PATH=f"{fake_bin}:{env['PATH']}",
                FAKE_HEAD=head,
                FAKE_GH_LOG=str(gh_log),
                FAKE_MERGE_MARKER=str(merge_marker),
                FAKE_GQL_COUNT=str(gql_count),
                MERGE_PR_REPO="owner/repo",
                MERGE_PR_REMOTE="fake",
            )
            proc = subprocess.run(
                ["bash", str(REPO / "scripts" / "merge-pr.sh"), "1"],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
            self.assertIn("title/body changed after the closing-keyword scan", proc.stderr)
            self.assertFalse(merge_marker.exists(), proc.stdout + proc.stderr)


if __name__ == "__main__":
    unittest.main()
