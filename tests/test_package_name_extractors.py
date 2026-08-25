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
import unittest
import warnings
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCRIPTS = REPO / "scripts"

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


if __name__ == "__main__":
    unittest.main()
