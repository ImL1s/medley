#!/usr/bin/env python3
"""Fail when an xai-grok-shell unit test mutates process env without unkeyed serial.

`EnvGuard` (and raw `std::env::{set_var, remove_var}`) is only sound when no
other thread touches the environment. The crate-wide regime is unkeyed
`#[serial_test::serial]` / `#[serial]`. A keyed slot such as
`#[serial(heap_profile_monitor)]` only serializes tests that share that key,
so it does not compose with the rest of the env-mutating suite (#319).

Guards are found by DEFINITION, not by name: a type or helper is an env
mutator because its own body mutates env. Matching the literal `EnvGuard::`
saw neither `EnvVarGuard::` nor `TestEnvGuard::` and was silent about 170
env-mutating tests in this very crate (#446).

A finding is cleared by any of five regimes: unkeyed `#[serial]`; an env lock
whose guard is bound and alive across every mutation; a guard type or helper
that owns the lock itself; the sole test in its own integration binary; or a
keyed `#[serial(k)]` whose variable is consistently keyed across the binary.

WHAT THIS DOES NOT CHECK, so that a green tick is not read as more than it is:

    Two tests can each be sound under a DIFFERENT regime and still race each
    other. Unkeyed `#[serial]` and a crate's own env mutex are unrelated locks,
    so a test holding one has no exclusion against a test holding the other.
    This checker judges each test against its own regime and does not compare
    regimes across tests. Measured at the time of writing: 68 tests in this
    crate mutate a variable that another test mutates under an incompatible
    regime (25 unkeyed-serial, 43 lock-covered) -- `util/config/hints.rs` is
    the clearest pair, where some tests take `CONTEXTUAL_HINTS_ENV_LOCK` and
    others use `#[serial]`, both touching `ENV_CONTEXTUAL_HINTS`.
    Tracked in #459. Until it closes, a pass here means "each test is
    serialised against its own regime", not "no two env-mutating tests race".

    A qualified helper call is resolved ONE hop, and only in the shape
    `crate::a::b::fn()` reached through a SAME-FILE wrapper (`_file_helpers`'
    inclusion check, #449 review, Finding 1) -- the shape the real tree has
    (`session/worktree.rs`'s `init_git_repo` calling
    `crate::test_support::ensure_hermetic_git_on_path`). Two gaps remain
    un-widened because this tree currently has no member of either: a TEST
    calling a `crate::`-qualified mutator directly, with no same-file wrapper
    in between, is not resolved (`_env_mutation_reason` and `_mutation_sites`
    still read only `FREE_CALL`, i.e. unqualified names); and a chain longer
    than one hop (the wrapper's OWN target delegating again, through a THIRD
    file) is not followed.

    An imported or aliased mutator -- `use std::env::set_var;` followed by a
    bare `set_var(...)`, or `use std::env as x;` followed by `x::set_var(...)`
    -- is not recognised; `ENV_MUTATION` only matches `env::set_var` /
    `std::env::set_var` written out at the call site (#449 review, Finding 2).
    Measured at the time of writing: separate greps in the scan root for
    `use std::env::set_var`, `use std::env::remove_var`, a braced
    `use std::env::{set_var, ...}`, `use std::env as`, and a bare `set_var(` /
    `remove_var(` not preceded by `env::` all return zero matches, so this is
    a real, currently-empty class rather than a live hole. A future `use` of
    either shape would pass unseen until this regex is widened to know the
    file's aliases.

Scan scope is one crate: `crates/codegen/xai-grok-shell/src/**/*.rs` unless
`--scan-root` says otherwise. Known stragglers live in
`tests/ci/envguard-serial-allowlist.txt`; new hits and stale entries both fail,
while an entry naming a file outside the scan root is reported as exactly that
rather than as stale.

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
# `crate::a::b::fn_name(` — the one shape `FREE_CALL` structurally cannot
# match (its lookbehind excludes anything after `::`) and `TYPE_ASSOC_CALL`
# mis-splits (it captures the segment before the LAST `::`, treating a module
# as if it were a type). A test delegating through such a call to a helper
# defined in ANOTHER file produced no candidate at all: `_file_helpers` only
# ever reads the current file, so `session/worktree.rs` calling
# `crate::test_support::ensure_hermetic_git_on_path()` (which mutates `PATH`
# and `GIT_EXEC_PATH`) was invisible on both ends (#449 review, Finding 1).
# Deliberately `crate::`-only: that is the one prefix this checker can resolve
# without following `use` imports, and it is exactly the shape the finding
# named.
CRATE_QUALIFIED_CALL = re.compile(
    r"\bcrate\s*::\s*((?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)+)([a-z_][a-z0-9_]*)\s*\("
)


def _mod_path_of(segments: str) -> tuple[str, ...]:
    """`"test_support::"` (a `CRATE_QUALIFIED_CALL` module-path capture,
    trailing `::` included) -> `("test_support",)`."""

    return tuple(s.strip() for s in segments.split("::") if s.strip())


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
MOD_DECL = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*[;{]", re.M)
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")

# Serialisation regimes other than unkeyed `#[serial]`. A crate-wide `Mutex`
# held for the test's lifetime is exactly as sound (#319 names `serial` only
# because that is what xai-grok-shell happened to use); `xai-grok-workspace`
# uses `ENV_TEST_LOCK` + `LockedTestEnv` and is not thereby unprotected.
# Any `.lock(` used to count, so a test touching an UNRELATED mutex read as
# serialised against env mutation (#449 review). The lock has to be the one
# that serialises env, and its guard has to still be alive.
# Note the leading `*`, not `+`. Requiring one character before `ENV` is the
# same bug that hid `EnvVarGuard` from the old matcher, and it hid
# `ENV_TEST_LOCK` from this one — third instance of one mistake.
ENV_LOCK_NAME = r"(?:[A-Z0-9_]*ENV[A-Z0-9_]*LOCK|LockedTestEnv|[a-z0-9_]*env_lock)"
ENV_LOCK_ACQUIRE = re.compile(ENV_LOCK_NAME)
# A bound RHS owns a lock only if it ACQUIRES one. Searching for the identifier
# made `let cfg = read_env_lock_setting();` a lock owner and protected every
# mutation after it (#449 review).
ENV_LOCK_VALUE = re.compile(
    r"(?:[A-Z0-9_]*ENV[A-Z0-9_]*LOCK|LockedTestEnv)\s*(?:\.|::)\s*lock\s*\("
    r"|\b[a-z0-9_]*env_lock\s*\("
)
LOCK_CALL = re.compile(r"\.\s*lock\s*\(|::\s*lock\s*\(")
REEXPORT = re.compile(
    r"pub\s+use\s+(?P<src>[a-z_][a-z0-9_]*)\s*::(?:[A-Za-z0-9_]+\s*::\s*)*"
    r"(?P<item>[A-Za-z_][A-Za-z0-9_]*)\s*;"
)
# Where a name is brought in from: `use a::b::Name;` or an inline `a::b::Name::`.
USE_PATH = re.compile(
    r"use\s+(?P<path>(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)+)(?P<item>[A-Za-z_][A-Za-z0-9_]*)\s*;"
)
INLINE_PATH = re.compile(
    r"(?P<path>(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)+)(?P<item>[A-Za-z_][A-Za-z0-9_]*Guard)\s*::"
)


def _crate_from_segment(segment: str) -> str:
    return segment.replace("_", "-")


def _name_paths(code: str) -> dict[str, tuple[str, ...]]:
    """Name -> the module path it is referred to by, in this file."""

    paths: dict[str, tuple[str, ...]] = {}
    for pattern in (USE_PATH, INLINE_PATH):
        for match in pattern.finditer(code):
            segs = tuple(
                s.strip() for s in match.group("path").split("::") if s.strip()
            )
            paths.setdefault(match.group("item"), segs + (match.group("item"),))
    return paths
# Binding an expression that merely CONTAINS an env-lock-shaped identifier is
# not acquiring a lock: `let cfg = read_env_lock_setting();` matched, and every
# later mutation in that helper then read as serialised. `_protected_spans` was
# given `ENV_LOCK_VALUE` for this and the helper path was not — the fourth time
# in this file a property was fixed in one path and left in its sibling
# (#449 review).
ENV_LOCK_ACQUIRING_BINDING = re.compile(
    r"let\s+(?P<bind>_\b|[A-Za-z_][A-Za-z0-9_]*)\s*(?::[^=;]*)?=\s*[^;]*?"
    r"(?:(?:[A-Z0-9_]*ENV[A-Z0-9_]*LOCK|LockedTestEnv)\s*(?:\.|::)\s*lock\s*\("
    r"|\b[a-z0-9_]*env_lock\s*\()"
    r"[^;]*;"
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
    by_crate: dict[tuple[str, str], bool]
    funcs: dict[str, bool]
    type_vars: dict[str, frozenset[str]]
    module_alias: dict[tuple[str, str], str]
    name_alias: dict[tuple[str, str], str]

    def resolve_crate(self, name: str, crate: str, path: tuple[str, ...]) -> str:
        """Follow `pub use` re-exports to the crate that DEFINES ``name``.

        `crate::env::EnvVarGuard` in `xai-grok-shell` is
        `pub use xai_grok_shell_base::env`, whose `env.rs` says
        `pub use xai_grok_env::EnvVarGuard` — three crates from the use site to
        the definition, and only the last one knows whether it locks.
        """

        current = crate
        for module in path[:-1]:
            if module in ("crate", "self", "super"):
                continue
            current = self.module_alias.get((current, module), current)
        for _ in range(3):
            nxt = self.name_alias.get((current, name))
            if nxt is None or nxt == current:
                break
            current = nxt
        return current

    def self_locks(
        self,
        name: str,
        crate: str | None = None,
        file: str | None = None,
        path: tuple[str, ...] = (),
    ) -> bool:
        """Does the guard named ``name``, as seen from ``crate``, self-lock?

        Seven distinct types are called `EnvVarGuard` in this repo and they do
        NOT agree: `xai-grok-env`'s holds a lock, `xai-grok-pager`'s
        `test_util.rs` does not. Merging them by name with `or` made every use
        of the pager one read as self-locking and suppressed real findings
        (#449 review).

        Resolution prefers the definition in the using crate, and otherwise
        requires ALL definitions of that name to lock — conservative, because
        the failure direction of a guess here is a silent pass.
        """

        # File first (a guard declared inside one test module answers only for
        # that file), then the crate the import actually points at, then this
        # crate, then the conservative global AND.
        if file is not None and (file, name) in self.by_crate:
            return self.by_crate[(file, name)]
        if crate is not None and path:
            target = self.resolve_crate(name, crate, path)
            if (target, name) in self.by_crate:
                return self.by_crate[(target, name)]
        if crate is not None and (crate, name) in self.by_crate:
            return self.by_crate[(crate, name)]
        if name in self.types:
            return self.types[name]
        return bool(self.funcs.get(name))


def _env_lock_is_live(body: str) -> bool:
    """An env lock is acquired BEFORE the first mutation and still held at it.

    Used to classify HELPERS. Test bodies go through [`_protected_spans`],
    which is positional per mutation site; this is the same property for a
    helper, whose mutations are all its own.

    `let _ = ENV_LOCK.lock()` drops at the end of that statement, an explicit
    `drop(guard)` releases it early, and a lock taken AFTER the mutation never
    covered it — all three read as "serialised" if you only look for the token.
    """

    mutations = [m.start() for m in ENV_MUTATION.finditer(body)]
    if not mutations:
        return bool(ENV_LOCK_ACQUIRING_BINDING.search(body))
    first, last = mutations[0], mutations[-1]
    for match in ENV_LOCK_ACQUIRING_BINDING.finditer(body):
        name = match.group("bind")
        if name == "_":
            continue
        if match.start() > first:
            continue  # taken after a mutation it therefore never covered
        released = re.search(r"\bdrop\s*\(\s*" + re.escape(name) + r"\s*\)", body)
        if released and released.start() <= last:
            continue  # let go before the last mutation
        # Rust drops it at the enclosing block too. Scope tracking was added to
        # `_protected_spans` and not here, so a helper locking inside a nested
        # block and mutating again after it read as self-locking (#449 review).
        if _enclosing_block_end(body, match.end()) <= last:
            continue
        return True
    return False


def _crate_of(path: Path) -> str:
    parts = path.parts
    return parts[2] if len(parts) > 2 else str(path)


def _module_path(rel: Path) -> tuple[str, ...] | None:
    """`crate::a::b` module path for a file at `.../src/a/b.rs` or
    `.../src/a/b/mod.rs` — the layout convention this repo uses throughout
    (`test_support/mod.rs`, `session/worktree.rs`, ...).

    A file whose module lives somewhere else via `#[path = "..."]` resolves
    wrong here; there is no cheaper way to know that without a real parser,
    and the checker has no such case in its own scan root today (grep finds
    none). `None` for a file with no `src` component at all.
    """

    parts = rel.parts
    if "src" not in parts:
        return None
    segs = list(parts[parts.index("src") + 1 :])
    if not segs:
        return None
    segs[-1] = Path(segs[-1]).stem
    if segs[-1] in ("mod", "lib", "main"):
        segs = segs[:-1]
    return tuple(segs)


def _index_qualified_helpers(
    files: list[tuple[Path, str, str]], types: dict[str, bool]
) -> dict[tuple[str, tuple[str, ...]], frozenset[str]]:
    """`(crate, module path) -> {fn names that mutate env in that file}`.

    One hop only: built from each file's OWN `_file_helpers` pass (no
    `qualified` argument, so it only sees that file's local mutations), then
    used to resolve a `crate::a::b::fn()` call FROM another file. A helper
    that itself delegates cross-file to a THIRD file is not covered — that
    would need a second pass — and is left as a known gap (see the module
    docstring's "WHAT THIS DOES NOT CHECK").
    """

    out: dict[tuple[str, tuple[str, ...]], frozenset[str]] = {}
    for rel, source, code in files:
        module = _module_path(rel)
        if module is None:
            continue
        helpers, _vars, _returns = _file_helpers(source, code, types)
        if not helpers:
            continue
        key = (_crate_of(rel), module)
        out[key] = out.get(key, frozenset()) | frozenset(helpers)
    return out


def _process_group(path: Path) -> str:
    """Which test BINARY this file's tests run in.

    Key consistency only means something inside one process. Tests in two
    crates never share one, so comparing their keys invents clashes that
    cannot happen.
    """

    parts = path.parts
    if "tests" in parts and "src" not in parts:
        # `tests/root.rs` and everything under `tests/root/` compile into ONE
        # binary. Keying the root and its included modules separately split a
        # single process into several groups, so key clashes between them were
        # never compared (#449 review). Group by the first path component under
        # `tests/`, which is the root's stem either way.
        index = parts.index("tests")
        crate = "/".join(parts[:index])
        if index + 1 < len(parts):
            root = Path(parts[index + 1]).stem
            return f"bin:{crate}/tests/{root}"
        return f"bin:{crate}/tests"
    return f"lib:{_crate_of(path)}"


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

    parsed: list[tuple[Path, str, list[tuple[str, int, int]]]] = []
    sources_by_file: dict[str, str] = {}
    for rel, source in sources:
        code = _code_only(source)
        sources_by_file[rel.as_posix()] = source
        parsed.append((rel, code, _impl_blocks(code)))

    has_drop: set[str] = set()
    mutating: dict[str, bool] = {}
    # Per FILE, not per name: seven distinct types are called `EnvVarGuard`
    # here, so one global map lets the last file parsed answer for all of them
    # — the same bare-name collision this function exists to fix.
    structs_by_file: dict[str, dict[str, str]] = {}
    struct_bodies: dict[str, str] = {}
    per_def: dict[str, list[bool]] = {}
    by_crate: dict[tuple[str, str], bool] = {}
    by_file: dict[tuple[str, str], bool] = {}
    for rel, code, blocks in parsed:
        for match in re.finditer(r"\bimpl\b[^{;]*?\bDrop\b[^{;]*?\bfor\b([^{;]*)\{", code):
            idents = IDENT.findall(match.group(1))
            if idents:
                has_drop.add(idents[-1])
        local: dict[str, str] = {}
        for match in re.finditer(r"\bstruct\s+([A-Za-z_][A-Za-z0-9_]*)[^\n{;]*\{", code):
            open_index = match.end() - 1
            body = code[open_index : _balanced_end(code, open_index)]
            local[match.group(1)] = body
            struct_bodies.setdefault(match.group(1), body)
        structs_by_file[rel.as_posix()] = local
    for rel, code, blocks in parsed:
        local = structs_by_file.get(rel.as_posix(), {})
        for name, start, end in blocks:
            block = code[start:end]
            if not ENV_MUTATION.search(block):
                continue
            # A guard self-locks when it OWNS the `MutexGuard` — the lock then
            # lives exactly as long as the guard, which is the property that
            # matters. Keying on the lock's NAME instead was wrong in both
            # directions: `xai-grok-shell`'s `EarlyInvalidationGuard` holds
            # `EARLY_INVALIDATION_LOCK` for its lifetime and is sound despite
            # the name, while a guard could name a lock it never holds.
            # Storing a `MutexGuard` field IS the property: the lock then
            # lives exactly as long as the guard. Requiring a literal `.lock(`
            # as well was wrong — `xai_grok_env::EnvVarGuard` is built from
            # `env_lock()`, a helper that returns the guard.
            locks = "MutexGuard" in local.get(name, "")
            mutating[name] = mutating.get(name, False) or locks
            per_def.setdefault(name, []).append(locks)
            crate = _crate_of(rel)
            by_crate[(crate, name)] = by_crate.get((crate, name), locks) and locks
            by_file[(rel.as_posix(), name)] = locks

    # Conservative AND across definition sites; per-crate entries win at a use
    # site in that crate.
    types = {n: all(per_def.get(n, [False])) for n in mutating if n in has_drop}
    by_crate = {k: v for k, v in by_crate.items() if k[1] in types}
    by_crate.update({k: v for k, v in by_file.items() if k[1] in types})

    # One hop for a wrapper that owns a guard instead of touching env itself.
    for _ in range(3):
        grew = False
        for rel, code, blocks in parsed:
            for name, start, end in blocks:
                if name in types:
                    continue
                block = code[start:end]
                if not any(used in types for used in TYPE_ASSOC_CALL.findall(block)):
                    continue
                fields = structs_by_file.get(rel.as_posix(), {}).get(name, "")
                owns = "MutexGuard" in fields or any(t in fields for t in types)
                if not owns:
                    continue
                locks = "MutexGuard" in structs_by_file.get(rel.as_posix(), {}).get(name, "")
                types[name] = locks
                by_crate[(_crate_of(rel), name)] = locks
                grew = True
        if not grew:
            break
    module_alias: dict[tuple[str, str], str] = {}
    name_alias: dict[tuple[str, str], str] = {}
    for rel, code, _blocks in parsed:
        crate = _crate_of(rel)
        for match in REEXPORT.finditer(code):
            source = _crate_from_segment(match.group("src"))
            item = match.group("item")
            if item[:1].isupper():
                name_alias[(crate, item)] = source
            else:
                module_alias[(crate, item)] = source
    # What variable does the guard mutate in its OWN body? A guard whose
    # `set` takes only the new VALUE (`EnvVarGuard::set("42")` in
    # `util/config/persist.rs`, which writes a fixed const) made the call's
    # first argument look like a variable name, so two calls with different
    # values read as two different variables (#449 review).
    type_vars: dict[str, frozenset[str]] = {}
    for rel, code, blocks in parsed:
        raw = sources_by_file.get(rel.as_posix(), "")
        consts = _string_consts(raw)
        for name, start, end in blocks:
            if name not in types:
                continue
            fixed = _env_variables(raw[start:end], consts)
            if fixed:
                type_vars[name] = type_vars.get(name, frozenset()) | frozenset(fixed)
    return EnvMutators(
        types=types,
        by_crate=by_crate,
        funcs={},
        type_vars=type_vars,
        module_alias=module_alias,
        name_alias=name_alias,
    )


def _file_helpers(
    source: str,
    code: str,
    types: dict[str, bool],
    *,
    crate: str = "",
    qualified: dict[tuple[str, tuple[str, ...]], frozenset[str]] | None = None,
) -> tuple[dict[str, bool], dict[str, frozenset[str]], dict[str, bool]]:
    """Same-file non-test fns that mutate env: do they self-lock, and what do
    they touch?

    The variables matter as much as the lock. A keyed test that reaches its
    mutation through `set_home()` has an EMPTY variable set if you only read
    the test body, so key consistency has nothing to compare and approves it
    (#449 review).

    ``qualified`` (from [`_index_qualified_helpers`]) resolves a
    `crate::a::b::fn()` call to a cross-file mutator; omitted, this reads only
    the current file, which is what building that index itself needs (#449
    review, Finding 1).
    """

    impl_spans = [(s, e) for _n, s, e in _impl_blocks(code)]
    consts = _string_consts(source)
    bodies: dict[str, tuple[str, bool]] = {}
    helpers: dict[str, bool] = {}
    helper_vars: dict[str, frozenset[str]] = {}
    returns_guard: dict[str, bool] = {}
    for match in FN_DEF.finditer(code):
        if any(start <= match.start() < end for start, end in impl_spans):
            continue  # an inherent method, reached through its type instead
        if any(_is_test_attr(a) for a in _preceding_attributes(source, code, match.start())):
            continue
        body_range = _fn_body(source, match.end())
        if body_range is None:
            continue
        # A helper that locks internally and returns `()` releases on return;
        # binding its result gives the CALLER nothing (#449 review). Only a
        # helper that hands the guard back can protect its caller.
        signature = code[match.end() : body_range[0]]
        # `-> bool` / `-> Result<..>` / `-> ()` all have a `->`, and none of
        # them hand the caller a lock. The RETURN TYPE has to be a guard
        # (#449 review).
        returned = signature.split("->", 1)[1] if "->" in signature else ""
        hands_back = "MutexGuard" in returned or any(
            t in returned for t in types
        )
        body = code[body_range[0] : body_range[1]]
        bodies[match.group("name")] = (body, hands_back)
        uses_guard = any(used in types for used in TYPE_ASSOC_CALL.findall(body))
        # A qualified call this file cannot see the definition of but the
        # cross-file index resolved to a known mutator (Finding 1: e.g.
        # `crate::test_support::ensure_hermetic_git_on_path()`).
        uses_cross_file = qualified is not None and any(
            fname in qualified.get((crate, _mod_path_of(mods)), frozenset())
            for mods, fname in CRATE_QUALIFIED_CALL.findall(body)
        )
        if not ENV_MUTATION.search(body) and not uses_guard and not uses_cross_file:
            continue
        name = match.group("name")
        locks = _env_lock_is_live(body)
        helpers[name] = helpers.get(name, False) or locks
        returns_guard[name] = returns_guard.get(name, False) or (locks and hands_back)
        helper_vars[name] = helper_vars.get(name, frozenset()) | frozenset(
            _env_variables(source[body_range[0] : body_range[1]], consts)
        )

    # Two hops and longer: a helper that only DELEGATES to another helper has
    # no `env::set_var` and no guard-type call of its own, so it was skipped
    # and a test calling it never became a candidate at all (#449 review).
    for _ in range(4):
        grew = False
        for name, (body, sig_returns) in bodies.items():
            called = {c for c in FREE_CALL.findall(body) if c in helpers and c != name}
            if not called:
                continue
            if name not in helpers:
                helpers[name] = any(helpers[c] for c in called)
                returns_guard[name] = sig_returns and any(
                    returns_guard[c] for c in called
                )
                grew = True
            inherited = frozenset().union(*(helper_vars.get(c, frozenset()) for c in called))
            if not inherited <= helper_vars.get(name, frozenset()):
                helper_vars[name] = helper_vars.get(name, frozenset()) | inherited
                grew = True
        if not grew:
            break
    return helpers, helper_vars, returns_guard


LET_BINDING = re.compile(
    r"let\s+(?P<bind>[A-Za-z_][A-Za-z0-9_]*)\s*(?::[^=;]*)?=\s*(?P<rhs>[^;]*);"
)


def _enclosing_block_end(body: str, position: int) -> int:
    """Offset of the `}` closing the block that contains ``position``.

    Rust drops a binding at its enclosing block, so a guard taken inside a
    nested block protects nothing after that block closes — but a span running
    to the end of the body said otherwise (#449 review).
    """

    depth = 0
    for index in range(position, len(body)):
        char = body[index]
        if char == "{":
            depth += 1
        elif char == "}":
            if depth == 0:
                return index
            depth -= 1
    return len(body)


def _protected_spans(
    body: str,
    mutators: EnvMutators,
    helpers: dict[str, bool],
    crate: str,
    file: str,
    name_paths: dict[str, tuple[str, ...]],
    returning: dict[str, bool] | None = None,
) -> list[tuple[int, int]]:
    """`(from, until)` offsets over which serialisation actually holds.

    A protector is a BOUND value that owns the lock: an env-lock guard, a
    self-locking guard type, or a self-locking helper's return. `let _ = ...`
    and an unbound temporary both drop at the statement's semicolon and protect
    nothing, and an explicit `drop(x)` ends the span.
    """

    returning = helpers if returning is None else returning
    spans: list[tuple[int, int]] = []
    for match in LET_BINDING.finditer(body):
        bind, rhs = match.group("bind"), match.group("rhs")
        if bind == "_":
            continue
        holds = bool(ENV_LOCK_VALUE.search(rhs)) or any(
            mutators.self_locks(t, crate, file, name_paths.get(t, ()))
            for t in TYPE_ASSOC_CALL.findall(rhs)
        ) or any(returning.get(f) for f in FREE_CALL.findall(rhs))
        if not holds:
            continue
        released = re.search(r"\bdrop\s*\(\s*" + re.escape(bind) + r"\s*\)", body)
        scope_end = _enclosing_block_end(body, match.end())
        end = min(released.start(), scope_end) if released else scope_end
        spans.append((match.start(), end))
    return spans


def _mutation_sites(
    body: str,
    mutators: EnvMutators,
    helpers: dict[str, bool],
    crate: str = "",
    file: str = "",
    name_paths: dict[str, tuple[str, ...]] | None = None,
) -> list[int]:
    """Offsets where this body mutates env and needs a protector of its own.

    A call to a helper that locks around its OWN mutations is already covered
    by that lock, so it does not need a span here. That is a different question
    from whether the helper protects the CALLER's later mutations — it does not
    unless it hands the guard back, which [`_protected_spans`] decides.
    """

    name_paths = name_paths or {}
    sites = [m.start() for m in ENV_MUTATION.finditer(body)]
    # A self-locking guard's own construction runs under its own mutex, bound
    # or not, so it does not need a caller span -- the same reasoning already
    # applied to self-locking helpers, one level over (#449 review). A later
    # RAW mutation is still a site and still needs its own span.
    sites += [
        m.start()
        for m in TYPE_ASSOC_CALL.finditer(body)
        if m.group(1) in mutators.types
        and not mutators.self_locks(m.group(1), crate, file, name_paths.get(m.group(1), ()))
    ]
    sites += [
        m.start()
        for m in ENV_GUARD_NAME.finditer(body)
        if not mutators.self_locks(m.group(1), crate, file, name_paths.get(m.group(1), ()))
    ]
    sites += [
        m.start()
        for m in FREE_CALL.finditer(body)
        if m.group(1) in helpers and not helpers[m.group(1)]
    ]
    return sorted(set(sites))


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
    regime: str
    group: str


def _is_integration_target(path: Path) -> bool:
    """`crates/<group>/<crate>/tests/<file>.rs` — its own test binary.

    Only a file directly under the crate's `tests/` dir is a target; a
    `tests/` MODULE under `src/` (`src/app/dispatch/tests/cta_e2e.rs`) shares
    the lib's process and must not be mistaken for one.
    """

    parts = path.parts
    return "src" not in parts and len(parts) >= 2 and parts[-2] == "tests"


STR_CONST = re.compile(
    r"\bconst\s+(?P<name>[A-Z][A-Z0-9_]*)\s*:\s*&(?:'static\s+)?str\s*=\s*\"(?P<value>[^\"]*)\""
)


def _string_consts(raw_source: str) -> dict[str, str]:
    """`const NAME: &str = "VALUE"` in this file.

    Key consistency compares variable NAMES, so a test mutating `HOME_VAR`
    and one mutating `"HOME"` recorded two different variables and were never
    compared — although they touch the same process-global (#449 review).
    """

    return {m.group("name"): m.group("value") for m in STR_CONST.finditer(raw_source)}


def _env_variables(raw_body: str, consts: dict[str, str] | None = None) -> set[str]:
    """Names of the env vars a test mutates.

    Read from the RAW body: `_code_only` masks string literals, which is
    where the names are. Bare SCREAMING_CASE consts count too — a crate that
    names its key (`ENV_AUTO_COMPACT_THRESHOLD_PERCENT`) is the common case.
    """

    found: set[str] = set()
    for match in ENV_VAR_ARG.finditer(raw_body):
        found.add(match.group("lit") or match.group("konst"))
    resolved = consts or {}
    return {resolved.get(v, v) for v in found if v}


def _test_variables(
    raw_body: str, code_body: str, mutators: EnvMutators, consts: dict[str, str]
) -> set[str]:
    """Variables this test mutates, preferring what a guard's DEFINITION says.

    `EnvVarGuard::set("42")` in `util/config/persist.rs` passes a value, not a
    key -- the guard writes a fixed const internally. Reading the call's first
    argument recorded `42` as a variable, so two calls with different values
    looked like two different variables and never had their keys compared
    (#449 review). When the guard's own body names the variable, that wins;
    when it mutates through a parameter instead, the call site is all there is.
    """

    found = _env_variables(raw_body, consts)
    for used in set(TYPE_ASSOC_CALL.findall(code_body)):
        fixed = mutators.type_vars.get(used)
        if not fixed:
            continue
        found -= set(_env_variables(raw_body, consts)) - found  # keep raw finds
        found = {v for v in found if v not in _call_args(raw_body, used)} | set(fixed)
    return found


def _call_args(raw_body: str, type_name: str) -> set[str]:
    """First-argument tokens passed to `type_name::method(..)` in this body."""

    pattern = re.compile(
        re.escape(type_name)
        + r"\s*::\s*[A-Za-z_][A-Za-z0-9_]*\s*\(\s*(?:\"(?P<lit>[^\"]*)\"|(?P<konst>[A-Z][A-Z0-9_]{2,}))"
    )
    return {m.group("lit") or m.group("konst") for m in pattern.finditer(raw_body)}


def analyze_source(
    source: str,
    *,
    relpath: Path | None = None,
    mutators: EnvMutators | None = None,
    qualified_index: dict[tuple[str, tuple[str, ...]], frozenset[str]] | None = None,
) -> list[Candidate]:
    """Every env-mutating test in ``source``, pre-judged for the local regimes.

    ``qualified_index`` is [`_index_qualified_helpers`]'s scan-root-wide map,
    letting a `crate::a::b::fn()` call in ``source`` resolve to a mutator
    defined in another file (#449 review, Finding 1). Omitted for a
    single-source call (no scan root to index), which is exactly today's
    behaviour.
    """

    path = relpath or Path("<input>")
    code = _code_only(source)
    if mutators is None:
        mutators = index_env_mutators([(path, source)])
    crate = _crate_of(path)
    helpers, helper_vars, returning = _file_helpers(
        source, code, mutators.types, crate=crate, qualified=qualified_index
    )
    name_paths = _name_paths(code)
    consts = _string_consts(source)
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
        regime = "none"
        if "unkeyed" in kinds:
            regime = "unkeyed-serial"
        else:
            # Positional, not presence-based. "Is a lock mentioned anywhere in
            # this body?" says yes to a lock taken AFTER the mutation, to an
            # unbound guard temporary that drops at its own semicolon, and to a
            # helper that releases on return — each leaving a real mutation
            # unprotected (#449 review). Every mutation must fall inside a span
            # where a bound protector is still alive.
            spans = _protected_spans(
                body, mutators, helpers, crate, path.as_posix(), name_paths, returning
            )
            sites = _mutation_sites(
                body, mutators, helpers, crate, path.as_posix(), name_paths
            )
            # No remaining sites means every mutation this test performs is
            # already covered where it happens (inside a self-locking helper),
            # so there is nothing left for a caller span to protect.
            if all(any(start <= site < end for start, end in spans) for site in sites):
                regime = "lock-covers-every-mutation"
            elif (
                _is_integration_target(path)
                and test_count == 1
                and not MOD_DECL.search(code)
            ):
                # Nothing shares its process, so there is no sibling to
                # corrupt -- but only if the target really is this one file.
                # An integration root declaring modules (`mod common;`,
                # `#[path = ".."] mod x;`) compiles their tests into the SAME
                # binary, and `xai-grok-pager` does exactly that with
                # `tests/pty_e2e/` (#449 review). Counting only local tests
                # would declare an unprotected mutation isolated. Any module
                # declaration disqualifies the regime: conservative, and the
                # test still has four other ways to be sound.
                regime = "sole-test-in-binary"
        sound = regime != "none"
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
                variables=tuple(
                    sorted(
                        _test_variables(
                            source[body_range[0] : body_range[1]], body, mutators, consts
                        )
                        | {
                            var
                            for call in FREE_CALL.findall(body)
                            for var in helper_vars.get(call, ())
                        }
                    )
                ),
                sound=sound,
                regime=regime,
                group=_process_group(path),
            )
        )
    return out


def key_map(candidates: list[Candidate]) -> dict[tuple[str, str], list[frozenset[str]]]:
    """(test binary, variable) -> the serial keys under which it is mutated.

    Scoped per test BINARY: two crates never share a process, so comparing
    their keys invents clashes that cannot occur.

    `<unkeyed>` marks a mutation with no key at all. A variable appearing
    under two entries is one no keyed `#[serial]` can protect.
    """

    mapping: dict[tuple[str, str], list[frozenset[str]]] = {}
    for cand in candidates:
        # Sound tests stay in the map. Unkeyed `#[serial]` and `#[serial(home)]`
        # take DIFFERENT locks and can overlap, so skipping the unkeyed one let
        # a variable look consistently keyed and approved the keyed test
        # (#449 review). Same for a lock-serialised test: it does not compose
        # with a keyed one either. Being sound is about the test itself; it is
        # not a promise to anybody else's key.
        if cand.keyed:
            # Jointly held: a test carrying #[serial(a)] and #[serial(b)] holds
            # BOTH locks. Recording them as separate labels made the test
            # conflict with itself (#449 review).
            label = frozenset(cand.keyed)
        elif cand.regime == "sole-test-in-binary":
            continue  # alone in its process; it cannot clash with anyone
        else:
            label = frozenset({f"<{cand.regime}>"})
        for var in cand.variables:
            mapping.setdefault((cand.group, var), []).append(label)
    return mapping


def _shared_key(label_sets: list[frozenset[str]]) -> bool:
    """Is there one lock every mutation of this variable holds?"""

    if not label_sets:
        return True
    common = set(label_sets[0])
    for other in label_sets[1:]:
        common &= other
    return bool(common)


def judge(candidates: list[Candidate], keys: dict[tuple[str, str], list[frozenset[str]]]) -> list[Finding]:
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
        if cand.keyed and not cand.variables:
            # Keyed serialisation is only sound if the key is consistent for
            # the variable, and we could not determine which variable this
            # touches. Unknown is not the same as safe.
            findings.append(
                Finding(
                    path=cand.path,
                    line=cand.line,
                    name=cand.name,
                    reason=(
                        f"{cand.mention} with keyed #[serial({cand.keyed[0]})], but the "
                        "mutated variable could not be determined, so key "
                        "consistency cannot be checked"
                    ),
                )
            )
            continue
        if cand.keyed:
            clashes = sorted(
                var
                for var in cand.variables
                if not _shared_key(keys.get((cand.group, var), []))
            )
            if not clashes:
                continue
            others = sorted(
                lbl
                for group in keys[(cand.group, clashes[0])]
                for lbl in group
                if lbl not in cand.keyed
            )
            other = others[0] if others else "another regime"
            reason = (
                f"{cand.mention} with keyed #[serial({cand.keyed[0]})], but "
                f"{clashes[0]} is also mutated under {other}"
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
    # `crate::a::b::fn()` only ever names something in the SAME crate, and
    # the scan root is one crate by convention (module docstring), so this
    # index is built from the scan root alone rather than the wider
    # `index_dir` above — no cross-crate case exists for it to miss.
    scan_files = [
        (p.relative_to(repo), p.read_text(encoding="utf-8")) for p in rust_files(scan_root)
    ]
    qualified_index = _index_qualified_helpers(
        [(rel, source, _code_only(source)) for rel, source in scan_files], mutators.types
    )
    # Two passes: key consistency is a property of the whole scanned set, not
    # of one file — a variable is only safely keyed if EVERY test touching it
    # agrees on the key, and the disagreeing test is usually elsewhere.
    candidates: list[Candidate] = []
    for rel, text in scan_files:
        candidates.extend(
            analyze_source(
                text, relpath=rel, mutators=mutators, qualified_index=qualified_index
            )
        )
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
