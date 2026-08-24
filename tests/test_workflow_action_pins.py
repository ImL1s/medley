"""Reject floating GitHub Action tags in shipped workflows (issue #30).

Every `uses:` that names a remote action — first-party `actions/*` included —
must pin a 40-character commit SHA. The human-readable tag in a trailing
comment is documentation only. Local composite actions (`./…`) are exempt.
"""

from __future__ import annotations

import re
import tempfile
import textwrap
import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parent.parent
WORKFLOWS = REPO / ".github" / "workflows"

# `uses: owner/repo@ref` or `uses: owner/repo/path@ref`, optional quotes.
USES_LINE = re.compile(
    r"""^\s*(?:-\s+)?uses:\s*['"]?(?P<ref>[^'"\s#]+)['"]?"""
)
SHA = re.compile(r"^[0-9a-fA-F]{40}$")


def iter_uses(text: str) -> list[tuple[int, str]]:
    hits: list[tuple[int, str]] = []
    for lineno, line in enumerate(text.splitlines(), 1):
        if line.lstrip().startswith("#"):
            continue
        match = USES_LINE.match(line)
        if match:
            hits.append((lineno, match.group("ref")))
    return hits


def is_local_action(ref: str) -> bool:
    return ref.startswith("./") or ref.startswith(".\\")


def pin_ok(ref: str) -> bool:
    if is_local_action(ref):
        return True
    if "@" not in ref:
        return False
    spec = ref.rsplit("@", 1)[1]
    return SHA.fullmatch(spec) is not None


def unpinned_uses(text: str) -> list[tuple[int, str]]:
    return [(lineno, ref) for lineno, ref in iter_uses(text) if not pin_ok(ref)]


class PinOk(unittest.TestCase):
    def test_full_sha_is_accepted(self) -> None:
        sha = "fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09"
        self.assertTrue(pin_ok(f"actions/checkout@{sha}"))
        self.assertTrue(pin_ok(f"dtolnay/rust-toolchain@{sha}"))

    def test_floating_tag_is_rejected(self) -> None:
        self.assertFalse(pin_ok("actions/checkout@v5"))
        self.assertFalse(pin_ok("Swatinem/rust-cache@v2"))
        self.assertFalse(pin_ok("dtolnay/rust-toolchain@master"))

    def test_short_sha_is_rejected(self) -> None:
        self.assertFalse(pin_ok("actions/checkout@fbc6f3992d24b796d5a048ff"))

    def test_local_action_is_allowed(self) -> None:
        self.assertTrue(pin_ok("./.github/actions/local"))
        self.assertTrue(pin_ok("./.github/actions/local@v1"))

    def test_docker_ref_is_rejected(self) -> None:
        self.assertFalse(pin_ok("docker://alpine:3.20"))


class ParseUses(unittest.TestCase):
    def test_extracts_uses_and_ignores_comments(self) -> None:
        text = textwrap.dedent(
            """\
            jobs:
              one:
                steps:
                  - uses: actions/checkout@v5
                  # uses: actions/checkout@v4
                    uses: dtolnay/rust-toolchain@master
                  - name: Cache
                    uses: "Swatinem/rust-cache@v2" # trailing
            """
        )
        self.assertEqual(
            [ref for _, ref in iter_uses(text)],
            [
                "actions/checkout@v5",
                "dtolnay/rust-toolchain@master",
                "Swatinem/rust-cache@v2",
            ],
        )

    def test_unpinned_uses_reports_line_numbers(self) -> None:
        text = textwrap.dedent(
            """\
            - uses: actions/checkout@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
            - uses: actions/setup-python@v5
            """
        )
        self.assertEqual(unpinned_uses(text), [(2, "actions/setup-python@v5")])


class ShippedWorkflows(unittest.TestCase):
    def _workflow_paths(self) -> list[Path]:
        paths = sorted(
            [*WORKFLOWS.glob("*.yml"), *WORKFLOWS.glob("*.yaml")],
            key=lambda p: p.name,
        )
        self.assertTrue(paths, f"no workflow files under {WORKFLOWS}")
        return paths

    def test_workflows_directory_contains_uses_lines(self) -> None:
        """An empty scan is not a pass — that is how a broken glob goes green."""
        total = 0
        for path in self._workflow_paths():
            total += len(iter_uses(path.read_text(encoding="utf-8")))
        self.assertGreater(total, 0, "parsed zero uses: lines from shipped workflows")

    def test_every_remote_action_is_sha_pinned(self) -> None:
        failures: list[str] = []
        for path in self._workflow_paths():
            rel = path.relative_to(REPO)
            for lineno, ref in unpinned_uses(path.read_text(encoding="utf-8")):
                failures.append(f"{rel}:{lineno}: {ref}")
        self.assertEqual(
            failures,
            [],
            "unpinned GitHub Action uses: (need a 40-char commit SHA):\n"
            + "\n".join(failures),
        )

    def test_fixture_workflow_with_a_tag_fails_the_same_check(self) -> None:
        """The repo-scan assertion is only as good as this parser on a known-bad file."""
        with tempfile.TemporaryDirectory() as tmp:
            bad = Path(tmp) / "ci.yml"
            bad.write_text("- uses: actions/checkout@v5\n", encoding="utf-8")
            self.assertEqual(
                unpinned_uses(bad.read_text(encoding="utf-8")),
                [(1, "actions/checkout@v5")],
            )


if __name__ == "__main__":
    unittest.main()
