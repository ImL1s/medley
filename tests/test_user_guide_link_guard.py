"""Keep user-guide links resolvable after install extraction.

Startup extracts the user guide to `<grok_home>/docs/user-guide/`
(`extract_user_guide_docs`, crates/codegen/xai-grok-pager/src/docs.rs), and
only the managed `NN-*.md` guides listed in `USER_GUIDE` are written there.
`REFERENCE_DOCS` is documented as "not extracted to disk", and
`docs/architecture/` is neither extracted nor bundled -- `architecture` does
not appear in `docs.rs` at all.

So a relative link that escapes the guide directory resolves in a repository
checkout and is broken for every installed reader, human or agent. #385 added
one at `16-subagents.md` pointing five levels up at
`docs/architecture/native-subagent-route-contract.md`; nothing caught it, and
it survived long enough to be copied into a second page before the Codex
review on PR #402 found it. Both were changed to absolute URLs.

Scope and deliberate omissions:

- Only `crates/codegen/xai-grok-pager/docs/user-guide/` is checked. The
  `tutorial/` pages and the reference docs beside them are not extracted, so
  the invariant does not apply to them.
- Inline links, reference-style definitions, and HTML `href` attributes are
  all scanned. Only inline links exist today; the other two are the obvious
  ways the same mistake could return.
- Fenced blocks and inline code spans are blanked before scanning, so a page
  that *documents* this rule with a `](../` example does not trip it. Fence
  recognition follows CommonMark indentation: at four spaces a line is an
  indented code block, not a fence. Indented code blocks are deliberately
  *not* blanked -- a link inside one is reported rather than ignored, which
  errs towards a visible false positive instead of a silent broken link.
- Link targets are normalised lexically, never resolved on disk. This guard
  is about escaping the directory, not about whether the target exists.
"""

from __future__ import annotations

import posixpath
import re
import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parent.parent
GUIDE = REPO / "crates" / "codegen" / "xai-grok-pager" / "docs" / "user-guide"
DOCS_RS = REPO / "crates" / "codegen" / "xai-grok-pager" / "src" / "docs.rs"

# Inline `[text](target)`, image `![alt](target)`, and a trailing "title".
INLINE_LINK = re.compile(r"!?\[[^\]]*\]\(\s*<?([^)>\s]+)>?(?:\s+[\"'(][^)]*)?\)")
# Reference-style definition: `[label]: target`.
REFERENCE_DEF = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*<?([^\s>]+)>?", re.MULTILINE)
# HTML anchors, which markdown renderers pass through. The attribute value
# may be double-quoted, single-quoted, or bare -- all three are valid HTML.
HTML_HREF = re.compile(
    r"""<a\s[^>]*href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>"'`]+))""",
    re.IGNORECASE,
)

# `scheme:` prefix -- https:, mailto:, file: and friends are not relative.
HAS_SCHEME = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.\-]*:")
INLINE_CODE = re.compile(r"`+[^`\n]*`+")
# A fence may be indented up to three spaces. At four it is an indented code
# block, not a fence -- treating one as an opener would blank the rest of the
# page and hide real links behind it.
FENCE_LINE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")


def _blank_code(text: str) -> str:
    """Blank fenced blocks and inline code, preserving line numbering."""
    out: list[str] = []
    fence: str | None = None
    for line in text.split("\n"):
        match = FENCE_LINE.match(line)
        if fence is None:
            # A backtick fence's info string may not contain a backtick.
            if match and not (match.group(1)[0] == "`" and "`" in match.group(2)):
                fence = match.group(1)
                out.append("")
                continue
            out.append(line)
        else:
            out.append("")
            # A closer uses the same character, is at least as long, and
            # carries nothing but trailing whitespace.
            if (
                match
                and match.group(1)[0] == fence[0]
                and len(match.group(1)) >= len(fence)
                and not match.group(2).strip()
            ):
                fence = None
    # Inline spans cannot contain a newline, so this keeps the line count.
    return INLINE_CODE.sub("", "\n".join(out))


def _target_of(match: re.Match[str]) -> str:
    """The first group that matched; href alternation yields three."""
    return next(group for group in match.groups() if group is not None)


def _escapes(rel_dir: str, target: str) -> bool:
    """True when `target`, read from a file in `rel_dir`, leaves the guide."""
    if target.startswith("/"):
        return True
    normalised = posixpath.normpath(posixpath.join(rel_dir, target))
    return normalised == ".." or normalised.startswith("../")


def _offending_links() -> list[str]:
    findings: list[str] = []
    for path in sorted(GUIDE.rglob("*.md")):
        text = _blank_code(path.read_text(encoding="utf-8"))
        rel_dir = posixpath.dirname(path.relative_to(GUIDE).as_posix()) or "."
        for pattern in (INLINE_LINK, REFERENCE_DEF, HTML_HREF):
            for match in pattern.finditer(text):
                target = _target_of(match).split("#", 1)[0].strip()
                if not target or target.startswith("//") or HAS_SCHEME.match(target):
                    continue
                if _escapes(rel_dir, target):
                    line = text.count("\n", 0, match.start()) + 1
                    findings.append(
                        f"{path.relative_to(REPO).as_posix()}:{line}: {target}"
                    )
    return findings


class UserGuideLinkGuardTests(unittest.TestCase):
    def test_extraction_premise_still_holds(self) -> None:
        """The invariant only matters while the guide is extracted this way."""
        text = DOCS_RS.read_text(encoding="utf-8")
        self.assertIn(
            "pub fn extract_user_guide_docs",
            text,
            "docs.rs no longer extracts the user guide -- re-check this guard",
        )
        self.assertIn(
            '.join("user-guide")',
            text,
            "the extraction target changed -- re-check this guard",
        )
        self.assertIn(
            "not extracted to disk",
            text,
            "REFERENCE_DOCS no longer documents itself as unextracted",
        )
        self.assertNotIn(
            "architecture",
            text,
            "docs.rs now mentions architecture docs; if they are bundled or "
            "extracted, this guard's premise needs revisiting",
        )

    def test_guide_directory_is_populated(self) -> None:
        """A moved or renamed directory must fail, not silently pass."""
        self.assertTrue(GUIDE.is_dir(), f"missing guide directory: {GUIDE}")
        pages = sorted(GUIDE.rglob("*.md"))
        self.assertGreater(len(pages), 1, "guide directory scanned no pages")
        names = {page.name for page in pages}
        self.assertIn("README.md", names, "guide index is missing")
        self.assertTrue(
            any(re.fullmatch(r"\d\d-.+\.md", name) for name in names),
            "no managed NN-*.md guides found; the tree layout changed",
        )

    def test_no_relative_link_escapes_the_guide_directory(self) -> None:
        findings = _offending_links()
        self.assertEqual(
            findings,
            [],
            "user-guide pages are extracted to <state-dir>/docs/user-guide/, "
            "where a relative link out of that directory does not resolve. "
            "Use an absolute URL instead:\n  " + "\n  ".join(findings),
        )


class LinkDetectionTests(unittest.TestCase):
    """Tests for the detector itself.

    The `new-test-filters` job carries the same reasoning (ci.yml): a checker
    without its own tests is an unverified guard against unverified content.
    """

    def _flags(self, body: str) -> bool:
        text = _blank_code(body)
        for pattern in (INLINE_LINK, REFERENCE_DEF, HTML_HREF):
            for match in pattern.finditer(text):
                target = _target_of(match).split("#", 1)[0].strip()
                if not target or target.startswith("//") or HAS_SCHEME.match(target):
                    continue
                if _escapes(".", target):
                    return True
        return False

    def test_flags_escaping_forms(self) -> None:
        for label, body in (
            ("inline", "See [x](../../docs/architecture/foo.md)."),
            ("dot-slash", "See [x](./../docs/architecture/foo.md)."),
            ("root-absolute", "See [x](/docs/architecture/foo.md)."),
            ("image", "![d](../../assets/logo.png)"),
            ("reference definition", "[spec]: ../../../docs/architecture/foo.md"),
            ("html href", '<a href="../../docs/architecture/foo.md">x</a>'),
            ("html href, single-quoted", "<a href='../../docs/architecture/foo.md'>x</a>"),
            ("html href, unquoted", "<a href=../../docs/architecture/foo.md>x</a>"),
        ):
            with self.subTest(form=label):
                self.assertTrue(self._flags(body + "\n"), f"{label} not flagged")

    def test_allows_links_that_resolve_after_extraction(self) -> None:
        for label, body in (
            ("sibling", "See [x](11-custom-models.md)."),
            ("anchored sibling", "See [x](05-configuration.md#file-locations)."),
            ("bare anchor", "See [x](#credential-resolution)."),
            ("absolute url", "See [x](https://github.com/ImL1s/medley)."),
            ("mailto", "Mail [x](mailto:someone@example.com)."),
            ("html href, sibling", '<a href="11-custom-models.md">x</a>'),
            ("html href, unquoted sibling", "<a href=11-custom-models.md>x</a>"),
        ):
            with self.subTest(form=label):
                self.assertFalse(self._flags(body + "\n"), f"{label} wrongly flagged")

    def test_ignores_code_that_documents_this_rule(self) -> None:
        """A page explaining the rule must not trip it."""
        self.assertFalse(self._flags("```sh\ngrep '](../' *.md\n```\n"))
        self.assertFalse(self._flags("~~~\n[x](../../escaped.md)\n~~~\n"))
        self.assertFalse(self._flags("Run `grep -n '](../' *.md` here.\n"))

    def test_over_indented_backticks_are_not_a_fence(self) -> None:
        """An indented example must not blank the rest of the page.

        A four-space-indented line of backticks is an indented code block, not
        a fence. Treating it as an unclosed opener would blank everything
        after it and hide a real escaping link (Codex review, #404).
        """
        body = (
            "Example:\n"
            "\n"
            "    ```sh\n"
            "    echo hi\n"
            "\n"
            "See [x](../../docs/architecture/foo.md).\n"
        )
        self.assertTrue(self._flags(body), "link after an indented example was hidden")

    def test_fence_indented_up_to_three_spaces_still_fences(self) -> None:
        self.assertFalse(self._flags("   ```\n   [x](../../escaped.md)\n   ```\n"))

    def test_closing_fence_must_match_the_opener(self) -> None:
        # A shorter run, or a different character, does not close the block.
        self.assertFalse(self._flags("````\n```\n[x](../../escaped.md)\n````\n"))
        self.assertFalse(self._flags("~~~\n```\n[x](../../escaped.md)\n~~~\n"))
        # A longer run of the same character does close it.
        self.assertTrue(self._flags("```\nhi\n````\n[x](../../escaped.md)\n"))

    def test_blanking_preserves_line_numbers(self) -> None:
        body = "one\n```\nfenced\n```\nfour\n[x](../../escaped.md)\n"
        blanked = _blank_code(body)
        self.assertEqual(body.count("\n"), blanked.count("\n"))
        match = INLINE_LINK.search(blanked)
        assert match is not None
        self.assertEqual(blanked.count("\n", 0, match.start()) + 1, 6)


if __name__ == "__main__":
    unittest.main()
