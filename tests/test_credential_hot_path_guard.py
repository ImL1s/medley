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
import unittest
from collections import deque
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CLAUDE_MD = ROOT / "CLAUDE.md"
_CRATE_ROOTS = ("crates", "prod", "third_party")

_TEST_ATTR = re.compile(r"^\s*#\[(?:tokio::)?test\b")
_FN = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"
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
) -> bool:
    """`src/lib.rs`, an integration target `tests/*.rs`, or an explicit `[[test]].path`.

    `required-features` targets are off by default and are not part of the
    documented hot-path invocation (#507 review).
    """

    key = rs.resolve()
    if gated_roots and key in gated_roots:
        return False
    if extra_roots and key in extra_roots:
        return True
    split = _crate_source_rel(rs)
    if split is None:
        return False
    marker, rest = split
    if marker == "src" and rest == ["lib.rs"]:
        return True
    if marker == "tests" and len(rest) == 1 and rest[0].endswith(".rs"):
        return True
    return False


def _manifest_default_features(text: str) -> set[str]:
    """Names listed in `[features] default = [...]`, if any."""

    in_features = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped == "[features]":
            in_features = True
            continue
        if stripped.startswith("["):
            in_features = False
            continue
        if not in_features:
            continue
        match = re.match(r"^default\s*=\s*\[(.*)\]", stripped)
        if match:
            return set(re.findall(r'"([^"]+)"', match.group(1)))
    return set()


def _cargo_test_targets(root: Path) -> tuple[set[Path], set[Path]]:
    """Explicit `[[test]]` paths and feature-gated integration targets.

    Extra roots: `path =` files cargo compiles without extra features.
    `tests/leader_pty_e2e/mod.rs` is one -- `_is_cargo_crate_root_file`'s
    `tests/*.rs` shape misses it (#507 review).

    Gated: `required-features` targets whose features are not all in the
    crate's `default` set. Default `cargo test` (the CLAUDE.md hot path)
    does not build those.
    """

    extra: set[Path] = set()
    gated: set[Path] = set()
    for manifest in root.rglob("Cargo.toml"):
        if "target" in manifest.parts:
            continue
        try:
            text = manifest.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        crate = manifest.parent
        default_feats = _manifest_default_features(text)
        in_test = False
        name: str | None = None
        path_s: str | None = None
        required_feats: set[str] = set()

        def flush() -> None:
            nonlocal name, path_s, required_feats, in_test
            if not in_test:
                return
            target: Path | None = None
            if path_s:
                target = (crate / path_s).resolve()
            elif name:
                target = (crate / "tests" / f"{name}.rs").resolve()
            if target is not None:
                extra_required = required_feats - default_feats
                if extra_required:
                    gated.add(target)
                elif path_s and target.is_file():
                    extra.add(target)
            name = None
            path_s = None
            required_feats = set()
            in_test = False

        for line in text.splitlines():
            stripped = line.strip()
            if stripped == "[[test]]":
                flush()
                in_test = True
                name = None
                path_s = None
                required_feats = set()
                continue
            if stripped.startswith("["):
                flush()
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
    return extra, gated


def _mod_search_dir(
    declaring: Path,
    extra_roots: set[Path] | frozenset[Path] | None = None,
    gated_roots: set[Path] | frozenset[Path] | None = None,
) -> Path:
    """Directory rustc searches for `mod name;` declared in `declaring`.

    An explicit `[[test]] path = "integration/custom.rs"` is a crate root,
    so `mod child;` loads `integration/child.rs`, not
    `integration/custom/child.rs` (#507 review).
    """

    if declaring.name in ("lib.rs", "main.rs", "mod.rs"):
        return declaring.parent
    if _is_cargo_crate_root_file(declaring, extra_roots, gated_roots):
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
    masked_lines = _mask_rust_literals(text).splitlines()
    if len(masked_lines) < len(raw_lines):
        masked_lines.extend([""] * (len(raw_lines) - len(masked_lines)))
    depth = 0
    inline_stack: list[tuple[int, str, bool]] = []
    for i, raw in enumerate(raw_lines):
        line = _strip_line_comment(masked_lines[i])
        raw_no_line_comment = _strip_line_comment(raw)
        stripped_raw = raw_no_line_comment.strip()
        attrs, remainder = _leading_attrs(line)
        enclosing_off = any(off for _, _, off in inline_stack)
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
                _cfg_attr_is_inactive(a) for a in pending_attrs + attrs
            )
            skip = enclosing_off or cfg_off
            if not skip:
                name = semi.group(1)
                inline_names = tuple(n for _, n, _ in inline_stack)
                search = _mod_search_dir(declaring, extra_roots, gated_roots)
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
                _cfg_attr_is_inactive(a) for a in pending_attrs + attrs
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
    return decls


def _declared_module_overrides(root: Path) -> dict[Path, list[list[str]]]:
    """Logical module prefixes for files reached via `mod` / `#[path]`.

    Ordinary `mod common;` from several integration roots counts once per
    target (#507 review). `#[path]` prefixes propagate into descendant
    `mod child;` files so Cargo's logical path is what the scan records.
    """
    overrides: dict[Path, list[list[str]]] = {}
    queue: deque[tuple[Path, tuple[str, ...], tuple[Path, ...]]] = deque()
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

    extra_roots, gated_roots = _cargo_test_targets(root)
    for base in _CRATE_ROOTS:
        base_dir = root / base
        if not base_dir.is_dir():
            continue
        for rs in base_dir.rglob("*.rs"):
            if not _is_lib_or_integration_source(rs, extra_roots):
                continue
            if _is_cargo_crate_root_file(rs, extra_roots, gated_roots):
                queue.append((rs.resolve(), tuple(_path_module_prefix(rs)), ()))

    while queue:
        declaring, prefix, ancestors = queue.popleft()
        if declaring in ancestors:
            continue
        text = read_rs(declaring)
        if text is None:
            continue
        for name, child, inline_names in _iter_module_decls(
            text, declaring, extra_roots, gated_roots
        ):
            child_prefix = list(prefix) + list(inline_names) + [name]
            overrides.setdefault(child, []).append(child_prefix)
            queue.append((child, tuple(child_prefix), ancestors + (declaring,)))
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


def _cfg_atom(atom: str) -> bool | None:
    atom = atom.strip()
    if atom == "test":
        return True
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
    return None


def _eval_cfg(expr: str) -> bool | None:
    expr = expr.strip()
    for kind in ("not", "all", "any"):
        prefix = f"{kind}("
        if expr.startswith(prefix) and expr.endswith(")"):
            inner = expr[len(prefix) : -1]
            if kind == "not":
                value = _eval_cfg(inner)
                return None if value is None else (not value)
            values = [_eval_cfg(part) for part in _cfg_split_args(inner)]
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
    return _cfg_atom(expr)


def _cfg_attr_is_inactive(attr: str) -> bool:
    match = _CFG_ATTR.match(attr.strip())
    if match is None:
        return False
    return _eval_cfg(match.group(1).strip()) is False


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


def _tests_in_file(text: str, file_mods: list[str]) -> list[str]:
    names: list[str] = []
    raw_lines = text.splitlines()
    masked_lines = _mask_rust_literals(text).splitlines()
    if len(masked_lines) < len(raw_lines):
        masked_lines.extend([""] * (len(raw_lines) - len(masked_lines)))
    mod_stack: list[tuple[int, str, bool]] = []
    pending: list[str] = []
    depth = 0
    n = len(raw_lines)
    for i in range(n):
        masked = masked_lines[i]
        attrs, remainder_masked = _leading_attrs(masked)
        has_test = any(_TEST_ATTR.match(a) for a in attrs) or bool(
            _TEST_ATTR.match(masked)
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
            inactive = enclosing_off or any(
                _cfg_attr_is_inactive(a) for a in all_attrs
            )
            if found and not inactive:
                prefix_parts = file_mods + [name for _, name, _ in mod_stack]
                prefix = "::".join(prefix_parts)
                names.append(f"{prefix}::{found}" if prefix else found)
        elif attrs and not remainder_masked.strip():
            pending.extend(attrs)
        elif remainder_masked.strip():
            item_off = enclosing_off or any(_cfg_attr_is_inactive(a) for a in pending)
            pending = []
            line = _strip_line_comment(masked)
            mod_match = _MOD_OPEN.match(line)
            if mod_match:
                mod_stack.append((depth, mod_match.group(1), item_off))

        line = _strip_line_comment(masked)
        if not remainder_masked.strip() or has_test:
            pass
        depth += line.count("{") - line.count("}")
        while mod_stack and depth <= mod_stack[-1][0]:
            mod_stack.pop()
    return names


def _module_prefixes_for_source(
    rs: Path,
    overrides: dict[Path, list[list[str]]],
    extra_roots: set[Path] | frozenset[Path] | None = None,
    gated_roots: set[Path] | frozenset[Path] | None = None,
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
    if _is_cargo_crate_root_file(rs, extra_roots, gated_roots):
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
    extra_roots, gated_roots = _cargo_test_targets(root)
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
            prefix_lists = _module_prefixes_for_source(
                rs, overrides, extra_roots, gated_roots
            )
            if prefix_lists is None:
                continue
            for file_mods in prefix_lists:
                names.extend(_tests_in_file(text, file_mods))
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
