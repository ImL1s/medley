#!/usr/bin/env python3
"""Fail when a test touches a registered shared item without its serial key.

## The problem (#496)

`#[serial(key)]` groups are a convention with no enforcement. A test that
touches shared state and forgets the tag runs concurrently with the group
and nothing objects -- the group still passes, because the untagged test is
simply not IN it. The failure mode is worse than it sounds: the untagged
toucher does not fail. It makes SOMETHING ELSE fail, later, intermittently,
under load, and the person diagnosing it is reading the innocent test that
broke, not the one that raced it. That is exactly the shape #475 took:
`test_claimant_reindexes_even_when_marker_exists` failed 9 of 15 times when
paired with an epoch-bumping sibling, and 0 of 10 in isolation.

This repo already solved the analogous problem once, for env mutation
(`check_envguard_serial.py`, #319/#446): guards are found by DEFINITION, not
by name. The transferable idea is the SAME here -- which tests touch a given
shared item is answerable from the item's own identifier, not from a list a
person has to remember to update -- but the shape of "touching" is different
enough (an arbitrary named `static`, not a well-known `env::set_var` call
with a string-literal variable name as its first argument) that this is a
SIBLING script, not a generalisation of that one. Decided, not left open:
      - `check_envguard_serial.py`'s env-specific machinery (five
        alternative regimes, lock-liveness tracking, env-var-argument
        extraction) does not transfer -- a shared static has exactly one
        sound regime, an exact keyed `#[serial(key)]`, not "any lock".
      - Its pure SYNTAX primitives (comment/string masking, attribute
        collection, `#[test]` detection, balanced-brace body extraction)
        are generic Rust-scanning code, not env-specific, and are
        duplicated below rather than imported. Importing internals from a
        CI-critical sibling script would couple two independently-owned
        checkers' internals together for a few hundred lines of stable
        code; a copy that drifts is a smaller risk than a shared internal
        API neither script's own tests would catch changing under it.

## What "registered" means, and why an annotation on the static itself

Every process-global is NOT in scope -- most are read-only after
initialisation, or a `OnceLock`/`Lazy` that is genuinely safe under
concurrent test execution. Scope is opt-in, anchored at the declaration:

    // SERIAL-GROUP: heap_profile_monitor
    static TEST_RESIDENT: AtomicU64 = AtomicU64::new(0);
    static TEST_ALLOCATED: AtomicU64 = AtomicU64::new(0);
    ...

A `// SERIAL-GROUP: <key>` comment claims every contiguous `static` (or
`static mut`) declaration immediately below it -- skipping only blank lines,
further comments, and attributes -- as one registry item keyed by `<key>`.
There is deliberately no separate identifier list to keep in sync: the
static block IS its own definition, so nothing can name a static the marker
does not also cover, or vice versa, without the block boundary itself
changing. A marker that claims zero statics (typo'd placement, or the block
it pointed at was deleted or moved) is a hard error naming the marker's own
location -- not a silent no-op -- because a registry item nobody can find
touchers for is indistinguishable from one that has none, and this guard's
entire premise is not assuming that.

**Registering a new global is still a manual step, and that is deliberate**
(see #496's own design question: "should adding a new global require
registering it"). Introducing a new process-global mutable `static` is rare
and visible in review; forgetting a per-test attribute happens every time
someone adds a test. Moving the manual step from the frequent event to the
rare one is the fix, not eliminating manual steps altogether -- membership
of TESTS in a registered item's group is what must be derived, and is.

## How a toucher is found, and its two stages

Stage 1, DIRECT: a function is a toucher of key `K` if `K`'s registered
identifier appears as a whole word in that function's own code-only body
(comments and string/char literals masked, same masking
`check_envguard_serial.py` uses).

Stage 2, TRANSITIVE: a `#[test]` / `#[tokio::test]` function is a member of
`K`'s group if it is itself a Stage-1 toucher, or REACHES one through calls,
however many hops that takes. Every function's key set starts at its Stage-1
keys and is repeatedly grown by re-scanning every function for call-shaped
references into a function that already holds keys, unioning them in, until
a full pass adds nothing. Keys only ever get ADDED, never removed, over a
finite universe of keys and functions, so this is monotonic on a finite
lattice and provably terminates (`_MAX_ROUNDS` is a generous safety bound on
top of that, not the termination argument; exceeding it is a hard error,
not a silent pass). Three call shapes propagate a
hop: a bare `name(` resolved within the SAME FILE, then via a `use`
import of that name (`use crate::a::bump; bump()`), then (if still
unresolved) not at all; a `path::to::name(`
resolved by full module path when `path` starts with `crate`, and ALSO by
its last segment alone against every file's own module leaf (its filename
stem) either way -- the shape a sibling module is actually called by in
this tree, with no `crate::` prefix at all; and a `Type::assoc_fn(`
resolved against `impl Type { .. }` blocks in the same cargo process
group, so a call written
`some_mod::Type::assoc_fn(...)` still resolves on the `Type::assoc_fn(`
suffix regardless of what qualifies it.

This is a real design turn, not an ideal from the outset: an earlier
one-hop-only version of this checker calibrated to 15/15 against
`heap_profile_monitor` (a single-file, single-hop chain) but, dry-run tested
against `search_cache_epoch`'s real shape (#492, merged to `providers` since
-- see below), missed 5 of 6 currently-tagged tests. `heal_quarantines_only_on_
confirmed_corruption` alone needs TWO hops (test -> `quarantined_after` ->
`heal_unusable`, all in `search_recovery.rs`); others route through several
hops of production orchestration code the test never names directly. A
fixed hop count tuned to fit one item's deepest chain is wrong again at the
next item -- so there is no fixed count, only the fixpoint.

WHAT THIS DOES NOT CHECK, so a green tick is not read as more than it is:

    A function pointer passed as a VALUE -- not called -- is not a call and
    is not followed. `heap_profile_monitor`'s own test module wires
    `test_dump` / `test_stats` / `test_set_active` / `test_rss` into a
    `HeapProfileMonitor` via `.with_test_hooks(test_dump, ...)`; those names
    appear as bare arguments, never as `test_dump(...)`. Chasing bare
    mentions was tried during design and measured to be WRONG, not just
    imprecise: it marks `monitor()` (and everything that calls it,
    including `session_id_is_sticky_and_rejects_non_uuid` and seven other
    currently-untagged tests in that same file) as touchers, when none of
    them ever actually run the hooks -- only `.poll_tick()` does, and the
    tests that call it are exactly the ones already tagged. A false
    positive here would have meant recommending a tag on a fine test.

    A method call on an arbitrary receiver (`epoch.changed()`) is not
    resolved -- there is no cheap way to know a bare variable's type
    without a real type checker, and neither does `check_envguard_serial.py`
    for the equivalent shape. `Type::assoc_fn(...)` (an explicit,
    syntactic type name) IS resolved; an instance method is not. Measured
    not to block the `search_cache_epoch` dry run below: every chain that
    needed to cross this had an associated-fn call (`CacheEpoch::now()`) on
    the same path as the unresolved instance call (`.changed()`), so the
    chain still connects on the call the checker CAN see.

    Reachability is not control flow: a function that calls a toucher only
    on an ERROR-recovery branch is indistinguishable, to a textual scan,
    from one that calls it unconditionally. Measured, not hypothetical:
    `search_fts.rs::SessionSearchIndex::open_or_create` calls
    `search_recovery::heal_unusable(..)` only in its
    `Err(e) if is_unusable_db_error(&e)` arm, which a fresh-tmpdir test
    fixture essentially never takes -- but the transitive closure has no way
    to know that, so it derives 37 candidate `search_cache_epoch` members
    from that one file, against 7 a human tagged in #492 by judging which
    tests could actually OBSERVE a moved counter, not merely reach the call
    that moves it. This is sound (nothing is missed) but not tight (some
    flagged tests likely never execute the branch that matters), and it is
    exactly why `search_cache_epoch` is not registered by this change --
    see the section below.

    That superset is not pure noise, though, and this is the strongest
    argument for deriving membership rather than trusting a hand-written
    list: `test_malformed_db_file_is_quarantined_and_recreated` (also in
    `search_fts.rs`) is IN the 37-candidate dry run, reaching `CACHE_EPOCH`
    through `with_index` -> `heal_unusable`, two hops, no direct mention of
    the identifier anywhere in the test's own body. It was NOT among the
    6 tests #492 originally tagged by grep -- a real member the grep could
    not see, because it goes through a caller two levels up. Codex's review
    of #492 found the same gap independently and #492 was corrected to tag
    it, which is how it is now among the 7. The dry run had already found
    it, in the noisier 36-candidate superset, before that review comment
    existed.

    Two tests can each be individually sound and still race if they are
    governed by two DIFFERENT keys that are not the same lock (`#319`,
    `#459`'s equivalent finding for env). This checker judges a test
    against the KEY its own registered item requires; it does not compare
    keys across items.

    `Self::name(...)` resolves to the calling function's OWN enclosing
    `impl` type -- but only when it has one. `Self` inside a TRAIT
    definition's own default method body (`trait Foo { fn helper() {
    Self::other(); } }`, not an `impl` block) has no enclosing type this
    checker tracks, and is not resolved; sound (skipped, not
    misattributed), not tight. Measured, per this checker's own standard
    for what stays a documented boundary rather than a fix: this crate's
    four traits (`AsyncTerminalRunner`, `FacetProvider`, `StorageAdapter`,
    `SessionNotificationSender`) declare zero default method bodies
    between them, so `Self::` inside one is not a shape that currently
    occurs here -- if that changes, this residual needs revisiting, not
    just re-citing.

    `self::[...]name(...)` / `super::[...]name(...)` -- an arbitrary
    chain of leading `self`/`super` segments, optionally followed by a
    named module path -- ARE walked, not left as a residual: measured at
    129 occurrences for `super::sibling_mod::name(...)` (a `super` then a
    named module) and 15 for `super::super::name(...)` (two `super`s, no
    trailing module) in this crate, both real enough that an earlier,
    single-segment-only version of this fix would have silently missed
    the majority of relative-qualifier calls in the tree. Since a
    file-based module model cannot tell whether a `super` crossed a REAL
    directory boundary or just an INLINE `mod tests { use super::*; ...
    }` block (both are invisible the same way to `_module_path`), every
    ascent from 0 (every `super` was inline nesting) up to the full
    count (every `super` was a real parent) is tried and unioned, sound
    over tight in the same way the rest of this checker is.

    An `impl` for a non-path type (`impl Trait for &Foo { .. }`, `impl
    Trait for dyn Foo { .. }`, `impl Trait for (A, B) { .. }`) has no
    single identifier this checker extracts as "the type," and the whole
    block is skipped -- its methods are simply not indexed under any type
    name, so a `Type::method(...)` call into one of them does not resolve
    (though an unqualified same-file call still would, same as any other
    function). Measured: zero occurrences of any of those three shapes in
    this crate today.

Scan scope is one crate: `crates/codegen/xai-grok-shell/src/**/*.rs` and
`crates/codegen/xai-grok-shell/tests/**/*.rs` unless `--scan-root` says
otherwise -- src unit tests and that crate's integration binaries. The
same "one crate" assumption `check_envguard_serial.py` documents still
holds for `crate::` resolution (`crate::` in an integration target is
that binary, not the library).

There is deliberately no allowlist file, unlike the env checker: this guard
ships with its full registry (`heap_profile_monitor`) already at zero
findings, verified by calibrating derived membership against every test
currently tagged `#[serial(heap_profile_monitor)]` in the tree (15/15
match, no more, no fewer) -- so there is no "known straggler" for a first
slice to defer. `search_cache_epoch` (CACHE_EPOCH, #475/#492) is NOT
registered by this change, for two independent reasons: #492 (which
introduces that key) has since merged to `providers`, but it did not add a
`// SERIAL-GROUP: search_cache_epoch` marker -- there is nothing in the
tree today for this checker to anchor a registry entry on, so adding one
here would be introducing the marker unilaterally rather than the person
who owns that key's shape doing so; and a dry run of this checker against
#492's own diff (registering the marker in a scratch worktree, never
committed) found the reachability closure derives 37 candidate members
against the 7 a human actually tagged (6 by grep, a 7th added after this
same dry run's superset -- and, independently, Codex's review -- both
named it), for the reachability-is-not-control-flow reason documented
above. Registering that key for real is a decision for whoever lands it --
accept the wider serialisation, or decide this checker needs a scoping
mechanism this design does not attempt -- not something to force through
here by narrowing the resolver to fit one
item's shape.

Usage:
    python3 scripts/check_shared_state_serial.py
    python3 scripts/check_shared_state_serial.py --dump
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

DEFAULT_SCAN_ROOT = Path("crates/codegen/xai-grok-shell/src")
DEFAULT_SCAN_TEST_ROOT = Path("crates/codegen/xai-grok-shell/tests")
DEFAULT_SCAN_ROOTS = (DEFAULT_SCAN_ROOT, DEFAULT_SCAN_TEST_ROOT)

# --- pure Rust-syntax primitives -------------------------------------------
# Duplicated from `check_envguard_serial.py` rather than imported -- see the
# module docstring's "Decided, not left open" section for why.

RAW_STRING_START = re.compile(r'r(#+)?"')
CHAR_LITERAL = re.compile(r"'(?:\\.|[^\\'\n])'")
FN_DEF = re.compile(
    r"(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?fn\s+"
    r"(?:r#)?(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
)
MACRO_DEF = re.compile(r"macro_rules!\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)")
MACRO_INVOKE = re.compile(
    r"(?<![:.\w])(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*([A-Za-z_][A-Za-z0-9_]*)\s*!"
)
INLINE_MOD = re.compile(
    r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{"
)
MACRO_TEST_FN = re.compile(
    r"\bfn\s+(?:\$)?([A-Za-z_][A-Za-z0-9_]*)"
)
TEST_ATTR = re.compile(
    r"#\s*\[\s*(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*test\b",
    re.DOTALL,
)
SERIAL_ATTR = re.compile(
    r"#\s*\[\s*(?:serial_test\s*::\s*)?serial\s*(?:\((?P<args>.*)\))?\s*\]",
    re.DOTALL,
)
IMPL_KW = re.compile(r"\bimpl\b")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
FREE_CALL = re.compile(r"(?<![:.\w])(?:r#)?([a-z_][a-z0-9_]*)\s*\(")
USE_PLAIN = re.compile(
    r"\buse\s+(crate|super|self)((?:::(?:r#)?[A-Za-z_][A-Za-z0-9_]*)+)"
    r"(?:\s+as\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*))?\s*;"
)
USE_BRACE = re.compile(
    r"\buse\s+(crate|super|self)((?:::(?:r#)?[A-Za-z_][A-Za-z0-9_]*)*)"
    r"::\{([^}]+)\}\s*;"
)
USE_HEAD = re.compile(
    r"\buse\s+(crate|super|self|(?:r#)?[A-Za-z_][A-Za-z0-9_]*)"
)
# Any `path::to::name(` call, `crate`-rooted or not. Resolved two ways (see
# `_resolve_calls`): a `crate`-rooted path by its FULL module path, and every
# path (rooted or not) by its LAST segment alone against a file's own module
# leaf -- the shape a sibling module is actually called by in this tree
# (`search_recovery::heal_unusable(`, no `crate::` prefix at all).
QUALIFIED_CALL = re.compile(
    r"\b((?:(?:r#)?[A-Za-z_][A-Za-z0-9_]*\s*::\s*)+)(?:r#)?([a-z_][a-z0-9_]*)\s*\("
)
TYPE_ASSOC_CALL = re.compile(
    r"\b([A-Z][A-Za-z0-9_]*)\s*::\s*([A-Za-z_][A-Za-z0-9_]*)\s*(?:::\s*<[^>]*>\s*)?\("
)
# `<Type as Trait>::method(` is scanned by `_ufcs_calls`, not a regex:
# `[^\>]*` cannot cross nested generics (`<Box<Vec<u8>> as Bump>::bump()`)
# (#516 review).

# --- the registry: `// SERIAL-GROUP: <key>` anchors a `static` block -------

REGISTRY_MARKER = re.compile(
    r"^[ \t]*//[ \t]*SERIAL-GROUP:[ \t]*(?P<key>[a-z][a-z0-9_]*)[ \t]*$", re.M
)
STATIC_DECL = re.compile(
    r"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?static[ \t]+(?:mut[ \t]+)?"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)[ \t]*:"
)
# A line the block-scan may pass THROUGH without ending the block: blank, a
# comment (plain or doc), or an attribute. Not a static decl itself.
SKIPPABLE_LINE = re.compile(r"^[ \t]*($|//|#\[)")
# A `/* ... */` opener at column 0 of the remaining line. The registry walk
# then consumes through the closer, including when it spans lines -- a
# block comment between two statics must not end the group (#516 review).
_BLOCK_COMMENT_OPEN = re.compile(r"^[ \t]*/\*")


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


def _macro_invoke_arity(source: str, bang_end: int) -> int:
    """Top-level items in `macro!(...)` / `![...]` / `!{...}`.

    `cases!(one, two)` with one `fn $name` template expands twice; counting
    the invocation's arity is what lets those expansions exist as two
    members before the sole-member exemption (#516 review).
    """

    index = bang_end
    while index < len(source) and source[index].isspace():
        index += 1
    if index >= len(source) or source[index] not in "([{":
        return 1
    end = _balanced_end(source, index)
    items = 0
    saw_item = False
    i = index + 1
    limit = end - 1 if end > index else index
    while i < limit:
        comment_end = _skip_comment(source, i)
        if comment_end is not None:
            i = comment_end
            continue
        raw_end = _skip_raw_string(source, i)
        if raw_end is not None:
            i = raw_end
            saw_item = True
            continue
        ch = source[i]
        if ch == '"':
            i = _skip_quoted(source, i, ch)
            saw_item = True
            continue
        if ch == "'" and (char_end := _skip_char_literal(source, i)) is not None:
            i = char_end
            saw_item = True
            continue
        if ch in "([{":
            i = _balanced_end(source, i)
            saw_item = True
            continue
        if ch == ",":
            if saw_item:
                items += 1
            saw_item = False
            i += 1
            continue
        if not ch.isspace():
            saw_item = True
        i += 1
    if saw_item:
        items += 1
    return max(items, 1)


def _macro_invoke_inner(source: str, bang_end: int) -> str:
    """Argument text inside `macro!(...)` / `![...]` / `!{...}`."""

    index = bang_end
    while index < len(source) and source[index].isspace():
        index += 1
    if index >= len(source) or source[index] not in "([{":
        return ""
    end = _balanced_end(source, index)
    inner_end = end - 1 if end > index + 1 else index + 1
    return source[index + 1 : inner_end]


_TOKEN = re.compile(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*|[0-9]+|::|[^\sA-Za-z0-9_]")


def _token_list(text: str) -> list[str]:
    return _TOKEN.findall(text)


_METAVAR = re.compile(
    r"\$(?P<name>[A-Za-z_][A-Za-z0-9_]*)(?::(?P<kind>[A-Za-z_][A-Za-z0-9_]*))?"
)


def _matcher_named_parts(
    inner: str,
) -> list[tuple[str | None, str | None, str | None]]:
    """(`literal`, kind, metavar_name) in matcher order.

    `(clean $name:ident)` vs `(touch $name:ident)` share arity; literals
    distinguish them. `$value:expr` is not a single token (#516 review).
    """

    parts: list[tuple[str | None, str | None, str | None]] = []
    index = 0
    n = len(inner)
    while index < n:
        if inner[index].isspace():
            index += 1
            continue
        if inner[index] == "$":
            k = index + 1
            while k < n and inner[k].isspace():
                k += 1
            if k < n and inner[k] == "(":
                end = _balanced_end(inner, k)
                body = inner[k + 1 : end - 1]
                j = end
                while j < n and inner[j].isspace():
                    j += 1
                sep_start = j
                while j < n and inner[j] not in "*+?":
                    j += 1
                if j < n:
                    sep = inner[sep_start:j].strip()
                    parts.append((None, "repeat", f"{body}\x1f{sep}\x1f{inner[j]}"))
                    index = j + 1
                    continue
        metavar = _METAVAR.match(inner, index)
        if metavar:
            parts.append((None, metavar.group("kind") or "ident", metavar.group("name")))
            index = metavar.end()
            continue
        token = _TOKEN.match(inner, index)
        if token:
            parts.append((token.group(0), None, None))
            index = token.end()
            continue
        index += 1
    return parts


def _matcher_parts(
    inner: str,
) -> list[tuple[str | None, str | None, str | None]]:
    return _matcher_named_parts(inner)


def _fragment_token_ok(token: str, kind: str) -> bool:
    """Whether one token satisfies a `macro_rules` fragment specifier."""

    if kind == "tt":
        return True
    if kind == "ident":
        return bool(re.fullmatch(r"(?:r#)?[A-Za-z_][A-Za-z0-9_]*", token))
    if kind == "lifetime":
        return bool(re.fullmatch(r"'(?:_|[A-Za-z_][A-Za-z0-9_]*)", token))
    if kind == "literal":
        if token in {"true", "false"}:
            return True
        if re.fullmatch(r"[0-9]+(?:\.[0-9]+)?", token):
            return True
        return len(token) >= 2 and token[0] == token[-1] and token[0] in "\"'"
    return True


def _consume_fragment(
    tokens: list[str], cursor: int, kind: str, next_literal: str | None
) -> int | None:
    if cursor >= len(tokens):
        return None
    if kind in {"ident", "lifetime", "literal", "tt"}:
        if not _fragment_token_ok(tokens[cursor], kind):
            return None
        return cursor + 1
    depth = 0
    index = cursor
    while index < len(tokens):
        token = tokens[index]
        if depth == 0 and next_literal is not None and token == next_literal:
            return index if index > cursor else None
        if token in "([{":
            depth += 1
        elif token in ")]}" and depth:
            depth -= 1
        index += 1
    if next_literal is None:
        return index
    return None


def _matcher_consume(
    parts: list[tuple[str | None, str | None, str | None]],
    tokens: list[str],
    cursor: int,
) -> int | None:
    """Advance `cursor` through `tokens` if `parts` match a prefix."""

    for index, (literal, kind, name) in enumerate(parts):
        if kind == "repeat":
            body, sep, rep = (name or "\x1f\x1f*").split("\x1f")
            body_parts = _matcher_named_parts(body)
            sep_tokens = _token_list(sep)
            groups = 0
            while cursor < len(tokens):
                saved = cursor
                if groups and sep_tokens:
                    width = len(sep_tokens)
                    if tokens[cursor : cursor + width] != sep_tokens:
                        break
                    cursor += width
                nxt = _matcher_consume(body_parts, tokens, cursor)
                if nxt is None or nxt == cursor:
                    cursor = saved
                    break
                cursor = nxt
                groups += 1
                if rep == "?" and groups >= 1:
                    break
            if rep == "+" and groups < 1:
                return None
            continue
        if literal is not None:
            if cursor >= len(tokens) or tokens[cursor] != literal:
                return None
            cursor += 1
            continue
        next_literal = None
        for later_literal, _later_kind, _later_name in parts[index + 1 :]:
            if later_literal is not None:
                next_literal = later_literal
                break
        nxt = _consume_fragment(tokens, cursor, kind or "ident", next_literal)
        if nxt is None:
            return None
        cursor = nxt
    return cursor


def _matcher_matches_invoke(inner: str, invoke_inner: str) -> bool:
    tokens = _token_list(invoke_inner)
    cursor = _matcher_consume(_matcher_parts(inner), tokens, 0)
    return cursor is not None and cursor == len(tokens)


def _parse_repetition(inner: str) -> tuple[str, str, str] | None:
    """`(body, separator, *|+|?)` when `inner` is exactly `$(body)sep*`.

    `$(clean $name:ident),*` must not accept `touch one, touch two`
    (#516 review).
    """

    text = inner.strip()
    if not text.startswith("$"):
        return None
    index = 1
    while index < len(text) and text[index].isspace():
        index += 1
    if index >= len(text) or text[index] != "(":
        return None
    end = _balanced_end(text, index)
    if end <= index + 1:
        return None
    body = text[index + 1 : end - 1]
    rest = text[end:].strip()
    if not rest or rest[-1] not in "*+?":
        return None
    return body, rest[:-1].strip(), rest[-1]


def _repetition_matches(body: str, sep: str, rep: str, invoke_inner: str) -> bool:
    tokens = _token_list(invoke_inner)
    parts = _matcher_parts(body)
    sep_tokens = _token_list(sep)
    if not tokens:
        return rep in "*?"
    cursor = 0
    groups = 0
    n = len(tokens)
    while cursor < n:
        if groups and sep_tokens:
            width = len(sep_tokens)
            if tokens[cursor : cursor + width] != sep_tokens:
                return False
            cursor += width
        nxt = _matcher_consume(parts, tokens, cursor)
        if nxt is None or nxt == cursor:
            return False
        cursor = nxt
        groups += 1
        if rep == "?" and groups > 1:
            return False
    if cursor != n:
        return False
    if rep == "+" and groups < 1:
        return False
    return True


def _invoke_repeat_count(matcher: str, invoke_inner: str) -> int:
    """How many times a `$(...)*` matcher expands for this invocation.

    The separator is the matcher's, not a hardcoded comma: `$(ident);*`
    counts `cases!(one; two)` as two (#516 review).
    """

    inner = matcher.strip()
    if len(inner) >= 2 and inner[0] in "([{" and inner[-1] in ")]}":
        inner = inner[1:-1]
    parsed = _parse_repetition(inner)
    if parsed is None:
        named = _matcher_named_parts(inner)
        rep_at = next(
            (i for i, part in enumerate(named) if part[1] == "repeat"),
            None,
        )
        if rep_at is None:
            return 1
        tokens = _token_list(invoke_inner)
        cursor = _matcher_consume(named[:rep_at], tokens, 0)
        if cursor is None:
            return 0
        body, sep, _rep = (named[rep_at][2] or "\x1f\x1f*").split("\x1f")
        parts = _matcher_named_parts(body)
        sep_tokens = _token_list(sep)
        groups = 0
        n = len(tokens)
        suffix = named[rep_at + 1 :]
        while cursor < n:
            saved = cursor
            if groups and sep_tokens:
                width = len(sep_tokens)
                if tokens[cursor : cursor + width] != sep_tokens:
                    break
                cursor += width
            nxt = _matcher_consume(parts, tokens, cursor)
            if nxt is None or nxt == cursor:
                cursor = saved
                break
            if suffix:
                tail = _matcher_consume(suffix, tokens, nxt)
                if tail == len(tokens):
                    cursor = nxt
                    groups += 1
                    break
            cursor = nxt
            groups += 1
        return groups
    body, sep, _rep = parsed
    tokens = _token_list(invoke_inner)
    if not tokens:
        return 0
    parts = _matcher_parts(body)
    sep_tokens = _token_list(sep)
    cursor = 0
    groups = 0
    n = len(tokens)
    while cursor < n:
        if groups and sep_tokens:
            width = len(sep_tokens)
            if tokens[cursor : cursor + width] != sep_tokens:
                break
            cursor += width
        nxt = _matcher_consume(parts, tokens, cursor)
        if nxt is None or nxt == cursor:
            break
        cursor = nxt
        groups += 1
    return groups


def _strip_matcher_inner(matcher: str) -> str:
    inner = matcher.strip()
    if len(inner) >= 2 and inner[0] in "([{" and inner[-1] in ")]}":
        return inner[1:-1]
    return inner


def _bind_inner(inner: str, invoke_inner: str) -> dict[str, str]:
    """Map `$name` captures in `inner` to invocation tokens."""

    tokens = _token_list(invoke_inner)
    parts = _matcher_named_parts(inner)
    cursor = 0
    bindings: dict[str, str] = {}
    for index, (literal, kind, name) in enumerate(parts):
        if kind == "repeat":
            nxt = _matcher_consume([(None, "repeat", name)], tokens, cursor)
            if nxt is None:
                return {}
            cursor = nxt
            continue
        if literal is not None:
            if cursor >= len(tokens) or tokens[cursor] != literal:
                return {}
            cursor += 1
            continue
        next_literal = None
        for later_literal, later_kind, _later_name in parts[index + 1 :]:
            if later_literal is not None:
                next_literal = later_literal
                break
            if later_kind == "repeat":
                break
        nxt = _consume_fragment(tokens, cursor, kind or "ident", next_literal)
        if nxt is None:
            return {}
        if name:
            bindings[name] = "".join(tokens[cursor:nxt])
        cursor = nxt
    if cursor != len(tokens):
        return {}
    return bindings


def _repetition_group_inners(matcher_inner: str, invoke_inner: str) -> list[str]:
    parsed = _parse_repetition(matcher_inner)
    if parsed is None:
        return [invoke_inner]
    body, sep, _rep = parsed
    tokens = _token_list(invoke_inner)
    parts = _matcher_parts(body)
    sep_tokens = _token_list(sep)
    cursor = 0
    groups: list[str] = []
    n = len(tokens)
    while cursor < n:
        if groups and sep_tokens:
            width = len(sep_tokens)
            if tokens[cursor : cursor + width] != sep_tokens:
                break
            cursor += width
        start = cursor
        nxt = _matcher_consume(parts, tokens, cursor)
        if nxt is None or nxt == cursor:
            break
        groups.append("".join(tokens[start:nxt]))
        cursor = nxt
    return groups


def _bindings_for_invoke(
    matcher: str, invoke_inner: str, *, rep: int, in_repeat: bool
) -> dict[str, str]:
    inner = _strip_matcher_inner(matcher)
    if in_repeat:
        parsed = _parse_repetition(inner)
        if parsed is not None:
            body, _sep, _rep = parsed
            groups = _repetition_group_inners(inner, invoke_inner)
            if 0 <= rep < len(groups):
                return _bind_inner(body, groups[rep])
            return {}
        named = _matcher_named_parts(inner)
        rep_at = next(
            (i for i, part in enumerate(named) if part[1] == "repeat"),
            None,
        )
        if rep_at is not None:
            tokens = _token_list(invoke_inner)
            cursor = _matcher_consume(named[:rep_at], tokens, 0)
            if cursor is None:
                return {}
            body, sep, _rep = (named[rep_at][2] or "\x1f\x1f*").split("\x1f")
            groups = _repetition_group_inners(
                f"$({body}){sep}*", "".join(tokens[cursor:])
            )
            if 0 <= rep < len(groups):
                return _bind_inner(body, groups[rep])
            return {}
    return _bind_inner(inner, invoke_inner)


_SUB_METAVAR = re.compile(r"\$([A-Za-z_][A-Za-z0-9_]*)")


def _serial_from_attrs(
    attrs: tuple[str, ...] | list[str], bindings: dict[str, str]
) -> tuple[frozenset[str], bool]:
    """`#[serial]` keys after substituting invocation metavars (#516 review)."""

    held: set[str] = set()
    has_unkeyed = False
    for attr in attrs:
        parsed = _serial_keys(_substitute_metavars(attr, bindings))
        if parsed is None:
            continue
        if not parsed:
            has_unkeyed = True
        else:
            held.update(parsed)
    return frozenset(held), has_unkeyed


def _substitute_metavars(text: str, bindings: dict[str, str]) -> str:
    """Replace `$name` in a selected arm with this invocation's captures.

    `$crate` is left alone; `$(` repetitions are not `$ident` (#516 review).
    """

    def repl(match: re.Match[str]) -> str:
        name = match.group(1)
        if name == "crate":
            return match.group(0)
        return bindings.get(name, match.group(0))

    return _SUB_METAVAR.sub(repl, text)


def _arm_accepts(matcher: str, invoke_inner: str, invoke_arity: int) -> bool:
    """True when this `macro_rules!` matcher would accept the invocation.

    Literal arms (`(touch)` vs `(clean)`) compare tokens; metavariable
    arms match remaining literals then consume a fragment (`ident` is one
    token, `expr`/`ty`/`path` run to the next literal); `$(...)*|+|?`
    arms match each repetition against the inner matcher rather than
    accepting any arity. First matching arm wins (#516 review).
    """

    if matcher == "*":
        return True
    inner = matcher.strip()
    if len(inner) >= 2 and inner[0] in "([{" and inner[-1] in ")]}":
        inner = inner[1:-1]
    if "$" not in inner:
        return _token_list(inner) == _token_list(invoke_inner)
    _ = invoke_arity
    parsed = _parse_repetition(inner)
    if parsed is not None:
        return _repetition_matches(*parsed, invoke_inner)
    return _matcher_matches_invoke(inner, invoke_inner)


def _skip_generic_params(source: str, open_index: int) -> int:
    """Skip a `<...>` generic parameter list starting at `open_index`
    (`source[open_index]` must be `<`), respecting nesting (`<A<B>>`) and
    comments/strings.

    Deliberately a SEPARATE function from `_balanced_end`, not an addition
    of `<`/`>` to its `pairs` dict: `<`/`>` are also Rust's comparison
    operators, and `_balanced_end` is called from contexts (general
    balanced-brace scanning) where a stray comparison inside an ordinary
    expression must not be misread as opening a generic. Every current
    caller (`_fn_body` at a function name's immediate next character,
    `_impl_type_name` walking an `impl` head's own type-path segments,
    `_impl_blocks` / `_ufcs_calls` skipping nested `<>` (and const-generic
    `{ 1 }` braces inside them), `_strip_turbofish` at a literal `::<`) is a
    position already known by its own caller's structure to be a generic-list
    opener specifically,
    never an ordinary comparison -- that constraint travels with each call
    site, not with this function.

    Guards one real ambiguity: a `->` arrow inside a trait-bound generic
    (`fn f<T: Fn() -> U>(..)`) contains a `>` that is not a close. Anything
    subtler (a `<=`/`>=` inside a const-generic default expression, for
    instance) is not attempted -- the codebase this was written for and
    checked against has none, measured by this bug's own discovery: it
    hid `with_index<R>` from every function index until fixed.
    """

    depth = 1
    index = open_index + 1
    while index < len(source) and depth:
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
        elif char == "<":
            depth += 1
            index += 1
        elif char == ">":
            if index > 0 and source[index - 1] == "-":
                # `->`, not a generic close.
                index += 1
            else:
                depth -= 1
                index += 1
        else:
            index += 1
    return index


def _strip_turbofish(source: str) -> str:
    """Remove every `::<...>` turbofish (respecting nested generics via
    `_skip_generic_params`, so `bump::<Vec<u8>>()` strips to `bump()`, not
    something truncated at the inner `>`).

    `FREE_CALL`/`QUALIFIED_CALL`/`TYPE_ASSOC_CALL` all require `(`
    immediately (whitespace aside) after the called name; an explicit
    turbofish sits between the two and broke all three. Stripping it
    first, once, is simpler than teaching each regex its own optional
    `(?:::<[^<>]*>)?` -- which would still need `_skip_generic_params`-grade
    nesting awareness to be correct, at three call sites instead of one.

    Only ever called on an already `_code_only`-masked body (see
    `FnInfo.body`'s own doc), so a `::<` appearing inside what was once a
    string or comment cannot occur here -- masking already replaced that
    content before this runs.
    """

    out: list[str] = []
    index = 0
    n = len(source)
    while index < n:
        if source[index : index + 3] == "::<":
            index = _skip_generic_params(source, index + 2)
            continue
        out.append(source[index])
        index += 1
    return "".join(out)


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
    """Collect `#[attr]` blocks immediately before ``position``."""

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
    stripped = attr.strip()
    if TEST_ATTR.match(stripped) is not None:
        return True
    # `#[$attr] fn $name()` — the invocation supplies `test` (#516 review).
    if re.match(r"#\s*\[\s*\$", stripped) is not None:
        return True
    # `#[cfg_attr(test, test)]` / `#[cfg_attr(test, tokio::test)]` is a
    # live test under `cargo test` (#516 review).
    return (
        re.search(
            r"cfg_attr\s*\(\s*test\s*,\s*(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*test\b",
            stripped,
        )
        is not None
    )


def _serial_keys(attr: str) -> tuple[str, ...] | None:
    """`None` = not a `#[serial]` attribute. `()` = unkeyed. Else the keys."""

    match = SERIAL_ATTR.fullmatch(attr.strip())
    if match is None:
        return None
    args = (match.group("args") or "").strip()
    if not args:
        return ()
    return tuple(a.strip().strip('"').strip("'") for a in args.split(",") if a.strip())


def _fn_body(source: str, name_end: int) -> tuple[int, int] | None:
    index = name_end
    while index < len(source) and source[index].isspace():
        index += 1
    if index < len(source) and source[index] == "<":
        index = _skip_generic_params(source, index)
        while index < len(source) and source[index].isspace():
            index += 1
    if index >= len(source) or source[index] != "(":
        return None
    index = _balanced_end(source, index)
    # After the parameter list: skip return types / where-clauses, including
    # const-generic braces inside `<>` (`fn bump() -> impl Trait<{ 1 }> {`)
    # so the first `{` at depth 0 is the actual body (#516 review).
    angle = paren = square = 0
    while index < len(source):
        if source[index].isspace():
            index += 1
            continue
        comment_end = _skip_comment(source, index)
        if comment_end is not None:
            index = comment_end
            continue
        raw_end = _skip_raw_string(source, index)
        if raw_end is not None:
            index = raw_end
            continue
        ch = source[index]
        if ch == '"':
            index = _skip_quoted(source, index, ch)
            continue
        if ch == "'" and (char_end := _skip_char_literal(source, index)) is not None:
            index = char_end
            continue
        if ch == "<":
            angle += 1
            index += 1
            continue
        if ch == ">" and angle:
            angle -= 1
            index += 1
            continue
        if ch == "(":
            paren += 1
            index += 1
            continue
        if ch == ")" and paren:
            paren -= 1
            index += 1
            continue
        if ch == "[":
            square += 1
            index += 1
            continue
        if ch == "]" and square:
            square -= 1
            index += 1
            continue
        if ch == "{":
            if angle == 0 and paren == 0 and square == 0:
                return index, _balanced_end(source, index)
            index = _balanced_end(source, index)
            continue
        if ch == ";" and angle == 0 and paren == 0 and square == 0:
            return None
        index += 1
    return None


def _macro_body(source: str, name_end: int) -> tuple[int, int] | None:
    """The `{ ... }` group of `macro_rules! name { ... }`."""
    index = name_end
    while index < len(source):
        if source[index].isspace():
            index += 1
            continue
        comment_end = _skip_comment(source, index)
        if comment_end is not None:
            index = comment_end
            continue
        if source[index] == "{":
            return index, _balanced_end(source, index)
        return None
    return None


def _skip_ws(source: str, index: int) -> int:
    n = len(source)
    while index < n and source[index].isspace():
        index += 1
    return index


def _read_type_path(source: str, index: int) -> tuple[str | None, int]:
    """One `Path::To::Type<generics>` -- last segment's identifier.

    Generic arguments use `_skip_generic_params`, so `Box<Vec<u8>>` yields
    `Box` and lands after the matching `>`, not the inner `>` (#516).
    """

    n = len(source)
    name = None
    while True:
        index = _skip_ws(source, index)
        match = IDENT.match(source, index)
        if match is None:
            break
        name = match.group(0)
        index = _skip_ws(source, match.end())
        if index < n and source[index] == "<":
            index = _skip_ws(source, _skip_generic_params(source, index))
        if source[index : index + 2] == "::":
            index += 2
            continue
        break
    return name, index


def _impl_type_name(head: str) -> tuple[str | None, str | None]:
    """`(implementing type, trait name or None)` for an `impl` head.

    Trait impls (`impl Trait for Type {`) return the type after `for` and
    the trait so `Trait::method(` can resolve (#516 review). Inherent impls
    return `(Type, None)`.

    The earlier version of this function took `IDENT.findall(head)[-1]` --
    the LAST identifier anywhere in the head -- which happens to equal the
    right answer for `impl Type {` and `impl Trait for Type {` (the type
    name really is the last identifier there), but is wrong the moment any
    generic parameter or argument follows it: `impl<T> Box<T> {` ends in
    the generic parameter `T`, not `Box`. This version walks the head
    structurally instead, skipping `<...>` lists (via
    `_skip_generic_params`, so nested generics like `Box<Vec<T>>` do not
    end the walk early) wherever they appear, rather than trusting
    position alone.

    Named residuals, not attempted: a non-path impl type (`impl Trait for
    &Foo {`, `impl Trait for dyn Foo {`, `impl Trait for (A, B) {`) returns
    `None` and the whole block is skipped -- sound (nothing is
    misattributed), just not tight for those shapes.
    """

    match = re.match(r"\s*impl\b", head)
    if match is None:
        return None, None
    index = match.end()
    n = len(head)
    index = _skip_ws(head, index)
    if index < n and head[index] == "<":
        index = _skip_ws(head, _skip_generic_params(head, index))
    first_name, index = _read_type_path(head, index)
    index = _skip_ws(head, index)
    if head[index : index + 3] == "for" and not (
        index + 3 < n and (head[index + 3].isalnum() or head[index + 3] == "_")
    ):
        second_name, _index = _read_type_path(head, index + 3)
        return second_name, first_name
    return first_name, None


def _impl_blocks(code: str) -> list[tuple[str, str | None, int, int]]:
    """`(type, trait or None, start, end)` for each `impl … { … }`.

    `<...>` lists are skipped before looking for the body brace, so
    `impl Bump<{ 1 }> for S {` does not treat `{ 1 }` as the body
    (#516 review). Nested impls remain visible: search resumes at the
    body `{`, not after the whole block.
    """

    blocks: list[tuple[str, str | None, int, int]] = []
    index = 0
    n = len(code)
    while index < n:
        match = IMPL_KW.search(code, index)
        if match is None:
            break
        head_start = match.start()
        i = match.end()
        body_open: int | None = None
        while i < n:
            comment_end = _skip_comment(code, i)
            if comment_end is not None:
                i = comment_end
                continue
            raw_end = _skip_raw_string(code, i)
            if raw_end is not None:
                i = raw_end
                continue
            char = code[i]
            if char == '"':
                i = _skip_quoted(code, i, char)
                continue
            if char == "'" and (char_end := _skip_char_literal(code, i)) is not None:
                i = char_end
                continue
            if char == "<":
                i = _skip_generic_params(code, i)
                continue
            if char == "{":
                body_open = i
                break
            if char == ";":
                break
            i += 1
        if body_open is None:
            index = match.end()
            continue
        type_name, trait_name = _impl_type_name(code[head_start : body_open + 1])
        if type_name is not None:
            blocks.append(
                (type_name, trait_name, body_open, _balanced_end(code, body_open))
            )
        index = body_open + 1
    return blocks


def _mask_nested_item_bodies(
    code: str,
    body_start: int,
    body_end: int,
    impls: list[tuple[str, str | None, int, int]] | None = None,
) -> str:
    """Replace nested `fn` / `impl` bodies with spaces.

    An unused `fn helper() { COUNTER.fetch_add(...) }` inside a test
    must not be a Stage-1 touch of the enclosing function; nested items
    are indexed separately and propagate only when called (#516 review).
    """

    chars = list(code[body_start:body_end])

    def blank(abs_start: int, abs_end: int) -> None:
        rel_s = max(0, abs_start - body_start)
        rel_e = min(len(chars), abs_end - body_start)
        for i in range(rel_s, rel_e):
            if chars[i] != "\n":
                chars[i] = " "

    for match in FN_DEF.finditer(code, body_start, body_end):
        nested = _fn_body(code, match.end())
        if nested is None:
            continue
        ns, ne = nested
        if ns < body_start or ne > body_end:
            continue
        # Include the signature: `fn helper()` is otherwise a false
        # `helper()` call in the enclosing Stage-1 / call scan.
        blank(match.start(), ne)
    for _type, _trait, impl_open, impl_end in impls or ():
        if impl_open <= body_start or impl_end > body_end:
            continue
        blank(impl_open, impl_end)
    return "".join(chars)


def _lib_crate_idents() -> frozenset[str]:
    """Rust idents for the scanned library (`xai-grok-shell` → both
    hyphen and underscore forms). Integration tests import it as
    `xai_grok_shell::...`, not `crate::` (#516 review)."""

    crate_dir = DEFAULT_SCAN_ROOT.parent.name
    rust = crate_dir.replace("-", "_")
    return frozenset({crate_dir, rust})


def _is_lib_crate_ident(name: str) -> bool:
    rust = name.replace("-", "_")
    return rust in {ident.replace("-", "_") for ident in _lib_crate_idents()}


def _crate_of(path: Path) -> str:
    """Owning crate name for process-group keys.

    Real tree paths are `crates/<family>/<crate>/...`, so `parts[2]` is
    the crate. Short fixture paths (`src/a.rs`) have no such prefix:
    siblings under the same `src/` share one crate so library unit tests
    (and `Type::method` lookup) stay crate-wide (#516 review).
    """

    parts = path.parts
    if len(parts) > 2:
        return parts[2]
    if "src" in parts:
        prefix = parts[: parts.index("src")]
        return "/".join(prefix) if prefix else "."
    if "tests" in parts:
        prefix = parts[: parts.index("tests")]
        return "/".join(prefix) if prefix else "."
    return str(path)


def _norm_posix(path: Path) -> str:
    parts: list[str] = []
    for part in path.as_posix().split("/"):
        if part == "..":
            if parts:
                parts.pop()
        elif part not in (".", ""):
            parts.append(part)
    return "/".join(parts)


_PATH_MOD = re.compile(
    r"#\[\s*path\s*=\s*\"([^\"]+)\"\s*\]\s*"
    r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)"
)
_PATH_OVERRIDE: dict[str, tuple[str, ...]] = {}
_REEXPORTS: list[tuple[tuple[str, ...], str, tuple[str, ...], str]] = []


def _load_path_overrides(sources: list[tuple[Path, str]]) -> None:
    """`#[path = \"foo.rs\"] mod bar;` maps foo.rs to this file's module
    plus `bar`, not foo's filename stem. Nested `#[path]` files inherit
    the already-resolved parent module (#516 review)."""

    global _PATH_OVERRIDE
    _PATH_OVERRIDE = {}
    decls: list[tuple[Path, str, str]] = []
    for rel, text in sources:
        code = _code_only(text)
        for match in _PATH_MOD.finditer(text):
            start = match.start()
            if start < len(code) and code[start] == " " and text[start] != " ":
                continue
            child = _norm_posix(rel.parent / match.group(1))
            decls.append((rel, child, match.group(2)))
    if not decls:
        return
    for _ in range(len(decls) + 1):
        progressed = False
        for declaring, child, mod_name in decls:
            parent = _module_path(declaring)
            if parent is None:
                parent = ()
            new = parent + (mod_name,)
            if _PATH_OVERRIDE.get(child) != new:
                _PATH_OVERRIDE[child] = new
                progressed = True
        if not progressed:
            break


def _load_reexports(sources: list[tuple[Path, str]]) -> None:
    """Index `pub use` so a re-exported static still matches (#516 review)."""

    global _REEXPORTS
    _REEXPORTS = []
    for path, text in sources:
        _REEXPORTS.extend(_pub_reexports(path, text))


def _reexport_reaches(
    module: tuple[str, ...],
    fname: str,
    static_module: tuple[str, ...] | None,
    identifiers: tuple[str, ...],
) -> bool:
    """True when `(module, fname)` is the registered static or pub-uses it."""

    if static_module is None:
        return False
    seen: set[tuple[tuple[str, ...], str]] = set()
    cur_mod, cur_name = module, fname
    while True:
        if cur_mod == static_module and cur_name in identifiers:
            return True
        key = (cur_mod, cur_name)
        if key in seen:
            return False
        seen.add(key)
        nxt: tuple[tuple[str, ...], str] | None = None
        for dest, local, src, src_fname in _REEXPORTS:
            if dest != cur_mod:
                continue
            if local == "*":
                nxt = (src, cur_name)
                break
            if local == cur_name:
                nxt = (src, src_fname)
                break
        if nxt is None:
            return False
        cur_mod, cur_name = nxt


def _module_path_fs(rel: Path) -> tuple[str, ...] | None:
    """`crate::a::b` module path for `.../src/a/b.rs` or `.../src/a/b/mod.rs`.

    Integration support modules (`tests/common/mod.rs`) are `("common",)`
    so `common::helper()` resolves; binary roots (`tests/foo.rs`,
    `tests/foo/main.rs`) are that crate's root and have no extra prefix
    (#516 review).
    """

    parts = rel.parts
    root_key = "src" if "src" in parts else "tests" if "tests" in parts else None
    if root_key is None:
        return None
    segs = list(parts[parts.index(root_key) + 1 :])
    if not segs:
        return None
    if root_key == "src" and segs and segs[0] == "bin":
        rest = segs[1:]
        if len(rest) <= 1:
            return None
        if len(rest) == 2 and rest[1] == "main.rs":
            return None
        nested = list(rest[1:])
        nested[-1] = Path(nested[-1]).stem
        if nested[-1] in ("mod", "main"):
            nested = nested[:-1]
        return tuple(nested)
    if root_key == "tests":
        if len(segs) == 1:
            return None
        if len(segs) == 2 and segs[1] == "main.rs":
            return None
    segs[-1] = Path(segs[-1]).stem
    if segs[-1] in ("mod", "lib", "main"):
        segs = segs[:-1]
    # `()` is the crate root (`src/lib.rs`); do not collapse it to None.
    return tuple(segs)


def _module_path(rel: Path) -> tuple[str, ...] | None:
    found = _PATH_OVERRIDE.get(_norm_posix(rel))
    if found is not None:
        return found
    return _module_path_fs(rel)


def _raw_ident(name: str) -> str:
    return name[2:] if name.startswith("r#") else name


def _skip_ws_code(code: str, i: int) -> int:
    n = len(code)
    while i < n and code[i].isspace():
        i += 1
    return i


def _read_use_ident(code: str, i: int) -> tuple[str | None, int]:
    i = _skip_ws_code(code, i)
    if i + 1 < len(code) and code[i : i + 2] == "r#":
        i += 2
    match = IDENT.match(code, i)
    if match is None:
        return None, i
    return match.group(0), match.end()


def _parse_use_as(code: str, i: int) -> tuple[str | None, int]:
    j = _skip_ws_code(code, i)
    if j + 2 <= len(code) and code[j : j + 2] == "as" and (
        j + 2 == len(code) or not (code[j + 2].isalnum() or code[j + 2] == "_")
    ):
        alias, k = _read_use_ident(code, j + 2)
        if alias is not None:
            return _raw_ident(alias), k
    return None, i


def _collect_use_leaves(
    code: str, i: int, prefix: tuple[str, ...], pos: int, out: list
) -> int:
    """Parse a use-tree node starting at `i`; append `(mid, fname, local)` leaves."""

    i = _skip_ws_code(code, i)
    if i < len(code) and code[i] == "*":
        out.append((prefix, "*", "*"))
        return i + 1
    if i < len(code) and code[i] == "{":
        i += 1
        n = len(code)
        while i < n:
            i = _skip_ws_code(code, i)
            if i < n and code[i] == "}":
                return i + 1
            start = i
            i = _collect_use_leaves(code, i, prefix, pos, out)
            if i == start:
                i += 1
            i = _skip_ws_code(code, i)
            if i < n and code[i] == ",":
                i += 1
        return i
    segs: list[str] = []
    n = len(code)
    while True:
        name, j = _read_use_ident(code, i)
        if name is None:
            break
        segs.append(_raw_ident(name))
        i = _skip_ws_code(code, j)
        if i + 1 < n and code[i : i + 2] == "::":
            i += 2
            i = _skip_ws_code(code, i)
            if i < n and code[i] == "{":
                return _collect_use_leaves(code, i, prefix + tuple(segs), pos, out)
            if i < n and code[i] == "*":
                out.append((prefix + tuple(segs), "*", "*"))
                return i + 1
            continue
        break
    alias, i = _parse_use_as(code, i)
    if segs and segs[0] == "self" and prefix:
        fname = prefix[-1]
        out.append((prefix[:-1], fname, alias or fname))
        return i
    if segs:
        fname = segs[-1]
        out.append((prefix + tuple(segs[:-1]), fname, alias or fname))
    return i


def _iter_use_leaves(
    code: str,
) -> list[tuple[int, str, tuple[str, ...], str, str]]:
    """`(pos, root, mid, fname, local)` for every `use` leaf, nested braces included."""

    leaves: list[tuple[int, str, tuple[str, ...], str, str]] = []
    for match in USE_HEAD.finditer(code):
        root = match.group(1)
        i = _skip_ws_code(code, match.end())
        if i + 1 < len(code) and code[i : i + 2] == "::":
            i += 2
        collected: list[tuple[tuple[str, ...], str, str]] = []
        _collect_use_leaves(code, i, (), match.start(), collected)
        for mid, fname, local in collected:
            leaves.append((match.start(), root, mid, fname, local))
    return leaves


def _is_pub_use(code: str, use_start: int) -> bool:
    return re.search(r"\bpub(?:\s*\([^)]*\))?\s*$", code[:use_start]) is not None


def _use_module_prefix(
    root: str,
    mid: tuple[str, ...],
    file_mod: tuple[str, ...] | None,
    inline_mods: tuple[str, ...] = (),
) -> tuple[str, ...] | None:
    if root == "crate" or _is_lib_crate_ident(root):
        return mid
    if file_mod is None:
        return None
    effective = file_mod + inline_mods
    if root == "self":
        return effective + mid
    if root == "super":
        cur = effective[:-1] if effective else ()
        rest = list(mid)
        while rest and rest[0] == "super":
            cur = cur[:-1] if cur else ()
            rest = rest[1:]
        return tuple(cur) + tuple(rest)
    return None


@dataclass(frozen=True)
class _UseBinding:
    pos: int
    inline: tuple[str, ...]
    local: str
    module: tuple[str, ...]
    fname: str


def _use_bindings(path: Path, text: str) -> list[_UseBinding]:
    """Every `use` binding with its source offset.

    File-wide last-write let `mod second { use … as bump }` steal
    `mod first`'s import (#516 review). Function-local `use` is split
    out later so two tests in one module cannot overwrite each other
    (#516 review).
    """

    code = _code_only(text)
    file_mod = _module_path(path)
    spans = _inline_module_spans(code)
    out: list[_UseBinding] = []

    def record(
        pos: int, root: str, mid: tuple[str, ...], fname: str, local: str
    ) -> None:
        inline = _inline_path_from_spans(spans, pos)
        module = _use_module_prefix(root, mid, file_mod, inline)
        if module is None:
            return
        out.append(_UseBinding(pos, inline, local, module, fname))

    for pos, root, mid, fname, local in _iter_use_leaves(code):
        record(pos, root, mid, fname, local)
    return out


def _pos_in_spans(pos: int, spans: list[tuple[int, int]]) -> bool:
    return any(start <= pos < end for start, end in spans)


def _imports_outside_bodies(
    bindings: list[_UseBinding], body_spans: list[tuple[int, int]]
) -> tuple[
    dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]],
    dict[tuple[str, ...], list[tuple[str, ...]]],
]:
    scoped: dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]] = {}
    globs: dict[tuple[str, ...], list[tuple[str, ...]]] = {}
    for b in bindings:
        if _pos_in_spans(b.pos, body_spans):
            continue
        if b.fname == "*":
            globs.setdefault(b.inline, []).append(b.module)
            continue
        scoped.setdefault(b.inline, {})[b.local] = (b.module, b.fname)
    return scoped, globs


def _block_end_containing(text: str, pos: int) -> int:
    """Innermost `{ ... }` that still contains `pos`."""

    best_start = -1
    best_end = len(text)
    index = 0
    while index < pos:
        if text[index] == "{":
            end = _balanced_end(text, index)
            if index < pos < end and index >= best_start:
                best_start = index
                best_end = end
        index += 1
    return best_end


def _local_uses_from_body(
    body: str,
    file_mod: tuple[str, ...] | None,
    inline_mods: tuple[str, ...],
) -> tuple[tuple[int, int, str, tuple[str, ...], str], ...]:
    """Function-local `use` bindings with the brace block they apply in.

    A nested `{ use crate::b::bump; }` must not rewrite outer `bump()`
    calls (#516 review).
    """

    out: list[tuple[int, int, str, tuple[str, ...], str]] = []
    for pos, root, mid, fname, local in _iter_use_leaves(body):
        if fname == "*":
            continue
        module = _use_module_prefix(root, mid, file_mod, inline_mods)
        if module is None:
            continue
        out.append((pos, _block_end_containing(body, pos), local, module, fname))
    return tuple(out)


def _imports_in_span(
    bindings: list[_UseBinding], start: int, end: int
) -> tuple[dict[str, tuple[tuple[str, ...], str]], list[tuple[str, ...]]]:
    local: dict[str, tuple[tuple[str, ...], str]] = {}
    globs: list[tuple[str, ...]] = []
    for b in bindings:
        if start <= b.pos < end:
            if b.fname == "*":
                globs.append(b.module)
            else:
                local[b.local] = (b.module, b.fname)
    return local, globs


def _globs_in_scope(
    file_globs: dict[tuple[str, ...], list[tuple[str, ...]]],
    inline_mods: tuple[str, ...],
    extra: tuple[tuple[str, ...], ...] = (),
) -> list[tuple[str, ...]]:
    out = list(extra)
    prefix = inline_mods
    while True:
        out.extend(file_globs.get(prefix, []))
        if not prefix:
            break
        prefix = prefix[:-1]
    return out


def _overlay_fn_imports(
    module: dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]],
    local: dict[str, tuple[tuple[str, ...], str]],
    inline_mods: tuple[str, ...],
) -> dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]]:
    merged = {key: dict(value) for key, value in module.items()}
    if local:
        merged.setdefault(inline_mods, {}).update(local)
    return merged


def _pub_reexports(
    path: Path, text: str
) -> list[tuple[tuple[str, ...], str, tuple[str, ...], str]]:
    """`(dest_module, local, src_module, fname)` for `pub use` bindings.

    A re-export is not a physical definition, so `by_module[dest][local]`
    would otherwise miss `crate::b::bump()` when `b.rs` only has
    `pub use crate::a::bump` (#516 review).
    """

    code = _code_only(text)
    file_mod = _module_path(path)
    if file_mod is None:
        return []
    spans = _inline_module_spans(code)
    out: list[tuple[tuple[str, ...], str, tuple[str, ...], str]] = []

    def record(
        pos: int, root: str, mid: tuple[str, ...], fname: str, local: str
    ) -> None:
        inline = _inline_path_from_spans(spans, pos)
        src = _use_module_prefix(root, mid, file_mod, inline)
        if src is None:
            return
        dest = file_mod + inline
        out.append((dest, local, src, fname))

    for pos, root, mid, fname, local in _iter_use_leaves(code):
        if not _is_pub_use(code, pos):
            continue
        record(pos, root, mid, fname, local)
    return out


def _copy_reexports_into_indices(
    reexports: list[tuple[tuple[str, ...], str, tuple[str, ...], str]],
    by_module: dict[tuple[str, ...], dict[str, list[int]]],
    by_leaf: dict[str, dict[str, list[int]]],
) -> None:
    if not reexports:
        return
    for _ in range(len(reexports) + 1):
        progressed = False
        for dest, local, src, fname in reexports:
            if fname == "*":
                exported = list(by_module.get(src, {}).items())
            else:
                exported = [(local, list(by_module.get(src, {}).get(fname, [])))]
            if not exported:
                continue
            progressed_here = False
            for dest_name, js in exported:
                js = list(js)
                if not js:
                    continue
                slot = by_module.setdefault(dest, {}).setdefault(dest_name, [])
                for j in js:
                    if j not in slot:
                        slot.append(j)
                        progressed = True
                        progressed_here = True
                if dest:
                    leaf_slot = by_leaf.setdefault(dest[-1], {}).setdefault(dest_name, [])
                    for j in js:
                        if j not in leaf_slot:
                            leaf_slot.append(j)
                            progressed = True
                            progressed_here = True
            if progressed_here:
                continue
        if not progressed:
            break


def _lookup_import(
    file_imports: dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]],
    inline_mods: tuple[str, ...],
    name: str,
) -> tuple[tuple[str, ...], str] | None:
    prefix = inline_mods
    while True:
        found = file_imports.get(prefix, {}).get(name)
        if found is not None:
            return found
        if not prefix:
            return None
        prefix = prefix[:-1]


def _is_integration_target(path: Path) -> bool:
    parts = path.parts
    return "src" not in parts and len(parts) >= 2 and parts[-2] == "tests"


def _process_group(path: Path) -> str:
    parts = path.parts
    if "tests" in parts and "src" not in parts:
        index = parts.index("tests")
        crate = "/".join(parts[:index])
        if index + 1 < len(parts):
            root = Path(parts[index + 1]).stem
            return f"bin:{crate}/tests/{root}"
        return f"bin:{crate}/tests"
    stem = _src_bin_target_stem(path)
    if stem is not None:
        return f"bin:{_crate_of(path)}/src/bin/{stem}"
    return f"lib:{_crate_of(path)}"


def _src_bin_target_stem(path: Path) -> str | None:
    """Binary target name for `src/bin/tool.rs` / `src/bin/tool/main.rs`."""

    parts = path.parts
    if "src" not in parts:
        return None
    segs = list(parts[parts.index("src") + 1 :])
    if not segs or segs[0] != "bin":
        return None
    rest = segs[1:]
    if not rest:
        return None
    if len(rest) == 1 and rest[0].endswith(".rs"):
        return Path(rest[0]).stem
    return rest[0]


_MOD_DECL = re.compile(r"\bmod\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)\s*;")


def _integration_binary_stem(path: Path) -> str | None:
    """Test-binary name when `path` is that binary's crate root."""

    parts = path.parts
    if "tests" not in parts or "src" in parts:
        return None
    rest = parts[parts.index("tests") + 1 :]
    if len(rest) == 1 and rest[0].endswith(".rs"):
        return Path(rest[0]).stem
    if len(rest) == 2 and rest[1] == "main.rs":
        return rest[0]
    return None


def _path_is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def _path_attr_files(declaring: Path, text: str) -> list[Path]:
    """Files a `#[path = \"...\"] mod` on `declaring` actually compiles."""

    code = _code_only(text)
    out: list[Path] = []
    for match in _PATH_MOD.finditer(text):
        start = match.start()
        if start < len(code) and code[start] == " " and text[start] != " ":
            continue
        out.append(Path(_norm_posix(declaring.parent / match.group(1))))
    return out


def _path_attr_covers(path: Path, target: Path) -> bool:
    if _norm_posix(path) == _norm_posix(target):
        return True
    if target.name == "mod.rs":
        return _path_is_relative_to(path, target.parent)
    if not str(target).endswith(".rs"):
        return _path_is_relative_to(path, target)
    return False


def _file_process_groups(
    sources: list[tuple[Path, str]],
) -> dict[Path, frozenset[str]]:
    """Process groups each file's tests actually run in.

    `mod common;` in `tests/race.rs` compiles `tests/common/**` into the
    `race` binary, not a fictitious `tests/common` process (#516 review).
    `#[path = "shared.rs"] mod support;` does the same for the path
    target (#516 review).
    """

    text_of = dict(sources)
    groups: dict[Path, set[str]] = {path: set() for path, _text in sources}
    binaries: list[tuple[Path, str]] = []
    for path, _text in sources:
        if _integration_binary_stem(path) is None:
            continue
        group = _process_group(path)
        groups[path].add(group)
        binaries.append((path, group))
    for bin_path, group in binaries:
        parent = bin_path.parent
        for name in _MOD_DECL.findall(_code_only(text_of[bin_path])):
            file_mod = parent / f"{name}.rs"
            dir_mod = parent / name
            for path in groups:
                if path == bin_path:
                    continue
                if path == file_mod or _path_is_relative_to(path, dir_mod):
                    groups[path].add(group)
        pending = [bin_path]
        seen: set[str] = set()
        while pending:
            current = pending.pop()
            key = _norm_posix(current)
            if key in seen:
                continue
            seen.add(key)
            text = text_of.get(current)
            if text is None:
                for path, body in text_of.items():
                    if _norm_posix(path) == key:
                        text = body
                        current = path
                        break
            if text is None:
                continue
            for target in _path_attr_files(current, text):
                for path in groups:
                    if path == current:
                        continue
                    if not _path_attr_covers(path, target):
                        continue
                    if group not in groups[path]:
                        groups[path].add(group)
                    pending.append(path)
    out: dict[Path, frozenset[str]] = {}
    for path, _text in sources:
        found = groups.get(path) or set()
        out[path] = frozenset(found) if found else frozenset({_process_group(path)})
    return out


def rust_files(scan_root: Path) -> list[Path]:
    return sorted(path for path in scan_root.rglob("*.rs") if path.is_file())


def collect_sources(
    repo: Path, scan_roots: list[Path]
) -> list[tuple[Path, str]]:
    """Rust files under each scan root, relative to `repo`, first root wins."""

    seen: set[Path] = set()
    sources: list[tuple[Path, str]] = []
    for scan_root in scan_roots:
        if not scan_root.is_dir():
            continue
        for path in rust_files(scan_root):
            rel = path.relative_to(repo)
            if rel in seen:
                continue
            seen.add(rel)
            sources.append((rel, path.read_text(encoding="utf-8")))
    return sources


# --- registry discovery ------------------------------------------------------


@dataclass(frozen=True)
class SharedItem:
    key: str
    file: Path
    identifiers: tuple[str, ...]
    line: int
    inline_mods: tuple[str, ...] = ()


def _keep_only_line_comments(source: str) -> str:
    """Blank strings and block comments; keep `//` line comments and newlines.

    `REGISTRY_MARKER` is line-anchored, so a raw string or `/* */` that
    contains an exact `// SERIAL-GROUP:` line would otherwise register as
    a real group (#516 review). Length and newlines are preserved so
    `_line()` offsets still match the original source.
    """
    chars = list(source)
    i = 0
    n = len(source)

    def blank(start: int, end: int) -> None:
        for j in range(start, min(end, n)):
            if chars[j] != "\n":
                chars[j] = " "

    while i < n:
        raw_end = _skip_raw_string(source, i)
        if raw_end is not None:
            blank(i, raw_end)
            i = raw_end
            continue
        if source[i] == '"':
            end = _skip_quoted(source, i, '"')
            blank(i, end)
            i = end
            continue
        char_end = _skip_char_literal(source, i)
        if char_end is not None:
            blank(i, char_end)
            i = char_end
            continue
        if source.startswith("/*", i):
            end = _skip_comment(source, i) or n
            blank(i, end)
            i = end
            continue
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = n if end < 0 else end
            continue
        i += 1
    return "".join(chars)


def _lines_spanned(text: str, end: int) -> int:
    """How many lines of `text` are occupied by `text[:end]`.

    Maps a byte offset in a `''.join(lines)` remainder back to a count of
    consumed `lines` entries.
    """
    if end <= 0:
        return 0
    prefix = text[:end]
    n = prefix.count("\n")
    if prefix.endswith("\n"):
        return n
    return n + 1


def _static_decl_semicolon_end(source: str) -> int | None:
    """Byte index just past the depth-0 `;` that ends a `static` decl.

    An inner `;` (`LazyLock::new(|| { let x = 1; x })`) does not count
    (#516 review). Comments, strings, and raw strings are skipped the same
    way `_balanced_end` skips them.
    """
    pairs = {"(": ")", "{": "}", "[": "]"}
    stack: list[str] = []
    index = 0
    n = len(source)
    while index < n:
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
            continue
        if char == "'" and (char_end := _skip_char_literal(source, index)) is not None:
            index = char_end
            continue
        if char in pairs:
            stack.append(pairs[char])
            index += 1
            continue
        if stack and char == stack[-1]:
            stack.pop()
            index += 1
            continue
        if char == ";" and not stack:
            return index + 1
        index += 1
    return None


def _consume_static_decl(lines: list[str], start: int) -> tuple[str | None, int]:
    """Read one `static NAME: ...;` possibly split across rustfmt lines.

    Returns `(name, index_after_decl)`. `name` is None when `lines[start]`
    is not a static declaration.
    """
    if start >= len(lines):
        return None, start
    decl = STATIC_DECL.match(lines[start])
    if not decl:
        return None, start
    remainder = "".join(lines[start:])
    end = _static_decl_semicolon_end(remainder)
    if end is None:
        return decl.group("name"), start + 1
    return decl.group("name"), start + max(_lines_spanned(remainder, end), 1)


def _skip_registry_filler(lines: list[str], start: int) -> int | None:
    """Advance past one blank/comment/attr line, or a `/* ... */` comment.

    Returns the next index, or `None` if `lines[start]` ends the block.
    A `#[...]` attribute is consumed as a whole, including rustfmt-wrapped
    `#[cfg(any(\\n ... \\n))]` (#516 review).
    """
    if start >= len(lines):
        return None
    if SKIPPABLE_LINE.match(lines[start]):
        remainder = "".join(lines[start:])
        stripped = lines[start].lstrip()
        if stripped.startswith("#["):
            indent = len(lines[start]) - len(stripped)
            bracket = indent + 1  # the `[` in `#[`
            end = _balanced_end(remainder, bracket)
            return start + max(_lines_spanned(remainder, end), 1)
        return start + 1
    if not _BLOCK_COMMENT_OPEN.match(lines[start]):
        return None
    remainder = "".join(lines[start:])
    stripped = lines[start].lstrip()
    indent = len(lines[start]) - len(stripped)
    end = _skip_comment(remainder, indent)
    if end is None:
        return len(lines)
    return start + max(_lines_spanned(remainder, end), 1)


def find_registry(sources: list[tuple[Path, str]]) -> tuple[list[SharedItem], list[str]]:
    """Every `SERIAL-GROUP` marker, and its claimed `static` block.

    A marker that claims no `static` at all is an error naming the marker's
    own location, not a silently-empty registry item -- see the module
    docstring's "hard error" paragraph.
    """

    items: list[SharedItem] = []
    errors: list[str] = []
    for rel, raw in sources:
        searchable = _keep_only_line_comments(raw)
        for match in REGISTRY_MARKER.finditer(searchable):
            key = match.group("key")
            line = _line(raw, match.start())
            rest = raw[match.end() :].splitlines(keepends=True)
            identifiers: list[str] = []
            i = 0
            while i < len(rest):
                name, next_i = _consume_static_decl(rest, i)
                if name is not None:
                    identifiers.append(name)
                    i = next_i
                    continue
                skip_to = _skip_registry_filler(rest, i)
                if skip_to is not None:
                    i = skip_to
                    continue
                break
            if not identifiers:
                errors.append(
                    f"{rel.as_posix()}:{line}: SERIAL-GROUP({key}) names no "
                    "`static` declaration directly below it -- moved, "
                    "deleted, or misplaced marker"
                )
                continue
            code = _code_only(raw)
            static_pos = match.end()
            first = STATIC_DECL.search(code[match.end() :])
            if first is not None:
                static_pos = match.end() + first.start()
            items.append(
                SharedItem(
                    key=key,
                    file=rel,
                    identifiers=tuple(identifiers),
                    line=line,
                    inline_mods=_inline_path_from_spans(
                        _inline_module_spans(code), static_pos
                    ),
                )
            )
    return items, errors


# --- toucher indexing (Stage 1: direct reference) ----------------------------


@dataclass(frozen=True)
class FnInfo:
    name: str
    file: Path
    type_name: str | None  # set for an `impl Type { fn name }` method
    trait_name: str | None  # set for `impl Trait for Type` methods
    is_macro: bool
    inline_mods: tuple[str, ...]  # inline `mod a { mod b { fn } }` path
    body: str  # code-only, turbofish-stripped -- see `_strip_turbofish`
    start: int
    keys: frozenset[str]  # Stage-1 direct touch, possibly empty
    is_test: bool
    serial_held: frozenset[str]  # keys held by any #[serial(..)] on this fn
    has_unkeyed_serial: bool
    attrs_line: int
    local_imports: dict[str, tuple[tuple[str, ...], str]] = field(
        default_factory=dict
    )
    glob_modules: tuple[tuple[str, ...], ...] = ()
    local_uses: tuple[tuple[int, int, str, tuple[str, ...], str], ...] = ()
    macro_arms: tuple[tuple[str, frozenset[str]], ...] = ()


def _import_from_uses(
    uses: tuple[tuple[int, int, str, tuple[str, ...], str], ...],
    name: str,
    pos: int,
) -> tuple[tuple[str, ...], str] | None:
    hits = [
        entry for entry in uses if entry[2] == name and entry[0] <= pos < entry[1]
    ]
    if not hits:
        return None
    hits.sort(key=lambda entry: (entry[1], -entry[0]))
    return hits[0][3], hits[0][4]


def _fn_import(
    fn: FnInfo,
    name: str,
    call_pos: int,
    imports_by_file: dict[
        Path, dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]]
    ],
) -> tuple[tuple[str, ...], str] | None:
    """Resolve `name` at `call_pos` in `fn.body`, honoring nested blocks."""

    imported = _import_from_uses(fn.local_uses, name, call_pos)
    if imported is not None:
        return imported
    imported = fn.local_imports.get(name)
    if imported is not None:
        return imported
    return _lookup_import(imports_by_file.get(fn.file, {}), fn.inline_mods, name)


def _item_module(item: SharedItem) -> tuple[str, ...] | None:
    """Owning module of a registered static, including inline `mod` path.

    A marker inside `mod tests { static COUNTER }` is `file::tests`, not
    the file module alone; `super::COUNTER` from a nested module must
    compare against that inline path (#516 review).
    """

    file_mod = _module_path(item.file)
    if file_mod is None:
        return None
    return file_mod + item.inline_mods


def _resolve_path_module(
    segs: tuple[str, ...],
    fn_module: tuple[str, ...] | None,
    inline_mods: tuple[str, ...],
) -> tuple[str, ...] | None:
    """Module owning the last ident of a `crate::b::COUNTER`-style path."""

    if not segs:
        return None
    base = (fn_module or ()) + inline_mods
    if segs[0] == "crate" or _is_lib_crate_ident(segs[0]):
        return segs[1:]
    if segs[0] == "self":
        return base + segs[1:]
    if segs[0] == "super":
        cur = base
        rest = segs
        while rest and rest[0] == "super":
            cur = cur[:-1] if cur else ()
            rest = rest[1:]
        return cur + rest
    return base + segs


def _mask_use_items(body: str) -> str:
    """Blank `use ...;` so import paths are not direct touches (#516 review)."""

    chars = list(body)
    index = 0
    n = len(body)
    while index < n:
        match = re.search(r"\buse\b", body[index:])
        if match is None:
            break
        start = index + match.start()
        depth = 0
        cursor = start + 3
        while cursor < n:
            char = body[cursor]
            if char in "{([":
                depth += 1
            elif char in "})]":
                depth = max(0, depth - 1)
            elif char == ";" and depth == 0:
                cursor += 1
                break
            cursor += 1
        for pos in range(start, cursor):
            if chars[pos] != "\n":
                chars[pos] = " "
        index = cursor
    return "".join(chars)


def _body_touches(
    code_only_body: str,
    identifiers: tuple[str, ...],
    *,
    original: tuple[str, ...] | None = None,
    static_module: tuple[str, ...] | None = None,
    fn_module: tuple[str, ...] | None = None,
    inline_mods: tuple[str, ...] = (),
    scoped_imports: dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]]
    | None = None,
    local_uses: tuple[tuple[int, int, str, tuple[str, ...], str], ...] = (),
) -> bool:
    """True if `code_only_body` names this registered static.

    Bare names keep the fail-closed whole-word match. Qualified paths
    (`crate::b::COUNTER`) only match a static whose module is `b`, not a
    same-named `a::COUNTER` (#516 review).
    """

    del original
    code_only_body = _mask_use_items(code_only_body)

    def imported_at(name: str, pos: int) -> tuple[tuple[str, ...], str] | None:
        found = _import_from_uses(local_uses, name, pos)
        if found is not None:
            return found
        if scoped_imports is None:
            return None
        return _lookup_import(scoped_imports, inline_mods, name)

    for ident in identifiers:
        pattern = re.compile(
            rf"(?:((?:(?:r#)?[A-Za-z_][A-Za-z0-9_]*\s*::\s*)+))"
            rf"?(?<![A-Za-z0-9_])(?:r#)?{re.escape(ident)}\b"
        )
        for match in pattern.finditer(code_only_body):
            prefix_raw = match.group(1)
            if prefix_raw:
                segs = tuple(
                    _raw_ident(part)
                    for part in (p.strip() for p in prefix_raw.split("::"))
                    if part.strip()
                )
                imported = imported_at(segs[0], match.start()) if segs else None
                if imported is not None:
                    module, fname = imported
                    resolved = module + (fname,) + segs[1:]
                    if static_module is None or resolved == static_module:
                        return True
                    if _reexport_reaches(
                        resolved, ident, static_module, identifiers
                    ):
                        return True
                    continue
                if static_module is None:
                    return True
                resolved_mod = _resolve_path_module(
                    segs, fn_module, inline_mods
                )
                if resolved_mod == static_module:
                    return True
                if _reexport_reaches(
                    resolved_mod or (), ident, static_module, identifiers
                ):
                    return True
                continue
            imported = imported_at(ident, match.start())
            if imported is not None:
                module, fname = imported
                if static_module is not None and not (
                    module == static_module and fname in identifiers
                ):
                    if not _reexport_reaches(
                        module, fname, static_module, identifiers
                    ):
                        continue
            return True
    return False


def _with_aliases(
    file_imports: dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]],
    inline_mods: tuple[str, ...],
    identifiers: tuple[str, ...],
    static_module: tuple[str, ...] | None = None,
) -> tuple[str, ...]:
    """Registered identifiers plus in-scope `use … as` aliases of them.

    The alias must resolve to this registered static's module, not some
    other item that happens to share the identifier (#516 review).
    """

    known = set(identifiers)
    extra: list[str] = []
    prefix = inline_mods
    while True:
        for local, (module, fname) in file_imports.get(prefix, {}).items():
            if local in known:
                continue
            if static_module is None:
                if fname not in known:
                    continue
            elif module == static_module and fname in identifiers:
                pass
            elif _reexport_reaches(module, fname, static_module, identifiers):
                pass
            else:
                continue
            extra.append(local)
        if not prefix:
            break
        prefix = prefix[:-1]
    if not extra:
        return identifiers
    return identifiers + tuple(extra)


def _inline_module_spans(code: str) -> list[tuple[int, int, int, str]]:
    """`(mod_start, open_index, end, name)` for each inline `mod name { ... }`."""

    spans: list[tuple[int, int, int, str]] = []
    for match in INLINE_MOD.finditer(code):
        open_index = match.end() - 1
        end = _balanced_end(code, open_index)
        spans.append((match.start(), open_index, end, match.group(1)))
    return spans


def _inline_path_from_spans(
    spans: list[tuple[int, int, int, str]], pos: int
) -> tuple[str, ...]:
    hits = [
        (start, name)
        for start, open_index, end, name in spans
        if start < pos and open_index <= pos < end
    ]
    hits.sort()
    return tuple(name for _, name in hits)


def _inline_path_at(code: str, pos: int) -> tuple[str, ...]:
    """Inline `mod name { ... }` modules whose body still contains `pos`."""

    return _inline_path_from_spans(_inline_module_spans(code), pos)


def _macro_generates_tests(body: str) -> bool:
    """`#[test] fn $name()` / `#[test] fn generated()` inside `macro_rules!`."""

    return bool(_generated_test_templates(body))


def _serial_from_macro_body(body: str) -> tuple[frozenset[str], bool]:
    """`#[serial(...)]` the macro emits next to the generated `#[test]`."""

    held: set[str] = set()
    has_unkeyed = False
    for match in SERIAL_ATTR.finditer(body):
        parsed = _serial_keys(match.group(0))
        if parsed is None:
            continue
        if not parsed:
            has_unkeyed = True
        else:
            held.update(parsed)
    return frozenset(held), has_unkeyed


def _inside_dollar_repeat(source: str, pos: int) -> bool:
    """True when ``pos`` sits inside `$ ( ... ) *|+`.

    That is the expansion pattern that repeats per invocation item;
    argument count is multiplicity only then (#516 review).
    """

    i = 0
    n = len(source)
    while i < n:
        dollar = source.find("$", i)
        if dollar < 0 or dollar >= pos:
            return False
        j = dollar + 1
        while j < n and source[j].isspace():
            j += 1
        if j >= n or source[j] != "(":
            i = dollar + 1
            continue
        end = _balanced_end(source, j)
        if j < pos < end:
            k = end
            while k < n and source[k].isspace():
                k += 1
            if k < n and source[k] in "*+":
                return True
        i = dollar + 1
    return False


_METAVAR_CALL = re.compile(r"\$([A-Za-z_][A-Za-z0-9_]*)\s*\(")


def _helper_refs(fn_body: str, helpers: dict[str, str]) -> list[str]:
    names = list(_METAVAR_CALL.findall(fn_body))
    for name in helpers:
        if re.search(rf"(?<![:.\w]){re.escape(name)}\s*\(", fn_body):
            names.append(name)
    return names


def _expand_generated_helper_bodies(fn_body: str, helpers: dict[str, str]) -> str:
    """Inline `$helper()` / `helper()` bodies, including nested helpers."""

    seen: set[str] = set()
    extra: list[str] = []
    stack = _helper_refs(fn_body, helpers)
    while stack:
        name = stack.pop()
        if name in seen or name not in helpers:
            continue
        seen.add(name)
        extra.append(helpers[name])
        stack.extend(_helper_refs(helpers[name], helpers))
    return fn_body + "".join(extra)


def _macro_rule_arms(body: str) -> list[tuple[str, str]]:
    """`(matcher, arm_body)` for each `macro_rules!` match arm.

    Unparseable bodies yield a single catch-all matcher so one-arm macros
    keep working. Matcher text is what `_arm_accepts` compares against an
    invocation (#516 review).
    """

    code = _code_only(body)
    i = 0
    n = len(code)
    while i < n and code[i].isspace():
        i += 1
    if i < n and code[i] == "{":
        end = _balanced_end(code, i)
        code = code[i + 1 : end - 1]
        i = 0
        n = len(code)
    arms: list[tuple[str, str]] = []
    while i < n:
        while i < n and code[i].isspace():
            i += 1
        if i >= n:
            break
        if code[i] not in "([{":
            return [("*", body)] if not arms else arms
        matcher_end = _balanced_end(code, i)
        matcher = code[i:matcher_end]
        j = matcher_end
        while j < n and code[j].isspace():
            j += 1
        if j + 1 >= n or code[j : j + 2] != "=>":
            return [("*", body)] if not arms else arms
        j += 2
        while j < n and code[j].isspace():
            j += 1
        if j >= n or code[j] not in "{([":
            return [("*", body)] if not arms else arms
        end = _balanced_end(code, j)
        inner_start = j + 1
        inner_end = end - 1 if end > inner_start else end
        arms.append((matcher, code[inner_start:inner_end]))
        i = end
        while i < n and code[i].isspace():
            i += 1
        if i < n and code[i] == ";":
            i += 1
    return arms or [("*", body)]


def _generated_test_templates_in_arm(
    arm: str,
) -> list[tuple[frozenset[str], bool, bool, str, tuple[str, ...]]]:
    parsed: list[tuple[str, bool, frozenset[str], bool, bool, str, tuple[str, ...]]] = []
    code = _code_only(arm)
    for match in MACRO_TEST_FN.finditer(code):
        attrs = _preceding_attributes(arm, code, match.start())
        is_test = any(_is_test_attr(a) for a in attrs)
        held: set[str] = set()
        has_unkeyed = False
        for attr in attrs:
            parsed_keys = _serial_keys(attr)
            if parsed_keys is None:
                continue
            if not parsed_keys:
                has_unkeyed = True
            else:
                held.update(parsed_keys)
        metavar = match.group(1)
        span = _fn_body(code, match.end())
        fn_body = _strip_turbofish(code[span[0] : span[1]]) if span else ""
        parsed.append(
            (
                metavar,
                is_test,
                frozenset(held),
                has_unkeyed,
                _inside_dollar_repeat(arm, match.start()),
                fn_body,
                tuple(attrs),
            )
        )
    helpers = {
        metavar: fn_body
        for metavar, is_test, _held, _unkeyed, _rep, fn_body, _attrs in parsed
        if not is_test
    }
    out: list[tuple[frozenset[str], bool, bool, str, tuple[str, ...]]] = []
    for _metavar, is_test, held, has_unkeyed, in_repeat, fn_body, attrs in parsed:
        if not is_test:
            continue
        out.append(
            (
                held,
                has_unkeyed,
                in_repeat,
                _expand_generated_helper_bodies(fn_body, helpers),
                attrs,
            )
        )
    return out


def _generated_test_templates(
    body: str,
) -> list[tuple[frozenset[str], bool, bool, str, tuple[str, ...]]]:
    """Serial attrs, repetition, and body for each generated `fn $name`.

    Keys later come from that body (and its callees), not the macro-wide
    union (#516 review). A generated test that calls `$helper()` still
    inherits that helper's body, because the helper is not a real `fn`.
    Nested `$relay()` -> `$leaf()` calls are expanded transitively.
    Helpers are per match arm so a later arm cannot replace an earlier
    arm's same-named helper (#516 review).
    """

    out: list[tuple[frozenset[str], bool, bool, str, tuple[str, ...]]] = []
    for _matcher, arm in _macro_rule_arms(body):
        out.extend(_generated_test_templates_in_arm(arm))
    return out


def _generated_test_serials(body: str) -> list[tuple[frozenset[str], bool]]:
    return [
        (held, unkeyed)
        for held, unkeyed, _rep, _body, _attrs in _generated_test_templates(body)
    ]


@dataclass(frozen=True)
class _PendingMacroTest:
    file: Path
    macro_name: str
    macro_file: Path
    line: int
    start: int
    inline_mods: tuple[str, ...]
    serial_held: frozenset[str]
    has_unkeyed_serial: bool
    slot: int
    template_index: int
    body: str = ""


@dataclass
class _MacroArm:
    matcher: str
    serials: list[tuple[frozenset[str], bool, bool, int, tuple[str, ...]]]


def _serials_for_macro_invoke(
    *,
    generated_by_macro: dict[tuple[Path, str], list[_MacroArm]],
    exported_macros: dict[str, Path],
    file_by_module: dict[tuple[str, ...], Path],
    rel: Path,
    macro_name: str,
    invoke_text: str,
    file_imports: dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]],
    inline_mods: tuple[str, ...],
) -> tuple[list[_MacroArm], Path] | None:
    """Arms for this invocation, bound to the defining file (#516 review)."""

    full = invoke_text.strip()
    if full.endswith("!"):
        full = full[:-1]
    segs = tuple(s.strip() for s in full.split("::") if s.strip())
    if len(segs) >= 2 and segs[0] == "crate":
        module = segs[1:-1]
        fname = segs[-1]
        def_file = file_by_module.get(module)
        if def_file is None:
            return None
        arms = generated_by_macro.get((def_file, fname))
        if arms is None:
            return None
        return arms, def_file
    arms = generated_by_macro.get((rel, macro_name))
    if arms is not None:
        return arms, rel
    imported = _lookup_import(file_imports, inline_mods, macro_name)
    if imported is not None:
        module, fname = imported
        def_file = file_by_module.get(module)
        if def_file is not None:
            found = generated_by_macro.get((def_file, fname))
            if found is not None:
                return found, def_file
    export_file = exported_macros.get(macro_name)
    if export_file is not None:
        found = generated_by_macro.get((export_file, macro_name))
        if found is not None:
            return found, export_file
    return None


def index_functions(
    sources: list[tuple[Path, str]], registry: list[SharedItem]
) -> tuple[
    list[FnInfo],
    list[_PendingMacroTest],
    dict[Path, dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]]],
    dict[Path, dict[tuple[str, ...], list[tuple[str, ...]]]],
]:
    _load_path_overrides(sources)
    _load_reexports(sources)
    out: list[FnInfo] = []
    pending: list[_PendingMacroTest] = []
    generated_by_macro: dict[tuple[Path, str], list[_MacroArm]] = {}
    exported_macros: dict[str, Path] = {}
    file_by_module: dict[tuple[str, ...], Path] = {}
    for rel, _raw in sources:
        module = _module_path(rel)
        if module is not None:
            file_by_module[module] = rel
    imports_by_file: dict[
        Path, dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]]
    ] = {}
    globs_by_file: dict[Path, dict[tuple[str, ...], list[tuple[str, ...]]]] = {}
    scans: list[
        tuple[Path, str, str, list[tuple[int, int]], list[tuple[int, int, int, str]]]
    ] = []
    for rel, raw in sources:
        code = _code_only(raw)
        impls = _impl_blocks(code)
        inline_spans = _inline_module_spans(code)
        bindings = _use_bindings(rel, raw)
        occupied: list[tuple[int, int]] = []
        pending_fns: list[
            tuple[re.Match[str], str, int, int, str, tuple[str, ...]]
        ] = []
        macro_bodies: list[tuple[int, int]] = []
        for macro_match in MACRO_DEF.finditer(code):
            body_span = _macro_body(raw, macro_match.end())
            if body_span is not None:
                macro_bodies.append(body_span)
        for match in FN_DEF.finditer(code):
            if any(start <= match.start() < end for start, end in macro_bodies):
                continue
            name = match.group("name")
            body_span = _fn_body(raw, match.end())
            if body_span is None:
                continue
            body_start, body_end = body_span
            occupied.append((body_start, body_end))
            body_code = _strip_turbofish(
                _mask_nested_item_bodies(code, body_start, body_end, impls)
            )
            inline_mods = _inline_path_from_spans(inline_spans, match.start())
            pending_fns.append(
                (match, name, body_start, body_end, body_code, inline_mods)
            )
        for match in MACRO_DEF.finditer(code):
            body_span = _macro_body(raw, match.end())
            if body_span is not None:
                occupied.append(body_span)
        file_imports, file_globs = _imports_outside_bodies(bindings, occupied)
        imports_by_file[rel] = file_imports
        globs_by_file[rel] = file_globs
        for match, name, body_start, body_end, body_code, inline_mods in pending_fns:
            local, local_globs = _imports_in_span(bindings, body_start, body_end)
            local_uses = _local_uses_from_body(
                body_code, _module_path(rel), inline_mods
            )
            keys = frozenset(
                item.key
                for item in registry
                if _body_touches(
                    body_code,
                    _with_aliases(
                        file_imports,
                        inline_mods,
                        item.identifiers,
                        static_module=_item_module(item),
                    )
                    + tuple(
                        {
                            use[2]
                            for use in local_uses
                            if use[4] in item.identifiers
                        }
                    ),
                    original=item.identifiers,
                    static_module=_item_module(item),
                    fn_module=_module_path(rel),
                    inline_mods=inline_mods,
                    scoped_imports=file_imports,
                    local_uses=local_uses,
                )
            )
            type_name = None
            trait_name = None
            for tname, trname, istart, iend in impls:
                if istart <= match.start() < iend:
                    type_name = tname
                    trait_name = trname
                    break
            attrs = _preceding_attributes(raw, code, match.start())
            is_test = any(_is_test_attr(a) for a in attrs)
            serial_held: set[str] = set()
            has_unkeyed = False
            for a in attrs:
                parsed = _serial_keys(a)
                if parsed is None:
                    continue
                if not parsed:
                    has_unkeyed = True
                else:
                    serial_held.update(parsed)
            out.append(
                FnInfo(
                    name=name,
                    file=rel,
                    type_name=type_name,
                    trait_name=trait_name,
                    is_macro=False,
                    inline_mods=inline_mods,
                    body=body_code,
                    start=body_start,
                    keys=keys,
                    is_test=is_test,
                    serial_held=frozenset(serial_held),
                    has_unkeyed_serial=has_unkeyed,
                    attrs_line=_line(raw, match.start()),
                    local_imports=local,
                    glob_modules=tuple(local_globs),
                    local_uses=local_uses,
                )
            )
        for match in MACRO_DEF.finditer(code):
            name = match.group("name")
            body_span = _macro_body(raw, match.end())
            if body_span is None:
                continue
            body_start, body_end = body_span
            occupied.append((body_start, body_end))
            body_code = _strip_turbofish(code[body_start:body_end])
            macro_attrs = _preceding_attributes(raw, code, match.start())
            is_export = any("macro_export" in a for a in macro_attrs)
            keys = frozenset(
                item.key
                for item in registry
                if _body_touches(
                    body_code,
                    item.identifiers,
                    original=item.identifiers,
                    static_module=_item_module(item),
                    fn_module=_module_path(rel),
                    scoped_imports=file_imports,
                )
            )
            raw_macro_body = raw[body_start:body_end]
            arm_list: list[_MacroArm] = []
            arm_key_pairs: list[tuple[str, frozenset[str]]] = []
            for matcher, arm_body in _macro_rule_arms(raw_macro_body):
                arm_code = _strip_turbofish(_code_only(arm_body))
                arm_key_pairs.append(
                    (
                        matcher,
                        frozenset(
                            item.key
                            for item in registry
                            if _body_touches(
                                arm_code,
                                item.identifiers,
                                original=item.identifiers,
                                static_module=_item_module(item),
                                fn_module=_module_path(rel),
                                scoped_imports=file_imports,
                            )
                        ),
                    )
                )
                serials_for_arm: list[
                    tuple[frozenset[str], bool, bool, int, tuple[str, ...]]
                ] = []
                for held, unkeyed, in_repeat, test_body, attrs in _generated_test_templates_in_arm(
                    arm_body
                ):
                    template_keys = frozenset(
                        item.key
                        for item in registry
                        if _body_touches(
                            test_body,
                            item.identifiers,
                            original=item.identifiers,
                            static_module=_item_module(item),
                            fn_module=_module_path(rel),
                            scoped_imports=file_imports,
                        )
                    )
                    template_index = len(out)
                    out.append(
                        FnInfo(
                            name=f"{name}#template{template_index}",
                            file=rel,
                            type_name=None,
                            trait_name=None,
                            is_macro=False,
                            inline_mods=(),
                            body=test_body,
                            start=body_start,
                            keys=template_keys,
                            is_test=False,
                            serial_held=held,
                            has_unkeyed_serial=unkeyed,
                            attrs_line=_line(raw, match.start()),
                        )
                    )
                    serials_for_arm.append(
                        (held, unkeyed, in_repeat, template_index, attrs)
                    )
                arm_list.append(_MacroArm(matcher, serials_for_arm))
            if any(arm.serials for arm in arm_list):
                generated_by_macro[(rel, name)] = arm_list
            if is_export:
                exported_macros[name] = rel
            out.append(
                FnInfo(
                    name=name,
                    file=rel,
                    type_name=None,
                    trait_name=None,
                    is_macro=True,
                    inline_mods=(),
                    body=body_code,
                    start=body_start,
                    keys=keys,
                    is_test=False,
                    serial_held=frozenset(),
                    has_unkeyed_serial=False,
                    attrs_line=_line(raw, match.start()),
                    macro_arms=tuple(arm_key_pairs),
                )
            )
        scans.append((rel, raw, code, occupied, inline_spans))
    for rel, raw, code, occupied, inline_spans in scans:
        file_imports = imports_by_file.get(rel, {})
        for invoke in MACRO_INVOKE.finditer(code):
            macro_name = invoke.group(1)
            if any(start <= invoke.start() < end for start, end in occupied):
                continue
            inline_mods = _inline_path_from_spans(inline_spans, invoke.start())
            resolved = _serials_for_macro_invoke(
                generated_by_macro=generated_by_macro,
                exported_macros=exported_macros,
                file_by_module=file_by_module,
                rel=rel,
                macro_name=macro_name,
                invoke_text=invoke.group(0),
                file_imports=file_imports,
                inline_mods=inline_mods,
            )
            if resolved is None:
                continue
            arms, macro_file = resolved
            line = _line(raw, invoke.start())
            arity = _macro_invoke_arity(code, invoke.end())
            inner = _macro_invoke_inner(code, invoke.end())
            chosen: _MacroArm | None = None
            for arm in arms:
                if _arm_accepts(arm.matcher, inner, arity):
                    chosen = arm
                    break
            if chosen is None:
                continue
            serials = chosen.serials
            for slot, (_serial_held, _has_unkeyed, in_repeat, template_index, attrs) in enumerate(
                serials
            ):
                reps = (
                    _invoke_repeat_count(chosen.matcher, inner)
                    if in_repeat
                    else 1
                )
                if reps < 1:
                    continue
                template_body = (
                    out[template_index].body
                    if 0 <= template_index < len(out)
                    else ""
                )
                for rep in range(reps):
                    bindings = _bindings_for_invoke(
                        chosen.matcher, inner, rep=rep, in_repeat=in_repeat
                    )
                    held, unkeyed = _serial_from_attrs(attrs, bindings)
                    pending.append(
                        _PendingMacroTest(
                            file=rel,
                            macro_name=macro_name,
                            macro_file=macro_file,
                            line=line,
                            start=invoke.start(),
                            inline_mods=inline_mods,
                            serial_held=held,
                            has_unkeyed_serial=unkeyed,
                            slot=rep * len(serials) + slot,
                            template_index=template_index,
                            body=_substitute_metavars(template_body, bindings),
                        )
                    )
    return out, pending, imports_by_file, globs_by_file


# --- membership: a monotonic fixpoint over the call graph -------------------
#
# Not "one hop": measured against the real motivating case (#475/#492,
# `search_cache_epoch`, dry-run calibrated below) and found to need more.
# `heal_quarantines_only_on_confirmed_corruption` reaches `CACHE_EPOCH` through
# TWO same-file calls (test -> `quarantined_after` -> `heal_unusable`);
# `test_claimant_reindexes_even_when_marker_exists` reaches it through several
# hops of production orchestration code the test never names directly. A fixed
# hop count is either too shallow for that (misses real touchers -- silently,
# which is the one failure mode this guard exists to not have) or an arbitrary
# number chosen to fit today's deepest chain and wrong again at the next one.
#
# Instead: every function starts with its Stage-1 (direct-reference) keys,
# and a fixed point is computed by repeatedly re-scanning every function's
# body for CALLS into a function that already has keys, unioning those keys
# in. Because keys only ever get ADDED (never removed) and the key universe
# is finite, this is monotonic on a finite lattice and provably terminates;
# `_MAX_ROUNDS` below is a generous safety bound, not the actual termination
# argument. Exceeding it is a hard error (#516 review): a truncated
# closure must not report 0 violations. A call not shaped like one of the
# four resolved forms below
# breaks the chain at that point -- silently, same as it would for
# `check_envguard_serial.py`'s own one-hop resolution -- see WHAT THIS DOES
# NOT CHECK in the module docstring.
#
# Four call shapes are resolved, all requiring `name(` -- never a bare
# mention without a call, per the module docstring's measured false-positive
# finding:
#   1. `name(`                    -- same file, then a `use` import of that
#                                     name (`use crate::a::bump; bump()`)
#   2. `crate::a::b::name(`       -- resolved by full module path
#   3. `some_mod::name(`          -- resolved by the LAST path segment
#                                     against every file's own module leaf
#                                     (its filename stem) -- the shape a
#                                     sibling module is actually called by
#                                     in this tree (`search_recovery::
#                                     heal_unusable(`), and deliberately
#                                     imprecise: a leaf name is not scoped
#                                     to which `use` brought it in, so a
#                                     crate with two same-named file stems
#                                     would over-resolve. Measured: none in
#                                     `xai-grok-shell` do (`rust_files`
#                                     over the scan root, grouped by stem).
#   4. `Type::assoc_fn(`          -- resolved crate-wide by type name against
#                                     every `impl Type { .. }` block, so a
#                                     call written as `mod_path::Type::fn()`
#                                     resolves on the `Type::fn(` suffix
#                                     alone, regardless of the qualifying
#                                     path in front of it. An INSTANCE method
#                                     call (`value.method()`) is a fifth
#                                     shape this does not attempt: knowing
#                                     `value`'s type without a real type
#                                     checker is not cheap, and neither
#                                     sibling script does it. Measured not to
#                                     block the dry run below: every chain
#                                     that needed to cross this had an
#                                     associated-fn call (`CacheEpoch::now()`)
#                                     on the same path as the unresolved
#                                     instance call (`.changed()`), so the
#                                     other call in the chain still connects.

_MAX_ROUNDS = 64


@dataclass(frozen=True)
class Finding:
    path: Path
    line: int
    name: str
    key: str
    reason: str


def _ufcs_calls(body: str) -> list[tuple[str, str]]:
    """`<Type as Trait>::method(` and `<Type>::method(`.

    Resolves like TYPE_ASSOC_CALL: last segment of the type path against
    `by_type` (#516 review). `<Type>::method()` is valid Rust and is
    not rejected for lacking `as Trait` (#516 review).
    """

    out: list[tuple[str, str]] = []
    index = 0
    n = len(body)
    while index < n:
        lt = body.find("<", index)
        if lt < 0:
            break
        i = lt + 1
        type_name, i = _read_type_path(body, i)
        i = _skip_ws(body, i)
        if type_name is None:
            index = lt + 1
            continue
        if body.startswith("as", i):
            after_as = i + 2
            if after_as < n and (body[after_as].isalnum() or body[after_as] == "_"):
                index = lt + 1
                continue
            i = _skip_ws(body, after_as)
            _trait, i = _read_type_path(body, i)
            i = _skip_ws(body, i)
        if i >= n or body[i] != ">":
            index = lt + 1
            continue
        i = _skip_ws(body, i + 1)
        if body[i : i + 2] != "::":
            index = lt + 1
            continue
        i = _skip_ws(body, i + 2)
        method = IDENT.match(body, i)
        if method is None:
            index = lt + 1
            continue
        i = _skip_ws(body, method.end())
        if i < n and body[i] == "(":
            out.append((type_name, method.group(0)))
        index = lt + 1
    return out


def _gain_from(
    gained: set[str],
    keys_of: list[frozenset[str]],
    self_index: int,
    slot: list[int] | None,
) -> None:
    if not slot:
        return
    for j in slot:
        if j != self_index:
            gained.update(keys_of[j])


def _resolve_calls(
    fn: FnInfo,
    *,
    by_file: dict[Path, dict[str, list[int]]],
    by_module: dict[tuple[str, ...], dict[str, list[int]]],
    by_leaf: dict[str, dict[str, list[int]]],
    by_type: dict[tuple[str, str], dict[str, list[int]]],
    by_macro: dict[Path, dict[str, int]],
    by_macro_any: dict[str, list[int]],
    by_macro_arms: dict[Path, dict[str, tuple[tuple[str, frozenset[str]], ...]]],
    by_inline: dict[tuple[Path, tuple[str, ...]], dict[str, list[int]]],
    imports_by_file: dict[
        Path, dict[tuple[str, ...], dict[str, tuple[tuple[str, ...], str]]]
    ],
    globs_by_file: dict[Path, dict[tuple[str, ...], list[tuple[str, ...]]]],
    keys_of: list[frozenset[str]],
    self_index: int,
    by_crate_module: dict[tuple[str, tuple[str, ...]], dict[str, list[int]]] | None = None,
    file_groups: dict[Path, frozenset[str]] | None = None,
) -> frozenset[str]:
    gained: set[str] = set()
    file_index = by_file.get(fn.file, {})
    caller_module = _module_path(fn.file)
    crate_ns = by_crate_module or {}
    groups_of = file_groups or {}
    for m in FREE_CALL.finditer(fn.body):
        # Resolve a bare call inside the caller's inline module first, then
        # ancestors. File-wide last-definition lookup lets `mod b { fn bump }`
        # steal `mod a { bump() }` (#516 review). Same-scope cfg twins are
        # all kept; callers union their keys (#516 review).
        name = m.group(1)
        js: list[int] = []
        imported = _fn_import(fn, name, m.start(), imports_by_file)
        if imported is not None:
            module, fname = imported
            js = by_module.get(module, {}).get(fname, [])
        if not js:
            prefix = fn.inline_mods
            while True:
                js = by_inline.get((fn.file, prefix), {}).get(name, [])
                if js:
                    break
                if not prefix:
                    break
                prefix = prefix[:-1]
        if not js:
            for module in _globs_in_scope(
                globs_by_file.get(fn.file, {}),
                fn.inline_mods,
                fn.glob_modules,
            ):
                js.extend(by_module.get(module, {}).get(name, []))
        _gain_from(gained, keys_of, self_index, js)
    macro_index = by_macro.get(fn.file, {})
    file_arms = by_macro_arms.get(fn.file, {})
    for m in MACRO_INVOKE.finditer(fn.body):
        name = m.group(1)
        imported = _fn_import(fn, name, m.start(), imports_by_file)
        resolved = imported[1] if imported is not None else name
        arms = file_arms.get(name) or file_arms.get(resolved)
        if arms:
            inner = _macro_invoke_inner(fn.body, m.end())
            arity = _macro_invoke_arity(fn.body, m.end())
            chosen: frozenset[str] | None = None
            for matcher, arm_keys in arms:
                if _arm_accepts(matcher, inner, arity):
                    chosen = arm_keys
                    break
            if chosen is not None:
                gained.update(chosen)
                continue
        j = macro_index.get(name)
        if j is None:
            j = macro_index.get(resolved)
        if j is None:
            _gain_from(
                gained,
                keys_of,
                self_index,
                by_macro_any.get(resolved, []) or by_macro_any.get(name, []),
            )
            continue
        if j != self_index:
            gained.update(keys_of[j])
    for m in QUALIFIED_CALL.finditer(fn.body):
        segs = tuple(s.strip() for s in m.group(1).split("::") if s.strip())
        if segs and segs[0] == "crate":
            caller_group = _process_group(fn.file)
            if caller_group.startswith("lib:"):
                _gain_from(
                    gained,
                    keys_of,
                    self_index,
                    by_module.get(segs[1:], {}).get(m.group(2), []),
                )
            else:
                # Integration/`src/bin` `crate::` is that target, not the
                # library's `by_module[()]` (#516 review).
                for group in groups_of.get(fn.file, frozenset({caller_group})):
                    _gain_from(
                        gained,
                        keys_of,
                        self_index,
                        crate_ns.get((group, segs[1:]), {}).get(m.group(2), []),
                    )
                if not segs[1:]:
                    _gain_from(
                        gained, keys_of, self_index, file_index.get(m.group(2), [])
                    )
        elif segs and _is_lib_crate_ident(segs[0]):
            _gain_from(
                gained,
                keys_of,
                self_index,
                by_module.get(segs[1:], {}).get(m.group(2), []),
            )
        elif segs and segs[0] in ("self", "super"):
            # `self::[...]name(` / `super::[...]name(`, with zero or more
            # ADDITIONAL segments after the relative prefix -- measured,
            # not assumed: `super::sibling_mod::name(...)` (a leading
            # `super` then a NAMED sibling module) occurs 129 times in
            # this crate, `super::super::name(...)` (two `super`s, no
            # trailing module) 15 times. A fix that only handled a bare
            # `self`/`super` with nothing after it, however many levels,
            # would silently miss both of those real shapes.
            #
            # Each leading `self` contributes 0 levels of ascent (stays
            # at "the current module"); each leading `super` contributes
            # 1. Whatever segments remain after that prefix (`sibling_mod`
            # above) are a module path relative to the ascended module.
            ascend = 0
            index = 0
            while index < len(segs) and segs[index] in ("self", "super"):
                if segs[index] == "super":
                    ascend += 1
                index += 1
            trailing = segs[index:]
            if ascend == 0 and not trailing:
                # `self::name(` alone: "the current module" is exactly
                # "the current file" under this checker's file-based
                # model, the same lookup a bare unqualified call already
                # uses -- and, unlike every other case below, this one
                # needs no `_module_path` at all, so it still works for a
                # file with no `src` component (this checker's own test
                # fixtures, and in principle any Rust file laid out
                # differently than this repo's own crates are).
                _gain_from(
                    gained,
                    keys_of,
                    self_index,
                    file_index.get(m.group(2), []),
                )
            elif caller_module is not None:
                # Ambiguity this checker cannot resolve, named rather than
                # guessed at: a `super` prefix's ascent count conflates
                # two things a file-based module model cannot tell apart
                # -- nesting INSIDE the same file (an inline `mod tests {
                # use super::*; ... }` block, invisible to
                # `_module_path`) and a REAL parent-directory module
                # (visible). Sound over tight: try every ascent from 0
                # (every `super` was inline nesting, so this stays at the
                # same file) up to `ascend` (every `super` was a real
                # parent), and union all of them, rather than picking one
                # and risking a silent miss on the other.
                for levels in range(ascend + 1):
                    if levels > len(caller_module):
                        break
                    base = caller_module[: len(caller_module) - levels]
                    _gain_from(
                        gained,
                        keys_of,
                        self_index,
                        by_module.get(base + trailing, {}).get(m.group(2), []),
                    )
        if segs and segs[0] not in ("crate", "self", "super"):
            # `use crate::a as h; h::bump()` — `h` is not a filename leaf
            # (#516 review).
            imported = _fn_import(fn, segs[0], m.start(), imports_by_file)
            if imported is not None:
                module, fname = imported
                resolved = module + (fname,) + segs[1:]
                _gain_from(
                    gained,
                    keys_of,
                    self_index,
                    by_module.get(resolved, {}).get(m.group(2), []),
                )
        leaf = segs[-1] if segs else None
        if (
            leaf
            and leaf not in ("crate", "self", "super")
            and not (segs and segs[0] == "crate")
        ):
            _gain_from(
                gained,
                keys_of,
                self_index,
                by_leaf.get(leaf, {}).get(m.group(2), []),
            )
        # Inline `mod inner { fn relay }` is not a filename leaf (#516 review).
        for prefix in (fn.inline_mods, ()):
            _gain_from(
                gained,
                keys_of,
                self_index,
                by_inline.get((fn.file, prefix + segs), {}).get(m.group(2), []),
            )
    for m in TYPE_ASSOC_CALL.finditer(fn.body):
        # `Self::name(` resolves against the CALLING function's own
        # enclosing impl type, not a literal lookup on the string "Self"
        # (which is never a real registered type name -- `by_type` is
        # keyed by concrete type names from `_impl_blocks`).
        raw_type = m.group(1)
        type_name = fn.type_name if raw_type == "Self" else raw_type
        if type_name is None:
            continue
        if raw_type != "Self":
            imported = _fn_import(fn, raw_type, m.start(), imports_by_file)
            if imported is not None:
                type_name = imported[1]
        caller_groups = groups_of.get(
            fn.file, frozenset({_process_group(fn.file)})
        )
        slots: list[int] = []
        for group in caller_groups:
            slots.extend(
                by_type.get((group, type_name), {}).get(m.group(2), [])
            )
        _gain_from(
            gained,
            keys_of,
            self_index,
            slots,
        )
    for type_name, method in _ufcs_calls(fn.body):
        imported = _fn_import(fn, type_name, 0, imports_by_file)
        if imported is not None:
            type_name = imported[1]
        caller_groups = groups_of.get(
            fn.file, frozenset({_process_group(fn.file)})
        )
        slots = []
        for group in caller_groups:
            slots.extend(by_type.get((group, type_name), {}).get(method, []))
        _gain_from(
            gained,
            keys_of,
            self_index,
            slots,
        )
    return frozenset(gained)

def analyze(
    sources: list[tuple[Path, str]], *, scan_root: Path
) -> tuple[list[Finding], list[str], dict[str, list[tuple[Path, int, str]]]]:
    """Findings, registry errors, and derived membership (for `--dump`)."""

    registry, errors = find_registry(sources)
    if errors:
        return [], errors, {}

    functions, pending_macro_tests, imports_by_file, globs_by_file = index_functions(
        sources, registry
    )

    file_groups = _file_process_groups(sources)
    by_file: dict[Path, dict[str, list[int]]] = {}
    by_module: dict[tuple[str, ...], dict[str, list[int]]] = {}
    by_crate_module: dict[tuple[str, tuple[str, ...]], dict[str, list[int]]] = {}
    by_leaf: dict[str, dict[str, list[int]]] = {}
    by_type: dict[tuple[str, str], dict[str, list[int]]] = {}
    by_macro: dict[Path, dict[str, int]] = {}
    by_macro_any: dict[str, list[int]] = {}
    by_macro_arms: dict[Path, dict[str, tuple[tuple[str, frozenset[str]], ...]]] = {}
    by_inline: dict[tuple[Path, tuple[str, ...]], dict[str, list[int]]] = {}
    for i, fn in enumerate(functions):
        if fn.is_macro:
            by_macro.setdefault(fn.file, {})[fn.name] = i
            by_macro_any.setdefault(fn.name, []).append(i)
            if fn.macro_arms:
                by_macro_arms.setdefault(fn.file, {})[fn.name] = fn.macro_arms
            continue
        by_file.setdefault(fn.file, {}).setdefault(fn.name, []).append(i)
        by_inline.setdefault((fn.file, fn.inline_mods), {}).setdefault(
            fn.name, []
        ).append(i)
        module = _module_path(fn.file)
        crate_mod = module if module is not None else ()
        for group in file_groups.get(fn.file, frozenset({_process_group(fn.file)})):
            by_crate_module.setdefault((group, crate_mod), {}).setdefault(
                fn.name, []
            ).append(i)
            if fn.inline_mods:
                by_crate_module.setdefault(
                    (group, crate_mod + fn.inline_mods), {}
                ).setdefault(fn.name, []).append(i)
            if fn.type_name is not None:
                by_type.setdefault((group, fn.type_name), {}).setdefault(
                    fn.name, []
                ).append(i)
            if fn.trait_name is not None:
                by_type.setdefault((group, fn.trait_name), {}).setdefault(
                    fn.name, []
                ).append(i)
        if module is not None:
            # `module` is a valid module path even when empty (`()` is the
            # crate root itself, from a function declared directly in
            # `src/lib.rs`/`src/main.rs` -- reachable as `crate::name()`,
            # so `by_module` indexing still applies). `module[-1]` has no
            # meaning for that case, though: no sibling ever calls a
            # crate-root function as `lib::name()`/`main::name()`, so
            # `by_leaf` is simply not populated for it, guarded here
            # instead of raising `IndexError` on the empty tuple.
            by_module.setdefault(module, {}).setdefault(fn.name, []).append(i)
            if fn.inline_mods:
                by_module.setdefault(module + fn.inline_mods, {}).setdefault(
                    fn.name, []
                ).append(i)
            if module:
                by_leaf.setdefault(module[-1], {}).setdefault(fn.name, []).append(i)

    reexports: list[tuple[tuple[str, ...], str, tuple[str, ...], str]] = []
    for path, text in sources:
        reexports.extend(_pub_reexports(path, text))
    _copy_reexports_into_indices(reexports, by_module, by_leaf)

    keys_of: list[frozenset[str]] = [fn.keys for fn in functions]
    converged = False
    for _round in range(_MAX_ROUNDS):
        changed = False
        for i, fn in enumerate(functions):
            gained = _resolve_calls(
                fn,
                by_file=by_file,
                by_module=by_module,
                by_leaf=by_leaf,
                by_type=by_type,
                by_macro=by_macro,
                by_macro_any=by_macro_any,
                by_macro_arms=by_macro_arms,
                by_inline=by_inline,
                imports_by_file=imports_by_file,
                globs_by_file=globs_by_file,
                keys_of=keys_of,
                self_index=i,
                by_crate_module=by_crate_module,
                file_groups=file_groups,
            )
            if not gained:
                continue
            new_total = keys_of[i] | gained
            if new_total != keys_of[i]:
                keys_of[i] = new_total
                changed = True
        if not changed:
            converged = True
            break
    if not converged:
        errors.append(
            f"call-graph closure did not converge in {_MAX_ROUNDS} rounds"
        )
        return [], errors, {}

    # Synthesize macro-generated tests only after the call-graph closure, so
    # a `#[test] fn $name() { helper(); }` expansion inherits keys `helper`
    # acquired transitively (#516 review).
    macro_index = {(fn.file, fn.name): i for i, fn in enumerate(functions) if fn.is_macro}
    for pending in pending_macro_tests:
        j = macro_index.get((pending.macro_file, pending.macro_name))
        indices = [j] if j is not None else []
        if not indices:
            continue
        keys: frozenset[str] = frozenset()
        if 0 <= pending.template_index < len(keys_of):
            keys = keys_of[pending.template_index]
        elif indices:
            for idx in indices:
                keys = keys | keys_of[idx]
        if pending.body:
            synth = FnInfo(
                name=pending.macro_name,
                file=pending.file,
                type_name=None,
                trait_name=None,
                is_macro=False,
                inline_mods=pending.inline_mods,
                body=pending.body,
                start=pending.start,
                keys=frozenset(),
                is_test=False,
                serial_held=frozenset(),
                has_unkeyed_serial=False,
                attrs_line=pending.line,
            )
            keys = keys | _resolve_calls(
                synth,
                by_file=by_file,
                by_module=by_module,
                by_leaf=by_leaf,
                by_type=by_type,
                by_macro=by_macro,
                by_macro_any=by_macro_any,
                by_macro_arms=by_macro_arms,
                by_inline=by_inline,
                imports_by_file=imports_by_file,
                globs_by_file=globs_by_file,
                keys_of=keys_of,
                self_index=-1,
                by_crate_module=by_crate_module,
                file_groups=file_groups,
            )
            for item in registry:
                if _body_touches(
                    pending.body,
                    item.identifiers,
                    original=item.identifiers,
                    static_module=_item_module(item),
                    fn_module=_module_path(pending.file),
                    inline_mods=pending.inline_mods,
                    scoped_imports=imports_by_file.get(pending.file, {}),
                ):
                    keys = keys | {item.key}
        if not keys:
            continue
        functions.append(
            FnInfo(
                name=f"{pending.macro_name}!@{pending.line}#{pending.slot}",
                file=pending.file,
                type_name=None,
                trait_name=None,
                is_macro=False,
                inline_mods=pending.inline_mods,
                body=pending.body,
                start=pending.start,
                keys=keys,
                is_test=True,
                serial_held=pending.serial_held,
                has_unkeyed_serial=pending.has_unkeyed_serial,
                attrs_line=pending.line,
            )
        )
        keys_of.append(keys)

    membership: dict[str, list[tuple[Path, int, str]]] = {item.key: [] for item in registry}
    for i, fn in enumerate(functions):
        if not fn.is_test:
            continue
        for key in sorted(keys_of[i]):
            membership[key].append((fn.file, fn.attrs_line, fn.name))

    # Only now do we know, per key, how many distinct process groups its
    # members span -- a lone member in its own process group cannot race
    # anything else in that process (mirrors `check_envguard_serial.py`'s
    # "sole test in its own integration binary" regime, generalised to "sole
    # test in its own process").
    members_by_process: dict[tuple[str, str], list[tuple[Path, int, str]]] = {}
    for key, members in membership.items():
        for path, line, name in members:
            for group in file_groups.get(path, frozenset({_process_group(path)})):
                members_by_process.setdefault((key, group), []).append(
                    (path, line, name)
                )

    findings: list[Finding] = []
    for i, fn in enumerate(functions):
        if not fn.is_test:
            continue
        for key in sorted(keys_of[i]):
            fn_groups = file_groups.get(fn.file, frozenset({_process_group(fn.file)}))
            if all(
                len(members_by_process.get((key, group), [])) <= 1
                for group in fn_groups
            ):
                continue  # sole member in every process it joins
            if key in fn.serial_held:
                continue
            if fn.has_unkeyed_serial:
                reason = (
                    f"carries unkeyed #[serial], which is a DIFFERENT lock from "
                    f"#[serial({key})] (#319/#459) and does not exclude this "
                    f"item's group"
                )
            elif fn.serial_held:
                held = ", ".join(sorted(fn.serial_held))
                reason = f"tagged #[serial({held})], missing '{key}'"
            else:
                reason = f"touches the '{key}' shared item, no #[serial({key})]"
            findings.append(
                Finding(path=fn.file, line=fn.attrs_line, name=fn.name, key=key, reason=reason)
            )

    return findings, [], membership


def scan_tree(scan_root: Path, *, repo: Path) -> tuple[list[Finding], list[str], dict]:
    sources = [
        (p.relative_to(repo), p.read_text(encoding="utf-8")) for p in rust_files(scan_root)
    ]
    return analyze(sources, scan_root=scan_root)


def scan_source(source: str, *, path: str = "fixture.rs") -> list[Finding]:
    """Single-file convenience for tests: registry marker and touchers both
    live in one string. Cross-file resolution needs `analyze` directly with
    multiple `(Path, str)` entries instead."""

    findings, errors, _membership = analyze([(Path(path), source)], scan_root=Path("."))
    if errors:
        raise AssertionError(f"registry error(s) in fixture: {errors}")
    return findings


def format_report(
    findings: list[Finding], errors: list[str], *, scan_rel: str | None = None
) -> str:
    scope = f" [scanned: {scan_rel}]" if scan_rel else ""
    lines = [f"shared-state-serial{scope}: {len(findings)} violation(s), {len(errors)} registry error(s)"]
    if errors:
        lines.append("")
        lines.append("Registry errors (a SERIAL-GROUP marker names no static below it):")
        for e in errors:
            lines.append(f"  {e}")
    if findings:
        lines.append("")
        lines.append("Tests touching a registered shared item without its serial key:")
        for f in sorted(findings, key=lambda x: (str(x.path), x.line, x.name)):
            lines.append(f"  {f.path}:{f.line}: {f.name}: {f.reason}")
        lines.append("")
        lines.append(
            "Add the matching `#[serial(<key>)]` from the item's SERIAL-GROUP "
            "marker, or -- if the reference is spurious -- check whether this "
            "checker's call-graph resolution over-reached (see the module "
            "docstring's WHAT THIS DOES NOT CHECK)."
        )
    return "\n".join(lines)


def format_dump(
    registry_sources: list[tuple[Path, str]], membership: dict[str, list[tuple[Path, int, str]]]
) -> str:
    registry, _errors = find_registry(registry_sources)
    lines = []
    for item in registry:
        lines.append(f"{item.key} ({item.file.as_posix()}:{item.line}):")
        lines.append(f"  identifiers: {', '.join(item.identifiers)}")
        members = sorted(set(membership.get(item.key, [])))
        lines.append(f"  {len(members)} derived member test(s):")
        for path, line, name in members:
            lines.append(f"    {path.as_posix()}:{line}: {name}")
        lines.append("")
    return "\n".join(lines)


def _repo_from_script() -> Path:
    return Path(__file__).resolve().parents[1]


def main(argv: list[str] | None = None) -> int:
    repo = _repo_from_script()
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repo", type=Path, default=repo)
    ap.add_argument("--scan-root", type=Path, default=None)
    ap.add_argument(
        "--dump", action="store_true", help="print the derived registry and membership, exit 0"
    )
    args = ap.parse_args(argv)

    root = args.repo.resolve()
    if args.scan_root is not None:
        chosen = args.scan_root
        scan_roots = [(chosen if chosen.is_absolute() else root / chosen).resolve()]
    else:
        scan_roots = [(root / path).resolve() for path in DEFAULT_SCAN_ROOTS]
    sources = collect_sources(root, scan_roots)
    existing = [path for path in scan_roots if path.is_dir()]
    try:
        scan_rel = ", ".join(path.relative_to(root).as_posix() for path in existing)
    except ValueError:
        scan_rel = ", ".join(path.as_posix() for path in existing)

    if args.dump:
        _findings, _errors, membership = analyze(
            sources, scan_root=scan_roots[0]
        )
        sys.stdout.write(format_dump(sources, membership))
        return 0

    findings, errors, _membership = analyze(sources, scan_root=scan_roots[0])
    print(format_report(findings, errors, scan_rel=scan_rel or None))
    if findings or errors:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
