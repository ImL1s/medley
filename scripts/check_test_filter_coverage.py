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

# Shared with the sibling guards so this copy cannot drift out of
# tolerance again (#494); this script is the one whose miss produced a
# wrong name rather than a missing one -- see `workspace_members`.
from toml_package_name import package_name

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


def workspace_members(root: Path) -> set[str]:
    """Crate names from the root manifest's `[workspace] members`.

    The sweep cannot be derived from the workflow's own `cargo test` lines: a
    crate whose last test lane is deleted then vanishes from the report, which
    is the guard going quiet about exactly the change that made all of its tests
    stop running. Nor from the baseline, which has no entry for a crate that is
    fully covered -- `xai-grok-auth`, `xai-grok-pager-bin` and `xai-proto-build`
    are all at 100% and would drop out of both sides (#408 review).
    """
    text = (root / "Cargo.toml").read_text()
    block = re.search(r"^members\s*=\s*\[(.*?)\]", text, re.S | re.M)
    if not block:
        return set()
    names = set()
    for m in re.finditer(r'"([^"]+)"', block.group(1)):
        rel = m.group(1)
        if "*" in rel:
            continue
        # The package name is what `-p` takes, and it is NOT reliably the last
        # path component: `prod/mc/cli-chat-proxy-types` is the package
        # `prod-mc-cli-chat-proxy-types`. Deriving it from the path is right for
        # 76 of these 81 and silently wrong for the rest, which surfaced as
        # `did not match any packages` -- caught only because a failed listing
        # is fatal (#408 review). Read the manifest.
        #
        # `package_name` reads the `[package]` table specifically. The
        # whole-file `re.search` this replaced would have returned a
        # `[[bin]] name` declared above `[package]`; no member in this tree is
        # ordered that way today, so the change is measured to move no name
        # (81/81 unchanged), and it removes the ordering dependency (#494).
        # The path fallback stays: a manifest that cannot be read or declares
        # no `[package]` still has to contribute *something* to the sweep, and
        # dropping it silently is the failure this guard exists to prevent.
        manifest = root / rel / "Cargo.toml"
        try:
            declared = package_name(manifest.read_text())
        except OSError:
            declared = None
        names.add(declared or Path(rel).name)
    return names


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


def _parse_workflow(text: str):
    """Map crate -> target -> filters, and the same keyed by lane `--features`.

    Joins YAML line continuations first: `ci.yml` wraps long invocations with a
    trailing backslash, and the filter is usually on the continuation line.
    """
    joined = re.sub(r"\\\s*\n\s*", " ", text)
    per_crate: dict[str, dict[str, set[str]]] = defaultdict(lambda: defaultdict(set))
    # Same filters, keyed additionally by the lane's `--features`, so coverage
    # can be judged against the build a filter actually ran in (#408 review).
    by_features: dict[str, dict[frozenset, dict[str, set[str]]]] = defaultdict(
        lambda: defaultdict(lambda: defaultdict(set))
    )

    for line in joined.splitlines():
        m = _RUNNER.match(_strip_env_assignments_prefix(line))
        if not m:
            continue
        # Drop anything after the `--` separator: those are libtest args
        # (`--nocapture`, `--skip <pat>`), not filters.
        # `--exact` changes libtest's selection from substring to equality, so a
        # lane carrying it must be judged by equality too -- otherwise a test
        # whose path merely *contains* the filter is reported covered by a lane
        # that will not run it (#408 review).
        tail = m.group(1).split(" -- ")[1] if " -- " in m.group(1) else ""
        exact = "--exact" in tail.split()
        args_text = m.group(1).split(" -- ")[0]
        try:
            args = shlex.split(args_text)
        except ValueError:
            continue

        crate: str | None = None
        filters: list[str] = []
        targets: list[str] = []
        features: list[str] = []
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
                    elif a == "--features":
                        # A lane's feature set decides which tests even exist in
                        # its build, so a filter cannot be credited with covering
                        # a test that is `#[cfg(feature = ...)]`-ed out of it.
                        features.extend(v for v in val.replace(",", " ").split() if v)
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

        feat = frozenset(features)
        # An exact filter is recorded with a sentinel prefix so the matcher can
        # tell the two selection semantics apart without a parallel structure.
        marked = [(EXACT_PREFIX + f if exact else f) for f in filters]
        if filters:
            for t in targets:
                per_crate[crate][t].update(filters)
                by_features[crate][feat][t].update(marked)
        elif not has_original_filters:
            # No positional filter and no target restriction: the crate's lib
            # tests run unfiltered, so everything in it is covered. The
            # empty string is a substring of every name.
            # Decide for yourself whether an unfiltered target-scoped invocation should still count as full coverage for that target only, and say why in the code.
            # Ans: Yes, because an unfiltered target-scoped cargo test runs all tests within that target, covering all of them.
            for t in targets:
                per_crate[crate][t].add("")
                by_features[crate][feat][t].add("")

    # Convert defaultdicts to regular dicts for a clean return shape
    flat = {c: dict(targets_dict) for c, targets_dict in per_crate.items()}
    nested = {
        c: {f: dict(t) for f, t in feats.items()} for c, feats in by_features.items()
    }
    return flat, nested


def parse_workflow(text: str) -> dict[str, dict[str, set[str]]]:
    """Crate -> target -> filters, ignoring which `--features` each lane used.

    Kept as the module's public shape because `tests/test_new_test_filter_guard.py`
    pins it. The feature-keyed view is a strictly finer grouping of the same
    parse, so it gets its own name rather than changing this one's contract --
    breaking eleven tests to accommodate a refactor is the wrong direction.
    """
    return _parse_workflow(text)[0]


# Marks a filter that came from a lane carrying `--exact`, where libtest matches
# by equality rather than substring.
EXACT_PREFIX = "\0exact\0"


def _selects(filter_: str, test: str) -> bool:
    if filter_.startswith(EXACT_PREFIX):
        return filter_[len(EXACT_PREFIX):] == test
    return filter_ in test


# Distinct from an empty listing, which is a legitimate "this crate has no lib
# tests". Both used to collapse into `[]`, so a crate whose harness could not be
# launched vanished from the report and the run still exited 0.
NO_LIB_TARGET = object()
LISTING_FAILED = object()


def list_tests(crate: str, manifest_root: Path, features: frozenset = frozenset()):
    """Test paths in the crate's **lib** target.

    Lib-scoped on purpose: `ci.yml`'s filters are overwhelmingly `--lib`, and a
    `--test <target>` invocation covers that target rather than the lib, so
    comparing a whole-crate listing against lib filters would report integration
    tests as uncovered when they are simply a different surface. Keeping both
    sides lib-only makes "uncovered" mean one thing.
    """
    cmd = ["cargo", "test", "-p", crate, "--lib"]
    if features:
        cmd += ["--features", ",".join(sorted(features))]
    proc = subprocess.run(
        cmd + ["--", "--list"],
        cwd=manifest_root,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        # A crate with no lib target is an expected skip: this tool is
        # lib-scoped (see the docstring), and such a crate's tests are reached
        # by `--bins` / `--test` filters it deliberately does not model. Say so
        # rather than warning, so it cannot be mistaken for the other branch.
        if "no library targets found" in proc.stderr:
            return NO_LIB_TARGET
        # Anything else means the listing did not happen. Returning an empty
        # list here would make the crate silently drop out of the comparison
        # and the run still exit 0 -- a guard that cannot tell "nothing new"
        # from "never checked" is the defect this whole check exists to catch.
        label = f"{crate}" + (f" --features {','.join(sorted(features))}" if features else "")
        print(f"error: could not list tests for {label}", file=sys.stderr)
        print(proc.stderr[-2000:], file=sys.stderr)
        return LISTING_FAILED
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

    per_crate, by_features = _parse_workflow(args.workflow.read_text())
    if not per_crate:
        print("error: no cargo test invocations found -- has ci.yml's shape changed?", file=sys.stderr)
        return 2

    # Sweep the workflow's crates UNION the baseline's. Deriving the sweep only
    # from surviving `cargo test` lines means deleting a crate's last lane also
    # deletes it from this report -- the guard would go quiet about exactly the
    # change that made every one of its tests stop running (#408 review).
    baseline_crates: set[str] = set()

    baseline: set[str] = set()
    if args.baseline and args.baseline.exists():
        baseline = {
            line.strip()
            for line in args.baseline.read_text().splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        }
        baseline_crates = {e.split("::", 1)[0] for e in baseline if "::" in e}

    members = workspace_members(args.root)
    if not args.crate and len(members) < 20:
        # A manifest this tool cannot parse must read as red, not as "no crates,
        # no gaps, ok" -- the failure mode this whole check exists to catch.
        print(
            f"error: only {len(members)} workspace member(s) parsed from "
            f"{args.root / 'Cargo.toml'}; refusing to sweep from a corpus that "
            "small.",
            file=sys.stderr,
        )
        return 2
    crates = args.crate or sorted(set(per_crate) | baseline_crates | members)

    all_uncovered: list[str] = []
    newly_uncovered: list[str] = []
    unlistable: list[str] = []
    stale_covered: list[str] = []   # exempt, but a filter now selects them
    stale_absent: list[str] = []    # exempt, but the test no longer exists

    for crate in crates:
        lanes = by_features.get(crate, {})

        # Every feature set any lane builds this crate with, plus default
        # features. A test gated behind `--features X` exists only in the
        # X build, so listing with defaults alone reports it as absent and the
        # crate as fully covered (#408 review: xai-grok-auth read 6/6 while 10
        # `retry_middleware` tests behind `--features middleware` ran nowhere).
        feature_sets = {frozenset()} | set(lanes)

        if args.list_from and any(lanes):
            # A captured `<crate>.list` has no feature dimension, so every test
            # in it lands under default features while a `--features` lane's
            # filters are judged against that lane's own listing. The verdict
            # would be wrong in both directions, so refuse instead (#408 review).
            if any(f for f in lanes if f):
                print(
                    f"error: {crate} has a --features lane; captured listings "
                    "cannot express feature sets. Re-run without --list-from.",
                    file=sys.stderr,
                )
                unlistable.append(crate)
                continue

        if args.list_from:
            f = args.list_from / f"{crate}.list"
            if not f.exists():
                # Same reasoning as a failed `--list`: a missing capture is not
                # evidence of anything, so it must not pass silently.
                print(f"error: no captured list for {crate}", file=sys.stderr)
                unlistable.append(crate)
                continue
            captured = [line[: -len(": test")].strip()
                        for line in f.read_text().splitlines() if line.endswith(": test")]
            listings = {frozenset(): captured}
        else:
            listings = {}
            failed = False
            for feat in sorted(feature_sets, key=lambda fs: sorted(fs)):
                got = list_tests(crate, args.root, feat)
                if got is NO_LIB_TARGET:
                    listings = NO_LIB_TARGET
                    break
                if got is LISTING_FAILED:
                    failed = True
                    break
                listings[feat] = got
            if failed:
                unlistable.append(crate)
                continue

        if listings is NO_LIB_TARGET:
            print(f"{crate}: no lib target -- skipped (this check is lib-scoped)")
            continue

        tests = sorted({t for names in listings.values() for t in names})
        if not tests:
            print(f"{crate}: 0 tests in the lib target")
            continue

        # A lane covers a test only if the test EXISTS in that lane's build and
        # one of its filters matches. Crediting a default-feature filter with a
        # feature-gated test is how the gap above stayed invisible.
        def _is_covered(test: str) -> bool:
            for feat, target_filters in lanes.items():
                if test not in listings.get(feat, ()):
                    continue
                fs = target_filters.get("lib", set()) | target_filters.get("*", set())
                if any(_selects(f, test) for f in fs):
                    return True
            return False

        filters = {
            f
            for target_filters in lanes.values()
            for f in target_filters.get("lib", set()) | target_filters.get("*", set())
        }
        if not lanes:
            # Named loudly rather than silently skipped, which is the vanishing
            # this sweep was widened to prevent. But its tests are NOT folded
            # into this baseline: "no lane names this crate at all" is exactly
            # `check_uncovered_crates.py`'s verdict (#280), and it has its own
            # allowlist. Two guards owning one fact means two places to grant an
            # exemption and one of them will drift.
            print(
                f"{crate}: no filter in the workflow at all -- deferring to "
                "check_uncovered_crates.py (#280)"
            )
            continue
        missing = [t for t in tests if not _is_covered(t)]

        # Baseline hygiene, both directions (#408 review). An entry that a
        # filter now selects must be deleted, or re-narrowing that filter later
        # silently re-exempts the test and the ratchet only ever loosens.
        crate_baseline = {e for e in baseline if e.startswith(f"{crate}::")}
        present = {f"{crate}::{t}" for t in tests}
        missing_keys = {f"{crate}::{t}" for t in missing}
        stale_covered.extend(sorted(crate_baseline & present - missing_keys))
        stale_absent.extend(sorted(crate_baseline - present))
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

    # Before any verdict: a crate we could not list was not checked, and a
    # verdict that ignores it is a claim about tests nobody enumerated. This
    # also guards --write-baseline, so a baseline can never be written from a
    # partial sweep and then silently exempt whatever the sweep missed.
    if unlistable:
        print(
            f"\nerror: could not list tests for {len(unlistable)} crate(s): "
            + ", ".join(unlistable)
            + "\n       This check did not run for them, which is not the same as"
            " them passing.\n       Fix the listing (or add a deliberate skip) rather"
            " than reading this run as green.",
            file=sys.stderr,
        )
        return 2

    # A stale exemption is a loosened ratchet, so it fails like a new one.
    # `--write-baseline` regenerates from the current sweep, which prunes both
    # kinds, so the fix is mechanical once the change is deliberate.
    if stale_covered and not args.write_baseline:
        print(
            f"\nerror: {len(stale_covered)} baseline entr(ies) are now selected by a "
            "filter and must be removed:",
            file=sys.stderr,
        )
        for e in stale_covered[:50]:
            print(f"    {e}", file=sys.stderr)
        if len(stale_covered) > 50:
            print(f"    ... and {len(stale_covered) - 50} more", file=sys.stderr)
        print(
            "       Leaving them exempt lets a later narrowing of that filter "
            "un-cover the test\n       without this check noticing.",
            file=sys.stderr,
        )
        return 1

    # Deliberately NOT fatal: a test that no longer exists is not a regression,
    # and failing on it would turn every rename into a red build. Reported so
    # the baseline can be pruned, and re-generating is what prunes it.
    if stale_absent and not args.write_baseline:
        print(
            f"\nnote: {len(stale_absent)} baseline entr(ies) name tests that no longer "
            "exist; regenerate to prune."
        )

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
