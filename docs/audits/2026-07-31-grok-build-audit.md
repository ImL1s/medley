# Grok-build audit and issue backlog — 2026-07-31

- Repository: `ImL1s/grok-build`
- Shipping branch: `providers`
- Audited commit: `eafa57c43bfc1d0d8516ffceee5371c45759eb8f`
- Upstream point checked: `dd04f397b1d02f2272b092555669dfba1f01bc85`

## Status

The 28-entry backlog was refreshed against the current `providers` base on 2026-07-31. GitHub Issues is enabled. The repository's existing [Codex OAuth issue #3](https://github.com/ImL1s/grok-build/issues/3) has no audit marker, does not share a manifest title, and is intentionally outside the `GB-001` through `GB-028` namespace.

This branch contains:

- `.github/audit/2026-07-31-issues.json` — the reviewable canonical manifest with 26 label definitions and 28 issue bodies;
- `scripts/publish-audit-issues.py` — the dependency-free validator and stable-marker publisher;
- `tests/test_audit_issue_publishers.py` — offline regression coverage for validation, non-mutation, identity, retries, and GitHub command construction;
- this review summary.

Validate the canonical source without contacting GitHub:

```bash
python3 scripts/publish-audit-issues.py --validate-only
```

Review or export the full rendered Markdown backlog:

```bash
python3 scripts/publish-audit-issues.py \
  --dump-markdown /tmp/grok-build-issues.md \
  --skip-labels
```

Preview the complete publication plan (the default is a network-free dry run):

```bash
python3 scripts/publish-audit-issues.py --repo ImL1s/grok-build
```

After review, create/update the label taxonomy and publish missing issues:

```bash
python3 scripts/publish-audit-issues.py \
  --repo ImL1s/grok-build \
  --apply
```

Every managed body receives a visible source footer plus a hidden `grok-build-audit-id` marker. Reruns match that stable ID rather than mutable titles. An unmarked exact-title collision fails closed instead of being adopted. Existing marker-managed issues are skipped unless `--update-existing` is supplied.

`--update-existing` converges the managed issue to the manifest's exact title, body, and label set; labels not present in the manifest are removed. Use it only when that replacement behavior is intended.

GitHub mutations cannot be transactional: the publisher validates the entire local manifest and remote identity plan first, then fails on the first remote error. A corrected rerun converges through stable markers without duplicating successfully created issues.

## Result

- Total: **28**
- P0: **3**
- P1: **19**
- P2: **6**
- Bugs: **11**
- Enhancements: **15**
- Documentation: **2**

## Refresh evidence

All 28 entries were rechecked for continuing applicability against `providers@eafa57c`. Representative direct evidence includes:

- `crates/codegen/xai-grok-shell/src/remote/client.rs`: custom model-catalog auth still reaches the first-party fallback path (GB-001);
- `crates/codegen/xai-grok-sampler/src/client.rs`: credential-derived prefix diagnostics and incomplete external metadata stripping remain (GB-002, GB-003);
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/types.rs`: unclassified restricted tools still default to retained (GB-004);
- `crates/codegen/xai-grok-pager/src/app/cli.rs`: the remote-agent secret remains a short UUID-derived value (GB-005);
- `.github/workflows/ci.yml`: product CI is still Ubuntu-only and uses floating action references (GB-023, GB-027);
- `scripts/sync-upstream.sh` and `README.md`: the atomic-sync and fork-installation gaps remain (GB-024, GB-025).

The upstream-sync entry was rewritten rather than carried forward unchanged: `providers` already contains upstream `dd04f39`, while the `main` mirror and durable fork-invariant certification remain unresolved (GB-021).

## Highest-risk confirmed findings

### 1. xAI session token can cross into a custom `/models` endpoint

Custom model discovery can fall back from `XAI_API_KEY` to the active first-party session key and attach it to a configured third-party catalog. Tracked as **GB-001 (P0)**.

### 2. Credential fragments are written to logs/telemetry

Known sampler and agent/subagent diagnostics still derive token prefixes or suffixes. Production credentials must be wholly absent from observability payloads. Tracked as **GB-002 (P0)**.

### 3. Restricted subagent modes retain unclassified tools

`SubagentCapabilityModeExt::filter_tool_config` keeps tools whose kind is unclassified. That can bypass a named restricted capability mode. Tracked as **GB-004 (P0)**.

### 4. External providers receive unnecessary internal request identity

Conversation, session, agent, model-override, and trace metadata are not uniformly removed at the external-provider boundary. Tracked as **GB-003 (P1)**.

### 5. Remote agent exposure needs stronger defaults

The generated server key is short and URL-borne, while non-loopback plaintext WebSocket exposure needs explicit hardening. Tracked as **GB-005 (P1)**.

### 6. Model switching has correctness gaps

Unavailable harnesses, catalog-ID versus wire-slug lookup, and persistence-before-success can leave a session on mismatched model behavior. Tracked as **GB-006 through GB-008 (P1)**.

### 7. Long-running continuity is incomplete

Interrupted workflow runs are not a durable resumable state, and portable cross-agent handoff remains separate unfinished work. Tracked as **GB-017 and GB-018 (P1)**.

### 8. Maintenance and release gates are incomplete

The current upstream merge still lacks a reconciled mirror/certification record; CI is narrow; the sync script, fork installation docs, release provenance, and supply-chain checks remain incomplete. Tracked as **GB-021 through GB-028**.

## Recommended execution order

1. **Credential and permission boundary:** GB-001, GB-002, GB-004.
2. **External privacy and remote server hardening:** GB-003, GB-005.
3. **Model correctness:** GB-006 through GB-010.
4. **Provider/subagent operability:** GB-011 through GB-016.
5. **Long-run continuity:** GB-017 through GB-020.
6. **Sync, CI, docs, and releases:** GB-021 through GB-028.

Do not let maintenance work postpone active P0 fixes. Preserve each provider/security invariant across later upstream syncs.

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
| GB-021 | [P1][maintenance] Reconcile the upstream mirror and certify fork invariants after sync | `priority:p1`, `type:enhancement`, `area:maintenance`, `effort:l` |
| GB-022 | [P1][CI] Add a complete multi-provider regression gate | `priority:p1`, `type:enhancement`, `area:ci`, `area:providers`, `effort:xl` |
| GB-023 | [P1][CI] Add macOS and Windows provider smoke coverage | `priority:p1`, `type:enhancement`, `area:ci`, `area:providers`, `effort:l` |
| GB-024 | [P1][ops] Make upstream sync atomic, resumable, and repository-safe | `priority:p1`, `type:bug`, `area:maintenance`, `area:ops`, `effort:l` |
| GB-025 | [P1][docs] Replace upstream installer guidance with fork-specific instructions | `priority:p1`, `type:docs`, `area:docs`, `area:release`, `effort:m` |
| GB-026 | [P2][release] Add reproducible fork releases with checksums and provenance | `priority:p2`, `type:enhancement`, `area:release`, `area:supply-chain`, `effort:xl` |
| GB-027 | [P2][security] Pin CI actions and add dependency/supply-chain checks | `priority:p2`, `type:enhancement`, `area:supply-chain`, `area:ci`, `effort:l` |
| GB-028 | [P2][docs] Validate documented TOML and provider examples in CI | `priority:p2`, `type:docs`, `area:docs`, `area:ci`, `effort:m`, `good first issue` |

## Manifest and publisher guarantees

The manifest provides a prioritized implementation specification for each backlog item. Bodies contain the relevant combination of current behavior, affected code or scope, implementation steps, acceptance checks, validation, and guardrails. Some capability-gap issues intentionally describe a subsystem scope rather than pretending a missing implementation already has a concrete file path.

Before any GitHub mutation, the publisher validates:

- schema version and required audit metadata;
- exact `OWNER/REPO` binding and full commit identifiers;
- label names, colors, descriptions, and references;
- ordered `GB-NNN` identifiers, unique IDs/titles, non-empty bodies, and unique per-issue labels;
- absence of publisher-reserved markers in source bodies;
- unique remote audit markers and collision-free issue identity.

The publisher remains dry-run by default, supports selected `GB-NNN` IDs, never adopts an unmanaged exact-title collision, and updates marker-managed bodies only with explicit `--update-existing`.
