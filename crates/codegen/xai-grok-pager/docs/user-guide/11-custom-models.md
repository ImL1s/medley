# Custom Models

Grok connects to custom model endpoints for alternative providers, self-hosted models, and overriding built-in settings. This guide explains how to select models, configure endpoints, and integrate third-party providers.

> **Fork note (Medley).** Several capabilities on this page —
> `auth_scheme = "none"` for keyless local servers, the fail-closed handling of an
> invalid `auth_scheme`, and the `openai-codex` provider — ship in
> [Medley](https://github.com/ImL1s/medley), a community fork that is not
> affiliated with or endorsed by xAI. Provider and model names below are the
> trademarks of their owners and appear only to identify the endpoints you can
> point Medley at; connecting to any of them is governed by that provider's terms,
> not by this project.

---

## Default Models

By default, Grok uses models hosted by SpaceXAI, and new sessions start with `grok-4.5`. Default models require no configuration. Authenticate with `grok login` or an API key, then start a session.

List all available models:

<!-- medley-doc-test:shell-offline:offline-list-models -->
```bash
grok models
```

---

## Selecting a Model

### CLI Flag

```bash
grok -p "Hello" -m grok-build
```

### Slash Command

In the TUI, switch models during a session:

```
/model grok-build
```

Or use the alias:

```
/m grok-build
```

### Model Picker (Ctrl+M)

Press `Ctrl+M` from the scrollback pane to open the model picker. It lists all available models, both built-in and custom, and lets you switch with a single keystroke. With the prompt focused, `Ctrl+M` toggles multiline input instead -- use `/model` to switch without leaving the prompt.

Each row shows a short provider hint and a readiness badge (`ready`, `missing`, or `none`). Models that are not ready (for example, missing `OPENAI_API_KEY`) are dimmed and cannot be selected; `/model <id>` rejects them with the same reason. Switching between auth classes (xAI session, env API key, or keyless local) asks for confirmation first.

### Config Default

Set a persistent default in `~/.medley/config.toml`:

<!-- medley-doc-test:toml:models-default -->
```toml
[models]
default = "grok-4.5"
```

---

## Supported API Backends

Grok supports the following API backends. Set `api_backend` in your
`[model.*]` config to choose which protocol the model uses:

| Value | API | Default |
|-------|-----|---------|
| `"chat_completions"` | OpenAI Chat Completions (`/v1/chat/completions`) [types.rs:1016](file:///private/tmp/n193b/crates/codegen/xai-grok-sampling-types/src/types.rs#L1016), [client.rs:2471](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L2471) | Yes |
| `"responses"` | OpenAI Responses (`/v1/responses`) [types.rs:1018](file:///private/tmp/n193b/crates/codegen/xai-grok-sampling-types/src/types.rs#L1018), [client.rs:2477](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L2477) | |
| `"codex_responses"` | ChatGPT Codex Responses (`/backend-api/codex/responses`) [types.rs:1024](file:///private/tmp/n193b/crates/codegen/xai-grok-sampling-types/src/types.rs#L1024), [client.rs:2477](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L2477) | Built-in provider only |
| `"messages"` | Anthropic Messages (`/v1/messages`) [types.rs:1026](file:///private/tmp/n193b/crates/codegen/xai-grok-sampling-types/src/types.rs#L1026), [client.rs:2483](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L2483) | |

When you omit `api_backend`, Grok uses `chat_completions` ([types.rs:1015-1016](file:///private/tmp/n193b/crates/codegen/xai-grok-sampling-types/src/types.rs#L1015-1016)).

To send provider-specific non-secret headers -- for example, Anthropic's `anthropic-version` -- use the `extra_headers` field described below. Grok sends those headers verbatim with every request to the endpoint. Prefer `auth_scheme` and `env_key` / `api_key` for credentials rather than putting secrets in `extra_headers`.

---

## Configuring Custom Models

Add custom model endpoints under `[model.<name>]` sections.

You can define them globally in `~/.medley/config.toml` and/or per-project in
`.grok/config.toml`. Project `[models]` and `[model.*]` sections merge over the
global config (repo root to cwd; closer files win) **when the workspace is
trusted**. In an untrusted clone, project model sections are ignored.

`[model_providers.*]` remains global-only. A project-local
`[model_providers.*]` block is ignored and reported in config warnings.

```toml
[model.my-model]
model = "model-id"                        # Model identifier sent to the API
base_url = "https://api.example.com/v1"   # OpenAI-compatible endpoint
name = "Display Name"                     # Shown in the model picker
description = "Model description"          # Optional description
api_key = "..."                           # API key for this provider (optional)
env_key = "PROVIDER_API_KEY"              # Env var name (optional; string or array of [A-Za-z_][A-Za-z0-9_]* names)
auth_scheme = "bearer"                    # "bearer" (default), "x_api_key", or "none"
api_backend = "chat_completions"          # "chat_completions", "responses", or "messages"
temperature = 0.7                         # Sampling temperature
top_p = 0.95                              # Nucleus sampling parameter
max_completion_tokens = 8192              # Maximum tokens per response
context_window = 128000                   # Total context window in tokens
extra_headers = { "X-Request-Tags" = "team=example" }  # Extra request headers, sent verbatim (optional)
query_params = { api-version = "2026-07-22" } # Query params appended to every request URL (optional)
env_http_headers = { "X-Tenant" = "TENANT_TOKEN" }    # Headers from env vars, resolved at client build (optional)
```

### Field reference: `auth_scheme`

| Value | Behavior |
|-------|----------|
| `"bearer"` | Default. Sends `Authorization: Bearer <key>` from `api_key` / `env_key` ([config.rs:23](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/config.rs#L23), [client.rs:946](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L946), [client.rs:1156](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L1156)). The ambient fallback (your session token or `XAI_API_KEY`) applies **only when the final effective URL is a first-party xAI origin** ([client.rs:860-921](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L860-921)). |
| `"x_api_key"` | Sends `x-api-key: <key>` (Anthropic-style) instead of Bearer. Use with `env_key` or `api_key` ([config.rs:24](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/config.rs#L24), [client.rs:937](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L937), [client.rs:1151](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L1151)). |
| `"none"` | Sends **no** auth header. Required for keyless local servers so ambient xAI credentials are not attached ([config.rs:25](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/config.rs#L25), [client.rs:952](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L952), [client.rs:1178](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/client.rs#L1178)). |

When `auth_scheme` is omitted, Grok uses `"bearer"` ([config.rs:22-23](file:///private/tmp/n193b/crates/codegen/xai-grok-sampler/src/config.rs#L22-23)).

A **first-party xAI origin** is `https://x.ai` or any `https://*.x.ai` host, plus the built-in xAI API endpoint — HTTPS only, never loopback, never cleartext. On every other origin the ambient fallback (session token, `XAI_API_KEY`) is withheld.

**Invalid values fail closed.** If `auth_scheme` is misspelled or unsupported (for example `"bearer "` or `"noauth"`), Grok keeps the model entry but marks it **unready** with a validation error. The model picker and `/model` refuse to select it. New sessions, session restore, and ACP model switches also refuse to attach an unready model (they fall back or block prompts instead of silently using ambient Bearer credentials). No sampling request is sent for an unready selection. Grok does **not** silently fall back to `"bearer"` or `"none"` for bad values.

**Duplicate routing slugs.** The catalog is keyed by each entry's config key (`[model.<key>]`). Two entries may share the same wire `model` slug but must use distinct catalog keys. Always pick models by their catalog key in config and `/model`; slug-only lookups can bind to the wrong entry when duplicates exist.

When you need two routes for the same upstream slug, keep distinct catalog keys
and point defaults at those keys (not the wire slug):

<!-- medley-doc-test:toml:catalog-key-wire-slug-default-refs -->
```toml
[model.prod-grok-build]
model = "grok-4.5"
base_url = "https://api.x.ai/v1"
context_window = 256000

[model.canary-grok-build]
model = "grok-4.5"
base_url = "https://api.x.ai/v1"
context_window = 256000

[models]
default = "canary-grok-build"
web_search = "prod-grok-build"
session_summary = "canary-grok-build"
```

**Native subagent route contract.** The same catalog-key-versus-wire-slug
identity governs Medley's optional native subagent route contract: route
requests name catalog keys, never bare wire slugs, and each enhanced child
session records a secret-free receipt naming the selected catalog key, wire
model, and access route, so two entries sharing one slug stay distinct in
receipts and diagnostics. The extension is capability-negotiated, and original
Grok Build does not implement it. See
[`docs/architecture/native-subagent-route-contract.md`](https://github.com/ImL1s/medley/blob/providers/docs/architecture/native-subagent-route-contract.md).

**xAI identity headers.** On third-party endpoints or when `auth_scheme = "none"`, Grok omits `x-grok-user-id` and `x-grok-deployment-id` so account metadata is not sent to external hosts.

### Credential Resolution

Grok resolves the API key in this order (skipped entirely when `auth_scheme = "none"`):

1. The `api_key` field in the model config
2. The environment variable(s) named by `env_key` — a single string or an array of names. Names are trimmed; empty/whitespace-only entries and names outside `[A-Za-z_][A-Za-z0-9_]*` are rejected with a config warning pointing at the model field (they are never selected silently). The first set, non-empty value among the remaining names wins (for example `env_key = ["ANTHROPIC_AUTH_TOKEN", "LC_ANTHROPIC_AUTH_TOKEN"]` for SSH `LC_*` forwarding)
3. A named `auth_provider` — the provider owns auth for this model. If its credential is unavailable the request fails closed and **never** falls back to xAI credentials
4. An explicit `Authorization` or `x-api-key` header supplied through `extra_headers` or `env_http_headers` — treated as auth you own, so no ambient credential is ever added underneath it
5. **First-party xAI origins only:** your signed-in session token (from `grok login`)
6. **First-party xAI origins only:** the `XAI_API_KEY` environment variable (`GROK_CODE_XAI_API_KEY` is also accepted, for backward compatibility)

A model whose final effective URL — after `model_provider`, `base_url`, and `api_base_url` are merged — is **not** a first-party xAI origin, and which declares none of sources 1–4, is marked **unready**. The picker dims it, `/model` and ACP switches reject it, and no request is sent. Your session token and `XAI_API_KEY` are never attached to a non-xAI endpoint, and no unauthenticated request is sent in their place.

**If a configuration relied on the old fallback:**

- Talking to an external provider: set `env_key = "PROVIDER_API_KEY"`, or `api_key`, or `auth_provider`, or an explicit credential header.
- Running a keyless local server (Ollama, LM Studio, llama.cpp): set `auth_scheme = "none"`.

### Trusting a Self-Hosted xAI Gateway Origin

If you run a **self-hosted gateway that terminates xAI API traffic** (your own
TLS endpoint in front of the xAI upstream), the first-party rule above withholds
your ambient credential from it — the origin is recomputed from the URL and is
not `*.x.ai`, so it is treated as external. `trusted_xai_origins` is the
supported, explicit way to declare that one such origin is yours:

```toml
# ~/.medley/config.toml  (or a system managed config) — local disk only
trusted_xai_origins = ["https://gateway.internal.example"]
```

The constraints are the feature:

- **Local and explicit.** The key is read only from config files on this
  machine (`~/.medley/config.toml` and the system managed tiers). A
  `trusted_xai_origins` key in a project `.grok/config.toml` loads **nowhere** —
  a trust decision cannot arrive with a cloned repo — and Grok reports it as an
  inert section at session start and under Config Warnings in `grok inspect`.
  There is deliberately no environment variable for this.
- **HTTPS only, exact origin.** Entries must be `https://host[:port]` with no
  userinfo, query, or fragment. Matching is exact on host + effective port
  (default 443): declaring `https://gateway.internal.example` does not trust
  `https://gateway.internal.example:8443` or `https://evil-gateway.internal.example`.
  Entries that fail these rules are rejected with a warning that lists them
  (sanitized — userinfo, query, and fragment are stripped before display) and
  are never trusted.
- **Narrow grant.** A declared origin receives the ambient credential
  (session token / `XAI_API_KEY`) and session-token refresh, nothing more.
  xAI identity headers (`x-grok-user-id`, `x-grok-deployment-id`) stay off, and
  the external metadata boundary still applies — a declared origin is not a
  first-party origin, it is a named exception for the credential only. The
  HTTPS floor is re-derived from the final URL at send time, so a declaration
  can never attach the credential to a cleartext endpoint.
- **Visible.** The first time a declaration actually changes an outcome (an
  ambient credential is forwarded because of it), Grok prints a one-time
  warning naming the origin, and `grok inspect` lists the declared and the
  rejected entries under "Trusted xAI Origins (user-declared)".
- **Revocable.** Remove the entry and the gate closes again on the next model
  resolution; there is no cached trust.

**Private CA.** A self-hosted gateway usually means an internal certificate
authority. The declaration does not relax TLS verification — the gateway's
certificate must still validate. Point Grok at your CA with the existing
`GROK_EXTRA_CA_BUNDLE` environment variable (path to a PEM bundle); the bundle
is added to the verifier, not substituted for it.

### Context Window

The `context_window` value is the total the context bar shows and the basis for
auto-compaction. It is **local configuration**, not a live report from the
provider. Catalog models often inherit a window from the remote model list or
response metadata; a built-in preset such as OpenAI Codex's `gpt-5.6-sol`
ships a conservative figure in the binary, and that is what the bar displays
until you override it.

When you override a known model, Grok inherits that model's context window.
When you define a new model and omit `context_window`, Grok defaults to
200,000 tokens, so set it explicitly to match your provider.

If the context bar looks too low for a built-in model — or auto-compaction
fires earlier than the provider still accepts — set a metadata-only override
in the **global** config. Specifying only `context_window` keeps the preset's
routing and credential:

```toml
[model."gpt-5.6-sol"]
context_window = 400000
```

The `400000` above is an example override, not a claimed provider capacity.
Use a value accurate for the model and account you are calling. See
[OpenAI Codex](#openai-codex-chatgpt-subscription) for the same shape on the
Codex preset, and [Auto-Compaction Threshold](#auto-compaction-threshold) if
you only need to move the trigger point without changing the window.

### Global Default Headers

To apply the same headers to *every* model in the catalog -- built-in, prefetched from `/v1/models`, or custom -- set them once under the global `[models]` section instead of repeating them per model:

```toml
[models]
extra_headers = { "X-Request-Tags" = "team=example,env=prod" }
```

These act as a base for each model's inference requests. A per-model `[model.<id>].extra_headers` entry overrides the global default **per key** (matched case-insensitively): a key set on the model wins, while any global-only keys are still inherited by that model. Like the per-model field, they ride on that model's inference calls -- not on separate services such as image generation or video generation -- which makes them handy for attribution tags (for example, cost tracking) without re-declaring them whenever a new model appears.

### Global Default Values

A few common per-model settings can also be set once under `[models]` as a default for *every* model. A per-model `[model.<id>]` value always wins; the global only fills in where a model (or the server's model list) left the field unset:

```toml
[models]
temperature                 = 0.7
top_p                       = 0.95
max_completion_tokens       = 8192
max_retries                 = 8
inference_idle_timeout_secs = 600
stream_tool_calls           = true
```

This is a small, fixed set of environment-wide knobs. Settings that identify a specific model (`model`, `base_url`, `api_key`, `context_window`, ...) cannot be defaulted this way, and a few settings with their own dedicated configuration -- auto-compaction (`[session]` global default, which can also be set per-model or via env var), the system-prompt label (`[agent]`), and reasoning effort (per-model `reasoning_effort` under `[model.<id>]`; `[models].default_reasoning_effort`, despite its name, applies only to the model named by `[models].default` and is stamped after per-model overrides, so on that one model the global value wins) -- keep their existing homes. See [Reasoning Effort](#reasoning-effort) and [Auto-Compaction Threshold](#auto-compaction-threshold) below for details.

> **Note on `stream_tool_calls`:** this one affects request *shape*, not just sampling. A few endpoints (some BYOK providers) expect it left unset; if a global `stream_tool_calls = true` causes problems for such a model, opt that model out with `stream_tool_calls = false` in its `[model.<id>]` block.

### Request Query Parameters

Some gateways route or version on the query string. `query_params` appends percent-encoded query parameters to every request Grok makes for a model. For example, a gateway that selects an API version this way:

```toml
[model.my-gateway]
model = "my-model"
base_url = "https://gateway.example/v1"
api_backend = "responses"
env_key = "GATEWAY_API_KEY"
query_params = { api-version = "2026-07-22" }
```

A key that also appears in the `base_url` query string is overridden (last value wins) rather than duplicated. Query parameters are saved in the session, so do not put secrets in them: use `env_http_headers` for a secret.

### Environment-Variable Headers

`env_http_headers` maps a request header to the name of an environment variable that supplies its value, so a per-request secret never has to be written into `config.toml`:

```toml
[model.gateway]
model = "my-model"
base_url = "https://gateway.example/v1"
env_http_headers = { "X-Tenant-Token" = "GATEWAY_TENANT_TOKEN" }
```

Grok reads each variable when it builds the client for a session and places the value in the request headers only, never on disk. A header is skipped when its variable is unset or blank, and a resolved value overrides an `extra_headers` entry of the same name. Use `extra_headers` for a static value and `env_http_headers` for one that comes from the environment.

Both fields also work on a shared `[model_providers.<id>]` block. A model that points at a provider with `model_provider = "<id>"` inherits the provider's `query_params` and `env_http_headers` when it sets none of its own, matching how `extra_headers` is inherited.

### Reasoning Effort

For models that support reasoning effort levels, you can configure per-model parameters under `[model.<id>]`:

* `supports_reasoning_effort` (boolean): Explicitly declares whether the model supports reasoning effort.
  * For models using the **Messages (Anthropic-style)** backend, this is automatically enabled (`true`) and does not need to be declared.
  * For **OpenAI-compatible** endpoints (e.g. Chat Completions or Responses backends), you must explicitly set `supports_reasoning_effort = true`.
  * If a non-empty `reasoning_efforts` list is defined, support is automatically implied — this derive runs after per-model overrides merge, so the list turns support on even when `supports_reasoning_effort = false` was set explicitly.
* `reasoning_effort` (string): The default client/session reasoning tier. Backend-specific wire conversion happens when Grok builds a request.
  * Valid effort levels (lowercase) are: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`.
  * `ultra` is distinct from `max` in model selection, ACP state, and persisted sessions. It is selectable only when the model's catalog entry or custom `reasoning_efforts` menu advertises it; accepting the canonical string does not add it to a model's menu.
  * **Responses and Codex Responses:** `ultra` is encoded as `max`, the highest value supported by the typed Responses request. A response that reports `max` remains `max`; Grok does not infer that the server used the client-side `ultra` tier.
  * **Chat Completions:** `ultra` is sent literally. Only advertise it when the endpoint accepts that value.
  * **Messages (Anthropic-style):** `none` and `minimal` are not sent at all. Both map to an omitted effort, so the request carries no `output_config.effort` and no adaptive thinking — the same wire shape as leaving `reasoning_effort` unset. Selecting `minimal` therefore turns the field off. The other levels, including `ultra`, are sent literally, so only advertise values the endpoint accepts.
  * For an authenticated Codex session using `ultra`, Grok adds proactive multi-agent guidance. This guidance tells the model to consider subagents when the available tools and task make delegation useful; the model still decides whether to spawn, and may complete a turn without spawning. Tool filtering, permissions, and agent-profile capabilities still apply.
  * If this is omitted but `reasoning_efforts` is provided, Grok will automatically derive a default reasoning effort (preferring the option flagged as `default = true`, or falling back to the first defined option).
* `reasoning_efforts` (array of strings or tables): Declares the list of reasoning effort levels selectable in the UI. Each entry can be:
  * A bare string corresponding to the effort level, e.g. `"high"`.
  * A table specifying details:
    * `value` (string, required): The canonical effort level, e.g. `"high"`.
    * `id` (string, optional): Presentation ID (defaults to the lowercase value).
    * `label` (string, optional): Display label (defaults to the capitalized ID, e.g. `"High"`).
    * `description` (string, optional): A short description of the effort level.
    * `default` (boolean, optional): Whether this is the default effort level.

#### Worked Example: OpenAI-compatible Provider (e.g. OpenAI o3-mini)

```toml
[model.my-reasoning-model]
model = "o3-mini"
base_url = "https://api.openai.com/v1"
env_key = "OPENAI_API_KEY"
supports_reasoning_effort = true
reasoning_effort = "medium"
reasoning_efforts = [
    { value = "low", label = "Low Effort" },
    { value = "medium", label = "Medium Effort", default = true },
    { value = "high", label = "High Effort" }
]
```

#### Worked Example: Messages Backend (Anthropic-style)

Anthropic Messages backend models automatically enable `supports_reasoning_effort = true`:

```toml
[model.my-claude-reasoning]
model = "claude-3-7-sonnet"
base_url = "https://api.anthropic.com/v1"
api_backend = "messages"
auth_scheme = "x_api_key"
env_key = "ANTHROPIC_API_KEY"
reasoning_efforts = ["low", "medium", "high"]
```

#### Web Search and Backend Search

* `supports_backend_search` (boolean): For custom models that support server-side search, set this to `true` (e.g. `supports_backend_search = true`). This tells the shell to allow server-side web search for this model. Backend search is on by default; disable it globally with `[features].backend_tools = false` or the `GROK_BACKEND_SEARCH` environment variable. Hosted tools are only put on the wire by the Responses-family backends — Chat Completions and Messages requests never carry them, and the Codex backend excludes them — so set this only on a model using `api_backend = "responses"`. On any other backend the flag drops the local `web_search` tool without gaining server-side search.

> [!NOTE]
> For keyless local models configured with `auth_scheme = "none"`, the behavior of the `web_search` tool is currently an **open product question** (see issue [#178](https://github.com/ImL1s/medley/issues/178)). Do not assume the local web search tool is fully integrated or settled for these routes.

### Auto-Compaction Threshold

Auto-compaction keeps your prompt history clean by summarizing or shrinking context when it fills up. The point at which compaction triggers is controlled by the auto-compaction threshold (expressed as a percentage of the context window, default is `85`). 

This value can be configured at multiple levels. Grok resolves the active threshold using the following six-tier precedence (highest priority first):

1. **Environment Variable**: `GROK_AUTO_COMPACT_THRESHOLD_PERCENT` (sets the threshold per-process).
2. **Per-Model TOML Override**: `auto_compact_threshold_percent` in the model section, e.g. `[model.<id>] auto_compact_threshold_percent = 70`.
3. **Session Global TOML**: `auto_compact_threshold_percent` in the global `[session]` section, e.g. `[session] auto_compact_threshold_percent = 80`.
4. **Remote Settings Per-Model**: The default threshold set by the model provider’s catalog.
5. **Remote Settings Global**: The global default threshold set by the remote environment.
6. **Built-in Fallback**: The default value of `85`%.

Example TOML configuration:

```toml
[session]
auto_compact_threshold_percent = 80  # Global fallback for all models

[model.my-model]
auto_compact_threshold_percent = 70  # Override specifically for my-model
```

---

## Overriding Built-in Models

You can override specific fields of built-in models without redefining everything. Only specify the fields you want to change:

```toml
# Override only the API key for a default model
[model.grok-build]
api_key = "my-api-key"

# Override temperature and add a custom API key
[model.grok-build]
temperature = 0.5
api_key = "my-custom-key"
```

When you override a built-in model, Grok starts with the default configuration (including the correct `base_url`), then applies only the fields you specify. Unspecified fields inherit from the default.

### Priority Order

1. Your config (`[model.*]`) -- highest priority
2. Prefetched models from remote `/v1/models`
3. Hardcoded defaults -- lowest priority

---

## Provider Examples

### Anthropic (Claude)

Use Claude models directly via the Anthropic Messages API:

<!-- medley-doc-test:toml:provider-anthropic-claude -->
```toml
[model.claude-opus]
model = "claude-opus-4-6"
base_url = "https://api.anthropic.com/v1"
name = "Claude Opus 4.6"
api_backend = "messages"
auth_scheme = "x_api_key"
env_key = "ANTHROPIC_API_KEY"
extra_headers = { "anthropic-version" = "2023-06-01" }
context_window = 200000
```

The `messages` backend uses the Anthropic Messages protocol. Set `auth_scheme = "x_api_key"` so Grok sends the resolved key as `x-api-key` rather than `Authorization: Bearer`. Keep non-secret version headers in `extra_headers`; do not put the API key there.

### OpenAI (Chat Completions)

<!-- medley-doc-test:toml:provider-openai-chat -->
```toml
[model.gpt-4o]
model = "gpt-4o"
base_url = "https://api.openai.com/v1"
name = "GPT-4o"
env_key = "OPENAI_API_KEY"
```

`api_backend` defaults to `"chat_completions"` and `auth_scheme` defaults to `"bearer"`, so you don't need to set either explicitly for OpenAI.

### OpenAI (Responses API)

If your provider supports the newer Responses API:

<!-- medley-doc-test:toml:provider-openai-responses -->
```toml
[model.gpt-4o-responses]
model = "gpt-4o"
base_url = "https://api.openai.com/v1"
name = "GPT-4o (Responses)"
api_backend = "responses"
env_key = "OPENAI_API_KEY"
```

### OpenAI Codex (ChatGPT subscription)

Grok ships a built-in Codex model, so signing in is the only required step:

```bash
grok login --provider openai-codex
```

The preset is `gpt-5.6-sol`. It is present in `grok models` and the `/model`
picker from a fresh install, listed as unready with the reason `sign in with
grok login --provider openai-codex` until the credential above exists, and
selectable once it does. Nothing has to be written to `config.toml` for it.

The context bar's total for this preset is a **local default** shipped with
the binary, not a figure ChatGPT reported on the wire.
Auto-compaction uses the same number, so an under-reported window compacts
early and throws away context the provider would still have accepted. When the
bar looks wrong, raise it with a metadata-only override in the **global**
config — see [Context Window](#context-window):

```toml
[model."gpt-5.6-sol"]
context_window = 400000 # example only; use a value accurate for your account
```

To retune other metadata (display name, and so on), declare the same catalog
key the same way; it edits the preset in place rather than adding a second
entry:

```toml
[model."gpt-5.6-sol"]
name = "OpenAI Codex"
```

An override like that names no endpoint and no credential, so it keeps the
preset's Codex routing. Declaring your own `base_url`, `env_key`, `api_key`,
`api_base_url`, `auth_provider`, or `model_provider` instead takes the key over
completely — useful if you want `gpt-5.6-sol` to mean your own OpenAI Platform
model, but it is then no longer a Codex entry and the ChatGPT credential does
not apply to it.

To add a *different* Codex model, give it its own key and name the provider:

<!-- medley-doc-test:toml:provider-openai-codex-secondary -->
```toml
[model.my-other-codex-model]
model = "<the wire id>"
model_provider = "openai-codex"
name = "OpenAI Codex"
context_window = 200000 # example only; use metadata accurate for the model
```

The `openai-codex` provider is compatibility code for pinned public Codex
source snapshot
[`2b5bdcf67547860f2e5c5a605009a70026796b2b`](https://github.com/openai/codex/tree/2b5bdcf67547860f2e5c5a605009a70026796b2b);
it is not an OpenAI endorsement or a promise that the ChatGPT backend is a
stable public API. The OAuth consent page identifies the registered Codex
public client, and the user's account/workspace policy and applicable OpenAI
terms govern use.

The provider fixes the transport to
`https://chatgpt.com/backend-api/codex/responses` and supplies the live bearer
and trusted ChatGPT workspace-routing metadata from one provider-scoped
credential snapshot. Medley sends its own truthful transport identity and
does not impersonate the official Codex CLI. It rejects custom origins, query
parameters, arbitrary routing headers, and attempts to override the reserved
provider. Do not add `api_key`, `env_key`, `base_url`, `extra_headers`, or
`api_backend` to a Codex-backed model; use `OPENAI_API_KEY` with the normal
OpenAI examples above for the separate OpenAI Platform API transport.

### Gemini (OpenAI-compatible)

Google's OpenAI-compatible endpoint uses Bearer auth:

<!-- medley-doc-test:toml:provider-gemini -->
```toml
[model.gemini-flash]
model = "gemini-2.0-flash"
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
name = "Gemini 2.0 Flash"
env_key = "GEMINI_API_KEY"
```

### OpenRouter

<!-- medley-doc-test:toml:provider-openrouter -->
```toml
[model.openrouter-llama]
model = "meta-llama/llama-3.3-70b-instruct"
base_url = "https://openrouter.ai/api/v1"
name = "Llama 3.3 70B (OpenRouter)"
env_key = "OPENROUTER_API_KEY"
extra_headers = { "HTTP-Referer" = "https://example.com", "X-Title" = "My App" }
```

Optional attribution headers (`HTTP-Referer`, `X-Title`) are non-secret; put the key in `env_key`, not in `extra_headers`.

### Together AI

<!-- medley-doc-test:toml:provider-together -->
```toml
[model.together-mixtral]
model = "mistralai/Mixtral-8x7B-Instruct-v0.1"
base_url = "https://api.together.xyz/v1"
name = "Mixtral 8x7B"
env_key = "TOGETHER_API_KEY"
```

### Generic hosted OpenAI-compatible

Any hosted OpenAI-compatible Chat Completions endpoint:

<!-- medley-doc-test:toml:provider-hosted-generic -->
```toml
[model.hosted-custom]
model = "provider-model-id"
base_url = "https://api.example.com/v1"
name = "Hosted Custom"
env_key = "PROVIDER_API_KEY"
```

### Local models (`auth_scheme = "none"`)

Keyless local servers need an explicit `auth_scheme = "none"`. Without it the entry declares Bearer auth with no credential to satisfy it, and is marked **unready** — ambient xAI credentials are never attached to a local endpoint.

Tools, reasoning, images, and structured output depend on what the local server and model support; Grok does not invent capabilities the backend lacks.

#### Ollama

<!-- medley-doc-test:toml:provider-local-ollama -->
```toml
[model.ollama-codellama]
model = "codellama"
base_url = "http://localhost:11434/v1"
name = "CodeLlama (Ollama)"
auth_scheme = "none"
context_window = 16384
```

Make sure Ollama is running (`ollama serve`) and the model is pulled (`ollama pull codellama`).

#### LM Studio

<!-- medley-doc-test:toml:provider-local-lmstudio -->
```toml
[model.lmstudio-local]
model = "local-model"
base_url = "http://localhost:1234/v1"
name = "LM Studio"
auth_scheme = "none"
context_window = 32768
```

#### llama.cpp

<!-- medley-doc-test:toml:provider-local-llamacpp -->
```toml
[model.llamacpp]
model = "local-model"
base_url = "http://localhost:8080/v1"
name = "llama.cpp"
auth_scheme = "none"
context_window = 8192
```

#### vLLM

<!-- medley-doc-test:toml:provider-local-vllm -->
```toml
[model.vllm-local]
model = "meta-llama/Llama-3.1-8B-Instruct"
base_url = "http://localhost:8000/v1"
name = "vLLM"
auth_scheme = "none"
context_window = 128000
```

---

## Custom Models Endpoint

Point Grok at a custom OpenAI-compatible `/v1/models` endpoint instead of the default. Use this when your models sit behind a corporate gateway or a self-hosted inference service.

### Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `GROK_MODELS_BASE_URL` | Yes | Base URL for inference. Grok fetches the model list from `{base_url}/models` ([config.rs:715](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/agent/config.rs#L715)). |
| `XAI_API_KEY` | Yes | API key sent as `Authorization: Bearer` ([auth_method.rs:36](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/agent/auth_method.rs#L36)). Grok also accepts `GROK_CODE_XAI_API_KEY`. |
| `GROK_MODELS_LIST_URL` | No | Override the model-list URL when it differs from `{base_url}/models`. Set the final URL; catalog redirects are rejected ([config.rs:716](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/agent/config.rs#L716)). |

### Setup

```bash
export GROK_MODELS_BASE_URL="https://api.acme.com/v1"
export XAI_API_KEY="your-api-key"
grok
```

### Config File Alternative

```toml
[endpoints]
models_base_url = "https://api.acme.com/v1"

# Override only the API key for a specific model
[model.grok-build]
api_key = "my-api-key"
```

When you use `[endpoints]` with partial model overrides, Grok inherits the `base_url` from the endpoints config, so you do not need to specify it in each `[model.*]` section.

### Custom Catalog Discovery Authentication

Grok supports configuring an explicit authentication scheme, custom headers, and custom timeout for remote catalog discovery under the global `[models]` section:

<!-- medley-doc-test:toml:models-catalog-auth -->
```toml
[models]
endpoint = "https://api.acme.com/v1/models"
catalog_auth_scheme = "bearer"                  # "bearer", "x_api_key", or "none"
catalog_env_key = "MY_COMPANY_CATALOG_KEY"      # Environment variable supplying the key
catalog_headers = { "X-Organization" = "Acme" } # Custom request headers (optional)
catalog_timeout_secs = 15                       # Request timeout in seconds (optional)
```

The fields are processed as follows:
- `endpoint` ([config.rs:1303](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/agent/config.rs#L1303)): Custom URL to fetch the model list from.
- `catalog_auth_scheme` ([config.rs:1304](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/agent/config.rs#L1304)): The authentication scheme used. Supported values are `"bearer"`, `"x_api_key"`, and `"none"`.
- `catalog_env_key` ([config.rs:1305](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/agent/config.rs#L1305)): The name of the environment variable (or array of variables) holding the API key.
- `catalog_headers` ([config.rs:1307](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/agent/config.rs#L1307)): Custom headers sent during discovery. Case-insensitive protected headers (`authorization`, `x-api-key`, `host`, and xAI client headers) cannot be overridden ([client.rs:917](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/remote/client.rs#L917), [client.rs:859-866](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/remote/client.rs#L859-866)).
- `catalog_timeout_secs` ([config.rs:1310](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/agent/config.rs#L1310)): HTTP timeout for catalog requests.

The per-model `api_key` above authenticates inference requests only. Catalog discovery still requires `XAI_API_KEY` (or the legacy `GROK_CODE_XAI_API_KEY`) in the environment unless configured under `[models]` as shown above.

### Auth Behavior

When you set `models_base_url` or `models_list_url`, Grok uses an explicit API key (`Authorization: Bearer`) instead of session auth. A `grok login` credential never authenticates a custom catalog, so set `XAI_API_KEY` (or its legacy alias) separately unless using the custom catalog config above. Catalog redirects are rejected so the API key cannot be forwarded to a redirect target; configure `GROK_MODELS_LIST_URL` or `[models].endpoint` with the final URL.

---

## Web Search Model

The `web_search` tool uses a separate model. Configure it with:

```toml
[models]
web_search = "grok-4.5"
```

Or via environment variable:

```bash
export GROK_WEB_SEARCH_MODEL="grok-4.5"
```

If you point web search at a custom model, you also need a `[model.*]` entry so Grok can reach it. Server-side ("backend") web search runs only when the model sets `supports_backend_search = true` and backend search is enabled (it is by default; `[features].backend_tools` / `GROK_BACKEND_SEARCH` control it), and only on the Responses-family backends — see [Web Search and Backend Search](#web-search-and-backend-search):

```toml
[models]
web_search = "my-custom-model"

[model.my-custom-model]
model = "my-custom-model"
base_url = "https://api.example.com/v1"
api_backend = "responses"        # hosted tools only ride the Responses-family wire format
env_key = "PROVIDER_API_KEY"
supports_backend_search = true
```

> [!IMPORTANT]
> **Credential Override Protection.** If the custom `web_search` model route has explicitly declared auth headers in its `extra_headers` or `env_http_headers` (verified via [`has_declared_credential_header`](file:///private/tmp/n193b/crates/codegen/xai-grok-tools/src/implementations/web_search/client.rs#L47)), an ambient xAI session token or credentials will **never** override or overwrite the route's custom headers (shipped in PR #181, verified at [`client.rs:282`](file:///private/tmp/n193b/crates/codegen/xai-grok-tools/src/implementations/web_search/client.rs#L282)).

---

## Using Custom Models

```bash
# List available models (including custom)
grok models

# Use in the TUI via slash command
/model my-model

# Use in headless mode
grok -p "Hello" -m my-model

# Set as default in config.toml:
[models]
default = "my-model"
```

---

## Enterprise Deployment

A complete config for an enterprise deployment with custom models:

```toml
[cli]
auto_update = false

[auth]
auth_provider_command = "/usr/local/bin/my-company-auth-provider"
auth_provider_label = "Acme Corp"
auth_token_ttl = 3600

[models]
default = "company-grok"

[model.company-grok]
model = "grok-build"
base_url = "https://grok-proxy.acme.com/"
name = "Grok Build Latest (Proxy)"
context_window = 128000

[features]
telemetry = false
```

---

## Troubleshooting

### Context bar shows 200K for Codex / auto-compact fires early

Grok attempts to fetch the live models list from the OpenAI Codex service online ([`fetch_openai_codex_catalog_models`](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/agent/model_providers.rs#L202-244)). If the fetch fails (due to network issues or missing login credentials), it falls back to the built-in `gpt-5.6-sol` preset ([`openai_codex_preset_models`](file:///private/tmp/n193b/crates/codegen/xai-grok-shell/src/agent/model_providers.rs#L274-286)). This preset uses a local default context window of 200,000. You can override it in the global config — see [Context Window](#context-window) — or adjust the compaction trigger using [Auto-Compaction Threshold](#auto-compaction-threshold).

### Model Not Found

```bash
# List available models
grok models

# Check config.toml for typos in [model.*] sections
```

### Connection Errors

Verify the endpoint is reachable:

```bash
curl -s https://api.example.com/v1/models \
  -H "Authorization: Bearer $XAI_API_KEY"
```

### Debug Logging

```bash
RUST_LOG=debug GROK_LOG_FILE=/tmp/grok.log grok
tail -f /tmp/grok.log
```

Look for log entries containing `model` or `sampling` to trace model selection and API calls.
