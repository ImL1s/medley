# Optional plugin-facing native subagent route contract

**Status:** Partially shipped — the core contract landed on `providers` via
[ImL1s/medley#385](https://github.com/ImL1s/medley/pull/385),
[#386](https://github.com/ImL1s/medley/pull/386) and
[#387](https://github.com/ImL1s/medley/pull/387); a further #290 slice landed via
[#388](https://github.com/ImL1s/medley/pull/388).
This page is the 2026-08-09 design record, kept as written — much of it is
normative intent that was descoped or deferred, not a description of current
behaviour. For the surface that actually shipped, including what was
deliberately left out, see
[`docs/architecture/native-subagent-route-contract.md`](../architecture/native-subagent-route-contract.md).  
**Date:** 2026-08-09  
**Target branch:** `providers`  
**Tracking issue:** [ImL1s/medley#287](https://github.com/ImL1s/medley/issues/287)  
**Medley architecture/docs:** [ImL1s/medley#289](https://github.com/ImL1s/medley/issues/289)  
**Medley TUI/UX:** [ImL1s/medley#290](https://github.com/ImL1s/medley/issues/290)  
**Optional product consumer:** [ImL1s/oh-my-grok#131](https://github.com/ImL1s/oh-my-grok/issues/131)  
**Consumer UX:** [ImL1s/oh-my-grok#134](https://github.com/ImL1s/oh-my-grok/issues/134)  
**Counterpart plan PR:** [ImL1s/oh-my-grok#132](https://github.com/ImL1s/oh-my-grok/pull/132)

## Decision

Medley will provide a generic, typed, secret-free, capability-negotiated **optional native subagent route extension** for orchestration products.

The extension resolves exact, inherited, or ordered Medley catalog candidates into one canonical effective route; enforces readiness/access/capability/harness constraints; creates the child session; and records an immutable route receipt.

The orchestration consumer supplies policy intent. Medley does not embed that consumer's agent names, prompt families, categories, workflow semantics, candidate ranking, or completion state.

```text
consumer policy
  agent/profile + exact/inherit/ordered Medley catalog IDs + requirements
                                  │
                                  ▼
optional Medley native route contract
  capability negotiation + eligibility + effective route + child session
                                  │
                                  ▼
route receipt + attempt lineage + inspect/ACP/TUI/usage projections
```

This extension does not redefine original Grok Build. A consumer such as oh-my-grok may retain a first-class original Grok Build baseline when the extension is absent.

Medley's `main` remains a pristine upstream mirror; implementation and documentation land only on `providers`.

## Compatibility contract

The extension must preserve:

1. Medley does not require oh-my-grok or any specific orchestration product.
2. A consumer may operate on original Grok Build without this extension.
3. Missing extension capability is `unsupported`, not a broken baseline host.
4. Consumers negotiate versioned capabilities rather than detecting Medley by binary name, state path, branding, or loose version matching.
5. Medley catalog IDs are never silently interpreted as original Grok Build model IDs.
6. Generic Medley types contain no OMG/Sisyphus/Oracle/Librarian or workflow-specific taxonomy.
7. Existing exact/inherit agent, role, persona, and Task configuration remains compatible.
8. Unsupported schema/version is rejected explicitly rather than ignored into another meaning.

## Existing foundations

Medley already has:

- catalog IDs distinct from wire-model strings;
- provider/backend/auth/readiness resolution;
- exact/inherit `AgentDefinition.model`;
- exact `[subagents.models]`, role, persona, and model-facing `Task.model` overrides;
- native child sessions with independent sampling configuration;
- capability ceilings, tool filtering, background execution, worktree isolation, persistence, and resume;
- declarative plugin agents;
- typed credential/effective-route/access foundations in related work;
- a native `/agents` modal listing built-in, user, project, bundled, and plugin definitions.

The missing seam is a consumer-facing request/receipt contract and route-aware agent UX that do not rebuild provider or credential logic.

Existing issues remain authoritative:

- [#19](https://github.com/ImL1s/medley/issues/19): capability-aware eligibility and explain/loss facts;
- [#18](https://github.com/ImL1s/medley/issues/18): replay-safe provider/model failover;
- [#110](https://github.com/ImL1s/medley/issues/110): canonical effective route and credential/origin binding;
- [#187](https://github.com/ImL1s/medley/issues/187): access, billing, usage-scope, and connection identity;
- [#23](https://github.com/ImL1s/medley/issues/23): provider/model/subagent usage attribution;
- [#207](https://github.com/ImL1s/medley/issues/207): provider-aware TUI/CLI control plane;
- completed [#136](https://github.com/ImL1s/medley/issues/136): credential material and provenance remain structurally inseparable.

## Responsibility boundary

### Medley owns execution truth

- catalog lookup and duplicate-wire-slug disambiguation;
- final provider, backend, origin, readiness, and access identity;
- typed credential/access provenance without secret bytes;
- capability, tool, context, modality, local-only, and harness eligibility;
- native child-session creation, persistence, cancellation, resume, and usage;
- deterministic candidate selection;
- immutable route receipts and attempt observations;
- replay-safety admission for cross-route fallback;
- human, JSON, ACP, TUI, and usage projections derived from the same canonical route.

### The consumer owns policy

- agent/profile/category names;
- role/workflow semantics;
- exact/inherit/candidate preference and attempt budget;
- model-family prompt and reasoning policy;
- user-facing workflow explanation;
- external CLI executor selection and process topology;
- evidence, acceptance, and verified/completion state.

### Medley does not own

- product-specific roles or prompts;
- online quality ranking or task-content sampling;
- external Codex/Gemini/Cursor/Antigravity process launch;
- a consumer's default candidate order;
- compatibility policy for original Grok Build beyond publishing an optional generic extension;
- an orchestration product's completion authority.

## Identity model

| Identity | Meaning | Owner |
|---|---|---|
| Catalog ID | Stable configured Medley route key | Medley |
| Wire model | Provider request identifier | Medley effective route |
| Effective provider route | Backend, final origin, auth/access identity | Medley |
| Agent/harness | Prompt/tool runtime used by the child | Native host contract |
| Consumer policy | Selection order, requirements, prompt profile | Consumer |
| External executor | Separate CLI process and supervision | Consumer |

One `provider` string must not represent all six concepts.

## Capability negotiation

Expose one stable machine-readable registry consumed by inspect/ACP/plugins and drift-tested against docs.

Conceptual capabilities:

```text
medley.native-exact-model.v1
medley.native-ordered-candidates.v1
medley.native-route-receipt.v1
medley.native-model-family-metadata.v1
medley.native-replay-safe-fallback.v1
```

Final names may change but must be single-source across code, schema, JSON, docs, and tests.

Required states:

```text
supported
unsupported
unavailable
incompatible
unknown
```

Only `supported` authorizes use. Capability discovery performs no inference request and exposes no credential/account data.

The Rust sketches below are the 2026-08-09 shape. The shipped types are in
`xai-grok-subagent-resolution::native_route::types` and differ in several fields;
read them there rather than copying from here.

## Request contract

```rust
enum NativeModelSelection {
    Inherit,
    Exact { catalog_id: String },
    OrderedCandidates { catalog_ids: Vec<String> },
}

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

Rules:

- candidate entries are Medley catalog IDs, never bare wire slugs;
- consumer policy ID/digest is opaque provenance, not authority over route/access facts;
- unknown capability/readiness never satisfies a hard requirement;
- routing may narrow but never widen tools, capability, harness, local-only, or access scope;
- requests contain no credential, endpoint override, header/query value, API key, account ID, or billing assertion;
- a consumer that cannot negotiate the schema must not send it.

## Declarative host extension

The minimum plugin-facing extension is generic ordered policy on agent/role definitions, not an arbitrary callback that rewrites spawn arguments.

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

Compatibility:

- existing exact `model` and `model: inherit` remain valid;
- conflicting `model` and `models` is rejected unless one documented migration rule exists;
- empty candidates are invalid, not inherit;
- model-facing `Task.model` remains an exact override with distinct provenance;
- arbitrary candidate lists are not exposed to model output in the first slice;
- plugin/project trust and global credential/access ownership remain unchanged;
- this syntax is a Medley capability and is not claimed as upstream Grok Build behavior.

## Resolution semantics

Freeze current single-model precedence in golden tests before extending it.

1. **Resume preserves identity.** Source model/route and receipt lineage remain pinned.
2. **Exact means exact.** Missing, unready, incompatible, or out-of-policy exact selection fails closed and never becomes the parent model.
3. **Candidates preserve order.** The first fully eligible catalog route wins.
4. **Inheritance is explicit.** Parent route is selected only through `Inherit` or documented legacy mapping.
5. **No permission repair.** Selection cannot add tools, widen capability, change harness, or relax local-only/access constraints.
6. **Unknown is not ready.** Cold/incomplete state returns a typed unresolved result.
7. **Duplicate wire slugs remain distinct.** Catalog/connection/access identity is part of the route.
8. **Selection is offline.** No task content or inference request is sent to rank candidates.

## Immutable route receipt

Every successful enhanced native child receives a versioned, deterministic, secret-free receipt containing:

```text
parent/child/session identity
requested selection and candidate summary
consumer policy ID/digest
selected catalog ID and wire model
canonical route/access summary
harness and effective capability mode
selection provenance
ordered rejected candidates/reasons
route digest and attempt
created timestamp
resume/source receipt relationship
```

The receipt is derived from the same effective route used to build the sampling client. It never contains credentials, header/query values, authorization URLs, account IDs, JWT material, prompts, or full responses.

Inspect, TUI, ACP, usage, and consumers use the receipt/canonical route rather than reconstructing labels. Receipt absence on a non-supporting host is an unsupported capability, not a fabricated empty receipt.

## Runtime fallback boundary

Initial candidate selection and runtime provider-error fallback are distinct.

Expose attempt lifecycle facts:

```text
attempt_started
first_provider_byte_seen
visible_output_committed
tool_call_emitted
tool_side_effect_started
attempt_terminal
```

Medley #18 may advance only when replay safety is proven. At minimum fallback is refused after visible output, a tool call without explicit replay proof, or any side effect.

Every candidate independently resolves credential/access identity. No credential object is copied. `429` or retryable `5xx` is an error classification input, not replay authorization.

## Native versus external execution

```rust
enum WorkerRoute {
    Native(NativeSubagentRouteRequest),
    ExternalExecutor(ExternalExecutorDescriptor),
}
```

Medley owns only `Native`. The external descriptor is a boundary type owned by the consumer. CLI executable/model flags, PTY, process identity, and supervision are not Medley provider/access routes.

No external CLI spawning is in scope.

## TUI and UI/UX contract

[#290](https://github.com/ImL1s/medley/issues/290) owns the native UI delivery.

The existing `/agents` modal is enhanced rather than replaced:

- compact rows preserve agent identity, exact/inherit/candidate or selected route state, readiness, and capability floor;
- expanded detail shows policy provenance, candidate order, selected route/access summary, typed rejection reasons, prompt profile, receipt, attempt, resume, and fallback lineage;
- subagent lifecycle cards distinguish initial selection, same-route retry, cross-route fallback, refusal, resume, replacement, failure, and cancellation;
- actions link to canonical `/route`, provider/model detail, and offline doctor services from #207;
- opening/filtering/expanding performs no inference or live provider probe;
- mutations are generation-bound, explicit about session versus persisted scope, revalidated, and rollback-safe;
- narrow/normal/wide, no-color, keyboard/mouse, resize, UTF-8/CJK, suspend/resume, and large-catalog behavior are test gates.

One typed `AgentRouteUxSnapshot` or canonical source must back TUI, inspect, JSON, ACP, usage, and compatible consumer adapters. Pager code never parses human strings or builds a second route resolver.

## Diagnostics, persistence, and usage

One stable JSON schema and concise human projection explain:

- requested policy and provenance;
- capability/schema version;
- selected route and rejected candidates;
- provider/backend/origin/access summary;
- harness/capability ceiling;
- exact/candidate/inherit/resume semantics;
- attempt/receipt lineage.

Persistence stores catalog/access identity and route digest, not only a wire model. Restore must not rebind the same wire slug to a different route. Usage includes route digest/catalog/access identity, agent/role, attempt, and provenance without prompts or credentials.

## Security invariants

- policy cannot introduce credentials or endpoints;
- project policy cannot weaken global credential/access ownership;
- capability/tool restrictions combine by intersection;
- unknown tools/capabilities fail closed in restricted modes;
- local-only cannot select cloud routes;
- different access/billing profiles remain distinct even with the same wire model;
- exact routes cannot silently cross subscription, PAYG, gateway, or local boundaries;
- Debug/Serialize/inspect/ACP/TUI/copy actions receive sentinel redaction tests;
- worktree/integration isolation is not an execution sandbox;
- no consumer hard dependency is created.

## Delivery sequence

1. **Capability contract and goldens** — exact/inherit/resume precedence, capability registry, pure request/result/rejection types and offline resolver.
2. **Declarative integration** — one ordered-policy serialization, schema/version/trust validation, exact fail-closed behavior.
3. **Receipt and read-only projections** — persistence, inspect/ACP/JSON/usage seams, docs #289.
4. **Route-aware TUI** — #290 read-only rows/details/actions and lifecycle cards, then generation-bound mutations.
5. **Replay-safe fallback seam** — attempt lifecycle and #18 admission; no unconditional retry loop.

## Program definition of done

### Contract complete

- one versioned optional request/receipt contract covers exact, inherit, and ordered candidates;
- existing single-model behavior is preserved;
- explicit exact never silently uses parent/another route;
- deterministic offline selection and typed rejections are proven;
- every enhanced child has a secret-free route-bound receipt;
- resume preserves route/provenance;
- native and external executor concepts cannot be confused;
- no consumer-specific role/prompt policy is hardcoded in Medley.

### Full product integration complete

- TUI, inspect, JSON, ACP, usage, and consumer adapters agree on policy, selected route, status, attempt, and digest;
- `/agents` and lifecycle UX satisfy #290 responsive/accessibility/stale-action gates;
- replay-safe fallback is proven and refused after visible output/tool/side effect;
- #287, #289, #290, #19, #18, #23, #110, #187, and #207 use one canonical route/reason taxonomy;
- original Grok Build remains an optional consumer baseline concern and is not misrepresented as implementing this extension;
- no unresolved P0/P1 security, route-identity, exact-route, resume, unsafe-replay, secret-leak, stale-action, or UI/JSON parity finding remains;
- required hermetic CI passes and bounded Medley live evidence is tied to exact SHAs;
- documentation states implemented versus planned capabilities truthfully and never lands on `main`.

## Non-goals

- no OMG agent roster or prompt profiles in Medley;
- no requirement that OMG use Medley;
- no claim that original Grok Build implements the extension;
- no external CLI process manager;
- no online benchmark or quality ranking;
- no task-content sampling to choose a route;
- no silent capability relaxation;
- no implicit cross-billing or local-to-cloud fallback;
- no execution-sandbox claim;
- no product changes on `main`.
