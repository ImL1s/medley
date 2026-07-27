# Grok-build audit and issue backlog — 2026-07-28

Repository: `ImL1s/grok-build`  
Shipping branch: `providers`  
Audited commit: `228fc8120fe55f263248e324dbd30b00f26c3b1c`  
Upstream point checked: `02d9359435d0e9c20a20945679389cdce441e431`

## Status

The repository had GitHub Issues disabled when the first audited issue was submitted. GitHub returned HTTP 410, so **no issue was created**.

This branch contains:

- `.github/audit/2026-07-28-issues.json.gz.b64.part-01` through `part-04` — split canonical manifest with all 28 complete issue bodies, label definitions, implementation plans, acceptance criteria, tests, and guardrails.
- `scripts/publish-audit-issues-from-parts.py` — verifies and decodes the split manifest.
- `scripts/publish-audit-issues.py` — dependency-free, exact-title-deduplicating core publisher.
- this review summary.

Review or export the full Markdown backlog:

```bash
python3 scripts/publish-audit-issues-from-parts.py \
  --dump-markdown /tmp/grok-build-issues.md \
  --skip-labels
```

Enable Issues, create/update the label taxonomy, and publish all missing issues:

```bash
python3 scripts/publish-audit-issues-from-parts.py \
  --repo ImL1s/grok-build \
  --enable-issues \
  --apply
```

The wrapper validates contiguous parts plus base64/gzip integrity before invoking the core publisher. The publisher is dry-run by default and requires an administrator-authenticated `gh` CLI only when `--apply` is used. Exact-title matching makes reruns safe; existing issues are skipped unless `--update-existing` is supplied.

## Result

- Total: **28**
- P0: **3**
- P1: **19**
- P2: **6**
- Bugs: **11**
- Enhancements: **15**
- Documentation: **2**

## Highest-risk confirmed findings

### 1. xAI session token can cross into a custom `/models` endpoint

`remote/client.rs` falls back from `XAI_API_KEY` to the current auth/session key for custom model-catalog discovery. A third-party catalog can therefore receive a first-party xAI session credential.

Tracked as **GB-001 (P0)**.

### 2. Credential fragments are written to logs/telemetry

Known paths include sampler request logging, subagent parent/child credential diagnostics, and agent auth initialization/disk-refresh telemetry. Prefix/suffix logging is not acceptable for production credentials.

Tracked as **GB-002 (P0)**.

### 3. Restricted subagent capability modes keep unclassified MCP/custom tools

`SubagentCapabilityModeExt::filter_tool_config` retains `ToolConfig.kind == None`. An unclassified MCP/custom tool can therefore bypass a named read-only/execute restriction.

Tracked as **GB-004 (P0)**.

### 4. External providers receive unnecessary internal request identity

Conversation/session/agent/model-override identifiers and trace context can be sent to non-xAI endpoints even though those providers do not require them.

Tracked as **GB-003 (P1)**.

### 5. `grok agent serve` remote exposure is weak by default

The generated key is a short UUID-derived substring, the startup URL contains the secret in the query string, and non-loopback plaintext WebSocket exposure needs stronger gating.

Tracked as **GB-005 (P1)**.

### 6. Model switching can proceed with a stale incompatible harness

A zero-turn switch warns and continues when the required `AgentDefinition` is unavailable. The active model can therefore use the wrong system prompt/toolset.

Tracked as **GB-006 (P1)**.

### 7. Duplicate wire slugs can select the wrong context/compaction metadata

The fork distinguishes catalog ID from provider wire slug, but the switch path still performs a compaction lookup by wire slug.

Tracked as **GB-007 (P1)**.

### 8. Default model persistence is committed before the switch succeeds

A rejected/cancelled switch can leave the active session restored while the on-disk default points at the rejected model.

Tracked as **GB-008 (P1)**.

### 9. Long-running workflow interruption is terminal

The workflow view treats `interrupted` as terminal and excludes it from resumable states. This directly affects quota/provider/process interruption recovery.

Tracked as **GB-017 (P1)**.

### 10. Fork maintenance gates are too narrow

The fork was five upstream commits behind at the audit point, while CI ran only on Ubuntu and used a small set of name-filtered provider tests. Upstream changes overlap the fork's custom hotspots.

Tracked as **GB-021 through GB-024**.

## Recommended execution order

1. **Credential and permission boundary:** GB-001, GB-002, GB-004.
2. **External privacy and remote server hardening:** GB-003, GB-005.
3. **Model correctness:** GB-006 through GB-010.
4. **Provider/subagent operability:** GB-011 through GB-016.
5. **Long-run continuity:** GB-017 through GB-020.
6. **Sync, CI, docs, and releases:** GB-021 through GB-028.

Do not let upstream sync postpone active P0 fixes. Either land the fixes first and preserve them during sync, or implement them directly on the reviewed sync branch.

## Issue index

| ID | Title | Labels |
|---|---|---|
| GB-001 | [P0][security] Never send xAI session credentials to custom model-catalog endpoints | `priority:p0`, `type:bug`, `area:security`, `area:providers`, `effort:m` |
| GB-002 | [P0][security] Remove credential fragments from logs and telemetry | `priority:p0`, `type:bug`, `area:security`, `effort:m` |
| GB-003 | [P1][privacy] Apply the first-party metadata boundary to all non-xAI requests | `priority:p1`, `type:bug`, `area:privacy`, `area:providers`, `effort:l` |
| GB-004 | [P0][security] Fail closed for unclassified MCP/custom tools in restricted subagent modes | `priority:p0`, `type:bug`, `area:security`, `area:subagents`, `effort:l` |
| GB-005 | [P1][security] Harden `grok agent serve` secrets and non-loopback exposure | `priority:p1`, `type:bug`, `area:security`, `area:ops`, `effort:m` |
| GB-006 | [P1][bug] Fail closed when a model requires an unavailable agent harness | `priority:p1`, `type:bug`, `area:models`, `effort:m` |
| GB-007 | [P1][bug] Resolve compaction and context settings by catalog ID, not wire slug | `priority:p1`, `type:bug`, `area:models`, `effort:m` |
| GB-008 | [P1][bug] Persist the default model only after a successful switch | `priority:p1`, `type:bug`, `area:models`, `effort:m` |
| GB-009 | [P1][providers] Support explicit auth schemes for custom model-catalog discovery | `priority:p1`, `type:enhancement`, `area:providers`, `area:models`, `effort:l` |
| GB-010 | [P2][config] Normalize and reject whitespace-only `env_key` entries | `priority:p2`, `type:bug`, `area:providers`, `effort:s`, `good first issue` |
| GB-011 | [P1][subagents] Refresh MCP, skills, and tool capabilities at child spawn time | `priority:p1`, `type:bug`, `area:subagents`, `effort:l` |
| GB-012 | [P1][UX] Add provider diagnostics to `/doctor` and `grok doctor --json` | `priority:p1`, `type:enhancement`, `area:providers`, `area:ops`, `effort:l` |
| GB-013 | [P1][UX] Add safe interactive provider onboarding and config editing | `priority:p1`, `type:enhancement`, `area:providers`, `effort:xl` |
| GB-014 | [P1][UX] Make the model picker provider-aware and actionable | `priority:p1`, `type:enhancement`, `area:models`, `area:providers`, `effort:l` |
| GB-015 | [P1][resilience] Add ordered model/provider failover without widening permissions | `priority:p1`, `type:enhancement`, `area:providers`, `area:models`, `effort:xl` |
| GB-016 | [P1][routing] Add capability-aware subagent model routing and loss reports | `priority:p1`, `type:enhancement`, `area:subagents`, `area:models`, `effort:xl` |
| GB-017 | [P1][workflow] Persist checkpoints and resume interrupted workflow runs | `priority:p1`, `type:enhancement`, `area:workflow`, `area:resume`, `effort:xl` |
| GB-018 | [P1][resume] Add portable cross-agent handoff bundles separate from session restore | `priority:p1`, `type:enhancement`, `area:resume`, `effort:l` |
| GB-019 | [P2][startup] Add a composable bare startup profile | `priority:p2`, `type:enhancement`, `area:startup`, `effort:l` |
| GB-020 | [P2][usage] Report usage per provider/model and distinguish unknown quota | `priority:p2`, `type:enhancement`, `area:usage`, `area:providers`, `effort:l` |
| GB-021 | [P1][maintenance] Sync upstream through `02d9359` and preserve fork invariants | `priority:p1`, `type:enhancement`, `area:maintenance`, `effort:l` |
| GB-022 | [P1][CI] Add a complete multi-provider regression gate | `priority:p1`, `type:enhancement`, `area:ci`, `area:providers`, `effort:xl` |
| GB-023 | [P1][CI] Add macOS and Windows provider smoke coverage | `priority:p1`, `type:enhancement`, `area:ci`, `area:providers`, `effort:l` |
| GB-024 | [P1][ops] Make upstream sync atomic, resumable, and repository-safe | `priority:p1`, `type:bug`, `area:maintenance`, `area:ops`, `effort:l` |
| GB-025 | [P1][docs] Replace upstream installer guidance with fork-specific instructions | `priority:p1`, `type:docs`, `area:docs`, `area:release`, `effort:m` |
| GB-026 | [P2][release] Add reproducible fork releases with checksums and provenance | `priority:p2`, `type:enhancement`, `area:release`, `area:supply-chain`, `effort:xl` |
| GB-027 | [P2][security] Pin CI actions and add dependency/supply-chain checks | `priority:p2`, `type:enhancement`, `area:supply-chain`, `area:ci`, `effort:l` |
| GB-028 | [P2][docs] Validate documented TOML and provider examples in CI | `priority:p2`, `type:docs`, `area:docs`, `area:ci`, `effort:m`, `good first issue` |

## Manifest guarantees

Each issue body contains:

- current behavior and why it matters;
- affected files/functions;
- expected behavior;
- ordered implementation plan;
- acceptance checklist;
- deterministic tests;
- guardrails/non-goals;
- dependencies where relevant.

The publisher also:

- validates schema, unique IDs/titles, and label references;
- creates/updates the full label taxonomy;
- checks repository Issue state;
- skips exact-title duplicates;
- can publish a selected `GB-NNN`;
- can update existing issue bodies only when explicitly requested;
- remains dry-run unless `--apply` is supplied.
