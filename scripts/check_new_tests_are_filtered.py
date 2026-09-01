#!/usr/bin/env python3
"""Fail when a newly added test matches no `ci.yml` filter (issue #171).

`ci.yml` has no unfiltered test job: every `cargo test` invocation names a
filter, so a test matching none of them runs nowhere and its author gets a
green tick that says nothing about it. `run_nonzero` already fails when a
filter matches *zero* tests; nothing watches the opposite direction.

This reads the **diff**, not a test listing, which is what makes it usable:

- No baseline file to bootstrap, and no per-crate `--list` build.
- `#[cfg(target_os = "linux")]` tests are covered. That is the case that made
  the gap invisible in the first place -- the author checked on macOS, where
  the test did not exist to be missed. It is in the diff on every platform.

Cargo's filter is a plain substring match on the full test path (`ci.yml` uses
no `--exact`), so a new test is "selected" when some filter for its crate is a
substring of its name. Matching on the bare function name is deliberately
lenient: a filter like `slash::commands::model::` selects by module path, which
this cannot see from the diff alone, so module-path filters are also checked
against the file's path — and, for #460, against inline `mod name {` blocks and
same-directory `#[path = "..."] mod name;` declarations the file path omits.

What this deliberately does NOT cover (issue #408): a test that already
existed before `--base`. Reading the diff is what buys the two properties
above, and it costs exactly this one. The complement is
`check_test_filter_coverage.py`, which compares a per-crate `--list` against
the same filter set and is wired into the `compile-tests` job against
`tests/ci/unenrolled-tests.baseline`. If you are here asking "is this class
handled?", the answer is yes but not by this script.

Usage:
    check_new_tests_are_filtered.py --workflow .github/workflows/ci.yml --base origin/providers
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_test_filter_coverage import parse_workflow  # noqa: E402
from toml_package_name import name_in_block  # noqa: E402

_TEST_ATTR = re.compile(r"^\+\s*#\[(?:tokio::)?test\b")
_FN = re.compile(r"^\+\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)")
_DIFF_FILE = re.compile(r"^\+\+\+ b/(.+)$")
_INLINE_MOD_OPEN = re.compile(r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{")
_PATH_ATTR = re.compile(r"#\[path\s*=\s*\"([^\"]+)\"\]")
_MOD_SEMI = re.compile(r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;")
_FN_NAME = re.compile(
    r"\b(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\("
)

# Corpus + CI call `selected` many times per file; cache disk reads (#460).
_SOURCE_CACHE: dict[str, str] = {}
_PATH_ATTR_CACHE: dict[str, list[str] | None] = {}
_INLINE_INDEX_CACHE: dict[str, dict[str, list[str]]] = {}


def _crate_split(path: str) -> tuple[str, tuple[str, ...]] | None:
    """`crates/codegen/xai-grok-shell/src/a/b.rs` -> `("crates/codegen/xai-grok-shell",
    ("src", "a", "b.rs"))`.

    Returning the crate *directory* as well as the name is what lets `target_of`
    anchor on it. Cargo's layout is only meaningful relative to the crate root:
    `tests/` there is an integration-test target, `tests/` anywhere below `src/` is
    an ordinary module that happens to be called that.
    """
    parts = Path(path).parts
    if "crates" not in parts:
        return None
    i = parts.index("crates")
    # crates/<group>/<crate>/...
    if len(parts) <= i + 3:
        return None
    return str(Path(*parts[: i + 3])), parts[i + 3 :]


def crate_of(path: str) -> str | None:
    """`crates/codegen/xai-grok-shell/src/...` -> `xai-grok-shell`."""
    split = _crate_split(path)
    if split is None:
        # A file directly in the crate directory (`Cargo.toml`) still names a crate.
        parts = Path(path).parts
        if "crates" in parts:
            i = parts.index("crates")
            if len(parts) > i + 2:
                return parts[i + 2]
        return None
    return Path(split[0]).name


def _bin_target_name(crate_dir: str, rel_path: str, fallback: str) -> str:
    """The name cargo gives the binary at `rel_path`, which is **not** derivable from
    the path whenever `Cargo.toml` says otherwise.

    Two ways to get this wrong, and this repo has both:

    - `src/main.rs` defaults to the *package* name, except here the crate is
      `xai-grok-pager-bin` and the target is `[[bin]] name = "xai-grok-pager"` --
      deliberately, because renaming the cargo target would churn every upstream sync
      (CLAUDE.md), so the rename to `medley` happens at packaging instead.
    - `src/bin/foo.rs` autobins to the *stem*, except a `[[bin]]` entry may rename it.
      Measured in this workspace: `mouse_events_playground` -> `mouse-events-playground`,
      `cli` -> `fast-worktree`, `workspace_server` -> `xai-workspace-server`,
      `code_graph` -> `code-graph`, `pty_scenario` -> `pty-scenario`.

    An underscore-vs-hyphen miss is enough: the filter map is keyed by the cargo name,
    so the lookup finds nothing and a test CI runs is reported as running nowhere.

    `fallback` is what cargo would do with no `[[bin]]` entry -- the package name for
    `src/main.rs`, the file stem for `src/bin/*`.
    """
    manifest = Path(crate_dir) / "Cargo.toml"
    try:
        text = manifest.read_text()
    except OSError:
        return fallback
    want = rel_path.removeprefix("./")
    # Minimal scan rather than a TOML parse: this runs in a job with no dependencies.
    for block in re.split(r"^\s*\[\[bin\]\]\s*$", text, flags=re.M)[1:]:
        block = re.split(r"^\s*\[", block, maxsplit=1, flags=re.M)[0]
        # Shared with the sibling guards (#494). Not a `[package] name`, but
        # the same line shape with the same tolerance gaps: a `[[bin]]` entry
        # spelled `name = 'foo'` or `"name" = "foo"` used to fall through to
        # `fallback` -- a confident wrong target name, not a missing one.
        # (The `path` read below has not been given the same treatment.)
        bin_name = name_in_block(block)
        if bin_name is None:
            continue
        path_m = re.search(r'^\s*path\s*=\s*"([^"]+)"', block, flags=re.M)
        if path_m is None:
            # No `path`: cargo infers it from the name, so this entry only claims
            # `rel_path` if the inferred location matches.
            inferred = {f"src/bin/{bin_name}.rs", "src/main.rs"}
            if want in inferred:
                return bin_name
            continue
        if path_m.group(1).removeprefix("./") == want:
            return bin_name
    return fallback


def _test_roots_including(crate_dir: str, rel_under_tests: str) -> list[str]:
    """Every `tests/*.rs` root that pulls this file in, directly or through a `mod.rs`.

    Plural on purpose. The roots share modules -- `pty_e2e/common.rs` is declared
    by all nine -- so a file can compile into more than one target, and a filter
    naming any one of them runs it. Returning a single root would report a
    covered test as uncovered whenever `ci.yml` happens to name a different
    sibling.

    The walk up the ancestors is what finds a leaf: the root declares
    `#[path = "pty_e2e/minimal/mod.rs"] mod minimal;`, and `mod.rs` declares the
    case files. Only the root is a cargo target, so that is what gets reported.
    """
    tests_dir = Path(crate_dir) / "tests"
    try:
        roots = sorted(p for p in tests_dir.glob("*.rs") if p.is_file())
    except OSError:
        return []
    parts = rel_under_tests.split("/")
    needles = [f'#[path = "{rel_under_tests}"]']
    for depth in range(len(parts) - 1, 0, -1):
        needles.append(f'#[path = "{"/".join(parts[:depth])}/mod.rs"]')
    found: list[str] = []
    for root in roots:
        try:
            text = root.read_text()
        except OSError:
            continue
        if any(n in text for n in needles):
            found.append(root.stem)
    return found


def _test_root_including(crate_dir: str, rel_under_tests: str) -> str | None:
    """Which `tests/*.rs` root pulls `tests/<rel_under_tests>` in via `#[path]`.

    Cargo's integration-test targets are the `.rs` files directly under `tests/`
    (plus `tests/<dir>/main.rs`). A subdirectory without a `main.rs` is not a
    target; it is a pile of modules that some root declares. This repository
    splits one PTY suite across nine such roots so the families can be scheduled
    separately, and every case lives under `tests/pty_e2e/`.

    Returning the *declaring root* is what lets a filter naming a real target
    cover the file. Roots are read rather than inferred, for the same reason
    `_bin_target_name` reads `Cargo.toml`: the mapping is a decision someone
    wrote down, not a property of the path.
    """
    tests_dir = Path(crate_dir) / "tests"
    try:
        roots = sorted(p for p in tests_dir.glob("*.rs") if p.is_file())
    except OSError:
        return None
    found = _test_roots_including(crate_dir, rel_under_tests)
    return found[0] if found else None


def targets_of(path: str, crate_name: str | None) -> list[str]:
    """Every cargo target that compiles this file.

    One entry for everything except a file shared by several integration-test
    roots; [`target_of`] is the first of these and stays the answer to "which
    target is this", which is what the classification tests assert.
    """
    split = _crate_split(path)
    if split is not None:
        crate_dir, rel = split
        if rel and rel[0] == "tests" and len(rel) > 2:
            if not (Path(crate_dir) / "tests" / rel[1] / "main.rs").exists():
                roots = _test_roots_including(crate_dir, "/".join(rel[1:]))
                if roots:
                    return [f"test:{r}" for r in roots]
    single = target_of(path, crate_name)
    return [single] if single else []


def target_of(path: str, crate_name: str | None) -> str | None:
    """Which cargo target compiles this file.

    Anchored on the crate root, because that is the only place cargo's layout means
    anything:

    - `<crate>/src/**` is `lib` -- including `src/**/tests/*.rs`, which is a module
      named `tests`, not an integration-test target
    - `<crate>/tests/foo.rs` (or `tests/foo/main.rs`) is `test:foo`
    - `<crate>/src/main.rs` and `<crate>/src/bin/foo.rs` are both
      `bin:<name cargo actually uses>` -- read from `Cargo.toml`, never inferred from
      the path, because a `[[bin]]` entry may rename either
    - `<crate>/examples/foo.rs` is `example:foo`, `<crate>/benches/foo.rs` is `bench:foo`

    Matching `tests` at any depth -- which is what this did first -- classified
    `src/agent/subagent/tests/rest.rs` as `test:rest`, a target that does not exist, so
    no filter could match and every test in those modules was reported as never run.
    Note the shape of that mistake: scoping filters per target replaced a rule that was
    too permissive with a *classifier*, and the classifier became the new place to be
    wrong. Review then found the same class one branch away -- `src/bin/**` was still
    inferring the target from the filename stem, in a workspace with five bins whose
    `[[bin]] name` differs from it. Fixing one instance of a mistake is not fixing the
    mistake.
    """
    split = _crate_split(path)
    if split is None:
        return None
    crate_dir, rel = split
    if not rel:
        return None
    if rel[0] == "tests" and len(rel) > 1:
        if len(rel) > 2 or not rel[-1].endswith(".rs"):
            # `tests/<dir>/**` is only a target when `tests/<dir>/main.rs`
            # exists. Otherwise the files are pulled into one or more sibling
            # roots by `#[path]`, and the directory names no target at all --
            # `tests/pty_e2e/` has no `main.rs`, and its cases compile into
            # `pty_e2e_minimal`, `pty_e2e_queue` and the rest.
            #
            # Naming the directory was how a `ci.yml` line came to say
            # `--test pty_e2e`: `cargo test` errors on it, so the invocation
            # never produced a count, and this classifier agreed with the
            # fiction, so the guard passed too. Two readers agreeing on a
            # target that does not exist is worse than either being wrong
            # alone.
            if (Path(crate_dir) / "tests" / rel[1] / "main.rs").exists():
                return f"test:{rel[1]}"
            including = _test_root_including(crate_dir, "/".join(rel[1:]))
            if including:
                return f"test:{including}"
        return f"test:{rel[1].rsplit('.rs', 1)[0]}"
    if rel[0] == "examples" and len(rel) > 1:
        return f"example:{rel[1].rsplit('.rs', 1)[0]}"
    if rel[0] == "benches" and len(rel) > 1:
        return f"bench:{rel[1].rsplit('.rs', 1)[0]}"
    if rel[0] == "src":
        if len(rel) > 2 and rel[1] == "bin":
            stem = rel[2].rsplit(".rs", 1)[0]
            rel_posix = "/".join(rel)
            return f"bin:{_bin_target_name(crate_dir, rel_posix, stem)}"
        if rel[-1] == "main.rs" and len(rel) == 2:
            return f"bin:{_bin_target_name(crate_dir, 'src/main.rs', crate_name or 'bin')}"
        return "lib"
    return None


def added_tests(diff: str) -> list[tuple[str, str]]:
    """(file, test_fn_name) for every test function this diff adds.

    A test is "added" when an added `#[test]` / `#[tokio::test]` attribute is
    followed by an added `fn`. Requiring the attribute to be added too means a
    function merely reindented under an existing attribute is not counted.
    """
    out: list[tuple[str, str]] = []
    current_file = ""
    pending_attr = False
    for line in diff.splitlines():
        m = _DIFF_FILE.match(line)
        if m:
            current_file = m.group(1)
            pending_attr = False
            continue
        if _TEST_ATTR.match(line):
            pending_attr = True
            continue
        if pending_attr:
            fn = _FN.match(line)
            if fn:
                out.append((current_file, fn.group(1)))
                pending_attr = False
            elif line.startswith("+") and line.strip() not in ("+", "+}"):
                # Another added attribute, comment, or other helper between
                # the test attribute and the fn: keep looking as long as it
                # cannot end a Rust item (items end with ';' or '}').
                if ";" in line or "}" in line:
                    pending_attr = False
    return out


def _file_module_components(file_path: str | Path) -> list[str]:
    """Approximate Rust module components from a source file path under `src/`."""
    path = Path(file_path)
    parts = list(path.parts)
    try:
        src_index = parts.index("src")
    except ValueError:
        return []
    module_components = parts[src_index + 1 :]
    if module_components:
        stem = Path(module_components[-1]).stem
        if stem == "mod":
            module_components.pop()
        else:
            module_components[-1] = stem
    return module_components


def enclosing_inline_mods(source: str, test_fn: str) -> list[str]:
    """Inline `mod name { ... }` path enclosing `fn test_fn`, if present (#460).

    File-path approximation cannot see modules that never become their own file.
    A line-oriented brace walk is enough for the ci.yml filters that previously
    looked unresolvable: they name an inline `tests` (or similar) block.
    """
    return _inline_mods_index(source).get(test_fn, [])


def _inline_mods_index(source: str) -> dict[str, list[str]]:
    """Map each `fn` name to the inline `mod` stack at its definition site."""
    stack: list[tuple[int, str]] = []
    depth = 0
    index: dict[str, list[str]] = {}
    for raw in source.splitlines():
        code = raw.split("//", 1)[0]
        for match in _INLINE_MOD_OPEN.finditer(code):
            stack.append((depth, match.group(1)))
        for match in _FN_NAME.finditer(code):
            index[match.group(1)] = [name for _, name in stack]
        depth += code.count("{") - code.count("}")
        while stack and depth <= stack[-1][0]:
            stack.pop()
    return index


def _inline_mods_for_path(path: Path, test_fn: str) -> list[str]:
    key = str(path)
    if key not in _INLINE_INDEX_CACHE:
        source = _read_source(path)
        _INLINE_INDEX_CACHE[key] = _inline_mods_index(source) if source else {}
    return _INLINE_INDEX_CACHE[key].get(test_fn, [])


def _read_source(path: Path) -> str | None:
    key = str(path)
    if key in _SOURCE_CACHE:
        return _SOURCE_CACHE[key]
    try:
        text = path.read_text(encoding="utf-8", errors="ignore")
    except OSError:
        _SOURCE_CACHE[key] = ""
        return None
    _SOURCE_CACHE[key] = text
    return text


def path_attr_module_components(file_path: Path) -> list[str] | None:
    """Module path when `file_path` is pulled in via same-dir `#[path] mod NAME` (#460).

    Example: `auth/manager.rs` declares `#[path = "manager_tests.rs"] mod tests;`,
    so tests in `manager_tests.rs` live at `auth::manager::tests`, not
    `auth::manager_tests`.
    """
    key = str(file_path)
    if key in _PATH_ATTR_CACHE:
        return _PATH_ATTR_CACHE[key]
    if not file_path.name.endswith(".rs"):
        _PATH_ATTR_CACHE[key] = None
        return None
    parent = file_path.parent
    if not parent.is_dir():
        _PATH_ATTR_CACHE[key] = None
        return None
    want = file_path.name
    try:
        siblings = sorted(p for p in parent.glob("*.rs") if p != file_path)
    except OSError:
        _PATH_ATTR_CACHE[key] = None
        return None
    result: list[str] | None = None
    for sibling in siblings:
        text = _read_source(sibling)
        if text is None:
            continue
        for path_match in _PATH_ATTR.finditer(text):
            rel = path_match.group(1)
            if Path(rel).name != want and rel != want:
                continue
            after = text[path_match.end() : path_match.end() + 240]
            mod_match = _MOD_SEMI.search(after)
            if mod_match is None:
                continue
            result = _file_module_components(sibling) + [mod_match.group(1)]
            break
        if result is not None:
            break
    _PATH_ATTR_CACHE[key] = result
    return result


def _module_path_candidates(test_fn: str, file_path: str) -> list[list[str]]:
    """Every module-path candidate `selected` should try for this test (#460)."""
    module_components = _file_module_components(file_path)
    candidates = [module_components + [test_fn]]
    # Sibling `*_tests.rs` files are commonly `#[path]`-included as `mod tests`
    # from the stem before `_tests` (restore_fetch_tests → restore_fetch).
    if module_components and module_components[-1].endswith("_tests"):
        included = module_components.copy()
        included[-1] = included[-1].removesuffix("_tests")
        candidates.append(included + [test_fn])

    path = Path(file_path)
    if not path.is_file():
        return candidates

    inline = _inline_mods_for_path(path, test_fn)
    if inline:
        candidates.append(module_components + inline + [test_fn])

    via_path_attr = path_attr_module_components(path)
    if via_path_attr is not None:
        candidates.append(via_path_attr + [test_fn])
    return candidates


def selected(test_fn: str, file_path: str, filters: set[str]) -> bool:
    candidates: list[list[str]] | None = None

    def module_filter_matches(filter_value: str) -> bool:
        nonlocal candidates
        wanted = [component for component in filter_value.strip(":").split("::") if component]
        if not wanted:
            return False
        if candidates is None:
            candidates = _module_path_candidates(test_fn, file_path)
        if any(
            candidate[start : start + len(wanted)] == wanted
            for candidate in candidates
            for start in range(len(candidate) - len(wanted) + 1)
        ):
            return True
        # Cargo filters are substrings of the full test path. A filter like
        # `leader::lock::tests::reclaim` selects `...::reclaim_removes_...`
        # even though `reclaim` is not its own path component (#460).
        # Keep the `::` delimiters — stripping them would let `session::`
        # match inside `session_state`.
        return any(filter_value in "::".join(candidate) for candidate in candidates)

    for f in filters:
        if not f:
            return True
        if f in test_fn:
            return True
        # Module-path filters (`slash::commands::model::`) cannot be matched
        # against a bare fn name; approximate the module path from the file,
        # plus inline / `#[path]` modules the file-path walk cannot see (#460).
        # Compare Rust components rather than raw substrings: Cargo's
        # `session::` filter selects a `session` module but not the
        # `extensions::session_state` module.
        if "::" in f and module_filter_matches(f):
            return True
    return False


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--workflow", required=True, type=Path)
    ap.add_argument("--base", default="origin/providers",
                    help="ref to diff against (default: origin/providers)")
    ap.add_argument("--diff-file", type=Path, default=None,
                    help="read a pre-captured diff instead of running git")
    args = ap.parse_args()

    if args.diff_file:
        diff = args.diff_file.read_text()
    else:
        merge_base = subprocess.run(
            ["git", "merge-base", "HEAD", args.base],
            capture_output=True, text=True, check=False,
        ).stdout.strip()
        if not merge_base:
            print(f"error: no merge base with {args.base}", file=sys.stderr)
            return 2
        diff = subprocess.run(
            ["git", "diff", "-U0", f"{merge_base}...HEAD"],
            capture_output=True, text=True, check=False,
        ).stdout

    per_crate = parse_workflow(args.workflow.read_text())
    tests = added_tests(diff)

    if not tests:
        print("no new tests in this diff")
        return 0

    unselected: list[tuple[str, str, str]] = []
    for file_path, fn in tests:
        crate = crate_of(file_path)
        if crate is None:
            continue
        target_filters = per_crate.get(crate, {})
        # Union over every target that compiles this file: a shared
        # integration-test module belongs to each root that declares it, and a
        # filter naming any one of them runs the test.
        targets = targets_of(file_path, crate) or ["lib"]
        filters = target_filters.get("*", set())
        for t in targets:
            filters = filters | target_filters.get(t, set())
        if not selected(fn, file_path, filters):
            unselected.append((crate, file_path, fn))

    print(f"{len(tests)} new test(s); {len(unselected)} selected by no ci.yml filter")
    if not unselected:
        return 0

    print()
    for crate, file_path, fn in unselected:
        print(f"  {fn}")
        print(f"      {file_path}")
        target_filters = per_crate.get(crate, {})
        formatted_filters = []
        unresolved_module_filters: list[str] = []
        for tgt in sorted(target_filters):
            flts = sorted(target_filters[tgt])
            if flts:
                formatted_filters.append(f"{tgt}: {flts}")
            for flt in flts:
                if "::" not in flt:
                    continue
                wanted = [c for c in flt.strip(":").split("::") if c]
                # Honest #460 signal: a module-path filter whose components never
                # appear in any candidate for this file — including after inline
                # / #[path] resolution — cannot be judged as "unenrolled" the
                # same way a name filter can.
                candidates = _module_path_candidates(fn, file_path)
                if wanted and not any(
                    candidate[start : start + len(wanted)] == wanted
                    for candidate in candidates
                    for start in range(max(0, len(candidate) - len(wanted) + 1))
                ):
                    # Only report when the filter names a component the file
                    # path itself does not provide (likely inline / path-attr).
                    file_comps = set(_file_module_components(file_path))
                    if any(comp not in file_comps and comp != fn for comp in wanted):
                        unresolved_module_filters.append(flt)
        print(f"      crate `{crate}` filters: {formatted_filters if formatted_filters else '(none)'}")
        if unresolved_module_filters:
            print(
                "      note: these module-path filters name components this "
                "guard still cannot resolve for this file "
                f"(#460): {sorted(set(unresolved_module_filters))}"
            )
    print()
    print("These tests will not run in CI. `ci.yml` has no unfiltered test job, so a")
    print("test no filter selects executes nowhere -- not on a developer machine if it")
    print("is platform-gated, and not in CI either (#171).")
    print()
    print("Fix by either widening an existing filter in `.github/workflows/ci.yml` or")
    print("adding a `run_nonzero -p <crate> --lib <filter> -- --nocapture` line for the")
    print("new area. `run_nonzero` fails on a zero-match filter, so a filter that stops")
    print("matching is caught too.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
