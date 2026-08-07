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
against the file's path.

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

_TEST_ATTR = re.compile(r"^\+\s*#\[(?:tokio::)?test\b")
_FN = re.compile(r"^\+\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)")
_DIFF_FILE = re.compile(r"^\+\+\+ b/(.+)$")


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
        name = re.search(r'^\s*name\s*=\s*"([^"]+)"', block, flags=re.M)
        if not name:
            continue
        path_m = re.search(r'^\s*path\s*=\s*"([^"]+)"', block, flags=re.M)
        if path_m is None:
            # No `path`: cargo infers it from the name, so this entry only claims
            # `rel_path` if the inferred location matches.
            inferred = {f"src/bin/{name.group(1)}.rs", "src/main.rs"}
            if want in inferred:
                return name.group(1)
            continue
        if path_m.group(1).removeprefix("./") == want:
            return name.group(1)
    return fallback


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


def selected(test_fn: str, file_path: str, filters: set[str]) -> bool:
    for f in filters:
        if not f:
            return True
        if f in test_fn:
            return True
        # Module-path filters (`slash::commands::model::`) cannot be matched
        # against a bare fn name; approximate the module path from the file.
        if "::" in f:
            as_path = f.strip(":").replace("::", "/")
            if as_path in file_path:
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
        t = target_of(file_path, crate)
        if t is None:
            t = "lib"
        filters = target_filters.get(t, set()) | target_filters.get("*", set())
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
        for tgt in sorted(target_filters):
            flts = sorted(target_filters[tgt])
            if flts:
                formatted_filters.append(f"{tgt}: {flts}")
        print(f"      crate `{crate}` filters: {formatted_filters if formatted_filters else '(none)'}")
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
