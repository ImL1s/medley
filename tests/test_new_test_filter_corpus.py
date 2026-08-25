"""Corpus tests for `check_new_tests_are_filtered.py`'s identifier patterns (#455).

The guard's regexes classify things that exist in the repository — test
attributes, function signatures, module paths. They were written against
imagined forms and checked against hand-written examples, which is the exact
shape that let `check_envguard_serial.py`'s `EnvGuard::` matcher ship seeing
one guard type out of five: the same mental model writes the pattern and the
example, so a wrong model produces a matching wrong test.

Every corpus here is enumerated from the tree by a mechanism that is NOT the
pattern under test, so the inputs are not chosen by whoever wrote the regex:

* test attributes — bracket-scan `#[...]`, and to classify a line, strip its
  arguments and split on `::` to see whether the final segment is exactly
  `test`. What gets stored in the corpus is the attribute's real text,
  arguments and all -- classification strips them to decide keep/discard,
  it does not throw them away (#458 review);
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
        # Keyed on the real attribute text, arguments and all -- reconstructing
        # just the path (`tokio::test`) tests a form that is not actually in
        # the tree and would hide a pattern that only matches a test attribute
        # with no arguments, which is 1,032 of the real instances here (#458
        # review).
        forms: Counter[str] = Counter()
        for path in _rust_files():
            for line in path.read_text(encoding="utf-8", errors="ignore").splitlines():
                if _is_test_attribute(line):
                    forms[line.strip()] += 1
        cls.forms = forms

    def test_the_corpus_is_not_empty(self):
        # A corpus scan that finds nothing satisfies every assertion below
        # while checking nothing at all.
        self.assertGreater(sum(self.forms.values()), 1000, dict(self.forms))

    def test_every_attribute_form_in_the_tree_is_recognised(self):
        missed = [f for f in self.forms if not guard._TEST_ATTR.match(f"+    {f}")]
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
        # #171, so there is no historical version to run against. Plausible
        # narrowings stand in for one -- dropping `tokio::` support, and
        # requiring the attribute to end immediately after `test` (the shape
        # that let a path-normalized corpus pass while missing every
        # argumented form, #458 review).
        for narrowed in (
            re.compile(r"^\+\s*#\[test\b"),
            re.compile(r"^\+\s*#\[(?:tokio::)?test\]"),
        ):
            missed = [f for f in self.forms if not narrowed.match(f"+    {f}")]
            self.assertTrue(
                missed,
                f"corpus cannot distinguish {narrowed.pattern!r}, so it proves nothing",
            )


class FunctionSignatureCorpus(unittest.TestCase):
    """`_FN` must match the signature line of every test in the tree."""

    @classmethod
    def setUpClass(cls):
        shapes: Counter[str] = Counter()
        misses: list[str] = []
        # A test attribute this walk cannot turn into a one-line signature --
        # e.g. `fn` and the name on separate lines, which real rustfmt::skip
        # code in this tree does to other constructs -- must not just vanish.
        # An attribute that disappears here is indistinguishable from one
        # that was checked and passed, which is the same absence-reads-as-
        # success shape #461 catalogs elsewhere (#458 review).
        unresolved: list[str] = []
        for path in _rust_files():
            lines = path.read_text(encoding="utf-8", errors="ignore").splitlines()
            for index, line in enumerate(lines):
                if not _is_test_attribute(line):
                    continue
                cursor = index + 1
                depth = 0
                while cursor < len(lines):
                    candidate = lines[cursor].strip()
                    # A `#[cfg_attr(..)]` can span lines (real cases in
                    # `config/watcher.rs`, `session/git.rs`); tracking bracket
                    # depth walks past it instead of stopping on its first
                    # continuation line and losing the test (#458 review).
                    if depth > 0:
                        depth += candidate.count("[") - candidate.count("]")
                        cursor += 1
                        continue
                    if candidate.startswith("#["):
                        depth = candidate.count("[") - candidate.count("]")
                        cursor += 1
                        continue
                    if candidate.startswith("//") or not candidate:
                        cursor += 1
                        continue
                    break
                location = f"{path.relative_to(REPO)}:{index + 1}"
                if cursor >= len(lines):
                    unresolved.append(f"{location} (ran off end of file)")
                    continue
                signature = lines[cursor]
                if not re.search(r"\bfn\s", signature):
                    unresolved.append(f"{location} -> {signature.strip()!r}")
                    continue
                shapes[re.sub(r"\bfn\s+[A-Za-z_][A-Za-z0-9_]*.*", "fn <name>(..)", signature.strip())] += 1
                if not guard._FN.match("+" + signature):
                    misses.append(signature.strip())
        cls.shapes = shapes
        cls.misses = misses
        cls.unresolved = unresolved

    def test_the_corpus_is_not_empty(self):
        self.assertGreater(sum(self.shapes.values()), 1000, dict(self.shapes))

    def test_every_test_signature_in_the_tree_is_matched(self):
        self.assertEqual(
            self.misses[:10], [], f"_FN misses {len(self.misses)} real test signatures"
        )

    def test_every_test_attribute_resolves_to_a_signature(self):
        # Complements the assertion above: that one only judges signatures
        # this walk found. A test attribute it could not turn into a
        # signature line at all never reaches `misses` -- or `shapes` -- so
        # without this it reads as a pass it was never checked for (#458
        # review).
        self.assertEqual(
            self.unresolved[:10],
            [],
            f"{len(self.unresolved)} test attribute(s) could not be resolved to a signature line",
        )

    def test_the_pattern_still_discriminates(self):
        for not_a_fn in ("+    struct Foo {", "+    let fn_like = 1;", "     fn unchanged()"):
            self.assertIsNone(guard._FN.match(not_a_fn), not_a_fn)


def _workflow_module_filters() -> list[tuple[str, str, str]]:
    """`(crate, target, filter)` for every module-path filter, from the GUARD's parser.

    Re-deriving the filter list with a second regex was itself a proxy: mine
    captured only filters ending in `::`, so `agent::models` and friends were
    absent and a regression in them left this test green (#458 review).
    `parse_workflow` is what the guard actually uses, and it is crate-scoped,
    which is also what makes resolution checkable against the right crate.
    """

    per_crate = guard.parse_workflow(WORKFLOW.read_text(encoding="utf-8"))
    out: set[tuple[str, str, str]] = set()
    for crate, targets in per_crate.items():
        for target, filters in targets.items():
            for value in filters:
                if "::" in value:
                    # Keep the TARGET. A filter attached to `--test api` must
                    # not be resolved against the crate's lib or another test
                    # binary; dropping it let a same-named module elsewhere
                    # vouch for it (#458 review).
                    out.add((crate, target, value))
    return sorted(out)


def _files_by_crate() -> dict[str, list[str]]:
    grouped: dict[str, list[str]] = {}
    for path in CRATES.rglob("*.rs"):
        rel = path.relative_to(REPO).as_posix()
        crate = guard.crate_of(rel)
        if crate:
            grouped.setdefault(crate, []).append(rel)
    return grouped


FN_NAME = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")


def _tests_in(source: str) -> list[str]:
    """Every `#[test]`-marked function name, with no distance limit.

    A `[\\s\\S]{0,400}` window between the marker and its `fn` silently drops a
    test whose attribute block is longer — the same fixed-window proxy that
    made `check_envguard_serial.py` read the next item's lock (#458 review).
    Walking the attribute lines has no such cliff.
    """

    names: list[str] = []
    lines = source.splitlines()
    for index, line in enumerate(lines):
        if not _is_test_attribute(line):
            continue
        cursor, depth = index + 1, 0
        while cursor < len(lines):
            candidate = lines[cursor].strip()
            if depth > 0:
                depth += candidate.count("[") - candidate.count("]")
                cursor += 1
                continue
            if candidate.startswith("#["):
                depth = candidate.count("[") - candidate.count("]")
                cursor += 1
                continue
            if candidate.startswith("//") or not candidate:
                cursor += 1
                continue
            break
        if cursor < len(lines) and (match := FN_NAME.search(lines[cursor])):
            names.append(match.group(1))
    return names


def _real_tests_by_crate() -> dict[str, list[tuple[str, str]]]:
    """`(file, test fn)` for every real test, grouped by crate.

    A sentinel function name cannot resolve a filter whose LAST segment is a
    test function rather than a module — `selected` needs the real name to
    match that shape, so a sentinel reports it unresolvable when the guard
    handles it correctly (#458 review). Resolving against the actual population
    is the same correction as taking the corpus from the tree in the first
    place.
    """

    grouped: dict[str, list[tuple[str, str]]] = {}
    for path in CRATES.rglob("*.rs"):
        rel = path.relative_to(REPO).as_posix()
        crate = guard.crate_of(rel)
        if not crate:
            continue
        source = path.read_text(encoding="utf-8", errors="ignore")
        for name in _tests_in(source):
            grouped.setdefault(crate, []).append((rel, name))
    return grouped

# Module filters that resolve to no file in their own crate, because the
# component they name is an INLINE module and the guard derives module paths
# from file paths. Each is a filter that works in Cargo and that the guard
# would report as selecting nothing -- #460. This list is a ratchet: it must
# not grow, and it shrinks to empty when #460 is fixed.
#
# Keyed on (crate, target, value), TARGET included. Collapsing to (crate,
# value) let an allowlisted filter reused against a different, genuinely
# broken target be absorbed by the entry a DIFFERENT target earned -- a new
# breakage would reuse an existing key and `test_no_new_unresolvable_module_filter`
# would report nothing new. Proven by construction, not by count: today every
# (crate, value) here maps to exactly one target ("lib"), so this widening is
# a re-expression of the same seven entries, not new coverage (#458 review).
KNOWN_UNRESOLVABLE = {
    ("xai-grok-shell", "lib", "auth::manager::tests::"),
    ("xai-grok-shell", "lib", "auth::openai_codex::tests::"),
    (
        "xai-grok-shell",
        "lib",
        "auth::openai_codex::tests::full_login_flow_persists_provider_scoped_codex_credential",
    ),
    ("xai-grok-shell", "lib", "leader::lock::tests::reclaim"),
    ("xai-grok-shell", "lib", "terminal::pty_session::tests::"),
    ("xai-grok-shell", "lib", "terminal::pty_session::tests::dup_fd_is_not_inherited_by_exec_child"),
    ("xai-grok-subagent-resolution", "lib", "resume::tests"),
}


class ModulePathApproximationCorpus(unittest.TestCase):
    """#171's file-path approximation, against filters the guard itself parsed.

    The property is that a module-path filter RESOLVES: some real file in its
    own crate makes `selected()` true through the module-path branch. Checking
    instead that each component appears somewhere in the repository is a proxy
    -- `auth::openai_codex::tests::` passes that because some unrelated file
    contributes a `tests` component, while the guard rejects a new test in that
    module (#458 review). A component set is exactly the kind of container
    whose non-emptiness reads as success.
    """

    @classmethod
    def setUpClass(cls):
        cls.filters = _workflow_module_filters()
        cls.by_crate = _real_tests_by_crate()
        cls.unresolvable = {
            # TARGET stays in the key. Dropping it here would let a filter
            # reused against a second, genuinely broken target collapse onto
            # the (crate, value) an unrelated target already earned its way
            # into KNOWN_UNRESOLVABLE with -- proven by construction (#458
            # review): injecting a synthetic (crate, "new_target", value)
            # triple that reuses an allowlisted value left this test green
            # under the old (crate, value) key.
            (crate, target, value)
            for crate, target, value in cls.filters
            if not any(
                guard.selected(fn, path, {value})
                for path, fn in cls.by_crate.get(crate, [])
                if target == "*" or target in guard.targets_of(path, crate)
            )
        }

    def test_the_corpus_is_not_empty(self):
        self.assertGreater(len(self.filters), 50, len(self.filters))
        self.assertGreater(len(self.by_crate), 10, len(self.by_crate))
        self.assertGreater(
            sum(len(v) for v in self.by_crate.values()), 1000, "no real tests found"
        )

    def test_the_corpus_includes_filters_without_a_trailing_separator(self):
        # The bug this class had: only trailing-`::` filters were collected.
        self.assertTrue(
            any(not value.endswith("::") for _c, _t, value in self.filters),
            "corpus omits filters that do not end in `::`",
        )

    def test_no_new_unresolvable_module_filter(self):
        new = self.unresolvable - KNOWN_UNRESOLVABLE
        self.assertEqual(
            new,
            set(),
            "these ci.yml module filters resolve to no file in their crate, so "
            f"the guard judges tests in them unenrolled (see #460): {sorted(new)}",
        )

    def test_the_known_list_does_not_go_stale(self):
        fixed = KNOWN_UNRESOLVABLE - self.unresolvable
        self.assertEqual(
            fixed,
            set(),
            f"these now resolve; remove them from KNOWN_UNRESOLVABLE: {sorted(fixed)}",
        )

    def test_the_approximation_really_cannot_see_an_inline_module(self):
        # Pins WHY the list above exists. If this starts passing, #460 is fixed
        # and the list should be emptied rather than left as folklore.
        self.assertFalse(
            guard.selected(
                "some_unrelated_test_name",
                "crates/codegen/x/src/main.rs",
                {"version_json_payload_tests::"},
            ),
            "the file-path approximation now resolves inline modules; see #460",
        )
