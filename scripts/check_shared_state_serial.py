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
top of that, not the termination argument). Three call shapes propagate a
hop: a bare `name(` resolved within the SAME FILE; a `path::to::name(`
resolved by full module path when `path` starts with `crate`, and ALSO by
its last segment alone against every file's own module leaf (its filename
stem) either way -- the shape a sibling module is actually called by in
this tree, with no `crate::` prefix at all; and a `Type::assoc_fn(`
resolved crate-wide against `impl Type { .. }` blocks, so a call written
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

Scan scope is one crate: `crates/codegen/xai-grok-shell/src/**/*.rs` unless
`--scan-root` says otherwise -- the same default and the same "one crate"
assumption `check_envguard_serial.py` documents, for the same reason
(`crate::`-qualified resolution only ever names something in the same
crate).

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
from dataclasses import dataclass
from pathlib import Path

DEFAULT_SCAN_ROOT = Path("crates/codegen/xai-grok-shell/src")

# --- pure Rust-syntax primitives -------------------------------------------
# Duplicated from `check_envguard_serial.py` rather than imported -- see the
# module docstring's "Decided, not left open" section for why.

RAW_STRING_START = re.compile(r'r(#+)?"')
CHAR_LITERAL = re.compile(r"'(?:\\.|[^\\'\n])'")
FN_DEF = re.compile(
    r"(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
)
MACRO_DEF = re.compile(r"macro_rules!\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)")
MACRO_INVOKE = re.compile(r"(?<![:.\w])([A-Za-z_][A-Za-z0-9_]*)\s*!")
TEST_ATTR = re.compile(
    r"#\s*\[\s*(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*test\b",
    re.DOTALL,
)
SERIAL_ATTR = re.compile(
    r"#\s*\[\s*(?:serial_test\s*::\s*)?serial\s*(?:\((?P<args>.*)\))?\s*\]",
    re.DOTALL,
)
IMPL_HEAD = re.compile(r"\bimpl\b[^{;]*\{")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
FREE_CALL = re.compile(r"(?<![:.\w])([a-z_][a-z0-9_]*)\s*\(")
# Any `path::to::name(` call, `crate`-rooted or not. Resolved two ways (see
# `_resolve_calls`): a `crate`-rooted path by its FULL module path, and every
# path (rooted or not) by its LAST segment alone against a file's own module
# leaf -- the shape a sibling module is actually called by in this tree
# (`search_recovery::heal_unusable(`, no `crate::` prefix at all).
QUALIFIED_CALL = re.compile(
    r"\b((?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)+)([a-z_][a-z0-9_]*)\s*\("
)
TYPE_ASSOC_CALL = re.compile(
    r"\b([A-Z][A-Za-z0-9_]*)\s*::\s*([A-Za-z_][A-Za-z0-9_]*)\s*(?:::\s*<[^>]*>\s*)?\("
)
# `<Type as Trait>::method(` -- QUALIFIED_CALL cannot cross `as Trait>`.
# Resolves the same way as TYPE_ASSOC_CALL: last segment of the type path
# against `by_type` (#516 review).
UFCS_CALL = re.compile(
    r"<\s*(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*([A-Z][A-Za-z0-9_]*)"
    r"(?:\s*<[^>]*>)?"
    r"\s+as\s+"
    r"(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*[A-Za-z_][A-Za-z0-9_]*"
    r"(?:\s*<[^>]*>)?"
    r"\s*>\s*::\s*"
    r"([A-Za-z_][A-Za-z0-9_]*)"
    r"\s*\("
)

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
    `_strip_turbofish` at a literal `::<`) is a position already known by
    its own caller's structure to be a generic-list opener specifically,
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
    return TEST_ATTR.match(attr.strip()) is not None


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
    while index < len(source):
        if source[index].isspace():
            index += 1
            continue
        comment_end = _skip_comment(source, index)
        if comment_end is not None:
            # `fn bump() /* { } */ { body }` — the comment's braces are not
            # the body (#516 review).
            index = comment_end
            continue
        if source[index] == "{":
            return index, _balanced_end(source, index)
        if source[index] == ";":
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

    def skip_ws(i: int) -> int:
        while i < n and head[i].isspace():
            i += 1
        return i

    def read_type_path(i: int) -> tuple[str | None, int]:
        """One `Path::To::Type<generics>` expression -- returns its LAST
        segment's identifier (the type/trait's own name, generic
        parameters/arguments of that segment skipped) and the index just
        past it."""
        name = None
        while True:
            i = skip_ws(i)
            m = IDENT.match(head, i)
            if m is None:
                break
            name = m.group(0)
            i = skip_ws(m.end())
            if i < n and head[i] == "<":
                i = skip_ws(_skip_generic_params(head, i))
            if head[i : i + 2] == "::":
                i += 2
                continue
            break
        return name, i

    index = skip_ws(index)
    if index < n and head[index] == "<":
        index = skip_ws(_skip_generic_params(head, index))
    first_name, index = read_type_path(index)
    index = skip_ws(index)
    if head[index : index + 3] == "for" and not (
        index + 3 < n and (head[index + 3].isalnum() or head[index + 3] == "_")
    ):
        second_name, _index = read_type_path(index + 3)
        return second_name, first_name
    return first_name, None


def _impl_blocks(code: str) -> list[tuple[str, str | None, int, int]]:
    """`(type, trait or None, start, end)` for each `impl … { … }`."""

    blocks: list[tuple[str, str | None, int, int]] = []
    for match in IMPL_HEAD.finditer(code):
        type_name, trait_name = _impl_type_name(match.group(0))
        if type_name is None:
            continue
        open_index = match.end() - 1
        blocks.append(
            (type_name, trait_name, open_index, _balanced_end(code, open_index))
        )
    return blocks


def _crate_of(path: Path) -> str:
    parts = path.parts
    return parts[2] if len(parts) > 2 else str(path)


def _module_path(rel: Path) -> tuple[str, ...] | None:
    """`crate::a::b` module path for `.../src/a/b.rs` or `.../src/a/b/mod.rs`."""

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
    return f"lib:{_crate_of(path)}"


def rust_files(scan_root: Path) -> list[Path]:
    return sorted(path for path in scan_root.rglob("*.rs") if path.is_file())


# --- registry discovery ------------------------------------------------------


@dataclass(frozen=True)
class SharedItem:
    key: str
    file: Path
    identifiers: tuple[str, ...]
    line: int


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
            items.append(
                SharedItem(key=key, file=rel, identifiers=tuple(identifiers), line=line)
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
    body: str  # code-only, turbofish-stripped -- see `_strip_turbofish`
    start: int
    keys: frozenset[str]  # Stage-1 direct touch, possibly empty
    is_test: bool
    serial_held: frozenset[str]  # keys held by any #[serial(..)] on this fn
    has_unkeyed_serial: bool
    attrs_line: int


def _body_touches(code_only_body: str, identifiers: tuple[str, ...]) -> bool:
    return any(re.search(rf"\b{re.escape(ident)}\b", code_only_body) for ident in identifiers)


def index_functions(
    sources: list[tuple[Path, str]], registry: list[SharedItem]
) -> list[FnInfo]:
    out: list[FnInfo] = []
    for rel, raw in sources:
        code = _code_only(raw)
        impls = _impl_blocks(code)
        for match in FN_DEF.finditer(code):
            name = match.group("name")
            body_span = _fn_body(raw, match.end())
            if body_span is None:
                continue
            body_start, body_end = body_span
            body_code = _strip_turbofish(code[body_start:body_end])
            keys = frozenset(
                item.key for item in registry if _body_touches(body_code, item.identifiers)
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
                    body=body_code,
                    start=body_start,
                    keys=keys,
                    is_test=is_test,
                    serial_held=frozenset(serial_held),
                    has_unkeyed_serial=has_unkeyed,
                    attrs_line=_line(raw, match.start()),
                )
            )
        for match in MACRO_DEF.finditer(code):
            name = match.group("name")
            body_span = _macro_body(raw, match.end())
            if body_span is None:
                continue
            body_start, body_end = body_span
            body_code = _strip_turbofish(code[body_start:body_end])
            keys = frozenset(
                item.key for item in registry if _body_touches(body_code, item.identifiers)
            )
            out.append(
                FnInfo(
                    name=name,
                    file=rel,
                    type_name=None,
                    trait_name=None,
                    is_macro=True,
                    body=body_code,
                    start=body_start,
                    keys=keys,
                    is_test=False,
                    serial_held=frozenset(),
                    has_unkeyed_serial=False,
                    attrs_line=_line(raw, match.start()),
                )
            )
    return out


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
# argument. A call not shaped like one of the four resolved forms below
# breaks the chain at that point -- silently, same as it would for
# `check_envguard_serial.py`'s own one-hop resolution -- see WHAT THIS DOES
# NOT CHECK in the module docstring.
#
# Four call shapes are resolved, all requiring `name(` -- never a bare
# mention without a call, per the module docstring's measured false-positive
# finding:
#   1. `name(`                    -- same file only (unambiguous without
#                                     import resolution)
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


def _resolve_calls(
    fn: FnInfo,
    *,
    by_file: dict[Path, dict[str, int]],
    by_module: dict[tuple[str, ...], dict[str, int]],
    by_leaf: dict[str, dict[str, int]],
    by_type: dict[str, dict[str, list[int]]],
    by_macro: dict[Path, dict[str, int]],
    keys_of: list[frozenset[str]],
    self_index: int,
) -> frozenset[str]:
    gained: set[str] = set()
    file_index = by_file.get(fn.file, {})
    caller_module = _module_path(fn.file)
    for m in FREE_CALL.finditer(fn.body):
        j = file_index.get(m.group(1))
        if j is not None and j != self_index:
            gained.update(keys_of[j])
    macro_index = by_macro.get(fn.file, {})
    for m in MACRO_INVOKE.finditer(fn.body):
        j = macro_index.get(m.group(1))
        if j is not None and j != self_index:
            gained.update(keys_of[j])
    for m in QUALIFIED_CALL.finditer(fn.body):
        segs = tuple(s.strip() for s in m.group(1).split("::") if s.strip())
        if segs and segs[0] == "crate":
            j = by_module.get(segs[1:], {}).get(m.group(2))
            if j is not None and j != self_index:
                gained.update(keys_of[j])
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
                j = file_index.get(m.group(2))
                if j is not None and j != self_index:
                    gained.update(keys_of[j])
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
                    j = by_module.get(base + trailing, {}).get(m.group(2))
                    if j is not None and j != self_index:
                        gained.update(keys_of[j])
        leaf = segs[-1] if segs else None
        if leaf and leaf not in ("crate", "self", "super"):
            j = by_leaf.get(leaf, {}).get(m.group(2))
            if j is not None and j != self_index:
                gained.update(keys_of[j])
    for m in TYPE_ASSOC_CALL.finditer(fn.body):
        # `Self::name(` resolves against the CALLING function's own
        # enclosing impl type, not a literal lookup on the string "Self"
        # (which is never a real registered type name -- `by_type` is
        # keyed by concrete type names from `_impl_blocks`).
        type_name = fn.type_name if m.group(1) == "Self" else m.group(1)
        if type_name is None:
            continue
        for j in by_type.get(type_name, {}).get(m.group(2), []):
            if j != self_index:
                gained.update(keys_of[j])
    for m in UFCS_CALL.finditer(fn.body):
        for j in by_type.get(m.group(1), {}).get(m.group(2), []):
            if j != self_index:
                gained.update(keys_of[j])
    return frozenset(gained)


def analyze(
    sources: list[tuple[Path, str]], *, scan_root: Path
) -> tuple[list[Finding], list[str], dict[str, list[tuple[Path, int, str]]]]:
    """Findings, registry errors, and derived membership (for `--dump`)."""

    registry, errors = find_registry(sources)
    if errors:
        return [], errors, {}

    functions = index_functions(sources, registry)

    by_file: dict[Path, dict[str, int]] = {}
    by_module: dict[tuple[str, ...], dict[str, int]] = {}
    by_leaf: dict[str, dict[str, int]] = {}
    by_type: dict[str, dict[str, list[int]]] = {}
    by_macro: dict[Path, dict[str, int]] = {}
    for i, fn in enumerate(functions):
        if fn.is_macro:
            by_macro.setdefault(fn.file, {})[fn.name] = i
            continue
        by_file.setdefault(fn.file, {})[fn.name] = i
        module = _module_path(fn.file)
        if module is not None:
            # `module` is a valid module path even when empty (`()` is the
            # crate root itself, from a function declared directly in
            # `src/lib.rs`/`src/main.rs` -- reachable as `crate::name()`,
            # so `by_module` indexing still applies). `module[-1]` has no
            # meaning for that case, though: no sibling ever calls a
            # crate-root function as `lib::name()`/`main::name()`, so
            # `by_leaf` is simply not populated for it, guarded here
            # instead of raising `IndexError` on the empty tuple.
            by_module.setdefault(module, {})[fn.name] = i
            if module:
                by_leaf.setdefault(module[-1], {})[fn.name] = i
        if fn.type_name is not None:
            by_type.setdefault(fn.type_name, {}).setdefault(fn.name, []).append(i)
        if fn.trait_name is not None:
            by_type.setdefault(fn.trait_name, {}).setdefault(fn.name, []).append(i)

    keys_of: list[frozenset[str]] = [fn.keys for fn in functions]
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
                keys_of=keys_of,
                self_index=i,
            )
            if not gained:
                continue
            new_total = keys_of[i] | gained
            if new_total != keys_of[i]:
                keys_of[i] = new_total
                changed = True
        if not changed:
            break

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
            members_by_process.setdefault((key, _process_group(path)), []).append(
                (path, line, name)
            )

    findings: list[Finding] = []
    for i, fn in enumerate(functions):
        if not fn.is_test:
            continue
        for key in sorted(keys_of[i]):
            group = _process_group(fn.file)
            if len(members_by_process.get((key, group), [])) <= 1:
                continue  # sole member in its process: nothing to race
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
    scan_root = (args.scan_root or (root / DEFAULT_SCAN_ROOT)).resolve()
    sources = [
        (p.relative_to(root), p.read_text(encoding="utf-8")) for p in rust_files(scan_root)
    ]

    if args.dump:
        _findings, _errors, membership = analyze(sources, scan_root=scan_root)
        sys.stdout.write(format_dump(sources, membership))
        return 0

    findings, errors, _membership = analyze(sources, scan_root=scan_root)
    try:
        scan_rel = scan_root.relative_to(root).as_posix()
    except ValueError:
        scan_rel = None
    print(format_report(findings, errors, scan_rel=scan_rel))
    if findings or errors:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
