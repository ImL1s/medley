"""Pins CLAUDE.md's credential hot-path counts (#487).

CLAUDE.md names five patterns/tests as CI's credential hot path -- "if a
change is going to break something, that is where it shows." One of the
five was a literal substring, `never_emit_credential_bytes`, that read as
a family name but selected only one of its own three tests. Both of the
missed tests run in CI today via unrelated filters, so nothing was
actually uncovered -- but a report of "1 passed" for this filter reads as
"the suite is green" with no way to tell it is one third of the family.
`run_nonzero`'s own zero-match guard cannot catch this: one match is not
zero.

This guard checks two things CLAUDE.md's prose alone cannot self-verify:

1. That the counts recorded in CLAUDE.md still match what each
   pattern/name actually selects. Enumerated independently of `cargo
   test`'s own filter mechanism: a source scan for
   `#[test]`/`#[tokio::test]`-attributed function names under every real
   crate's `src/` AND `tests/` directories, qualified with their in-file
   module prefix. Both scope dimensions matter, and this guard's own first
   version got each wrong once, caught by review both times:

   - `--lib`-only scope missed 3 of the 5 documented counts (#487's own
     investigation) -- `xai-grok-sampler/tests/shared_http_wire.rs` is a
     separate integration-test target, invisible to a `--lib`-only
     listing.
   - A repo-wide total, ignoring the `-p` / `--lib` / `--test` of the
     `run_nonzero` line that actually invokes the filter, lets a
     sampler-lib test vanish while a same-pattern test appears in
     another crate and both the documented count and `run_nonzero`
     stay green (#507 review). Counts for a dedicated CI filter are
     therefore taken only inside that invocation's package and cargo
     target.
   - fn-name-only classification (no module prefix) reproduces the exact
     defect this guard exists to catch (#507 review): libtest matches a
     substring filter against the full `module::path::fn` name, so a
     generic test added under a module whose name matches a pattern
     (`mod none_auth_scheme_regressions { fn works() {} }`) is selected by
     the real filter but invisible to a fn-name-only scan.

2. That CLAUDE.md's counts are the thing actually checked, not a value
   duplicated in this file (#507 review): a guard that pins its own copy
   of "3", not the "3" written in CLAUDE.md, goes green forever no matter
   what CLAUDE.md is edited to say. This module parses the counts straight
   out of the documented paragraph and treats a parse failure as a loud
   error, never as a silent fallback to some other expected value.
"""

from __future__ import annotations

import platform
import re
import struct
import subprocess
import sys
import sysconfig
import tempfile
import textwrap
import tomllib
import unicodedata
import unittest
from bisect import bisect_right
from collections import deque
from pathlib import Path
from typing import NamedTuple

ROOT = Path(__file__).resolve().parent.parent
CLAUDE_MD = ROOT / "CLAUDE.md"
_CRATE_ROOTS = ("crates", "prod", "third_party")
CI_YML = ROOT / ".github" / "workflows" / "ci.yml"

sys.path.insert(0, str(ROOT / "scripts"))
from check_test_filter_coverage import (  # noqa: E402
    ALL_FEATURES_TOKEN,
    EXACT_PREFIX,
    NO_DEFAULT_FEATURES_TOKEN,
    parse_workflow,
    parse_workflow_by_features,
)
from toml_package_name import package_name  # noqa: E402

_TEST_ATTR = re.compile(r"^\s*#\[(?:tokio::)?test\b")
_IGNORE_ATTR = re.compile(r"^#\[\s*ignore\b")
_RUST_IDENT = r"(?:[A-Za-z_]|[^\W\d_])(?:[A-Za-z0-9_]|[^\x00-\x7f])*"
_FN = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:(?:async|const|unsafe|extern(?:\s+\"[^\"]*\")?)\s+)*"
    rf"fn\s+(?:r#)?({_RUST_IDENT})"
)
_INCLUDE = re.compile(
    r'\binclude!\s*[(\[{]\s*(?:"([^"]*)"|r(#*)"((?:.|\n)*?)"\2)\s*[)\]}]',
    re.DOTALL,
)
_MOD_OPEN = re.compile(
    rf"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?({_RUST_IDENT})\s*\{{"
)
_MOD_SEMI = re.compile(
    rf"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?({_RUST_IDENT})\s*;"
)
_MOD_KW_ONLY = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s*$"
)
_IDENT_SEMI = re.compile(rf"^\s*(?:r#)?({_RUST_IDENT})\s*;")
_IDENT_OPEN = re.compile(rf"^\s*(?:r#)?({_RUST_IDENT})\s*\{{")
_INLINE_MOD_OPEN = re.compile(
    rf"(?:pub(?:\([^)]*\))?\s+)?mod\s+(?:r#)?({_RUST_IDENT})\s*\{{"
)
_PATH_ATTR = re.compile(
    r'#\[\s*path\s*=\s*(?:"([^"]*)"|r(#*)"((?:.|\n)*?)"\2)\s*\]',
    re.DOTALL,
)

_HOT_PATH_MARKER = "CI's hot path is exactly this suite"
_DOC_ENTRY = re.compile(r"`([a-z][a-z0-9_]*)`\s*\((\d+)\)")

# The seven names the hot-path paragraph must keep enumerating. Counts
# still come from CLAUDE.md; this set is the ratchet that a deleted
# named entry cannot silently drop out of the loop (#507 review).
_REQUIRED_HOT_PATH_ENTRIES = frozenset(
    {
        "is_secret_free_",
        "omits_xai_identity",
        "hostile_injector",
        "none_auth_scheme_",
        "sampler_request_logs_never_emit_credential_bytes",
        "transport_failure_never_emits_query_credential_bytes",
        "subagent_resolution_diagnostics_never_emit_parent_or_child_credentials",
    }
)


def _strip_line_comment(line: str) -> str:
    # Used only for `mod name {` detection on already-masked lines.
    idx = line.find("//")
    return line if idx == -1 else line[:idx]


_RAW_STRING_START = re.compile(r"(?:c|b)?r(#*)\"")
_MACRO_IDENT = re.compile(rf"(?:r#)?({_RUST_IDENT})")
_MACRO_INVOKE = re.compile(rf"(?<![\w:])(?:r#)?({_RUST_IDENT})\s*!\s*[([{{]")
_CRATE_MACRO_INVOKE = re.compile(
    rf"\b(?:crate|self|super)\s*::\s*(?:r#)?({_RUST_IDENT})\s*!\s*[([{{]"
)
_MODULE_MACRO_INVOKE = re.compile(
    rf"\b(?!(?:crate|self|super)\b)"
    rf"((?:(?:r#)?{_RUST_IDENT}\s*::\s*)+)"
    rf"(?:r#)?({_RUST_IDENT})\s*!\s*[([{{]"
)
_MACRO_USE_ALIAS = re.compile(
    rf"\buse\s+(?:(?:crate|self|super)\s*::\s*)?(?:(?:r#)?{_RUST_IDENT}\s*::\s*)*"
    rf"(?:r#)?({_RUST_IDENT})\s+as\s+(?:r#)?({_RUST_IDENT})\s*;"
)
_MACRO_USE_GROUP_START = re.compile(
    rf"\buse\s+(?:(?:crate|self|super)\s*::\s*)?(?:(?:r#)?{_RUST_IDENT}\s*::\s*)*\{{"
)
_MACRO_USE_ALIAS_IN_TREE = re.compile(
    rf"(?:r#)?({_RUST_IDENT})\s+as\s+(?:r#)?({_RUST_IDENT})"
)


def _macro_name(match: re.Match[str]) -> str:
    """Rust identifiers compare after NFC normalization."""

    name = unicodedata.normalize("NFC", match.group(1))
    return name if name.isidentifier() else ""


def _macro_use_alias_entries(
    masked: str,
    *,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> list[tuple[str, str, int, tuple[int, ...]]]:
    """`(alias, original, start, brace_scope)` for macro `use` imports.

    Handles both `use crate::emit as aliased;` and grouped
    `use crate::{emit as aliased};`. Cfg-gated imports are dropped.
    Scope is the brace path containing the `use` (#507 review).
    """

    attr_spans = _outer_attr_spans(masked)
    attr_ends = [end for _s, end, _a in attr_spans]
    pairs: list[tuple[str, str, int]] = []
    for match in _MACRO_USE_ALIAS.finditer(masked):
        original = unicodedata.normalize("NFC", match.group(1))
        alias = unicodedata.normalize("NFC", match.group(2))
        if original.isidentifier() and alias.isidentifier():
            pairs.append((alias, original, match.start()))
    for match in _MACRO_USE_GROUP_START.finditer(masked):
        end = _balanced_pair_end(masked, match.end() - 1)
        inner = masked[match.end() : end - 1]
        for alias_match in _MACRO_USE_ALIAS_IN_TREE.finditer(inner):
            original = unicodedata.normalize("NFC", alias_match.group(1))
            alias = unicodedata.normalize("NFC", alias_match.group(2))
            if original.isidentifier() and alias.isidentifier():
                pairs.append((alias, original, match.start()))
    starts = {start for _a, _o, start in pairs}
    scopes = _brace_scopes_at(masked, starts, []) if starts else {}
    out: list[tuple[str, str, int, tuple[int, ...]]] = []
    for alias, original, start in pairs:
        attrs = _outer_attrs_before(masked, start, attr_spans, attr_ends)
        if any(_cfg_attr_is_inactive(a, enabled_features) for a in attrs):
            continue
        out.append((alias, original, start, scopes.get(start, ())))
    return out


def _macro_use_aliases(
    masked: str,
    *,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> dict[str, str]:
    """File-wide alias map for call sites that lack an invoke position."""

    return {
        alias: original
        for alias, original, _start, _scope in _macro_use_alias_entries(
            masked, enabled_features=enabled_features
        )
    }


def _resolve_macro_alias(
    name: str,
    aliases: dict[str, str],
    *,
    entries: list[tuple[str, str, int, tuple[int, ...]]] | None = None,
    invoke_scope: tuple[int, ...] | None = None,
) -> str:
    if entries is not None and invoke_scope is not None:
        for alias, original, _start, scope in reversed(entries):
            if alias != name:
                continue
            if invoke_scope[: len(scope)] == scope:
                return original
        return name
    return aliases.get(name, name)


def _fn_name(match: re.Match[str]) -> str:
    return unicodedata.normalize("NFC", match.group(1))


def _path_prefix_segments(path: str) -> tuple[str, ...]:
    """Normalize `macros ::` / `foo::bar::` into identifier segments."""

    out: list[str] = []
    for raw in path.split("::"):
        seg = raw.strip()
        if not seg:
            continue
        if seg.startswith("r#"):
            seg = seg[2:]
        seg = unicodedata.normalize("NFC", seg)
        if seg.isidentifier():
            out.append(seg)
    return tuple(out)


def _append_crate_qualified_invokes(
    masked: str,
    scoped_defs: tuple[tuple[str, str, int, tuple[int, ...]], ...],
    inherited_macros: tuple[tuple[str, str], ...],
    def_spans: list[tuple[int, int]],
    invoke_at: list[tuple[int, str, str, str]],
) -> None:
    """Module- or crate-qualified macro invocations (#507 review).

    `crate::`/`self::`/`super::` keep the root/inherited lookup. Ordinary
    module paths such as `macros::emit!` resolve only to macros whose
    enclosing inline-module path matches the qualifier — they must not
    fall back to an unrelated same-named root macro.
    """

    available = {name for name, _source, _end, _scope in scoped_defs} | {
        name for name, _source in inherited_macros
    }
    for match in _CRATE_MACRO_INVOKE.finditer(masked):
        name = unicodedata.normalize("NFC", match.group(1))
        if not name.isidentifier() or name not in available:
            continue
        if any(start <= match.start() < end for start, end in def_spans):
            continue
        root = [
            (end, source)
            for def_name, source, end, def_scope in scoped_defs
            if def_name == name and not def_scope and end <= match.start()
        ]
        if root:
            source = max(root)[1]
        else:
            source = next(
                (
                    inherited
                    for def_name, inherited in inherited_macros
                    if def_name == name
                ),
                "",
            )
        if not source:
            continue
        delim = match.end() - 1
        end = _balanced_pair_end(masked, delim)
        invoke_at.append(
            (match.start(), name, masked[delim + 1 : end - 1], source)
        )
    for match in _MODULE_MACRO_INVOKE.finditer(masked):
        name = unicodedata.normalize("NFC", match.group(2))
        if not name.isidentifier():
            continue
        if any(start <= match.start() < end for start, end in def_spans):
            continue
        segs = _path_prefix_segments(match.group(1))
        if not segs:
            continue
        module_defs = [
            (end, source)
            for def_name, source, end, _def_scope in scoped_defs
            if def_name == name
            and end <= match.start()
            and _inline_mods_at(masked, max(0, end - 1)) == segs
        ]
        if not module_defs:
            continue
        source = max(module_defs)[1]
        delim = match.end() - 1
        end = _balanced_pair_end(masked, delim)
        invoke_at.append(
            (match.start(), name, masked[delim + 1 : end - 1], source)
        )


def _qualified_macro_invocation(masked: str, start: int) -> bool:
    """Whether the identifier is preceded by a possibly spaced `::`."""

    index = start - 1
    while index >= 0 and masked[index].isspace():
        index -= 1
    return index > 0 and masked[index - 1 : index + 1] == "::"


def _skip_quoted(text: str, index: int, quote: str) -> int:
    index += 1
    n = len(text)
    while index < n:
        if text[index] == "\\":
            index += 2
            continue
        if text[index] == quote:
            return index + 1
        index += 1
    return n


def _attribute_end(text: str, hash_index: int) -> int | None:
    """End of `#[...]` / `#![...]`, skipping string values."""

    n = len(text)
    j = hash_index + 1
    if j < n and text[j] == "!":
        j += 1
    if j >= n or text[j] != "[":
        return None
    depth = 0
    while j < n:
        raw = _RAW_STRING_START.match(text, j)
        if raw:
            hashes = raw.group(1)
            closer = '"' + hashes
            end = text.find(closer, raw.end())
            j = n if end < 0 else end + len(closer)
            continue
        if text[j] in "\"'":
            j = _skip_quoted(text, j, text[j])
            continue
        if text[j] == "[":
            depth += 1
        elif text[j] == "]":
            depth -= 1
            j += 1
            if depth == 0:
                return j
            continue
        j += 1
    return n


def _mask_attr_string_braces(text: str) -> str:
    """Blank `{`/`}` inside attribute string literals so brace depth
    ignores `#[doc = "}"]` while cfg strings stay on the unmasked copy
    (#507 review)."""

    chars = list(text)
    i = 0
    n = len(text)
    while i < n:
        end = _attribute_end(text, i) if text[i] == "#" else None
        if end is None:
            i += 1
            continue
        j = i
        while j < end:
            raw = _RAW_STRING_START.match(text, j)
            if raw:
                hashes = raw.group(1)
                closer = '"' + hashes
                close = text.find(closer, raw.end())
                close = n if close < 0 else close + len(closer)
                for t in range(j, min(close, end)):
                    if chars[t] in "{}":
                        chars[t] = " "
                j = close
                continue
            if text[j] in "\"'":
                k = _skip_quoted(text, j, text[j])
                for t in range(j, min(k, end)):
                    if chars[t] in "{}":
                        chars[t] = " "
                j = k
                continue
            j += 1
        i = end
    return "".join(chars)


def _mask_rust_literals(text: str) -> str:
    """Replace comments and string/char/raw-string bodies with spaces.

    Newlines are kept so line-oriented `mod` / `#[test]` scanning stays
    aligned. Brace counting on the masked text then ignores `{` inside
    `"{"`, `'{'`, `r#"{"paths":[]}"#`, and similar (#507 review).
    """

    out: list[str] = []
    i = 0
    n = len(text)

    def keep_newlines(chunk: str) -> str:
        return "".join("\n" if c == "\n" else " " for c in chunk)

    while i < n:
        attr_end = _attribute_end(text, i) if text[i] == "#" else None
        if attr_end is not None:
            out.append(text[i:attr_end])
            i = attr_end
            continue
        if text.startswith("//", i):
            end = text.find("\n", i)
            if end < 0:
                out.append(" " * (n - i))
                break
            out.append(" " * (end - i))
            i = end
            continue
        if text.startswith("/*", i):
            depth = 1
            j = i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth += 1
                    j += 2
                    continue
                if text.startswith("*/", j):
                    depth -= 1
                    j += 2
                    continue
                j += 1
            out.append(keep_newlines(text[i:j]))
            i = j
            continue
        raw = _RAW_STRING_START.match(text, i)
        if raw:
            hashes = raw.group(1)
            closer = '"' + hashes
            end = text.find(closer, raw.end())
            end = n if end < 0 else end + len(closer)
            out.append(keep_newlines(text[i:end]))
            i = end
            continue
        if text[i] in "\"'":
            q = text[i]
            if q == "'" and i + 1 < n and (text[i + 1].isalpha() or text[i + 1] == "_"):
                j = i + 1
                while j < n and (text[j].isalnum() or text[j] == "_"):
                    j += 1
                if j >= n or text[j] != "'":
                    # Lifetime or label (`'a`, `'static`, `'foo:`) — not a
                    # character literal (#507 review).
                    out.append(" " * (j - i))
                    i = j
                    continue
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == q:
                    j += 1
                    break
                j += 1
            out.append(keep_newlines(text[i:j]))
            i = j
            continue
        out.append(text[i])
        i += 1
    return "".join(out)


_MACRO_RULES = re.compile(r"\bmacro_rules\s*!")


def _balanced_pair_end(text: str, open_index: int) -> int:
    pairs = {"{": "}", "(": ")", "[": "]"}
    opener = text[open_index]
    closer = pairs[opener]
    depth = 0
    for index in range(open_index, len(text)):
        char = text[index]
        if char == opener:
            depth += 1
        elif char == closer:
            depth -= 1
            if depth == 0:
                return index + 1
    return len(text)


def _macro_rules_defs(masked: str) -> list[tuple[str, int, int]]:
    """`(name, body_start, body_end)` for each `macro_rules!` definition."""

    defs: list[tuple[str, int, int]] = []
    for match in _MACRO_RULES.finditer(masked):
        index = match.end()
        while index < len(masked) and masked[index].isspace():
            index += 1
        ident = _MACRO_IDENT.match(masked, index)
        if not ident:
            continue
        name = _macro_name(ident)
        if not name:
            continue
        index = ident.end()
        while index < len(masked) and masked[index].isspace():
            index += 1
        if index < len(masked) and masked[index] in "{([":
            defs.append((name, index, _balanced_pair_end(masked, index)))
    return defs


def _brace_scopes_at(
    masked: str,
    offsets: set[int],
    macro_spans: list[tuple[int, int]],
) -> dict[int, tuple[int, ...]]:
    """Lexical brace scope at source offsets, skipping macro bodies."""

    if not offsets:
        return {}
    spans_by_start = dict(macro_spans)
    scopes: dict[int, tuple[int, ...]] = {}
    stack: list[int] = []
    limit = max(offsets)
    i = 0
    while i <= limit and i < len(masked):
        if i in offsets:
            scopes[i] = tuple(stack)
        macro_end = spans_by_start.get(i)
        if macro_end is not None:
            i = macro_end
            continue
        if masked[i] == "{":
            stack.append(i)
        elif masked[i] == "}":
            if stack:
                stack.pop()
        i += 1
    return scopes


def _scoped_macro_rules_sources(
    masked: str,
    defs: list[tuple[str, int, int]] | None = None,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> tuple[tuple[str, str, int, tuple[int, ...]], ...]:
    """`(name, source, end, lexical brace scope)` macro definitions."""

    if defs is None:
        defs = _macro_rules_defs(masked)
    if not defs:
        return ()
    starts = [masked.rfind("macro_rules", 0, start) for _, start, _ in defs]
    spans = sorted(
        (start, end)
        for start, (_name, _body_start, end) in zip(starts, defs)
        if start >= 0
    )
    scopes = _brace_scopes_at(
        _mask_attr_string_braces(masked), set(starts), spans
    )
    attr_spans = _outer_attr_spans(masked)
    attr_ends = [end for _start, end, _attr in attr_spans]
    return tuple(
        (
            name,
            masked[start:body_end],
            body_end,
            scopes.get(start, ()),
        )
        for start, (name, _body_start, body_end) in zip(starts, defs)
        if start >= 0
        if not any(
            _cfg_attr_is_inactive(attr, enabled_features)
            for attr in _effective_attrs(
                _outer_attrs_before(masked, start, attr_spans, attr_ends),
                enabled_features,
            )
        )
    )


def _outer_attr_spans(masked: str) -> list[tuple[int, int, str]]:
    spans: list[tuple[int, int, str]] = []
    index = 0
    while index < len(masked):
        end = (
            _attribute_end(masked, index)
            if masked[index] == "#" and not masked.startswith("#![", index)
            else None
        )
        if end is None:
            index += 1
            continue
        spans.append((index, end, masked[index:end]))
        index = end
    return spans


def _outer_attrs_before(
    masked: str,
    item_start: int,
    spans: list[tuple[int, int, str]],
    ends: list[int],
) -> list[str]:
    """Contiguous outer attributes attached to the item at `item_start`."""

    attrs: list[str] = []
    cursor = item_start
    index = bisect_right(ends, cursor) - 1
    while index >= 0:
        start, end, attr = spans[index]
        if masked[end:cursor].strip():
            break
        attrs.append(attr)
        cursor = start
        index -= 1
    attrs.reverse()
    return attrs


def _macro_rules_sources_before(
    definitions: tuple[tuple[str, str, int, tuple[int, ...]], ...],
    before: int,
    scope: tuple[int, ...],
) -> tuple[tuple[str, str], ...]:
    """Macro definitions lexically available before a child `mod` item.

    External module files inherit the parent's textual `macro_rules!` scope.
    Keep the nearest definition first so a later parent definition shadows an
    earlier definition with the same name, as rustc does.
    """

    sources: list[tuple[str, str]] = []
    for name, source, body_end, def_scope in definitions:
        if body_end > before:
            continue
        if scope[: len(def_scope)] == def_scope:
            sources.append((name, source))
    sources.reverse()
    return tuple(sources)


def _invoked_macro_names(
    masked: str, defs: list[tuple[str, int, int]]
) -> set[str]:
    """Macro names that have a `name!(...)` invocation outside any
    `macro_rules!` body. Invoked macros emit their `#[test]` items
    (#507 review)."""

    return {name for name, _inner in _macro_invoke_inners(masked, defs)}


_ARM_TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|[0-9]+|[^\sA-Za-z0-9_]")


def _is_ident_token(token: str) -> bool:
    return bool(token) and (token[:1].isalpha() or token[:1] == "_" or ord(token[0]) > 127)


def _consume_generic_args(tokens: list[str], index: int) -> int | None:
    """Consume `<...>` including nested generics; `->` is not a close."""

    if index >= len(tokens) or tokens[index] != "<":
        return None
    depth = 0
    i = index
    n = len(tokens)
    while i < n:
        tok = tokens[i]
        if tok == "<":
            depth += 1
            i += 1
            continue
        if tok == ">":
            depth -= 1
            i += 1
            if depth == 0:
                return i
            continue
        if tok == "-" and i + 1 < n and tokens[i + 1] == ">":
            i += 2
            continue
        i += 1
    return None


def _consume_ty(tokens: list[str], cursor: int) -> int | None:
    """One `$ty:ty` / `$path:path` fragment (`Option<u8>`, paths)."""

    n = len(tokens)
    i = cursor
    if i >= n:
        return None
    while i < n and tokens[i] in {"&", "*"}:
        i += 1
        if i < n and tokens[i] == "'":
            i += 1
            if i < n:
                i += 1
        if i < n and tokens[i] in {"mut", "const"}:
            i += 1
    if i < n and tokens[i] in {"dyn", "impl"}:
        i += 1
    if i >= n:
        return None
    if tokens[i] in "([":
        opener = tokens[i]
        closer = ")" if opener == "(" else "]"
        depth = 0
        while i < n:
            if tokens[i] == opener:
                depth += 1
            elif tokens[i] == closer:
                depth -= 1
                i += 1
                if depth == 0:
                    return i
                continue
            i += 1
        return None
    if not _is_ident_token(tokens[i]):
        return None
    i += 1
    while i + 1 < n and tokens[i] == ":" and tokens[i + 1] == ":":
        i += 2
        if i < n and tokens[i] == "<":
            nxt = _consume_generic_args(tokens, i)
            if nxt is None:
                return None
            i = nxt
            continue
        if i >= n or not _is_ident_token(tokens[i]):
            return None
        i += 1
    if i < n and tokens[i] == "<":
        nxt = _consume_generic_args(tokens, i)
        if nxt is None:
            return None
        i = nxt
    return i if i > cursor else None


def _consume_kind(tokens: list[str], cursor: int, kind: str) -> int | None:
    """Advance past one `macro_rules` fragment starting at `cursor`."""

    if cursor >= len(tokens):
        return None
    kind = kind or "ident"
    if kind in {"ty", "path"}:
        return _consume_ty(tokens, cursor)
    if kind == "block":
        if tokens[cursor] != "{":
            return None
        depth = 0
        index = cursor
        while index < len(tokens):
            if tokens[index] == "{":
                depth += 1
            elif tokens[index] == "}":
                depth -= 1
                if depth == 0:
                    return index + 1
            index += 1
        return None
    if kind == "expr":
        depth = 0
        index = cursor
        while index < len(tokens):
            token = tokens[index]
            if token in "([{":
                depth += 1
            elif token in ")]}":
                if depth:
                    depth -= 1
                elif index > cursor:
                    return index
            elif token == "," and depth == 0:
                return index if index > cursor else None
            elif (
                depth == 0
                and token == "="
                and index + 1 < len(tokens)
                and tokens[index + 1] == ">"
            ):
                # `($e:expr => $name:ident)`: `_ARM_TOKEN` splits `=>`
                # into `=` and `>` (#507 review).
                return index if index > cursor else None
            index += 1
        return index if index > cursor else None
    return cursor + 1


def _macro_invoke_inners(
    masked: str,
    defs: list[tuple[str, int, int]],
    *,
    text: str | None = None,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> list[tuple[str, str]]:
    """`(name, invoke_inner)` for invocations outside macro definitions."""

    known_defs = {name for name, _, _ in defs}
    alias_entries = [
        entry
        for entry in _macro_use_alias_entries(
            masked, enabled_features=enabled_features
        )
        if entry[1] in known_defs
    ]
    aliases = {alias: original for alias, original, _s, _sc in alias_entries}
    if not known_defs:
        return []
    bodies = [(start, end) for _, start, end in defs]
    invoke_starts = {
        match.start()
        for match in _MACRO_INVOKE.finditer(masked)
        if not _qualified_macro_invocation(masked, match.start())
    }
    invoke_scopes = (
        _brace_scopes_at(masked, invoke_starts, bodies)
        if invoke_starts
        else {}
    )
    out: list[tuple[str, str]] = []
    for match in _MACRO_INVOKE.finditer(masked):
        if _qualified_macro_invocation(masked, match.start()):
            continue
        name = _resolve_macro_alias(
            _macro_name(match),
            aliases,
            entries=alias_entries,
            invoke_scope=invoke_scopes.get(match.start(), ()),
        )
        if name not in known_defs:
            continue
        if any(start <= match.start() < end for start, end in bodies):
            continue
        if text is not None and _position_cfg_inactive(
            text, masked, match.start(), enabled_features
        ):
            continue
        delim = match.end() - 1
        end = _balanced_pair_end(masked, delim)
        out.append((name, masked[delim + 1 : end - 1]))
    return out


def _macro_arm_spans(
    masked: str, body_start: int, body_end: int
) -> list[tuple[str, int, int]]:
    """`(matcher, inner_start, inner_end)` for each `macro_rules!` arm."""

    inner = masked[body_start:body_end]
    if not inner:
        return []
    i = 1
    n = max(0, len(inner) - 1)
    arms: list[tuple[str, int, int]] = []
    while i < n:
        while i < n and inner[i].isspace():
            i += 1
        if i >= n or inner[i] not in "{([":
            break
        matcher_end = _balanced_pair_end(inner, i)
        matcher = inner[i:matcher_end]
        j = matcher_end
        while j < n and inner[j].isspace():
            j += 1
        if j + 1 >= n or inner[j : j + 2] != "=>":
            break
        j += 2
        while j < n and inner[j].isspace():
            j += 1
        if j >= n or inner[j] not in "{([":
            break
        end = _balanced_pair_end(inner, j)
        arms.append(
            (matcher, body_start + j + 1, body_start + end - 1)
        )
        i = end
        while i < n and inner[i].isspace():
            i += 1
        if i < n and inner[i] == ";":
            i += 1
    return arms


def _simple_arm_accepts(matcher: str, invoke_inner: str) -> bool:
    inner = matcher.strip()
    if len(inner) >= 2 and inner[0] in "([{" and inner[-1] in ")]}":
        inner = inner[1:-1]
    if "$" not in inner:
        return _ARM_TOKEN.findall(inner) == _ARM_TOKEN.findall(invoke_inner)
    if re.search(r"\$\s*\(", inner):
        return True
    parts = re.split(r"\$[A-Za-z_][A-Za-z0-9_]*(?::[A-Za-z_][A-Za-z0-9_]*)?", inner)
    tokens = _ARM_TOKEN.findall(invoke_inner)
    metavars = _ARM_METAVAR.findall(inner)
    cursor = 0
    for index, piece in enumerate(parts):
        lits = _ARM_TOKEN.findall(piece)
        for lit in lits:
            if cursor >= len(tokens) or tokens[cursor] != lit:
                return False
            cursor += 1
        if index < len(parts) - 1:
            kind = metavars[index][1] if index < len(metavars) else "ident"
            nxt = _consume_kind(tokens, cursor, kind)
            if nxt is None:
                return False
            cursor = nxt
    return cursor == len(tokens)


_ARM_METAVAR = re.compile(
    r"\$([A-Za-z_][A-Za-z0-9_]*)(?::([A-Za-z_][A-Za-z0-9_]*))?"
)
_ARM_SUB = re.compile(r"\$([A-Za-z_][A-Za-z0-9_]*)")


def _bind_simple_arm(matcher: str, invoke_inner: str) -> dict[str, str]:
    """`$name` captures for the selected arm (#507 review)."""

    inner = matcher.strip()
    if len(inner) >= 2 and inner[0] in "([{" and inner[-1] in ")]}":
        inner = inner[1:-1]
    if "$" not in inner or re.search(r"\$\s*\(", inner):
        return {}
    names = _ARM_METAVAR.findall(inner)
    parts = re.split(r"\$[A-Za-z_][A-Za-z0-9_]*(?::[A-Za-z_][A-Za-z0-9_]*)?", inner)
    tokens = _ARM_TOKEN.findall(invoke_inner)
    cursor = 0
    bindings: dict[str, str] = {}
    name_i = 0
    for index, piece in enumerate(parts):
        lits = _ARM_TOKEN.findall(piece)
        for lit in lits:
            if cursor >= len(tokens) or tokens[cursor] != lit:
                return {}
            cursor += 1
        if index < len(parts) - 1:
            if cursor >= len(tokens) or name_i >= len(names):
                return {}
            kind = names[name_i][1]
            nxt = _consume_kind(tokens, cursor, kind)
            if nxt is None:
                return {}
            bindings[names[name_i][0]] = "".join(tokens[cursor:nxt])
            name_i += 1
            cursor = nxt
    if cursor != len(tokens):
        return {}
    return bindings


def _substitute_arm_metavars(arm: str, bindings: dict[str, str]) -> str:
    if not bindings:
        return arm

    def repl(match: re.Match[str]) -> str:
        name = match.group(1)
        if name == "crate":
            return match.group(0)
        return bindings.get(name, match.group(0))

    return _ARM_SUB.sub(repl, arm)


def _split_depth_sep(text: str, sep: str) -> list[str]:
    """Top-level `sep` split, ignoring commas inside `()`, `[]`, `{}`."""

    parts: list[str] = []
    depth = 0
    start = 0
    for i, ch in enumerate(text):
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth = max(0, depth - 1)
        elif ch == sep and depth == 0:
            part = text[start:i].strip()
            if part:
                parts.append(part)
            start = i + 1
    tail = text[start:].strip()
    if tail:
        parts.append(tail)
    return parts


def _find_dollar_repeat(
    text: str, start: int = 0
) -> tuple[int, str, str, str, int] | None:
    """Next `$(...)sep*` from `start`: (at, inner, sep, kind, end)."""

    n = len(text)
    i = start
    while i < n:
        if text[i] != "$":
            i += 1
            continue
        j = i + 1
        while j < n and text[j].isspace():
            j += 1
        if j >= n or text[j] != "(":
            i += 1
            continue
        close = _balanced_pair_end(text, j)
        inner = text[j + 1 : close - 1]
        k = close
        while k < n and text[k].isspace():
            k += 1
        sep = ""
        if k < n and text[k] in ",;":
            sep = text[k]
            k += 1
            while k < n and text[k].isspace():
                k += 1
        kind = ""
        if k < n and text[k] in "*+?":
            kind = text[k]
            k += 1
        if kind:
            return i, inner, sep, kind, k
        i += 1
    return None


def _repeat_matcher_inner(matcher: str) -> str:
    inner = matcher.strip()
    if len(inner) >= 2 and inner[0] in "([{" and inner[-1] in ")]}":
        inner = inner[1:-1]
    return inner


def _bind_repeat_rows(
    matcher: str, invoke_inner: str
) -> list[dict[str, str]] | None:
    """One binding dict per `$($name:ident),*` capture, or `None` if
    the matcher is not a repetition (#507 review)."""

    inner = _repeat_matcher_inner(matcher)
    found = _find_dollar_repeat(inner)
    if found is None:
        return None
    start, rep_inner, sep, _kind, end = found
    if inner[:start].strip() or inner[end:].strip():
        return None
    names = [pair[0] for pair in _ARM_METAVAR.findall(rep_inner)]
    if not names:
        return []
    raw = invoke_inner.strip()
    if not raw:
        return []
    if sep:
        pieces = _split_depth_sep(raw, sep)
    else:
        pieces = [t for t in _ARM_TOKEN.findall(raw) if t[0].isalpha() or t[0] == "_"]
    rows: list[dict[str, str]] = []
    for piece in pieces:
        idents = [
            t
            for t in _ARM_TOKEN.findall(piece)
            if t[0].isalpha() or t[0] == "_"
        ]
        if len(idents) < len(names):
            continue
        rows.append(dict(zip(names, idents)))
    return rows


def _expand_arm_repeats(arm: str, rows: list[dict[str, str]]) -> str:
    """Expand `$(...)*` in an arm body using captured rows."""

    out: list[str] = []
    i = 0
    n = len(arm)
    while i < n:
        found = _find_dollar_repeat(arm, i)
        if found is None:
            leftover = arm[i:]
            if len(rows) == 1:
                leftover = _substitute_arm_metavars(leftover, rows[0])
            out.append(leftover)
            break
        at, inner, sep, _kind, end = found
        out.append(arm[i:at])
        pieces = [_substitute_arm_metavars(inner, row) for row in rows]
        out.append(sep.join(pieces))
        i = end
    return "".join(out)


def _inactive_macro_spans(
    masked: str,
    *,
    text: str | None = None,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> list[tuple[int, int]]:
    """Spans rustc does not expand: uninvoked macros and unselected arms.

    One `generated!(cold)` must not unmask a sibling `(hot)` arm
    (#507 review). Cfg-disabled invocations do not select an arm
    (#507 review).
    """

    defs = _macro_rules_defs(masked)
    invokes_by_name: dict[str, list[str]] = {}
    for name, inner in _macro_invoke_inners(
        masked, defs, text=text, enabled_features=enabled_features
    ):
        invokes_by_name.setdefault(name, []).append(inner)
    inactive: list[tuple[int, int]] = []
    for name, start, end in defs:
        inners = invokes_by_name.get(name, [])
        if not inners:
            inactive.append((start, end))
            continue
        arms = _macro_arm_spans(masked, start, end)
        if not arms:
            continue
        selected: list[tuple[int, int]] = []
        for invoke_inner in inners:
            for matcher, arm_start, arm_end in arms:
                if _simple_arm_accepts(matcher, invoke_inner):
                    selected.append((arm_start, arm_end))
                    break
        selected.sort()
        cursor = start
        for arm_start, arm_end in selected:
            if cursor < arm_start:
                inactive.append((cursor, arm_start))
            cursor = max(cursor, arm_end)
        if cursor < end:
            inactive.append((cursor, end))
    return inactive


def _macro_rules_body_spans(masked: str) -> list[tuple[int, int]]:
    return _inactive_macro_spans(masked)


def parse_documented_hot_path(text: str) -> dict[str, int]:
    """CLAUDE.md's own `` `pattern` (N) `` entries in its hot-path
    paragraph -- the guard's source of truth, not a value copied into
    this file (#507 review). Raises `AssertionError` on any parse
    failure: a guard that cannot read its own source of truth must fail
    loudly, never fall back to an expectation nothing here checked.
    """
    start = text.find(_HOT_PATH_MARKER)
    if start == -1:
        raise AssertionError(
            f"could not find {_HOT_PATH_MARKER!r} in CLAUDE.md -- has the "
            "wording moved?"
        )
    end = text.find("\n\n", start)
    paragraph = text[start : end if end != -1 else len(text)]
    entries = dict(
        (name, int(count)) for name, count in _DOC_ENTRY.findall(paragraph)
    )
    if not entries:
        raise AssertionError(
            "parsed zero `pattern` (N) entries from CLAUDE.md's hot-path "
            "paragraph -- has the formatting moved?"
        )
    return entries


class _TestRecord(NamedTuple):
    package: str
    target: str
    name: str


def _package_name_for(path: Path, cache: dict[Path, str]) -> str:
    current = path.parent if path.is_file() else path
    for parent in (current, *current.parents):
        cached = cache.get(parent)
        if cached is not None:
            return cached
        manifest = parent / "Cargo.toml"
        if not manifest.is_file():
            continue
        try:
            text = manifest.read_text(encoding="utf-8")
        except OSError:
            continue
        name = package_name(text)
        if name:
            cache[parent] = name
            return name
    return ""


def _cargo_target_of(
    rs: Path,
    extra_roots: set[Path] | frozenset[Path] | None = None,
    test_names: dict[Path, str] | None = None,
    lib_roots: set[Path] | frozenset[Path] | None = None,
) -> str:
    """Cargo test target key matching `parse_workflow` (`lib`, `test:name`)."""

    key = rs.resolve()
    if test_names and key in test_names:
        return f"test:{test_names[key]}"
    split = _crate_source_rel(rs)
    if split is None:
        if lib_roots and key in lib_roots:
            return "lib"
        if extra_roots and key in extra_roots:
            return f"test:{rs.stem}"
        return ""
    marker, rest = split
    if marker == "src":
        if rest[:1] == ["bin"] or rest == ["main.rs"]:
            return ""
        return "lib"
    if marker == "tests":
        if not rest:
            return ""
        first = rest[0]
        stem = Path(first).stem if first.endswith(".rs") else first
        return f"test:{stem}"
    return ""


def _filters_contain_pattern(filters: set[str], pattern: str) -> bool:
    return pattern in filters or (EXACT_PREFIX + pattern) in filters


def _ci_feature_lanes(
    by_features: dict[str, dict[frozenset[str], dict[str, set[str]]]],
    pattern: str,
) -> list[tuple[str, frozenset[str], str, bool, str]]:
    """`(package, --features set, target, exact, filter)` lanes for `pattern`.

    A longer CI filter that contains `pattern` (for example
    `provider_error_body_preview_is_secret_free_and_bounded` covering
    documented `is_secret_free_`) still selects that lane's feature set
    so cfg-gated matches are counted. Matching uses the lane's actual
    filter token, not the shorter documented pattern (#507 review).
    """

    found: list[tuple[str, frozenset[str], str, bool, str]] = []
    for crate, featmap in by_features.items():
        for feat, targets in featmap.items():
            for target, filters in targets.items():
                hit: tuple[bool, str] | None = None
                if (EXACT_PREFIX + pattern) in filters:
                    hit = (True, pattern)
                elif pattern in filters:
                    hit = (False, pattern)
                else:
                    for filt in filters:
                        if filt.startswith(EXACT_PREFIX):
                            token = filt[len(EXACT_PREFIX) :]
                            if pattern != token and pattern in token:
                                hit = (True, token)
                                break
                        elif pattern != filt and pattern in filt:
                            hit = (False, filt)
                            break
                if hit is not None:
                    exact, token = hit
                    found.append((crate, feat, target, exact, token))
    return found


def _hot_path_matches_for_lanes(
    records_for_feat: dict[frozenset[str], list[_TestRecord]],
    pattern: str,
    lanes: list[tuple[str, frozenset[str], str, bool, str]],
) -> list[_TestRecord]:
    """Count tests selected by each lane's actual CI filter under its features.

    `pattern` is retained for call-site clarity; matching uses each lane's
    `filter` token so a covering longer filter cannot also select unrelated
    names that only share the documented substring (#507 review).
    """

    _ = pattern
    hits: list[_TestRecord] = []
    seen: set[tuple[str, str, str]] = set()
    for crate, feat, target, exact, filt in lanes:
        records = records_for_feat.get(feat)
        if records is None:
            continue
        for record in records:
            if exact:
                if record.name != filt:
                    continue
            elif filt not in record.name:
                continue
            if record.package != crate:
                continue
            if target != "*" and record.target != target:
                continue
            key = (record.package, record.target, record.name)
            if key in seen:
                continue
            seen.add(key)
            hits.append(record)
    return hits


def _ci_scopes_for_pattern(
    parsed: dict[str, dict[str, set[str]]], pattern: str
) -> set[tuple[str, str]] | None:
    """`(package, target)` pairs whose `run_nonzero` filter token is
    exactly `pattern`. `None` means no dedicated invocation -- count
    repo-wide (#507 review)."""

    found: set[tuple[str, str]] = set()
    for crate, targets in parsed.items():
        for target, filters in targets.items():
            if pattern in filters:
                found.add((crate, target))
    return found or None


def _hot_path_matches(
    records: list[_TestRecord],
    pattern: str,
    scopes: set[tuple[str, str]] | None,
) -> list[_TestRecord]:
    hits = [r for r in records if pattern in r.name]
    if scopes is None:
        return hits
    return [r for r in hits if (r.package, r.target) in scopes]


def _crate_source_rel(rs: Path) -> tuple[str, list[str]] | None:
    """(`src`|`tests`, remaining components including the filename).

    Anchored at the crate source root, not the last `src`/`tests` in the
    path. `src/agent/subagent/tests/rest.rs` is `agent::subagent::tests::rest`,
    not `rest` -- libtest uses the crate-root-relative module path
    (#507 review). Mirrors `check_new_tests_are_filtered.py`'s `_crate_split`.
    """
    parts = list(rs.parts)
    if "crates" in parts:
        i = parts.index("crates")
        # crates/<group>/<crate>/{src,tests}/... in the real tree;
        # crates/<crate>/{src,tests}/... in this file's unit fixtures.
        for crate_end in (i + 3, i + 2):
            if len(parts) > crate_end and parts[crate_end] in ("src", "tests"):
                return parts[crate_end], list(parts[crate_end + 1 :])
        return None
    if "prod" in parts or "third_party" in parts:
        marker = "prod" if "prod" in parts else "third_party"
        i = parts.index(marker)
        for j in range(i + 1, len(parts)):
            if parts[j] in ("src", "tests"):
                return parts[j], list(parts[j + 1 :])
        return None
    return None


def _path_module_prefix(rs: Path) -> list[str]:
    """Module path implied by a file under `src/` or `tests/`.

    `mod name;` in a parent file loads `name.rs` (or `name/mod.rs`);
    libtest then reports `name::fn`, not a bare `fn`. Walking only
    inline `mod X {` blocks misses that prefix (#507 review).

    Integration targets `tests/foo.rs` are a cargo test binary named
    after the stem: libtest reports `fn`, not `foo::fn`. Prefixing the
    stem would inflate CLAUDE.md counts (#507 review).
    """
    split = _crate_source_rel(rs)
    if split is None:
        return []
    marker, rest = split
    if not rest:
        return []
    if marker == "tests" and len(rest) == 1:
        return []
    rest = list(rest)
    rest[-1] = Path(rest[-1]).stem
    if rest[-1] in ("lib", "main", "mod"):
        rest.pop()
    if marker == "tests" and rest:
        # `tests/foo/bar.rs`: `foo` is the integration crate, not a module.
        rest = rest[1:]
    return list(rest)


def _is_lib_or_integration_source(
    rs: Path, extra_roots: set[Path] | frozenset[Path] | None = None
) -> bool:
    """CLAUDE.md measures `--lib` plus integration targets, not bins.

    `src/main.rs` and `src/bin/**` are cargo binary roots; a test there is
    not selected by the documented hot-path invocation (#507 review).
    An explicit `[[test]] path` outside `src/`/`tests/` is still a cargo
    integration target (#507 review).
    """
    if extra_roots and rs.resolve() in extra_roots:
        return True
    split = _crate_source_rel(rs)
    if split is None:
        return False
    marker, rest = split
    if marker == "src" and rest:
        if rest[0] == "bin":
            return False
        if rest == ["main.rs"]:
            return False
    return True


def _is_cargo_crate_root_file(
    rs: Path,
    extra_roots: set[Path] | frozenset[Path] | None = None,
    gated_roots: set[Path] | frozenset[Path] | None = None,
    suppressed_libs: set[Path] | frozenset[Path] | None = None,
    no_autotest: set[Path] | frozenset[Path] | None = None,
) -> bool:
    """`src/lib.rs`, an integration target `tests/*.rs`, or an explicit `[[test]].path`.

    `required-features` targets are off by default and are not part of the
    documented hot-path invocation (#507 review). An explicit `[lib] path`
    replaces `src/lib.rs` (#507 review). `autotests = false` turns off
    `tests/*.rs` auto-discovery (#507 review).
    """

    key = rs.resolve()
    if gated_roots and key in gated_roots:
        return False
    if extra_roots and key in extra_roots:
        return True
    if suppressed_libs and key in suppressed_libs:
        return False
    split = _crate_source_rel(rs)
    if split is None:
        return False
    marker, rest = split
    if marker == "src" and rest == ["lib.rs"]:
        return True
    if marker == "tests" and len(rest) == 1 and rest[0].endswith(".rs"):
        crate_dir = rs.parent.parent.resolve()
        if no_autotest and crate_dir in no_autotest:
            return False
        return True
    return False


def _load_manifest_toml(text: str) -> dict | None:
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return None
    return data if isinstance(data, dict) else None


def _optional_dep_features(data: dict) -> set[str]:
    """Implicit features Cargo synthesizes for `optional = true` deps."""

    names: set[str] = set()
    for table_name in (
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
    ):
        table = data.get(table_name)
        if not isinstance(table, dict):
            continue
        for name, spec in table.items():
            if isinstance(spec, dict) and spec.get("optional") is True:
                names.add(name)
    return names


def _toml_str_list(value: object) -> set[str]:
    if not isinstance(value, list):
        return set()
    return {item for item in value if isinstance(item, str)}


def _feature_closure(features: object, roots: set[str]) -> set[str]:
    """Features enabled by `roots`, including transitive `feature = [...]`
    edges (#507 review)."""

    table = features if isinstance(features, dict) else {}
    enabled = set(roots)
    stack = list(roots)
    while stack:
        name = stack.pop()
        for dep in _toml_str_list(table.get(name)):
            if dep.startswith("dep:"):
                continue
            if dep not in enabled:
                enabled.add(dep)
                stack.append(dep)
    return enabled


def _suppressed_optional_features(data: dict) -> set[str]:
    """Optional-dep names referenced as `dep:name` in `[features]`.

    That form suppresses Cargo's implicit same-named feature (#507 review).
    """

    names: set[str] = set()
    feats = data.get("features")
    if not isinstance(feats, dict):
        return names
    for value in feats.values():
        for item in _toml_str_list(value):
            if item.startswith("dep:"):
                names.add(item[4:])
    return names


def _manifest_default_features(text: str) -> set[str]:
    """Names listed in `[features] default = [...]`, including multiline."""

    data = _load_manifest_toml(text)
    if data is None:
        return set()
    feats = data.get("features")
    if not isinstance(feats, dict):
        return set()
    return _toml_str_list(feats.get("default"))


def _normalize_package_features(
    package: str, features: set[str] | frozenset[str]
) -> set[str]:
    """Map Cargo package-qualified selectors onto this package's feature names.

    `--features demo/hot` activates feature `hot` in package `demo`. Other
    packages' `name/feat` selectors are ignored for this manifest (#507).
    """

    out: set[str] = set()
    for feat in features:
        if "/" not in feat:
            out.add(feat)
            continue
        pkg, name = feat.split("/", 1)
        if pkg == package and name:
            out.add(name)
    return out


def _cargo_test_targets(
    root: Path,
    extra_features: frozenset[str] | set[str] | None = None,
    all_features: bool = False,
    no_default_features: bool = False,
) -> tuple[
    set[Path],
    set[Path],
    set[Path],
    set[Path],
    dict[Path, set[str]],
    dict[Path, str],
    set[Path],
]:
    """Explicit `[[test]]` paths and feature-gated integration targets.

    Extra roots: `path =` files cargo compiles without extra features.
    `tests/leader_pty_e2e/mod.rs` is one -- `_is_cargo_crate_root_file`'s
    `tests/*.rs` shape misses it (#507 review). `[lib] path = "lib/custom.rs"`
    is another: Cargo's library root is not always `src/lib.rs` (#507 review).

    Gated: `required-features` targets whose features are not all in the
    crate's `default` set. Default `cargo test` (the CLAUDE.md hot path)
    does not build those.
    """

    extra: set[Path] = set()
    gated: set[Path] = set()
    suppressed_libs: set[Path] = set()
    no_autotest: set[Path] = set()
    crate_feats: dict[Path, set[str]] = {}
    test_names: dict[Path, str] = {}
    lib_roots: set[Path] = set()
    extra_features = extra_features or frozenset()
    for manifest in root.rglob("Cargo.toml"):
        if "target" in manifest.parts:
            continue
        try:
            text = manifest.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        crate = manifest.parent
        data = _load_manifest_toml(text)
        pkg_name = crate.name
        if data is not None:
            pkg = data.get("package")
            if isinstance(pkg, dict):
                name = pkg.get("name")
                if isinstance(name, str) and name:
                    pkg_name = name
                if pkg.get("autotests") is False:
                    no_autotest.add(crate.resolve())
            lib = data.get("lib") if data is not None else None
            if (
                isinstance(pkg, dict)
                and pkg.get("autolib") is False
                and not isinstance(lib, dict)
            ):
                lib_rs = (crate / "src" / "lib.rs").resolve()
                suppressed_libs.add(lib_rs)
                gated.add(lib_rs)
            normalized_extra = _normalize_package_features(
                pkg_name, extra_features
            )
            feat_table = (
                data.get("features")
                if isinstance(data.get("features"), dict)
                else {}
            )
            default_feats = _feature_closure(
                feat_table, _toml_str_list(feat_table.get("default"))
            )
            if all_features:
                enabled = _feature_closure(
                    feat_table,
                    set(feat_table)
                    | (
                        _optional_dep_features(data)
                        - _suppressed_optional_features(data)
                    ),
                )
            elif no_default_features:
                enabled = _feature_closure(feat_table, set(normalized_extra))
            else:
                enabled = _feature_closure(
                    feat_table,
                    default_feats | {"default"} | set(normalized_extra),
                )
            crate_feats[crate.resolve()] = enabled
            lib = data.get("lib")
            if isinstance(lib, dict):
                lib_path = lib.get("path")
                lib_target = (
                    (crate / lib_path).resolve()
                    if isinstance(lib_path, str)
                    else (crate / "src" / "lib.rs").resolve()
                )
                if lib.get("test") is False or lib.get("harness") is False:
                    gated.add(lib_target)
                    if isinstance(lib_path, str):
                        suppressed_libs.add(
                            (crate / "src" / "lib.rs").resolve()
                        )
                elif isinstance(lib_path, str):
                    if lib_target.is_file():
                        extra.add(lib_target)
                        lib_roots.add(lib_target)
                    suppressed_libs.add((crate / "src" / "lib.rs").resolve())
            tests = data.get("test")
            if isinstance(tests, dict):
                tests = [tests]
            if not isinstance(tests, list):
                tests = []
            for table in tests:
                if not isinstance(table, dict):
                    continue
                name = table.get("name")
                path_s = table.get("path")
                required_feats = _toml_str_list(table.get("required-features"))
                target = None
                if isinstance(path_s, str):
                    target = (crate / path_s).resolve()
                elif isinstance(name, str):
                    target = (crate / "tests" / f"{name}.rs").resolve()
                if target is None:
                    continue
                if isinstance(name, str):
                    test_names[target] = name
                if table.get("test") is False or table.get("harness") is False:
                    gated.add(target)
                    continue
                extra_required = required_feats - enabled
                if extra_required:
                    gated.add(target)
                elif target.is_file():
                    extra.add(target)
            continue
        default_feats = set()
        in_test = False
        in_lib = False
        name: str | None = None
        path_s: str | None = None
        lib_path: str | None = None
        required_feats: set[str] = set()
        test_enabled = True
        harness_enabled = True
        lib_test_enabled = True
        lib_harness_enabled = True

        def flush() -> None:
            nonlocal name, path_s, required_feats, in_test, in_lib, lib_path, test_enabled, harness_enabled, lib_test_enabled, lib_harness_enabled
            if in_lib:
                if lib_path:
                    target = (crate / lib_path).resolve()
                    if not lib_test_enabled or not lib_harness_enabled:
                        gated.add(target)
                        suppressed_libs.add(
                            (crate / "src" / "lib.rs").resolve()
                        )
                    else:
                        if target.is_file():
                            extra.add(target)
                            lib_roots.add(target)
                        suppressed_libs.add(
                            (crate / "src" / "lib.rs").resolve()
                        )
                elif not lib_test_enabled or not lib_harness_enabled:
                    gated.add((crate / "src" / "lib.rs").resolve())
            if in_test:
                target = None
                if path_s:
                    target = (crate / path_s).resolve()
                elif name:
                    target = (crate / "tests" / f"{name}.rs").resolve()
                if target is not None:
                    if all_features:
                        enabled_feats = set(extra_features) | default_feats | {"default"}
                    elif no_default_features:
                        enabled_feats = set(extra_features)
                    else:
                        enabled_feats = default_feats | {"default"} | set(extra_features)
                    extra_required = set() if all_features else required_feats - enabled_feats
                    if name:
                        test_names[target.resolve()] = name
                    if not test_enabled or not harness_enabled:
                        gated.add(target)
                    elif extra_required:
                        gated.add(target)
                    elif target.is_file():
                        extra.add(target)
            name = None
            path_s = None
            lib_path = None
            required_feats = set()
            test_enabled = True
            harness_enabled = True
            lib_test_enabled = True
            lib_harness_enabled = True
            in_test = False
            in_lib = False

        for line in text.splitlines():
            stripped = line.strip()
            if stripped == "[[test]]":
                flush()
                in_test = True
                continue
            if stripped == "[lib]":
                flush()
                in_lib = True
                continue
            if stripped.startswith("["):
                flush()
                continue
            if in_lib:
                match = re.match(r'^path\s*=\s*"([^"]+)"', stripped)
                if match:
                    lib_path = match.group(1)
                    continue
                if re.match(r"^test\s*=\s*false\b", stripped):
                    lib_test_enabled = False
                    continue
                if re.match(r"^test\s*=\s*true\b", stripped):
                    lib_test_enabled = True
                    continue
                if re.match(r"^harness\s*=\s*false\b", stripped):
                    lib_harness_enabled = False
                    continue
                if re.match(r"^harness\s*=\s*true\b", stripped):
                    lib_harness_enabled = True
                    continue
                continue
            if not in_test:
                continue
            match = re.match(r'^name\s*=\s*"([^"]+)"', stripped)
            if match:
                name = match.group(1)
                continue
            match = re.match(r'^path\s*=\s*"([^"]+)"', stripped)
            if match:
                path_s = match.group(1)
                continue
            if re.match(r"^test\s*=\s*false\b", stripped):
                test_enabled = False
                continue
            if re.match(r"^test\s*=\s*true\b", stripped):
                test_enabled = True
                continue
            if re.match(r"^harness\s*=\s*false\b", stripped):
                harness_enabled = False
                continue
            if re.match(r"^harness\s*=\s*true\b", stripped):
                harness_enabled = True
                continue
            if stripped.startswith("required-features"):
                inner = stripped.split("=", 1)[-1]
                required_feats = set(re.findall(r'"([^"]+)"', inner))
        flush()
        if all_features:
            crate_feats[crate.resolve()] = set(extra_features) | default_feats | {"default"}
        elif no_default_features:
            crate_feats[crate.resolve()] = set(extra_features)
        else:
            crate_feats[crate.resolve()] = default_feats | {"default"} | set(extra_features)
    return extra, gated, suppressed_libs, no_autotest, crate_feats, test_names, lib_roots


def _features_for(path: Path, crate_feats: dict[Path, set[str]]) -> set[str]:
    current = path.resolve()
    if current.is_file():
        current = current.parent
    for parent in (current, *current.parents):
        found = crate_feats.get(parent)
        if found is not None:
            return found
    return set()


def _mod_search_dir(
    declaring: Path,
    extra_roots: set[Path] | frozenset[Path] | None = None,
    gated_roots: set[Path] | frozenset[Path] | None = None,
    *,
    as_crate_root: bool = False,
) -> Path:
    """Directory rustc searches for `mod name;` declared in `declaring`.

    An explicit `[[test]] path = "integration/custom.rs"` is a crate root,
    so `mod child;` loads `integration/child.rs`, not
    `integration/custom/child.rs` (#507 review). When the same file is
    later visited as `mod shared;`, it is a nested module and `mod child;`
    loads `shared/child.rs` (#507 review).
    """

    if declaring.name in ("lib.rs", "main.rs", "mod.rs"):
        return declaring.parent
    if as_crate_root:
        return declaring.parent
    return declaring.parent / declaring.stem


def _existing_mod_file(search_dir: Path, name: str) -> Path | None:
    for candidate in (search_dir / f"{name}.rs", search_dir / name / "mod.rs"):
        if candidate.is_file():
            return candidate.resolve()
    return None


def _iter_module_decls(
    text: str,
    declaring: Path,
    extra_roots: set[Path] | frozenset[Path] | None = None,
    gated_roots: set[Path] | frozenset[Path] | None = None,
    *,
    as_crate_root: bool = False,
    enabled_features: set[str] | frozenset[str] | None = None,
    masked: str | None = None,
    module_search_dir: Path | None = None,
) -> list[tuple[str, Path, tuple[str, ...], int, Path]]:
    """Resolved child modules plus offset and the child's search directory.

    Each tuple is `(name, child file, enclosing inline modules, source offset,
    child search directory)` for `#[path]` and
    ordinary `mod name;` that resolve.

    `mod inner;` inside `mod outer { ... }` is loaded from `outer/inner.rs`
    (or `outer/inner/mod.rs`) relative to the declaring file's module search
    dir, not from the declaring file's own directory, and libtest qualifies
    it as `outer::inner::...` (#507 review).
    """

    decls: list[tuple[str, Path, tuple[str, ...], int, Path]] = []
    pending_path: str | None = None
    pending_attrs: list[str] = []
    pending_mod = False
    pending_path_frag: str | None = None
    pending_path_mask: str | None = None
    raw_lines = text.splitlines()
    if masked is None:
        masked = _mask_rust_literals(text)
    masked_lines = masked.splitlines()
    if len(masked_lines) < len(raw_lines):
        masked_lines.extend([""] * (len(raw_lines) - len(masked_lines)))
    inactive = _inactive_macro_spans(
        masked, text=text, enabled_features=enabled_features
    )
    depth = 0
    inline_stack: list[tuple[int, str, bool]] = []
    line_start = 0
    for i, raw in enumerate(raw_lines):
        line = _strip_line_comment(masked_lines[i])
        raw_no_line_comment = _strip_line_comment(raw)
        stripped_raw = raw_no_line_comment.strip()
        attrs, remainder, _unclosed = _leading_attrs(line)
        enclosing_off = any(off for _, _, off in inline_stack)
        in_macro = any(start <= line_start < end for start, end in inactive)
        # `#[path = "x"]` stores the path in a string, so a full literal
        # mask blanks it. Search the line-comment-stripped raw text, then
        # keep the match only if that `#[path]` is live code: the `#`
        # survives the comment/string mask. Attributes may split across
        # lines (`#[path =\n"actual.rs"]`) (#507 review).
        raw_for_path = raw_no_line_comment
        mask_for_path = masked_lines[i]
        if pending_path_frag is not None:
            raw_for_path = pending_path_frag + "\n" + raw_no_line_comment
            mask_for_path = (pending_path_mask or "") + "\n" + masked_lines[i]
            pending_path_frag = None
            pending_path_mask = None
        redirected, unclosed_path = _live_path_redirect(
            raw_for_path, mask_for_path, enabled_features
        )
        if unclosed_path and re.search(
            r"#\[\s*(?:path|cfg_attr)\b", raw_for_path
        ):
            pending_path_frag = raw_for_path
            pending_path_mask = mask_for_path
        elif redirected:
            pending_path = redirected
            pending_attrs.extend(attrs)
        ident_semi = None
        ident_open = None
        if pending_mod:
            ident_semi = _IDENT_SEMI.match(line) or _IDENT_SEMI.match(remainder)
            if ident_semi is None:
                ident_open = _IDENT_OPEN.match(line) or _IDENT_OPEN.match(remainder)
        semi = ident_semi or _MOD_SEMI.match(line) or _MOD_SEMI.match(remainder)
        if semi:
            pending_mod = False
            cfg_off = any(
                _cfg_attr_is_inactive(a, enabled_features)
                for a in _effective_attrs(
                    pending_attrs + attrs, enabled_features
                )
            )
            skip = enclosing_off or cfg_off or in_macro
            if not skip:
                name = _fn_name(semi)
                inline_names = tuple(n for _, n, _ in inline_stack)
                search = module_search_dir or _mod_search_dir(
                    declaring,
                    extra_roots,
                    gated_roots,
                    as_crate_root=as_crate_root,
                )
                for inline_name in inline_names:
                    search = search / inline_name
                if pending_path:
                    # `#[path]` beside a file-level `mod` is relative to the
                    # declaring file's directory. Inside `mod outer { ... }`
                    # rustc loads `outer/<path>` (#507 review).
                    base = search if inline_names else declaring.parent
                    child = (base / pending_path).resolve()
                    decls.append(
                        (name, child, inline_names, line_start, child.parent)
                    )
                else:
                    child = _existing_mod_file(search, name)
                    if child is not None:
                        child_search = (
                            child.parent
                            if child.name == "mod.rs"
                            else child.parent / child.stem
                        )
                        decls.append(
                            (
                                name,
                                child,
                                inline_names,
                                line_start,
                                child_search,
                            )
                        )
            pending_path = None
            pending_attrs = []
        elif not redirected and pending_path_frag is None:
            cfg_off = any(
                _cfg_attr_is_inactive(a, enabled_features)
                for a in _effective_attrs(
                    pending_attrs + attrs, enabled_features
                )
            )
            skip = enclosing_off or cfg_off
            brace_mod = (
                ident_open
                or _MOD_OPEN.match(line)
                or _MOD_OPEN.match(remainder)
            )
            if brace_mod:
                inline_stack.append((depth, _fn_name(brace_mod), skip))
                pending_path = None
                pending_attrs = []
                pending_mod = False
            elif _MOD_KW_ONLY.match(line) or _MOD_KW_ONLY.match(remainder):
                pending_mod = True
                pending_attrs.extend(attrs)
            elif attrs and not remainder.strip():
                pending_attrs.extend(attrs)
            elif stripped_raw:
                pending_path = None
                pending_attrs = []
                pending_mod = False
        depth += line.count("{") - line.count("}")
        while inline_stack and depth <= inline_stack[-1][0]:
            inline_stack.pop()
        line_start += len(masked_lines[i]) + 1
    return decls


def _selected_macro_expansions(
    masked: str,
    enabled_features: set[str] | frozenset[str] | None = None,
    inherited_macros: tuple[tuple[str, str], ...] = (),
    *,
    text: str | None = None,
) -> list[tuple[str, tuple[str, ...]]]:
    """Selected `macro_rules!` arm bodies, with enclosing inline modules.

    Invoked expansions can emit `mod child;` items that rustc then loads
    beside the invocation. `_iter_module_decls` skips unexpanded
    `macro_rules!` bodies, so those children have to be recovered from
    the selected arm (#507 review).
    """

    defs = _macro_rules_defs(masked)
    scoped_defs = _scoped_macro_rules_sources(masked, defs, enabled_features)
    def_spans = [(start, end) for _, start, end in defs]
    base_names = {name for name, _source, _end, _scope in scoped_defs} | {
        name for name, _source in inherited_macros
    }
    aliases = {
        alias: original
        for alias, original in _macro_use_aliases(
            masked, enabled_features=enabled_features
        ).items()
        if original in base_names
    }
    alias_entries = [
        entry
        for entry in _macro_use_alias_entries(
            masked, enabled_features=enabled_features
        )
        if entry[1] in base_names
    ]
    available_names = base_names | set(aliases)
    raw_invocations = [
        match
        for match in _MACRO_INVOKE.finditer(masked)
        if not _qualified_macro_invocation(masked, match.start())
        if _macro_name(match) in available_names
        if not any(start <= match.start() < end for start, end in def_spans)
    ]
    brace_masked = _mask_attr_string_braces(masked)
    macro_spans = [
        (masked.rfind("macro_rules", 0, start), end)
        for _name, start, end in defs
    ]
    invoke_scopes = (
        _brace_scopes_at(
            brace_masked,
            {match.start() for match in raw_invocations},
            macro_spans,
        )
        if raw_invocations
        else {}
    )
    invoke_at: list[tuple[int, str, str, str]] = []
    for im in raw_invocations:
        inv_scope = invoke_scopes.get(im.start(), ())
        resolved = _resolve_macro_alias(
            _macro_name(im),
            aliases,
            entries=alias_entries,
            invoke_scope=inv_scope,
        )
        local = [
            (len(def_scope), end, source)
            for name, source, end, def_scope in scoped_defs
            if name == resolved
            and end <= im.start()
            and inv_scope[: len(def_scope)] == def_scope
        ]
        if local:
            source = max(local)[2]
        else:
            source = next(
                (
                    inherited_source
                    for name, inherited_source in inherited_macros
                    if name == resolved
                ),
                "",
            )
        if not source:
            continue
        delim = im.end() - 1
        end = _balanced_pair_end(masked, delim)
        invoke_at.append(
            (
                im.start(),
                resolved,
                masked[delim + 1 : end - 1],
                source,
            )
        )
    _append_crate_qualified_invokes(
        masked, scoped_defs, inherited_macros, def_spans, invoke_at
    )
    invoke_at.sort()
    expansions: list[tuple[str, tuple[str, ...]]] = []
    for _pos, inv_name, inner, source in invoke_at:
        if text is not None and _position_cfg_inactive(
            text, masked, _pos, enabled_features
        ):
            continue
        arm_text = _selected_arm_source(
            source, _macro_rules_defs(source), inv_name, inner
        )
        if arm_text:
            # Scope at the invocation column so `mod outer { emit!(); }`
            # loads `outer/child.rs`, not `child.rs` (#507 review).
            expansions.append(
                (arm_text, _inline_mods_at(masked, _pos, enabled_features))
            )
    return expansions


_EXPORTED_MACROS: dict[Path, tuple[tuple[str, str], ...]] = {}


def _exported_macro_sources(
    text: str,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> tuple[tuple[str, str], ...]:
    """`#[macro_export] macro_rules!` sources, crate-root visible."""

    return _child_macro_sources(
        text, enabled_features=enabled_features, require_export=True
    )


def _macro_use_module_sources(
    text: str,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> tuple[tuple[str, str], ...]:
    """Ordinary `macro_rules!` from a `#[macro_use] mod` child (#507)."""

    return _child_macro_sources(
        text, enabled_features=enabled_features, require_export=False
    )


def _child_macro_sources(
    text: str,
    *,
    enabled_features: set[str] | frozenset[str] | None = None,
    require_export: bool,
) -> tuple[tuple[str, str], ...]:
    masked = _mask_rust_literals(text)
    defs = _macro_rules_defs(masked)
    if not defs:
        return ()
    starts = [masked.rfind("macro_rules", 0, start) for _, start, _ in defs]
    spans = _outer_attr_spans(masked)
    ends = [end for _start, end, _attr in spans]
    out: list[tuple[str, str]] = []
    for start, (name, _body_start, body_end) in zip(starts, defs):
        if start < 0:
            continue
        attrs = _outer_attrs_before(masked, start, spans, ends)
        if require_export and not any(
            re.search(r"#\[\s*macro_export\b", attr) for attr in attrs
        ):
            continue
        if any(
            _cfg_attr_is_inactive(attr, enabled_features)
            for attr in _effective_attrs(attrs, enabled_features)
        ):
            continue
        out.append((name, masked[start:body_end]))
    return tuple(out)


def _mod_decl_has_macro_use(
    masked: str,
    offset: int,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> bool:
    """True when `#[macro_use]` precedes a `mod` declaration."""

    spans = _outer_attr_spans(masked)
    ends = [end for _start, end, _attr in spans]
    attrs = _outer_attrs_before(masked, offset, spans, ends)
    return any(
        re.search(r"#\[\s*macro_use\b", attr)
        and not _cfg_attr_is_inactive(attr, enabled_features)
        for attr in _effective_attrs(attrs, enabled_features)
    )


def _cargo_manifest_dir(path: Path) -> Path | None:
    current = path if path.is_dir() else path.parent
    for parent in (current, *current.parents):
        if (parent / "Cargo.toml").is_file():
            return parent
    return None


def _eval_concat_include_args(inner: str, declaring: Path) -> Path | None:
    """Resolve `concat!(env!("CARGO_MANIFEST_DIR"), \"/x.rs\")`."""

    manifest = _cargo_manifest_dir(declaring)
    pieces: list[str] = []
    for piece in _split_depth_sep(inner, ","):
        stripped = piece.strip()
        env = re.fullmatch(
            r'env!\s*\(\s*"CARGO_MANIFEST_DIR"\s*\)', stripped
        )
        if env is not None:
            if manifest is None:
                return None
            pieces.append(str(manifest))
            continue
        quoted = re.fullmatch(r'"([^"]*)"', stripped)
        if quoted is not None:
            pieces.append(quoted.group(1))
            continue
        raw = re.fullmatch(r'r(#*)"(.*?)"\1', stripped, re.DOTALL)
        if raw is not None:
            pieces.append(raw.group(2))
            continue
        return None
    if not pieces:
        return None
    joined = Path("".join(pieces))
    return joined if joined.is_file() else None


def _include_concat_hits(text: str) -> list[tuple[int, str]]:
    """`(offset, concat! args)` for `include!(concat!(...))`."""

    out: list[tuple[int, str]] = []
    index = 0
    while True:
        start = text.find("include!", index)
        if start < 0:
            break
        cursor = start + len("include!")
        while cursor < len(text) and text[cursor].isspace():
            cursor += 1
        if cursor >= len(text) or text[cursor] not in "([{":
            index = cursor + 1
            continue
        end = _balanced_pair_end(text, cursor)
        inner = text[cursor + 1 : end - 1].strip()
        if inner.startswith("concat!"):
            bang = inner.find("!")
            paren = inner.find("(", bang)
            if paren >= 0:
                close = _balanced_pair_end(inner, paren)
                out.append((start, inner[paren + 1 : close - 1]))
        index = end
    return out


def _lane_feature_args(
    feat: frozenset[str],
) -> tuple[frozenset[str], bool, bool]:
    all_features = ALL_FEATURES_TOKEN in feat
    no_default = NO_DEFAULT_FEATURES_TOKEN in feat
    named = frozenset(
        name
        for name in feat
        if name not in {ALL_FEATURES_TOKEN, NO_DEFAULT_FEATURES_TOKEN}
    )
    return named, all_features, no_default


def _declared_module_overrides(
    root: Path,
    extra_features: frozenset[str] | None = None,
    all_features: bool = False,
    no_default_features: bool = False,
) -> dict[Path, list[tuple[list[str], str, tuple[tuple[str, str], ...]]]]:
    """Logical module prefixes for files reached via `mod` / `#[path]`.

    Ordinary `mod common;` from several integration roots counts once per
    target (#507 review). `#[path]` prefixes propagate into descendant
    `mod child;` files so Cargo's logical path is what the scan records.
    Each occurrence carries the cargo target of the crate root that
    imported it, so `tests/common/mod.rs` pulled in by
    `--test shared_http_wire` is `test:shared_http_wire` not
    `test:common` (#507 review).
    """
    overrides: dict[
        Path,
        list[tuple[list[str], str, tuple[tuple[str, str], ...], str, frozenset[str]]],
    ] = {}
    queue: deque[
        tuple[
            Path,
            tuple[str, ...],
            tuple[Path, ...],
            bool,
            str,
            tuple[tuple[str, str], ...],
            Path,
            str,
            frozenset[str],
        ]
    ] = deque()
    pkg_cache: dict[Path, str] = {}
    texts: dict[Path, str] = {}
    global _EXPORTED_MACROS
    _EXPORTED_MACROS = {}

    def read_rs(rs: Path) -> str | None:
        key = rs.resolve()
        if key in texts:
            return texts[key]
        try:
            text = rs.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            return None
        texts[key] = text
        return text

    extra_roots, gated_roots, suppressed_libs, no_autotest, crate_feats, test_names, lib_roots = (
        _cargo_test_targets(
            root,
            extra_features=extra_features,
            all_features=all_features,
            no_default_features=no_default_features,
        )
    )
    for base in _CRATE_ROOTS:
        base_dir = root / base
        if not base_dir.is_dir():
            continue
        for rs in base_dir.rglob("*.rs"):
            if not _is_lib_or_integration_source(rs, extra_roots):
                continue
            if _is_cargo_crate_root_file(
                rs, extra_roots, gated_roots, suppressed_libs, no_autotest
            ):
                queue.append(
                    (
                        rs.resolve(),
                        (),
                        (),
                        True,
                        _cargo_target_of(rs, extra_roots, test_names, lib_roots),
                        (),
                        rs.resolve().parent,
                        _package_name_for(rs, pkg_cache),
                        frozenset(_features_for(rs, crate_feats)),
                    )
                )

    while queue:
        (
            declaring,
            prefix,
            ancestors,
            as_crate_root,
            root_target,
            macro_env,
            module_search_dir,
            origin_pkg,
            enabled,
        ) = queue.popleft()
        if declaring in ancestors:
            continue
        text = read_rs(declaring)
        if text is None:
            continue
        if _file_inner_cfg_inactive(text, enabled):
            continue
        masked = _mask_rust_literals(text)
        scoped_macros = _scoped_macro_rules_sources(
            masked, enabled_features=enabled
        )
        macro_spans = [
            (masked.rfind("macro_rules", 0, start), end)
            for _name, start, end in _macro_rules_defs(masked)
        ]
        include_hits: list[tuple[re.Match[str] | None, int, Path]] = []
        for inc in _INCLUDE.finditer(text):
            start = inc.start()
            if start >= len(masked) or not masked[start].isalpha():
                continue
            if _position_cfg_inactive(text, masked, start, enabled):
                continue
            included = (declaring.parent / _path_attr_value(inc)).resolve()
            if not included.is_file() or included == declaring:
                continue
            if included in ancestors:
                continue
            include_hits.append((inc, start, included))
        for start, concat_args in _include_concat_hits(text):
            if start >= len(masked) or not masked[start].isalpha():
                continue
            if _position_cfg_inactive(text, masked, start, enabled):
                continue
            included = _eval_concat_include_args(concat_args, declaring)
            if included is None or included == declaring:
                continue
            included = included.resolve()
            if included in ancestors:
                continue
            include_hits.append((None, start, included))
        include_scopes = (
            _brace_scopes_at(
                masked,
                {start for _inc, start, _included in include_hits},
                macro_spans,
            )
            if include_hits and scoped_macros
            else {}
        )
        for _inc, start, included in include_hits:
            inc_prefix = list(prefix) + list(
                _inline_mods_at(masked, start, enabled)
            )
            include_macro_env = (
                _macro_rules_sources_before(
                    scoped_macros,
                    start,
                    include_scopes.get(start, ()),
                )
                + macro_env
            )
            overrides.setdefault(included, []).append(
                (inc_prefix, root_target, include_macro_env, origin_pkg, enabled)
            )
            queue.append(
                (
                    included,
                    tuple(inc_prefix),
                    ancestors + (declaring,),
                    False,
                    root_target,
                    include_macro_env,
                    included.parent,
                    origin_pkg,
                    enabled,
                )
            )
        decls = list(
            _iter_module_decls(
                text,
                declaring,
                extra_roots,
                gated_roots,
                as_crate_root=as_crate_root,
                enabled_features=enabled,
                masked=masked,
                module_search_dir=module_search_dir,
            )
        )
        child_exports: list[tuple[str, str]] = []
        seen_export_names: set[str] = set()
        for _name, child, _inline, off, _search in decls:
            child_text = read_rs(child)
            if child_text is None:
                continue
            pieces = list(_exported_macro_sources(child_text, enabled))
            if _mod_decl_has_macro_use(masked, off, enabled):
                pieces.extend(_macro_use_module_sources(child_text, enabled))
            for mac_name, source in pieces:
                if mac_name in seen_export_names:
                    continue
                seen_export_names.add(mac_name)
                child_exports.append((mac_name, source))
        child_export_t = tuple(child_exports)
        _EXPORTED_MACROS[declaring.resolve()] = child_export_t
        for arm_text, expansion_inline in _selected_macro_expansions(
            masked, enabled, macro_env + child_export_t, text=text
        ):
            arm_search = module_search_dir
            for inline_name in expansion_inline:
                arm_search = arm_search / inline_name
            for decl in _iter_module_decls(
                arm_text,
                declaring,
                extra_roots,
                gated_roots,
                as_crate_root=as_crate_root and not expansion_inline,
                enabled_features=enabled,
                module_search_dir=arm_search,
            ):
                name, child, inner_inline, decl_offset, child_search_dir = decl
                decls.append(
                    (
                        name,
                        child,
                        expansion_inline + inner_inline,
                        decl_offset,
                        child_search_dir,
                    )
                )
        unique_decls = []
        seen_decls: set[tuple[str, Path, tuple[str, ...]]] = set()
        for decl in decls:
            key = (decl[0], decl[1], decl[2])
            if key in seen_decls:
                continue
            seen_decls.add(key)
            unique_decls.append(decl)
        decls = unique_decls
        decl_scopes = (
            _brace_scopes_at(
                masked,
                {decl_offset for _, _, _, decl_offset, _ in decls},
                macro_spans,
            )
            if scoped_macros
            else {}
        )
        for name, child, inline_names, decl_offset, child_search_dir in decls:
            child_prefix = list(prefix) + list(inline_names) + [name]
            child_macro_env = (
                _macro_rules_sources_before(
                    scoped_macros,
                    decl_offset,
                    decl_scopes.get(decl_offset, ()),
                )
                + macro_env
            )
            overrides.setdefault(child, []).append(
                (child_prefix, root_target, child_macro_env, origin_pkg, enabled)
            )
            queue.append(
                (
                    child,
                    tuple(child_prefix),
                    ancestors + (declaring,),
                    False,
                    root_target,
                    child_macro_env,
                    child_search_dir,
                    origin_pkg,
                    enabled,
                )
            )
    return overrides


_CFG_ATTR = re.compile(r"^#\[\s*cfg\s*\((.*)\)\s*\]\s*$", re.DOTALL)


def _host_target_arch() -> str:
    """Normalize `platform.machine()` to rustc `target_arch` names."""

    machine = platform.machine().lower()
    if machine in {"amd64", "x64"}:
        return "x86_64"
    if machine in {"arm64", "aarch64"}:
        return "aarch64"
    return machine


def _host_pointer_width() -> str:
    """Host pointer width as rustc `target_pointer_width` (decimal bits)."""

    return str(struct.calcsize("P") * 8)


def _host_target_env() -> str:
    """rustc `target_env` for this host (`gnu` / `msvc` / `musl` / empty)."""

    if sys.platform == "win32":
        return "msvc"
    if sys.platform == "darwin":
        return ""
    try:
        host = (sysconfig.get_config_var("HOST_GNU_TYPE") or "").lower()
        abi = (sysconfig.get_config_var("SOABI") or "").lower()
        if "musl" in host or "musl" in abi:
            return "musl"
    except (TypeError, ValueError):
        pass
    return "gnu"


def _host_target_vendor() -> str:
    """rustc `target_vendor` for this host."""

    if sys.platform == "darwin":
        return "apple"
    if sys.platform == "win32":
        return "pc"
    return "unknown"


def _decode_cooked_path(value: str) -> str:
    """Interpret cooked-string escapes (`\\x75` → `u`)."""

    try:
        return value.encode("utf-8").decode("unicode_escape")
    except UnicodeDecodeError:
        return value


def _path_attr_value(match: re.Match[str]) -> str:
    quoted = match.group(1)
    if quoted is not None:
        return _decode_cooked_path(quoted)
    return match.group(3) or ""


def _live_path_redirect(
    raw_for_path: str,
    mask_for_path: str,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> tuple[str | None, bool]:
    """Live `#[path]` or `#[cfg_attr(..., path = ...)]` value.

    `#[cfg_attr(test, path = "actual.rs")]` is `#[path]` under cargo test,
    so rustc loads `actual.rs` rather than the ordinary `child.rs`
    (#507 review). Returns `(path, unclosed)`.
    """

    attrs, _remainder, unclosed = _leading_attrs(raw_for_path)
    if unclosed:
        return None, True
    for attr in _effective_attrs(attrs, enabled_features):
        match = _PATH_ATTR.search(attr)
        if match is None:
            continue
        start = raw_for_path.find("#[")
        if start < 0 or start >= len(mask_for_path) or mask_for_path[start] != "#":
            continue
        return _path_attr_value(match), False
    path_match = _PATH_ATTR.search(raw_for_path)
    if path_match:
        start = path_match.start()
        if start < len(mask_for_path) and mask_for_path[start] == "#":
            return _path_attr_value(path_match), False
    return None, False


def _selected_arm_source(
    masked: str,
    defs: list[tuple[str, int, int]],
    name: str,
    invoke_inner: str,
) -> str | None:
    """Body of the first `macro_rules!` arm that accepts this invocation."""

    for def_name, start, end in defs:
        if def_name != name:
            continue
        arms = _macro_arm_spans(masked, start, end)
        if not arms:
            if start + 1 < end:
                return masked[start + 1 : end - 1]
            return None
        for matcher, arm_start, arm_end in arms:
            if _simple_arm_accepts(matcher, invoke_inner):
                arm = masked[arm_start:arm_end]
                rows = _bind_repeat_rows(matcher, invoke_inner)
                if rows is not None:
                    return _expand_arm_repeats(arm, rows)
                return _substitute_arm_metavars(
                    arm, _bind_simple_arm(matcher, invoke_inner)
                )
        return None
    return None


def _cfg_split_args(inner: str) -> list[str]:
    parts: list[str] = []
    depth = 0
    start = 0
    for i, ch in enumerate(inner):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        elif ch == "," and depth == 0:
            parts.append(inner[start:i].strip())
            start = i + 1
    tail = inner[start:].strip()
    if tail:
        parts.append(tail)
    return parts


def _cfg_atom(
    atom: str, enabled_features: set[str] | frozenset[str] | None = None
) -> bool | None:
    atom = atom.strip()
    if atom == "test":
        return True
    if atom == "true":
        return True
    if atom == "false":
        return False
    if atom == "unix":
        return sys.platform != "win32"
    if atom == "windows":
        return sys.platform == "win32"
    if atom == "macos":
        return sys.platform == "darwin"
    if atom == "linux":
        return sys.platform.startswith("linux")
    os_eq = re.fullmatch(r'target_os\s*=\s*"([^"]+)"', atom)
    if os_eq:
        wanted = os_eq.group(1)
        if sys.platform == "darwin":
            actual = "macos"
        elif sys.platform == "win32":
            actual = "windows"
        elif sys.platform.startswith("linux"):
            actual = "linux"
        else:
            actual = sys.platform
        return actual == wanted
    family = re.fullmatch(r'target_family\s*=\s*"([^"]+)"', atom)
    if family:
        fam = family.group(1)
        if fam == "unix":
            return sys.platform != "win32"
        if fam == "windows":
            return sys.platform == "win32"
    feat = re.fullmatch(r'feature\s*=\s*"([^"]+)"', atom)
    if feat:
        enabled = enabled_features if enabled_features is not None else set()
        return feat.group(1) in enabled
    arch_eq = re.fullmatch(r'target_arch\s*=\s*"([^"]+)"', atom)
    if arch_eq:
        return _host_target_arch() == arch_eq.group(1)
    width_eq = re.fullmatch(r'target_pointer_width\s*=\s*"([^"]+)"', atom)
    if width_eq:
        return _host_pointer_width() == width_eq.group(1)
    env_eq = re.fullmatch(r'target_env\s*=\s*"([^"]+)"', atom)
    if env_eq:
        return _host_target_env() == env_eq.group(1)
    vendor_eq = re.fullmatch(r'target_vendor\s*=\s*"([^"]+)"', atom)
    if vendor_eq:
        return _host_target_vendor() == vendor_eq.group(1)
    endian_eq = re.fullmatch(r'target_endian\s*=\s*"([^"]+)"', atom)
    if endian_eq:
        return sys.byteorder == endian_eq.group(1)
    if atom == "debug_assertions":
        # `cargo test` is the debug profile; the documented hot path is
        # never `--release` (#507 review).
        return True
    return None


def _eval_cfg(
    expr: str, enabled_features: set[str] | frozenset[str] | None = None
) -> bool | None:
    expr = expr.strip()
    for kind in ("not", "all", "any"):
        prefix = f"{kind}("
        if expr.startswith(prefix) and expr.endswith(")"):
            inner = expr[len(prefix) : -1]
            if kind == "not":
                value = _eval_cfg(inner, enabled_features)
                return None if value is None else (not value)
            values = [
                _eval_cfg(part, enabled_features) for part in _cfg_split_args(inner)
            ]
            if kind == "all":
                if any(v is False for v in values):
                    return False
                if any(v is None for v in values):
                    return None
                return True
            if any(v is True for v in values):
                return True
            if any(v is None for v in values):
                return None
            return False
    return _cfg_atom(expr, enabled_features)


def _attrs_before(source: str, code: str, position: int) -> list[str]:
    """`#[attr]` blocks immediately before ``position``."""

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


def _position_cfg_inactive(
    text: str,
    masked: str,
    pos: int,
    enabled_features: set[str] | frozenset[str] | None,
) -> bool:
    """True when attrs or an enclosing inline module cfg excludes ``pos``."""

    attrs = _attrs_before(text, masked, pos)
    if any(
        _cfg_attr_is_inactive(a, enabled_features)
        for a in _effective_attrs(attrs, enabled_features)
    ):
        return True
    stack: list[tuple[int, str, bool]] = []
    depth = 0
    offset = 0
    for mline in masked.splitlines(keepends=True):
        ml = mline.rstrip("\n")
        line_start = offset
        line_end = offset + len(mline)
        if pos < line_start:
            break
        if line_start <= pos < line_end:
            col = max(0, min(pos - line_start, len(ml)))
            stack = _mod_stack_at_column(
                ml, col, stack, depth, enabled_features
            )
            break
        stack = _mod_stack_at_column(
            ml, len(ml), stack, depth, enabled_features
        )
        depth += ml.count("{") - ml.count("}")
        offset = line_end
    return any(off for _, _, off in stack)


def _inline_mods_at(
    masked: str,
    pos: int,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> tuple[str, ...]:
    """Inline `mod name { ... }` names enclosing ``pos``."""

    stack: list[tuple[int, str, bool]] = []
    depth = 0
    offset = 0
    for mline in masked.splitlines(keepends=True):
        ml = mline.rstrip("\n")
        line_start = offset
        line_end = offset + len(mline)
        if pos < line_start:
            break
        if line_start <= pos < line_end:
            col = max(0, min(pos - line_start, len(ml)))
            stack = _mod_stack_at_column(
                ml, col, stack, depth, enabled_features
            )
            break
        stack = _mod_stack_at_column(
            ml, len(ml), stack, depth, enabled_features
        )
        depth += ml.count("{") - ml.count("}")
        offset = line_end
    return tuple(name for _, name, off in stack if not off)


def _cfg_attr_is_inactive(
    attr: str, enabled_features: set[str] | frozenset[str] | None = None
) -> bool:
    match = _CFG_ATTR.match(attr.strip())
    if match is None:
        return False
    return _eval_cfg(match.group(1).strip(), enabled_features) is False


_CFG_ATTR_WRAP = re.compile(r"^#\[\s*cfg_attr\s*\((.*)\)\s*\]\s*$", re.DOTALL)
_TEST_ATTR_HEAD = re.compile(r"#\[(?:tokio::)?test\b")


def _split_cfg_attr_args(inner: str) -> tuple[str, str] | None:
    depth = 0
    for i, ch in enumerate(inner):
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth = max(0, depth - 1)
        elif ch == "," and depth == 0:
            return inner[:i].strip(), inner[i + 1 :].strip()
    return None


def _expand_cfg_attr(
    attr: str,
    enabled_features: set[str] | frozenset[str] | None = None,
    depth: int = 0,
) -> list[str]:
    """`#[cfg_attr(test, ignore)]` becomes `#[ignore]` when `test` is on."""

    match = _CFG_ATTR_WRAP.match(attr.strip())
    if match is None:
        return [attr]
    split = _split_cfg_attr_args(match.group(1))
    if split is None:
        return [attr]
    pred, rest = split
    active = _eval_cfg(pred, enabled_features)
    if active is False:
        return []
    if active is not True:
        return [attr]
    out: list[str] = []
    for piece in _split_depth_sep(rest, ","):
        if not piece:
            continue
        emitted = f"#[{piece}]"
        if _CFG_ATTR_WRAP.match(emitted.strip()) and depth < 8:
            out.extend(_expand_cfg_attr(emitted, enabled_features, depth + 1))
        else:
            out.append(emitted)
    return out


def _effective_attrs(
    attrs: list[str], enabled_features: set[str] | frozenset[str] | None = None
) -> list[str]:
    out: list[str] = []
    for attr in attrs:
        out.extend(_expand_cfg_attr(attr, enabled_features))
    return out


def _is_test_attr(attr: str) -> bool:
    return bool(_TEST_ATTR_HEAD.match(attr.strip()))


def _compact_test_fns(
    masked_line: str,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> list[tuple[str, bool, bool, int]]:
    """`(name, inactive, ignored, column)` for each same-line `#[test] fn`."""

    out: list[tuple[str, bool, bool, int]] = []
    i = 0
    while True:
        j = masked_line.find("#[", i)
        if j < 0:
            break
        attrs, rest, _unclosed = _leading_attrs(masked_line[j:])
        effective = _effective_attrs(attrs, enabled_features)
        if not any(_is_test_attr(a) for a in effective):
            i = j + 2
            continue
        matched = _FN.match(rest)
        if matched is None:
            i = j + 2
            continue
        inactive = any(
            _cfg_attr_is_inactive(a, enabled_features) for a in effective
        )
        ignored = any(_IGNORE_ATTR.match(a.strip()) for a in effective)
        out.append((_fn_name(matched), inactive, ignored, j))
        consumed = len(masked_line[j:]) - len(rest) + matched.end()
        i = j + max(consumed, 2)
    return out


def _mod_stack_at_column(
    masked_line: str,
    column: int,
    start_stack: list[tuple[int, str, bool]],
    start_depth: int,
    enabled_features: set[str] | frozenset[str] | None = None,
) -> list[tuple[int, str, bool]]:
    """Module nest at `column` on this line, not the line-wide stack.

    `mod name { #[test] fn inner() {} } #[test] fn outer() {}` must
    qualify only `inner` (#507 review). `#[cfg(windows)] mod name { #[test]
    fn works() {} }` keeps the cfg inactive flag (#507 review).
    """

    stack = list(start_stack)
    depth = start_depth
    i = 0
    n = min(column, len(masked_line))
    mod_open = _INLINE_MOD_OPEN
    while i < n:
        matched = mod_open.match(masked_line, i)
        if matched:
            prefix = masked_line[: matched.start()]
            attr_at = prefix.rfind("#[")
            inactive = False
            if attr_at >= 0:
                attrs, _rest, _unclosed = _leading_attrs(prefix[attr_at:])
                inactive = any(
                    _cfg_attr_is_inactive(a, enabled_features)
                    for a in _effective_attrs(attrs, enabled_features)
                )
            stack.append((depth, _fn_name(matched), inactive))
            depth += 1
            i = matched.end()
            continue
        ch = masked_line[i]
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            while stack and depth <= stack[-1][0]:
                stack.pop()
        i += 1
    return stack


def _file_inner_cfg_inactive(
    text: str, enabled_features: set[str] | frozenset[str] | None = None
) -> bool:
    """True when the file is gated off by a leading `#![cfg(...)]`."""

    for raw in _mask_rust_literals(text).splitlines():
        stripped = raw.strip()
        if not stripped or stripped.startswith("//"):
            continue
        if "//" in stripped:
            stripped = stripped[: stripped.index("//")].rstrip()
        if stripped.startswith("#!["):
            if "cfg" in stripped:
                attr = stripped.replace("#![", "#[", 1)
                expanded = _effective_attrs([attr], enabled_features)
                if any(
                    _cfg_attr_is_inactive(item, enabled_features)
                    for item in expanded
                ):
                    return True
            continue
        return False
    return False


def _leading_attrs(line: str) -> tuple[list[str], str, bool]:
    """Leading `#[...]` attrs. The bool is True when a `#[` is still open."""

    attrs: list[str] = []
    i = 0
    n = len(line)
    while i < n:
        if line[i].isspace():
            i += 1
            continue
        if not line.startswith("#[", i):
            break
        depth = 0
        j = i
        while j < n:
            if line[j] == "[":
                depth += 1
            elif line[j] == "]":
                depth -= 1
                if depth == 0:
                    j += 1
                    attrs.append(line[i:j])
                    i = j
                    break
            j += 1
        else:
            return attrs, line[i:], True
    return attrs, line[i:], False


def _tests_in_file(
    text: str,
    file_mods: list[str],
    enabled_features: set[str] | frozenset[str] | None = None,
    inherited_macros: tuple[tuple[str, str], ...] = (),
    *,
    _expand_depth: int = 0,
) -> list[str]:
    if _expand_depth > 8 or _file_inner_cfg_inactive(text, enabled_features):
        return []
    names: list[str] = []
    raw_lines = text.splitlines()
    masked = _mask_rust_literals(text)
    masked_lines = masked.splitlines()
    brace_masked = _mask_attr_string_braces(masked)
    brace_lines = brace_masked.splitlines()
    if len(masked_lines) < len(raw_lines):
        masked_lines.extend([""] * (len(raw_lines) - len(masked_lines)))
    if len(brace_lines) < len(raw_lines):
        brace_lines.extend([""] * (len(raw_lines) - len(brace_lines)))
    defs = _macro_rules_defs(masked)
    scoped_defs = _scoped_macro_rules_sources(masked, defs, enabled_features)
    def_spans = [(start, end) for _, start, end in defs]
    macro_spans = [
        (masked.rfind("macro_rules", 0, start), end)
        for _name, start, end in defs
    ]
    available_names = {name for name, _source, _end, _scope in scoped_defs} | {
        name for name, _source in inherited_macros
    }
    aliases = {
        alias: original
        for alias, original in _macro_use_aliases(
            masked, enabled_features=enabled_features
        ).items()
        if original in available_names
    }
    alias_entries = [
        entry
        for entry in _macro_use_alias_entries(
            masked, enabled_features=enabled_features
        )
        if entry[1] in available_names
    ]
    available_names = available_names | set(aliases)
    raw_invocations = [
        match
        for match in _MACRO_INVOKE.finditer(masked)
        if not _qualified_macro_invocation(masked, match.start())
        if _macro_name(match) in available_names
        if not any(start <= match.start() < end for start, end in def_spans)
    ]
    invoke_scopes = (
        _brace_scopes_at(
            brace_masked,
            {match.start() for match in raw_invocations},
            macro_spans,
        )
        if raw_invocations
        else {}
    )
    invoke_at: list[tuple[int, str, str, str]] = []
    for im in raw_invocations:
        inv_scope = invoke_scopes.get(im.start(), ())
        resolved = _resolve_macro_alias(
            _macro_name(im),
            aliases,
            entries=alias_entries,
            invoke_scope=inv_scope,
        )
        local = [
            (len(def_scope), end, source)
            for name, source, end, def_scope in scoped_defs
            if name == resolved
            and end <= im.start()
            and inv_scope[: len(def_scope)] == def_scope
        ]
        if local:
            source = max(local)[2]
        else:
            source = next(
                (
                    inherited_source
                    for name, inherited_source in inherited_macros
                    if name == resolved
                ),
                "",
            )
        if not source:
            continue
        delim = im.end() - 1
        end = _balanced_pair_end(masked, delim)
        invoke_at.append(
            (
                im.start(),
                resolved,
                masked[delim + 1 : end - 1],
                source,
            )
        )
    _append_crate_qualified_invokes(
        masked, scoped_defs, inherited_macros, def_spans, invoke_at
    )
    invoke_at.sort()
    invoke_i = 0
    invocations: list[tuple[str, str, str, list[str]]] = []
    mod_stack: list[tuple[int, str, bool]] = []
    pending: list[str] = []
    pending_open_attr = ""
    depth = 0
    n = len(raw_lines)
    line_start = 0
    for i in range(n):
        masked_line = masked_lines[i]
        line_stack = list(mod_stack)
        line_depth = depth
        scan = (
            f"{pending_open_attr}\n{masked_line}"
            if pending_open_attr
            else masked_line
        )
        attrs, remainder_masked, unclosed = _leading_attrs(scan)
        if unclosed:
            pending_open_attr = remainder_masked
            if attrs:
                pending.extend(attrs)
            compact_fns = []
            has_test = False
            remainder_masked = ""
            attrs = []
        else:
            pending_open_attr = ""
            compact_fns = _compact_test_fns(masked_line, enabled_features)
            has_test = any(
                _is_test_attr(a)
                for a in _effective_attrs(pending + attrs, enabled_features)
            ) or bool(_TEST_ATTR.match(masked_line))
        enclosing_off = any(off for _, _, off in mod_stack)
        line_cfg_off = enclosing_off or any(
            _cfg_attr_is_inactive(a, enabled_features)
            for a in _effective_attrs(pending + attrs, enabled_features)
        )
        pushed_mod = False
        found = None

        if has_test:
            all_attrs = pending + attrs
            pending = []
            found = None
            for follow in [remainder_masked, *masked_lines[i + 1 :]]:
                follow = follow.strip()
                if follow.startswith("//"):
                    continue
                if follow.startswith("#["):
                    more, rest, _unclosed = _leading_attrs(follow)
                    all_attrs.extend(more)
                    matched = _FN.match(rest)
                    if matched:
                        found = _fn_name(matched)
                        break
                    continue
                if not follow:
                    continue
                matched = _FN.match(follow)
                if matched:
                    found = _fn_name(matched)
                    break
                if re.match(
                    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:mod|struct|enum|impl|use)\b",
                    follow,
                ):
                    break
                continue
            inactive = enclosing_off or any(
                _cfg_attr_is_inactive(a, enabled_features)
                for a in _effective_attrs(all_attrs, enabled_features)
            )
            ignored = any(
                _IGNORE_ATTR.match(a.strip())
                for a in _effective_attrs(all_attrs, enabled_features)
            )
            in_macro_def = any(start <= line_start < end for start, end in def_spans)
            if (
                found
                and not inactive
                and not ignored
                and not in_macro_def
                and not compact_fns
            ):
                prefix_parts = file_mods + [name for _, name, _ in mod_stack]
                prefix = "::".join(prefix_parts)
                names.append(f"{prefix}::{found}" if prefix else found)
        elif attrs and not remainder_masked.strip():
            pending.extend(attrs)
        elif remainder_masked.strip():
            item_off = enclosing_off or any(
                _cfg_attr_is_inactive(a, enabled_features)
                for a in _effective_attrs(pending + attrs, enabled_features)
            )
            pending = []
            line = _strip_line_comment(masked_line)
            mod_match = _MOD_OPEN.match(line) or _MOD_OPEN.match(
                remainder_masked
            )
            if mod_match:
                mod_stack.append((depth, _fn_name(mod_match), item_off))
                pushed_mod = True

        if not pushed_mod:
            inline_mod = _INLINE_MOD_OPEN.search(masked_line)
            if inline_mod is not None:
                mod_stack.append((depth, _fn_name(inline_mod), line_cfg_off))

        in_macro_def = any(start <= line_start < end for start, end in def_spans)
        seen_on_line: set[str] = set()
        if has_test and found and not compact_fns:
            prefix_parts = file_mods + [name for _, name, _ in mod_stack]
            prefix = "::".join(prefix_parts)
            seen_on_line.add(f"{prefix}::{found}" if prefix else found)
        for fname, cinactive, cignored, col in compact_fns:
            col_stack = _mod_stack_at_column(
                masked_line, col, line_stack, line_depth, enabled_features
            )
            if any(off for _, _, off in col_stack):
                continue
            prefix_parts = file_mods + [name for _, name, _ in col_stack]
            prefix = "::".join(prefix_parts)
            qualified = f"{prefix}::{fname}" if prefix else fname
            if qualified in seen_on_line:
                continue
            if enclosing_off or cinactive or cignored or in_macro_def:
                continue
            names.append(qualified)
            seen_on_line.add(qualified)

        invoke_end = line_start + len(masked_line)
        while (
            invoke_i < len(invoke_at)
            and line_start <= invoke_at[invoke_i][0] <= invoke_end
        ):
            _pos, inv_name, inner, source = invoke_at[invoke_i]
            invoke_i += 1
            if line_cfg_off:
                continue
            col_stack = _mod_stack_at_column(
                masked_line,
                _pos - line_start,
                line_stack,
                line_depth,
                enabled_features,
            )
            prefix = file_mods + [name for _, name, _ in col_stack]
            invocations.append((inv_name, inner, source, prefix))

        line = _strip_line_comment(brace_lines[i])
        if not remainder_masked.strip() or has_test:
            pass
        depth += line.count("{") - line.count("}")
        while mod_stack and depth <= mod_stack[-1][0]:
            mod_stack.pop()
        line_start += len(masked_line) + 1
    file_macros = inherited_macros + tuple(
        (name, src) for name, src, _end, _scope in scoped_defs
    )
    for inv_name, inner, source, prefix in invocations:
        defs = _macro_rules_defs(source)
        arm_text = _selected_arm_source(source, defs, inv_name, inner)
        if not arm_text:
            continue
        names.extend(
            _tests_in_file(
                arm_text,
                prefix,
                enabled_features,
                file_macros,
                _expand_depth=_expand_depth + 1,
            )
        )
    return names


def _module_prefixes_for_source(
    rs: Path,
    overrides: dict[
        Path,
        list[tuple[list[str], str, tuple[tuple[str, str], ...], str, frozenset[str]]],
    ],
    extra_roots: set[Path] | frozenset[Path] | None = None,
    gated_roots: set[Path] | frozenset[Path] | None = None,
    suppressed_libs: set[Path] | frozenset[Path] | None = None,
    no_autotest: set[Path] | frozenset[Path] | None = None,
    test_names: dict[Path, str] | None = None,
    lib_roots: set[Path] | frozenset[Path] | None = None,
    crate_feats: dict[Path, set[str]] | None = None,
) -> list[tuple[list[str], str, tuple[tuple[str, str], ...], str, frozenset[str]]] | None:
    """Prefixes to scan `rs` under, or `None` to skip an unreachable file.

    Cargo crate roots (`src/lib.rs`, `tests/*.rs`, explicit `[lib] path`
    / `[[test]] path`) are prefixless: libtest reports `fn`, not
    `file_stem::fn`. Nested files must appear in the module graph
    (#507 review); an orphan leftover after a `mod` was removed is not
    compiled and must not inflate CLAUDE.md counts. Each prefix is
    paired with the cargo target of the crate root that compiled it
    (#507 review).
    """

    key = rs.resolve()
    prefixes: list[
        tuple[list[str], str, tuple[tuple[str, str], ...], str, frozenset[str]]
    ] = []
    if key in overrides:
        prefixes.extend(overrides[key])
    if _is_cargo_crate_root_file(
        rs, extra_roots, gated_roots, suppressed_libs, no_autotest
    ):
        root_entry = (
            [],
            _cargo_target_of(rs, extra_roots, test_names, lib_roots),
            (),
            _package_name_for(rs, {}),
            frozenset(_features_for(rs, crate_feats or {})),
        )
        if root_entry not in prefixes:
            prefixes.append(root_entry)
    return prefixes or None


def _qualified_test_records(
    root: Path,
    extra_features: frozenset[str] | None = None,
) -> list[_TestRecord]:
    """Every `#[test]`/`#[tokio::test]` function under `crates/`/`prod/`
    with the cargo package and target that would compile it.

    Names are prefixed with the file-path module plus in-file `mod X { ... }`
    blocks. File-path prefix covers the common `mod name;` / `name.rs` shape
    libtest uses (#507 review). `#[path = "..."] mod name;` replaces the
    target file's path prefix with the declaring file's prefix plus
    `name`. Cross-file `mod x;`
    whose file is not `x.rs` and has no `#[path]` is still a shorter
    name than cargo would report -- conservative, same direction as
    the defect this guard exists to catch.

    `extra_features` is a CI lane's `--features` set (including the
    all-features sentinel) so cfg-gated tests in that lane are counted
    (#507 review).
    """
    named, all_features, no_default = _lane_feature_args(
        extra_features or frozenset()
    )
    overrides = _declared_module_overrides(
        root,
        extra_features=named,
        all_features=all_features,
        no_default_features=no_default,
    )
    extra_roots, gated_roots, suppressed_libs, no_autotest, crate_feats, test_names, lib_roots = (
        _cargo_test_targets(
            root,
            extra_features=named,
            all_features=all_features,
            no_default_features=no_default,
        )
    )
    records: list[_TestRecord] = []
    pkg_cache: dict[Path, str] = {}
    candidates: list[Path] = []
    seen: set[Path] = set()
    for base in _CRATE_ROOTS:
        base_dir = root / base
        if not base_dir.is_dir():
            continue
        for rs in base_dir.rglob("*.rs"):
            key = rs.resolve()
            if key in seen:
                continue
            seen.add(key)
            candidates.append(rs)
    for extra in overrides:
        key = extra.resolve()
        if key in seen:
            continue
        seen.add(key)
        candidates.append(extra)
    for rs in candidates:
        if (
            not _is_lib_or_integration_source(rs, extra_roots)
            and rs.resolve() not in overrides
        ):
            continue
        try:
            text = rs.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        prefix_lists = _module_prefixes_for_source(
            rs,
            overrides,
            extra_roots,
            gated_roots,
            suppressed_libs,
            no_autotest,
            test_names,
            lib_roots,
            crate_feats,
        )
        if prefix_lists is None:
            continue
        for file_mods, target, inherited_macros, origin_pkg, enabled in prefix_lists:
            if _file_inner_cfg_inactive(text, enabled):
                continue
            pkg = origin_pkg or _package_name_for(rs, pkg_cache)
            extra_macros = _EXPORTED_MACROS.get(rs.resolve(), ())
            for name in _tests_in_file(
                text, file_mods, enabled, inherited_macros + extra_macros
            ):
                records.append(_TestRecord(pkg, target, name))
    return records


def _qualified_test_names(root: Path) -> list[str]:
    return [record.name for record in _qualified_test_records(root)]


def _cargo_list_test_names(crate: Path) -> list[str]:
    """libtest names from Cargo, used as the oracle for parser fixtures."""

    completed = subprocess.run(
        ["cargo", "test", "--quiet", "--", "--list"],
        cwd=crate,
        check=True,
        capture_output=True,
        text=True,
    )
    return sorted(
        line.removesuffix(": test")
        for line in completed.stdout.splitlines()
        if line.endswith(": test")
    )


class CredentialHotPathCorpus(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.documented = parse_documented_hot_path(
            CLAUDE_MD.read_text(encoding="utf-8")
        )
        cls.records = _qualified_test_records(ROOT)
        cls.names = [record.name for record in cls.records]
        cls.ci_filters = parse_workflow(
            CI_YML.read_text(encoding="utf-8"), root=ROOT
        )
        cls.ci_by_features = parse_workflow_by_features(
            CI_YML.read_text(encoding="utf-8"), root=ROOT
        )
        needed: set[frozenset[str]] = set()
        for pattern in cls.documented:
            for _crate, feat, _target, _exact, _filt in _ci_feature_lanes(
                cls.ci_by_features, pattern
            ):
                if feat:
                    needed.add(feat)
        cls.records_for_feat: dict[frozenset[str], list[_TestRecord]] = {
            frozenset(): cls.records
        }
        for feat in needed:
            cls.records_for_feat[feat] = _qualified_test_records(
                ROOT, extra_features=feat
            )

    def test_the_corpus_is_not_empty(self):
        # A scan that silently finds nothing satisfies every assertion
        # below while checking nothing at all.
        self.assertGreater(len(self.names), 1000, len(self.names))

    def test_claude_md_still_documents_all_required_hot_path_entries(self):
        # A deleted named entry must not drop out of the count loop
        # because this assertion only listed the four patterns (#507 review).
        self.assertEqual(set(self.documented), _REQUIRED_HOT_PATH_ENTRIES)

    def test_dedicated_ci_filters_pin_their_package_and_target(self):
        # The documented substring is the filter token CI actually passes
        # to `run_nonzero`, not a family name that happens to match.
        self.assertEqual(
            _ci_scopes_for_pattern(self.ci_filters, "hostile_injector"),
            {("xai-grok-sampler", "lib")},
        )
        self.assertEqual(
            _ci_scopes_for_pattern(self.ci_filters, "omits_xai_identity"),
            {("xai-grok-sampler", "lib")},
        )
        self.assertEqual(
            _ci_scopes_for_pattern(self.ci_filters, "none_auth_scheme_"),
            {("xai-grok-sampler", "test:shared_http_wire")},
        )
        self.assertIsNone(
            _ci_scopes_for_pattern(self.ci_filters, "is_secret_free_")
        )

    def test_each_documented_entry_selects_its_documented_count(self):
        # The counterexample CLAUDE.md's own commit history should never
        # reproduce (#507 review): editing only this file's number, with
        # nothing about the source changing, must turn this test red --
        # the guard checks CLAUDE.md's count against source, not a copy
        # of the count against itself. A dedicated CI filter is counted
        # only inside that invocation's package and cargo target, so a
        # sampler-lib test cannot be replaced by a same-pattern test in
        # another crate without reddening the count (#507 review).
        wrong = {}
        for pattern, expected in self.documented.items():
            # Dedicated `run_nonzero` tokens stay package/target-scoped.
            # Patterns with no dedicated invocation (today
            # `is_secret_free_`) stay repo-wide, while covering longer
            # filters still contribute cfg-gated feature-lane hits
            # (#507 review / CLAUDE.md).
            scopes = _ci_scopes_for_pattern(self.ci_filters, pattern)
            lanes = _ci_feature_lanes(self.ci_by_features, pattern)
            if scopes is not None:
                matched = _hot_path_matches_for_lanes(
                    self.records_for_feat, pattern, lanes
                )
            else:
                matched = list(
                    _hot_path_matches(self.records, pattern, None)
                )
                seen = {
                    (r.package, r.target, r.name) for r in matched
                }
                for record in _hot_path_matches_for_lanes(
                    self.records_for_feat, pattern, lanes
                ):
                    key = (record.package, record.target, record.name)
                    if key in seen:
                        continue
                    seen.add(key)
                    matched.append(record)
            if len(matched) != expected:
                wrong[pattern] = (
                    len(matched),
                    expected,
                    [(r.package, r.target, r.name) for r in matched],
                    scopes
                    if scopes is not None
                    else {
                        (crate, target)
                        for crate, _feat, target, _exact, _filt in lanes
                    },
                )
        self.assertEqual(
            wrong,
            {},
            f"CLAUDE.md's documented count does not match source (got, "
            f"documented, matches, ci-scopes): {wrong}",
        )


class CiPackageTargetCounts(unittest.TestCase):
    def test_same_pattern_in_another_package_does_not_fill_the_ci_count(self):
        """`hostile_injector` is `-p xai-grok-sampler --lib` in CI; a
        same-pattern test added to another crate must not keep that
        count green (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            sampler = root / "crates" / "codegen" / "xai-grok-sampler"
            (sampler / "src").mkdir(parents=True)
            (sampler / "Cargo.toml").write_text(
                '[package]\nname = "xai-grok-sampler"\n'
            )
            (sampler / "src" / "lib.rs").write_text(
                "#[test]\nfn none_scheme_post_strips_auth_headers_after_hostile_injector() {}\n"
            )
            other = root / "crates" / "codegen" / "xai-grok-shell"
            (other / "src").mkdir(parents=True)
            (other / "Cargo.toml").write_text(
                '[package]\nname = "xai-grok-shell"\n'
            )
            (other / "src" / "lib.rs").write_text(
                "#[test]\nfn extra_hostile_injector_elsewhere() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --lib "
                "hostile_injector -- --nocapture\n"
            )
            records = _qualified_test_records(root)
            scopes = _ci_scopes_for_pattern(
                parse_workflow(wf, root=root), "hostile_injector"
            )
            matched = _hot_path_matches(records, "hostile_injector", scopes)
            self.assertEqual(
                [r.name for r in matched],
                ["none_scheme_post_strips_auth_headers_after_hostile_injector"],
            )
            self.assertEqual(
                {(r.package, r.target) for r in matched},
                {("xai-grok-sampler", "lib")},
            )

    def test_lib_match_does_not_fill_an_integration_filter_count(self):
        """`none_auth_scheme_` CI is `--test shared_http_wire`, not `--lib`
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            (crate / "src").mkdir(parents=True)
            (crate / "tests").mkdir()
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "xai-grok-sampler"\n'
            )
            (crate / "src" / "lib.rs").write_text(
                "#[test]\nfn none_auth_scheme_in_lib() {}\n"
            )
            (crate / "tests" / "shared_http_wire.rs").write_text(
                "#[test]\nfn none_auth_scheme_on_the_wire() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --test "
                "shared_http_wire none_auth_scheme_ -- --nocapture\n"
            )
            records = _qualified_test_records(root)
            scopes = _ci_scopes_for_pattern(
                parse_workflow(wf, root=root), "none_auth_scheme_"
            )
            matched = _hot_path_matches(records, "none_auth_scheme_", scopes)
            self.assertEqual(
                [r.name for r in matched],
                ["none_auth_scheme_on_the_wire"],
            )
            self.assertEqual(
                {(r.package, r.target) for r in matched},
                {("xai-grok-sampler", "test:shared_http_wire")},
            )

    def test_imported_integration_module_keeps_the_root_target(self):
        """`mod common;` from `--test shared_http_wire` compiles under
        `test:shared_http_wire`, not `test:common` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            tests = crate / "tests"
            (crate / "src").mkdir(parents=True)
            (tests / "common").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "xai-grok-sampler"\n'
            )
            (crate / "src" / "lib.rs").write_text("")
            (tests / "shared_http_wire.rs").write_text("mod common;\n")
            (tests / "common" / "mod.rs").write_text(
                "#[test]\nfn none_auth_scheme_from_common() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --test "
                "shared_http_wire none_auth_scheme_ -- --nocapture\n"
            )
            records = _qualified_test_records(root)
            scopes = _ci_scopes_for_pattern(
                parse_workflow(wf, root=root), "none_auth_scheme_"
            )
            matched = _hot_path_matches(records, "none_auth_scheme_", scopes)
            self.assertEqual(
                [(r.target, r.name) for r in matched],
                [("test:shared_http_wire", "common::none_auth_scheme_from_common")],
            )

    def test_explicit_test_name_is_the_cargo_target(self):
        """`[[test]] name = \"renamed\"` is `--test renamed`, not the
        file stem (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            integ = crate / "integration"
            (crate / "src").mkdir(parents=True)
            integ.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"xai-grok-sampler\"\n\n"
                "[[test]]\nname = \"renamed\"\n"
                'path = "integration/custom.rs"\n',
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text("")
            (integ / "custom.rs").write_text(
                "#[test]\nfn none_auth_scheme_renamed() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --test "
                "renamed none_auth_scheme_ -- --nocapture\n"
            )
            records = _qualified_test_records(root)
            scopes = _ci_scopes_for_pattern(
                parse_workflow(wf, root=root), "none_auth_scheme_"
            )
            matched = _hot_path_matches(records, "none_auth_scheme_", scopes)
            self.assertEqual(
                [(r.target, r.name) for r in matched],
                [("test:renamed", "none_auth_scheme_renamed")],
            )

    def test_pattern_without_a_ci_invocation_stays_repo_wide(self):
        """`is_secret_free_` is not a `run_nonzero` token; its count stays
        the repo-wide substring total (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            sampler = root / "crates" / "codegen" / "xai-grok-sampler"
            (sampler / "src").mkdir(parents=True)
            (sampler / "Cargo.toml").write_text(
                '[package]\nname = "xai-grok-sampler"\n'
            )
            (sampler / "src" / "lib.rs").write_text(
                "#[test]\nfn provider_controlled_stream_error_is_secret_free_for_all_stream_apis() {}\n"
            )
            types = root / "crates" / "codegen" / "xai-grok-sampling-types"
            (types / "src").mkdir(parents=True)
            (types / "Cargo.toml").write_text(
                '[package]\nname = "xai-grok-sampling-types"\n'
            )
            (types / "src" / "lib.rs").write_text(
                "#[test]\nfn provider_error_body_preview_is_secret_free_and_bounded() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --lib "
                "hostile_injector -- --nocapture\n"
            )
            records = _qualified_test_records(root)
            scopes = _ci_scopes_for_pattern(
                parse_workflow(wf, root=root), "is_secret_free_"
            )
            self.assertIsNone(scopes)
            matched = _hot_path_matches(records, "is_secret_free_", scopes)
            self.assertEqual(
                sorted(r.name for r in matched),
                [
                    "provider_controlled_stream_error_is_secret_free_for_all_stream_apis",
                    "provider_error_body_preview_is_secret_free_and_bounded",
                ],
            )

    def test_longer_feature_filter_covers_documented_pattern(self):
        """A `--features hot` lane whose filter contains
        `is_secret_free_` counts cfg-gated matches (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"xai-grok-sampler\"\n\n"
                "[features]\nhot = []\n",
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[cfg(feature = \"hot\")]\n"
                "#[test]\n"
                "fn provider_error_body_preview_is_secret_free_and_bounded() {}\n"
                "#[cfg(feature = \"hot\")]\n"
                "#[test]\n"
                "fn unrelated_is_secret_free_case() {}\n"
                "#[test]\n"
                "fn unrelated_cold_case() {}\n",
                encoding="utf-8",
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --features hot "
                "--lib provider_error_body_preview_is_secret_free_and_bounded "
                "-- --nocapture\n"
            )
            by_feat = parse_workflow_by_features(wf, root=root)
            lanes = _ci_feature_lanes(by_feat, "is_secret_free_")
            self.assertTrue(lanes)
            self.assertEqual(
                {(exact, filt) for _c, _f, _t, exact, filt in lanes},
                {
                    (
                        False,
                        "provider_error_body_preview_is_secret_free_and_bounded",
                    )
                },
            )
            records_for_feat = {
                frozenset(): _qualified_test_records(root),
                frozenset({"hot"}): _qualified_test_records(
                    root, extra_features=frozenset({"hot"})
                ),
            }
            matched = _hot_path_matches_for_lanes(
                records_for_feat, "is_secret_free_", lanes
            )
            self.assertEqual(
                [r.name for r in matched],
                ["provider_error_body_preview_is_secret_free_and_bounded"],
            )

    def test_package_qualified_feature_activates_cfg_gated_tests(self):
        """`--features demo/hot` enables feature `hot` in package `demo`
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "demo"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\n\n'
                "[features]\nhot = []\n",
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                '#[cfg(feature = "hot")]\n'
                "#[test]\nfn none_auth_scheme_hot() {}\n"
                "#[test]\nfn none_auth_scheme_cold() {}\n",
                encoding="utf-8",
            )
            wf = (
                "          run_nonzero -p demo --features demo/hot "
                "--lib none_auth_scheme_ -- --nocapture\n"
            )
            by_feat = parse_workflow_by_features(wf, root=root)
            lanes = _ci_feature_lanes(by_feat, "none_auth_scheme_")
            feat = frozenset({"demo/hot"})
            records_for_feat = {
                feat: _qualified_test_records(root, extra_features=feat)
            }
            matched = _hot_path_matches_for_lanes(
                records_for_feat, "none_auth_scheme_", lanes
            )
            self.assertEqual(
                sorted(r.name for r in matched),
                ["none_auth_scheme_cold", "none_auth_scheme_hot"],
            )

    def test_features_lane_counts_cfg_gated_and_required_feature_tests(self):
        """A `--features hot` lane must count `#[cfg(feature = \"hot\")]`
        tests and `required-features` targets that those features
        unlock (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            (crate / "src").mkdir(parents=True)
            (crate / "tests").mkdir()
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"xai-grok-sampler\"\n\n"
                "[features]\nhot = []\n\n"
                "[[test]]\nname = \"wire\"\n"
                'path = "tests/wire.rs"\n'
                'required-features = ["hot"]\n',
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[cfg(feature = \"hot\")]\n"
                "#[test]\nfn none_auth_scheme_hot() {}\n"
                "#[test]\nfn none_auth_scheme_cold() {}\n"
            )
            (crate / "tests" / "wire.rs").write_text(
                "#[test]\nfn none_auth_scheme_wire() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --features hot "
                "--lib none_auth_scheme_ -- --nocapture\n"
                "          run_nonzero -p xai-grok-sampler --features hot "
                "--test wire none_auth_scheme_ -- --nocapture\n"
            )
            by_feat = parse_workflow_by_features(wf, root=root)
            lanes = _ci_feature_lanes(by_feat, "none_auth_scheme_")
            feat = frozenset({"hot"})
            records_for_feat = {
                feat: _qualified_test_records(root, extra_features=feat)
            }
            matched = _hot_path_matches_for_lanes(
                records_for_feat, "none_auth_scheme_", lanes
            )
            self.assertEqual(
                sorted((r.target, r.name) for r in matched),
                [
                    ("lib", "none_auth_scheme_cold"),
                    ("lib", "none_auth_scheme_hot"),
                    ("test:wire", "none_auth_scheme_wire"),
                ],
            )

    def test_all_features_lane_counts_cfg_gated_tests(self):
        """`--all-features` must enable cfg-gated hot-path tests
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"xai-grok-sampler\"\n\n"
                "[features]\nhot = []\n",
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[cfg(feature = \"hot\")]\n"
                "#[test]\nfn none_auth_scheme_hot() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --all-features "
                "--lib none_auth_scheme_ -- --nocapture\n"
            )
            by_feat = parse_workflow_by_features(wf, root=root)
            lanes = _ci_feature_lanes(by_feat, "none_auth_scheme_")
            feat = frozenset({ALL_FEATURES_TOKEN})
            records_for_feat = {
                feat: _qualified_test_records(root, extra_features=feat)
            }
            matched = _hot_path_matches_for_lanes(
                records_for_feat, "none_auth_scheme_", lanes
            )
            self.assertEqual([r.name for r in matched], ["none_auth_scheme_hot"])

    def test_all_features_includes_optional_dependency_features(self):
        """`--all-features` enables implicit optional-dep features
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"xai-grok-sampler\"\n\n"
                "[dependencies]\n"
                'dep = { version = "1.0", optional = true }\n',
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[cfg(feature = \"dep\")]\n"
                "#[test]\nfn none_auth_scheme_dep() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --all-features "
                "--lib none_auth_scheme_ -- --nocapture\n"
            )
            by_feat = parse_workflow_by_features(wf, root=root)
            lanes = _ci_feature_lanes(by_feat, "none_auth_scheme_")
            feat = frozenset({ALL_FEATURES_TOKEN})
            records_for_feat = {
                feat: _qualified_test_records(root, extra_features=feat)
            }
            matched = _hot_path_matches_for_lanes(
                records_for_feat, "none_auth_scheme_", lanes
            )
            self.assertEqual([r.name for r in matched], ["none_auth_scheme_dep"])

    def test_all_features_suppresses_dep_colon_optional_features(self):
        """`hot = [\"dep:dep\"]` does not enable `feature = \"dep\"`
        under `--all-features` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"xai-grok-sampler\"\n\n"
                "[features]\nhot = [\"dep:dep\"]\n\n"
                "[dependencies]\n"
                'dep = { version = "1.0", optional = true }\n',
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[cfg(feature = \"hot\")]\n"
                "#[test]\nfn none_auth_scheme_hot() {}\n"
                "#[cfg(feature = \"dep\")]\n"
                "#[test]\nfn none_auth_scheme_dep() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --all-features "
                "--lib none_auth_scheme_ -- --nocapture\n"
            )
            by_feat = parse_workflow_by_features(wf, root=root)
            lanes = _ci_feature_lanes(by_feat, "none_auth_scheme_")
            feat = frozenset({ALL_FEATURES_TOKEN})
            records_for_feat = {
                feat: _qualified_test_records(root, extra_features=feat)
            }
            matched = _hot_path_matches_for_lanes(
                records_for_feat, "none_auth_scheme_", lanes
            )
            self.assertEqual([r.name for r in matched], ["none_auth_scheme_hot"])

    def test_default_cfg_name_is_enabled_with_manifest_defaults(self):
        """Cargo enables `feature = \"default\"` when defaults are on
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"xai-grok-sampler\"\n\n"
                "[features]\ndefault = [\"hot\"]\nhot = []\n",
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[cfg(feature = \"default\")]\n"
                "#[test]\nfn none_auth_scheme_default_cfg() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --lib "
                "none_auth_scheme_ -- --nocapture\n"
            )
            by_feat = parse_workflow_by_features(wf, root=root)
            lanes = _ci_feature_lanes(by_feat, "none_auth_scheme_")
            feat = frozenset()
            records_for_feat = {
                feat: _qualified_test_records(root, extra_features=feat)
            }
            matched = _hot_path_matches_for_lanes(
                records_for_feat, "none_auth_scheme_", lanes
            )
            self.assertEqual(
                [r.name for r in matched], ["none_auth_scheme_default_cfg"]
            )

    def test_no_default_features_lane_does_not_count_default_cfg(self):
        """`--no-default-features` must not reuse the default-feature
        lane key (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"xai-grok-sampler\"\n\n"
                "[features]\ndefault = [\"hot\"]\nhot = []\n",
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[cfg(feature = \"hot\")]\n"
                "#[test]\nfn none_auth_scheme_hot() {}\n"
                "#[cfg(not(feature = \"hot\"))]\n"
                "#[test]\nfn none_auth_scheme_off() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler "
                "--no-default-features --lib none_auth_scheme_ "
                "-- --nocapture\n"
            )
            by_feat = parse_workflow_by_features(wf, root=root)
            lanes = _ci_feature_lanes(by_feat, "none_auth_scheme_")
            self.assertTrue(
                any(NO_DEFAULT_FEATURES_TOKEN in feat for _c, feat, _t, _e in lanes)
            )
            feat = frozenset({NO_DEFAULT_FEATURES_TOKEN})
            records_for_feat = {
                feat: _qualified_test_records(root, extra_features=feat)
            }
            matched = _hot_path_matches_for_lanes(
                records_for_feat, "none_auth_scheme_", lanes
            )
            self.assertEqual([r.name for r in matched], ["none_auth_scheme_off"])

    def test_exact_lane_counts_only_the_equal_name(self):
        """`--exact` must not count substring superstrings (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "xai-grok-sampler"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "xai-grok-sampler"\n',
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[test]\nfn none_auth_scheme_exact() {}\n"
                "#[test]\nfn none_auth_scheme_exact_extra() {}\n"
            )
            wf = (
                "          run_nonzero -p xai-grok-sampler --lib "
                "none_auth_scheme_exact -- --exact --nocapture\n"
            )
            by_feat = parse_workflow_by_features(wf, root=root)
            lanes = _ci_feature_lanes(by_feat, "none_auth_scheme_exact")
            self.assertTrue(any(exact for _c, _f, _t, exact in lanes))
            feat = frozenset()
            records_for_feat = {
                feat: _qualified_test_records(root, extra_features=feat)
            }
            matched = _hot_path_matches_for_lanes(
                records_for_feat, "none_auth_scheme_exact", lanes
            )
            self.assertEqual([r.name for r in matched], ["none_auth_scheme_exact"])


class ExternalModulePrefix(unittest.TestCase):
    def test_mod_decl_file_uses_the_file_stem_as_libtest_prefix(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text("mod none_auth_scheme_regressions;\n")
            (src / "none_auth_scheme_regressions.rs").write_text(
                "#[test]\nfn works() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_regressions::works", names)

    def test_mod_decl_split_across_lines_is_followed(self):
        """`mod\\nname;` still loads `name.rs` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text("mod\nnone_auth_scheme_regressions;\n")
            (src / "none_auth_scheme_regressions.rs").write_text(
                "#[test]\nfn works() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_regressions::works", names)

    def test_path_attr_overrides_the_file_stem(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                '#[path = "elsewhere.rs"]\nmod none_auth_scheme_regressions;\n'
            )
            (src / "elsewhere.rs").write_text("#[test]\nfn works() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_regressions::works", names)

    def test_raw_string_path_attr_overrides_the_file_stem(self):
        """`#[path = r\"actual.rs\"]` is a live redirect (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                '#[path = r"actual.rs"]\nmod child;\n'
            )
            (src / "actual.rs").write_text(
                "#[test]\nfn none_auth_scheme_actual() {}\n"
            )
            (src / "child.rs").write_text(
                "#[test]\nfn none_auth_scheme_child() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("child::none_auth_scheme_actual", names)
            self.assertNotIn("child::none_auth_scheme_child", names)

    def test_cooked_path_escape_is_decoded(self):
        """`#[path = \"act\\x75al.rs\"]` loads actual.rs (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                '#[path = "act\\x75al.rs"]\nmod child;\n'
            )
            (src / "actual.rs").write_text(
                "#[test]\nfn none_auth_scheme_escaped_path() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("child::none_auth_scheme_escaped_path", names)

    def test_path_attr_split_across_lines_is_followed(self):
        """`#[path =\\n\"actual.rs\"]` still redirects the module
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                '#[path =\n"elsewhere.rs"]\nmod none_auth_scheme_regressions;\n'
            )
            (src / "elsewhere.rs").write_text("#[test]\nfn works() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_regressions::works", names)

    def test_same_line_path_attr_and_mod_is_followed(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                '#[path = "elsewhere.rs"] mod none_auth_scheme_alias;\n'
            )
            (src / "elsewhere.rs").write_text("#[test]\nfn works() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_alias::works", names)

    def test_cfg_attr_path_overrides_the_file_stem(self):
        """`#[cfg_attr(test, path = \"actual.rs\")]` is `#[path]` under
        cargo test (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                '#[cfg_attr(test, path = "elsewhere.rs")]\n'
                "mod none_auth_scheme_regressions;\n"
            )
            (src / "elsewhere.rs").write_text("#[test]\nfn works() {}\n")
            (src / "none_auth_scheme_regressions.rs").write_text(
                "#[test]\nfn decoy() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_regressions::works", names)
            self.assertNotIn("none_auth_scheme_regressions::decoy", names)

    def test_inactive_cfg_attr_path_keeps_the_file_stem(self):
        """`#[cfg_attr(false, path = ...)]` does not redirect rustc
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                '#[cfg_attr(false, path = "elsewhere.rs")]\n'
                "mod none_auth_scheme_regressions;\n"
            )
            (src / "elsewhere.rs").write_text("#[test]\nfn works() {}\n")
            (src / "none_auth_scheme_regressions.rs").write_text(
                "#[test]\nfn decoy() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_regressions::decoy", names)
            self.assertNotIn("none_auth_scheme_regressions::works", names)

    def test_nested_src_tests_dir_keeps_the_crate_root_prefix(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "codegen" / "demo" / "src"
            nested = crate / "agent" / "subagent" / "tests"
            nested.mkdir(parents=True)
            (crate / "lib.rs").write_text("mod agent;\n")
            (crate / "agent").mkdir(parents=True, exist_ok=True)
            (crate / "agent" / "mod.rs").write_text("mod subagent;\n")
            (crate / "agent" / "subagent").mkdir(parents=True, exist_ok=True)
            (crate / "agent" / "subagent" / "mod.rs").write_text("mod tests;\n")
            (nested / "mod.rs").write_text("mod rest;\n")
            (nested / "rest.rs").write_text("#[test]\nfn works() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("agent::subagent::tests::rest::works", names)
            self.assertNotIn("rest::works", names)

    def test_orphan_src_file_is_not_scanned(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text("mod kept;\n")
            (src / "kept.rs").write_text("#[test]\nfn visible() {}\n")
            (src / "orphan.rs").write_text("#[test]\nfn hidden() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("kept::visible", names)
            self.assertNotIn("orphan::hidden", names)
            self.assertNotIn("hidden", names)

    def test_explicit_cargo_test_path_is_a_seeded_root(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            nested = crate / "tests" / "leader_pty_e2e"
            nested.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[[test]]\nname = \"leader_pty_e2e\"\n"
                'path = "tests/leader_pty_e2e/mod.rs"\n',
                encoding="utf-8",
            )
            (nested / "mod.rs").write_text("mod cluster;\n", encoding="utf-8")
            (nested / "cluster.rs").write_text("#[test]\nfn boots() {}\n", encoding="utf-8")
            names = _qualified_test_names(root)
            self.assertIn("cluster::boots", names)

    def test_required_features_integration_target_is_not_seeded(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            tests = crate / "tests"
            tests.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[[test]]\nname = \"gated\"\n"
                'required-features = ["test-support"]\n',
                encoding="utf-8",
            )
            (tests / "gated.rs").write_text("#[test]\nfn hidden() {}\n")
            (tests / "live.rs").write_text("#[test]\nfn visible() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("visible", names)
            self.assertNotIn("hidden", names)

    def test_multiline_required_features_still_gate_the_target(self):
        """`required-features = [` split across lines is still gated
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            tests = crate / "tests"
            tests.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[[test]]\nname = \"gated\"\n"
                'required-features = [\n    "test-support",\n]\n',
                encoding="utf-8",
            )
            (tests / "gated.rs").write_text("#[test]\nfn hidden() {}\n")
            (tests / "live.rs").write_text("#[test]\nfn visible() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("visible", names)
            self.assertNotIn("hidden", names)

    def test_commented_path_attr_does_not_redirect_live_mod(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "codegen" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                '/* #[path = "orphan.rs"] */\nmod live;\n'
            )
            (src / "orphan.rs").write_text("#[test]\nfn hidden() {}\n")
            (src / "live.rs").write_text("#[test]\nfn visible() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("live::visible", names)
            self.assertNotIn("orphan::hidden", names)
            self.assertNotIn("hidden", names)

    def test_explicit_cargo_test_path_outside_src_tests_is_scanned(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            integ = crate / "integration"
            integ.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[[test]]\nname = \"custom\"\n"
                'path = "integration/custom.rs"\n',
                encoding="utf-8",
            )
            (integ / "custom.rs").write_text("#[test]\nfn boots() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("boots", names)

    def test_custom_lib_path_is_seeded(self):
        """`[lib] path = \"lib/custom.rs\"` is the crate root (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            libdir = crate / "lib"
            libdir.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[lib]\n"
                'path = "lib/custom.rs"\n',
                encoding="utf-8",
            )
            (libdir / "custom.rs").write_text(
                "#[test]\nfn none_auth_scheme_lib_root() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_lib_root", names)
            records = _qualified_test_records(root)
            scoped = _hot_path_matches(
                records, "none_auth_scheme_lib_root", {("demo", "lib")}
            )
            self.assertEqual(
                [(r.package, r.target, r.name) for r in scoped],
                [("demo", "lib", "none_auth_scheme_lib_root")],
            )

    def test_custom_lib_path_inside_src_is_prefixless(self):
        """`[lib] path = \"src/none_auth_scheme_root.rs\"` is the crate
        root, not a module named after the file (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[lib]\n"
                'path = "src/none_auth_scheme_root.rs"\n',
                encoding="utf-8",
            )
            (src / "none_auth_scheme_root.rs").write_text(
                "#[test]\nfn works() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("works", names)
            self.assertNotIn("none_auth_scheme_root::works", names)

    def test_custom_lib_path_replaces_stale_src_lib(self):
        """An explicit `[lib] path` is the only library root (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            libdir = crate / "lib"
            src.mkdir(parents=True)
            libdir.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[lib]\n"
                'path = "lib/custom.rs"\n',
                encoding="utf-8",
            )
            (libdir / "custom.rs").write_text(
                "#[test]\nfn none_auth_scheme_lib_root() {}\n"
            )
            (src / "lib.rs").write_text(
                "#[test]\nfn none_auth_scheme_stale() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_lib_root", names)
            self.assertNotIn("none_auth_scheme_stale", names)

    def test_autotests_false_skips_undeclared_integration_files(self):
        """`autotests = false` disables `tests/*.rs` auto-discovery
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            tests = crate / "tests"
            tests.mkdir(parents=True)
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n"
                "autotests = false\n\n"
                "[[test]]\nname = \"kept\"\n"
                'path = "tests/kept.rs"\n',
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text("#[test]\nfn lib_ok() {}\n")
            (tests / "kept.rs").write_text("#[test]\nfn kept() {}\n")
            (tests / "stale.rs").write_text(
                "#[test]\nfn none_auth_scheme_stale() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("lib_ok", names)
            self.assertIn("kept", names)
            self.assertNotIn("none_auth_scheme_stale", names)

    def test_explicit_test_false_target_is_not_counted(self):
        """`[[test]] test = false` is excluded from `cargo test`
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            tests = crate / "tests"
            tests.mkdir(parents=True)
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n"
                "autotests = false\n\n"
                "[[test]]\nname = \"off\"\n"
                'path = "tests/off.rs"\n'
                "test = false\n",
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[test]\nfn none_auth_scheme_live() {}\n"
            )
            (tests / "off.rs").write_text(
                "#[test]\nfn none_auth_scheme_off() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_live", names)
            self.assertNotIn("none_auth_scheme_off", names)

    def test_lib_test_false_is_not_counted(self):
        """`[lib] test = false` is not a cargo test target (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[lib]\ntest = false\n",
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[test]\nfn none_auth_scheme_lib() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertNotIn("none_auth_scheme_lib", names)

    def test_autolib_false_does_not_seed_inferred_lib(self):
        """`[package] autolib = false` does not infer `src/lib.rs`
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n"
                "autolib = false\n",
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                "#[test]\nfn none_auth_scheme_lib() {}\n"
            )
            (src / "main.rs").write_text("fn main() {}\n")
            names = _qualified_test_names(root)
            self.assertNotIn("none_auth_scheme_lib", names)

    def test_include_literal_is_scanned_in_the_including_module(self):
        """`include!(\"included.rs\")` splices tests into this module
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text('include!("included.rs");\n')
            (src / "included.rs").write_text(
                "#[test]\nfn none_auth_scheme_included() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_included", names)

    def test_include_non_rs_extension_is_scanned(self):
        """`include!(\"tests.inc\")` still contributes tests (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text('include!("tests.inc");\n')
            (src / "tests.inc").write_text(
                "#[test]\nfn none_auth_scheme_inc() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_inc", names)

    def test_braced_include_literal_is_scanned(self):
        """`include! { \"included.rs\" }` splices tests (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text('include! { "included.rs" };\n')
            (src / "included.rs").write_text(
                "#[test]\nfn none_auth_scheme_braced_include() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_braced_include", names)

    def test_concat_env_include_is_scanned(self):
        """`include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/included.rs\"))`
        splices tests (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n'
            )
            (src / "lib.rs").write_text(
                'include!(concat!(env!("CARGO_MANIFEST_DIR"), "/included.rs"));\n'
            )
            (crate / "included.rs").write_text(
                "#[test]\nfn none_auth_scheme_concat() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_concat", names)

    def test_raw_string_include_literal_is_scanned(self):
        """`include!(r\"included.rs\")` splices tests into this module
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text('include!(r"included.rs");\n')
            (src / "included.rs").write_text(
                "#[test]\nfn none_auth_scheme_raw_include() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_raw_include", names)

    def test_include_sees_macros_defined_in_the_including_file(self):
        """A `macro_rules!` before `include!` is visible in the included
        file (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "macro_rules! emit {\n"
                "    ($name:ident) => {\n"
                "        #[test]\n"
                "        fn $name() {}\n"
                "    };\n"
                "}\n"
                'include!("included.rs");\n'
            )
            (src / "included.rs").write_text(
                "emit!(none_auth_scheme_included);\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_included", names)

    def test_macro_export_from_child_module_is_invocable(self):
        """`mod macros; crate::emit!(...)` sees `#[macro_export]`
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "mod macros;\n"
                "crate::emit!(none_auth_scheme_hot);\n"
            )
            (src / "macros.rs").write_text(
                "#[macro_export]\n"
                "macro_rules! emit {\n"
                "    ($name:ident) => {\n"
                "        #[test]\n"
                "        fn $name() {}\n"
                "    };\n"
                "}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_hot", names)

    def test_macro_use_from_child_module_is_invocable(self):
        """`#[macro_use] mod macros;` imports ordinary child macros
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "#[macro_use]\n"
                "mod macros;\n"
                "emit!(none_auth_scheme_child_import);\n"
            )
            (src / "macros.rs").write_text(
                "macro_rules! emit {\n"
                "    ($name:ident) => {\n"
                "        #[test]\n"
                "        fn $name() {}\n"
                "    };\n"
                "}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_child_import", names)

    def test_include_inside_inline_module_keeps_prefix(self):
        """`mod none_auth_scheme_ { include!(...) }` is
        `none_auth_scheme_::works` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "mod none_auth_scheme_ {\n"
                '    include!("included.rs");\n'
                "}\n"
            )
            (src / "included.rs").write_text(
                "#[test]\nfn works() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_::works", names)
            self.assertNotIn("works", names)

    def test_cfg_gated_include_is_not_scanned(self):
        """`#[cfg(windows)] include!(\"win.rs\")` is off on Unix
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                '#[cfg(windows)]\ninclude!("win.rs");\n'
            )
            (src / "win.rs").write_text(
                "#[test]\nfn none_auth_scheme_windows() {}\n"
            )
            names = _qualified_test_names(root)
            if sys.platform == "win32":
                self.assertIn("none_auth_scheme_windows", names)
            else:
                self.assertNotIn("none_auth_scheme_windows", names)

    def test_lib_harness_false_is_not_counted(self):
        """`[lib] harness = false` is not a libtest target (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[lib]\nharness = false\n",
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[test]\nfn none_auth_scheme_lib() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertNotIn("none_auth_scheme_lib", names)

    def test_harness_false_target_is_not_counted(self):
        """`[[test]] harness = false` is a binary, not libtest
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            tests = crate / "tests"
            tests.mkdir(parents=True)
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n"
                "autotests = false\n\n"
                "[[test]]\nname = \"off\"\n"
                'path = "tests/off.rs"\n'
                "harness = false\n",
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text(
                "#[test]\nfn none_auth_scheme_live() {}\n"
            )
            (tests / "off.rs").write_text(
                "fn main() {}\n"
                "#[test]\nfn none_auth_scheme_off() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_live", names)
            self.assertNotIn("none_auth_scheme_off", names)

    def test_autotests_false_name_only_explicit_test_is_kept(self):
        """`[[test]] name = "kept"` with no `path` still seeds
        `tests/kept.rs` when `autotests = false` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            tests = crate / "tests"
            tests.mkdir(parents=True)
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n"
                "autotests = false\n\n"
                "[[test]]\nname = \"kept\"\n",
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text("#[test]\nfn lib_ok() {}\n")
            (tests / "kept.rs").write_text("#[test]\nfn kept() {}\n")
            (tests / "stale.rs").write_text(
                "#[test]\nfn none_auth_scheme_stale() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("lib_ok", names)
            self.assertIn("kept", names)
            self.assertNotIn("none_auth_scheme_stale", names)

    def test_explicit_integration_root_resolves_sibling_mod(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            integ = crate / "integration"
            integ.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[[test]]\nname = \"custom\"\n"
                'path = "integration/custom.rs"\n',
                encoding="utf-8",
            )
            (integ / "custom.rs").write_text("mod child;\n")
            (integ / "child.rs").write_text("#[test]\nfn boots() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("child::boots", names)

    def test_required_features_that_are_default_stay_seeded(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            tests = crate / "tests"
            tests.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[features]\ndefault = [\"test-support\"]\n"
                'test-support = []\n\n'
                "[[test]]\nname = \"gated\"\n"
                'required-features = ["test-support"]\n',
                encoding="utf-8",
            )
            (tests / "gated.rs").write_text("#[test]\nfn visible() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("visible", names)

    def test_transitively_enabled_required_features_stay_seeded(self):
        """`default = [\"bundle\"]` enabling `hot` still seeds a target
        that requires `hot` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            tests = crate / "tests"
            tests.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[features]\ndefault = [\"bundle\"]\n"
                'bundle = ["hot"]\n'
                'hot = []\n\n'
                "[[test]]\nname = \"gated\"\n"
                'required-features = ["hot"]\n',
                encoding="utf-8",
            )
            (tests / "gated.rs").write_text("#[test]\nfn visible() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("visible", names)

    def test_non_default_feature_cfg_is_not_counted(self):
        """`#[cfg(feature = \"optional\")]` is off under default cargo test
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[features]\ndefault = []\n"
                'optional = []\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                "#[cfg(feature = \"optional\")]\n"
                "#[test]\nfn none_auth_scheme_optional() {}\n"
                "#[test]\nfn none_auth_scheme_always() {}\n",
                encoding="utf-8",
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_always", names)
            self.assertNotIn("none_auth_scheme_optional", names)

    def test_default_feature_cfg_is_counted(self):
        """`#[cfg(feature = \"hot\")]` stays when `hot` is default
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[features]\ndefault = [\"hot\"]\n"
                'hot = []\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                "#[cfg(feature = \"hot\")]\n"
                "#[test]\nfn none_auth_scheme_hot() {}\n",
                encoding="utf-8",
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_hot", names)

    def test_multiline_default_features_keep_required_targets_seeded(self):
        """`default = [` split across lines is still the crate default
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            tests = crate / "tests"
            tests.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n"
                "[features]\ndefault = [\n    \"test-support\",\n]\n"
                'test-support = []\n\n'
                "[[test]]\nname = \"gated\"\n"
                'required-features = ["test-support"]\n',
                encoding="utf-8",
            )
            (tests / "gated.rs").write_text("#[test]\nfn visible() {}\n")
            names = _qualified_test_names(root)
            self.assertIn("visible", names)

    def test_string_brace_does_not_nest_following_module(self):
        text = textwrap.dedent(
            """\
            mod first {
                #[test]
                fn in_first() {}
                fn helper() { let _s = "{"; }
            }
            mod second {
                #[test]
                fn in_second() {}
            }
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["first::in_first", "second::in_second"])

    def test_lifetime_apostrophe_does_not_swallow_following_module(self):
        text = textwrap.dedent(
            """\
            fn helper<'a>() {}
            mod none_auth_scheme_ {
                #[test]
                fn works() {}
            }
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_::works"])

    def test_path_attr_keeps_the_declaring_file_prefix(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            session = root / "crates" / "codegen" / "demo" / "src" / "session"
            session.mkdir(parents=True)
            (session.parent / "lib.rs").write_text("mod session;\n")
            (session / "mod.rs").write_text("mod acp_session;\n")
            (session / "acp_session.rs").write_text(
                '#[path = "acp_session_tests/auth_error_no_retry_tests.rs"]\n'
                "mod auth_error_no_retry_tests;\n"
            )
            tests_dir = session / "acp_session_tests"
            tests_dir.mkdir()
            (tests_dir / "auth_error_no_retry_tests.rs").write_text(
                "#[test]\nfn works() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn(
                "session::acp_session::auth_error_no_retry_tests::works", names
            )
            self.assertNotIn(
                "session::acp_session_tests::auth_error_no_retry_tests::works",
                names,
            )

    def test_integration_target_has_no_file_stem_prefix(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            tests = root / "crates" / "codegen" / "demo" / "tests"
            tests.mkdir(parents=True)
            (tests / "shared_http_wire.rs").write_text(
                "#[test]\nfn none_auth_scheme_sends() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_sends", names)
            self.assertNotIn(
                "shared_http_wire::none_auth_scheme_sends", names
            )

    def test_third_party_crate_is_scanned(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "third_party" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text("mod none_auth_scheme_regressions;\n")
            (src / "none_auth_scheme_regressions.rs").write_text(
                "#[test]\nfn works() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_regressions::works", names)

    def test_shared_module_included_by_two_targets_is_counted_twice(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            tests = root / "crates" / "codegen" / "demo" / "tests"
            tests.mkdir(parents=True)
            (tests / "shared.rs").write_text(
                "#[test]\nfn none_auth_scheme_sends() {}\n"
            )
            (tests / "a.rs").write_text('#[path = "shared.rs"]\nmod common;\n')
            (tests / "b.rs").write_text('#[path = "shared.rs"]\nmod common;\n')
            names = _qualified_test_names(root)
            self.assertEqual(names.count("common::none_auth_scheme_sends"), 2)
            self.assertEqual(
                names.count("none_auth_scheme_sends"),
                1,
                "tests/shared.rs is also its own integration target",
            )

    def test_ordinary_mod_from_two_integration_roots_is_counted_twice(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            tests = root / "crates" / "codegen" / "demo" / "tests"
            (tests / "common").mkdir(parents=True)
            (tests / "common" / "mod.rs").write_text(
                "#[test]\nfn none_auth_scheme_sends() {}\n"
            )
            (tests / "a.rs").write_text("mod common;\n")
            (tests / "b.rs").write_text("mod common;\n")
            names = _qualified_test_names(root)
            self.assertEqual(names.count("common::none_auth_scheme_sends"), 2)

    def test_shared_module_child_is_counted_once_per_including_target(self):
        """Duplicate (path, prefix) visits must still walk `mod child;` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            tests = root / "crates" / "codegen" / "demo" / "tests"
            (tests / "common").mkdir(parents=True)
            (tests / "common" / "mod.rs").write_text("mod child;\n")
            (tests / "common" / "child.rs").write_text(
                "#[test]\nfn none_auth_scheme_sends() {}\n"
            )
            (tests / "a.rs").write_text("mod common;\n")
            (tests / "b.rs").write_text("mod common;\n")
            names = _qualified_test_names(root)
            self.assertEqual(names.count("common::child::none_auth_scheme_sends"), 2)

    def test_integration_file_imported_as_module_resolves_nested_child(self):
        """`tests/shared.rs` as its own target uses `tests/child.rs`; as
        `mod shared` it uses `tests/shared/child.rs` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            tests = root / "crates" / "codegen" / "demo" / "tests"
            (tests / "shared").mkdir(parents=True)
            (tests / "shared.rs").write_text("mod child;\n")
            (tests / "child.rs").write_text(
                "#[test]\nfn crate_root_child() {}\n"
            )
            (tests / "shared" / "child.rs").write_text(
                "#[test]\nfn none_auth_scheme_nested() {}\n"
            )
            (tests / "a.rs").write_text("mod shared;\n")
            names = _qualified_test_names(root)
            self.assertIn("child::crate_root_child", names)
            self.assertIn("shared::child::none_auth_scheme_nested", names)

    def test_external_module_under_an_inline_module_keeps_the_inline_prefix(self):
        """`mod outer { mod inner; }` loads `outer/inner.rs` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "codegen" / "demo" / "src"
            (src / "outer").mkdir(parents=True)
            (src / "lib.rs").write_text(
                "mod outer {\n    mod inner;\n}\n"
            )
            (src / "outer" / "inner.rs").write_text(
                "#[test]\nfn none_auth_scheme_sends() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("outer::inner::none_auth_scheme_sends", names)
            self.assertNotIn("inner::none_auth_scheme_sends", names)

    def test_path_attr_inside_an_inline_module_uses_the_inline_directory(self):
        """`mod outer { #[path = "actual.rs"] mod alias; }` loads
        `outer/actual.rs` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "codegen" / "demo" / "src"
            (src / "outer").mkdir(parents=True)
            (src / "lib.rs").write_text(
                'mod outer {\n    #[path = "actual.rs"]\n    mod alias;\n}\n'
            )
            (src / "outer" / "actual.rs").write_text(
                "#[test]\nfn none_auth_scheme_sends() {}\n"
            )
            (src / "actual.rs").write_text(
                "#[test]\nfn none_auth_scheme_wrong_dir() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("outer::alias::none_auth_scheme_sends", names)
            self.assertNotIn("none_auth_scheme_wrong_dir", names)
            self.assertNotIn("alias::none_auth_scheme_sends", names)

    def test_block_commented_test_attr_is_not_counted(self):
        """Wrapping a hot-path test in `/* ... */` must drop it (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "#[test]\nfn none_auth_scheme_live() {}\n"
                "/*\n#[test]\nfn none_auth_scheme_commented() {}\n*/\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_live", names)
            self.assertNotIn("none_auth_scheme_commented", names)

    def test_same_line_test_attr_and_fn_is_counted(self):
        names = _tests_in_file("#[test] fn none_auth_scheme_case() {}\n", [])
        self.assertEqual(names, ["none_auth_scheme_case"])
        names = _tests_in_file(
            "#[test] /* rationale */ fn none_auth_scheme_commented_gap() {}\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_commented_gap"])

    def test_split_cfg_attr_is_still_evaluated(self):
        """`#[cfg(\\nwindows\\n)]` still inactivates the following test
        (#507 review)."""

        names = _tests_in_file(
            "#[cfg(\nwindows\n)]\n#[test]\nfn none_auth_scheme_windows_only() {}\n"
            "#[test]\nfn none_auth_scheme_everywhere() {}\n",
            [],
        )
        self.assertIn("none_auth_scheme_everywhere", names)
        if sys.platform == "win32":
            self.assertIn("none_auth_scheme_windows_only", names)
        else:
            self.assertNotIn("none_auth_scheme_windows_only", names)

    def test_const_and_extern_test_fns_are_counted(self):
        """`const fn` and `extern \"C\" fn` tests are registered by
        libtest (#507 review)."""

        names = _tests_in_file(
            "#[test] const fn none_auth_scheme_const() {}\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_const"])
        names = _tests_in_file(
            '#[test] extern "C" fn none_auth_scheme_extern() {}\n',
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_extern"])

    def test_same_line_attr_and_inline_mod_keeps_the_prefix(self):
        """`#[allow(dead_code)] mod none_auth_scheme_ {` must still qualify
        inner tests (#507 review)."""

        text = textwrap.dedent(
            """\
            #[allow(dead_code)] mod none_auth_scheme_ {
                #[test]
                fn works() {}
            }
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_::works"])

        text = textwrap.dedent(
            """\
            #[cfg(false)] mod none_auth_scheme_ {
                #[test]
                fn skipped() {}
            }
            #[test]
            fn none_auth_scheme_live() {}
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_live"])
        self.assertNotIn("none_auth_scheme_::skipped", names)

    def test_nested_block_comment_does_not_leak_braces(self):
        text = textwrap.dedent(
            """\
            mod first {
                fn helper() { let _c = /* /* inner */ { */ 1; }
                #[test]
                fn in_first() {}
            }
            mod none_auth_scheme_ {
                #[test]
                fn works() {}
            }
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["first::in_first", "none_auth_scheme_::works"])

    def test_cfg_false_tests_are_not_counted_on_this_target(self):
        text = textwrap.dedent(
            """\
            #[cfg(windows)]
            #[test]
            fn none_auth_scheme_windows_only() {}
            #[test]
            fn none_auth_scheme_everywhere() {}
            #[cfg(unix)]
            #[test]
            fn none_auth_scheme_unix_only() {}
            """
        )
        names = _tests_in_file(text, [])
        self.assertIn("none_auth_scheme_everywhere", names)
        if sys.platform == "win32":
            self.assertIn("none_auth_scheme_windows_only", names)
            self.assertNotIn("none_auth_scheme_unix_only", names)
        else:
            self.assertNotIn("none_auth_scheme_windows_only", names)
            self.assertIn("none_auth_scheme_unix_only", names)

    def test_cfg_target_os_string_is_still_evaluated(self):
        """`#[cfg(target_os = \"macos\")]` must keep its string after
        masking (#507 review)."""

        text = textwrap.dedent(
            """\
            #[cfg(target_os = "linux")]
            #[test]
            fn none_auth_scheme_linux() {}
            #[cfg(target_os = "macos")]
            #[test]
            fn none_auth_scheme_macos() {}
            #[cfg(target_os = "windows")]
            #[test]
            fn none_auth_scheme_windows() {}
            """
        )
        names = _tests_in_file(text, [])
        if sys.platform.startswith("linux"):
            self.assertEqual(names, ["none_auth_scheme_linux"])
        elif sys.platform == "darwin":
            self.assertEqual(names, ["none_auth_scheme_macos"])
        elif sys.platform == "win32":
            self.assertEqual(names, ["none_auth_scheme_windows"])
        else:
            self.assertEqual(names, [])

    def test_cfg_after_test_attr_is_honored(self):
        text = textwrap.dedent(
            """\
            #[test]
            #[cfg(windows)]
            fn none_auth_scheme_windows_only() {}
            #[test]
            fn none_auth_scheme_everywhere() {}
            """
        )
        names = _tests_in_file(text, [])
        self.assertIn("none_auth_scheme_everywhere", names)
        if sys.platform == "win32":
            self.assertIn("none_auth_scheme_windows_only", names)
        else:
            self.assertNotIn("none_auth_scheme_windows_only", names)

    def test_cfg_false_external_module_is_not_scanned_on_this_target(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "codegen" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "#[cfg(windows)]\nmod platform;\n"
                "#[test]\nfn none_auth_scheme_everywhere() {}\n"
            )
            (src / "platform.rs").write_text(
                "#[test]\nfn none_auth_scheme_windows_mod() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_everywhere", names)
            if sys.platform == "win32":
                self.assertIn("platform::none_auth_scheme_windows_mod", names)
            else:
                self.assertNotIn("platform::none_auth_scheme_windows_mod", names)

    def test_cfg_attr_cfg_windows_module_is_not_scanned_on_unix(self):
        """`#[cfg_attr(test, cfg(windows))] mod child;` is off on Linux
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "#[cfg_attr(test, cfg(windows))]\n"
                "mod none_auth_scheme_child;\n"
                "#[test]\nfn none_auth_scheme_everywhere() {}\n"
            )
            (src / "none_auth_scheme_child.rs").write_text(
                "#[test]\nfn phantom() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_everywhere", names)
            if sys.platform == "win32":
                self.assertIn("none_auth_scheme_child::phantom", names)
            else:
                self.assertNotIn("none_auth_scheme_child::phantom", names)

    def test_path_prefix_propagates_to_descendant_modules(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            session = root / "crates" / "codegen" / "demo" / "src" / "session"
            impl = session / "acp_session_impl"
            impl.mkdir(parents=True)
            (root / "crates" / "codegen" / "demo" / "src" / "lib.rs").write_text(
                "mod session;\n"
            )
            (session / "mod.rs").write_text("mod acp_session;\n")
            (session / "acp_session.rs").write_text(
                '#[path = "acp_session_impl/extensions.rs"]\nmod extensions;\n'
            )
            (impl / "extensions.rs").write_text("mod idle_prompt;\n")
            (impl / "idle_prompt.rs").write_text(
                "#[test]\nfn none_auth_scheme_sends() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn(
                "session::acp_session::extensions::idle_prompt::none_auth_scheme_sends",
                names,
            )

    def test_multiline_tokio_test_attr_is_counted(self):
        """`#[tokio::test(` options may close on a later line (#507 review)."""

        text = textwrap.dedent(
            """\
            #[tokio::test(
                flavor = "multi_thread"
            )]
            fn none_auth_scheme_async() {}
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_async"])

    def test_inner_cfg_file_is_skipped_on_this_target(self):
        """`#![cfg(windows)]` at the crate/module start is a file-level
        gate, not a per-item `#[cfg]` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "codegen" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "mod platform;\n"
                "#[test]\nfn none_auth_scheme_everywhere() {}\n"
            )
            (src / "platform.rs").write_text(
                "#![cfg(windows)]\n"
                "#[test]\nfn none_auth_scheme_windows_inner() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_everywhere", names)
            if sys.platform == "win32":
                self.assertIn("platform::none_auth_scheme_windows_inner", names)
            else:
                self.assertNotIn("platform::none_auth_scheme_windows_inner", names)

    def test_inner_cfg_after_block_comment_is_honored(self):
        """A leading `/* license */` must not hide `#![cfg(windows)]`
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "codegen" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "mod platform;\n"
                "#[test]\nfn none_auth_scheme_everywhere() {}\n"
            )
            (src / "platform.rs").write_text(
                "/* license */\n"
                "#![cfg(windows)]\n"
                "#[test]\nfn none_auth_scheme_windows_inner() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_everywhere", names)
            if sys.platform == "win32":
                self.assertIn("platform::none_auth_scheme_windows_inner", names)
            else:
                self.assertNotIn("platform::none_auth_scheme_windows_inner", names)

    def test_ignored_tests_are_not_counted(self):
        """`#[ignore]` is skipped by libtest unless `--ignored` (#507 review)."""

        text = textwrap.dedent(
            """\
            #[test]
            fn none_auth_scheme_live() {}
            #[test]
            #[ignore]
            fn none_auth_scheme_ignored() {}
            #[ignore]
            #[test]
            fn none_auth_scheme_ignored_first() {}
            #[test]
            #[ignore = "not on this runner"]
            fn none_auth_scheme_ignored_reason() {}
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_live"])

    def test_cfg_attr_ignore_is_not_counted(self):
        """`#[cfg_attr(test, ignore)]` is `#[ignore]` under libtest
        (#507 review)."""

        text = textwrap.dedent(
            """\
            #[test]
            fn none_auth_scheme_live() {}
            #[test]
            #[cfg_attr(test, ignore)]
            fn none_auth_scheme_cfg_attr_after() {}
            #[cfg_attr(test, ignore)]
            #[test]
            fn none_auth_scheme_cfg_attr_before() {}
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_live"])

    def test_cfg_attr_test_is_counted(self):
        """`#[cfg_attr(test, test)]` is a test under the harness
        (#507 review)."""

        names = _tests_in_file(
            "#[cfg_attr(test, test)]\nfn none_auth_scheme_generated() {}\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_generated"])

    def test_cfg_attr_emits_each_attribute(self):
        """`#[cfg_attr(test, should_panic, test)]` is a live test
        (#507 review)."""

        names = _tests_in_file(
            "#[cfg_attr(test, should_panic, test)]\n"
            "fn none_auth_scheme_case() {}\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_case"])

    def test_raw_identifier_test_name_is_counted(self):
        """`fn r#none_auth_scheme_case()` is `none_auth_scheme_case`
        in libtest (#507 review)."""

        names = _tests_in_file(
            "#[test]\nfn r#none_auth_scheme_case() {}\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_case"])

    def test_unicode_identifier_test_name_is_counted(self):
        """`fn café_none_auth_scheme_case` is the full XID name
        (#507 review)."""

        names = _tests_in_file(
            "#[test]\nfn café_none_auth_scheme_case() {}\n",
            [],
        )
        self.assertEqual(names, ["café_none_auth_scheme_case"])

    def test_unicode_module_prefix_is_counted(self):
        """`mod café_none_auth_scheme_ { #[test] fn works }` is
        `café_none_auth_scheme_::works` (#507 review)."""

        names = _tests_in_file(
            "mod café_none_auth_scheme_ { #[test] fn works() {} }\n",
            [],
        )
        self.assertEqual(names, ["café_none_auth_scheme_::works"])

    def test_path_imported_file_keeps_declaring_package(self):
        """A `#[path]` file under crate B still belongs to crate A
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            a_src = root / "crates" / "a" / "src"
            b_src = root / "crates" / "b" / "src"
            a_src.mkdir(parents=True)
            b_src.mkdir(parents=True)
            (root / "crates" / "a" / "Cargo.toml").write_text(
                '[package]\nname = "a"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (root / "crates" / "b" / "Cargo.toml").write_text(
                '[package]\nname = "b"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (a_src / "lib.rs").write_text(
                '#[path = "../../b/src/helper.rs"]\nmod helper;\n'
            )
            (b_src / "lib.rs").write_text("\n")
            (b_src / "helper.rs").write_text(
                "#[test]\nfn none_auth_scheme_imported() {}\n"
            )
            records = _qualified_test_records(root)
            scoped = _hot_path_matches(
                records, "none_auth_scheme_imported", {("a", "lib")}
            )
            self.assertEqual(
                [(r.package, r.target, r.name) for r in scoped],
                [("a", "lib", "helper::none_auth_scheme_imported")],
            )

    def test_path_imported_file_uses_declaring_crate_features(self):
        """`#[path]` from crate A evaluates `cfg(feature)` with A's
        defaults, not the physical crate B (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            a_src = root / "crates" / "a" / "src"
            b_src = root / "crates" / "b" / "src"
            a_src.mkdir(parents=True)
            b_src.mkdir(parents=True)
            (root / "crates" / "a" / "Cargo.toml").write_text(
                "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n"
                "[features]\ndefault = [\"hot\"]\nhot = []\n",
                encoding="utf-8",
            )
            (root / "crates" / "b" / "Cargo.toml").write_text(
                "[package]\nname = \"b\"\nversion = \"0.1.0\"\n\n"
                "[features]\nhot = []\n",
                encoding="utf-8",
            )
            (a_src / "lib.rs").write_text(
                '#[path = "../../b/src/helper.rs"]\nmod helper;\n'
            )
            (b_src / "lib.rs").write_text("\n")
            (b_src / "helper.rs").write_text(
                "#[cfg(feature = \"hot\")]\n"
                "#[test]\nfn none_auth_scheme_hot() {}\n"
            )
            records = _qualified_test_records(root)
            scoped = _hot_path_matches(
                records, "none_auth_scheme_hot", {("a", "lib")}
            )
            self.assertEqual(
                [(r.package, r.target, r.name) for r in scoped],
                [("a", "lib", "helper::none_auth_scheme_hot")],
            )
            b_scoped = _hot_path_matches(
                records, "none_auth_scheme_hot", {("b", "lib")}
            )
            self.assertEqual(b_scoped, [])

    def test_raw_identifier_module_prefix_is_counted(self):
        """`mod r#none_auth_scheme_ { #[test] fn works() }` is
        `none_auth_scheme_::works` (#507 review)."""

        names = _tests_in_file(
            "mod r#none_auth_scheme_ { #[test] fn works() {} }\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_::works"])

    def test_nested_cfg_attr_test_is_counted(self):
        """`#[cfg_attr(test, cfg_attr(test, test))]` is a live test
        (#507 review)."""

        names = _tests_in_file(
            "#[cfg_attr(test, cfg_attr(test, test))]\n"
            "fn none_auth_scheme_case() {}\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_case"])

    def test_same_line_mod_and_test_are_both_seen(self):
        """`mod name { #[test] fn works() {} }` is `name::works`
        (#507 review)."""

        names = _tests_in_file(
            "mod none_auth_scheme_ { #[test] fn works() {} }\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_::works"])

    def test_same_line_test_after_closed_inline_mod_is_not_nested(self):
        """`mod name { #[test] fn inner() {} } #[test] fn outer() {}`
        must not qualify `outer` (#507 review)."""

        names = _tests_in_file(
            "mod none_auth_scheme_ { #[test] fn works() {} } #[test] fn plain() {}\n",
            [],
        )
        self.assertEqual(sorted(names), ["none_auth_scheme_::works", "plain"])

    def test_same_line_cfg_inactive_mod_is_not_counted(self):
        """`#[cfg(windows)] mod name { #[test] fn works() {} }` is off
        on Unix (#507 review)."""

        names = _tests_in_file(
            "#[cfg(windows)] mod none_auth_scheme_ { #[test] fn works() {} }\n"
            "#[test] fn none_auth_scheme_live() {}\n",
            [],
        )
        self.assertIn("none_auth_scheme_live", names)
        if sys.platform == "win32":
            self.assertIn("none_auth_scheme_::works", names)
        else:
            self.assertNotIn("none_auth_scheme_::works", names)

    def test_debug_assertions_cfg_is_active_under_cargo_test(self):
        """The documented hot path is debug `cargo test` (#507 review)."""

        names = _tests_in_file(
            "#[cfg(debug_assertions)]\n#[test]\nfn live() {}\n"
            "#[cfg(not(debug_assertions))]\n#[test]\nfn hidden() {}\n",
            [],
        )
        self.assertEqual(names, ["live"])
        self.assertNotIn("hidden", names)

    def test_two_tests_on_the_same_line_are_both_counted(self):
        names = _tests_in_file(
            "#[test] fn none_auth_scheme_a() {} #[test] fn none_auth_scheme_b() {}\n",
            [],
        )
        self.assertEqual(
            sorted(names),
            ["none_auth_scheme_a", "none_auth_scheme_b"],
        )

    def test_same_line_same_basename_in_different_modules_are_both_counted(self):
        """Two inline modules on one line with the same fn name both register
        under their qualified paths (#507 review)."""

        names = _tests_in_file(
            "mod a { #[test] fn none_auth_scheme_same() {} } "
            "mod b { #[test] fn none_auth_scheme_same() {} }\n",
            [],
        )
        self.assertEqual(
            sorted(names),
            ["a::none_auth_scheme_same", "b::none_auth_scheme_same"],
        )

    def test_unexpanded_macro_rules_test_is_not_counted(self):
        """`#[test]` inside an uninvoked `macro_rules!` is not a libtest
        case (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! phantom {
                () => {
                    #[test]
                    fn none_auth_scheme_phantom() {}
                };
            }
            #[test]
            fn none_auth_scheme_live() {}
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_live"])
        self.assertNotIn("none_auth_scheme_phantom", names)

    def test_invoked_macro_rules_test_is_counted(self):
        """An invoked `macro_rules!` emits its `#[test]` item; rustc
        registers it (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                () => {
                    #[test]
                    fn none_auth_scheme_generated() {}
                };
            }
            generated!();
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_generated"])

    def test_crate_qualified_local_macro_is_counted(self):
        """`crate::generated!(name)` is a crate-root invocation (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            crate::generated!(none_auth_scheme_case);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_case"])

    def test_module_qualified_macro_is_counted(self):
        """`macros::emit!(name)` expands a module-reexported macro
        (#507 review)."""

        text = textwrap.dedent(
            """\
            mod macros {
                macro_rules! emit {
                    ($name:ident) => {
                        #[test]
                        fn $name() {}
                    };
                }
                pub(crate) use emit;
            }
            macros::emit!(none_auth_scheme_case);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_case"])

    def test_self_qualified_local_macro_is_counted(self):
        """`self::generated!(name)` at the crate root is an invocation
        (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            self::generated!(none_auth_scheme_self);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_self"])

    def test_imported_macro_alias_is_counted(self):
        """`use crate::emit as aliased; aliased!(name)` still expands
        (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! emit {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            use crate::emit as aliased;
            aliased!(none_auth_scheme_case);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_case"])

    def test_grouped_macro_use_alias_is_counted(self):
        """`use crate::{emit as aliased};` still expands (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! emit {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            use crate::{emit as aliased};
            aliased!(none_auth_scheme_group);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_group"])

    def test_cfg_disabled_macro_alias_is_ignored(self):
        """`#[cfg(windows)] use crate::emit as aliased;` does not rewrite
        on non-Windows hosts (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! emit {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            #[cfg(windows)]
            use crate::emit as aliased;
            macro_rules! aliased {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            aliased!(harmless);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["harmless"])

    def test_macro_alias_stays_in_lexical_scope(self):
        """An alias inside `mod inner` does not rewrite root invocations
        (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! emit {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            mod inner {
                use crate::emit as aliased;
                aliased!(inner_hot);
            }
            macro_rules! aliased {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            aliased!(harmless);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(
            sorted(names), ["harmless", "inner::inner_hot"]
        )

    def test_parent_macro_rules_test_in_external_child_is_counted(self):
        """A parent macro defined before `mod child;` is lexically visible
        inside the external child module (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    """\
                    macro_rules! generated {
                        ($name:ident) => {
                            #[test]
                            fn $name() {}
                        };
                    }
                    mod child;
                    """
                ),
                encoding="utf-8",
            )
            (src / "child.rs").write_text(
                "generated!(none_auth_scheme_inherited);\n",
                encoding="utf-8",
            )

            self.assertIn(
                "child::none_auth_scheme_inherited",
                _qualified_test_names(root),
            )

    def test_super_qualified_macro_in_child_is_counted(self):
        """`super::emit!(name)` in a child module is an invocation
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    """\
                    macro_rules! generated {
                        ($name:ident) => {
                            #[test]
                            fn $name() {}
                        };
                    }
                    mod child;
                    """
                ),
                encoding="utf-8",
            )
            (src / "child.rs").write_text(
                "super::generated!(none_auth_scheme_super);\n",
                encoding="utf-8",
            )

            self.assertIn(
                "child::none_auth_scheme_super",
                _qualified_test_names(root),
            )

    def test_external_child_macros_resolve_at_each_invocation(self):
        """Local/inline definitions shadow only after and inside their scope."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    """\
                    macro_rules! generated {
                        () => {
                            #[test]
                            fn parent_case() {}
                        };
                    }
                    mod child;
                    """
                ),
                encoding="utf-8",
            )
            (src / "child.rs").write_text(
                textwrap.dedent(
                    """\
                    generated!();
                    macro_rules! generated {
                        () => { #[test] fn local_case() {} };
                    }
                    mod inner {
                        macro_rules! generated {
                            () => { #[test] fn inner_case() {} };
                        }
                        generated!();
                    }
                    generated!();
                    """
                ),
                encoding="utf-8",
            )

            cargo_names = _cargo_list_test_names(crate)
            self.assertEqual(
                cargo_names,
                [
                    "child::inner::inner_case",
                    "child::local_case",
                    "child::parent_case",
                ],
            )
            self.assertEqual(sorted(_qualified_test_names(root)), cargo_names)

    def test_cfg_inactive_macro_does_not_shadow_active_parent_macro(self):
        """Only cfg-active definitions participate in lexical shadowing."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    """\
                    #[cfg(unix)]
                    macro_rules! generated {
                        () => {
                            #[test]
                            fn none_auth_scheme_sends() {}
                        };
                    }
                    #[cfg(windows)]
                    macro_rules! generated {
                        () => {
                            #[test]
                            fn clean_case() {}
                        };
                    }
                    mod first;
                    mod second;
                    """
                ),
                encoding="utf-8",
            )
            (src / "first.rs").write_text("generated!();\n", encoding="utf-8")
            (src / "second.rs").write_text("generated!();\n", encoding="utf-8")

            cargo_names = _cargo_list_test_names(crate)
            self.assertEqual(
                cargo_names,
                [
                    "first::none_auth_scheme_sends",
                    "second::none_auth_scheme_sends",
                ],
            )
            self.assertEqual(sorted(_qualified_test_names(root)), cargo_names)

    def test_raw_and_unicode_macro_names_are_inherited_by_children(self):
        """Rust raw and Unicode identifiers work in definitions/invocations."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    """\
                    macro_rules! r#type {
                        () => {
                            #[test]
                            fn none_auth_scheme_raw() {}
                        };
                    }
                    macro_rules! 生成 {
                        () => {
                            #[test]
                            fn none_auth_scheme_unicode() {}
                        };
                    }
                    mod raw_child;
                    mod unicode_child;
                    """
                ),
                encoding="utf-8",
            )
            (src / "raw_child.rs").write_text("r#type!();\n", encoding="utf-8")
            (src / "unicode_child.rs").write_text("生成!();\n", encoding="utf-8")

            cargo_names = _cargo_list_test_names(crate)
            self.assertEqual(
                cargo_names,
                [
                    "raw_child::none_auth_scheme_raw",
                    "unicode_child::none_auth_scheme_unicode",
                ],
            )
            self.assertEqual(sorted(_qualified_test_names(root)), cargo_names)

    def test_unicode_xid_macro_names_match_after_nfc_normalization(self):
        """Rust normalizes XID identifiers, including combining marks, to NFC."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    """\
                    macro_rules! fanto\u0302me {
                        () => { #[test] fn phantom_case() {} };
                    }
                    macro_rules! cafe\u0301 {
                        () => { #[test] fn invoked_case() {} };
                    }
                    caf\u00e9!();
                    """
                ),
                encoding="utf-8",
            )

            cargo_names = _cargo_list_test_names(crate)
            self.assertEqual(cargo_names, ["invoked_case"])
            self.assertNotIn("phantom_case", cargo_names)
            self.assertEqual(sorted(_qualified_test_names(root)), cargo_names)

    def test_attr_string_brace_does_not_corrupt_macro_lexical_scope(self):
        """Attribute string braces cannot change macro shadowing scope."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    '''\
                    macro_rules! generated {
                        () => { #[test] fn outer_case() {} };
                    }
                    mod nested {
                        #[doc = "}"]
                        macro_rules! generated {
                            () => { #[test] fn nested_case() {} };
                        }
                        generated!();
                    }
                    generated!();
                    '''
                ),
                encoding="utf-8",
            )

            cargo_names = _cargo_list_test_names(crate)
            self.assertEqual(cargo_names, ["nested::nested_case", "outer_case"])
            self.assertEqual(sorted(_qualified_test_names(root)), cargo_names)

    def test_qualified_macro_does_not_resolve_to_lexical_same_name(self):
        """A path-qualified invocation cannot select an unrelated local macro."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\nedition = "2021"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    """\
                    macro_rules! generated {
                        () => { #[test] fn lexical_case() {} };
                    }
                    mod macros {
                        macro_rules! generated {
                            () => {};
                        }
                        pub(crate) use generated;
                    }
                    macros :: generated!();
                    """
                ),
                encoding="utf-8",
            )

            cargo_names = _cargo_list_test_names(crate)
            self.assertEqual(cargo_names, [])
            self.assertEqual(sorted(_qualified_test_names(root)), cargo_names)

    def test_invoked_macro_emits_external_child_module(self):
        """A selected expansion's `mod child;` is a live module (#507)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    """\
                    macro_rules! generated {
                        () => {
                            mod none_auth_scheme_child;
                        };
                    }
                    generated!();
                    """
                ),
                encoding="utf-8",
            )
            (src / "none_auth_scheme_child.rs").write_text(
                "#[test]\nfn works() {}\n",
                encoding="utf-8",
            )

            cargo_names = _cargo_list_test_names(crate)
            self.assertEqual(cargo_names, ["none_auth_scheme_child::works"])
            self.assertEqual(sorted(_qualified_test_names(root)), cargo_names)

    def test_same_line_module_emitting_macro_uses_column_scope(self):
        """`mod outer { emit!(); }` loads `outer/child.rs` (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "macro_rules! emit {\n"
                "    () => {\n"
                "        mod child;\n"
                "    };\n"
                "}\n"
                "mod outer { emit!(); }\n"
            )
            child = src / "outer"
            child.mkdir()
            (child / "child.rs").write_text(
                "#[test]\nfn none_auth_scheme_child() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("outer::child::none_auth_scheme_child", names)
            self.assertNotIn("child::none_auth_scheme_child", names)

    def test_module_emitting_macro_uses_lexical_scope(self):
        """A later sibling-block macro must not steal the outer invoke
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    """\
                    macro_rules! emit_mod {
                        () => { mod none_auth_scheme_root; };
                    }
                    {
                        macro_rules! emit_mod {
                            () => { mod none_auth_scheme_block; };
                        }
                        emit_mod!();
                    }
                    emit_mod!();
                    """
                ),
                encoding="utf-8",
            )
            (src / "none_auth_scheme_root.rs").write_text(
                "#[test]\nfn root_case() {}\n"
            )
            (src / "none_auth_scheme_block.rs").write_text(
                "#[test]\nfn block_case() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertIn("none_auth_scheme_root::root_case", names)
            self.assertIn("none_auth_scheme_block::block_case", names)

    def test_block_local_macro_does_not_leak_into_external_child(self):
        """A function/block macro is not in the later module item's scope."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                textwrap.dedent(
                    """\
                    macro_rules! generated {
                        () => {
                            #[test]
                            fn parent_case() {}
                        };
                    }
                    fn helper() {
                        macro_rules! generated {
                            () => {
                                #[test]
                                fn block_case() {}
                            };
                        }
                    }
                    mod child;
                    """
                ),
                encoding="utf-8",
            )
            (src / "child.rs").write_text(
                "generated!();\n",
                encoding="utf-8",
            )

            cargo_names = _cargo_list_test_names(crate)
            self.assertEqual(cargo_names, ["child::parent_case"])
            self.assertEqual(sorted(_qualified_test_names(root)), cargo_names)

    def test_path_module_descendant_uses_path_parent_search_base(self):
        """A `#[path]` module's children resolve beside the renamed file."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            alt = src / "alt"
            alt.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "demo"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                "macro_rules! generated {\n"
                "    () => { #[test] fn inherited_path_case() {} };\n"
                "}\n"
                '#[path = "alt/renamed.rs"]\nmod logical;\n',
                encoding="utf-8",
            )
            (alt / "renamed.rs").write_text("mod grand;\n", encoding="utf-8")
            (alt / "grand.rs").write_text("generated!();\n", encoding="utf-8")

            cargo_names = _cargo_list_test_names(crate)
            self.assertEqual(
                cargo_names, ["logical::grand::inherited_path_case"]
            )
            self.assertEqual(sorted(_qualified_test_names(root)), cargo_names)

    def test_unselected_macro_arm_test_is_not_counted(self):
        """`generated!(cold)` must not count a sibling `(hot)` arm
        (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                (hot) => {
                    #[test]
                    fn none_auth_scheme_generated() {}
                };
                (cold) => {};
            }
            generated!(cold);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, [])

    def test_selected_macro_arm_test_is_counted(self):
        text = textwrap.dedent(
            """\
            macro_rules! generated {
                (hot) => {
                    #[test]
                    fn none_auth_scheme_generated() {}
                };
                (cold) => {};
            }
            generated!(hot);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_generated"])

    def test_invoked_macro_tests_use_each_invocation_module_prefix(self):
        """`generated!()` in two modules emits two qualified tests
        (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                () => {
                    #[test]
                    fn works() {}
                };
            }
            mod none_auth_scheme_a {
                generated!();
            }
            mod none_auth_scheme_b {
                generated!();
            }
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(
            sorted(names),
            ["none_auth_scheme_a::works", "none_auth_scheme_b::works"],
        )

    def test_same_line_macro_invocation_uses_column_module(self):
        """`mod { generated!(inside); } generated!(outside);` must not
        qualify `outside` (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            mod none_auth_scheme_ { generated!(inside); } generated!(outside);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(sorted(names), ["none_auth_scheme_::inside", "outside"])

    def test_wrapper_macro_expands_nested_emit_test(self):
        """`wrapper!($name)` -> `emit_test!($name)` still registers the
        test (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! emit_test {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            macro_rules! wrapper {
                ($name:ident) => {
                    emit_test!($name);
                };
            }
            wrapper!(none_auth_scheme_nested);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_nested"])

    def test_foreign_target_arch_cfg_is_not_counted(self):
        """`#[cfg(target_arch = \"wasm32\")]` is off on the CI hosts
        (#507 review)."""

        names = _tests_in_file(
            "#[cfg(target_arch = \"wasm32\")]\n"
            "#[test]\nfn none_auth_scheme_wasm() {}\n"
            "#[test]\nfn none_auth_scheme_host() {}\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_host"])
        names = _tests_in_file(
            f'#[cfg(target_arch = "{_host_target_arch()}")]\n'
            "#[test]\nfn none_auth_scheme_native() {}\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_native"])

    def test_foreign_target_env_cfg_is_not_counted(self):
        """`#[cfg(target_env = \"msvc\")]` is off on gnu/apple hosts
        (#507 review)."""

        names = _tests_in_file(
            '#[cfg(target_env = "msvc")]\n'
            "#[test]\nfn none_auth_scheme_msvc() {}\n"
            "#[test]\nfn none_auth_scheme_host() {}\n",
            [],
        )
        if _host_target_env() == "msvc":
            self.assertEqual(
                sorted(names),
                ["none_auth_scheme_host", "none_auth_scheme_msvc"],
            )
        else:
            self.assertEqual(names, ["none_auth_scheme_host"])

    def test_foreign_pointer_width_cfg_is_not_counted(self):
        """`#[cfg(target_pointer_width = \"32\")]` is off on 64-bit CI
        hosts (#507 review)."""

        foreign = "32" if _host_pointer_width() != "32" else "64"
        names = _tests_in_file(
            f'#[cfg(target_pointer_width = "{foreign}")]\n'
            "#[test]\nfn none_auth_scheme_foreign_width() {}\n"
            "#[test]\nfn none_auth_scheme_host() {}\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_host"])
        names = _tests_in_file(
            f'#[cfg(target_pointer_width = "{_host_pointer_width()}")]\n'
            "#[test]\nfn none_auth_scheme_native_width() {}\n",
            [],
        )
        self.assertEqual(names, ["none_auth_scheme_native_width"])

    def test_macro_ident_metavar_is_substituted_before_scan(self):
        """`fn $name` with `generated!(none_auth_scheme_generated)` is
        that test, not an unmatched `$name` (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                ($name:ident) => {
                    #[test]
                    fn $name() {}
                };
            }
            generated!(none_auth_scheme_generated);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_generated"])

    def test_macro_block_fragment_selects_the_arm(self):
        """`($name:ident, $body:block)` accepts `{ assert!(true); }`
        (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! make_test {
                ($name:ident, $body:block) => {
                    #[test]
                    fn $name() $body
                };
            }
            make_test!(none_auth_scheme_case, { assert!(true); });
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_case"])

    def test_macro_ty_fragment_consumes_generic_type(self):
        """`($ty:ty, $name:ident)` accepts `Option<u8>` (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! make_test {
                ($ty:ty, $name:ident) => {
                    #[test]
                    fn $name() {
                        let _: $ty = None;
                    }
                };
            }
            make_test!(Option<u8>, none_auth_scheme_from_type);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_from_type"])

    def test_macro_expr_fragment_stops_at_fat_arrow(self):
        """`($e:expr => $name:ident)` accepts `1 + 2 => name` (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! make_test {
                ($e:expr => $name:ident) => {
                    #[test]
                    fn $name() {
                        let _ = $e;
                    }
                };
            }
            make_test!(1 + 2 => none_auth_scheme_expr);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_expr"])

    def test_repeated_macro_ident_metavars_are_substituted_before_scan(self):
        """`$($name:ident),*` with two invocation idents emits both
        tests (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                ($($name:ident),*) => {
                    $(
                        #[test]
                        fn $name() {}
                    )*
                };
            }
            generated!(none_auth_scheme_a, none_auth_scheme_b);
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(
            sorted(names),
            ["none_auth_scheme_a", "none_auth_scheme_b"],
        )

    def test_cfg_disabled_macro_invoke_is_not_counted(self):
        """`#[cfg(windows)] generated!();` on a non-Windows runner is
        not expanded (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                () => {
                    #[test]
                    fn none_auth_scheme_generated() {}
                };
            }
            #[cfg(windows)]
            generated!();
            #[test]
            fn none_auth_scheme_live() {}
            """
        )
        names = _tests_in_file(text, [])
        if sys.platform == "win32":
            self.assertEqual(
                sorted(names),
                ["none_auth_scheme_generated", "none_auth_scheme_live"],
            )
        else:
            self.assertEqual(names, ["none_auth_scheme_live"])

    def test_cfg_attr_disabled_macro_invoke_is_not_counted(self):
        """`#[cfg_attr(test, cfg(windows))] generated!();` is off on
        Linux (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                () => {
                    #[test]
                    fn none_auth_scheme_phantom() {}
                };
            }
            #[cfg_attr(test, cfg(windows))]
            generated!();
            #[test]
            fn none_auth_scheme_live() {}
            """
        )
        names = _tests_in_file(text, [])
        if sys.platform == "win32":
            self.assertEqual(
                sorted(names),
                ["none_auth_scheme_live", "none_auth_scheme_phantom"],
            )
        else:
            self.assertEqual(names, ["none_auth_scheme_live"])

    def test_inner_cfg_attr_gates_the_whole_file(self):
        """`#![cfg_attr(test, cfg(windows))]` excludes the file on Linux
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "#![cfg_attr(test, cfg(windows))]\n"
                "#[test]\nfn none_auth_scheme_phantom() {}\n"
            )
            names = _qualified_test_names(root)
            if sys.platform == "win32":
                self.assertIn("none_auth_scheme_phantom", names)
            else:
                self.assertNotIn("none_auth_scheme_phantom", names)

    def test_cfg_disabled_module_emitting_macro_is_not_followed(self):
        """`#[cfg(windows)] generated!();` emitting `mod child;` is off
        on Linux (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "demo" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(
                "macro_rules! generated {\n"
                "    () => {\n"
                "        mod none_auth_scheme_child;\n"
                "    };\n"
                "}\n"
                "#[cfg(windows)]\n"
                "generated!();\n"
                "#[test]\nfn none_auth_scheme_live() {}\n"
            )
            (src / "none_auth_scheme_child.rs").write_text(
                "#[test]\nfn none_auth_scheme_child() {}\n"
            )
            names = _qualified_test_names(root)
            if sys.platform == "win32":
                self.assertIn("none_auth_scheme_child::none_auth_scheme_child", names)
            else:
                self.assertIn("none_auth_scheme_live", names)
                self.assertNotIn(
                    "none_auth_scheme_child::none_auth_scheme_child", names
                )

    def test_cfg_disabled_mod_macro_invoke_is_not_counted(self):
        """An invocation inside `#[cfg(windows)] mod` is off on Linux
        (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! generated {
                () => {
                    #[test]
                    fn none_auth_scheme_generated() {}
                };
            }
            #[cfg(windows)]
            mod none_auth_scheme_off {
                generated!();
            }
            #[test]
            fn none_auth_scheme_live() {}
            """
        )
        names = _tests_in_file(text, [])
        if sys.platform == "win32":
            self.assertIn("none_auth_scheme_off::none_auth_scheme_generated", names)
        else:
            self.assertEqual(names, ["none_auth_scheme_live"])

    def test_uninvoked_macro_mod_is_not_followed(self):
        """`mod alias;` inside an uninvoked macro is not a live module
        (#507 review)."""

        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            crate = root / "crates" / "demo"
            src = crate / "src"
            src.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
                encoding="utf-8",
            )
            (src / "lib.rs").write_text(
                "macro_rules! phantom {\n"
                "    () => { mod alias; };\n"
                "}\n"
                "#[test]\nfn live() {}\n",
                encoding="utf-8",
            )
            (src / "alias.rs").write_text(
                "#[test]\nfn none_auth_scheme_stale() {}\n",
                encoding="utf-8",
            )
            names = _qualified_test_names(root)
            self.assertIn("live", names)
            self.assertNotIn("alias::none_auth_scheme_stale", names)
            self.assertNotIn("none_auth_scheme_stale", names)

    def test_attr_string_brace_does_not_close_the_module(self):
        """`#[doc = "}"]` must not pop the enclosing inline module
        (#507 review)."""

        text = textwrap.dedent(
            """\
            mod none_auth_scheme_ {
                #[doc = "}"]
                #[test]
                fn works() {}
            }
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_::works"])

    def test_unexpanded_macro_rules_paren_body_is_not_counted(self):
        """`macro_rules! name ( ... )` is a valid delimiter; `#[test]`
        inside it is still not a libtest case (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! phantom (
                () => {
                    #[test]
                    fn none_auth_scheme_phantom() {}
                }
            );
            #[test]
            fn none_auth_scheme_live() {}
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_live"])
        self.assertNotIn("none_auth_scheme_phantom", names)

    def test_unexpanded_macro_rules_bracket_body_is_not_counted(self):
        """`macro_rules! name [ ... ]` is a valid delimiter (#507 review)."""

        text = textwrap.dedent(
            """\
            macro_rules! phantom [
                () => {
                    #[test]
                    fn none_auth_scheme_phantom() {}
                }
            ];
            #[test]
            fn none_auth_scheme_live() {}
            """
        )
        names = _tests_in_file(text, [])
        self.assertEqual(names, ["none_auth_scheme_live"])
        self.assertNotIn("none_auth_scheme_phantom", names)

    def test_src_bin_tests_are_not_scanned(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            src = root / "crates" / "codegen" / "demo" / "src"
            (src / "bin").mkdir(parents=True)
            (src / "lib.rs").write_text("")
            (src / "bin" / "workspace_server.rs").write_text(
                "#[test]\nfn none_auth_scheme_sends() {}\n"
            )
            names = _qualified_test_names(root)
            self.assertNotIn("none_auth_scheme_sends", names)
            self.assertTrue(all("workspace_server" not in n for n in names))


if __name__ == "__main__":
    unittest.main()
