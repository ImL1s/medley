"""Corpus tests for `check_new_tests_are_filtered.py`'s identifier patterns (#455).

The guard's regexes classify things that exist in the repository — test
attributes, function signatures, module paths. They were written against
imagined forms and checked against hand-written examples, which is the exact
shape that let `check_envguard_serial.py`'s `EnvGuard::` matcher ship seeing
one guard type out of five: the same mental model writes the pattern and the
example, so a wrong model produces a matching wrong test.

Every corpus here is enumerated from the tree by a mechanism that is NOT the
pattern under test, so the inputs are not chosen by whoever wrote the regex:

* test attributes — bracket-scan `#[...]`, strip arguments, split on `::`, and
  keep the ones whose final segment is exactly `test`;
* function signatures — the first code line after such an attribute;
* module paths — actual `mod` declarations parsed from source, versus the
  guard's approximation from FILE paths. Those are genuinely different sources,
  which is what keeps this from being circular.

Scope is deliberately narrow (#455): whether the patterns match what is in the
tree. Where a corpus reveals a guard checking a fraction of its subject, that
is reported, not fixed here.
"""

from __future__ import annotations

import importlib.util
import re
import sys
import unittest
from collections import Counter
from pathlib import Path


REPO = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO / "scripts" / "check_new_tests_are_filtered.py"
WORKFLOW = REPO / ".github" / "workflows" / "ci.yml"
CRATES = REPO / "crates"


def load_guard():
    spec = importlib.util.spec_from_file_location("check_new_tests_are_filtered", SCRIPT_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {SCRIPT_PATH}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


guard = load_guard()


def _rust_files():
    return sorted(CRATES.rglob("*.rs"))


def _is_test_attribute(line: str) -> bool:
    """True when `line` is an attribute whose final `::` segment is `test`.

    Substring/segment logic on purpose — not a regex, and specifically not the
    regex under test.
    """

    stripped = line.strip()
    if not stripped.startswith("#["):
        return False
    head = stripped[2:].split("(")[0].rstrip("]").strip()
    return head.split("::")[-1].strip() == "test"


class TestAttributeCorpus(unittest.TestCase):
    """`_TEST_ATTR` must recognise every form of test attribute in the tree."""

    @classmethod
    def setUpClass(cls):
        forms: Counter[str] = Counter()
        for path in _rust_files():
            for line in path.read_text(encoding="utf-8", errors="ignore").splitlines():
                if _is_test_attribute(line):
                    forms[line.strip().rstrip("]")[2:].split("(")[0].strip()] += 1
        cls.forms = forms

    def test_the_corpus_is_not_empty(self):
        # A corpus scan that finds nothing satisfies every assertion below
        # while checking nothing at all.
        self.assertGreater(sum(self.forms.values()), 1000, dict(self.forms))

    def test_every_attribute_form_in_the_tree_is_recognised(self):
        missed = [f for f in self.forms if not guard._TEST_ATTR.match(f"+    #[{f}]")]
        self.assertEqual(
            missed, [], f"_TEST_ATTR misses test attributes that exist here: {missed}"
        )

    def test_the_pattern_still_discriminates(self):
        # Otherwise widening it until the corpus passes greens this test while
        # destroying its purpose.
        for not_a_test in ("#[cfg(test)]", "#[test_case(1)]", "#[tokio::main]", "#[derive(Debug)]"):
            self.assertIsNone(
                guard._TEST_ATTR.match(f"+    {not_a_test}"), not_a_test
            )

    def test_a_narrowed_pattern_would_fail_this_corpus(self):
        # Proof the corpus can fail: these two patterns are unchanged since
        # #171, so there is no historical version to run against. A plausible
        # narrowing stands in for one.
        narrowed = re.compile(r"^\+\s*#\[test\b")
        missed = [f for f in self.forms if not narrowed.match(f"+    #[{f}]")]
        self.assertTrue(
            missed, "corpus cannot distinguish a narrower pattern, so it proves nothing"
        )


class FunctionSignatureCorpus(unittest.TestCase):
    """`_FN` must match the signature line of every test in the tree."""

    @classmethod
    def setUpClass(cls):
        shapes: Counter[str] = Counter()
        misses: list[str] = []
        for path in _rust_files():
            lines = path.read_text(encoding="utf-8", errors="ignore").splitlines()
            for index, line in enumerate(lines):
                if not _is_test_attribute(line):
                    continue
                cursor = index + 1
                while cursor < len(lines):
                    candidate = lines[cursor].strip()
                    if candidate.startswith("#[") or candidate.startswith("//") or not candidate:
                        cursor += 1
                        continue
                    break
                if cursor >= len(lines):
                    continue
                signature = lines[cursor]
                if not re.search(r"\bfn\s", signature):
                    continue
                shapes[re.sub(r"\bfn\s+[A-Za-z_][A-Za-z0-9_]*.*", "fn <name>(..)", signature.strip())] += 1
                if not guard._FN.match("+" + signature):
                    misses.append(signature.strip())
        cls.shapes = shapes
        cls.misses = misses

    def test_the_corpus_is_not_empty(self):
        self.assertGreater(sum(self.shapes.values()), 1000, dict(self.shapes))

    def test_every_test_signature_in_the_tree_is_matched(self):
        self.assertEqual(
            self.misses[:10], [], f"_FN misses {len(self.misses)} real test signatures"
        )

    def test_the_pattern_still_discriminates(self):
        for not_a_fn in ("+    struct Foo {", "+    let fn_like = 1;", "     fn unchanged()"):
            self.assertIsNone(guard._FN.match(not_a_fn), not_a_fn)


def _inline_and_path_module_names() -> tuple[set[str], set[str]]:
    """Module names declared in SOURCE rather than implied by a file name.

    The independent half of the module-path comparison: the guard approximates
    module paths from file paths, so parsing the actual `mod` declarations is a
    different source, not a restatement of the same assumption.
    """

    inline_decl = re.compile(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{", re.M
    )
    path_decl = re.compile(
        r'#\[path\s*=\s*"[^"]+"\]\s*(?:pub\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;'
    )
    inline: set[str] = set()
    path_mods: set[str] = set()
    for path in CRATES.rglob("*.rs"):
        if "src" not in path.parts:
            continue
        source = path.read_text(encoding="utf-8", errors="ignore")
        inline |= set(inline_decl.findall(source))
        path_mods |= set(path_decl.findall(source))
    return inline, path_mods


def _file_provided_components() -> set[str]:
    """Module components the guard CAN derive, i.e. file and directory names."""

    provided: set[str] = set()
    for path in CRATES.rglob("*.rs"):
        if "src" not in path.parts:
            continue
        provided.add(path.stem)
        provided.add(path.parent.name)
    return provided


def _workflow_module_filters() -> list[str]:
    text = WORKFLOW.read_text(encoding="utf-8")
    return sorted(
        {
            token
            for token in re.findall(
                r"(?<![\w$/.-])([a-z_][a-z0-9_]*(?:::[a-z_][a-z0-9_]*)*::)(?=\s|$)", text
            )
        }
    )


class ModulePathApproximationCorpus(unittest.TestCase):
    """#171's `::` -> file-path approximation, against real `mod` declarations.

    The approximation reads module components off the FILE path, so a module
    declared inline (`mod x_tests { .. }`) or relocated with `#[path = ".."]`
    is invisible to it: a correct filter naming one reads as unenrolled.

    That gap is real but currently LATENT — every module-path filter in
    `ci.yml` names components a file provides. This pins that, so the day
    someone writes a filter naming an inline module the guard's silent wrong
    answer becomes a loud one. Closing the gap itself is out of scope (#455).
    """

    @classmethod
    def setUpClass(cls):
        cls.inline, cls.path_mods = _inline_and_path_module_names()
        cls.provided = _file_provided_components()
        cls.filters = _workflow_module_filters()

    def test_the_corpora_are_not_empty(self):
        self.assertGreater(len(self.filters), 20, self.filters)
        self.assertGreater(len(self.inline), 20, len(self.inline))

    def test_every_workflow_module_filter_resolves_to_file_components(self):
        unresolved = []
        for filter_value in self.filters:
            segments = [s for s in filter_value.strip(":").split("::") if s]
            missing = [s for s in segments if s not in self.provided]
            if missing:
                kinds = [
                    f"{m} ({'inline mod' if m in self.inline else '#[path] mod' if m in self.path_mods else 'unknown'})"
                    for m in missing
                ]
                unresolved.append(f"{filter_value} -> {', '.join(kinds)}")
        self.assertEqual(
            unresolved,
            [],
            "these ci.yml filters name modules the file-path approximation "
            f"cannot see, so the guard judges them unenrolled: {unresolved}",
        )

    def test_the_approximation_really_cannot_see_an_inline_module(self):
        # Pins WHY the assertion above matters. If this ever starts passing,
        # the approximation has been improved and the docstring above is stale.
        self.assertFalse(
            guard.selected(
                "some_unrelated_test_name",
                "crates/codegen/x/src/main.rs",
                {"version_json_payload_tests::"},
            ),
            "the file-path approximation now resolves inline modules; update #455's note",
        )
