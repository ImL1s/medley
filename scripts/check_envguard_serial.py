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
ENVGUARD_USE = re.compile(r"\bEnvGuard\s*::")
ENV_MUTATION = re.compile(
    r"\b(?:std\s*::\s*)?env\s*::\s*(?:set_var|remove_var)\s*\("
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


def _mentions_env_mutation(body: str) -> str | None:
    if ENVGUARD_USE.search(body):
        return "EnvGuard::"
    if ENV_MUTATION.search(body):
        return "std::env::{set_var,remove_var}"
    return None


def scan_source(source: str, *, relpath: Path | None = None) -> list[Finding]:
    """Return test functions that mutate process env without unkeyed serial."""

    path = relpath or Path("<input>")
    code = _code_only(source)
    findings: list[Finding] = []
    for match in FN_DEF.finditer(code):
        attrs = _preceding_attributes(source, code, match.start())
        if not any(_is_test_attr(attr) for attr in attrs):
            continue
        body_range = _fn_body(source, match.end())
        if body_range is None:
            continue
        body = code[body_range[0] : body_range[1]]
        mention = _mentions_env_mutation(body)
        if mention is None:
            continue
        kinds = [kind for attr in attrs if (kind := _serial_kind(attr))]
        if "unkeyed" in kinds:
            continue
        reason = (
            f"{mention} without unkeyed #[serial_test::serial]"
            if "keyed" not in kinds
            else f"{mention} with keyed #[serial(...)], which is not crate-wide"
        )
        findings.append(
            Finding(
                path=path,
                line=_line(source, match.start()),
                name=match.group("name"),
                reason=reason,
            )
        )
    return findings


def rust_files(scan_root: Path) -> list[Path]:
    return sorted(path for path in scan_root.rglob("*.rs") if path.is_file())


def scan_tree(scan_root: Path, *, repo: Path) -> list[Finding]:
    found: list[Finding] = []
    for path in rust_files(scan_root):
        rel = path.relative_to(repo)
        text = path.read_text(encoding="utf-8")
        found.extend(scan_source(text, relpath=rel))
    return found


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


def evaluate(
    findings: list[Finding],
    allowlist: list[str],
) -> tuple[list[Finding], list[str]]:
    allowed = set(allowlist)
    new = [item for item in findings if item.allowlist_id not in allowed]
    present = {item.allowlist_id for item in findings}
    stale = [entry for entry in allowlist if entry not in present]
    return new, stale


def format_report(
    new: list[Finding],
    stale: list[str],
    *,
    finding_count: int,
    allowlist_count: int,
) -> str:
    lines = [
        f"envguard-serial: {finding_count} violation(s), "
        f"{allowlist_count} allowlist entries, "
        f"{len(new)} new, {len(stale)} stale",
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

    allowlist = load_allowlist(allowlist_path)
    new, stale = evaluate(findings, allowlist)
    report = format_report(
        new,
        stale,
        finding_count=len(findings),
        allowlist_count=len(allowlist),
    )
    print(report)
    if new or stale:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
