"""Static regression guard for credential-derived observability fields.

The scanner is dependency-free so CI can run it before any Rust build. It
understands balanced Rust macro invocations, including nested delimiters and
strings, rather than relying on one-line regular expressions.
"""

from __future__ import annotations

import re
import sys
import unittest
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = Path(__file__).with_name("fixtures") / "credential_observability_guard"
RAW_STRING_START = re.compile(r'r(#+)?"')
CHAR_LITERAL = re.compile(r"'(?:\\.|[^\\'\n])'")

LEGACY_FRAGMENT_IDENTIFIERS = {
    "key_suffix",
    "token_suffix",
    "bearer_tail_fragment",
    "StampedBearerSuffix",
    "SENT_BEARER_PREFIX_LEN",
    "bearer_suffix",
    "sent_bearer_prefix",
    "auth_prefix",
    "key_prefix",
    "rt_prefix",
    "sent_key_prefix",
    "current_key_prefix",
    "tried_rt_prefix",
    "disk_rt_prefix",
    "disk_key_prefix",
    "tried_key_prefix",
    "adopted_key_prefix",
    "prev_key_prefix",
    "new_key_prefix",
    "old_key_prefix",
    "retained_key_prefix",
    "dropped_key_prefix",
    "written_key_prefix",
    "child_key_prefix",
    "parent_key_prefix",
}
FORBIDDEN_APIS = {
    "key_suffix",
    "attempt_bearer",
    "wire_bearer",
    "failed_bearer",
    "deployment_id_from_key",
    "api_key_id_for",
}
SECRET_CARRIERS = {
    "ChatStateSnapshot",
    "ConfigModelOverride",
    "Credentials",
    "CredentialSnapshot",
    "EndpointsConfig",
    "EndpointScopedCredentials",
    "ExternalOtelConfig",
    "ExternalOtelFileConfig",
    "GrokAuth",
    "GrokComConfig",
    "ImageGenConfig",
    "ManagedMcpConfig",
    "ModelEntry",
    "ModelProviderConfig",
    "ModelEntryConfig",
    "ModelInfo",
    "ModelsConfig",
    "OtelExporterConfig",
    "RemoteConfig",
    "MultipartInitResponse",
    "S3AccessCredentials",
    "SamplerConfig",
    "SamplingConfig",
    "SignedPartUrl",
    "UploadMethod",
    "TraceExportConfig",
    "TokenResponse",
    "VideoGenConfig",
    "VideoGenPollResponse",
    "VideoGenVideoInfo",
    "WebSearchConfig",
    "ZdrVideoOutputS3Config",
    "RefreshOutcome",
}

# These fields contain opaque cursors, cancellation handles, or local generation
# counters rather than authentication material. Keep exclusions type-qualified so
# a newly introduced generic `token` field is not silently treated as safe.
NON_SECRET_TOKEN_FIELDS = {
    ("CompositeCursor", "conv_page_token"),
    ("ConvQuery", "page_token"),
    ("CreateWorktreeFromWorktreeRequest", "cancellation_token"),
    ("ForeignResumeLaunch", "token"),
    ("ListConversationsPage", "next_page_token"),
    ("ListConversationsResponseWire", "next_page_token"),
    ("ListWorkspacesPage", "next_page_token"),
    ("ListWorkspacesResponseWire", "next_page_token"),
    ("SubagentRequest", "cancel_token"),
    ("WorkspacesListRequest", "page_token"),
    ("WorkspacesListResponse", "next_page_token"),
    ("WsQuery", "page_token"),
}
KNOWN_GENERIC_CREDENTIAL_FIELDS = {
    "CredentialSnapshot": {"token"},
    "GrokAuth": {"key"},
}

PRODUCTION_RUST_ROOTS = ("crates", "prod")
# This crate contains mock capture servers and test fixtures; its structs are
# deliberately raw assertion surfaces and are never linked into production.
NON_PRODUCTION_CRATE_ROOTS = {Path("crates/codegen/xai-grok-test-support")}
OBSERVABILITY_ERROR_MACROS = {
    "trace",
    "debug",
    "info",
    "warn",
    "error",
    "event",
    "span",
    "trace_span",
    "debug_span",
    "info_span",
    "warn_span",
    "error_span",
    "println",
    "eprintln",
    "panic",
    "unreachable",
    "todo",
    "anyhow",
    "bail",
    "ensure",
}

SAFE_OBSERVABILITY_IDENTIFIERS = {
    "log_url",
    "redacted_url",
    "safe_url",
    "sanitized_url",
    "status",
    "status_code",
    "transport_kind",
    "failure_kind",
}
CONFIGURABLE_URL_IDENTIFIERS = {
    "config_url",
    "server_url",
    "proxy_url",
    "proxy_base_url",
    "endpoint_url",
    "request_url",
    "managed_config_url",
    "api_base_url",
    "models_base_url",
    "models_list_url",
    "feedback_url",
    "feedback_proxy_url",
    "trace_upload_url",
    "grok_ws_url",
    "grok_ws_origin",
    "cli_chat_proxy_base_url",
    "xai_api_base_url",
    "hub_url",
    "npm_registry",
}
RESPONSE_CONTENT_IDENTIFIERS = {
    "body_preview",
    "error_body",
    "provider_message",
    "response_body",
    "response_preview",
    "response_text",
}
RAW_ERROR_IDENTIFIERS = {
    "err",
    "error",
    "reqwest_error",
    "request_error",
    "send_error",
    "transport_error",
}
GENERIC_LOCATION_IDENTIFIERS = {"url", "uri"}
GENERIC_METADATA_PARSE_IDENTIFIERS = {"key", "value", "v", "error"}


def _is_credential_shaped_identifier(type_name: str, name: str) -> bool:
    """Recognize values whose raw contents can authenticate a request."""

    if (type_name, name) in NON_SECRET_TOKEN_FIELDS:
        return False
    return bool(
        name in {"authorization", "x_api_key"}
        or re.search(r"(?:^|_)(?:api_key|secret|credential|credentials)$", name)
        or re.search(
            r"(?:^|_)(?:access|auth|bearer|client|id|jwt|mixpanel|refresh)_token$",
            name,
        )
        or re.search(
            r"(?:^|_)(?:alpha_test|deployment|events|management|private|service_account)_key$",
            name,
        )
        or re.search(r"(?:^|_)(?:private|service_account)_(?:key_pem|key_json|json)$", name)
    )


def _is_raw_observability_credential_identifier(name: str) -> bool:
    """Match concrete credential values, excluding ambiguous generic locals."""

    return bool(
        name in {"authorization", "x_api_key"}
        or re.search(r"(?:^|_)(?:api_key|client_secret)$", name)
        or re.search(
            r"(?:^|_)(?:access|auth|bearer|id|jwt|mixpanel|refresh)_token$",
            name,
        )
        or re.search(
            r"(?:^|_)(?:alpha_test|deployment|events|management|private|service_account)_key$",
            name,
        )
        or re.search(r"(?:^|_)(?:private|service_account)_(?:key_pem|key_json|json)$", name)
    )


def _block_ranges(code: str) -> list[tuple[int, int]]:
    """Return balanced brace ranges from already literal-masked Rust code."""

    stack: list[int] = []
    ranges: list[tuple[int, int]] = []
    for index, char in enumerate(code):
        if char == "{":
            stack.append(index)
        elif char == "}" and stack:
            ranges.append((stack.pop(), index + 1))
    return ranges


def _scope_end(blocks: list[tuple[int, int]], position: int, fallback: int) -> int:
    containing = [block for block in blocks if block[0] < position < block[1]]
    return min(containing, key=lambda block: block[1] - block[0])[1] if containing else fallback


def _value_preserving_path(expression: str) -> list[str] | None:
    """Return identifiers for a simple field/alias read, not a computation."""

    allowed_projections = {
        "as_deref",
        "as_ref",
        "as_str",
        "clone",
        "cloned",
        "copied",
        "expect",
        "flatten",
        "ok_or",
        "ok_or_else",
        "take",
        "to_owned",
        "to_string",
        "unwrap",
        "unwrap_or",
        "unwrap_or_default",
        "unwrap_or_else",
    }
    cursor = 0
    while cursor < len(expression) and expression[cursor].isspace():
        cursor += 1
    if cursor < len(expression) and expression[cursor] in "&*":
        cursor += 1
    while cursor < len(expression) and expression[cursor].isspace():
        cursor += 1
    mut_prefix = re.match(r"mut\b", expression[cursor:])
    if mut_prefix:
        cursor += mut_prefix.end()

    path: list[str] = []
    expect_component = True
    while cursor < len(expression):
        while cursor < len(expression) and expression[cursor].isspace():
            cursor += 1
        if cursor >= len(expression):
            break
        if not expect_component:
            if expression[cursor] == "?":
                cursor += 1
                continue
            if expression[cursor] != ".":
                return None
            cursor += 1
            while cursor < len(expression) and expression[cursor].isspace():
                cursor += 1

        identifier = re.match(r"[A-Za-z_]\w*", expression[cursor:])
        if identifier is None:
            return None
        value = identifier.group(0)
        cursor += identifier.end()
        while cursor < len(expression) and expression[cursor].isspace():
            cursor += 1

        if cursor < len(expression) and expression[cursor] == "(":
            if not path or value not in allowed_projections:
                return None
            end = _balanced_block_end(expression, cursor)
            if end <= cursor or expression[end - 1 : end] != ")":
                return None
            cursor = end
        else:
            path.append(value)
        expect_component = False

    if not path:
        return None
    return path


def _generic_credential_fields(
    code: str, carrier_types: set[str]
) -> dict[str, set[str]]:
    """Return generic `key`/`token` fields that are secret on known carriers."""

    fields = {
        type_name: set(field_names)
        for type_name, field_names in KNOWN_GENERIC_CREDENTIAL_FIELDS.items()
    }
    struct_start = re.compile(r"\bstruct\s+(?P<type>[A-Za-z_]\w*)\b[^;{]*\{", re.DOTALL)
    field_name = re.compile(
        r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?P<field>key|token)\s*:"
    )
    for match in struct_start.finditer(code):
        type_name = match.group("type")
        if type_name not in carrier_types:
            continue
        open_index = code.find("{", match.start(), match.end())
        end = _balanced_block_end(code, open_index)
        for field in field_name.finditer(code[open_index + 1 : end - 1]):
            field_value = field.group("field")
            if (type_name, field_value) not in NON_SECRET_TOKEN_FIELDS:
                fields.setdefault(type_name, set()).add(field_value)
    return fields


def _generic_credential_bindings(
    code: str,
    blocks: list[tuple[int, int]],
    generic_fields: dict[str, set[str]],
) -> dict[str, list[tuple[int, str, int]]]:
    """Index typed carrier bindings by their lexical Rust block."""

    bindings: dict[str, list[tuple[int, str, int]]] = {}
    if not generic_fields:
        return bindings
    carrier_names = "|".join(map(re.escape, sorted(generic_fields)))
    type_ref = rf"(?:[A-Za-z_]\w*\s*::\s*)*(?P<type>{carrier_names})\b"

    def add(name: str, type_name: str, start: int, end: int) -> None:
        bindings.setdefault(name, []).append((start, type_name, end))

    function_start = re.compile(r"\bfn\s+[A-Za-z_]\w*(?:\s*<[^>{}]*>)?\s*\(")
    typed_param = re.compile(
        rf"\b(?P<name>[A-Za-z_]\w*)\s*:\s*[^,\n)]*?{type_ref}"
    )
    for function in function_start.finditer(code):
        open_paren = code.find("(", function.start(), function.end())
        close_paren = _balanced_block_end(code, open_paren)
        tail = re.search(r"[;{]", code[close_paren:])
        if tail is None or tail.group(0) != "{":
            continue
        body_open = close_paren + tail.start()
        body_end = _balanced_block_end(code, body_open)
        params = code[open_paren + 1 : close_paren - 1]
        for parameter in typed_param.finditer(params):
            add(parameter.group("name"), parameter.group("type"), body_open, body_end)

    closure_start = re.compile(r"\|(?P<params>[^|\n]*)\|")
    for closure in closure_start.finditer(code):
        parameters = list(typed_param.finditer(closure.group("params")))
        if not parameters:
            continue
        body_start = closure.end()
        while body_start < len(code) and code[body_start].isspace():
            body_start += 1
        lexical_end = _scope_end(blocks, closure.start(), len(code))
        if body_start < len(code) and code[body_start] == "{":
            body_end = _balanced_block_end(code, body_start)
        else:
            semicolon = code.find(";", body_start, lexical_end)
            body_end = semicolon + 1 if semicolon >= 0 else lexical_end
        for parameter in parameters:
            add(
                parameter.group("name"),
                parameter.group("type"),
                body_start,
                body_end,
            )

    typed_let = re.compile(
        rf"\blet\s+(?:mut\s+)?(?P<name>[A-Za-z_]\w*)\s*:\s*[^=;\n]*?{type_ref}"
    )
    for binding in typed_let.finditer(code):
        add(
            binding.group("name"),
            binding.group("type"),
            binding.start(),
            _scope_end(blocks, binding.start(), len(code)),
        )

    constructor = re.compile(
        rf"\blet\s+(?:mut\s+)?(?P<name>[A-Za-z_]\w*)\s*=\s*"
        rf"(?:(?:[A-Za-z_]\w*\s*::\s*)*(?:Arc|Box|Rc)\s*::\s*new\s*\(\s*)*"
        rf"(?:[A-Za-z_]\w*\s*::\s*)*(?P<type>{carrier_names})"
        rf"(?:\s*\{{|\s*::\s*[A-Za-z_]\w*\s*\()"
    )
    for binding in constructor.finditer(code):
        add(
            binding.group("name"),
            binding.group("type"),
            binding.start(),
            _scope_end(blocks, binding.start(), len(code)),
        )

    impl_start = re.compile(
        rf"\bimpl(?:\s*<[^>{{}}]*>)?\s+(?:[^{{;]*?\s+for\s+)?"
        rf"(?:[A-Za-z_]\w*\s*::\s*)*(?P<type>{carrier_names})\b[^{{;]*\{{",
        re.DOTALL,
    )
    for impl in impl_start.finditer(code):
        body_open = code.find("{", impl.start(), impl.end())
        add("self", impl.group("type"), body_open, _balanced_block_end(code, body_open))

    for entries in bindings.values():
        entries.sort(key=lambda entry: entry[0])
    return bindings


def _expression_end(code: str, start: int, fallback: int) -> int:
    """Find a comma/semicolon ending an expression without crossing nesting."""

    pairs = {"(": ")", "{": "}", "[": "]"}
    stack: list[str] = []
    for index in range(start, min(fallback, len(code))):
        char = code[index]
        if char in pairs:
            stack.append(pairs[char])
        elif stack and char == stack[-1]:
            stack.pop()
        elif not stack and char in ",;":
            return index + 1
        elif not stack and char == "}":
            return index
    return fallback


def _generic_credential_destructures(
    code: str,
    blocks: list[tuple[int, int]],
    generic_fields: dict[str, set[str]],
) -> dict[str, list[tuple[int, bool, int]]]:
    """Taint generic secret fields introduced by let/match destructuring."""

    taint: dict[str, list[tuple[int, bool, int]]] = {}
    if not generic_fields:
        return taint
    carrier_names = "|".join(map(re.escape, sorted(generic_fields)))

    def add_fields(type_name: str, fields: str, start: int, end: int) -> None:
        for field in generic_fields.get(type_name, set()):
            binding = re.search(
                rf"\b{re.escape(field)}\b"
                rf"(?:\s*:\s*(?:ref\s+)?(?:mut\s+)?(?P<name>[A-Za-z_]\w*))?",
                fields,
            )
            if binding is None:
                continue
            name = binding.group("name") or field
            taint.setdefault(name, []).append((start, True, end))

    destructure = re.compile(
        rf"\b(?:[A-Za-z_]\w*\s*::\s*)*(?P<type>{carrier_names})\s*"
        r"\{(?P<fields>[^{}]*)\}\s*(?P<tail>=|=>)",
        re.DOTALL,
    )
    for match in destructure.finditer(code):
        if match.group("tail") == "=>":
            body_start = match.end()
            while body_start < len(code) and code[body_start].isspace():
                body_start += 1
            lexical_end = _scope_end(blocks, match.start(), len(code))
            if body_start < len(code) and code[body_start] == "{":
                body_end = _balanced_block_end(code, body_start)
            else:
                body_end = _expression_end(code, body_start, lexical_end)
        else:
            body_start = match.start()
            body_end = _scope_end(blocks, match.start(), len(code))

        add_fields(match.group("type"), match.group("fields"), body_start, body_end)

    parameter_destructure = re.compile(
        rf"\b(?:[A-Za-z_]\w*\s*::\s*)*(?P<type>{carrier_names})\s*"
        r"\{(?P<fields>[^{}]*)\}\s*:\s*"
        rf"[^,|)]*\b(?P=type)\b",
        re.DOTALL,
    )
    function_start = re.compile(r"\bfn\s+[A-Za-z_]\w*(?:\s*<[^>{{}}]*>)?\s*\(")
    for function in function_start.finditer(code):
        open_paren = code.find("(", function.start(), function.end())
        close_paren = _balanced_block_end(code, open_paren)
        tail = re.search(r"[;{]", code[close_paren:])
        if tail is None or tail.group(0) != "{":
            continue
        body_start = close_paren + tail.start()
        body_end = _balanced_block_end(code, body_start)
        params = code[open_paren + 1 : close_paren - 1]
        for match in parameter_destructure.finditer(params):
            add_fields(match.group("type"), match.group("fields"), body_start, body_end)

    closure_start = re.compile(r"\|(?P<params>[^|\n]*)\|")
    for closure in closure_start.finditer(code):
        matches = list(parameter_destructure.finditer(closure.group("params")))
        if not matches:
            continue
        body_start = closure.end()
        while body_start < len(code) and code[body_start].isspace():
            body_start += 1
        lexical_end = _scope_end(blocks, closure.start(), len(code))
        if body_start < len(code) and code[body_start] == "{":
            body_end = _balanced_block_end(code, body_start)
        else:
            body_end = _expression_end(code, body_start, lexical_end)
        for match in matches:
            add_fields(match.group("type"), match.group("fields"), body_start, body_end)

    for entries in taint.values():
        entries.sort(key=lambda entry: entry[0])
    return taint


def _generic_credential_path_type(
    path: list[str],
    position: int,
    bindings: dict[str, list[tuple[int, str, int]]],
    generic_fields: dict[str, set[str]],
) -> str | None:
    """Return the carrier type when a typed path ends in a generic secret field."""

    if len(path) < 2:
        return None
    root, field = path[0], path[-1]
    active = next(
        (
            entry
            for entry in reversed(bindings.get(root, ()))
            if entry[0] <= position < entry[2]
        ),
        None,
    )
    if active is None or field not in generic_fields.get(active[1], set()):
        return None
    return active[1]


def _credential_path_is_tainted(
    path: list[str] | None,
    position: int,
    local_taint: dict[str, list[tuple[int, bool, int]]],
    generic_bindings: dict[str, list[tuple[int, str, int]]],
    generic_fields: dict[str, set[str]],
) -> bool:
    """Classify a value-preserving path using field and local alias taint."""

    if path is None:
        return False
    terminal = path[-1]
    if _is_raw_observability_credential_identifier(terminal):
        return True
    if (
        _generic_credential_path_type(
            path, position, generic_bindings, generic_fields
        )
        is not None
    ):
        return True
    if len(path) != 1:
        return False
    preceding = next(
        (
            entry
            for entry in reversed(local_taint.get(terminal, ()))
            if entry[0] < position < entry[2]
        ),
        None,
    )
    return preceding is not None and preceding[1]


def _option_credential_extractions(
    code: str,
    blocks: list[tuple[int, int]],
    local_taint: dict[str, list[tuple[int, bool, int]]],
    generic_bindings: dict[str, list[tuple[int, str, int]]],
    generic_fields: dict[str, set[str]],
) -> dict[str, list[tuple[int, bool, int]]]:
    """Taint values extracted from credential-bearing Option/Result paths."""

    extracted: dict[str, list[tuple[int, bool, int]]] = {}

    conditional = re.compile(
        r"\b(?P<kind>if|while)?\s*let\s+(?:Some|Ok)\s*\(\s*"
        r"(?:ref\s+)?(?:mut\s+)?(?P<name>[A-Za-z_]\w*)\s*\)\s*=\s*"
        r"(?P<expr>.*?)(?=\s*(?:else\s*\{|[;{]))",
        re.DOTALL,
    )
    for match in conditional.finditer(code):
        path = _value_preserving_path(match.group("expr"))
        if not _credential_path_is_tainted(
            path, match.start(), local_taint, generic_bindings, generic_fields
        ):
            continue
        if match.group("kind"):
            body_open = code.find("{", match.end())
            lexical_end = _scope_end(blocks, match.start(), len(code))
            if body_open < 0 or body_open >= lexical_end:
                continue
            body_start = body_open
            body_end = _balanced_block_end(code, body_open)
        else:
            body_start = match.start()
            body_end = _scope_end(blocks, match.start(), len(code))
        extracted.setdefault(match.group("name"), []).append(
            (body_start, True, body_end)
        )

    match_start = re.compile(r"\bmatch\s+(?P<expr>[^;{]+)\{")
    option_arm = re.compile(
        r"\b(?:Some|Ok)\s*\(\s*(?:ref\s+)?(?:mut\s+)?"
        r"(?P<name>[A-Za-z_]\w*)\s*\)\s*(?:if\b.*?\s*)?=>",
        re.DOTALL,
    )
    for match_expr in match_start.finditer(code):
        path = _value_preserving_path(match_expr.group("expr"))
        if not _credential_path_is_tainted(
            path,
            match_expr.start(),
            local_taint,
            generic_bindings,
            generic_fields,
        ):
            continue
        body_open = code.find("{", match_expr.start(), match_expr.end())
        body_end = _balanced_block_end(code, body_open)
        body = code[body_open + 1 : body_end - 1]
        for arm in option_arm.finditer(body):
            arm_start = body_open + 1 + arm.end()
            while arm_start < body_end and code[arm_start].isspace():
                arm_start += 1
            if arm_start < body_end and code[arm_start] == "{":
                arm_end = _balanced_block_end(code, arm_start)
            else:
                arm_end = _expression_end(code, arm_start, body_end)
            extracted.setdefault(arm.group("name"), []).append(
                (arm_start, True, arm_end)
            )

    for entries in extracted.values():
        entries.sort(key=lambda entry: entry[0])
    return extracted
@dataclass(frozen=True)
class Violation:
    line: int
    message: str


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


def balanced_macro_bodies(source: str):
    """Yield ``(macro_name, body_start, body)`` for balanced Rust macros."""

    macro = re.compile(r"(?P<name>[A-Za-z_][A-Za-z0-9_:]*)!\s*(?P<open>[({\[])")
    pairs = {"(": ")", "{": "}", "[": "]"}
    cursor = 0
    while match := macro.search(source, cursor):
        name = match.group("name").rsplit("::", 1)[-1]
        start = match.end()
        stack = [pairs[match.group("open")]]
        index = start
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
        if not stack:
            yield name, start, source[start : index - 1]
        # Resume just after the current macro opener so nested observability
        # macros (for example inside `tokio::select!`) are scanned too.
        cursor = match.end()


def _line(source: str, offset: int) -> int:
    return source.count("\n", 0, offset) + 1


def _identifier_pattern(identifier: str) -> re.Pattern[str]:
    return re.compile(rf"(?<![A-Za-z0-9_]){re.escape(identifier)}(?![A-Za-z0-9_])")


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


def _without_cfg_test_modules(source: str) -> str:
    """Mask inline test modules so test-only constructors are not production uses."""

    result = list(source)
    code = _code_only(source)
    test_module = re.compile(
        r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*"
        r"(?:#\s*\[[^]]*\]\s*)*mod\s+[A-Za-z_]\w*\s*\{",
        re.DOTALL,
    )
    for match in test_module.finditer(code):
        open_index = code.find("{", match.start(), match.end())
        end = _balanced_block_end(source, open_index)
        for masked in range(match.start(), end):
            if result[masked] != "\n":
                result[masked] = " "
    return "".join(result)


def _balanced_block_end(source: str, open_index: int) -> int:
    """Return the index just after the balanced block starting at ``open_index``."""

    pairs = {"(": ")", "{": "}", "[": "]"}
    stack = [pairs[source[open_index]]]
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


def _is_safe_observability_identifier(identifier: str) -> bool:
    return identifier in SAFE_OBSERVABILITY_IDENTIFIERS or identifier.endswith(
        (
            "_configured",
            "_count",
            "_host",
            "_kind",
            "_len",
            "_present",
            "_scheme",
            "_status",
        )
    )


def _is_presence_only_use(tail: str) -> bool:
    """Allow closed presence/size projections without exposing the underlying value."""

    return bool(
        re.match(
            r"\s*(?:\.\s*get\s*\([^)]*\)\s*)?"
            r"(?:\.\s*(?:as_bytes|as_deref|as_ref|as_str)\s*\(\s*\))*"
            r"\.\s*(?:is_some|is_none|is_empty|len)\s*\(",
            tail,
        )
    )


def _discover_carrier_aliases_and_wrappers(code: str) -> tuple[set[str], set[str]]:
    """Return all carrier-like types and the aliases/wrappers discovered here."""

    carriers = set(SECRET_CARRIERS)
    wrapper_carriers = set(SECRET_CARRIERS)
    discovered: set[str] = set()

    # Discover named structs that directly retain credential-shaped fields.
    # This closes the false-negative gap where every new carrier type had to be
    # manually copied into SECRET_CARRIERS before `derive(Debug)` was rejected.
    struct_start = re.compile(
        r"\bstruct\s+(?P<type>[A-Za-z_]\w*)\b[^;{]*\{",
        re.DOTALL,
    )
    field_name = re.compile(
        r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?P<field>[A-Za-z_]\w*)\s*:"
    )

    for match in struct_start.finditer(code):
        open_index = code.find("{", match.start(), match.end())
        end = _balanced_block_end(code, open_index)
        type_name = match.group("type")
        body = code[open_index + 1 : end - 1]
        if any(
            _is_credential_shaped_identifier(type_name, field.group("field"))
            for field in field_name.finditer(body)
        ):
            if type_name not in carriers:
                carriers.add(type_name)

    changed = True
    while changed:
        changed = False
        carrier_names = "|".join(map(re.escape, sorted(wrapper_carriers)))
        alias = re.compile(
            rf"\btype\s+([A-Za-z_]\w*)\s*=\s*[^;]*\b(?:{carrier_names})\b[^;]*;"
        )
        tuple_wrapper = re.compile(
            rf"\bstruct\s+([A-Za-z_]\w*)\s*\([^;]*\b(?:{carrier_names})\b[^;]*\)\s*;",
            re.DOTALL,
        )
        named_wrapper = re.compile(
            rf"\bstruct\s+((?:Credential|Secret|Token|Auth)[A-Za-z_]\w*|"
            rf"[A-Za-z_]\w*(?:Envelope|Wrapper|Wire|Alias))\s*"
            rf"\{{[^}}]*\b(?:{carrier_names})\b[^}}]*\}}",
            re.DOTALL,
        )
        for pattern in (alias, tuple_wrapper, named_wrapper):
            for match in pattern.finditer(code):
                name = match.group(1)
                if name not in carriers:
                    carriers.add(name)
                    wrapper_carriers.add(name)
                    discovered.add(name)
                    changed = True
    return carriers, discovered


def _scan_observability_body(
    source: str,
    body: str,
    offset: int,
    macro_name: str,
    reqwest_error_names: set[str],
    local_url_taint: dict[str, list[tuple[int, bool, int]]],
    local_credential_taint: dict[str, list[tuple[int, bool, int]]],
    generic_credential_bindings: dict[str, list[tuple[int, str, int]]],
    generic_credential_fields: dict[str, set[str]],
) -> list[Violation]:
    violations: list[Violation] = []
    code_body = _code_only(body)

    for root, entries in generic_credential_bindings.items():
        active = next(
            (entry for entry in reversed(entries) if entry[0] <= offset < entry[2]),
            None,
        )
        if active is None:
            continue
        type_name = active[1]
        for field in generic_credential_fields.get(type_name, set()):
            path = re.compile(rf"\b{re.escape(root)}\s*\.\s*{re.escape(field)}\b")
            for match in path.finditer(code_body):
                if _is_presence_only_use(code_body[match.end() :]):
                    continue
                violations.append(
                    Violation(
                        _line(source, offset + match.start()),
                        f"raw credential field {type_name}.{field} in {macro_name}!",
                    )
                )

    partial = re.search(r"\bredact_middle\s*\(", code_body)
    if partial:
        violations.append(
            Violation(
                _line(source, offset + partial.start()),
                f"partial credential redaction in {macro_name}!",
            )
        )

    raw_acp_serialization = re.search(
        r"\b(?:compact_json|serde_json\s*::\s*to_(?:string|string_pretty|vec|value))"
        r"\s*\(\s*&?\s*(?:args\s*\.\s*)?(?:request|response)\b",
        code_body,
    )
    if raw_acp_serialization:
        violations.append(
            Violation(
                _line(source, offset + raw_acp_serialization.start()),
                f"raw ACP request/response serialized in {macro_name}!",
            )
        )

    # The ACP MCP bridge parses peer-controlled JSON-RPC into a generic `message`
    # value. Its params can contain authorization headers or tool arguments, so
    # logging that value is equivalent to serializing an arbitrary credential
    # carrier. Keep this scoped to the bridge marker to avoid treating unrelated,
    # already-sanitized user-facing message enums as secret carriers.
    raw_acp_json_message = None
    if "AcpReverseInvoker" in source:
        raw_acp_json_message = re.search(r"(?:[%?]\s*|=\s*[%?]?\s*)message\b", code_body)
    if raw_acp_json_message and not _is_presence_only_use(
        code_body[raw_acp_json_message.end() :]
    ):
        violations.append(
            Violation(
                _line(source, offset + raw_acp_json_message.start()),
                f"raw ACP JSON message in {macro_name}!",
            )
        )

    notification_command = re.search(
        r"\b(?:notification_command|notification_cmd|notify_command)\b|"
        r"\bnotification\s*\.\s*command\b",
        code_body,
    )
    notification_is_presence_only = notification_command and _is_presence_only_use(
        code_body[notification_command.end() :]
    )
    if notification_command and not notification_is_presence_only:
        violations.append(
            Violation(
                _line(source, offset + notification_command.start()),
                f"raw notification command in {macro_name}!",
            )
        )

    for match in re.finditer(r"\b[A-Za-z_]\w*\b", code_body):
        identifier = match.group(0)
        if _is_safe_observability_identifier(identifier):
            continue
        if re.match(r"\s*=", code_body[match.end() :]):
            continue
        if _is_presence_only_use(code_body[match.end() :]):
            continue
        if _is_raw_observability_credential_identifier(identifier):
            violations.append(
                Violation(
                    _line(source, offset + match.start()),
                    f"raw credential identifier {identifier} in {macro_name}!",
                )
            )
        if identifier in CONFIGURABLE_URL_IDENTIFIERS:
            violations.append(
                Violation(
                    _line(source, offset + match.start()),
                    f"raw configurable URL {identifier} in {macro_name}!",
                )
            )
        if identifier in GENERIC_LOCATION_IDENTIFIERS and (
            "TracingMiddleware" in source
            or "MakeClientSpan" in source
            or "spans_raw_locations" in source
        ):
            violations.append(
                Violation(
                    _line(source, offset + match.start()),
                    f"raw generic {identifier} in {macro_name}!",
                )
            )
        if (
            identifier in GENERIC_METADATA_PARSE_IDENTIFIERS
            and (
                "impl Injector for MetadataInjector" in source
                or "impl Extractor for HeaderExtractor" in source
            )
        ):
            violations.append(
                Violation(
                    _line(source, offset + match.start()),
                    f"raw metadata/header parse input {identifier} in {macro_name}!",
                )
            )
        if identifier in RESPONSE_CONTENT_IDENTIFIERS:
            violations.append(
                Violation(
                    _line(source, offset + match.start()),
                    f"raw provider response content {identifier} in {macro_name}!",
                )
            )

    for identifier in CONFIGURABLE_URL_IDENTIFIERS | RESPONSE_CONTENT_IDENTIFIERS:
        placeholder = re.search(rf"\{{\s*{re.escape(identifier)}(?:\s*:[^}}]+)?\s*\}}", body)
        if placeholder:
            kind = (
                "raw configurable URL"
                if identifier in CONFIGURABLE_URL_IDENTIFIERS
                else "raw provider response content"
            )
            violations.append(
                Violation(
                    _line(source, offset + placeholder.start()),
                    f"{kind} {identifier} in {macro_name}!",
                )
            )

    for identifier in set(local_credential_taint) | {
        match.group(0)
        for match in re.finditer(r"\b[A-Za-z_]\w*\b", body)
        if _is_raw_observability_credential_identifier(match.group(0))
    }:
        placeholder = re.search(
            rf"\{{\s*{re.escape(identifier)}(?:\s*:[^}}]+)?\s*\}}", body
        )
        assignments = local_credential_taint.get(identifier, ())
        preceding = next(
            (entry for entry in reversed(assignments) if entry[0] < offset < entry[2]),
            None,
        )
        if placeholder and (
            _is_raw_observability_credential_identifier(identifier)
            or (preceding is not None and preceding[1])
        ):
            violations.append(
                Violation(
                    _line(source, offset + placeholder.start()),
                    f"raw credential-derived value {identifier} in {macro_name}!",
                )
            )

    # A generic local can still carry a configurable URL (for example the MCP
    # credential-store `key = format!("{}:{}", server_name, server_url)`). Use
    # the nearest preceding assignment, pre-indexed once per source file, so
    # unrelated locals with common names are neither conflated nor rescanned.
    for match in re.finditer(r"\b[A-Za-z_]\w*\b", code_body):
        identifier = match.group(0)
        if _is_presence_only_use(code_body[match.end() :]):
            continue
        assignments = local_url_taint.get(identifier, ())
        preceding = next(
            (entry for entry in reversed(assignments) if entry[0] < offset < entry[2]),
            None,
        )
        if preceding is None or not preceding[1]:
            continue
        violations.append(
            Violation(
                _line(source, offset + match.start()),
                f"configurable-URL-derived value {identifier} in {macro_name}!",
            )
        )

    for match in re.finditer(r"\b[A-Za-z_]\w*\b", code_body):
        identifier = match.group(0)
        if _is_raw_observability_credential_identifier(identifier):
            continue
        if re.match(r"\s*=", code_body[match.end() :]):
            continue
        if _is_presence_only_use(code_body[match.end() :]):
            continue
        assignments = local_credential_taint.get(identifier, ())
        preceding = next(
            (entry for entry in reversed(assignments) if entry[0] < offset < entry[2]),
            None,
        )
        if preceding is None or not preceding[1]:
            continue
        violations.append(
            Violation(
                _line(source, offset + match.start()),
                f"credential-derived value {identifier} in {macro_name}!",
            )
        )

    raw_error = re.search(
        r"(?:\berror\s*=\s*[%?]\s*|[%?]\s*)([A-Za-z_]\w*)\b",
        code_body,
    )
    if raw_error and raw_error.group(1) in RAW_ERROR_IDENTIFIERS:
        identifier = raw_error.group(1)
        if "without_url" not in code_body and (
            identifier in reqwest_error_names
            or identifier in {"reqwest_error", "request_error", "transport_error"}
        ):
            violations.append(
                Violation(
                    _line(source, offset + raw_error.start()),
                    f"raw request error {identifier} in {macro_name}!",
                )
            )
    return violations


def _scan_custom_carrier_impls(
    source: str, code: str, carrier_types: set[str]
) -> list[Violation]:
    violations: list[Violation] = []
    carrier_names = "|".join(map(re.escape, sorted(carrier_types)))
    impl_start = re.compile(
        rf"\bimpl(?:\s+[A-Za-z_:<>,' ]+)?\s+"
        rf"(?P<trait>(?:std::fmt::)?(?:Debug|Display)|(?:serde::)?Serialize)\s+for\s+"
        rf"(?P<type>{carrier_names})\b[^{{]*\{{"
    )
    unsafe_self = re.compile(
        r"\bself\s*\.\s*(?:0|api_key|authorization|x_api_key|secret|token|credential|credentials|"
        r"[A-Za-z_]\w*_(?:api_key|secret|token|credential|credentials)|"
        r"[A-Za-z_]\w*_(?:deployment_key|alpha_test_key|private_key|service_account_key)|"
        r"[A-Za-z_]\w*_url|url|endpoint|issuer|client_id|"
        r"headers|command|body|message)\b"
    )
    for match in impl_start.finditer(code):
        open_index = code.find("{", match.start(), match.end())
        end = _balanced_block_end(source, open_index)
        body = code[open_index + 1 : end - 1]
        unsafe = None
        for candidate in unsafe_self.finditer(body):
            tail = body[candidate.end() :]
            presence_only = bool(
                re.match(
                    r"\s*(?:\.\s*as_ref\s*\(\s*\)\s*)*"
                    r"\.\s*(?:is_some|is_none|is_empty|len)\s*\(",
                    tail,
                )
                or re.match(
                    r"\s*\.\s*as_ref\s*\(\s*\)\s*"
                    r"\.\s*is_some_and\s*\(\s*\|[A-Za-z_]\w*\|\s*!?\s*"
                    r"[A-Za-z_]\w*\s*\.\s*is_empty\s*\(",
                    tail,
                )
                or re.match(
                    r'''\s*\.\s*as_ref\s*\(\s*\)\s*\.\s*map\s*\(\s*\|_\|\s*["']''',
                    tail,
                )
            )
            if not presence_only:
                unsafe = candidate
                break
        if unsafe is not None:
            trait = match.group("trait").rsplit("::", 1)[-1]
            violations.append(
                Violation(
                    _line(source, open_index + 1 + unsafe.start()),
                    f"secret carrier {match.group('type')} exposes raw data in {trait}",
                )
            )
    return violations


def _reqwest_variants(declaration: str) -> set[str]:
    """Return tuple/struct enum variants that retain a reqwest error."""

    return {
        match.group("variant")
        for match in re.finditer(
            r"\b(?P<variant>[A-Za-z_]\w*)\s*"
            r"(?:\([^)]*\breqwest\s*::\s*Error\b[^)]*\)|"
            r"\{[^}]*\breqwest\s*::\s*Error\b[^}]*\})",
            declaration,
            re.DOTALL,
        )
    }


def _impl_blocks(code: str, pattern: re.Pattern[str]) -> list[tuple[int, int, str]]:
    blocks: list[tuple[int, int, str]] = []
    for match in pattern.finditer(code):
        open_index = code.find("{", match.start(), match.end())
        end = _balanced_block_end(code, open_index)
        blocks.append((match.start(), end, code[open_index + 1 : end - 1]))
    return blocks


def _sanitized_reqwest_variants(source: str) -> set[tuple[str, str]]:
    """Find variants whose sole explicit reqwest conversion strips its URL."""

    if "reqwest::Error" not in source or "without_url" not in source:
        return set()
    code = _code_only(_without_cfg_test_modules(source))
    safe: set[tuple[str, str]] = set()
    error_declaration = re.compile(
        r"#\s*\[\s*derive\s*\((?P<derives>[^]]*)\)\s*\]\s*"
        r"(?:pub(?:\([^)]*\))?\s+)?enum\s+(?P<name>[A-Za-z_]\w*)[^\{]*\{",
        re.DOTALL,
    )
    for declaration_match in error_declaration.finditer(code):
        derives = declaration_match.group("derives")
        if "Debug" not in derives or "Error" not in derives:
            continue
        enum_name = declaration_match.group("name")
        open_index = code.find("{", declaration_match.start(), declaration_match.end())
        declaration_end = _balanced_block_end(code, open_index)
        declaration = code[open_index + 1 : declaration_end - 1]
        variants = _reqwest_variants(declaration)
        if not variants:
            continue

        from_pattern = re.compile(
            rf"\bimpl\s+From\s*<\s*reqwest\s*::\s*Error\s*>\s+for\s+"
            rf"{re.escape(enum_name)}\b[^{{]*\{{"
        )
        conversions = _impl_blocks(code, from_pattern)
        if len(conversions) != 1:
            continue
        _, _, conversion_body = conversions[0]

        for variant in variants:
            direct = re.search(
                rf"\b(?:Self|{re.escape(enum_name)})\s*::\s*{re.escape(variant)}\s*"
                r"\(\s*[A-Za-z_]\w*\s*\.\s*without_url\s*\(\s*\)\s*\)",
                conversion_body,
            )
            if direct:
                safe.add((enum_name, variant))
                continue

            helper_call = re.search(
                r"\bSelf\s*::\s*(?P<helper>[A-Za-z_]\w*)\s*\(\s*[A-Za-z_]\w*\s*\)",
                conversion_body,
            )
            if not helper_call:
                continue
            helper = helper_call.group("helper")
            helper_pattern = re.compile(
                rf"\b(?:pub(?:\([^)]*\))?\s+)?fn\s+{re.escape(helper)}\s*\("
                r"\s*(?P<arg>[A-Za-z_]\w*)\s*:\s*reqwest\s*::\s*Error\s*\)"
                r"\s*->\s*Self\b[^\{]*\{"
            )
            helpers = _impl_blocks(code, helper_pattern)
            if len(helpers) != 1:
                continue
            helper_arg = helper_pattern.search(code, helpers[0][0], helpers[0][1]).group("arg")
            helper_body = helpers[0][2]
            sanitized = re.search(
                rf"\bSelf\s*::\s*{re.escape(variant)}\s*\(\s*"
                rf"{re.escape(helper_arg)}\s*\.\s*without_url\s*\(\s*\)\s*\)",
                helper_body,
            )
            if sanitized:
                safe.add((enum_name, variant))
    return safe


def _has_raw_reqwest_constructor(
    source: str, enum_name: str, variant: str
) -> bool:
    """Check production code for a qualified raw construction of a safe variant."""

    code = _code_only(_without_cfg_test_modules(source))
    pattern = re.compile(
        rf"\b(?:Self|{re.escape(enum_name)})\s*::\s*{re.escape(variant)}\s*\("
    )
    for match in pattern.finditer(code):
        end = _balanced_block_end(code, code.find("(", match.start(), match.end()))
        call = code[match.start() : end]
        if ".without_url" in call:
            continue
        # Pattern matches do not construct a value. Qualified constructors in
        # expressions are forbidden even when an explicit sanitized From impl exists.
        tail = code[end : end + 40]
        statement_start = max(code.rfind(";", 0, match.start()), code.rfind("{", 0, match.start()))
        prefix = code[statement_start + 1 : match.start()]
        if re.match(r"\s*\|", tail) or re.match(r"[\s)]*=>", tail):
            continue
        if re.search(r"\b(?:if|while)\s+let\b", prefix) and re.match(r"\s*=", tail):
            continue
        if re.fullmatch(r"[^()]*(?:_|[A-Za-z_]\w*)[^()]*", call[call.find("(") + 1 : -1]) and (
            "matches!" in prefix or "match " in prefix
        ):
            continue
        return True
    return False


def scan_source(
    source: str,
    externally_raw_reqwest_variants: set[tuple[str, str]] | None = None,
) -> list[Violation]:
    violations: list[Violation] = []
    scan_surface = _without_cfg_test_modules(source)
    code = _code_only(scan_surface)
    carrier_types, carrier_wrappers = _discover_carrier_aliases_and_wrappers(code)
    blocks = _block_ranges(code)
    generic_fields = _generic_credential_fields(code, carrier_types)
    generic_bindings = _generic_credential_bindings(code, blocks, generic_fields)
    reqwest_error_names = set(
        re.findall(
            r"\b([A-Za-z_]\w*)\s*:\s*&?\s*(?:mut\s+)?reqwest\s*::\s*Error\b",
            code,
        )
    )
    has_forbidden_api = any(identifier in scan_surface for identifier in FORBIDDEN_APIS)
    has_fragment = "_credential_" in scan_surface or any(
        identifier in scan_surface for identifier in LEGACY_FRAGMENT_IDENTIFIERS
    )
    has_direct_serde = "serde_json::to_" in scan_surface
    local_url_taint: dict[str, list[tuple[int, bool, int]]] = {}
    local_credential_taint = _generic_credential_destructures(
        code, blocks, generic_fields
    )
    initial_extractions = _option_credential_extractions(
        code,
        blocks,
        local_credential_taint,
        generic_bindings,
        generic_fields,
    )
    for name, entries in initial_extractions.items():
        local_credential_taint.setdefault(name, []).extend(entries)
    assignments = list(
        re.finditer(
        r"\blet\s+(?:mut\s+)?(?P<name>[A-Za-z_]\w*)"
        r"(?:\s*:\s*[^=;\n]+)?\s*=\s*(?P<expr>[^;]+);",
        code,
        re.DOTALL,
        )
    )
    for assignment in assignments:
        expression = assignment.group("expr")
        # Only propagate through value-preserving URL composition. A request
        # function that merely accepts a URL does not make its response a URL
        # string, and treating it as such creates broad false positives.
        url_alias_path = _value_preserving_path(expression)
        tainted = False
        if url_alias_path is not None:
            terminal = url_alias_path[-1]
            tainted = terminal in CONFIGURABLE_URL_IDENTIFIERS
            if not tainted and len(url_alias_path) == 1:
                prior = local_url_taint.get(terminal, ())
                preceding = next(
                    (
                        entry
                        for entry in reversed(prior)
                        if entry[0] < assignment.start() < entry[2]
                    ),
                    None,
                )
                tainted = preceding is not None and preceding[1]

        is_url_composition = bool(
            re.search(
                r"\b(?:format|concat)!\s*\(|\b(?:Self|[A-Za-z_]\w*)::key\s*\(",
                expression,
            )
        )
        if not tainted and is_url_composition:
            tainted = any(
                _identifier_pattern(identifier).search(expression)
                for identifier in CONFIGURABLE_URL_IDENTIFIERS
            )
            if not tainted:
                for identifier, url_assignments in local_url_taint.items():
                    if not _identifier_pattern(identifier).search(expression):
                        continue
                    preceding = next(
                        (
                            entry
                            for entry in reversed(url_assignments)
                            if entry[0] < assignment.start() < entry[2]
                        ),
                        None,
                    )
                    if preceding is not None and preceding[1]:
                        tainted = True
                        break
        local_url_taint.setdefault(assignment.group("name"), []).append(
            (
                assignment.start(),
                tainted,
                _scope_end(blocks, assignment.start(), len(code)),
            )
        )
        alias_path = _value_preserving_path(expression)
        credential_tainted = _credential_path_is_tainted(
            alias_path,
            assignment.start(),
            local_credential_taint,
            generic_bindings,
            generic_fields,
        )
        local_credential_taint.setdefault(assignment.group("name"), []).append(
            (
                assignment.start(),
                credential_tainted,
                _scope_end(blocks, assignment.start(), len(code)),
            )
        )

    late_extractions = _option_credential_extractions(
        code,
        blocks,
        local_credential_taint,
        generic_bindings,
        generic_fields,
    )
    for name, entries in late_extractions.items():
        local_credential_taint.setdefault(name, []).extend(entries)

    # Revisit aliases once after Option/Result pattern bindings are known. This
    # preserves two-hop taint inside an `if let Some(secret)` or match arm.
    for assignment in assignments:
        credential_tainted = _credential_path_is_tainted(
            _value_preserving_path(assignment.group("expr")),
            assignment.start(),
            local_credential_taint,
            generic_bindings,
            generic_fields,
        )
        local_credential_taint.setdefault(assignment.group("name"), []).append(
            (
                assignment.start(),
                credential_tainted,
                _scope_end(blocks, assignment.start(), len(code)),
            )
        )

    if has_forbidden_api:
        for identifier in sorted(FORBIDDEN_APIS):
            for match in _identifier_pattern(identifier).finditer(code):
                violations.append(
                    Violation(_line(source, match.start()), f"forbidden API {identifier}")
                )

    wildcard = re.compile(r"\b[A-Za-z0-9_]*_credential_(?:prefix|suffix)\b")
    for macro_name, offset, body in balanced_macro_bodies(scan_surface):
        is_error_format = macro_name == "format" and re.search(
            r"(?:Err|Error|Failed)\s*\([^()]*(?:$|\n)",
            source[max(0, offset - 200) : offset],
        )
        if macro_name not in OBSERVABILITY_ERROR_MACROS and not is_error_format:
            continue
        violations.extend(
            _scan_observability_body(
                source,
                body,
                offset,
                "error format" if is_error_format else macro_name,
                reqwest_error_names,
                local_url_taint,
                local_credential_taint,
                generic_bindings,
                generic_fields,
            )
        )
        if has_fragment:
            for identifier in sorted(LEGACY_FRAGMENT_IDENTIFIERS):
                match = _identifier_pattern(identifier).search(body)
                if match:
                    violations.append(
                        Violation(
                            _line(source, offset + match.start()),
                            f"legacy credential fragment {identifier} in {macro_name}!",
                        )
                    )
            for match in wildcard.finditer(body):
                violations.append(
                    Violation(
                        _line(source, offset + match.start()),
                        f"legacy credential fragment {match.group(0)} in {macro_name}!",
                    )
                )

    carrier_names = "|".join(map(re.escape, sorted(carrier_types)))
    unsafe_debug = re.compile(
        rf"#\s*\[\s*derive\s*\([^]]*\bDebug\b[^]]*\)\s*\]\s*"
        rf"(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum)\s+({carrier_names})\b",
        re.DOTALL,
    )
    for match in unsafe_debug.finditer(code):
        violations.append(
            Violation(
                _line(source, match.start()),
                f"secret carrier {match.group(1)} derives Debug",
            )
        )

    if carrier_wrappers:
        wrapper_names = "|".join(map(re.escape, sorted(carrier_wrappers)))
        unsafe_wrapper_serde = re.compile(
            rf"#\s*\[\s*derive\s*\([^]]*\bSerialize\b[^]]*\)\s*\]\s*"
            rf"(?:#\s*\[[^]]*\]\s*)*"
            rf"(?:pub(?:\([^)]*\))?\s+)?struct\s+({wrapper_names})\b",
            re.DOTALL,
        )
        for match in unsafe_wrapper_serde.finditer(code):
            violations.append(
                Violation(
                    _line(source, match.start()),
                    f"secret carrier wrapper {match.group(1)} derives Serialize",
                )
            )

    violations.extend(_scan_custom_carrier_impls(source, code, carrier_types))

    sanitized_reqwest_variants = _sanitized_reqwest_variants(source)
    locally_raw_reqwest_variants = {
        pair
        for pair in sanitized_reqwest_variants
        if _has_raw_reqwest_constructor(source, *pair)
    }
    blocked_reqwest_variants = locally_raw_reqwest_variants | (
        externally_raw_reqwest_variants or set()
    )

    error_declaration = re.compile(
        r"#\s*\[\s*derive\s*\((?P<derives>[^]]*)\)\s*\]\s*"
        r"(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum)\s+(?P<name>[A-Za-z_]\w*)[^\{;]*(?P<open>[\{;])",
        re.DOTALL,
    )
    for match in error_declaration.finditer(code):
        derives = match.group("derives")
        if "Debug" not in derives or "Error" not in derives:
            continue
        open_index = match.start("open")
        if match.group("open") == "{":
            end = _balanced_block_end(source, open_index)
        else:
            end = open_index + 1
        declaration = code[match.start() : end]
        declaration_body = (
            code[open_index + 1 : end - 1]
            if match.group("open") == "{"
            else declaration
        )
        reqwest_source = re.search(r"\breqwest\s*::\s*Error\b", declaration)
        reqwest_variants = _reqwest_variants(declaration_body)
        safe_reqwest_variants = {
            variant
            for variant in reqwest_variants
            if (match.group("name"), variant) in sanitized_reqwest_variants
            and (match.group("name"), variant) not in blocked_reqwest_variants
        }
        if reqwest_source and reqwest_variants - safe_reqwest_variants:
            violations.append(
                Violation(
                    _line(source, match.start() + reqwest_source.start()),
                    f"reqwest error source is Debug-visible in {match.group('name')}",
                )
            )

    for attribute in re.finditer(r"#\s*\[\s*error\s*\((?P<body>.*?)\)\s*\]", source, re.DOTALL):
        body = attribute.group("body")
        for identifier in {
            match.group(0)
            for match in re.finditer(r"\b[A-Za-z_]\w*\b", body)
            if _is_raw_observability_credential_identifier(match.group(0))
        }:
            if re.search(rf"\{{\s*{re.escape(identifier)}(?:\s*:[^}}]+)?\s*\}}", body):
                violations.append(
                    Violation(
                        _line(source, attribute.start()),
                        f"raw credential identifier {identifier} in error Display",
                    )
                )
        for identifier in CONFIGURABLE_URL_IDENTIFIERS | RESPONSE_CONTENT_IDENTIFIERS:
            if re.search(rf"\{{\s*{re.escape(identifier)}(?:\s*:[^}}]+)?\s*\}}", body):
                kind = (
                    "raw configurable URL"
                    if identifier in CONFIGURABLE_URL_IDENTIFIERS
                    else "raw provider response content"
                )
                violations.append(
                    Violation(
                        _line(source, attribute.start()),
                        f"{kind} {identifier} in error Display",
                    )
                )

    direct_serde = re.compile(
        r"serde_json::to_(?:string|string_pretty|vec|value)\s*\([^)]*\b"
        r"(?:auth|credential_snapshot|sampler_config|upload_method|trace_export_config|token_response|refresh_outcome)\b",
        re.DOTALL,
    )
    if has_direct_serde:
        for match in direct_serde.finditer(code):
            violations.append(
                Violation(_line(source, match.start()), "secret carrier serialized directly")
            )

    return sorted(set(violations), key=lambda item: (item.line, item.message))


def production_rust_files(root: Path = ROOT):
    for source_root in PRODUCTION_RUST_ROOTS:
        directory = root / source_root
        if not directory.is_dir():
            continue
        for path in sorted(directory.rglob("*.rs")):
            relative = path.relative_to(root)
            excluded_test_support = any(
                relative == excluded or excluded in relative.parents
                for excluded in NON_PRODUCTION_CRATE_ROOTS
            )
            if (
                "target" not in relative.parts
                and "tests" not in relative.parts
                and not excluded_test_support
            ):
                yield path


def repository_violations(root: Path = ROOT):
    files = [(path, path.read_text(encoding="utf-8")) for path in production_rust_files(root)]
    sanitized = set().union(*(_sanitized_reqwest_variants(source) for _, source in files))
    externally_raw = {
        pair
        for pair in sanitized
        if any(
            (
                f"{pair[0]}::{pair[1]}" in source
                or (f"Self::{pair[1]}" in source and f"enum {pair[0]}" in source)
            )
            and _has_raw_reqwest_constructor(source, *pair)
            for _, source in files
        )
    }
    for path, source in files:
        for violation in scan_source(source, externally_raw):
            yield path.relative_to(root), violation


class CredentialObservabilityGuardTests(unittest.TestCase):
    def test_injected_observability_and_key_derived_id_violations_fail(self):
        source = (FIXTURES / "invalid.rs").read_text(encoding="utf-8")
        messages = [violation.message for violation in scan_source(source)]
        self.assertTrue(any("auth_prefix" in message for message in messages), messages)
        self.assertTrue(any("deployment_id_from_key" in message for message in messages), messages)

    def test_non_observability_object_storage_key_prefix_is_allowed(self):
        source = (FIXTURES / "object_storage_allowed.rs").read_text(encoding="utf-8")
        self.assertEqual(scan_source(source), [])

    def test_credential_adjacent_observability_sinks_are_rejected(self):
        source = (FIXTURES / "unsafe_sinks.rs").read_text(encoding="utf-8")
        messages = [violation.message for violation in scan_source(source)]
        expected_fragments = [
            "raw notification command",
            "partial credential redaction",
            "raw configurable URL proxy_base_url",
            "raw request error err",
            "raw provider response content response_body",
            "raw provider response content body_preview",
            "raw ACP request/response serialized",
            "reqwest error source is Debug-visible",
            "raw configurable URL config_url in error Display",
            "raw provider response content response_body in error Display",
        ]
        for expected in expected_fragments:
            self.assertTrue(
                any(expected in message for message in messages),
                f"missing {expected!r} in {messages}",
            )

    def test_configurable_url_derived_composite_key_is_rejected(self):
        source = (FIXTURES / "unsafe_url_derived_key.rs").read_text(encoding="utf-8")
        messages = [violation.message for violation in scan_source(source)]
        self.assertTrue(
            any("configurable-URL-derived value key" in message for message in messages),
            messages,
        )

    def test_generic_transport_and_parse_inputs_are_rejected(self):
        source = (FIXTURES / "unsafe_transport_observability.rs").read_text(
            encoding="utf-8"
        )
        messages = [violation.message for violation in scan_source(source)]
        for expected in (
            "raw generic url",
            "raw generic uri",
            "raw metadata/header parse input key",
            "raw metadata/header parse input value",
            "raw metadata/header parse input v",
            "raw metadata/header parse input error",
        ):
            self.assertTrue(
                any(expected in message for message in messages),
                f"missing {expected!r} in {messages}",
            )

    def test_acp_bridge_raw_json_message_is_rejected(self):
        source = '''
            trait AcpReverseInvoker {}
            fn observe(message: serde_json::Value) {
                tracing::debug!(%message, "discarding notification");
            }
        '''
        messages = [violation.message for violation in scan_source(source)]
        self.assertTrue(any("raw ACP JSON message" in message for message in messages), messages)

    def test_custom_carrier_alias_wrapper_and_serde_exposure_is_rejected(self):
        source = (FIXTURES / "unsafe_wrappers.rs").read_text(encoding="utf-8")
        messages = [violation.message for violation in scan_source(source)]
        self.assertTrue(any("CredentialEnvelope exposes raw data in Display" in m for m in messages))
        self.assertTrue(any("CredentialAlias exposes raw data in Debug" in m for m in messages))
        self.assertTrue(any("CredentialWire derives Serialize" in m for m in messages))

    def test_presence_count_sanitized_url_and_without_url_are_allowed(self):
        source = (FIXTURES / "safe_sinks.rs").read_text(encoding="utf-8")
        self.assertEqual(scan_source(source), [])

    def test_direct_field_and_simple_alias_credential_sinks_are_rejected(self):
        source = (FIXTURES / "unsafe_credential_taint.rs").read_text(encoding="utf-8")
        messages = [violation.message for violation in scan_source(source)]
        for expected in (
            "raw credential identifier api_key",
            "raw credential identifier access_token",
            "raw credential identifier alpha_test_key",
            "raw credential identifier authorization",
            "raw credential identifier x_api_key",
            "raw credential identifier management_api_key",
            "raw credential identifier events_api_key",
            "raw credential identifier mixpanel_token",
            "raw credential identifier jwt_token",
            "credential-derived value value",
            "raw credential-derived value field_read",
            "configurable-URL-derived value endpoint",
            "raw configurable URL cli_chat_proxy_base_url",
            "raw configurable URL xai_api_base_url",
            "raw configurable URL hub_url",
            "raw configurable URL npm_registry",
            "raw configurable URL grok_ws_origin",
            "raw credential identifier client_secret in error Display",
        ):
            self.assertTrue(
                any(expected in message for message in messages),
                f"missing {expected!r} in {messages}",
            )

    def test_typed_generic_credential_fields_and_aliases_are_rejected(self):
        source = (FIXTURES / "unsafe_generic_credential_fields.rs").read_text(
            encoding="utf-8"
        )
        messages = [violation.message for violation in scan_source(source)]
        for expected in (
            "raw credential field GrokAuth.key",
            "raw credential field CredentialSnapshot.token",
            "credential-derived value auth_value",
            "credential-derived value snapshot_value",
            "credential-derived value key",
            "credential-derived value token",
            "credential-derived value unwrapped",
            "credential-derived value expected",
            "credential-derived value defaulted",
            "credential-derived value fallback",
            "credential-derived value copied",
            "credential-derived value matched",
            "credential-derived value let_else_token",
            "credential-derived value ok_token",
            "credential-derived value guarded_token",
            "credential-derived value question_token",
        ):
            self.assertTrue(
                any(expected in message for message in messages),
                f"missing {expected!r} in {messages}",
            )
        self.assertGreaterEqual(
            sum("raw credential field GrokAuth.key" in message for message in messages),
            4,
            messages,
        )
        self.assertGreaterEqual(
            sum(
                "raw credential field CredentialSnapshot.token" in message
                for message in messages
            ),
            4,
            messages,
        )
        self.assertGreaterEqual(
            sum("credential-derived value key" in message for message in messages),
            3,
            messages,
        )
        self.assertGreaterEqual(
            sum("credential-derived value token" in message for message in messages),
            4,
            messages,
        )

    def test_safe_url_names_do_not_clear_configurable_url_taint(self):
        source = '''
            fn renamed_raw_urls(config: Config) {
                let safe_url = config.xai_api_base_url;
                tracing::warn!(%safe_url, "renamed raw API URL");

                let log_url = &config.hub_url;
                tracing::warn!(%log_url, "renamed raw hub URL");

                let sanitized_url = config.npm_registry.clone();
                tracing::warn!(%sanitized_url, "renamed raw registry URL");

                let first_alias = config.grok_ws_url.as_str();
                let redacted_url = first_alias;
                tracing::warn!(%redacted_url, "renamed raw websocket URL");
            }
        '''
        messages = [violation.message for violation in scan_source(source)]
        for identifier in ("safe_url", "log_url", "sanitized_url", "redacted_url"):
            self.assertTrue(
                any(
                    f"configurable-URL-derived value {identifier}" in message
                    for message in messages
                ),
                f"renaming a raw URL to {identifier} bypassed the guard: {messages}",
            )

    def test_presence_only_credential_projections_are_allowed(self):
        source = (FIXTURES / "safe_credential_presence.rs").read_text(encoding="utf-8")
        self.assertEqual(scan_source(source), [])

    def test_alias_taint_does_not_cross_a_safe_terminal_field(self):
        source = '''
            fn safe(config: Config) {
                let auth = config.access_token;
                let expires_at = auth.expires_at;
                tracing::info!(?expires_at, "credential expiry metadata");
            }
        '''
        self.assertEqual(scan_source(source), [])

    def test_clean_reassignment_clears_alias_taint(self):
        source = '''
            fn safe(config: Config, status: Status) {
                let value = config.access_token;
                let value = status;
                tracing::info!(?value, "safe status");
            }
        '''
        self.assertEqual(scan_source(source), [])

    def test_alias_taint_does_not_cross_function_boundaries(self):
        source = '''
            fn retain(config: Config) {
                let value = config.access_token;
                consume(value);
            }

            fn observe(value: Status) {
                tracing::info!(?value, "safe status");
            }
        '''
        self.assertEqual(scan_source(source), [])

    def test_inline_cfg_test_carriers_are_not_production_findings(self):
        source = '''
            #[cfg(test)]
            mod tests {
                #[derive(Debug)]
                struct CapturedHeaders {
                    authorization: String,
                }
            }
        '''
        self.assertEqual(scan_source(source), [])

    def test_reqwest_source_is_allowed_only_with_sanitized_conversion(self):
        source = (FIXTURES / "safe_reqwest_wrapper.rs").read_text(encoding="utf-8")
        self.assertEqual(scan_source(source), [])

    def test_derived_and_raw_constructor_reqwest_sources_are_rejected(self):
        source = (FIXTURES / "unsafe_reqwest_wrapper.rs").read_text(encoding="utf-8")
        messages = [violation.message for violation in scan_source(source)]
        self.assertEqual(
            messages,
            [
                "reqwest error source is Debug-visible in DerivedTransportError",
                "reqwest error source is Debug-visible in RawConstructorError",
            ],
        )

    def test_balanced_macro_scanner_checks_nested_multiline_fields(self):
        source = '''
            tracing::warn!(
                detail = format!("{}", nested(call(1, 2))),
                refresh_credential_suffix = "unsafe",
                "failed"
            );
        '''
        messages = [violation.message for violation in scan_source(source)]
        self.assertTrue(any("refresh_credential_suffix" in message for message in messages))

    def test_api_key_suffix_helper_is_forbidden_even_through_function_log_sinks(self):
        source = '''
            fn key_suffix(key: &str) -> &str {
                &key[key.len().saturating_sub(12)..]
            }

            fn unsafe_probe_log(key: &str) {
                xai_grok_telemetry::unified_log::info(
                    "probe",
                    None,
                    Some(serde_json::json!({"key_suffix": key_suffix(key)})),
                );
            }
        '''
        messages = [violation.message for violation in scan_source(source)]
        self.assertTrue(any("forbidden API key_suffix" in message for message in messages))

    def test_derived_debug_secret_carriers_report_violations_without_crashing(self):
        source = (FIXTURES / "unsafe_debug_carriers.rs").read_text(encoding="utf-8")
        messages = [violation.message for violation in scan_source(source)]
        self.assertEqual(
            messages,
            [
                "secret carrier SamplerConfig derives Debug",
                "secret carrier SamplingConfig derives Debug",
                "secret carrier GrokComConfig derives Debug",
                "secret carrier ModelEntryConfig derives Debug",
                "secret carrier ModelsConfig derives Debug",
                "secret carrier RemoteConfig derives Debug",
                "secret carrier ModelInfo derives Debug",
                "secret carrier ModelEntry derives Debug",
                "secret carrier OtelExporterConfig derives Debug",
                "secret carrier ManagedMcpConfig derives Debug",
                "secret carrier MultipartInitResponse derives Debug",
                "secret carrier SignedPartUrl derives Debug",
                "secret carrier ExternalOtelConfig derives Debug",
                "secret carrier ExternalOtelFileConfig derives Debug",
                "secret carrier TelemetryConfig derives Debug",
                "secret carrier McpOAuthConfig derives Debug",
                "secret carrier ServeArgs derives Debug",
                "secret carrier DeploymentConfig derives Debug",
                "secret carrier AlphaTestConfig derives Debug",
                "secret carrier ServiceAccountConfig derives Debug",
                "secret carrier PrivateKeyConfig derives Debug",
                "secret carrier AuthorizationConfig derives Debug",
            ],
        )

    def test_production_roots_include_nested_prod_and_exclude_tests_and_targets(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            included = root / "prod" / "nested" / "src" / "carrier.rs"
            excluded_test = root / "prod" / "nested" / "tests" / "carrier.rs"
            excluded_target = root / "prod" / "target" / "generated.rs"
            crate_file = root / "crates" / "example" / "src" / "lib.rs"
            third_party_file = root / "third_party" / "vendored" / "src" / "lib.rs"
            test_support_file = (
                root
                / "crates"
                / "codegen"
                / "xai-grok-test-support"
                / "src"
                / "lib.rs"
            )
            for path in [
                included,
                excluded_test,
                excluded_target,
                crate_file,
                third_party_file,
                test_support_file,
            ]:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("struct Example;\n", encoding="utf-8")

            found = {path.relative_to(root) for path in production_rust_files(root)}
            self.assertEqual(
                found,
                {
                    Path("crates/example/src/lib.rs"),
                    Path("prod/nested/src/carrier.rs"),
                },
            )

    def test_repository_has_no_credential_observability_regressions(self):
        found = list(repository_violations())
        self.assertEqual(
            found,
            [],
            "\n" + "\n".join(f"{path}:{item.line}: {item.message}" for path, item in found),
        )


def main() -> int:
    found = list(repository_violations())
    for path, violation in found:
        print(f"{path}:{violation.line}: {violation.message}")
    if found:
        print(f"credential observability guard failed with {len(found)} violation(s)", file=sys.stderr)
        return 1
    print("credential observability guard passed")
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--scan":
        raise SystemExit(main())
    unittest.main()
