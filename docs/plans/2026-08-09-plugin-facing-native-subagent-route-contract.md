# Plugin-facing native subagent route contract

**Status:** Proposed  
**Date:** 2026-08-09  
**Target branch:** `providers`  
**Tracking issue:** [ImL1s/medley#287](https://github.com/ImL1s/medley/issues/287)  
**Product consumer:** [ImL1s/oh-my-grok#131](https://github.com/ImL1s/oh-my-grok/issues/131)  
**Counterpart plan:** [oh-my-grok native agent model routing](https://github.com/ImL1s/oh-my-grok/blob/main/docs/plans/2026-08-09-medley-native-agent-model-routing.md)

## Decision

Medley will provide a generic native subagent execution contract for orchestration products. The contract resolves exact, inherited, or ordered catalog candidates into one canonical effective model route, enforces readiness/access/capability/harness constraints, creates the child session, and records an immutable secret-free route receipt.

The orchestration product supplies policy intent. Medley does not embed that product's agent names, prompt families, categories, or model preference order.

```text
orchestration policy
    agent/profile + ordered Medley catalog IDs + hard requirements
                         │
                         ▼
Medley native route resolver
    readiness + capability + harness + access/credential identity
                         │
                         ▼
selected effective route → child session → route receipt → usage/inspect/ACP
```

This plan is the architecture summary. The complete implementation scope, tests, acceptance criteria, and PR slicing live in Medley #287.

## Context

Medley already has most of the runtime primitives:

- catalog IDs distinct from provider wire-model strings;
- provider/backend/auth/readiness resolution;
- exact and inherited model selection on agents, roles, personas, and tasks;
- native child sessions with independent sampling configuration;
- capability ceilings, tool filtering, background execution, worktrees, persistence, and resume;
- declarative plugin agents.

The missing seam is a consumer-facing request and receipt that lets a declarative orchestration plugin express an ordered native policy without rebuilding provider or credential logic.

Existing issues remain authoritative:

- [#19](https://github.com/ImL1s/medley/issues/19): capability-aware eligibility and loss/explain reports;
- [#18](https://github.com/ImL1s/medley/issues/18): replay-safe provider/model failover;
- [#110](https://github.com/ImL1s/medley/issues/110): canonical effective route and credential/origin binding;
- [#187](https://github.com/ImL1s/medley/issues/187): access, billing, usage-scope, and connection identity;
- [#23](https://github.com/ImL1s/medley/issues/23): provider/model/subagent usage attribution;
- completed [#136](https://github.com/ImL1s/medley/issues/136): credential material and provenance must remain inseparable.

## Responsibility boundary

### Medley owns execution truth

Medley is authoritative for:

- catalog lookup and duplicate-wire-slug disambiguation;
- final provider, backend, origin, and route readiness;
- typed credential/access identity without secret bytes;
- capability, tool, context, modality, local-only, and harness eligibility;
- child-session creation, persistence, cancellation, resume, and usage;
- route receipts, attempt lineage, and replay-safety observations;
- human, JSON, ACP, TUI, and usage projections derived from the same route object.

### The consumer owns policy

The consumer is authoritative for:

- agent/profile/category names;
- ordered catalog preference;
- prompt-family and reasoning preferences;
- workflow-specific policy selection;
- user-facing explanation of why that product chose a policy.

Medley may record opaque policy identifiers and digests. It must not interpret product-specific role or prompt semantics.

## Identity model

The design keeps these concepts separate:

| Identity | Meaning | Owner |
|---|---|---|
| Catalog ID | Stable configured Medley route key | Medley |
| Wire model | Identifier sent to a provider | Medley route |
| Effective provider route | Backend, final origin, auth/access identity | Medley |
| Agent/harness | Prompt/tool runtime used by the child | Medley execution contract |
| Consumer policy | Exact/inherit/candidate preference and requirements | Orchestrator |
| External executor | Separate CLI process such as Codex or Gemini | Orchestrator |

A single field named `provider` must not stand for all of these.

## Request model

The generic request needs three selection modes:

```rust
enum NativeModelSelection {
    Inherit,
    Exact { catalog_id: String },
    OrderedCandidates { catalog_ids: Vec<String> },
}
```

The request also carries hard requirements and opaque policy provenance:

```rust
struct NativeSubagentRouteRequest {
    schema_version: u32,
    selection: NativeModelSelection,
    required_capabilities: CapabilityRequirements,
    capability_ceiling: Option<SubagentCapabilityMode>,
    required_harness: Option<String>,
    minimum_context_tokens: Option<u64>,
    local_only: bool,
    consumer_policy_id: Option<String>,
    consumer_policy_digest: Option<String>,
}
```

The request cannot contain credentials, endpoint overrides, raw headers, query values, billing assertions, or account identity. Those facts are derived from the selected Medley route.

## Declarative host extension

Medley plugins are primarily declarative. The minimum host extension is therefore a generic ordered model policy on agent/role definitions, not an arbitrary callback that rewrites spawn arguments.

Conceptually:

```yaml
model: inherit
models:
  - catalog-id-a
  - catalog-id-b
routingRequirements:
  structuredOutput: true
  minimumContextTokens: 128000
```

The final format must be defined once and reused by parsing, schemas, agent definitions, role definitions, generated documentation, and tests.

Compatibility rules:

- existing exact `model` and `model: inherit` remain valid;
- conflicting `model` and `models` input is rejected unless one explicit migration rule is defined;
- an empty list is invalid, not inherit;
- model-facing `Task.model` remains an exact override with distinct provenance;
- model output does not receive an unrestricted arbitrary candidate-list field in the first delivery slice.

## Resolution semantics

The implementation first freezes current single-model precedence in golden tests, then extends each policy slot without reordering legacy behavior.

Special semantics are fixed:

1. **Resume preserves identity.** Resume pins the source route/model and receipt lineage.
2. **Exact means exact.** An explicit exact selection fails closed and never silently becomes the parent model.
3. **Candidates preserve order.** The first fully eligible catalog route wins.
4. **Inheritance is explicit.** Parent inheritance occurs only through an inherit policy.
5. **No permission repair.** Selection cannot widen tools, capability, harness, or access scope.
6. **Unknown does not satisfy a hard requirement.** Cold or incomplete state yields a typed unresolved result.
7. **Selection is offline.** Medley does not send task content or probe candidate inference endpoints to rank routes.

## Route receipt

Each successful child receives an immutable, versioned, deterministic, secret-free route receipt covering:

- child and parent session identity;
- requested selection summary;
- consumer policy ID and digest;
- selected catalog ID and wire model;
- canonical effective route/access summary;
- harness and effective capability mode;
- selection provenance;
- ordered rejected candidates and typed reasons;
- route digest, attempt number, and timestamp;
- resume/source receipt relationship.

The receipt is not another independently constructed UI model. The same effective route used to create the sampling client supplies its provider/access facts.

The receipt never contains credentials, header/query values, authorization URLs, account IDs, JWT fragments, prompts, or full responses.

## Runtime fallback boundary

Initial candidate resolution and runtime provider-error fallback are different operations.

Initial selection chooses the first currently eligible candidate before the child starts. Runtime fallback is governed by Medley #18 and may move to another candidate only when the prior attempt is proven replay-safe.

The runtime must expose attempt facts such as:

```text
attempt_started
first_provider_byte_seen
visible_output_committed
tool_call_emitted
tool_side_effect_started
attempt_terminal
```

Fallback is refused after visible output, an unknown-replay-safety tool call, or any side effect. Each new candidate independently resolves its own credential/access route. No credential material is copied from the prior attempt.

## Native versus external execution

The shared architecture distinguishes native model routing from external CLI execution:

```rust
enum WorkerRoute {
    Native(NativeSubagentRouteRequest),
    ExternalExecutor(ExternalExecutorDescriptor),
}
```

Medley implements `Native`. The consumer may implement `ExternalExecutor`. External CLI names, model flags, PTY requirements, and process identity are not Medley provider/access routes.

## Diagnostics and persistence

One stable JSON contract and concise human view should explain:

- requested policy and provenance;
- selected route and access identity;
- rejected candidates and reasons;
- harness and capability ceiling;
- exact, inherited, candidate, or resumed semantics.

Persistence stores selected catalog/access identity and route digest, not only a wire model. Restore must not rebind the same wire slug to a different route. Usage attribution includes catalog/access identity, consumer role/agent, attempt, and selection provenance without prompts or credentials.

## Security invariants

- policy cannot introduce credentials or endpoints;
- project-local policy cannot weaken global credential/access ownership;
- capability and tool restrictions combine by intersection;
- unknown tools/capabilities fail closed in restricted modes;
- local-only cannot select an external route;
- distinct access/billing profiles remain distinct even with the same wire model;
- exact routes cannot silently cross subscription, PAYG, gateway, or local boundaries;
- all serialized/debug/UI projections receive sentinel redaction tests;
- worktree/integration isolation is not an execution sandbox.

## Delivery sequence

1. **Pure contract and goldens** — freeze exact/inherit/resume semantics and implement an offline candidate resolver over synthetic catalog fixtures.
2. **Declarative integration** — add one ordered-policy serialization for agents/roles while preserving existing exact configurations.
3. **Receipt and read-only projections** — persist the canonical receipt and expose inspect/ACP/JSON/usage seams.
4. **Replay-safe fallback seam** — integrate attempt lifecycle facts with Medley #18 without adding an unconditional retry loop.

The full file map, deterministic test matrix, and acceptance checklist are maintained in [Medley #287](https://github.com/ImL1s/medley/issues/287).

## Non-goals

- no OMG agent roster or prompt profiles in Medley;
- no external CLI process manager;
- no online benchmark or automatic quality ranking;
- no task-content sampling to choose a provider;
- no silent capability relaxation;
- no implicit cross-billing or local-to-cloud fallback;
- no product changes on `main`.
