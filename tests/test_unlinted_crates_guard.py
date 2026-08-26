"""Tests for the #439 lint-level CI ratchet in `scripts/`.

The checker exists because `ci.yml`'s clippy job names its crates one at a
time: a crate that is not named is not linted, and nothing reports either the
omission or the failures hiding behind it. `xai-grok-workspace` sat with three
clippy errors while `providers` looked green.

Every fixture here is synthetic. The guard's red direction has to be tested by
feeding it a workflow with a crate removed, and doing that by editing the real
`ci.yml` and restoring it afterwards is how a mutated workflow leaks into a
commit -- so nothing below touches the repository's own tree.
"""

from __future__ import annotations

import re
import shlex
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "scripts"))

from check_unlinted_crates import (  # noqa: E402
    AllowlistError,
    evaluate,
    iter_crates,
    linted_invocation_tokens,
    linted_tokens,
    load_allowlist,
    main,
    package_name,
    workspace_member_dirs,
)

CLIPPY = "cargo clippy --manifest-path crates/{d}/Cargo.toml --all-targets -- -D warnings"


def _write(path: Path, body: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(body), encoding="utf-8")


def _workspace(root: Path, crates: dict[str, str]) -> None:
    """`crates` maps member directory name -> package name."""
    members = "".join(f'    "crates/{d}",\n' for d in sorted(crates))
    _write(
        root / "Cargo.toml",
        f"[workspace]\nresolver = \"2\"\nmembers = [\n{members}]\n",
    )
    for directory, name in crates.items():
        _write(
            root / "crates" / directory / "Cargo.toml",
            f'[package]\nname = "{name}"\nversion = "0.1.0"\n',
        )


def _workflow(*lines: str) -> str:
    return "jobs:\n  lint:\n    steps:\n      - run: |\n" + "".join(
        f"          {line}\n" for line in lines
    )


class LintedTokens(unittest.TestCase):
    def test_counts_a_deny_level_all_targets_invocation(self):
        text = _workflow(CLIPPY.format(d="xai-grok-shell"))
        self.assertEqual(linted_tokens(text), {"xai-grok-shell"})

    def test_ignores_an_invocation_without_all_targets(self):
        # `--lib`-only clippy never sees test or bench targets; counting it
        # would move the job's blind spot inside this guard.
        text = _workflow(
            "cargo clippy --manifest-path crates/xai-grok-shell/Cargo.toml --lib -- -D warnings"
        )
        self.assertEqual(linted_tokens(text), set())

    def test_ignores_an_invocation_without_deny_warnings(self):
        text = _workflow(
            "cargo clippy --manifest-path crates/xai-grok-shell/Cargo.toml --all-targets"
        )
        self.assertEqual(linted_tokens(text), set())

    def test_ignores_a_bare_package_mention_from_another_job(self):
        # A `cargo test -p X` elsewhere in ci.yml says nothing about linting.
        text = _workflow("cargo test -p xai-grok-shell --lib")
        self.assertEqual(linted_tokens(text), set())

    def test_joins_line_continuations(self):
        text = (
            "      - run: |\n"
            "          cargo clippy --manifest-path crates/xai-grok-tools/Cargo.toml \\\n"
            "            --all-targets --features pi -- -D warnings\n"
        )
        self.assertEqual(linted_tokens(text), {"xai-grok-tools"})

    def test_ignores_a_commented_out_invocation(self):
        # A crate disabled by prefixing its `cargo clippy` line with `#` must
        # not be counted as linted -- that recreates the exact blind spot
        # this guard exists to close (#439 follow-up).
        text = _workflow("# " + CLIPPY.format(d="xai-grok-shell"))
        self.assertEqual(linted_tokens(text), set())

    def test_ignores_an_indented_commented_out_invocation(self):
        text = _workflow("    # " + CLIPPY.format(d="xai-grok-shell"))
        self.assertEqual(linted_tokens(text), set())

    def test_truncates_a_trailing_inline_comment_before_matching_flags(self):
        # The flags only appear in prose after `#`; the command that actually
        # runs carries neither, so it must not read as deny-level.
        text = _workflow(
            "cargo clippy --manifest-path crates/xai-grok-shell/Cargo.toml "
            "# --all-targets -- -D warnings"
        )
        self.assertEqual(linted_tokens(text), set())

    def test_keeps_a_flag_carrying_command_with_a_trailing_comment(self):
        # A trailing comment on an otherwise-complete, executed command must
        # not stop it from counting.
        text = _workflow(CLIPPY.format(d="xai-grok-shell") + "  # still runs")
        self.assertEqual(linted_tokens(text), {"xai-grok-shell"})

    def test_keeps_manifest_dirs_and_package_names_apart(self):
        # `-p`/`--package` and `--manifest-path` are different namespaces.
        # linted_invocation_tokens() must not merge them -- evaluate() relies
        # on that separation to avoid crediting a crate for an invocation
        # that names the *other* namespace's identically-spelled token
        # (#439 follow-up).
        text = _workflow(
            CLIPPY.format(d="xai-grok-shell"),
            "cargo clippy -p xai-grok-tools --all-targets -- -D warnings",
        )
        tokens = linted_invocation_tokens(text)
        self.assertEqual(tokens.manifest_dirs, frozenset({"xai-grok-shell"}))
        self.assertEqual(tokens.package_names, frozenset({"xai-grok-tools"}))

    def test_linted_tokens_is_still_the_union_of_both_kinds(self):
        # Back-compat contract: linted_tokens() itself is unchanged, it is
        # only no longer what evaluate() uses to decide what is linted.
        text = _workflow(
            CLIPPY.format(d="xai-grok-shell"),
            "cargo clippy -p xai-grok-tools --all-targets -- -D warnings",
        )
        self.assertEqual(linted_tokens(text), {"xai-grok-shell", "xai-grok-tools"})

    def test_a_comment_line_does_not_absorb_the_next_real_command(self):
        # Comment-stripping must happen before the line-continuation join:
        # a trailing `\` inside a comment does not continue it in bash, so a
        # commented-out line must never swallow the command after it.
        text = (
            "      - run: |\n"
            "          # cargo clippy --manifest-path crates/xai-grok-tools/Cargo.toml \\\n"
            "          cargo clippy --manifest-path crates/xai-grok-shell/Cargo.toml --all-targets -- -D warnings\n"
        )
        self.assertEqual(linted_tokens(text), {"xai-grok-shell"})

    def test_ignores_an_invocation_that_masks_its_exit_status_with_or_true(self):
        # `|| true` makes the shell report success no matter what clippy
        # found -- the same blind spot as commenting the line out, just via
        # a live command whose failure never reaches the exit code
        # (#439 follow-up).
        text = _workflow(CLIPPY.format(d="xai-grok-shell") + " || true")
        self.assertEqual(linted_tokens(text), set())

    def test_ignores_an_invocation_that_masks_its_exit_status_with_or_colon(self):
        text = _workflow(CLIPPY.format(d="xai-grok-shell") + " || :")
        self.assertEqual(linted_tokens(text), set())

    def test_ignores_an_invocation_followed_by_a_semicolon_true(self):
        text = _workflow(CLIPPY.format(d="xai-grok-shell") + "; true")
        self.assertEqual(linted_tokens(text), set())

    def test_ignores_an_invocation_chained_with_and_true(self):
        text = _workflow(CLIPPY.format(d="xai-grok-shell") + " && true")
        self.assertEqual(linted_tokens(text), set())

    def test_ignores_an_invocation_piped_to_another_command(self):
        # A pipe hides clippy's exit status behind the pipeline's last
        # command unless `pipefail` is on -- this must not count regardless
        # of whether it is, so no per-step `set -o pipefail` tracking is
        # needed (see linted_invocation_tokens()'s docstring).
        text = _workflow(CLIPPY.format(d="xai-grok-shell") + " | tee clippy.log")
        self.assertEqual(linted_tokens(text), set())

    def test_ignores_an_invocation_with_a_redirected_stderr(self):
        # `2>&1` contains `&` and is deliberately disqualified too, even
        # though it does not itself mask an exit status -- the module
        # docstring names this as an accepted, deliberate cost of matching
        # the operator rather than an enumerated pattern list.
        text = _workflow(CLIPPY.format(d="xai-grok-shell") + " 2>&1")
        self.assertEqual(linted_tokens(text), set())

    def test_a_masked_invocation_with_a_trailing_comment_is_still_ignored(self):
        # Composition: comment-stripping runs first and does not resurrect
        # an operator-disqualified line.
        text = _workflow(CLIPPY.format(d="xai-grok-shell") + " || true  # keep")
        self.assertEqual(linted_tokens(text), set())

    def test_an_unmasked_invocation_with_a_trailing_comment_still_counts(self):
        # Composition, the other direction: a trailing comment alone must
        # not disqualify an otherwise-clean, unmasked invocation.
        text = _workflow(CLIPPY.format(d="xai-grok-shell") + "  # keep")
        self.assertEqual(linted_tokens(text), {"xai-grok-shell"})

    def test_ignores_an_echo_of_the_clippy_command(self):
        # `"cargo clippy" in line` also matches `echo cargo clippy ...`,
        # which never invokes clippy. Counting it would let a diagnostic
        # print stand in for a deny-level lint (#508 review).
        text = _workflow("echo " + CLIPPY.format(d="xai-grok-shell"))
        self.assertEqual(linted_tokens(text), set())

    def test_counts_a_package_flag_invocation(self):
        text = _workflow(
            "cargo clippy -p xai-grok-shell --all-targets -- -D warnings"
        )
        self.assertEqual(linted_tokens(text), {"xai-grok-shell"})


def _independent_linted_crates(workflow_text: str) -> set[str]:
    """Real deny-level `cargo clippy` crates, found by `shlex`-tokenizing
    each line -- deliberately not `linted_invocation_tokens()`'s regex
    checks.

    Every discrimination case above (`|| true`, `;`, pipes, unknown
    spellings, comments) already has a hand-written test in `LintedTokens`;
    what that class cannot show is whether the pattern is right about the
    real file, since its own fixtures chose the examples (#455). This is
    the other leg: comment-stripping and operator-disqualifying are done
    here with plain string ops and `shlex`, not the production regex, and
    "does this token look like `-D warnings`" is answered by checking two
    separate shlex tokens rather than a literal substring match.

    `cargo clippy` must occupy the command position (after optional
    `NAME=value` assignments), not merely appear as a substring --
    `echo cargo clippy ...` never lints. Both `--manifest-path`
    directories and `-p` / `--package` names are collected: comparing
    only directories lets a `-p`-only deny-level line drift out of the
    corpus (#508 review).
    """
    lines: list[str] = []
    for raw in workflow_text.splitlines():
        stripped = raw.strip()
        if stripped.startswith("#"):
            continue
        lines.append(raw.split("#", 1)[0] if "#" in raw else raw)
    joined = re.sub(r"\\\s*\n\s*", " ", "\n".join(lines))
    found: set[str] = set()
    for line in joined.splitlines():
        if any(op in line for op in (";", "&", "|")):
            continue
        try:
            tokens = shlex.split(line)
        except ValueError:
            continue
        i = 0
        while i < len(tokens) and re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", tokens[i]):
            i += 1
        if i + 1 >= len(tokens) or tokens[i] != "cargo" or tokens[i + 1] != "clippy":
            continue
        if "--all-targets" not in tokens:
            continue
        if not ("-D" in tokens and "warnings" in tokens):
            continue
        for j, tok in enumerate(tokens):
            if tok == "--manifest-path" and j + 1 < len(tokens):
                found.add(Path(tokens[j + 1]).parent.name)
            if tok in ("-p", "--package") and j + 1 < len(tokens):
                found.add(tokens[j + 1])
    return found


class IndependentLintedOracle(unittest.TestCase):
    """The corpus oracle must reject non-invocations and see `-p` (#508)."""

    def test_does_not_count_an_echo_of_clippy(self):
        text = _workflow("echo " + CLIPPY.format(d="xai-grok-shell"))
        self.assertEqual(_independent_linted_crates(text), set())

    def test_counts_a_package_flag(self):
        text = _workflow(
            "cargo clippy -p xai-grok-shell --all-targets -- -D warnings"
        )
        self.assertEqual(_independent_linted_crates(text), {"xai-grok-shell"})

    def test_counts_a_manifest_path(self):
        text = _workflow(CLIPPY.format(d="xai-grok-shell"))
        self.assertEqual(_independent_linted_crates(text), {"xai-grok-shell"})


class LintedTokensCorpus(unittest.TestCase):
    """`linted_tokens()` against the real `ci.yml`, enumerated by
    `_independent_linted_crates` rather than the pattern under test.

    Compared as the union of `--manifest-path` directories and `-p`
    package names -- not directories alone -- so a deny-level `-p` line
    cannot leave the corpus (#508 review).
    """

    @classmethod
    def setUpClass(cls):
        cls.workflow_text = (REPO / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
        cls.oracle = _independent_linted_crates(cls.workflow_text)
        cls.real = linted_tokens(cls.workflow_text)

    def test_the_corpus_is_not_empty(self):
        # A silent-empty scan would pass the agreement check below by
        # vacuously agreeing with nothing.
        self.assertGreater(len(self.oracle), 3, self.oracle)

    def test_agrees_with_an_independently_tokenized_reading_of_the_real_workflow(self):
        self.assertEqual(self.real, self.oracle)


class PackageName(unittest.TestCase):
    def test_double_quoted_name(self):
        self.assertEqual(package_name('[package]\nname = "foo"\n'), "foo")

    def test_single_quoted_name(self):
        self.assertEqual(package_name("[package]\nname = 'foo'\n"), "foo")

    def test_tolerates_a_trailing_comment(self):
        # Valid TOML -- `iter_crates` must not silently drop a crate spelled
        # this way (#439 follow-up).
        self.assertEqual(
            package_name('[package]\nname = "foo" # renamed pending #999\n'), "foo"
        )

    def test_tolerates_a_trailing_comment_with_no_space(self):
        self.assertEqual(package_name('[package]\nname = "foo"# no space\n'), "foo")

    def test_tolerates_a_comment_containing_quotes(self):
        self.assertEqual(
            package_name('[package]\nname = "foo" # was "bar" before\n'), "foo"
        )

    def test_trailing_non_comment_text_is_not_a_name(self):
        # Not valid TOML and not a comment either -- must not be silently
        # accepted as a match just because it starts with a quoted name.
        self.assertIsNone(package_name('[package]\nname = "foo" extra\n'))

    def test_a_virtual_manifest_has_no_package_name(self):
        self.assertIsNone(package_name('[workspace]\nmembers = ["a"]\n'))


class Corpus(unittest.TestCase):
    def test_enumerates_from_the_workspace_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _workspace(root, {"alpha": "alpha-crate", "beta": "beta-crate"})
            self.assertEqual(workspace_member_dirs(root), ["crates/alpha", "crates/beta"])
            self.assertEqual(
                sorted(c.name for c in iter_crates(root)),
                ["alpha-crate", "beta-crate"],
            )

    def test_a_crate_dir_outside_members_is_not_a_workspace_crate(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _workspace(root, {"alpha": "alpha-crate"})
            _write(
                root / "crates" / "stray" / "Cargo.toml",
                '[package]\nname = "stray-crate"\nversion = "0.1.0"\n',
            )
            self.assertEqual([c.name for c in iter_crates(root)], ["alpha-crate"])

    def test_a_crate_named_with_a_trailing_comment_is_still_iterated(self):
        # Regression for #439 follow-up: `iter_crates` used to silently
        # treat this as a virtual manifest with no package name and drop it.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _workspace(root, {"alpha": "alpha-crate"})
            _write(
                root / "crates" / "alpha" / "Cargo.toml",
                '[package]\nname = "alpha-crate" # renamed pending #999\nversion = "0.1.0"\n',
            )
            self.assertEqual([c.name for c in iter_crates(root)], ["alpha-crate"])


class Allowlist(unittest.TestCase):
    def test_reads_name_and_reason(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "allow"
            _write(path, "# header\nalpha-crate = deliberately not linted, see #1\n")
            self.assertEqual(
                load_allowlist(path), {"alpha-crate": "deliberately not linted, see #1"}
            )

    def test_a_missing_reason_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "allow"
            _write(path, "alpha-crate\n")
            with self.assertRaises(AllowlistError):
                load_allowlist(path)

    def test_an_empty_reason_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "allow"
            _write(path, "alpha-crate =   \n")
            with self.assertRaises(AllowlistError):
                load_allowlist(path)


class Evaluate(unittest.TestCase):
    def _root(self, tmp: str) -> Path:
        root = Path(tmp)
        _workspace(root, {"alpha": "alpha-crate", "beta": "beta-crate"})
        return root

    def test_an_unlinted_crate_that_is_not_allowlisted_is_a_gap(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self._root(tmp)
            report = evaluate(
                root=root,
                workflow_text=_workflow(CLIPPY.format(d="alpha")),
                allowlisted={},
            )
            self.assertEqual(report.new_gaps, frozenset({"beta-crate"}))
            self.assertFalse(report.ok)

    def test_the_same_crate_allowlisted_is_not_a_gap(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self._root(tmp)
            report = evaluate(
                root=root,
                workflow_text=_workflow(CLIPPY.format(d="alpha")),
                allowlisted={"beta-crate": "not yet triaged"},
            )
            self.assertEqual(report.new_gaps, frozenset())
            self.assertTrue(report.ok)

    def test_an_allowlisted_crate_that_became_linted_is_stale(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self._root(tmp)
            report = evaluate(
                root=root,
                workflow_text=_workflow(
                    CLIPPY.format(d="alpha"), CLIPPY.format(d="beta")
                ),
                allowlisted={"beta-crate": "not yet triaged"},
            )
            self.assertEqual(report.stale, frozenset({"beta-crate"}))
            self.assertFalse(report.ok)

    def test_an_allowlist_entry_that_is_not_a_workspace_crate_is_unknown(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self._root(tmp)
            report = evaluate(
                root=root,
                workflow_text=_workflow(
                    CLIPPY.format(d="alpha"), CLIPPY.format(d="beta")
                ),
                allowlisted={"deleted-crate": "left behind by a rename"},
            )
            self.assertEqual(report.unknown, frozenset({"deleted-crate"}))
            self.assertFalse(report.ok)

    def _collision_root(self, tmp: str) -> Path:
        # Crate "shared": directory `crates/shared`, package `alpha`.
        # Crate "other": directory `crates/other`, package `shared`.
        # The package name of one equals the directory name of the other.
        root = Path(tmp)
        _workspace(root, {"shared": "alpha", "other": "shared"})
        return root

    def test_a_package_flag_does_not_credit_a_crate_whose_directory_shares_its_spelling(
        self,
    ):
        # `-p shared` lints the package named `shared` (crate "other"). It
        # must not also credit crate "shared" (package `alpha`) just because
        # its *directory* happens to be spelled `shared` (#439 follow-up).
        with tempfile.TemporaryDirectory() as tmp:
            root = self._collision_root(tmp)
            report = evaluate(
                root=root,
                workflow_text=_workflow(
                    "cargo clippy -p shared --all-targets -- -D warnings"
                ),
                allowlisted={},
            )
            self.assertEqual(report.linted, frozenset({"shared"}))
            self.assertEqual(report.new_gaps, frozenset({"alpha"}))
            self.assertFalse(report.ok)

    def test_a_manifest_path_does_not_credit_a_crate_whose_package_shares_its_directorys_spelling(
        self,
    ):
        # `--manifest-path crates/shared` lints the crate in directory
        # `shared` (package `alpha`). It must not also credit the crate
        # whose *package name* happens to be spelled `shared` (crate
        # "other") just because that spelling matches (#439 follow-up).
        with tempfile.TemporaryDirectory() as tmp:
            root = self._collision_root(tmp)
            report = evaluate(
                root=root,
                workflow_text=_workflow(
                    "cargo clippy --manifest-path crates/shared/Cargo.toml "
                    "--all-targets -- -D warnings"
                ),
                allowlisted={},
            )
            self.assertEqual(report.linted, frozenset({"alpha"}))
            self.assertEqual(report.new_gaps, frozenset({"shared"}))
            self.assertFalse(report.ok)


class MainExitCodes(unittest.TestCase):
    """Both directions the issue asks for, end to end through `main`."""

    def _tree(self, tmp: str, *, allow: str) -> tuple[Path, Path, Path]:
        root = Path(tmp)
        _workspace(root, {"alpha": "alpha-crate", "beta": "beta-crate"})
        # 20 filler crates so the corpus clears the implausibility floor.
        filler = {f"f{i}": f"filler-{i}" for i in range(20)}
        _workspace(root, {"alpha": "alpha-crate", "beta": "beta-crate", **filler})
        workflow = root / "ci.yml"
        _write(workflow, _workflow(CLIPPY.format(d="alpha")))
        allowlist = root / "allow"
        _write(allowlist, allow)
        return root, workflow, allowlist

    def test_fails_when_a_crate_is_not_linted_and_not_allowlisted(self):
        with tempfile.TemporaryDirectory() as tmp:
            allow = "".join(f"filler-{i} = not triaged\n" for i in range(20))
            root, workflow, allowlist = self._tree(tmp, allow=allow)
            code = main(
                [
                    "--workflow", str(workflow),
                    "--root", str(root),
                    "--allowlist", str(allowlist),
                ]
            )
            self.assertEqual(code, 1)

    def test_passes_once_that_crate_is_allowlisted(self):
        with tempfile.TemporaryDirectory() as tmp:
            allow = "beta-crate = not triaged\n" + "".join(
                f"filler-{i} = not triaged\n" for i in range(20)
            )
            root, workflow, allowlist = self._tree(tmp, allow=allow)
            code = main(
                [
                    "--workflow", str(workflow),
                    "--root", str(root),
                    "--allowlist", str(allowlist),
                ]
            )
            self.assertEqual(code, 0)

    def test_a_new_crate_named_with_a_trailing_comment_surfaces_as_a_gap(self):
        """Mirrors the #439 follow-up hole: a new, unlinted, unallowlisted
        crate declared as `name = "..." # explanation` must not vanish from
        the corpus. Pre-fix, `iter_crates` dropped it silently and the other
        crates kept the corpus above the plausibility floor, so CI stayed
        green without either linting or allowlisting it.
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            filler = {f"f{i}": f"filler-{i}" for i in range(20)}
            _workspace(
                root,
                {
                    "alpha": "alpha-crate",
                    "beta": "beta-crate",
                    "probe": "probe-crate",
                    **filler,
                },
            )
            # Overwrite with the valid-TOML trailing-comment spelling the
            # plain regex used to miss.
            _write(
                root / "crates" / "probe" / "Cargo.toml",
                '[package]\nname = "probe-crate" # renamed pending #999\nversion = "0.1.0"\n',
            )
            workflow = root / "ci.yml"
            _write(workflow, _workflow(CLIPPY.format(d="alpha")))
            allowlist = root / "allow"
            allow = "beta-crate = not triaged\n" + "".join(
                f"filler-{i} = not triaged\n" for i in range(20)
            )
            _write(allowlist, allow)

            code = main(
                [
                    "--workflow", str(workflow),
                    "--root", str(root),
                    "--allowlist", str(allowlist),
                ]
            )
            self.assertEqual(code, 1)

    def test_an_implausibly_small_corpus_fails_instead_of_reporting_ok(self):
        """A broken manifest parse must not read as `no crates, no gaps, ok`."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(root / "Cargo.toml", "[workspace]\nresolver = \"2\"\n")
            workflow = root / "ci.yml"
            _write(workflow, _workflow(CLIPPY.format(d="alpha")))
            allowlist = root / "allow"
            _write(allowlist, "")
            code = main(
                [
                    "--workflow", str(workflow),
                    "--root", str(root),
                    "--allowlist", str(allowlist),
                ]
            )
            self.assertEqual(code, 1)


class WriteAllowlist(unittest.TestCase):
    """`--write-allowlist` must clear the same plausibility floor as a report.

    Pre-fix, this branch checked `--allowlist`/report-only: `--write-allowlist`
    never read the floor, so a degraded corpus was written as the new
    baseline allowlist, exit 0, silently erasing every exemption for a crate
    the parse failed to see (#439 follow-up).
    """

    def test_writes_the_unlinted_set_when_the_corpus_is_healthy(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            filler = {f"f{i}": f"filler-{i}" for i in range(20)}
            _workspace(
                root, {"alpha": "alpha-crate", "beta": "beta-crate", **filler}
            )
            workflow = root / "ci.yml"
            _write(workflow, _workflow(CLIPPY.format(d="alpha")))
            target = root / "allow"
            code = main(
                [
                    "--workflow", str(workflow),
                    "--root", str(root),
                    "--write-allowlist", str(target),
                ]
            )
            self.assertEqual(code, 0)
            written = load_allowlist(target)
            self.assertEqual(set(written), {"beta-crate", *(f"filler-{i}" for i in range(20))})

    def test_refuses_when_the_corpus_is_implausibly_small_and_writes_nothing(self):
        # No `members` list at all -- the same broken-manifest shape as
        # MainExitCodes.test_an_implausibly_small_corpus_fails_instead_of_reporting_ok.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(root / "Cargo.toml", '[workspace]\nresolver = "2"\n')
            workflow = root / "ci.yml"
            _write(workflow, _workflow(CLIPPY.format(d="alpha")))
            target = root / "allow"
            code = main(
                [
                    "--workflow", str(workflow),
                    "--root", str(root),
                    "--write-allowlist", str(target),
                ]
            )
            self.assertEqual(code, 1)
            self.assertFalse(target.exists())

    def test_refuses_without_truncating_an_existing_target(self):
        # The sharper pin: a refusal that truncates the file before erroring
        # is not a fix. The target must be byte-identical after the refusal.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _write(root / "Cargo.toml", '[workspace]\nresolver = "2"\n')
            workflow = root / "ci.yml"
            _write(workflow, _workflow(CLIPPY.format(d="alpha")))
            target = root / "allow"
            sentinel = "existing-crate = deliberately not linted, see #1\n"
            _write(target, sentinel)
            code = main(
                [
                    "--workflow", str(workflow),
                    "--root", str(root),
                    "--write-allowlist", str(target),
                ]
            )
            self.assertEqual(code, 1)
            self.assertEqual(target.read_text(encoding="utf-8"), sentinel)


class RealTree(unittest.TestCase):
    """The shipped allowlist and workflow must agree with each other."""

    def test_the_repository_is_green_and_the_corpus_is_real(self):
        code = main(
            [
                "--workflow", str(REPO / ".github/workflows/ci.yml"),
                "--root", str(REPO),
                "--allowlist", str(REPO / "tests/ci/unlinted-crates.allowlist"),
            ]
        )
        self.assertEqual(code, 0)
        self.assertGreater(len(iter_crates(REPO)), 20)

    def test_the_issues_motivating_crate_is_linted_not_allowlisted(self):
        """#439 exists because `xai-grok-workspace` was unlinted and failing.

        Allowlisting it would have made this guard's first run a lie about
        its own motivating example.
        """
        text = (REPO / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertIn("xai-grok-workspace", linted_tokens(text))
        allowlisted = load_allowlist(REPO / "tests/ci/unlinted-crates.allowlist")
        self.assertNotIn("xai-grok-workspace", allowlisted)


if __name__ == "__main__":
    unittest.main()
