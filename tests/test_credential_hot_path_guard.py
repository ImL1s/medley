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

import re
import sys
import tempfile
import textwrap
import tomllib
import unittest
from collections import deque
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CLAUDE_MD = ROOT / "CLAUDE.md"
_CRATE_ROOTS = ("crates", "prod", "third_party")

_TEST_ATTR = re.compile(r"^\s*#\[(?:tokio::)?test\b")
_IGNORE_ATTR = re.compile(r"^#\[\s*ignore\b")
_FN = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:(?:async|const|unsafe|extern(?:\s+\"[^\"]*\")?)\s+)*"
    r"fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)
_MOD_OPEN = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*\{"
)
_MOD_SEMI = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;"
)
_PATH_ATTR = re.compile(r'#\[path\s*=\s*"([^"]+)"\]')

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
_MACRO_INVOKE = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*!\s*[([{]")


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
        ident = re.match(r"[A-Za-z_][A-Za-z0-9_]*", masked[index:])
        if not ident:
            continue
        name = ident.group(0)
        index += ident.end()
        while index < len(masked) and masked[index].isspace():
            index += 1
        if index < len(masked) and masked[index] in "{([":
            defs.append((name, index, _balanced_pair_end(masked, index)))
    return defs


def _invoked_macro_names(
    masked: str, defs: list[tuple[str, int, int]]
) -> set[str]:
    """Macro names that have a `name!(...)` invocation outside any
    `macro_rules!` body. Invoked macros emit their `#[test]` items
    (#507 review)."""

    return {name for name, _inner in _macro_invoke_inners(masked, defs)}


_ARM_TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|[0-9]+|[^\sA-Za-z0-9_]")


def _macro_invoke_inners(
    masked: str, defs: list[tuple[str, int, int]]
) -> list[tuple[str, str]]:
    """`(name, invoke_inner)` for invocations outside macro definitions."""

    known = {name for name, _, _ in defs}
    if not known:
        return []
    bodies = [(start, end) for _, start, end in defs]
    out: list[tuple[str, str]] = []
    for match in _MACRO_INVOKE.finditer(masked):
        name = match.group(1)
        if name not in known:
            continue
        if any(start <= match.start() < end for start, end in bodies):
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
    cursor = 0
    for index, piece in enumerate(parts):
        lits = _ARM_TOKEN.findall(piece)
        for lit in lits:
            if cursor >= len(tokens) or tokens[cursor] != lit:
                return False
            cursor += 1
        if index < len(parts) - 1:
            if cursor >= len(tokens):
                return False
            cursor += 1
    return cursor == len(tokens)


def _inactive_macro_spans(masked: str) -> list[tuple[int, int]]:
    """Spans rustc does not expand: uninvoked macros and unselected arms.

    One `generated!(cold)` must not unmask a sibling `(hot)` arm
    (#507 review).
    """

    defs = _macro_rules_defs(masked)
    invokes_by_name: dict[str, list[str]] = {}
    for name, inner in _macro_invoke_inners(masked, defs):
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
            if dep not in enabled:
                enabled.add(dep)
                stack.append(dep)
    return enabled


def _manifest_default_features(text: str) -> set[str]:
    """Names listed in `[features] default = [...]`, including multiline."""

    data = _load_manifest_toml(text)
    if data is None:
        return set()
    feats = data.get("features")
    if not isinstance(feats, dict):
        return set()
    return _toml_str_list(feats.get("default"))


def _cargo_test_targets(
    root: Path,
) -> tuple[set[Path], set[Path], set[Path], set[Path], dict[Path, set[str]]]:
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
    for manifest in root.rglob("Cargo.toml"):
        if "target" in manifest.parts:
            continue
        try:
            text = manifest.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        crate = manifest.parent
        data = _load_manifest_toml(text)
        if data is not None:
            pkg = data.get("package")
            if isinstance(pkg, dict) and pkg.get("autotests") is False:
                no_autotest.add(crate.resolve())
            feat_table = (
                data.get("features")
                if isinstance(data.get("features"), dict)
                else {}
            )
            default_feats = _feature_closure(
                feat_table, _toml_str_list(feat_table.get("default"))
            )
            crate_feats[crate.resolve()] = default_feats
            lib = data.get("lib")
            if isinstance(lib, dict):
                lib_path = lib.get("path")
                if isinstance(lib_path, str):
                    target = (crate / lib_path).resolve()
                    if target.is_file():
                        extra.add(target)
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
                extra_required = required_feats - default_feats
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

        def flush() -> None:
            nonlocal name, path_s, required_feats, in_test, in_lib, lib_path
            if in_lib and lib_path:
                target = (crate / lib_path).resolve()
                if target.is_file():
                    extra.add(target)
                suppressed_libs.add((crate / "src" / "lib.rs").resolve())
            if in_test:
                target = None
                if path_s:
                    target = (crate / path_s).resolve()
                elif name:
                    target = (crate / "tests" / f"{name}.rs").resolve()
                if target is not None:
                    extra_required = required_feats - default_feats
                    if extra_required:
                        gated.add(target)
                    elif target.is_file():
                        extra.add(target)
            name = None
            path_s = None
            lib_path = None
            required_feats = set()
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
            if stripped.startswith("required-features"):
                inner = stripped.split("=", 1)[-1]
                required_feats = set(re.findall(r'"([^"]+)"', inner))
        flush()
        crate_feats[crate.resolve()] = default_feats
    return extra, gated, suppressed_libs, no_autotest, crate_feats


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
) -> list[tuple[str, Path, tuple[str, ...]]]:
    """`(name, child file, enclosing inline modules)` for `#[path]` and
    ordinary `mod name;` that resolve.

    `mod inner;` inside `mod outer { ... }` is loaded from `outer/inner.rs`
    (or `outer/inner/mod.rs`) relative to the declaring file's module search
    dir, not from the declaring file's own directory, and libtest qualifies
    it as `outer::inner::...` (#507 review).
    """

    decls: list[tuple[str, Path, tuple[str, ...]]] = []
    pending_path: str | None = None
    pending_attrs: list[str] = []
    raw_lines = text.splitlines()
    masked = _mask_rust_literals(text)
    masked_lines = masked.splitlines()
    if len(masked_lines) < len(raw_lines):
        masked_lines.extend([""] * (len(raw_lines) - len(masked_lines)))
    inactive = _inactive_macro_spans(masked)
    depth = 0
    inline_stack: list[tuple[int, str, bool]] = []
    line_start = 0
    for i, raw in enumerate(raw_lines):
        line = _strip_line_comment(masked_lines[i])
        raw_no_line_comment = _strip_line_comment(raw)
        stripped_raw = raw_no_line_comment.strip()
        attrs, remainder = _leading_attrs(line)
        enclosing_off = any(off for _, _, off in inline_stack)
        in_macro = any(start <= line_start < end for start, end in inactive)
        # `#[path = "x"]` stores the path in a string, so a full literal
        # mask blanks it. Search the line-comment-stripped raw line, then
        # keep the match only if that `#[path]` is live code: the `#`
        # survives the comment/string mask. A `#[path]` inside `/* ... */`
        # or a string is spaces in the mask (#507 review).
        path_match = _PATH_ATTR.search(raw_no_line_comment)
        if path_match:
            start = path_match.start()
            masked_line = masked_lines[i]
            if start >= len(masked_line) or masked_line[start] != "#":
                path_match = None
        if path_match:
            pending_path = path_match.group(1)
            pending_attrs.extend(attrs)
        semi = _MOD_SEMI.match(line) or _MOD_SEMI.match(remainder)
        if semi:
            cfg_off = any(
                _cfg_attr_is_inactive(a, enabled_features)
                for a in pending_attrs + attrs
            )
            skip = enclosing_off or cfg_off or in_macro
            if not skip:
                name = semi.group(1)
                inline_names = tuple(n for _, n, _ in inline_stack)
                search = _mod_search_dir(
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
                    decls.append((name, child, inline_names))
                else:
                    child = _existing_mod_file(search, name)
                    if child is not None:
                        decls.append((name, child, inline_names))
            pending_path = None
            pending_attrs = []
        elif not path_match:
            cfg_off = any(
                _cfg_attr_is_inactive(a, enabled_features)
                for a in pending_attrs + attrs
            )
            skip = enclosing_off or cfg_off
            brace_mod = _MOD_OPEN.match(line) or _MOD_OPEN.match(remainder)
            if brace_mod:
                inline_stack.append((depth, brace_mod.group(1), skip))
                pending_path = None
                pending_attrs = []
            elif attrs and not remainder.strip():
                pending_attrs.extend(attrs)
            elif stripped_raw:
                pending_path = None
                pending_attrs = []
        depth += line.count("{") - line.count("}")
        while inline_stack and depth <= inline_stack[-1][0]:
            inline_stack.pop()
        line_start += len(masked_lines[i]) + 1
    return decls


def _declared_module_overrides(root: Path) -> dict[Path, list[list[str]]]:
    """Logical module prefixes for files reached via `mod` / `#[path]`.

    Ordinary `mod common;` from several integration roots counts once per
    target (#507 review). `#[path]` prefixes propagate into descendant
    `mod child;` files so Cargo's logical path is what the scan records.
    """
    overrides: dict[Path, list[list[str]]] = {}
    queue: deque[tuple[Path, tuple[str, ...], tuple[Path, ...], bool]] = deque()
    texts: dict[Path, str] = {}

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

    extra_roots, gated_roots, suppressed_libs, no_autotest, crate_feats = (
        _cargo_test_targets(root)
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
                    (rs.resolve(), tuple(_path_module_prefix(rs)), (), True)
                )

    while queue:
        declaring, prefix, ancestors, as_crate_root = queue.popleft()
        if declaring in ancestors:
            continue
        text = read_rs(declaring)
        if text is None:
            continue
        enabled = _features_for(declaring, crate_feats)
        if _file_inner_cfg_inactive(text, enabled):
            continue
        for name, child, inline_names in _iter_module_decls(
            text,
            declaring,
            extra_roots,
            gated_roots,
            as_crate_root=as_crate_root,
            enabled_features=enabled,
        ):
            child_prefix = list(prefix) + list(inline_names) + [name]
            overrides.setdefault(child, []).append(child_prefix)
            queue.append(
                (child, tuple(child_prefix), ancestors + (declaring,), False)
            )
    return overrides


_CFG_ATTR = re.compile(r"^#\[\s*cfg\s*\((.*)\)\s*\]\s*$", re.DOTALL)


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


def _cfg_attr_is_inactive(
    attr: str, enabled_features: set[str] | frozenset[str] | None = None
) -> bool:
    match = _CFG_ATTR.match(attr.strip())
    if match is None:
        return False
    return _eval_cfg(match.group(1).strip(), enabled_features) is False


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
                if _cfg_attr_is_inactive(attr, enabled_features):
                    return True
            continue
        return False
    return False


def _leading_attrs(line: str) -> tuple[list[str], str]:
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
            break
    return attrs, line[i:]


def _tests_in_file(
    text: str,
    file_mods: list[str],
    enabled_features: set[str] | frozenset[str] | None = None,
) -> list[str]:
    if _file_inner_cfg_inactive(text, enabled_features):
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
    macro_spans = _macro_rules_body_spans(masked)
    mod_stack: list[tuple[int, str, bool]] = []
    pending: list[str] = []
    depth = 0
    n = len(raw_lines)
    line_start = 0
    for i in range(n):
        masked_line = masked_lines[i]
        attrs, remainder_masked = _leading_attrs(masked_line)
        has_test = any(_TEST_ATTR.match(a) for a in attrs) or bool(
            _TEST_ATTR.match(masked_line)
        )
        enclosing_off = any(off for _, _, off in mod_stack)

        if has_test:
            all_attrs = pending + attrs
            pending = []
            found = None
            for follow in [remainder_masked, *masked_lines[i + 1 :]]:
                follow = follow.strip()
                if follow.startswith("//"):
                    continue
                if follow.startswith("#["):
                    more, rest = _leading_attrs(follow)
                    all_attrs.extend(more)
                    matched = _FN.match(rest)
                    if matched:
                        found = matched.group(1)
                        break
                    continue
                if not follow:
                    continue
                matched = _FN.match(follow)
                if matched:
                    found = matched.group(1)
                    break
                if re.match(
                    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:mod|struct|enum|impl|use)\b",
                    follow,
                ):
                    break
                continue
            inactive = enclosing_off or any(
                _cfg_attr_is_inactive(a, enabled_features) for a in all_attrs
            )
            ignored = any(_IGNORE_ATTR.match(a.strip()) for a in all_attrs)
            in_macro = any(start <= line_start < end for start, end in macro_spans)
            if found and not inactive and not ignored and not in_macro:
                prefix_parts = file_mods + [name for _, name, _ in mod_stack]
                prefix = "::".join(prefix_parts)
                names.append(f"{prefix}::{found}" if prefix else found)
        elif attrs and not remainder_masked.strip():
            pending.extend(attrs)
        elif remainder_masked.strip():
            item_off = enclosing_off or any(
                _cfg_attr_is_inactive(a, enabled_features) for a in pending + attrs
            )
            pending = []
            line = _strip_line_comment(masked_line)
            mod_match = _MOD_OPEN.match(line) or _MOD_OPEN.match(
                remainder_masked
            )
            if mod_match:
                mod_stack.append((depth, mod_match.group(1), item_off))

        line = _strip_line_comment(brace_lines[i])
        if not remainder_masked.strip() or has_test:
            pass
        depth += line.count("{") - line.count("}")
        while mod_stack and depth <= mod_stack[-1][0]:
            mod_stack.pop()
        line_start += len(masked_line) + 1
    return names


def _module_prefixes_for_source(
    rs: Path,
    overrides: dict[Path, list[list[str]]],
    extra_roots: set[Path] | frozenset[Path] | None = None,
    gated_roots: set[Path] | frozenset[Path] | None = None,
    suppressed_libs: set[Path] | frozenset[Path] | None = None,
    no_autotest: set[Path] | frozenset[Path] | None = None,
) -> list[list[str]] | None:
    """Prefixes to scan `rs` under, or `None` to skip an unreachable file.

    Cargo crate roots (`src/lib.rs`, `tests/*.rs`) keep their path prefix
    even when nothing `mod`s them. Every other file must appear in the
    module graph (#507 review); an orphan leftover after a `mod` was
    removed is not compiled and must not inflate CLAUDE.md counts.
    """

    key = rs.resolve()
    prefixes: list[list[str]] = []
    if key in overrides:
        prefixes.extend(overrides[key])
    if _is_cargo_crate_root_file(
        rs, extra_roots, gated_roots, suppressed_libs, no_autotest
    ):
        root_prefix = _path_module_prefix(rs)
        if root_prefix not in prefixes:
            prefixes.append(root_prefix)
    return prefixes or None


def _qualified_test_names(root: Path) -> list[str]:
    """Every `#[test]`/`#[tokio::test]` function's qualified name under
    `crates/`/`prod/` -- `src/` and `tests/` alike -- prefixed with its
    file-path module plus in-file `mod X { ... }` blocks.

    File-path prefix covers the common `mod name;` / `name.rs` shape
    libtest uses (#507 review). `#[path = "..."] mod name;` replaces the
    target file's path prefix with the declaring file's prefix plus
    `name`. Cross-file `mod x;`
    whose file is not `x.rs` and has no `#[path]` is still a shorter
    name than cargo would report -- conservative, same direction as
    the defect this guard exists to catch.
    """
    overrides = _declared_module_overrides(root)
    extra_roots, gated_roots, suppressed_libs, no_autotest, crate_feats = (
        _cargo_test_targets(root)
    )
    names: list[str] = []
    for base in _CRATE_ROOTS:
        base_dir = root / base
        if not base_dir.is_dir():
            continue
        for rs in base_dir.rglob("*.rs"):
            if (
                not _is_lib_or_integration_source(rs, extra_roots)
                and rs.resolve() not in overrides
            ):
                continue
            try:
                text = rs.read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError):
                continue
            enabled = _features_for(rs, crate_feats)
            if _file_inner_cfg_inactive(text, enabled):
                continue
            prefix_lists = _module_prefixes_for_source(
                rs, overrides, extra_roots, gated_roots, suppressed_libs, no_autotest
            )
            if prefix_lists is None:
                continue
            for file_mods in prefix_lists:
                names.extend(_tests_in_file(text, file_mods, enabled))
    return names


class CredentialHotPathCorpus(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.documented = parse_documented_hot_path(
            CLAUDE_MD.read_text(encoding="utf-8")
        )
        cls.names = _qualified_test_names(ROOT)

    def test_the_corpus_is_not_empty(self):
        # A scan that silently finds nothing satisfies every assertion
        # below while checking nothing at all.
        self.assertGreater(len(self.names), 1000, len(self.names))

    def test_claude_md_still_documents_all_required_hot_path_entries(self):
        # A deleted named entry must not drop out of the count loop
        # because this assertion only listed the four patterns (#507 review).
        self.assertEqual(set(self.documented), _REQUIRED_HOT_PATH_ENTRIES)

    def test_each_documented_entry_selects_its_documented_count(self):
        # The counterexample CLAUDE.md's own commit history should never
        # reproduce (#507 review): editing only this file's number, with
        # nothing about the source changing, must turn this test red --
        # the guard checks CLAUDE.md's count against source, not a copy
        # of the count against itself.
        wrong = {}
        for pattern, expected in self.documented.items():
            matched = [n for n in self.names if pattern in n]
            if len(matched) != expected:
                wrong[pattern] = (len(matched), expected, matched)
        self.assertEqual(
            wrong,
            {},
            f"CLAUDE.md's documented count does not match source (got, "
            f"documented, matches): {wrong}",
        )


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

    def test_path_prefix_propagates_to_descendant_modules(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            session = root / "crates" / "codegen" / "demo" / "src" / "session"
            impl = session / "acp_session_impl"
            (impl / "extensions").mkdir(parents=True)
            (root / "crates" / "codegen" / "demo" / "src" / "lib.rs").write_text(
                "mod session;\n"
            )
            (session / "mod.rs").write_text("mod acp_session;\n")
            (session / "acp_session.rs").write_text(
                '#[path = "acp_session_impl/extensions.rs"]\nmod extensions;\n'
            )
            (impl / "extensions.rs").write_text("mod idle_prompt;\n")
            (impl / "extensions" / "idle_prompt.rs").write_text(
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
