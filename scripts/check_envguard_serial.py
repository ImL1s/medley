#!/usr/bin/env python3
"""Fail when an xai-grok-shell unit test mutates process env without unkeyed serial.

`EnvGuard` (and raw `std::env::{set_var, remove_var}`) is only sound when no
other thread touches the environment. The crate-wide regime is unkeyed
`#[serial_test::serial]` / `#[serial]`. A keyed slot such as
`#[serial(heap_profile_monitor)]` only serializes tests that share that key,
so it does not compose with the rest of the env-mutating suite (#319).

This first slice is a direct mention scan of `#[test]` / `#[tokio::test]`
bodies under `crates/codegen/xai-grok-shell/src/**/*.rs`. Transitive helper
callers are a later PR. Known stragglers live in
`tests/ci/envguard-serial-allowlist.txt`; new hits and stale entries both fail.

Usage:
    python3 scripts/check_envguard_serial.py
    python3 scripts/check_envguard_serial.py --dump-allowlist
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass
from pathlib import Path


DEFAULT_SCAN_ROOT = Path("crates/codegen/xai-grok-shell/src")
DEFAULT_ALLOWLIST = Path("tests/ci/envguard-serial-allowlist.txt")

RAW_STRING_START = re.compile(r'r(#+)?"')
CHAR_LITERAL = re.compile(r"'(?:\\.|[^\\'\n])'")
FN_DEF = re.compile(
    r"(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
)
TEST_ATTR = re.compile(
    r"#\s*\[\s*(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*test\b",
    re.DOTALL,
)
SERIAL_ATTR = re.compile(
    r"#\s*\[\s*(?:serial_test\s*::\s*)?serial\s*(?:\((?P<args>.*)\))?\s*\]",
    re.DOTALL,
)
ENV_MUTATION = re.compile(
    r"\b(?:std\s*::\s*)?env\s*::\s*(?:set_var|remove_var)\s*\("
)

# A test rarely calls `env::set_var` itself; it uses an RAII guard type or a
# helper. Matching those by NAME is what made this guard under-report: the
# literal `EnvGuard::` it used to look for matches neither `EnvVarGuard::` nor
# `TestEnvGuard::` (nor `LockedTestEnv::`, which is not "…Guard" at all), so 63
# env-mutating tests were invisible — 27 of them inside the one crate CI scans
# (#446). Types and helpers are now found by DEFINITION: something is an env
# mutator because its body mutates env, not because of what it is called.
TYPE_ASSOC_CALL = re.compile(
    r"\b([A-Za-z_][A-Za-z0-9_]*)\s*::\s*[A-Za-z_][A-Za-z0-9_]*\s*(?:::\s*<[^>]*>\s*)?\("
)
FREE_CALL = re.compile(r"(?<![:.\w])([a-z_][a-z0-9_]*)\s*\(")
# Name fallback for a guard whose definition this run never read. Kept
# deliberately narrow (an "…Env…Guard" type is an env guard and little else)
# and note the leading `*`, not `+`: requiring one character before `Env` is
# the bug that hid `EnvVarGuard` from the old matcher in the first place.
ENV_GUARD_NAME = re.compile(r"\b([A-Za-z0-9_]*Env[A-Za-z0-9_]*Guard)\s*::")
# First argument of an env mutation: the variable being touched. Either a
# string literal or a SCREAMING_CASE const naming it.
ENV_VAR_ARG = re.compile(
    r"(?:env\s*::\s*(?:set_var|remove_var)|[A-Za-z_][A-Za-z0-9_]*\s*::\s*(?:set|unset|remove|isolate))\s*\(\s*"
    r"(?:\"(?P<lit>[^\"]*)\"|(?P<konst>[A-Z][A-Z0-9_]{2,}))"
)
IMPL_HEAD = re.compile(r"\bimpl\b[^{;]*\{")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")

# Serialisation regimes other than unkeyed `#[serial]`. A crate-wide `Mutex`
# held for the test's lifetime is exactly as sound (#319 names `serial` only
# because that is what xai-grok-shell happened to use); `xai-grok-workspace`
# uses `ENV_TEST_LOCK` + `LockedTestEnv` and is not thereby unprotected.
LOCK_ACQUIRE = re.compile(
    r"\.\s*lock\s*\(|::\s*lock\s*\(|\bMutexGuard\b|\b[A-Z][A-Z0-9_]*_LOCK\b"
)


@dataclass(frozen=True)
class Finding:
    path: Path
    line: int
    name: str
    reason: str

    @property
    def allowlist_id(self) -> str:
        return f"{_posix(self.path)}::{self.name}"


def _posix(path: Path) -> str:
    return path.as_posix()


def _skip_quoted(source: str, index: int, quote: str) -> int:
    index += 1
    while index < len(source):
        if source[index] == "\\":
            index += 2
        elif source[index] == quote:
            return index + 1
        else:
            index += 1
    return len(source)


def _skip_char_literal(source: str, index: int) -> int | None:
    match = CHAR_LITERAL.match(source, index)
    return match.end() if match else None


def _skip_raw_string(source: str, index: int) -> int | None:
    if source[index : index + 1] != "r":
        return None
    match = RAW_STRING_START.match(source, index)
    if not match:
        return None
    hashes = match.group(1) or ""
    end = source.find('"' + hashes, match.end())
    return len(source) if end < 0 else end + 1 + len(hashes)


def _skip_comment(source: str, index: int) -> int | None:
    if source[index : index + 1] != "/":
        return None
    if source.startswith("//", index):
        end = source.find("\n", index + 2)
        return len(source) if end < 0 else end
    if source.startswith("/*", index):
        depth = 1
        cursor = index + 2
        while cursor < len(source) and depth:
            if source.startswith("/*", cursor):
                depth += 1
                cursor += 2
            elif source.startswith("*/", cursor):
                depth -= 1
                cursor += 2
            else:
                cursor += 1
        return cursor
    return None


def _balanced_end(source: str, open_index: int) -> int:
    pairs = {"(": ")", "{": "}", "[": "]"}
    opener = source[open_index]
    if opener not in pairs:
        return open_index
    stack = [pairs[opener]]
    index = open_index + 1
    while index < len(source) and stack:
        comment_end = _skip_comment(source, index)
        if comment_end is not None:
            index = comment_end
            continue
        raw_end = _skip_raw_string(source, index)
        if raw_end is not None:
            index = raw_end
            continue
        char = source[index]
        if char == '"':
            index = _skip_quoted(source, index, char)
        elif char == "'" and (char_end := _skip_char_literal(source, index)) is not None:
            index = char_end
        elif char in pairs:
            stack.append(pairs[char])
            index += 1
        elif char == stack[-1]:
            stack.pop()
            index += 1
        else:
            index += 1
    return index


def _code_only(source: str) -> str:
    """Mask comments and literals while preserving offsets and newlines."""

    result = list(source)
    index = 0
    while index < len(source):
        end = _skip_comment(source, index)
        if end is None:
            end = _skip_raw_string(source, index)
        if end is None and source[index] == '"':
            end = _skip_quoted(source, index, source[index])
        if end is None and source[index] == "'":
            end = _skip_char_literal(source, index)
        if end is None:
            index += 1
            continue
        for masked in range(index, end):
            if result[masked] != "\n":
                result[masked] = " "
        index = end
    return "".join(result)


def _line(source: str, offset: int) -> int:
    return source.count("\n", 0, offset) + 1


def _preceding_attributes(source: str, code: str, position: int) -> list[str]:
    """Collect `#attr` blocks immediately before ``position``.

    Walk the literal-masked view so doc comments between `#[test]` and `fn`
    do not hide the attributes (the same gap `check_new_tests_are_filtered`
    has to skip).
    """

    attrs: list[str] = []
    index = position
    while index > 0:
        index -= 1
        if code[index].isspace():
            continue
        if code[index] != "]":
            break
        end = index + 1
        depth = 1
        cursor = index - 1
        while cursor >= 0 and depth:
            if code[cursor] == "]":
                depth += 1
                cursor -= 1
            elif code[cursor] == "[":
                depth -= 1
                cursor -= 1
            else:
                cursor -= 1
        hash_index = cursor
        while hash_index >= 0 and code[hash_index].isspace():
            hash_index -= 1
        if hash_index < 0 or code[hash_index] != "#":
            break
        attrs.append(source[hash_index:end])
        index = hash_index
    attrs.reverse()
    return attrs


def _is_test_attr(attr: str) -> bool:
    return TEST_ATTR.match(attr) is not None


def _serial_kind(attr: str) -> str | None:
    match = SERIAL_ATTR.fullmatch(attr.strip())
    if match is None:
        return None
    args = (match.group("args") or "").strip()
    return "unkeyed" if not args else "keyed"


def _fn_body(source: str, name_end: int) -> tuple[int, int] | None:
    index = name_end
    while index < len(source) and source[index].isspace():
        index += 1
    if index < len(source) and source[index] == "<":
        index = _balanced_end(source, index)
        while index < len(source) and source[index].isspace():
            index += 1
    if index >= len(source) or source[index] != "(":
        return None
    index = _balanced_end(source, index)
    while index < len(source):
        if source[index].isspace():
            index += 1
            continue
        if source[index] == "{":
            return index, _balanced_end(source, index)
        if source[index] == ";":
            return None
        index += 1
    return None


@dataclass(frozen=True)
class EnvMutators:
    """Types and helpers that mutate process env, and whether they self-lock.

    ``types`` is repo-wide: a guard type is imported across crates by name
    (``xai-grok-shell-base`` re-exports ``xai_grok_env::EnvVarGuard``), so
    resolving it per file would miss the majority. ``funcs`` is per-file: a
    bare helper name is far more likely to collide across crates than a type
    name, and one same-file hop is what the callers actually use.
    """

    types: dict[str, bool]
    funcs: dict[str, bool]

    def self_locks(self, name: str) -> bool:
        return bool(self.types.get(name) or self.funcs.get(name))


def _impl_blocks(code: str) -> list[tuple[str, int, int]]:
    """`(type name, start, end)` for each `impl … { … }`, block-scoped.

    Scoped with [`_balanced_end`] rather than a fixed window: a window that
    overruns the block reads the NEXT item's lock and calls the type
    self-locking when it is not — a silent pass, the failure mode this guard
    exists to prevent.
    """

    blocks: list[tuple[str, int, int]] = []
    for match in IMPL_HEAD.finditer(code):
        idents = IDENT.findall(match.group(0))
        if not idents:
            continue
        open_index = match.end() - 1
        blocks.append((idents[-1], open_index, _balanced_end(code, open_index)))
    return blocks


def index_env_mutators(sources: list[tuple[Path, str]]) -> EnvMutators:
    """Index RAII env-guard types, by shape rather than by name.

    Deliberately narrow. "Any type whose impl mentions `env::set_var`" is far
    too broad — `Config::load` reads config that sets env, and every test
    calling `Config::anything()` would be flagged; measured, that verdict was
    2505 violations in `xai-grok-shell` alone, all noise. A false red is worse
    than a miss for a CI gate, so a type qualifies only if it is shaped like an
    env guard:

    * it has `impl Drop for T` AND one of its impls mutates env, or
    * it STORES a known guard (or a `MutexGuard`) and its impl constructs one
      — the wrapper case (`LockedTestEnv` holds `Vec<TestEnvGuard>` plus the
      lock and delegates to `TestEnvGuard::set`, never naming `env::set_var`).
    """

    parsed: list[tuple[str, list[tuple[str, int, int]]]] = []
    for _rel, source in sources:
        code = _code_only(source)
        parsed.append((code, _impl_blocks(code)))

    has_drop: set[str] = set()
    mutating: dict[str, bool] = {}
    struct_bodies: dict[str, str] = {}
    for code, blocks in parsed:
        for match in re.finditer(r"\bimpl\b[^{;]*?\bDrop\b[^{;]*?\bfor\b([^{;]*)\{", code):
            idents = IDENT.findall(match.group(1))
            if idents:
                has_drop.add(idents[-1])
        for match in re.finditer(r"\bstruct\s+([A-Za-z_][A-Za-z0-9_]*)[^\n{;]*\{", code):
            open_index = match.end() - 1
            struct_bodies[match.group(1)] = code[open_index : _balanced_end(code, open_index)]
        for name, start, end in blocks:
            block = code[start:end]
            if ENV_MUTATION.search(block):
                mutating[name] = mutating.get(name, False) or bool(LOCK_ACQUIRE.search(block))

    types = {n: locks for n, locks in mutating.items() if n in has_drop}

    # One hop for a wrapper that owns a guard instead of touching env itself.
    for _ in range(3):
        grew = False
        for code, blocks in parsed:
            for name, start, end in blocks:
                if name in types:
                    continue
                block = code[start:end]
                if not any(used in types for used in TYPE_ASSOC_CALL.findall(block)):
                    continue
                fields = struct_bodies.get(name, "")
                owns = "MutexGuard" in fields or any(t in fields for t in types)
                if not owns:
                    continue
                types[name] = bool(LOCK_ACQUIRE.search(block))
                grew = True
        if not grew:
            break
    return EnvMutators(types=types, funcs={})


def _file_helpers(source: str, code: str, types: dict[str, bool]) -> dict[str, bool]:
    """Same-file non-test fns that mutate env, and whether they self-lock."""

    impl_spans = [(s, e) for _n, s, e in _impl_blocks(code)]
    helpers: dict[str, bool] = {}
    for match in FN_DEF.finditer(code):
        if any(start <= match.start() < end for start, end in impl_spans):
            continue  # an inherent method, reached through its type instead
        if any(_is_test_attr(a) for a in _preceding_attributes(source, code, match.start())):
            continue
        body_range = _fn_body(source, match.end())
        if body_range is None:
            continue
        body = code[body_range[0] : body_range[1]]
        uses_guard = any(used in types for used in TYPE_ASSOC_CALL.findall(body))
        if not ENV_MUTATION.search(body) and not uses_guard:
            continue
        name = match.group("name")
        helpers[name] = helpers.get(name, False) or bool(LOCK_ACQUIRE.search(body))
    return helpers


def _env_mutation_reason(
    body: str, mutators: EnvMutators, helpers: dict[str, bool]
) -> tuple[str, list[str]] | None:
    """`(reason, names whose own soundness can vouch for this test)`."""

    used = [name for name in TYPE_ASSOC_CALL.findall(body) if name in mutators.types]
    called = [name for name in FREE_CALL.findall(body) if name in helpers]
    named = ENV_GUARD_NAME.findall(body)
    if ENV_MUTATION.search(body):
        return "std::env::{set_var,remove_var}", used
    if used:
        return f"{used[0]}:: (env-mutating guard type)", used
    if named:
        return f"{named[0]}::", named
    if called:
        return f"{called[0]}() (env-mutating helper)", called
    return None


@dataclass(frozen=True)
class Candidate:
    """A test that mutates process env, with what it needs to be judged."""

    path: Path
    line: int
    name: str
    mention: str
    keyed: tuple[str, ...]
    variables: tuple[str, ...]
    sound: bool


def _is_integration_target(path: Path) -> bool:
    """`crates/<group>/<crate>/tests/<file>.rs` — its own test binary.

    Only a file directly under the crate's `tests/` dir is a target; a
    `tests/` MODULE under `src/` (`src/app/dispatch/tests/cta_e2e.rs`) shares
    the lib's process and must not be mistaken for one.
    """

    parts = path.parts
    return "src" not in parts and len(parts) >= 2 and parts[-2] == "tests"


def _env_variables(raw_body: str) -> set[str]:
    """Names of the env vars a test mutates.

    Read from the RAW body: `_code_only` masks string literals, which is
    where the names are. Bare SCREAMING_CASE consts count too — a crate that
    names its key (`ENV_AUTO_COMPACT_THRESHOLD_PERCENT`) is the common case.
    """

    found: set[str] = set()
    for match in ENV_VAR_ARG.finditer(raw_body):
        found.add(match.group("lit") or match.group("konst"))
    return {v for v in found if v}


def analyze_source(
    source: str,
    *,
    relpath: Path | None = None,
    mutators: EnvMutators | None = None,
) -> list[Candidate]:
    """Every env-mutating test in ``source``, pre-judged for the local regimes."""

    path = relpath or Path("<input>")
    code = _code_only(source)
    if mutators is None:
        mutators = index_env_mutators([(path, source)])
    helpers = _file_helpers(source, code, mutators.types)
    test_count = 0
    raw: list[tuple[re.Match[str], list[str], tuple[int, int]]] = []
    for match in FN_DEF.finditer(code):
        attrs = _preceding_attributes(source, code, match.start())
        if not any(_is_test_attr(attr) for attr in attrs):
            continue
        body_range = _fn_body(source, match.end())
        if body_range is None:
            continue
        test_count += 1
        raw.append((match, attrs, body_range))

    out: list[Candidate] = []
    for match, attrs, body_range in raw:
        body = code[body_range[0] : body_range[1]]
        found = _env_mutation_reason(body, mutators, helpers)
        if found is None:
            continue
        mention, vouchers = found
        kinds = [kind for attr in attrs if (kind := _serial_kind(attr))]
        sound = "unkeyed" in kinds
        # A crate-wide lock held for the test's lifetime is the same guarantee
        # as unkeyed `#[serial]`, whether the test takes it itself, the guard
        # type takes it in its constructor, or a same-file helper does.
        sound = sound or bool(LOCK_ACQUIRE.search(body))
        sound = sound or any(
            mutators.self_locks(name) or helpers.get(name) for name in vouchers
        )
        # Sole test in its own integration binary: nothing shares its process,
        # so there is no sibling for it to corrupt. A regime, not an exemption.
        sound = sound or (_is_integration_target(path) and test_count == 1)
        keys = tuple(
            (m.group("args") or "").strip()
            for attr in attrs
            if (m := SERIAL_ATTR.fullmatch(attr.strip())) and (m.group("args") or "").strip()
        )
        out.append(
            Candidate(
                path=path,
                line=_line(source, match.start()),
                name=match.group("name"),
                mention=mention,
                keyed=keys,
                variables=tuple(sorted(_env_variables(source[body_range[0] : body_range[1]]))),
                sound=sound,
            )
        )
    return out


def key_map(candidates: list[Candidate]) -> dict[str, set[str]]:
    """variable -> the set of serial keys under which it is mutated.

    `<unkeyed>` marks a mutation with no key at all. A variable appearing
    under two entries is one no keyed `#[serial]` can protect.
    """

    mapping: dict[str, set[str]] = {}
    for cand in candidates:
        if cand.sound:
            continue
        label = set(cand.keyed) or {"<unkeyed>"}
        for var in cand.variables:
            mapping.setdefault(var, set()).update(label)
    return mapping


def judge(candidates: list[Candidate], keys: dict[str, set[str]]) -> list[Finding]:
    """Apply the key-consistency regime and emit findings.

    "Is a keyed `#[serial]` sufficient?" is not a global verdict, it is a
    property: a keyed serial is sound exactly when every test mutating a given
    variable agrees on one key for it. `xai-grok-sandbox`'s tests all share
    `bwrap_env` and are sound; `#[serial(GROK_HOME)]` and `#[serial(HOME)]`
    both perturb home resolution under different keys and are not.
    """

    findings: list[Finding] = []
    for cand in candidates:
        if cand.sound:
            continue
        if cand.keyed:
            clashes = sorted(
                var for var in cand.variables if len(keys.get(var, set())) > 1
            )
            if not clashes:
                continue
            reason = (
                f"{cand.mention} with keyed #[serial({cand.keyed[0]})], but "
                f"{clashes[0]} is also mutated under "
                f"{sorted(keys[clashes[0]] - {cand.keyed[0]})[0]}"
            )
        else:
            reason = f"{cand.mention} without unkeyed #[serial_test::serial]"
        findings.append(
            Finding(path=cand.path, line=cand.line, name=cand.name, reason=reason)
        )
    return findings


def scan_source(
    source: str,
    *,
    relpath: Path | None = None,
    mutators: EnvMutators | None = None,
) -> list[Finding]:
    """Findings for one source, judged against its own key usage."""

    candidates = analyze_source(source, relpath=relpath, mutators=mutators)
    return judge(candidates, key_map(candidates))


def rust_files(scan_root: Path) -> list[Path]:
    return sorted(path for path in scan_root.rglob("*.rs") if path.is_file())


def scan_tree(scan_root: Path, *, repo: Path, index_root: Path | None = None) -> list[Finding]:
    """Scan ``scan_root``, resolving guard types against ``index_root``.

    The index is built over the whole repo by default even when the scan is
    narrower: `xai-grok-shell` uses `EnvVarGuard` re-exported from
    `xai_grok_env`, so an index limited to the scanned files cannot tell
    whether that guard locks.
    """

    index_dir = index_root or (repo / "crates")
    if not index_dir.is_dir():
        index_dir = scan_root
    mutators = index_env_mutators(
        [(p.relative_to(repo), p.read_text(encoding="utf-8")) for p in rust_files(index_dir)]
    )
    # Two passes: key consistency is a property of the whole scanned set, not
    # of one file — a variable is only safely keyed if EVERY test touching it
    # agrees on the key, and the disagreeing test is usually elsewhere.
    candidates: list[Candidate] = []
    for path in rust_files(scan_root):
        rel = path.relative_to(repo)
        text = path.read_text(encoding="utf-8")
        candidates.extend(analyze_source(text, relpath=rel, mutators=mutators))
    return judge(candidates, key_map(candidates))


def load_allowlist(path: Path) -> list[str]:
    if not path.is_file():
        return []
    entries: list[str] = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        entries.append(line)
    return entries


def _entry_path(entry: str) -> str:
    return entry.rsplit("::", 1)[0]


def evaluate(
    findings: list[Finding],
    allowlist: list[str],
    *,
    scan_rel: str | None = None,
) -> tuple[list[Finding], list[str], list[str]]:
    """Split the allowlist into new / stale / outside-the-scan-root.

    "Stale" used to mean "no matching finding", which lumped together two
    unrelated things: an entry whose test was fixed (remove it) and an entry
    naming a file this run never read (nothing is known about it). The second
    is not evidence of anything, and reporting it as stale sent the reader to
    the wrong fix — that mis-diagnosis is half of #446.
    """

    allowed = set(allowlist)
    new = [item for item in findings if item.allowlist_id not in allowed]
    present = {item.allowlist_id for item in findings}
    unmatched = [entry for entry in allowlist if entry not in present]
    if scan_rel is None:
        return new, unmatched, []
    prefix = scan_rel.rstrip("/") + "/"
    outside = [e for e in unmatched if not _entry_path(e).startswith(prefix)]
    stale = [e for e in unmatched if _entry_path(e).startswith(prefix)]
    return new, stale, outside


def format_report(
    new: list[Finding],
    stale: list[str],
    *,
    finding_count: int,
    allowlist_count: int,
    outside: list[str] | None = None,
    scan_rel: str | None = None,
) -> str:
    outside = outside or []
    scope = f" [scanned: {scan_rel}]" if scan_rel else ""
    lines = [
        f"envguard-serial{scope}: {finding_count} violation(s), "
        f"{allowlist_count} allowlist entries, "
        f"{len(new)} new, {len(stale)} stale, {len(outside)} outside the scan root",
    ]
    if new:
        lines.append("")
        lines.append("New EnvGuard / env-mutation tests missing unkeyed #[serial]:")
        for item in new:
            lines.append(f"  {item.path}:{item.line}: {item.name}: {item.reason}")
        lines.append("")
        lines.append(
            "Add unkeyed `#[serial_test::serial]` (or `#[serial]`) on the test, "
            "or — only for a known first-slice straggler — an allowlist id "
            "`<relpath>::<fn>` in tests/ci/envguard-serial-allowlist.txt."
        )
    if stale:
        lines.append("")
        lines.append("Stale allowlist entries (no longer violations; remove them):")
        for entry in stale:
            lines.append(f"  {entry}")
    if outside:
        lines.append("")
        lines.append(
            "Allowlist entries outside the scan root — this run read nothing "
            "about them, so they are NOT stale. Widen --scan-root to judge "
            "them, or drop them if the scope is deliberate:"
        )
        for entry in outside:
            lines.append(f"  {entry}")
    return "\n".join(lines)


def dump_allowlist(findings: list[Finding]) -> str:
    lines = [
        "# EnvGuard / std::env::{set_var,remove_var} tests that lack unkeyed",
        "# #[serial_test::serial]. First-slice bootstrap for issue #319.",
        "# Remove an entry when the test is annotated. Stale entries fail CI.",
        "",
    ]
    for ident in sorted({item.allowlist_id for item in findings}):
        lines.append(ident)
    if lines[-1] != "":
        lines.append("")
    return "\n".join(lines)


def _repo_from_script() -> Path:
    return Path(__file__).resolve().parents[1]


def main(argv: list[str] | None = None) -> int:
    repo = _repo_from_script()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--repo",
        type=Path,
        default=repo,
        help="repository root (default: parent of scripts/)",
    )
    ap.add_argument(
        "--scan-root",
        type=Path,
        default=None,
        help="directory of .rs files to scan (default: xai-grok-shell/src)",
    )
    ap.add_argument(
        "--allowlist",
        type=Path,
        default=None,
        help="allowlist file (default: tests/ci/envguard-serial-allowlist.txt)",
    )
    ap.add_argument(
        "--dump-allowlist",
        action="store_true",
        help="print a replacement allowlist for the current tree and exit 0",
    )
    args = ap.parse_args(argv)

    root = args.repo.resolve()
    scan_root = (args.scan_root or (root / DEFAULT_SCAN_ROOT)).resolve()
    allowlist_path = args.allowlist or (root / DEFAULT_ALLOWLIST)
    findings = scan_tree(scan_root, repo=root)

    if args.dump_allowlist:
        sys.stdout.write(dump_allowlist(findings))
        return 0

    try:
        scan_rel = scan_root.relative_to(root).as_posix()
    except ValueError:
        scan_rel = None
    allowlist = load_allowlist(allowlist_path)
    new, stale, outside = evaluate(findings, allowlist, scan_rel=scan_rel)
    report = format_report(
        new,
        stale,
        finding_count=len(findings),
        allowlist_count=len(allowlist),
        outside=outside,
        scan_rel=scan_rel,
    )
    print(report)
    # `outside` is deliberately not a failure: this run read nothing about
    # those files, so it has no basis to call them anything.
    if new or stale:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
