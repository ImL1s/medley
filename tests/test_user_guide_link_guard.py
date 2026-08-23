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

Design: suppress nothing, parse as little as possible.

For a guard, the two failure modes are not symmetric. A false positive is
loud, immediate, and fixed in one line. A false negative is silent, and it
is the exact failure this guard exists to prevent -- #385's broken link
survived unnoticed long enough to be copied into a second page. So every
choice here prefers a visible false positive over a silent miss.

That principle settled two questions that six Codex review rounds on #404
kept reopening. Those rounds found eleven genuine false negatives, and not
one was in the containment check -- every one lived in code that suppressed
text before scanning or tried to parse link syntax precisely. Suppressing
text can only ever hide a real link; parsing labels can only ever lose one.

So neither is done:

- Nothing is blanked. Not fenced blocks, not inline code spans, not
  indented blocks, not HTML comments. Fence blanking was removed after
  measuring it -- stubbing it to the identity function produced
  byte-identical results across all 25 pages, and the guide contains no
  literal `](..` at all. It was defending content that does not exist,
  while a four-space-indented pseudo-fence could blank the rest of a page.
  A future page documenting this rule with a literal `](../x)` example is
  reported; it should describe the path in prose or use a placeholder.
- Link labels are not parsed. Detection anchors on `](` and `]:`, so
  escaped or nested brackets in a label cannot lose the destination. An
  unrelated `](` in prose is flagged instead.

What is scanned, and what is not:

- Only `crates/codegen/xai-grok-pager/docs/user-guide/`. The `tutorial/`
  pages and the reference docs beside them are not extracted, so the
  invariant does not apply to them.
- Inline destinations, reference-style definitions, and HTML `href`
  attributes -- quoted, single-quoted, or bare. Only inline links exist
  today; the others are the obvious ways the same mistake could return.
- Destinations are read as a reader's tool would follow them: character
  references and backslash escapes decoded, angle-bracketed destinations
  may contain spaces, and a fragment or query dropped before the path is
  classified. Decoding can only reveal more parent components, never fewer.
- Targets are normalised lexically, never resolved on disk. This guard is
  about escaping the directory, not about whether the target exists.
"""

from __future__ import annotations

import html
import posixpath
import re
import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parent.parent
GUIDE = REPO / "crates" / "codegen" / "xai-grok-pager" / "docs" / "user-guide"
DOCS_RS = REPO / "crates" / "codegen" / "xai-grok-pager" / "src" / "docs.rs"

BACKSLASH_ESCAPE = re.compile(r"\\(.)")
# Inline and image destinations. Anchored on `](` rather than on the label:
# labels may contain escaped or nested brackets, and any attempt to match them
# can only lose links. An unrelated `](` in prose is flagged instead, which is
# the safe direction.
INLINE_LINK = re.compile(
    r"\]\(\s*(?:<([^>\n]*)>|([^)>\s]+))(?:\s+[\"'(][^)]*)?\)"
)
# Reference-style definition: `[label]: target`. Anchored on `]:` for the same
# reason inline links are anchored on `](` -- labels may contain brackets.
REFERENCE_DEF = re.compile(
    r"^ {0,3}\[[^\n]*\]:[ \t]*(?:<([^>\n]*)>|([^\s>]+))", re.MULTILINE
)
# HTML anchors, which markdown renderers pass through. The attribute value
# may be double-quoted, single-quoted, or bare -- all three are valid HTML.
HTML_HREF = re.compile(
    r"""<a\s[^>]*href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>"'`]+))""",
    re.IGNORECASE,
)

# `scheme:` prefix -- https:, mailto:, file: and friends are not relative.
HAS_SCHEME = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.\-]*:")
def _target_of(match: re.Match[str]) -> str:
    """The destination a reader's tool would follow.

    Patterns alternate over quoting and bracketing styles, so take whichever
    group matched. Character references and backslash escapes are resolved
    here: a browser follows `..&#47;..&#47;x.md` and Markdown renders
    `..\\/..\\/x.md`, both as `../../x.md`. Decoding can only reveal more
    parent components, never fewer, so it cannot mask an escape.
    """
    raw = next(group for group in match.groups() if group is not None)
    decoded = BACKSLASH_ESCAPE.sub(r"\1", html.unescape(raw))
    # A fragment or query is not part of the path: `..?download` and `..#top`
    # both address the parent directory.
    return re.split(r"[#?]", decoded, maxsplit=1)[0].strip()


def _escapes(rel_dir: str, target: str) -> bool:
    """True when `target`, read from a file in `rel_dir`, leaves the guide."""
    if target.startswith("/"):
        return True
    normalised = posixpath.normpath(posixpath.join(rel_dir, target))
    return normalised == ".." or normalised.startswith("../")


def _offending_links() -> list[str]:
    findings: list[str] = []
    for path in sorted(GUIDE.rglob("*.md")):
        text = path.read_text(encoding="utf-8")
        rel_dir = posixpath.dirname(path.relative_to(GUIDE).as_posix()) or "."
        for pattern in (INLINE_LINK, REFERENCE_DEF, HTML_HREF):
            for match in pattern.finditer(text):
                target = _target_of(match)
                if not target or target.startswith("//") or HAS_SCHEME.match(target):
                    continue
                if _escapes(rel_dir, target):
                    line = text.count("\n", 0, match.start()) + 1
                    findings.append(
                        f"{path.relative_to(REPO).as_posix()}:{line}: {target}"
                    )
    return findings


REMEDIATION = """
{listing}

Each link above leaves {guide}/.
That directory is extracted to <state-dir>/docs/user-guide/ when the pager
starts; nothing above it is. So the link resolves in a repository checkout
and is broken for everyone reading an installed copy.

If it is a real link, use an absolute URL:
    https://github.com/ImL1s/medley/blob/providers/<path-from-repo-root>

If it is a deliberate example rather than a real link, it must not appear as
a literal `](../...)`. Describe the path in prose, or use a placeholder such
as `](<parent>/...)`. A fenced code block does NOT exempt it: this guard
does no suppression at all, deliberately -- anything that can hide text from
it can also hide a broken link, which is the failure this guard exists to
prevent. See the module docstring.
"""


def _remediation(findings: list[str]) -> str:
    listing = "\n".join(f"  {finding}" for finding in findings)
    return REMEDIATION.format(
        listing=listing, guide=GUIDE.relative_to(REPO).as_posix()
    )


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
        self.assertEqual(findings, [], _remediation(findings))


class LinkDetectionTests(unittest.TestCase):
    """Tests for the detector itself.

    The `new-test-filters` job carries the same reasoning (ci.yml): a checker
    without its own tests is an unverified guard against unverified content.
    """

    def _flags(self, body: str) -> bool:
        text = body
        for pattern in (INLINE_LINK, REFERENCE_DEF, HTML_HREF):
            for match in pattern.finditer(text):
                target = _target_of(match)
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

    def test_fences_do_not_exempt_a_link(self) -> None:
        """Nothing is suppressed, fenced code blocks included.

        Fence blanking was removed after measuring it: stubbing it to the
        identity function produced byte-identical results across all 25
        pages, and the guide contains no literal `](..` at all. It was
        defending content that does not exist, and every one of the eleven
        false negatives Codex found on #404 lived in that suppression and
        parsing machinery -- including a four-space-indented pseudo-fence
        that blanked the rest of a page.

        The trade is deliberate. A fenced example is now reported: loud,
        immediate, one line to fix. A suppressed real link is silent, and
        that is exactly how #385's broken link survived long enough to be
        copied into a second page.
        """
        for label, body in (
            ("backtick fence", "```\n[x](../../escaped.md)\n```"),
            ("tilde fence", "~~~\n[x](../../escaped.md)\n~~~"),
            ("indented block", "    [x](../../escaped.md)"),
            ("html comment", "<!-- [x](../../escaped.md) -->"),
        ):
            with self.subTest(form=label):
                self.assertTrue(self._flags(body + "\n"), f"{label} was exempted")

    def test_inline_backticks_never_hide_a_link(self) -> None:
        """Code spans are not blanked, so no backtick trick can hide a link.

        Successive Codex rounds on #404 found three ways to make span
        blanking swallow a live link -- mismatched run lengths, and a
        backslash-escaped opener. Not blanking spans removes the class
        instead of patching each corner, at the cost of flagging an inline
        example. That is the safe direction, and no page has one.
        """
        for label, body in (
            ("mismatched runs", "`oops [x](../../escaped.md) ``code``"),
            ("escaped opener", "\\` `oops [x](../../escaped.md) `"),
            ("matched span", "`[x](../../escaped.md)`"),
        ):
            with self.subTest(form=label):
                self.assertTrue(self._flags(body + "\n"), f"{label} hid the link")

    def test_escaped_destinations_are_resolved_before_classifying(self) -> None:
        """`..\\/..\\/x.md` renders as `../../x.md` (Codex, #404)."""
        self.assertTrue(self._flags("[x](..\\/..\\/escaped.md)\n"))

    def test_awkward_link_labels_still_yield_their_destination(self) -> None:
        """Labels are not parsed, so brackets in them cannot lose a link."""
        for label, body in (
            ("escaped bracket", "[a \\] b](../../escaped.md)"),
            ("nested brackets", "[a [b] c](../../escaped.md)"),
            ("image", "![a](../../escaped.png)"),
            ("empty label", "[](../../escaped.md)"),
        ):
            with self.subTest(form=label):
                self.assertTrue(self._flags(body + "\n"), f"{label} was skipped")

    def test_reference_labels_are_not_parsed_either(self) -> None:
        """A bracket in a reference label must not lose the definition."""
        self.assertTrue(self._flags("[a \\] b]: ../../escaped.md\n"))
        self.assertFalse(self._flags("[spec]: 11-custom-models.md\n"))

    def test_angle_bracket_destinations_may_contain_spaces(self) -> None:
        """CommonMark allows `<...>` destinations to hold spaces."""
        self.assertTrue(self._flags("[x](<../../foo bar.md>)\n"))
        self.assertFalse(self._flags("[x](<11-custom-models.md>)\n"))

    def test_character_references_are_decoded(self) -> None:
        """A browser follows `..&#47;..&#47;x.md` as `../../x.md`."""
        self.assertTrue(self._flags('<a href="..&#47;..&#47;escaped.md">x</a>\n'))
        self.assertFalse(self._flags('<a href="11-custom&#45;models.md">x</a>\n'))

    def test_query_and_fragment_are_not_path_components(self) -> None:
        """`..?download` addresses the parent directory (Codex, #404)."""
        self.assertTrue(self._flags("[x](..?download)\n"))
        self.assertTrue(self._flags("[x](../..?a=b)\n"))
        self.assertTrue(self._flags("[x](..#top)\n"))
        self.assertFalse(self._flags("[x](11-custom-models.md?raw=1)\n"))

    def test_reported_line_is_the_line_the_link_is_on(self) -> None:
        """Failures cite a line an author can jump to."""
        body = "one\n```\nfenced\n```\nfour\n[x](../../escaped.md)\n"
        match = INLINE_LINK.search(body)
        assert match is not None
        self.assertEqual(body.count("\n", 0, match.start()) + 1, 6)

    def test_failure_message_says_how_to_fix_it(self) -> None:
        """A guard that flags without instructing gets deleted, not obeyed."""
        message = _remediation(["some/page.md:12: ../../escaped.md"])
        self.assertIn("some/page.md:12: ../../escaped.md", message)
        self.assertIn("absolute URL", message)
        self.assertIn("https://github.com/ImL1s/medley/blob/providers/", message)
        self.assertIn("deliberate example", message)
        self.assertIn("does NOT exempt", message)


if __name__ == "__main__":
    unittest.main()
