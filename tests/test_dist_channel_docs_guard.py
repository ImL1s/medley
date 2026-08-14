"""Keep README distChannel guidance aligned with the release stamp.

Release artifacts are rejected unless `medley version --json` contains
`"distChannel":"medley"`. `providers` is the git branch and tag suffix, not
the packaged product/channel. README used to say the expected stamp was
`providers` (issue #365).
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parent.parent
README = REPO / "README.md"
RELEASE_YML = REPO / ".github" / "workflows" / "release.yml"

# Compact JSON fragment the release job requires in `version --json`.
RELEASE_DIST_CHANNEL_JSON = '"distChannel":"medley"'
# Glob form as written in release.yml (`*'"distChannel":"medley"'*`).
RELEASE_DIST_CHANNEL_CASE = "*'\"distChannel\":\"medley\"'*"

# The stale contract: the distChannel field is expected to be providers.
STALE_PROVIDERS_EXPECTATION = re.compile(
    r"distChannel[^\n]*(?:"
    r"expected to be[`\s]+providers|"
    r"expected[`\s]+providers|"
    r"must be[`\s]+providers|"
    r"should be[`\s]+providers"
    r")",
    re.IGNORECASE,
)
DISTCHANNEL_BULLET = re.compile(r"^-\s+`distChannel`.+$", re.MULTILINE)


class DistChannelDocsGuardTests(unittest.TestCase):
    def test_release_yml_still_requires_distchannel_medley(self) -> None:
        text = RELEASE_YML.read_text(encoding="utf-8")
        self.assertIn(RELEASE_DIST_CHANNEL_JSON, text)
        self.assertIn(RELEASE_DIST_CHANNEL_CASE, text)

    def test_readme_does_not_expect_distchannel_providers(self) -> None:
        text = README.read_text(encoding="utf-8")
        match = STALE_PROVIDERS_EXPECTATION.search(text)
        self.assertIsNone(
            match,
            "README must not say distChannel is expected to be providers"
            + (f": {match.group(0)!r}" if match else ""),
        )

    def test_readme_distchannel_bullet_names_medley(self) -> None:
        text = README.read_text(encoding="utf-8")
        match = DISTCHANNEL_BULLET.search(text)
        self.assertIsNotNone(match, "README must have a distChannel bullet")
        self.assertIn(
            "medley",
            match.group(0),
            "the distChannel bullet must name medley",
        )


if __name__ == "__main__":
    unittest.main()
