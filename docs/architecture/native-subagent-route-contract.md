# Optional native subagent route contract

Medley is a community multi-provider fork of Grok Build. This page documents
the **optional, capability-negotiated native subagent route extension**. It is
a Medley extension, not an upstream Grok Build claim. The contract is generic
and contains no oh-my-grok agent names, roles, or workflow taxonomy.

Orchestration consumers may use the contract when present and treat it as
`unsupported` when absent. Original Grok Build compatibility remains the
consumer's responsibility. Medley's `main` stays a pristine upstream mirror;
this feature lives on `providers`.

Tracking: [ImL1s/medley#287](https://github.com/ImL1s/medley/issues/287) ·
docs [ImL1s/medley#289](https://github.com/ImL1s/medley/issues/289) ·
TUI [ImL1s/medley#290](https://github.com/ImL1s/medley/issues/290) ·
consumer [ImL1s/oh-my-grok#131](https://github.com/ImL1s/oh-my-grok/issues/131) /
[#133](https://github.com/ImL1s/oh-my-grok/issues/133) /
[#134](https://github.com/ImL1s/oh-my-grok/issues/134).

Foundations: capability-aware eligibility
[#19](https://github.com/ImL1s/medley/issues/19) · replay-safe failover
[#18](https://github.com/ImL1s/medley/issues/18) · effective route and
credential/origin binding [#110](https://github.com/ImL1s/medley/issues/110) ·
access and usage-scope identity
[#187](https://github.com/ImL1s/medley/issues/187) · usage attribution
[#23](https://github.com/ImL1s/medley/issues/23).

## Implemented versus planned

**Implemented in this slice** (`xai-grok-subagent-resolution::native_route`
plus live spawn in `xai-grok-shell`):

- versioned capability discovery (`supported` / `unsupported` / `unavailable` /
  `incompatible` / `unknown`);
- exact / inherit / ordered-candidate request types;
- deterministic resolver over a synthetic catalog and the live session catalog;
- immutable secret-free route receipts and digests;
- inspect JSON (`medley.native-subagent-route.inspect/v1`) via
  `medley inspect --native-subagent-route <PARENT_SESSION_ID>` (and `--json`),
  using `find_persisted_session_dir_by_id_result` plus bounded no-follow
  `subagents/<id>/meta.json` reads; surviving receipts are checked with
  shipped `inspect_document`;
- declarative `model` / `models` / `routingRequirements` parse on real
  `AgentDefinition` files;
- typed `AgentRouteUxSnapshot` plus compact/detail formatters used by `/agents`;
- spawn-time persistence of receipts on `SubagentMeta`, GCS `subagent.json`
  (`routeReceiptDigest`), and optional ACP `SubagentSpawned` fields;
- inspect/adapter usage facts helper from the canonical receipt (`catalogId` /
  `wireModel` / `accessProfile` / `routeDigest`); live `by_model` usage still
  keys by catalog id on the existing [#23](https://github.com/ImL1s/medley/issues/23)
  path;
- live exact `model:` fail-closed against the session catalog (unknown ids do
  **not** inherit);
- generation-bound `/agents` enable/disable and default mutations (persisted
  to `config.toml`; this session is not silently rebound);
- lifecycle card labels on `/agents` details (selecting / running / same-route
  retry / fallback / refusal / resume / terminal);
- compact-row a11y tests (narrow/normal/wide, CJK, 1,000 synthetic format
  rows, no color-only status);
- fail-closed replay-safe fallback *planner* (`plan_replay_safe_fallback`):
  pre-output same-lane ordered candidates only.

**Not implemented here (do not claim):**

- picker / `/providers` / `/route` control plane ([#207](https://github.com/ImL1s/medley/issues/207));
- live sampler auto-failover on a running child HTTP stream;
- session-only vs persist *model policy* editing in `/agents`;
- the full #290 interaction matrix (mouse/resize/suspend, 1,000-entry TUI
  latency, NO_COLOR terminal snapshots);
- qualified model-family metadata;
- bounded live evidence at exact SHAs (spawn a real child, capture ACP/meta,
  then `inspect --native-subagent-route` plus `omg agents explain --host-inspect`).

`medley.native-model-family-metadata.v1` and
`medley.native-replay-safe-fallback.v1` advertise `unsupported` because live
sampler auto-failover is not wired. The admission planner is tested and
fail-closed.

## Ownership

### Medley owns

Catalog-ID lookup and duplicate wire-slug disambiguation; readiness, harness,
local-only, and capability eligibility; native child-session construction
(existing spawn path); deterministic candidate resolution; immutable receipts;
replay-safety admission planner (live sampler auto-failover is not wired).

### Orchestration consumers own

Agent/profile names; candidate preference order; prompt-family profiles;
external CLI executors; their own evidence / `verified` state.

### Medley does not own

OMG/Sisyphus/Oracle/Librarian roles; online ranking; external Codex/Gemini/
Cursor/Antigravity process launch; a consumer's default candidate order.

## Capability negotiation

```text
medley.native-exact-model.v1
medley.native-ordered-candidates.v1
medley.native-route-receipt.v1
medley.native-model-family-metadata.v1
medley.native-replay-safe-fallback.v1
```

Only `supported` authorizes use. Discovery performs no inference request and
exposes no credentials. Executable name, branding, or state-directory matching
is not a capability.

## Request semantics

```text
Inherit                 — explicit parent catalog only
Exact { catalog_id }    — exact-or-error; never parent
OrderedCandidates       — first fully eligible catalog id in declared order
```

Candidates are catalog IDs, not bare wire slugs. Unknown readiness does not
satisfy a hard requirement. Routing may narrow but never widen capability /
harness / local-only. Consumer policy id/digest is opaque provenance.

Empty `models` is invalid, not inherit. Conflicting non-inherit `model` plus
`models` is rejected.

## Receipts

Successful enhanced resolution produces a versioned, secret-free receipt with
selected catalog id, wire model, route key / access profile, rejected
candidates, attempt, and a SHA-256 digest of the canonical payload. Resume
pins the source catalog/route key and cannot rebind the same wire slug onto
another access route.

Receipt absence on a non-supporting host is `unsupported`, not an empty
fabricated receipt.

## Native versus external executor

`WorkerRoute::Native` is Medley-owned. `WorkerRoute::ExternalExecutor` is a
consumer boundary; Medley does not resolve CLI argv, PTY, or process identity
as a provider/access route. Worktree isolation is not an execution sandbox.

## Fallback

Cross-route fallback after visible output, a tool call, or a side effect is
refused (`fallback_replay_unsafe`). Exact and inherit never fall over. A
`429` is not replay authorization by itself. Same-lane ordered candidates may
be *admitted* by `plan_replay_safe_fallback` before output; live sampler
auto-failover is not wired. Runtime failover remains
[#18](https://github.com/ImL1s/medley/issues/18).

## Inspect JSON

Consumers such as oh-my-grok read `medley.native-subagent-route.inspect/v1`
from an explicit inspect document (never inferred from PATH). Example
(fictional ids only):

```json
{
  "schema": "medley.native-subagent-route.inspect/v1",
  "schemaVersion": 1,
  "host": "medley",
  "capabilities": [
    {
      "capability_id": "medley.native-ordered-candidates.v1",
      "state": "supported",
      "version": "v1",
      "reason": "deterministic offline first-eligible catalog selection"
    }
  ],
  "receipts": []
}
```

## TUI

`/agents` compact rows append non-color route status, selection intent, and
capability floor. Expanded details add selection, route status, rejected
candidates, receipt fields, and a lifecycle card from `AgentRouteUxSnapshot`.
Enable/disable (`t`) and default (`s`) persist to `config.toml` and are
generation-bound; stale actions refuse with `stale_generation`. This session
is not silently rebound. Picker/#207 and the remaining a11y interaction
matrix stay [#290](https://github.com/ImL1s/medley/issues/290).
