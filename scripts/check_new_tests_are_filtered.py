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


def crate_of(path: str) -> str | None:
    """`crates/codegen/xai-grok-shell/src/...` -> `xai-grok-shell`."""
    parts = Path(path).parts
    if "crates" not in parts:
        return None
    i = parts.index("crates")
    # crates/<group>/<crate>/...
    if len(parts) > i + 2:
        return parts[i + 2]
    return None


def target_of(path: str, crate_name: str | None) -> str | None:
    """Determine the target name and type from a file path.

    - `src/**` (excluding `main.rs`, `bin/**`) is `lib`
    - `tests/foo.rs` is `test:foo`
    - `src/main.rs`/`src/bin/**` is `bin:<crate_name>` / `bin:<name>`
    """
    parts = Path(path).parts
    if "tests" in parts:
        idx = parts.index("tests")
        if len(parts) > idx + 1:
            name = parts[idx + 1].rsplit(".rs", 1)[0]
            return f"test:{name}"
    if "examples" in parts:
        idx = parts.index("examples")
        if len(parts) > idx + 1:
            name = parts[idx + 1].rsplit(".rs", 1)[0]
            return f"example:{name}"
    if "src" in parts:
        if "bin" in parts:
            b_idx = parts.index("bin")
            if len(parts) > b_idx + 1:
                name = parts[b_idx + 1].rsplit(".rs", 1)[0]
                return f"bin:{name}"
        if parts[-1] == "main.rs":
            return f"bin:{crate_name}" if crate_name else "bin"
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
