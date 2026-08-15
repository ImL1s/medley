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

## Implemented versus planned

**Implemented in this slice** (`xai-grok-subagent-resolution::native_route`
plus live spawn in `xai-grok-shell`):

- versioned capability discovery (`supported` / `unsupported` / `unavailable` /
  `incompatible` / `unknown`);
- exact / inherit / ordered-candidate request types;
- deterministic resolver over a synthetic catalog and the live session catalog;
- immutable secret-free route receipts and digests;
- inspect JSON (`medley.native-subagent-route.inspect/v1`);
- declarative `model` / `models` / `routingRequirements` parse on real
  `AgentDefinition` files;
- typed `AgentRouteUxSnapshot` plus compact/detail formatters used by `/agents`;
- spawn-time persistence of receipts on `SubagentMeta`, GCS `subagent.json`
  (`routeReceiptDigest`), and optional ACP `SubagentSpawned` fields;
- usage facts projected from the canonical receipt (`catalogId` / `wireModel` /
  `accessProfile` / `routeDigest`).

Live exact `model:` still uses the legacy pin path: an unknown catalog id warns
and falls through to inherit. Fail-closed exact selection is the offline
resolver and the `models:` ordered path.

**Not implemented here (do not claim):**

- generation-bound `/agents` mutation, lifecycle cards, picker/#207, or the
  full a11y matrix ([#290](https://github.com/ImL1s/medley/issues/290));
- replay-safe runtime fallback ([#18](https://github.com/ImL1s/medley/issues/18));
- qualified model-family metadata.

`medley.native-model-family-metadata.v1` and
`medley.native-replay-safe-fallback.v1` advertise `unsupported`.

## Ownership

### Medley owns

Catalog-ID lookup and duplicate wire-slug disambiguation; readiness, harness,
local-only, and capability eligibility; native child-session construction
(existing spawn path); deterministic candidate resolution; immutable receipts;
replay-safety *admission types* (cross-route fallback still refused in this
slice).

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
refused (`fallback_replay_unsafe`). A `429` is not replay authorization.
Runtime failover remains [#18](https://github.com/ImL1s/medley/issues/18).

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

`/agents` compact rows append a non-color route status. Expanded details add
selection, route status, and receipt fields from `AgentRouteUxSnapshot`.
Generation-bound mutation, lifecycle cards, and stale-action gates remain
[#290](https://github.com/ImL1s/medley/issues/290).
