#!/usr/bin/env python3
"""Report tests that no `ci.yml` filter selects (issue #171).

`ci.yml` has no unfiltered test job: every `cargo test` invocation names a
filter. A test matching none of them runs nowhere, and the author gets a green
tick that says nothing about it. `run_nonzero` already fails when a filter
matches zero tests -- the opposite direction, a test with no filter, has
nothing watching it.

This compares, per crate, the tests `cargo test -- --list` reports against the
set any filter would select. Cargo's filter is a plain substring match on the
full test path (there is no `--exact` in `ci.yml`), so "selected" is exactly
`any(f in test_path for f in filters)`.

Run it on the platform you care about. `#[cfg(target_os = "linux")]` tests are
absent from `--list` on macOS, which is precisely how the gap in #171 stayed
invisible: the author checked locally, where the test did not exist to be
missed.

Usage:
    check_test_filter_coverage.py --workflow .github/workflows/ci.yml [--crate NAME ...]
    check_test_filter_coverage.py --workflow ci.yml --list-from DIR

Exits 1 if any crate has uncovered tests.
"""

from __future__ import annotations

import argparse
import re
import shlex
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

# `run_nonzero -p <crate> ... <filter> -- --nocapture`
# and bare `cargo test --manifest-path <path> ... <filter> -- ...`
_RUNNER = re.compile(r"^\s*(?:run_nonzero|cargo test)\s+(.*)$")
_ENV_ASSIGNMENT_PREFIX = re.compile(
    r"^[A-Za-z_][A-Za-z0-9_]*=(?:\"[^\"]*\"|'[^']*'|\S+)\s+(.*)$"
)

# Flags that take a value, so the following token is not a filter.
_VALUED_FLAGS = {"-p", "--package", "--manifest-path", "--features", "--test", "--bin", "--example"}
# Valueless flags.
_BARE_FLAGS = {"--lib", "--all-targets", "--no-run", "--release", "--all-features", "--no-default-features"}

# Seen (token, line) pairs, so the shell-variable warning is emitted once per distinct
# workflow line rather than once per occurrence.
_warned_shell_tokens: set[tuple[str, str]] = set()


def _crate_from_manifest(path: str) -> str:
    """`crates/codegen/xai-grok-sampler/Cargo.toml` -> `xai-grok-sampler`."""
    return Path(path).parent.name


def _strip_env_assignments_prefix(line: str) -> str:
    """Drop leading `KEY=VALUE` assignments from a shell command line."""
    stripped = line.strip()
    while True:
        m = _ENV_ASSIGNMENT_PREFIX.match(stripped)
        if not m:
            return stripped
        stripped = m.group(1).lstrip()


def parse_workflow(text: str) -> dict[str, dict[str, set[str]]]:
    """Map crate -> target -> set of filter strings used against it.

    Joins YAML line continuations first: `ci.yml` wraps long invocations with a
    trailing backslash, and the filter is usually on the continuation line.
    """
    joined = re.sub(r"\\\s*\n\s*", " ", text)
    per_crate: dict[str, dict[str, set[str]]] = defaultdict(lambda: defaultdict(set))

    for line in joined.splitlines():
        m = _RUNNER.match(_strip_env_assignments_prefix(line))
        if not m:
            continue
        # Drop anything after the `--` separator: those are libtest args
        # (`--nocapture`, `--skip <pat>`), not filters.
        args_text = m.group(1).split(" -- ")[0]
        try:
            args = shlex.split(args_text)
        except ValueError:
            continue

        crate: str | None = None
        filters: list[str] = []
        targets: list[str] = []
        has_original_filters = False
        i = 0
        while i < len(args):
            a = args[i]
            if a in _VALUED_FLAGS:
                if i + 1 < len(args):
                    val = args[i + 1]
                    if a in ("-p", "--package"):
                        crate = val
                    elif a == "--manifest-path":
                        crate = _crate_from_manifest(val)
                    elif a == "--test":
                        targets.append(f"test:{val}")
                    elif a == "--bin":
                        targets.append(f"bin:{val}")
                    elif a == "--example":
                        targets.append(f"example:{val}")
                i += 2
                continue
            if a == "--lib":
                targets.append("lib")
                i += 1
                continue
            if a == "--all-targets":
                targets.append("*")
                i += 1
                continue
            if a in _BARE_FLAGS or a.startswith("-"):
                i += 1
                continue
            has_original_filters = True
            if "$" in a:
                # Deduplicated: `run_nonzero()` is redefined in every step that uses
                # it, so `cargo test "$@"` appears a dozen-odd times and an
                # unconditional warning buried the actual failure under thirty
                # identical lines in the CI log. A warning nobody reads is the same
                # silence it was added to break.
                key = (a, line.strip())
                if key not in _warned_shell_tokens:
                    _warned_shell_tokens.add(key)
                    print(
                        f"warning: ignoring filter token '{a}' containing shell "
                        f"variable in workflow line: {line.strip()}",
                        file=sys.stderr,
                    )
                i += 1
                continue
            filters.append(a)
            i += 1

        if crate is None:
            continue

        if not targets:
            targets = ["*"]

        if filters:
            for t in targets:
                per_crate[crate][t].update(filters)
        elif not has_original_filters:
            # No positional filter and no target restriction: the crate's lib
            # tests run unfiltered, so everything in it is covered. The
            # empty string is a substring of every name.
            # Decide for yourself whether an unfiltered target-scoped invocation should still count as full coverage for that target only, and say why in the code.
            # Ans: Yes, because an unfiltered target-scoped cargo test runs all tests within that target, covering all of them.
            for t in targets:
                per_crate[crate][t].add("")

    # Convert defaultdicts to regular dicts for a clean return shape
    return {c: dict(targets_dict) for c, targets_dict in per_crate.items()}


def list_tests(crate: str, manifest_root: Path) -> list[str]:
    """Test paths in the crate's **lib** target.

    Lib-scoped on purpose: `ci.yml`'s filters are overwhelmingly `--lib`, and a
    `--test <target>` invocation covers that target rather than the lib, so
    comparing a whole-crate listing against lib filters would report integration
    tests as uncovered when they are simply a different surface. Keeping both
    sides lib-only makes "uncovered" mean one thing.
    """
    proc = subprocess.run(
        ["cargo", "test", "-p", crate, "--lib", "--", "--list"],
        cwd=manifest_root,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        print(f"warning: could not list tests for {crate}", file=sys.stderr)
        print(proc.stderr[-2000:], file=sys.stderr)
        return []
    names = []
    for line in proc.stdout.splitlines():
        # libtest prints `some::path::test_name: test`
        if line.endswith(": test"):
            names.append(line[: -len(": test")].strip())
    return names


def uncovered(tests: list[str], filters: set[str]) -> list[str]:
    return [t for t in tests if not any(f in t for f in filters)]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--workflow", required=True, type=Path)
    ap.add_argument("--root", type=Path, default=Path("."))
    ap.add_argument("--crate", action="append", default=None,
                    help="restrict to these crates (default: every crate ci.yml filters)")
    ap.add_argument("--list-from", type=Path, default=None,
                    help="directory of pre-captured `<crate>.list` files, instead of running cargo")
    ap.add_argument("--baseline", type=Path, default=None,
                    help="file of already-uncovered test paths to exempt; only NEW ones fail")
    ap.add_argument("--write-baseline", type=Path, default=None,
                    help="write the current uncovered set to this file and exit 0")
    args = ap.parse_args()

    per_crate = parse_workflow(args.workflow.read_text())
    if not per_crate:
        print("error: no cargo test invocations found -- has ci.yml's shape changed?", file=sys.stderr)
        return 2

    crates = args.crate or sorted(per_crate)

    baseline: set[str] = set()
    if args.baseline and args.baseline.exists():
        baseline = {
            line.strip()
            for line in args.baseline.read_text().splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        }

    all_uncovered: list[str] = []
    newly_uncovered: list[str] = []

    for crate in crates:
        target_filters = per_crate.get(crate)
        if target_filters is None:
            print(f"{crate}: no filter in the workflow at all -- every test in it runs nowhere")
            continue

        if args.list_from:
            f = args.list_from / f"{crate}.list"
            if not f.exists():
                print(f"warning: no captured list for {crate}", file=sys.stderr)
                continue
            tests = [line[: -len(": test")].strip()
                     for line in f.read_text().splitlines() if line.endswith(": test")]
        else:
            tests = list_tests(crate, args.root)

        if not tests:
            continue

        filters = target_filters.get("lib", set()) | target_filters.get("*", set())
        missing = uncovered(tests, filters)
        fresh = [t for t in missing if f"{crate}::{t}" not in baseline]
        pct = 100.0 * (len(tests) - len(missing)) / len(tests)
        print(f"{crate}: {len(tests) - len(missing)}/{len(tests)} selected by a filter "
              f"({pct:.1f}%), {len(missing)} not selected"
              + (f", {len(fresh)} of them new" if baseline else "")
              + f", {len(filters)} filter(s)")

        all_uncovered.extend(f"{crate}::{t}" for t in missing)
        newly_uncovered.extend(f"{crate}::{t}" for t in fresh)

        if not args.write_baseline:
            for t in (fresh if baseline else missing):
                print(f"    {t}")

    if args.write_baseline:
        args.write_baseline.write_text(
            "# Tests no ci.yml filter selects, as of the commit that wrote this file.\n"
            "# Generated by scripts/check_test_filter_coverage.py --write-baseline.\n"
            "# Shrinking this list is good. Growing it means a new test runs nowhere.\n"
            + "".join(f"{t}\n" for t in sorted(all_uncovered))
        )
        print(f"\nwrote {len(all_uncovered)} entries to {args.write_baseline}")
        return 0

    if baseline:
        print(f"\n{len(all_uncovered)} not selected by any filter; "
              f"{len(newly_uncovered)} of those are new since the baseline")
        return 1 if newly_uncovered else 0

    print(f"\ntotal not selected by any filter: {len(all_uncovered)}")
    return 1 if all_uncovered else 0


if __name__ == "__main__":
    sys.exit(main())
