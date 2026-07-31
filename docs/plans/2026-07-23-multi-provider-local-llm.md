# Multi-provider credentials & keyless local LLMs Implementation Plan

> **For Codex:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Let `[model.*]` users select Bearer / `x_api_key` / explicit `none` auth so hosted provider keys and keyless local OpenAI-compatible servers work without leaking ambient xAI credentials or forcing interactive login.

**Architecture:** Extend the existing `AuthScheme` enum (single transport contract) with `None`, expose it on `ConfigModelOverride`, short-circuit credential resolution for `none`, harden `SamplingClient` so no auth header is emitted even if a key/resolver is present, and advertise a non-interactive ACP auth method only when the **startup-selected** model is `auth_scheme = "none"`. No vendor SDKs.

**Tech Stack:** Rust workspace (`xai-grok-sampler`, `xai-grok-shell`), TOML config overrides, ACP auth methods, Axum mock wire tests, existing `cargo test` / `clippy` gates.

**Source of truth (already drafted, do not re-litigate):**
- `.omx/plans/multi-provider-local-llm-prd.md`
- `.omx/plans/multi-provider-local-llm-test-spec.md`
- `.omx/specs/multi-provider-local-llm-spec.md`

**Hard constraints:**
- Do **not** modify or commit untracked `.omc/` or `QWEN.md`.
- Do **not** invent OMX Architect/Critic approvals; this plan is for direct Cursor/Codex execution.
- Never force-push; never reset `main`. Sync is fast-forward only.
- No new crate dependencies.

---

### Task 0: Sync main and open the feature branch

**Files:**
- None (git only)

**Step 1: Confirm clean tracked tree**

Run:

```bash
cd /Users/iml1s/Documents/mine/grok-build
git status -sb
git diff --stat HEAD
```

Expected: branch `main`, no modified tracked files. Untracked `.omc/`, `.omx/`, `QWEN.md`, and this plan under `docs/plans/` are OK if left unstaged during sync.

**Step 2: Fetch and fast-forward**

Run:

```bash
git fetch upstream
git fetch origin
git merge --ff-only upstream/main
git push origin main
git rev-parse upstream/main HEAD origin/main
```

Expected: all three SHAs identical. If ff-only fails, **stop** and report; do not reset.

**Step 3: Create feature branch**

Run:

```bash
git switch -c feat/multi-provider-local-llm
```

**Step 4: Re-check the three gaps still exist at the tip**

Confirm still true (else revise this plan before coding):

1. `ConfigModelOverride` in `crates/codegen/xai-grok-shell/src/agent/config.rs` has **no** `auth_scheme` field.
2. `AuthScheme` in `crates/codegen/xai-grok-sampler/src/config.rs` is only `Bearer | XApiKey`.
3. `resolve_aux_model_sampling_config` still returns early only when `sampler.api_key.is_some()`.

**Step 5: Commit plan (optional if already on disk)**

```bash
git add docs/plans/2026-07-23-multi-provider-local-llm.md
git commit -m "$(cat <<'EOF'
docs: add multi-provider local LLM implementation plan

EOF
)"
```

---

### Task 1: Failing sampler tests for `AuthScheme::None`

**Files:**
- Modify: `crates/codegen/xai-grok-sampler/src/client.rs` (unit tests near existing auth tests ~2082+)
- Modify (later): `crates/codegen/xai-grok-sampler/src/config.rs`
- Modify (later): `crates/codegen/xai-grok-sampler/src/client.rs` (production match arms)

**Step 1: Write the failing tests**

Add beside `messages_plus_anthropic_api_key_uses_x_api_key_and_not_authorization`:

```rust
#[test]
fn none_scheme_emits_no_auth_headers_even_with_api_key() {
    let cfg = SamplerConfig {
        api_key: Some("should-not-leak".to_string()),
        auth_scheme: AuthScheme::None,
        ..minimal_config()
    };
    let client = SamplingClient::new(cfg).expect("client should build");
    assert!(client.default_headers.get(AUTHORIZATION).is_none());
    assert!(
        client
            .default_headers
            .get(HeaderName::from_static("x-api-key"))
            .is_none()
    );
}

#[test]
fn none_scheme_auth_info_reports_absence_without_fragment() {
    let cfg = SamplerConfig {
        api_key: Some("should-not-leak".to_string()),
        auth_scheme: AuthScheme::None,
        ..minimal_config()
    };
    let client = SamplingClient::new(cfg).expect("client should build");
    let info = client.auth_info();
    assert_eq!(info.auth_type, "none");
    assert!(!info.auth_present);
}

#[test]
fn none_scheme_post_ignores_bearer_resolver() {
    #[derive(Debug)]
    struct LeakResolver;
    impl crate::config::BearerResolver for LeakResolver {
        fn current_bearer(&self) -> Option<String> {
            Some("live-should-not-leak".into())
        }
    }
    let mut cfg = SamplerConfig {
        api_key: Some("stale-should-not-leak".to_string()),
        auth_scheme: AuthScheme::None,
        ..minimal_config()
    };
    cfg.bearer_resolver = Some(std::sync::Arc::new(LeakResolver));
    let client = SamplingClient::new(cfg).expect("client should build");
    let req = client
        .post("http://localhost/test")
        .build()
        .expect("build request");
    assert!(req.headers().get(AUTHORIZATION).is_none());
    assert!(
        req.headers()
            .get(HeaderName::from_static("x-api-key"))
            .is_none()
    );
}
```

**Step 2: Run tests to verify they fail**

Run:

```bash
cargo test --manifest-path crates/codegen/xai-grok-sampler/Cargo.toml \
  none_scheme_ -- --nocapture
```

Expected: FAIL — `AuthScheme::None` does not exist / non-exhaustive match.

**Step 3: Implement minimal `AuthScheme::None` + client arms**

In `crates/codegen/xai-grok-sampler/src/config.rs`:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    #[default]
    Bearer,
    XApiKey,
    None,
}
```

In `SamplingClient::new` header construction, add:

```rust
AuthScheme::None => {
    // Explicit no-auth: never emit Authorization / x-api-key from api_key.
}
```

In `post()` resolver override match, add:

```rust
AuthScheme::None => {
    headers.remove(AUTHORIZATION);
    headers.remove(HeaderName::from_static("x-api-key"));
}
```

In `extract_sent_bearer`:

```rust
AuthScheme::None => None,
```

In `auth_info`:

```rust
let auth_present = self.current_credential_present();
let auth_type = match (&self.defaults.auth_scheme, auth_present) {
    (AuthScheme::None, _) => "none",
    (AuthScheme::XApiKey, true) => "x-api-key",
    (AuthScheme::Bearer, true) => "bearer",
    (_, false) => "none",
};
```

Record only `auth_present` and the closed `auth_type` value. Never derive or
log a credential prefix, suffix, or other substring. Keep existing Bearer /
XApiKey request behavior unchanged.

**Step 4: Re-run tests**

```bash
cargo test --manifest-path crates/codegen/xai-grok-sampler/Cargo.toml \
  none_scheme_ -- --nocapture
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/codegen/xai-grok-sampler/src/config.rs \
        crates/codegen/xai-grok-sampler/src/client.rs
git commit -m "$(cat <<'EOF'
feat(sampler): add AuthScheme::None with no auth headers

EOF
)"
```

---

### Task 2: Expose `auth_scheme` on `ConfigModelOverride` (TDD)

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (`ConfigModelOverride`, `apply`)
- Modify: `crates/codegen/xai-grok-shell/src/agent/config_model_override_parse.rs` (`fully_populated_override` + new tests)

**Step 1: Write failing parse/apply tests**

In `config_model_override_parse.rs` tests:

```rust
#[test]
fn auth_scheme_x_api_key_parses_and_applies() {
    let mut entry = toml::map::Map::new();
    entry.insert(
        "auth_scheme".into(),
        toml::Value::String("x_api_key".into()),
    );
    entry.insert("context_window".into(), toml::Value::Integer(200_000));
    let (models, warnings) = parse_single_entry(entry);
    assert!(warnings.is_empty());
    let over = models.get("m").expect("model m");
    assert_eq!(over.auth_scheme, Some(AuthScheme::XApiKey));
}

#[test]
fn auth_scheme_none_parses() {
    let mut entry = toml::map::Map::new();
    entry.insert("auth_scheme".into(), toml::Value::String("none".into()));
    let (models, warnings) = parse_single_entry(entry);
    assert!(warnings.is_empty());
    assert_eq!(models.get("m").unwrap().auth_scheme, Some(AuthScheme::None));
}

#[test]
fn invalid_auth_scheme_warns_and_keeps_entry() {
    let mut entry = toml::map::Map::new();
    entry.insert("model".into(), toml::Value::String("kept".into()));
    entry.insert(
        "auth_scheme".into(),
        toml::Value::String("not-a-scheme".into()),
    );
    let (models, warnings) = parse_single_entry(entry);
    let over = models.get("m").expect("model retained");
    assert_eq!(over.model.as_deref(), Some("kept"));
    assert!(over.auth_scheme.is_none());
    assert!(
        warnings
            .iter()
            .any(|w| w.kind == ModelOverrideWarningKind::InvalidValue
                && w.field.as_deref() == Some("auth_scheme"))
    );
}
```

Import `AuthScheme` / `ModelOverrideWarningKind` as needed.

Also extend `fully_populated_override()` with:

```rust
auth_scheme: Some(AuthScheme::XApiKey),
```

In `config.rs` tests (near existing `auth_scheme` credential tests ~5880), add:

```rust
#[test]
fn config_model_override_applies_auth_scheme() {
    let endpoints = EndpointsConfig::default(); // use whatever helper nearby tests use
    let over = ConfigModelOverride {
        auth_scheme: Some(AuthScheme::None),
        ..ConfigModelOverride::default()
    };
    let entry = over.apply("local", None, &endpoints);
    assert_eq!(entry.info.auth_scheme, AuthScheme::None);
}
```

Mirror the exact `EndpointsConfig` construction pattern used by nearby unit tests in the same module (do not invent a new helper).

**Step 2: Run to verify failure**

```bash
cargo test --manifest-path crates/codegen/xai-grok-shell/Cargo.toml \
  auth_scheme_ -- --nocapture
```

Expected: FAIL — missing field / compile error in `fully_populated_override`.

**Step 3: Minimal implementation**

Add to `ConfigModelOverride`:

```rust
pub auth_scheme: Option<AuthScheme>,
```

In `ConfigModelOverride::apply`, after `api_backend` apply:

```rust
if let Some(v) = self.auth_scheme {
    entry.info.auth_scheme = v;
}
```

Do **not** invent aliases unless serde/docs already require one.

**Step 4: Re-run**

```bash
cargo test --manifest-path crates/codegen/xai-grok-shell/Cargo.toml \
  auth_scheme_ fully_populated_override_round_trips -- --nocapture
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/config.rs \
        crates/codegen/xai-grok-shell/src/agent/config_model_override_parse.rs
git commit -m "$(cat <<'EOF'
feat(config): allow [model.*] auth_scheme overrides

EOF
)"
```

---

### Task 3: Credential resolution short-circuit for `none` (TDD)

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (`resolve_credentials`, `resolve_aux_model_sampling_config`, tests)

**Step 1: Write failing credential tests**

Near existing `resolve_credentials` tests (~5626+):

```rust
#[test]
fn none_auth_scheme_ignores_model_env_session_and_global_keys() {
    // SAFETY: serial_test / existing env patterns in this module — follow
    // the same env-guard style as neighboring tests.
    let mut model = /* build ModelEntry with:
         auth_scheme: AuthScheme::None,
         api_key: Some("model-key"),
         env_key: Some(EnvKeys::single("OPENAI_API_KEY")),
         base_url: "http://127.0.0.1:11434/v1"
    */;
    std::env::set_var("OPENAI_API_KEY", "env-key");
    std::env::set_var("XAI_API_KEY", "xai-key");
    let creds = resolve_credentials(&model, Some("session-jwt"));
    assert!(creds.api_key.is_none());
    assert_eq!(creds.auth_scheme, AuthScheme::None);
    assert_eq!(creds.base_url, "http://127.0.0.1:11434/v1");
    // clean up env vars the same way sibling tests do
}

#[test]
fn none_aux_model_resolves_without_api_key() {
    let mut models = IndexMap::new();
    // insert a catalog entry with auth_scheme None, no api_key/env_key
    let cfg = resolve_aux_model_sampling_config(
        "local",
        &models,
        &endpoints,
        Some("session-jwt"),
        false,
        None,
        None,
    );
    let sampler = cfg.expect("no-auth aux must resolve");
    assert!(sampler.api_key.is_none());
    assert_eq!(sampler.auth_scheme, AuthScheme::None);
}
```

Copy concrete `ModelEntry` / `EndpointsConfig` scaffolding from adjacent tests; do not invent new factories.

**Step 2: Run — expect FAIL**

```bash
cargo test --manifest-path crates/codegen/xai-grok-shell/Cargo.toml \
  none_auth_scheme_ignores none_aux_model -- --nocapture
```

Expected: FAIL — ambient keys still win / aux returns `None`.

**Step 3: Implement**

At the top of `resolve_credentials`:

```rust
pub fn resolve_credentials(model: &ModelEntry, session_key: Option<&str>) -> ResolvedCredentials {
    let info = model.info();
    if info.auth_scheme == AuthScheme::None {
        return ResolvedCredentials {
            api_key: None,
            base_url: info.base_url.clone(),
            auth_type: xai_chat_state::AuthType::ApiKey,
            auth_scheme: AuthScheme::None,
        };
    }
    // existing precedence unchanged...
}
```

In `resolve_aux_model_sampling_config`, change the accept condition from only `api_key.is_some()` to:

```rust
if sampler.api_key.is_some() || sampler.auth_scheme == AuthScheme::None {
    return Some(sampler);
}
```

Also harden `prepare_sampling_config_for_model` in `mvp_agent/agent_ops.rs`: when `model.info.auth_scheme == AuthScheme::None`, **do not** rewrite `auth_type` to `SessionToken` and **do not** attach session keys. Keep the change minimal and covered by a unit/integration assertion if one already exists nearby; otherwise cover via Task 4 auth-method tests + a focused `resolve_credentials` call through `prepare_sampling_config` if easily reachable.

**Step 4: Re-run**

```bash
cargo test --manifest-path crates/codegen/xai-grok-shell/Cargo.toml \
  none_auth_scheme_ignores none_aux_model -- --nocapture
```

Expected: PASS. Also re-run existing auth_scheme Bearer/XApiKey tests.

**Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/config.rs \
        crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs
git commit -m "$(cat <<'EOF'
fix(auth): isolate AuthScheme::None from ambient credentials

EOF
)"
```

---

### Task 4: ACP non-interactive no-auth method for selected local model (TDD)

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/auth_method.rs`
- Modify: `crates/codegen/xai-grok-shell/src/agent/mvp_agent/acp_agent.rs` (`initialize` auth method build inputs)
- Possibly: authenticate handler in `mvp_agent` (no-op success for the new method id)

**Design (lock this):**
- New method id: `local.none`
- Advertise + default-select it **only** when the **startup default / selected** model has `auth_scheme == None`
- Presence of some other catalog no-auth model must **not** suppress login for a selected xAI model
- `[auth] preferred_method = api_key|oidc` remains fail-closed (do not silently pick `local.none`)
- `AuthMethodKind::LocalNone`: not session-based, not interactive
- `session_token_auth_gate` must stay false for this method (already true if method is non-session-based)

**Step 1: Write failing unit tests in `auth_method.rs`**

```rust
#[test]
fn selected_no_auth_model_advertises_local_none_first_when_unpinned() {
    let inputs = AuthMethodsBuildInputs {
        has_external_api_key: false,
        has_cached_token: false,
        has_enterprise_oidc: false,
        enterprise_oidc_issuer: None,
        login_label: None,
        has_auth_provider_command: false,
        preferred_method: None,
        selected_model_is_no_auth: true,
    };
    let built = build_auth_methods(inputs);
    assert_eq!(method_ids(&built).first().copied(), Some(LOCAL_NONE_METHOD_ID));
    assert_eq!(default_id(&built), Some(LOCAL_NONE_METHOD_ID));
}

#[test]
fn non_selected_no_auth_does_not_change_xai_ordering() {
    let inputs = AuthMethodsBuildInputs {
        has_external_api_key: true,
        has_cached_token: true,
        preferred_method: None,
        selected_model_is_no_auth: false,
        ..default_inputs() // extend default_inputs if needed
    };
    let built = build_auth_methods(inputs);
    assert_eq!(method_ids(&built).first().copied(), Some(XAI_API_KEY_METHOD_ID));
}

#[test]
fn preferred_api_key_pin_does_not_fall_through_to_local_none() {
    let inputs = AuthMethodsBuildInputs {
        has_external_api_key: false,
        preferred_method: Some(PreferredAuthMethod::ApiKey),
        selected_model_is_no_auth: true,
        ..default_inputs()
    };
    let built = build_auth_methods(inputs);
    assert!(built.methods.is_empty());
    assert!(built.default_auth_method_id.is_none());
}
```

Adapt to the file's existing `method_ids` / `default_id` / `default_inputs` helpers (extend `default_inputs` with `selected_model_is_no_auth: false`).

**Step 2: Run — expect FAIL**

```bash
cargo test --manifest-path crates/codegen/xai-grok-shell/Cargo.toml \
  agent::auth_method -- --nocapture
```

Expected: FAIL — missing field / unknown method.

**Step 3: Implement**

1. Add constants + constructor:

```rust
pub const LOCAL_NONE_METHOD_ID: &str = "local.none";
pub fn local_none_auth_method() -> acp::AuthMethod {
    acp::AuthMethod::Agent(
        acp::AuthMethodAgent::new(
            acp::AuthMethodId::new(LOCAL_NONE_METHOD_ID),
            "local.none".to_string(),
        )
        .description(Some(
            "No credentials (auth_scheme = none on the selected model)".into(),
        )),
    )
}
```

2. Extend `AuthMethodKind` with `LocalNone` (not session-based, not interactive).

3. Extend `AuthMethodsBuildInputs` with `selected_model_is_no_auth: bool`.

4. In unpinned builder: if `selected_model_is_no_auth`, push `local.none` **first** and set it as `default_auth_method_id` (unless preferred pin forbids it — pinned paths unchanged / fail-closed).

5. In `MvpAgent::initialize` (`acp_agent.rs`), compute:

```rust
let selected_model_is_no_auth = /* resolve default/startup model entry */
    .map(|e| e.info.auth_scheme == AuthScheme::None)
    .unwrap_or(false);
```

Use the same model list already loaded for `should_advertise_xai_api_key` / default model resolution in that function — do not load config a second unnecessary time if avoidable.

6. Authenticate path: selecting `local.none` must succeed without reading/storing a key (mirror the lightweight success path used for `xai.api_key` when a key is present, but with empty credentials). Grep `authenticate` / `XAI_API_KEY_METHOD_ID` in `mvp_agent` and add a branch.

**Step 4: Re-run**

```bash
cargo test --manifest-path crates/codegen/xai-grok-shell/Cargo.toml \
  agent::auth_method -- --nocapture
```

Expected: PASS. Fix any `AuthMethodsBuildInputs { ... }` compile breaks across the crate.

**Step 5: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/agent/auth_method.rs \
        crates/codegen/xai-grok-shell/src/agent/mvp_agent/acp_agent.rs \
        crates/codegen/xai-grok-shell/src/agent/mvp_agent/*.rs
git commit -m "$(cat <<'EOF'
feat(auth): advertise local.none for selected no-auth models

EOF
)"
```

---

### Task 5: Session token gate + model auth facts for `none`

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs` (only if gate still activates for `AuthScheme::None`)
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (`resolve_model_auth_facts` / tests) if needed

**Step 1: Add a focused regression**

If `reconstruct_full_config` can still attach a bearer resolver when method is session-based but model scheme is `None`, add a unit-level assertion on `session_token_auth_gate` **or** force-disable resolver when `model_facts.auth_scheme == AuthScheme::None`:

```rust
let use_bearer_resolver =
    gate.active() && model_facts.auth_scheme != AuthScheme::None;
```

Prefer the explicit conjunction above — it is the security boundary against session-token leakage to local endpoints after a model switch.

**Step 2: Test**

Add a small pure unit test if one can live next to `session_token_auth_gate` tests; otherwise cover via existing session/auth test module patterns.

**Step 3: Commit**

```bash
git add crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs
git commit -m "$(cat <<'EOF'
fix(session): never attach session bearer resolver for AuthScheme::None

EOF
)"
```

---

### Task 6: Documentation — custom models guide

**Files:**
- Modify: `crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md`

**Step 1: Update Anthropic example to prefer `env_key` + `auth_scheme`**

Replace secret-bearing `extra_headers = { "x-api-key" = "sk-ant-..." }` primary example with:

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

Keep a short note that non-secret version headers still belong in `extra_headers`.

**Step 2: Add / refresh hosted examples**

Ensure copyable blocks exist for:

- OpenAI Chat Completions (`env_key = "OPENAI_API_KEY"`, default Bearer)
- OpenAI Responses (`api_backend = "responses"`)
- Gemini OpenAI-compatible + Bearer
- OpenRouter + Bearer (optional attribution headers as non-secret)
- Generic OpenAI-compatible hosted

**Step 3: Local examples with explicit `auth_scheme = "none"`**

Update Ollama / local server sections and add LM Studio, llama.cpp, vLLM:

```toml
[model.ollama-codellama]
model = "codellama"
base_url = "http://localhost:11434/v1"
name = "CodeLlama (Ollama)"
auth_scheme = "none"
context_window = 16384
```

State clearly: tools/reasoning/images/structured output depend on the local server/model; missing `auth_scheme = "none"` may still inherit ambient xAI credentials.

**Step 4: Document the `auth_scheme` field** in the field reference table / configuring section (`bearer` default, `x_api_key`, `none`).

**Step 5: Grep for accidental secrets**

```bash
rg -n "sk-|sk-ant-|xai-|Bearer ey" \
  crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md
```

Expected: no real-looking secrets (placeholder-only if any).

**Step 6: Commit**

```bash
git add crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md
git commit -m "$(cat <<'EOF'
docs: document auth_scheme for providers and local LLMs

EOF
)"
```

---

### Task 7: Full verification gate

**Step 1: Format**

```bash
cargo fmt --all -- --check
```

Expected: exit 0.

**Step 2: Targeted tests**

```bash
cargo test --manifest-path crates/codegen/xai-grok-sampler/Cargo.toml
cargo test --manifest-path crates/codegen/xai-grok-shell/Cargo.toml agent::config
cargo test --manifest-path crates/codegen/xai-grok-shell/Cargo.toml agent::auth_method
cargo test --manifest-path crates/codegen/xai-grok-shell/Cargo.toml \
  config_model_override_parse
```

Expected: all PASS.

**Step 3: Clippy / check**

```bash
cargo clippy --manifest-path crates/codegen/xai-grok-sampler/Cargo.toml \
  --all-targets -- -D warnings
cargo clippy --manifest-path crates/codegen/xai-grok-shell/Cargo.toml \
  --all-targets -- -D warnings
cargo check --manifest-path crates/codegen/xai-grok-shell/Cargo.toml
```

Expected: exit 0.

**Step 4: Adversarial checklist (manual reasoning + tests already added)**

| Scenario | Expected |
|---|---|
| `auth_scheme=none` + ambient `XAI_API_KEY` | no auth header |
| `auth_scheme=none` + cached session | no auth header / no refresh |
| `auth_scheme=none` + accidental model key | no auth header |
| Anthropic `env_key` + `x_api_key` | `x-api-key` only |
| OpenAI `env_key` | Bearer only |
| invalid `auth_scheme` | model kept, field warned |
| no-auth catalog entry but xAI selected | normal xAI login |
| preferred_method pinned | pin authoritative |
| logs / auth_info | no full key/token |

**Step 5: Commit any fmt/fix leftovers**, then push feature branch:

```bash
git push -u origin HEAD
git rev-parse HEAD
git ls-remote origin refs/heads/feat/multi-provider-local-llm
```

Expected: remote SHA matches local HEAD.

---

## Out of scope

- Vendor SDKs / native Gemini protocol (OpenAI-compatible Gemini only).
- OMX Ralplan Architect/Critic gate tooling (`collaborationspawn_agent` alias).
- Committing `.omc/` research artifacts.
- Changing root generated manifests unless upstream already did.

## Reference skills

- @superpowers:executing-plans — sequential task execution
- @superpowers:subagent-driven-development — if choosing subagent-per-task
- @test-driven-development — red/green/refactor discipline already embedded above
