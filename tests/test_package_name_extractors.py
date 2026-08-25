"""Every `[package] name` extractor in `scripts/` tolerates the same forms (#494).

Independently-written regexes classified the same identifier -- a Cargo
manifest's `name = "..."` line -- and drifted apart. `#464` taught one of
them to tolerate a trailing `# comment`; `#486` taught that same one to
tolerate a quoted key. Neither fix reached the others, and nothing reported
that, because every copy is correct on the plain spelling each real manifest
in this tree happens to use. The divergence is only visible on a spelling
nobody has written yet -- which is exactly when a guard silently drops a
crate from its corpus and reports `new-gaps: 0`.

This does not check how many copies exist. It checks the property the copies
kept getting wrong: **anything in `scripts/` that behaves as a reader of that
line accepts every TOML spelling of it.** A further copy therefore cannot be
added un-fixed, whatever it is named and however it is spelled, because it is
found by what it does rather than by what it says.

How "behaves as a package-name extractor" is decided, and why that is not a
literal search: every string constant in every `scripts/*.py` is compiled as
a regex and *run*. A constant qualifies when it captures `foo` from
`name = "foo"` and captures nothing from `license = "MIT"` or
`version = "0.1.0"` -- i.e. when it keys on the `name` field rather than on
any quoted value. #494 named three copies, found by reading the scripts;
running the patterns instead turned up a fourth, in
`check_new_tests_are_filtered.py`, which reads a `[[bin]]` target's name
rather than a `[package]` one and so does not answer to a search for "the
package-name regex" at all. A text search for `name\\s*=` would also miss a
copy spelled `[^\\s]+\\s*=` or `(?P<key>name)`; running the pattern cannot,
because matching is the same mechanism production uses.

What this does NOT find, stated so the green is not read as more than it is:
an extractor built without a regex (`line.split("=")`), one assembled at
runtime from concatenated fragments or an f-string, or one living outside
`scripts/*.py`. `scripts/*.sh` is not scanned either. The corpus tests in
`tests/` are deliberately excluded: their hand-rolled scanners exist *to be*
independent re-implementations, and requiring them to share this tolerance
would defeat the independence they are for.
"""

from __future__ import annotations

import ast
import re
import sys
import tomllib
import unittest
import warnings
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCRIPTS = REPO / "scripts"

sys.path.insert(0, str(SCRIPTS))
from toml_package_name import package_name  # noqa: E402

# The spelling every real manifest in this tree uses today. An extractor must
# read this one, or it is not an extractor.
_PLAIN = 'name = "foo"'

# Lines an extractor must NOT pull a value out of. Both are `[package]` keys
# that sit next to `name` in real manifests, so a pattern that keys on "any
# quoted value" rather than on the `name` field fails here -- which is what
# separates an extractor from `check_unlinted_crates.py`'s `_QUOTED`, a
# general quoted-value pattern that must not be dragged into this guard.
_NOT_A_NAME_LINE = ('license = "MIT"', 'version = "0.1.0"')

# Valid TOML spellings of the same line. Each one has silently dropped a crate
# from some copy of this regex at some point: the comment form is #464, the
# quoted key is #486, and the single-quoted value is the third form #494
# measured. `name` is a bare key here in the last two on purpose -- the quoted
# key and the quoted value are independent axes and a copy has fixed one
# without the other.
_TOLERATED_FORMS = (
    'name = "foo" # explanation',
    '"name" = "foo"',
    "'name' = \"foo\"",
    "name = 'foo'",
    "'name' = 'foo' # explanation",
)

# Files whose regexes carry an extractor today. Pinned so a fourth copy is a
# visible line in a diff someone reviews, rather than a thing that quietly
# appears. Adding to this list is allowed; the tolerance assertion below then
# applies to the new copy too, which is the point.
_EXPECTED_CARRIERS = {"toml_package_name.py"}


def _string_constants(path: Path) -> list[str]:
    """Every `str` constant in the file, docstrings included.

    Read with `ast` rather than by scanning text so a pattern written across
    an implicit concatenation or with an unusual quoting style is still seen
    as the single string the interpreter builds.
    """
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    return [
        node.value
        for node in ast.walk(tree)
        if isinstance(node, ast.Constant) and isinstance(node.value, str)
    ]


def _captures(pattern: re.Pattern[str], line: str) -> str | None:
    """Group 1 of `pattern` on `line`, or None when it does not apply.

    `search`, not `match`: a pattern anchored with `^` is unaffected, and one
    written to be used with `re.search` in production is judged the way it is
    used.
    """
    m = pattern.search(line)
    if m is None or m.re.groups < 1:
        return None
    try:
        return m.group(1)
    except IndexError:  # pragma: no cover - guarded by the groups check above
        return None


def is_package_name_extractor(raw: str) -> bool:
    """True when running `raw` as a regex behaves like a `[package] name` read."""
    try:
        with warnings.catch_warnings():
            # Most constants in these files are prose, and compiling prose as a
            # regex emits `FutureWarning: Possible nested set` for any `[[`.
            # Thirty of those in a CI log is noise that buries the one line
            # this guard exists to print.
            warnings.simplefilter("ignore")
            pattern = re.compile(raw)
    except (re.error, RecursionError, OverflowError):
        return False
    if pattern.groups < 1:
        return False
    if _captures(pattern, _PLAIN) != "foo":
        return False
    return all(_captures(pattern, other) is None for other in _NOT_A_NAME_LINE)


def find_extractors() -> dict[str, list[str]]:
    """`scripts/*.py` basename -> the extractor patterns it carries."""
    found: dict[str, list[str]] = {}
    for path in sorted(SCRIPTS.glob("*.py")):
        hits = [s for s in _string_constants(path) if is_package_name_extractor(s)]
        if hits:
            found[path.name] = hits
    return found


class Classifier(unittest.TestCase):
    """The classifier itself, in both directions.

    A classifier that answered "no" to everything would make every assertion
    below pass while checking nothing, so its positive direction is pinned
    here rather than assumed from the sweep's own results.
    """

    def test_recognises_the_shipped_tolerant_pattern(self):
        self.assertTrue(
            is_package_name_extractor(
                r"""^["']?name["']?\s*=\s*["']([^"']+)["']\s*(?:#.*)?$"""
            )
        )

    def test_recognises_an_intolerant_pattern_too(self):
        # The pre-#464 spelling. Being recognised is the whole point: an
        # intolerant copy has to be *found* before it can be failed.
        self.assertTrue(
            is_package_name_extractor(r"""^name\s*=\s*["']([^"']+)["']\s*$""")
        )

    def test_rejects_a_general_quoted_value_pattern(self):
        # `check_unlinted_crates.py`'s `_QUOTED`, which reads the workspace
        # members list. It captures from `name = "foo"` but equally from
        # `license = "MIT"`, so it does not key on the field and is not an
        # extractor. Demanding tolerance of it would be a false positive.
        self.assertFalse(is_package_name_extractor(r"""["']([^"']+)["']"""))

    def test_rejects_patterns_with_no_capture_group(self):
        self.assertFalse(is_package_name_extractor(r"^name\s*=\s*\".*\"$"))

    def test_rejects_a_string_that_is_not_a_regex(self):
        self.assertFalse(is_package_name_extractor("[unterminated"))


class Sweep(unittest.TestCase):
    def setUp(self):
        self.found = find_extractors()

    def test_the_sweep_finds_something(self):
        # A scan that silently matches nothing satisfies the tolerance
        # assertion below unconditionally. "Found no extractors" and "every
        # extractor is tolerant" must not be able to print the same green.
        self.assertTrue(
            self.found,
            "no package-name extractor found in scripts/*.py -- the scan is "
            "broken, not the tree",
        )

    def test_every_extractor_tolerates_every_form(self):
        failures = []
        for name, patterns in sorted(self.found.items()):
            for raw in patterns:
                pattern = re.compile(raw)
                for form in _TOLERATED_FORMS:
                    if _captures(pattern, form) != "foo":
                        failures.append(f"{name}: {raw!r} does not read {form!r}")
        self.assertEqual(
            failures,
            [],
            "package-name extractors that miss a valid TOML spelling (#494):\n"
            + "\n".join(failures),
        )

    def test_the_set_of_carrying_files_is_the_recorded_one(self):
        self.assertEqual(
            set(self.found),
            _EXPECTED_CARRIERS,
            "a scripts/*.py file gained or lost a package-name extractor; "
            "update _EXPECTED_CARRIERS deliberately",
        )


class SharedReaderTableScoping(unittest.TestCase):
    """`package_name()`'s section walk, which the regex sweep above cannot see.

    The sweep judges patterns. This judges the thing built on top of them:
    which table a `name` line is read as belonging to. Both halves of the
    module have to agree about what a key looks like -- round 2 of this PR's
    review gave the header a comment tolerance the value side already had,
    and round 3 found the header still lacked the quoting tolerance the value
    side also already had. Same defect, one layer down, twice.
    """

    def test_the_plain_spelling_every_manifest_here_uses(self):
        self.assertEqual(package_name('[package]\nname = "foo"\n'), "foo")

    def test_spellings_that_are_still_the_package_table(self):
        for header in (
            "[package] # metadata",
            '["package"]',
            "['package']",
            "[ package ]",
            '[ "package" ] # metadata',
        ):
            with self.subTest(header=header):
                self.assertEqual(
                    package_name(f'{header}\nname = "foo"\n'), "foo", header
                )

    def test_spellings_that_are_a_different_table(self):
        # Each of these must NOT be read as `[package]`. `[" package "]` is
        # the case that keeps the normalisation honest: whitespace outside
        # the quotes is insignificant, whitespace inside them is part of the
        # key, so a reader that strips both would answer `foo` here and be
        # wrong.
        for header in ('[" package "]', "[package.metadata]", "[[package]]"):
            with self.subTest(header=header):
                self.assertIsNone(package_name(f'{header}\nname = "foo"\n'), header)

    def test_an_unrecognised_header_would_not_close_the_table(self):
        # The worse direction, and the one the round-2 report did not name: a
        # header that is not recognised does not END `[package]` either, so a
        # `name` under a later table comes back AS the package name. A
        # confident wrong answer rather than a gap.
        for later in ("[features] # x", '["features"]', "[ features ]", "[[bin]]"):
            with self.subTest(later=later):
                self.assertIsNone(
                    package_name(f'[package]\nversion = "1"\n{later}\nname = "w"\n'),
                    later,
                )

    def test_a_bin_table_before_package_does_not_win(self):
        # Why the walk is section-scoped at all: the whole-file search it
        # replaced returned whichever `name` came first in the file, and this
        # workspace really does rename bin targets.
        self.assertEqual(
            package_name('[[bin]]\nname = "b"\n[package]\nname = "foo"\n'), "foo"
        )

    def test_a_virtual_manifest_has_no_package_name(self):
        self.assertIsNone(package_name("[workspace]\nmembers = []\n"))


# Spellings where this reader deliberately disagrees with `tomllib`, each with
# the reason it is a decision rather than a bug. Both are gaps (None), never
# wrong answers, and both are fail-closed in the one caller where the miss is
# a change in behaviour -- see `table_key`'s contract note.
#
# This list IS the recorded contract. Adding to it is allowed and is a visible
# diff someone reviews; what must not happen is a divergence nobody wrote down.
_KNOWN_DIVERGENCES = {
    '["pack\\u0061ge"]\nname = "foo"\n': (
        "escape sequences in a basic quoted key are not decoded -- parser "
        "work this module does not do; 0 of 83 manifests use it"
    ),
    '[package]\nname = """foo"""\n': (
        "a multiline string as the *name* value; NAME_LINE requires a "
        "single-delimiter value, so this reads as no name at all"
    ),
}


class DifferentialAgainstTomllib(unittest.TestCase):
    """`package_name()` against the reference parser, on real and made-up input.

    Four review rounds on this PR each turned up one more valid TOML spelling
    the reader mishandled. Answering them one at a time is a losing game, so
    the question gets asked once, mechanically: does this reader agree with
    `tomllib` -- and where it does not, is that written down?

    Production stays parser-free by choice, not because `tomllib` is missing
    -- the runner image is `ubuntu-24.04` and its Python is 3.12, so it is
    there. See the module docstring in `scripts/toml_package_name.py` for why
    a *guard* still should not depend on an unpinned interpreter feature.

    A test may, and this one does: it imports `tomllib` at module scope rather
    than skipping when absent. A skipped differential is a green that checked
    nothing. The trade is deliberate and worth naming -- this couples the
    `Audit publisher tests` job to a runner Python nothing in this repo pins,
    so if that image ever drops below 3.11 the import fails and the job goes
    red. Loud, and about the right thing; pinning the runner's Python instead
    would add a network dependency to a job that currently has none.
    """

    # Every spelling those four rounds turned up, plus the plain one.
    # `tomllib` supplies the expected answer, so nothing here encodes my own
    # belief about what the right answer is.
    CORPUS = (
        '[package]\nname = "foo"\n',
        '[package]\nname = "foo" # explanation\n',
        '[package]\n"name" = "foo"\n',
        "[package]\n'name' = 'foo'\n",
        '[package] # metadata\nname = "foo"\n',
        '["package"]\nname = "foo"\n',
        "['package']\nname = \"foo\"\n",
        '[ package ]\nname = "foo"\n',
        '[" package "]\nname = "foo"\n',
        '[package.metadata]\nname = "foo"\n',
        '[[bin]]\nname = "b"\n[package]\nname = "foo"\n',
        '[package]\nversion = "1"\n[features]\nname = "wrong"\n',
        '[package]\ndescription = """\nname = "sneaky"\n"""\nname = "real"\n',
        '[package]\ndescription = """\n[features]\n"""\nname = "actual"\n',
        "[package]\ndescription = '''\n[features]\n'''\nname = \"lit\"\n",
        '[package]\ndescription = """one line"""\nname = "same"\n',
        '[package]\nname = "first"\ndescription = """\n[x]\n"""\n',
        "[workspace]\nmembers = []\n",
        # The two deliberate divergences are listed here EXPLICITLY, not
        # splatted from `_KNOWN_DIVERGENCES`. Deriving them from the exemption
        # list made the exemption unfalsifiable: deleting an entry deleted the
        # case along with it, so the divergence vanished from the corpus
        # instead of becoming an unrecorded one, and every test stayed green.
        # Caught by mutating the exemption list and finding nothing went red.
        '["pack\\u0061ge"]\nname = "foo"\n',
        '[package]\nname = """foo"""\n',
    )

    @staticmethod
    def _reference(text):
        return tomllib.loads(text).get("package", {}).get("name")

    def test_the_corpus_parses_as_real_toml(self):
        # Every case must be valid TOML, or the comparison below is against
        # nothing. A malformed corpus would let `tomllib` raise and leave the
        # author quietly assuming agreement.
        for text in self.CORPUS:
            with self.subTest(text=text):
                tomllib.loads(text)

    def test_the_corpus_covers_both_answers(self):
        # A corpus where the reference says None everywhere would be satisfied
        # by a reader that always returns None.
        answers = {self._reference(t) for t in self.CORPUS}
        self.assertIn(None, answers)
        self.assertTrue(answers - {None}, answers)

    def test_agrees_with_tomllib_except_where_recorded(self):
        unexpected = []
        for text in self.CORPUS:
            ours, ref = package_name(text), self._reference(text)
            if ours == ref:
                continue
            if text in _KNOWN_DIVERGENCES:
                # A recorded divergence must stay a gap. If one ever becomes a
                # non-None wrong answer that is a different defect, and this
                # exemption does not cover it.
                self.assertIsNone(
                    ours, f"recorded divergence became a wrong answer: {text!r}"
                )
                continue
            unexpected.append(f"{text!r}: ours={ours!r} tomllib={ref!r}")
        self.assertEqual(unexpected, [], "\n".join(unexpected))

    def test_every_recorded_divergence_is_in_the_corpus(self):
        # Without this the two lists can drift apart and an exemption can name
        # a case nothing runs -- an exemption for a test that does not exist.
        missing = [t for t in _KNOWN_DIVERGENCES if t not in self.CORPUS]
        self.assertEqual(missing, [], f"exempted but not in the corpus: {missing}")

    def test_every_recorded_divergence_still_diverges(self):
        # A stale exemption hides a regression the way a stale allowlist entry
        # does: once a fix makes one of these agree, the entry has to go, or it
        # silently exempts whatever breaks here next.
        for text, reason in _KNOWN_DIVERGENCES.items():
            with self.subTest(reason=reason):
                self.assertNotEqual(
                    package_name(text),
                    self._reference(text),
                    f"no longer diverges; drop it from _KNOWN_DIVERGENCES: {text!r}",
                )

    def test_agrees_with_tomllib_on_every_real_manifest(self):
        manifests = [
            m
            for m in sorted(REPO.rglob("Cargo.toml"))
            if "/target/" not in str(m) and "/.git/" not in str(m)
        ]
        self.assertGreater(len(manifests), 20, "manifest sweep found almost nothing")
        mismatched = []
        for manifest in manifests:
            text = manifest.read_text(encoding="utf-8", errors="replace")
            try:
                ref = self._reference(text)
            except tomllib.TOMLDecodeError:
                continue
            if package_name(text) != ref:
                mismatched.append(f"{manifest}: {package_name(text)!r} != {ref!r}")
        self.assertEqual(mismatched, [], "\n".join(mismatched))


if __name__ == "__main__":
    unittest.main()
