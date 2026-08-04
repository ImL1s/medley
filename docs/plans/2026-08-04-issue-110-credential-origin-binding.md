# Issue #110 PR 1 — Ambient xAI Credential Origin-Binding (Security Invariant) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Ambient xAI credentials (signed-in session token, `XAI_API_KEY`, managed deployment key) become usable ONLY when the final effective URL of a model is a first-party xAI origin per the EXISTING canonical predicate `is_xai_api_bearer_url`; a custom External/Local model with no explicit credential source becomes unready before any request is built; the gate is enforced at the fork-owned choke point `sampling_config_for_model`, with `resolve_credentials` sealed to `pub(crate)` and a sampler construction-time assertion as defense in depth.

**Architecture:** Rust cargo workspace, branch `providers`. Three crates change: `xai-grok-sampling-types` (new `CredentialSource` enum next to the existing `EndpointTrustClass` — additive), `xai-grok-sampler` (new `credential_source` field on `SamplerConfig` + one construction-time assertion — no classifier work; the sampler's private `resolve_endpoint_trust` already exists and stays private), and `xai-grok-shell` (the origin gate + typed-source classification inside `sampling_config_for_model`, fail-closed `model_readiness`, aux/web-search/bearer-resolver hardening, `EffectiveModelRoute` seam for PR 2). **No new external dependencies; the generated root `Cargo.toml` is never edited. The inherited function `resolve_credentials` and the inherited struct `ResolvedCredentials` are NOT modified except one visibility keyword + doc comment.**

**Tech stack:** Rust, cargo, serde, reqwest::Url, axum (already a dev-dependency where needed), `serial_test` + `xai_grok_test_support::EnvGuard` for env-var tests.

---

## Design decision (settled in the issue's comment thread — read `gh issue view 110 --repo ImL1s/medley --comments` before starting)

The issue's STOP condition 1 ("`resolve_credentials` can still return an ambient xAI credential for a non-first-party final origin") is in tension with keeping the fork mergeable: `resolve_credentials` is inherited verbatim from upstream (upstream `config.rs:4689`, same order, no origin check — per the owner's comment; upstream line not independently re-verified here) in the file this fork diverges from most (14,383 lines vs upstream's 12,388). The owner evaluated three options in the comments and chose **option (1)**:

> **Gate in `sampling_config_for_model`, and make `resolve_credentials` private** so no new caller can reach the ungated version. […] it satisfies the intent of the STOP condition — no caller can obtain an ungated ambient credential — without editing upstream's function body, and the compiler enforces it rather than a review comment.

This plan implements exactly that, in four layers:

- **Layer 0 — readiness** (`model_readiness`, fully fork-owned; upstream has zero occurrences): credential-less non-first-party models become unready, and the EXISTING enforcement web already blocks unready models at `/model` + ACP switch (`crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs:154-163`), the prepare boundary (`agent_ops.rs:2289-2296`), turn-time reconstruct (`sampler_turn.rs:481-498` via `ModelAuthFacts.ready`), subagent spawn (`subagent/mod.rs:945-947`), and picker/ACP metadata (`to_acp_model_info`, `config.rs:6103`). **No new blocking code is needed.**
- **Layer 1 — the choke-point gate** (`sampling_config_for_model` at `config.rs:5679`, fully fork-owned: upstream's version has neither the readiness strip nor the Codex resolver swap; every credential decision in it is fork-added): classify the typed `CredentialSource` and strip ambient credentials whose final URL is not first-party. Precedent already exists in this function for deciding credentials on grounds other than readiness — the Codex `api_key` clear-and-swap for READY models at `config.rs:5747`. The gate joins an existing seam; it does not invent one.
- **Layer 2 — compiler seal**: `resolve_credentials` becomes `pub(crate)` with a warning doc-comment, so no future out-of-crate caller can reach the ungated resolver. The four production call sites that bypass the `resolve_credentials_enforced` wrapper — `config.rs:5802` (`resolve_model_to_sampling_config`), `models.rs:919` (**the main chat path**), `subagent/mod.rs:956` (`resolve_model_override_to_config`), `agent_ops.rs:2303` (`prepare_sampling_config_for_model`) — all converge on `sampling_config_for_model`, so Layer 1 covers them all. **Do NOT put the gate in `resolve_credentials_enforced`** — those four sites never call it; a wrapper-only gate would leave every path a user actually chats through unprotected (this was an explicit correction in the comment thread).
- **Layer 3 — sampler assertion**: `SamplingClient::new` rejects an ambient `credential_source` bound to a non-first-party endpoint, so a future upstream-merge regression in any shell caller still cannot ship the credential.

**The canonical endpoint classifier already exists — call it, do not build one.** `crates/codegen/xai-grok-shell-base/src/util/mod.rs:88` (`is_xai_api_url`) and `:94` (`is_xai_api_bearer_url`), 52 non-test call sites, already consulted by six concerns (API-key kill switch `config.rs:5320`, bearer-resolver inheritance `config.rs:5629`, subagent resolver attach `subagent/mod.rs:~763`, embedding credentials, model-list construction, sampler identity headers/trust `client.rs:55-92`). The gate predicate in this plan is **`is_xai_api_bearer_url`** — it requires https, rejects ALL loopback, and accepts only the cli-chat-proxy match or `x.ai`/`*.x.ai` hosts. That is the correct one of the two: `is_xai_api_url` (:88) accepts loopback (cli-chat-proxy dev compat) and would PASS the issue's own `http://127.0.0.1:PORT/v1` repro; `is_xai_api_bearer_url` exists precisely so "a session bearer is never attached to a cleartext endpoint, including loopback" (its own doc comment), and `stamp_session_local_sampler_fields` already uses it for exactly this purpose.

**Two inherited tests pin the leaking behaviour and MUST be flipped and renamed** (Phase A work, called out so the engineer knows the red is intentional and not a regression they caused): `config.rs:8175` `resolve_credentials_empty_env_key_falls_through_to_session` and `config.rs:8191` `resolve_credentials_empty_env_key_falls_through_to_global_key`. Both use `base_url = "https://inference.example/v1"` (external) and assert the ambient credential IS attached; both exist verbatim upstream (`:6550`/`:6566` per the owner's comment) and encode upstream's single-vendor assumption. Under option (1) the raw resolver still returns the credential, so these tests are rewritten against the new entry point (the choke point) — see Task 3 Step 4.

**Scope guard (from the issue):** PR 1 ONLY — typed credential source, canonical final-origin gate, readiness/blocking, main + auxiliary path coverage, sampler defense-in-depth, core documentation migration. PR 2 (startup/switch UI, `grok inspect`, ACP metadata) is NOT built here; its consumption seam is Task 9's `EffectiveModelRoute`. Never weaken the Codex allowlist (`normalize_codex_base_url`, `xai-grok-sampler/src/client.rs:166`) or the Codex readiness arm. `auth_scheme = "none"` stays ready and header-free. No secret values in any new Debug/serde/log/error output — names of env vars / providers / headers only.

**TDD discipline:** every behavior-changing task starts by writing the failing test, running it to capture RED for the intended reason, then implementing, then GREEN, then committing. Commits land only at green; the mid-task RED run is the Phase-A evidence.

---

## Key verified facts (verified against the working tree unless marked otherwise)

- Path coverage: the four direct-resolver call sites above plus every aux path (`resolve_credentials_enforced` → `resolve_aux_model_sampling_config` `config.rs:5461`, `resolve_web_search_sampling_config` `config.rs:5871`) build their `SamplerConfig` via `sampling_config_for_model` (`config.rs:5679`).
- The turn-time reconstruct path (`reconstruct_full_config`, `sampler_turn.rs:434`) does NOT call `sampling_config_for_model`; it is protected by Layer 0 (unready → `auth_scheme = None`, which makes `SamplingClient` remove `Authorization`/`x-api-key` at construction AND again after the header injector) plus the resolver-attach gate (Task 7). Configs rebuilt there carry `credential_source: None` (legacy), so Layer 3 is intentionally inert on that path — state this in the PR description.
- The session bearer-RESOLVER is a second leak carrier independent of `api_key`: `session_token_auth_gate` (`auth_method.rs:432`) returns `true` for `ModelByok::NotByok` on ANY endpoint, and its two feeders (`SessionTokenAuthGate::new` `sampler_turn.rs:55-68`, `session_bearer_resolver` `subagent/mod.rs:757`) classify with loopback-permissive `is_xai_api_url`. The owner's comment warns explicitly: the subagent gate covers resolver REFRESH, not the frozen `api_key` — do not read it as protection.
- Provider-inherited URLs merge into `merged.base_url` (`model_providers.rs:316-364`) BEFORE resolution, so `info.base_url` IS the final effective URL after `model_provider`/preset/config merging. The `XAI_API_KEY` resolver branch targets `model.api_base_url.unwrap_or(info.base_url)` (`config.rs:5281-5286`) and stores that URL in `credentials.base_url`, so the choke-point gate examines the branch-correct URL automatically.
- Header seams in `xai-grok-sampler/src/client.rs` that can (re)insert `Authorization`/`x-api-key` after selection (from the owner's comment, spot-checked): `SamplingClient::new` api_key insert (~:811-835), `extra_headers` (~:842-848), `apply_env_http_headers`, `bearer_resolver` in `post()` (~:1021-1038), `header_injector` (~:1049-1051); `AuthScheme::None` removes both twice; `enforce_external_metadata_boundary` (:143) allowlists both. This is why Layer 3 sits at construction and Task 7 gates the resolver attach: extra/env headers are user-owned by definition, and the injector is the shell's OTel traceparent injector.
- `EndpointTrustClass` already exists (`xai-grok-sampling-types/src/types.rs:1052`), re-exported by `xai-grok-sampler/src/config.rs:52`. `SamplerConfig` is serde round-tripped to disk (session snapshots) → any new field needs `#[serde(default)]`.
- `SamplingError::InvalidConfiguration(&'static str)` exists (used at `client.rs:168`).
- Test helpers: `test_model_entry(model, base_url, api_key, env_key, api_base_url)` (`config.rs:7906`), `resolve_models_from_toml` (`config.rs:9969`; usage `let (_, models) = resolve_models_from_toml(&toml, None)` at ~8225), `EnvGuard`, `#[serial]`, axum mock harness (`client.rs:3746` `codex_mock_request_has_exact_path_and_no_xai_extensions`; shell has axum as a dev-dep — see `web_search_e2e_tests.rs`). Shell tests module starts at `config.rs:6320`.
- Existing fail-closed tests that must STAY green (they assert no-xAI-fallback for declared-but-broken explicit sources, on the RAW resolver — unaffected by option (1)): `declared_unresolved_credential_fails_closed_on_provider_endpoint`, `model_own_unresolved_key_ignores_provider_inline_auth`, `fail_closed_ref_ignores_a_colliding_auth_provider_table`, `undefined_model_provider_fails_closed` (`model_providers.rs` tests), `none_auth_scheme_ignores_model_env_session_and_global_keys`, `none_aux_model_resolves_without_api_key` (`config.rs` tests).

**Run all commands from the repository root.** Test filters are substring filters.

---

## Task 1: `CredentialSource` type + gate-predicate pin test

Additive only; green from the start.

**Files:**
- Modify: `crates/codegen/xai-grok-sampling-types/src/types.rs` (append after `EndpointTrustClass`, which ends at line 1062)
- Modify: `crates/codegen/xai-grok-sampler/src/config.rs` (re-export, line 52)
- Modify: `crates/codegen/xai-grok-sampler/src/lib.rs` (add to the `pub use config::{...}` list at lines 39-42 — open the file first to see the current list)

**Step 1: Add the enum**

```rust
/// Which credential source won resolution for a model route (#110).
///
/// Carries only non-secret identifiers — env-var / provider / header NAMES —
/// never credential bytes, header values, account IDs, or JWT material.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialSource {
    /// `auth_scheme = "none"`: deliberately unauthenticated.
    None,
    /// Static `api_key` on the model (or inherited provider) config.
    ModelApiKey,
    /// Resolved `env_key`; `name` is the winning environment-variable name.
    EnvKey { name: String },
    /// Named `[auth_provider.*]` (including the OpenAI Codex profile).
    AuthProvider { name: String },
    /// User-supplied `Authorization` / `x-api-key` via `extra_headers` or
    /// `env_http_headers`; `env` is the variable NAME for the latter.
    ExplicitHeader { header: String, env: Option<String> },
    /// Ambient signed-in xAI session token. First-party xAI origins only.
    XaiSession,
    /// Ambient `XAI_API_KEY` (or legacy alias). First-party xAI origins only.
    XaiApiKeyEnv,
    /// Ambient managed deployment key (auxiliary fallback). First-party only.
    XaiDeploymentKey,
    /// A credential is required but nothing resolved; the model must be
    /// unready and no request may be built.
    Missing,
}

impl CredentialSource {
    /// Ambient first-party xAI credentials that must never travel to a
    /// non-first-party origin.
    pub fn is_ambient_xai(&self) -> bool {
        matches!(
            self,
            Self::XaiSession | Self::XaiApiKeyEnv | Self::XaiDeploymentKey
        )
    }
}
```

(`XaiDeploymentKey` is an addition relative to the issue's sketch — the issue says "exact names are flexible" — needed because the aux fallback chain at `config.rs:5502-5505` can select `endpoints.deployment_key`.)

**Step 2: Re-export** — `xai-grok-sampler/src/config.rs:52`: `pub use xai_grok_sampling_types::{CredentialSource, EndpointTrustClass};` and add `CredentialSource` to lib.rs's config re-export list.

**Step 3: Pin the gate predicate's semantics** (shell `config.rs` tests mod — this calls the EXISTING canonical helper, builds nothing new):

```rust
/// #110 gate predicate = the existing canonical first-party classifier
/// `is_xai_api_bearer_url` (xai-grok-shell-base util). Pin the rows the
/// origin gate depends on so a future edit to the helper surfaces here.
#[test]
fn ambient_gate_predicate_semantics() {
    let allowed = ["https://api.x.ai/v1", crate::env::PROD_CLI_CHAT_PROXY_BASE_URL];
    let refused = [
        "http://127.0.0.1:11434/v1",   // loopback: the issue's repro
        "http://localhost:8080/v1",
        "https://127.0.0.1:8443/v1",   // loopback even with TLS
        "http://api.x.ai/v1",          // cleartext bearer is never safe
        "https://api.openai.com/v1",   // external
        "https://evil.example/x.ai",   // x.ai in path, not host
        "not a url",
    ];
    for url in allowed {
        assert!(crate::util::is_xai_api_bearer_url(url), "{url} must be first-party");
    }
    for url in refused {
        assert!(!crate::util::is_xai_api_bearer_url(url), "{url} must NOT be first-party");
    }
}
```

(Verify `crate::util` re-exports the shell-base helper — shell code already calls `crate::util::is_xai_api_bearer_url` at `config.rs:5629`, so the path exists.)

**Step 4: Run**

```bash
cargo test -p xai-grok-sampling-types
cargo test -p xai-grok-sampler --lib
cargo test -p xai-grok-shell --lib ambient_gate_predicate_semantics
```
Expected: all PASS.

**Step 5: Commit** — `feat(types): add secret-free CredentialSource; pin ambient-gate predicate semantics (#110)`

---

## Task 2: `SamplerConfig.credential_source` field

**Files:**
- Modify: `crates/codegen/xai-grok-sampler/src/config.rs` (`SamplerConfig` struct; `Default` at ~185; manual `Debug` at ~146)

**Step 1: Add the field**

```rust
    /// Typed source of `api_key` (#110), carried for the construction-time
    /// origin assertion. `None` = legacy/deserialized config (not asserted).
    /// Never contains secret bytes — names only.
    #[serde(default)]
    pub credential_source: Option<CredentialSource>,
```

Add `credential_source: None` to `Default`, and `.field("credential_source", &self.credential_source)` to the manual `Debug` (the enum's derived Debug prints names only — safe).

**Step 2: Serde-compat test** (sibling of `config_without_doom_loop_recovery_deserializes_to_none` at config.rs:394):

```rust
#[test]
fn config_without_credential_source_deserializes_to_none() {
    let mut stripped = serde_json::to_value(SamplerConfig::default()).unwrap();
    stripped.as_object_mut().unwrap().remove("credential_source");
    let config: SamplerConfig = serde_json::from_value(stripped).unwrap();
    assert!(config.credential_source.is_none());
}
```

**Step 3: Compile fallout** — `cargo build -p xai-grok-sampler -p xai-grok-shell 2>&1 | head -50`. Any `SamplerConfig` full literal without `..Default::default()`/`..minimal_config()` gets `credential_source: None` (the compiler enumerates them; `sampling_config_for_model`'s literal at `config.rs:5715` is one — set `None` for now, Task 3 fills it).

**Step 4: Run** — `cargo test -p xai-grok-sampler --lib && cargo test -p xai-grok-shell --lib agent::config` → PASS (no behavior change).

**Step 5: Commit** — `feat(sampler): carry typed credential_source on SamplerConfig (#110)`

---

## Task 3: The origin gate + typed-source classification in `sampling_config_for_model` (core regression pin)

This is the load-bearing task. The gate lives HERE — fully fork-owned code — not in `resolve_credentials` and not in `resolve_credentials_enforced`.

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` — `sampling_config_for_model` (5679) + two new private helpers
- Modify: the two inherited tests at `config.rs:8175` and `config.rs:8191` (+ the sibling at ~8210)

**Step 1: Write the failing choke-point matrix** (config.rs tests mod):

```rust
/// #110 regression pin: whatever resolve_credentials hands over, the
/// choke point must never emit an ambient xAI credential for a
/// non-first-party final origin, on any backend.
#[test]
#[serial]
fn choke_point_strips_ambient_credentials_for_non_first_party_origins() {
    use xai_grok_sampler::CredentialSource;
    use xai_grok_test_support::EnvGuard;
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    const SESSION: &str = "XAI_SESSION_SENTINEL";
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    for backend in [ApiBackend::ChatCompletions, ApiBackend::Responses, ApiBackend::Messages] {
        for base_url in ["https://api.openai.com/v1", "http://127.0.0.1:11434/v1"] {
            let mut m = test_model_entry("ext", base_url, None, None, None);
            m.info.api_backend = backend.clone();
            let config = sampling_config_for_model(
                &m,
                resolve_credentials(&m, Some(SESSION)),
                None, None,
                Some("deployment-sentinel".into()),
                Some("user-sentinel".into()),
            );
            assert_eq!(config.api_key, None, "session leaked: {backend:?} {base_url}");
            assert_eq!(config.credential_source, Some(CredentialSource::Missing));
            assert_eq!(config.deployment_id, None);
            assert_eq!(config.user_id, None);
            let rendered = format!("{config:?}");
            assert!(!rendered.contains("SENTINEL"), "debug leaked a sentinel");
        }
    }
}

#[test]
#[serial]
fn choke_point_keeps_first_party_ambient_flow_and_labels_it() {
    use xai_grok_sampler::CredentialSource;
    use xai_grok_test_support::EnvGuard;
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    const SESSION: &str = "XAI_SESSION_SENTINEL";
    const KEY: &str = "XAI_API_KEY_SENTINEL";

    // first-party + session
    let m = test_model_entry("xai", "https://api.x.ai/v1", None, None, None);
    let config = sampling_config_for_model(&m, resolve_credentials(&m, Some(SESSION)), None, None, None, None);
    assert_eq!(config.api_key.as_deref(), Some(SESSION));
    assert_eq!(config.credential_source, Some(CredentialSource::XaiSession));

    // first-party + XAI_API_KEY (no session)
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, KEY);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
    let m = test_model_entry("xai", "https://api.x.ai/v1", None, None, None);
    let config = sampling_config_for_model(&m, resolve_credentials(&m, None), None, None, None, None);
    assert_eq!(config.api_key.as_deref(), Some(KEY));
    assert_eq!(config.credential_source, Some(CredentialSource::XaiApiKeyEnv));

    // external base_url + FIRST-PARTY api_base_url: the API-key branch's
    // effective URL is api_base_url (config.rs:5281-5286) and lands in
    // credentials.base_url, so the gate must allow it.
    let m = test_model_entry("split", "https://third.example/v1", None, None, Some("https://api.x.ai/v1"));
    let config = sampling_config_for_model(&m, resolve_credentials(&m, None), None, None, None, None);
    assert_eq!(config.api_key.as_deref(), Some(KEY));
    assert_eq!(config.base_url, "https://api.x.ai/v1");
}

#[test]
fn choke_point_prefers_explicit_header_over_ambient() {
    use xai_grok_sampler::CredentialSource;
    let mut m = test_model_entry("hdr", "https://api.example.com/v1", None, None, None);
    m.info.extra_headers.insert("Authorization".into(), "Bearer user-owned".into());
    let config = sampling_config_for_model(
        &m, resolve_credentials(&m, Some("XAI_SESSION_SENTINEL")), None, None, None, None,
    );
    assert_eq!(config.api_key, None, "ambient credential added underneath explicit header");
    assert_eq!(
        config.credential_source,
        Some(CredentialSource::ExplicitHeader { header: "authorization".into(), env: None })
    );
    // the explicit header itself still flows to the client via extra_headers
    assert_eq!(config.extra_headers.get("Authorization").map(String::as_str), Some("Bearer user-owned"));
}

/// Provider-inherited base_url IS the final url; the gate must see it.
#[test]
fn choke_point_covers_provider_inherited_base_url() {
    let (_, models) = resolve_models_from_toml(
        r#"
        [model_providers.gateway]
        base_url = "https://gateway.example/v1"

        [model.via-gateway]
        model = "m"
        model_provider = "gateway"
        context_window = 200000
        "#,
        None,
    );
    let model = models.get("via-gateway").expect("model resolves");
    let config = sampling_config_for_model(
        model, resolve_credentials(model, Some("XAI_SESSION_SENTINEL")), None, None, None, None,
    );
    assert_eq!(config.api_key, None, "session leaked to provider-inherited external origin");
}

/// Codex swap (config.rs:5747) is untouched: ready Codex entries keep
/// api_key=None + live resolver, labeled as provider-owned. Reuse an
/// existing signed-in Codex fixture from this module (see the tests around
/// config.rs:7028/7428 that build a Codex provider) and assert:
///   config.api_key == None
///   config.bearer_resolver.is_some()
///   config.credential_source == Some(CredentialSource::AuthProvider{..})
#[test]
fn choke_point_labels_codex_as_provider_owned() { /* per the note above */ }
```

**Step 2: Prove RED**

```bash
cargo test -p xai-grok-shell --lib choke_point_ 2>&1 | tail -25
```
Expected failure, for the intended reason: `session leaked: ChatCompletions https://api.openai.com/v1 — left: Some("XAI_SESSION_SENTINEL"), right: None`. (`choke_point_keeps_first_party...` may already pass except the `credential_source` labels; that is expected.) If a case fails differently, stop and re-read the code — do not bend the test.

**Step 3: Implement.** Two fork-owned helpers next to `sampling_config_for_model`:

```rust
/// Explicit user-owned credential header declared on the model config
/// (`extra_headers` / `env_http_headers`): (lowercased header name, env NAME).
/// Values are never read here (#110).
fn explicit_credential_header(info: &ModelInfo) -> Option<(String, Option<String>)> {
    fn is_credential_header(name: &str) -> bool {
        name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
    }
    if let Some(name) = info.extra_headers.keys().find(|n| is_credential_header(n)) {
        return Some((name.to_ascii_lowercase(), None));
    }
    info.env_http_headers
        .iter()
        .find(|(n, _)| is_credential_header(n))
        .map(|(n, env)| (n.to_ascii_lowercase(), Some(env.clone())))
}

/// Replay `resolve_credentials`' decision order to name the winning source
/// (#110) WITHOUT editing the inherited resolver (owner's option (1)).
/// Uses the same first two predicates the resolver used (`own_credential`,
/// `auth_provider`), then distinguishes the ambient arms by the auth_type
/// it set. Kept adjacent to the gate so the two cannot drift apart.
fn classify_credential_source(
    model: &ModelEntry,
    credentials: &ResolvedCredentials,
) -> xai_grok_sampler::CredentialSource {
    use xai_grok_sampler::CredentialSource;
    if credentials.auth_scheme == AuthScheme::None {
        return CredentialSource::None;
    }
    if model.own_credential().is_some() {
        return if model.api_key.as_deref().is_some_and(|k| !k.trim().is_empty()) {
            CredentialSource::ModelApiKey
        } else {
            let name = model
                .env_key
                .as_ref()
                .and_then(|keys| {
                    keys.names().into_iter().find(|n| {
                        std::env::var(n).ok().is_some_and(|v| !v.trim().is_empty())
                    })
                })
                .unwrap_or_default()
                .to_owned();
            CredentialSource::EnvKey { name }
        };
    }
    if let Some(provider) = model.auth_provider.as_ref() {
        return CredentialSource::AuthProvider { name: provider.name.clone() };
    }
    match (&credentials.api_key, credentials.auth_type) {
        (Some(_), xai_chat_state::AuthType::SessionToken) => CredentialSource::XaiSession,
        (Some(_), xai_chat_state::AuthType::ApiKey) => CredentialSource::XaiApiKeyEnv,
        (None, _) => CredentialSource::Missing,
    }
}
```

Inside `sampling_config_for_model`, BEFORE the existing `!ready` strip block (so the readiness reason keeps winning the log line), add the gate; afterwards thread the source into the literal:

```rust
    let mut source = classify_credential_source(model, &credentials);
    // #110 origin gate (Layer 1): whatever the resolver handed over, an
    // ambient xAI credential is only usable against a first-party
    // bearer-safe origin. Joins the existing pattern of this function
    // deciding credentials on non-readiness grounds (see the Codex swap
    // below). `credentials.base_url` is branch-correct: the XAI_API_KEY
    // arm already stored `api_base_url` there when it won.
    if source.is_ambient_xai() && !crate::util::is_xai_api_bearer_url(&credentials.base_url) {
        tracing::error!(
            model = %model.info().model,
            source = ?source,
            "sampling_config_for_model: stripping ambient xAI credential for non-first-party origin"
        );
        credentials.api_key = None;
        source = match explicit_credential_header(model.info()) {
            Some((header, env)) => xai_grok_sampler::CredentialSource::ExplicitHeader { header, env },
            None => xai_grok_sampler::CredentialSource::Missing,
        };
    } else if source.is_ambient_xai()
        && let Some((header, env)) = explicit_credential_header(model.info())
    {
        // Explicit user-owned auth: never add an ambient credential
        // underneath it, on any origin (#110 contract row).
        credentials.api_key = None;
        source = xai_grok_sampler::CredentialSource::ExplicitHeader { header, env };
    }
```

In the existing `!ready` strip block also set `source = xai_grok_sampler::CredentialSource::Missing;`. In the `SamplerConfig` literal set `credential_source: Some(source)` (replacing Task 2's `None`). In the Codex branch at 5747, after the swap, set `config.credential_source` to `Some(CredentialSource::AuthProvider { name: ... })` from `model.effective_auth_provider()` when the resolver was attached (keep the already-classified source otherwise).

**Step 4: Flip and rename the inherited tests.** Intentional Phase-A work — seeing them go red after Step 3 is the fix working, NOT a regression you caused. They exist verbatim upstream and encode upstream's single-vendor assumption (an external-looking fixture URL carried no meaning there). Under option (1) the raw resolver still returns the credential, so rewrite them against the new entry point:

- `resolve_credentials_empty_env_key_falls_through_to_session` (8175) → rename `empty_env_key_on_external_origin_yields_no_credential_via_choke_point`: keep the EnvGuard setup and the external `https://inference.example/v1` fixture, but pass the resolved credentials through `sampling_config_for_model` and assert `config.api_key == None` + `config.credential_source == Some(Missing)`.
- `resolve_credentials_empty_env_key_falls_through_to_global_key` (8191) → rename `empty_env_key_on_external_origin_ignores_global_xai_key_via_choke_point`: same shape with the `XAI_API_KEY` EnvGuard; assert absence.
- Same pattern sweep: `resolve_credentials_empty_api_key_falls_through_to_session` (~8210, external URL + session assert) gets the identical flip+rename treatment. `resolve_credentials_sets_auth_type` (~8240) asserts only `auth_type` labels on the RAW resolver, which option (1) leaves unchanged — keep it, add a one-line comment pointing at the choke-point tests.
- The legitimate fall-through ORDER those tests used to pin (empty env_key → session → global key) stays covered on first-party fixtures by `choke_point_keeps_first_party_ambient_flow_and_labels_it`.

**Step 5: GREEN + module sweep**

```bash
cargo test -p xai-grok-shell --lib choke_point_
cargo test -p xai-grok-shell --lib agent::config 2>&1 | tail -20
cargo test -p xai-grok-shell --lib agent::model_providers 2>&1 | tail -10
```
The `model_providers.rs` fail-closed tests assert `resolve_credentials(...).api_key == None` on the RAW resolver for declared-but-broken sources — unaffected by option (1), must stay green. **Unverified:** other in-crate tests may pipe external fixtures through `sampling_config_for_model` and expect an ambient key to arrive — the sweep enumerates them; flip each with a comment, mirroring Step 4.

**Step 6: Commit** — `fix(security): strip ambient xAI credentials for non-first-party origins at the sampling choke point (#110)`

---

## Task 4: Fail-closed `model_readiness` for credential-less non-first-party models

`model_readiness` is fully fork-owned (upstream has zero occurrences) — edit freely.

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (`model_readiness`, 6025)

**Step 1: Failing tests**

```rust
/// #110: a bearer/x_api_key model with no explicit credential source whose
/// final origin is not first-party can never be satisfied → unready BEFORE
/// any request is built. First-party stays ready (existing session flow).
#[test]
fn readiness_gate_fails_credential_less_non_first_party_models() {
    // external no-key bearer → unready with actionable reason
    let m = test_model_entry("ext", "https://api.openai.com/v1", None, None, None);
    let (ready, reason) = model_readiness(&m);
    assert!(!ready, "external no-key model must be unready");
    let reason = reason.expect("actionable reason");
    for hint in ["api_key", "env_key", "auth_provider", "auth_scheme"] {
        assert!(reason.contains(hint), "reason must mention {hint}: {reason}");
    }

    // loopback bearer no-key → unready (issue repro)
    let m = test_model_entry("local", "http://127.0.0.1:11434/v1", None, None, None);
    assert!(!model_readiness(&m).0);

    // loopback + auth_scheme none → READY (acceptance criterion)
    let mut m = test_model_entry("local", "http://127.0.0.1:11434/v1", None, None, None);
    m.info.auth_scheme = AuthScheme::None;
    assert_eq!(model_readiness(&m), (true, None));

    // first-party no-key → READY (existing xAI session flow)
    let m = test_model_entry("xai", "https://api.x.ai/v1", None, None, None);
    assert_eq!(model_readiness(&m), (true, None));
    let m = test_model_entry("proxy", crate::env::PROD_CLI_CHAT_PROXY_BASE_URL, None, None, None);
    assert_eq!(model_readiness(&m), (true, None));

    // external + static key → READY
    let m = test_model_entry("byok", "https://api.openai.com/v1", Some("sk"), None, None);
    assert_eq!(model_readiness(&m), (true, None));

    // external + explicit credential header → READY (user-owned auth)
    let mut m = test_model_entry("hdr", "https://api.openai.com/v1", None, None, None);
    m.info.extra_headers.insert("Authorization".into(), "Bearer user".into());
    assert_eq!(model_readiness(&m), (true, None));

    // external base_url but first-party api_base_url → READY (XAI_API_KEY
    // remains usable against the first-party api_base_url)
    let m = test_model_entry("split", "https://third.example/v1", None, None, Some("https://api.x.ai/v1"));
    assert_eq!(model_readiness(&m), (true, None));
}
```

**Step 2: Prove RED** — `cargo test -p xai-grok-shell --lib readiness_gate_fails_credential_less 2>&1 | tail -15`. Expected: the first assertion fails (current code returns `(true, None)`).

**Step 3: Implement.** Insert AFTER the declared-but-unset `env_key` check (~6089-6099, its more specific reason must keep winning) and BEFORE the final `(true, None)`:

```rust
    // #110 (Layer 0): no explicit credential source and no first-party URL
    // the ambient session / XAI_API_KEY flow could legally target → the
    // model can never be satisfied; fail closed before any request exists.
    // `api_base_url` is included because the XAI_API_KEY branch targets it.
    if explicit_credential_header(&model.info).is_none()
        && !crate::util::is_xai_api_bearer_url(&model.info.base_url)
        && !model
            .api_base_url
            .as_deref()
            .is_some_and(crate::util::is_xai_api_bearer_url)
    {
        return (
            false,
            Some(format!(
                "no credential for non-xAI endpoint {}: set api_key, env_key, or auth_provider — or auth_scheme = \"none\" for a keyless local server",
                provider_hint_for_url(&model.info.base_url)
            )),
        );
    }
```

(Reaching this point already implies non-Codex, `auth_scheme != None`, and `has_own_credentials() == false` — those return earlier at 6029/6083/6086. `provider_hint_for_url` exists at `config.rs:5996`, secret-free.)

**Step 4: GREEN + downstream sweep.** Readiness feeds the entire existing blocking web (Design decision, Layer 0) with zero new blocking code. Run and repair fixtures that used credential-less external models and expected them ready:

```bash
cargo test -p xai-grok-shell --lib readiness_gate_fails_credential_less
cargo test -p xai-grok-shell --lib 2>&1 | tail -30   # run_in_background; triage
```
**Unverified:** roster/picker/ACP-metadata tests may pin `ready: true` for external no-key fixtures. Preferred fix: give the fixture `api_key: Some("test-key")` (keeps the test's subject); where readiness IS the subject, assert the new reason.

**Step 5: Commit** — `fix(security): mark credential-less non-first-party models unready (#110)`

---

## Task 5: Seal `resolve_credentials` to `pub(crate)` (compiler-enforced Layer 2)

The owner's chosen option (1): no future caller outside the crate can reach the ungated resolver. Fallout is real work — plan for it.

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (`resolve_credentials`, 5252 — visibility + doc only; the body stays byte-identical to ease upstream syncs in #24/#27)

**Step 1: Prove no external caller exists**

```bash
grep -rn "resolve_credentials" crates/ --include="*.rs" | grep -v "crates/codegen/xai-grok-shell/src"
```
Expected: no hits (in-crate callers only: `models.rs:919`, `agent_ops.rs:2303`, `subagent/mod.rs:956`, `config.rs` internals + tests, `model_providers.rs` tests, `web_search_e2e_tests.rs` — all inside `xai-grok-shell/src`; the two inherited tests live in `config.rs`'s own tests mod and keep compiling under `pub(crate)`). **If a hit appears in another crate, STOP: keep `pub`, add the warning doc-comment anyway, and record the deviation in the PR description — do not silently widen the task.**

**Step 2: Change visibility + document the contract**

```rust
/// Priority: model api_key/env_key > cached auth-provider token > session
/// token > XAI_API_KEY.
///
/// `AuthScheme::None` short-circuits: no ambient model/env/session/global
/// credentials are attached (local / keyless OpenAI-compatible endpoints).
///
/// SECURITY (#110): this inherited resolver is deliberately UNGATED — it can
/// return an ambient xAI credential for any base_url. Every caller MUST pass
/// the result through `sampling_config_for_model`, which owns the
/// first-party origin gate and the typed `CredentialSource`. It is
/// `pub(crate)` so no out-of-crate caller can obtain an ungated credential;
/// keep the body upstream-identical (sync work in #24/#27).
pub(crate) fn resolve_credentials(model: &ModelEntry, session_key: Option<&str>) -> ResolvedCredentials {
```

**Step 3: Compile fallout** — `cargo build -p xai-grok-shell 2>&1 | head -20`, then `cargo check --workspace 2>&1 | tail -20`. In-crate callers are unaffected; anything else the compiler flags gets routed through `try_resolve_model_credentials` or `sampling_config_for_model` instead (record each rewrite in the commit message).

**Step 4: Run** — `cargo test -p xai-grok-shell --lib 2>&1 | tail -10` → green.

**Step 5: Commit** — `refactor(security): seal ungated resolve_credentials to pub(crate) (#110)`

---

## Task 6: Auxiliary + web-search path hardening

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` — `resolve_aux_model_sampling_config` (5461; ambient fallback 5502-5560), `resolve_web_search_sampling_config` (5871)

**Step 1: Failing tests**

```rust
/// #110: the aux fallback mints an ambient xAI bearer (session → XAI_API_KEY
/// → deployment_key) onto resolve_inference_base_url(). A custom
/// models_base_url pointing that at a non-first-party origin must refuse.
#[test]
fn aux_fallback_refuses_ambient_bearer_for_non_first_party_inference_base() {
    let endpoints = EndpointsConfig {
        models_base_url: Some("https://third-party.example/v1".into()),
        ..EndpointsConfig::default()
    };
    let resolved = resolve_aux_model_sampling_config(
        "image-describe", &IndexMap::new(), &endpoints,
        Some("XAI_SESSION_SENTINEL"), false, None, None,
    );
    assert!(resolved.is_none(), "ambient bearer routed to non-first-party inference base");
}

/// The aux fallback labels its ambient source truthfully — it stuffs the
/// bearer into a synthetic entry's api_key, which classify_credential_source
/// would otherwise mislabel as ModelApiKey and dodge the sampler assertion.
#[test]
fn aux_fallback_labels_ambient_source() {
    use xai_grok_sampler::CredentialSource;
    let endpoints = EndpointsConfig::default();
    let resolved = resolve_aux_model_sampling_config(
        "image-describe", &IndexMap::new(), &endpoints,
        Some("XAI_SESSION_SENTINEL"), false, None, None,
    ).expect("first-party fallback resolves");
    assert_eq!(resolved.credential_source, Some(CredentialSource::XaiSession));
}

/// #110: web search on an unready model is DISABLED, not sent
/// credential-stripped/unauthenticated to the external origin.
#[test]
fn web_search_disables_for_unready_external_model() {
    let mut catalog = IndexMap::new();
    catalog.insert(
        "search".to_string(),
        test_model_entry("search", "https://search.example/v1", None, None, None),
    );
    let resolved = resolve_web_search_sampling_config(
        "search", &catalog, Some("XAI_SESSION_SENTINEL"), false, None, None,
        &EndpointsConfig::default(),
    );
    assert!(resolved.is_none(), "unready external web-search model must disable web search");
}
```

(Check `EndpointsConfig`'s real fields/Default before writing — `resolve_inference_base_url` reads `models_base_url` at `config.rs:411-415`; `EndpointsConfig::default()` usage verified in `none_aux_model_resolves_without_api_key`. If the default inference base in tests is not first-party per `is_xai_api_bearer_url`, pin the prod URL via the config in `aux_fallback_labels_ambient_source`.)

**Step 2: Prove RED** — `cargo test -p xai-grok-shell --lib aux_fallback_ 2>&1 | tail -10; cargo test -p xai-grok-shell --lib web_search_disables 2>&1 | tail -6`. Expected: the fallback returns `Some` carrying the sentinel; web search returns `Some` (credential-stripped after Task 3 — still `Some`, so still RED).

**Step 3: Implement.** In `resolve_aux_model_sampling_config`, replace the `xai_bearer` fallback block:

```rust
    // #110: this fallback attaches an ambient xAI bearer; it may only
    // target a first-party origin (a custom models_base_url could point
    // resolve_inference_base_url() anywhere).
    let inference_base = endpoints.resolve_inference_base_url();
    if !crate::util::is_xai_api_bearer_url(&inference_base) {
        tracing::warn!(
            aux_model = %model_id,
            "auxiliary fallback refused: inference base is not first-party xAI"
        );
        return None;
    }
    let (bearer, ambient_source) = if let Some(s) = session_key {
        (s.to_owned(), xai_grok_sampler::CredentialSource::XaiSession)
    } else if let Ok(k) = crate::agent::auth_method::read_xai_api_key_env() {
        (k, xai_grok_sampler::CredentialSource::XaiApiKeyEnv)
    } else if let Some(k) = endpoints.deployment_key.clone() {
        (k, xai_grok_sampler::CredentialSource::XaiDeploymentKey)
    } else {
        tracing::warn!(aux_model = %model_id, "no credentials for auxiliary model; falling back to active model");
        return None;
    };
    // ... existing synthetic ModelEntry construction with api_key: Some(bearer) ...
    let mut sampler = sampling_config_for_model(&entry, credentials, alpha_test_key, client_version, None, None);
    sampler.credential_source = Some(ambient_source);
    Some(sampler)
```

In `resolve_web_search_sampling_config`, at the top of the catalog-entry branch (~5880):

```rust
        let (ready, reason) = model_readiness(&entry);
        if !ready {
            tracing::warn!(
                web_search_model = %model_id,
                reason = ?reason,
                "web search model is not ready; disabling web search"
            );
            return None;
        }
```

**Step 4: GREEN + neighbors** — `cargo test -p xai-grok-shell --lib aux_ ; cargo test -p xai-grok-shell --lib web_search 2>&1 | tail -10`. `none_aux_model_resolves_without_api_key`, the web-search e2e, and the Codex `*_preflight` paths must stay green (Codex preflight untouched).

**Step 5: Commit** — `fix(security): gate auxiliary/web-search sampling on origin and readiness (#110)`

---

## Task 7: Session bearer-RESOLVER attach gate (turn-time refresh path)

The resolver carries a live session bearer independent of `api_key`; the owner's comment warns the existing subagent gate covers resolver REFRESH only. Close the `NotByok`-on-any-endpoint hole and the loopback-permissive classification at both attach sites — by calling the existing canonical predicate, not by writing rules.

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/auth_method.rs` (`session_token_auth_gate`, 432)
- Modify: `crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs` (`SessionTokenAuthGate::new`, 55-68)
- Modify: `crates/codegen/xai-grok-shell/src/agent/subagent/mod.rs` (`session_bearer_resolver`, 757; classification at :766)

**Step 1: Failing test** (next to `session_token_auth_gate` if `auth_method.rs` has a tests mod; otherwise in config.rs tests):

```rust
#[test]
fn session_token_auth_gate_requires_first_party_for_not_byok() {
    use crate::agent::auth_method::{session_token_auth_gate, ModelByok};
    assert!(!session_token_auth_gate(true, ModelByok::NotByok, false),
        "#110: NotByok on a non-first-party endpoint must not attach a session resolver");
    assert!(session_token_auth_gate(true, ModelByok::NotByok, true));
    assert!(!session_token_auth_gate(true, ModelByok::Byok, true));
    assert!(session_token_auth_gate(true, ModelByok::Unknown, true));
    assert!(!session_token_auth_gate(true, ModelByok::Unknown, false));
    assert!(!session_token_auth_gate(false, ModelByok::NotByok, true));
}
```

**Step 2: Prove RED** — `cargo test -p xai-grok-shell --lib session_token_auth_gate_requires 2>&1 | tail -6` (first assertion fails: the arm is `NotByok => true` today).

**Step 3: Implement**
- `auth_method.rs:432`: `ModelByok::NotByok => true,` → `ModelByok::NotByok => endpoint_is_first_party,`; update the doc comment.
- `sampler_turn.rs` `SessionTokenAuthGate::new`: `endpoint_is_first_party: crate::util::is_xai_api_url(base_url),` → `endpoint_is_first_party: crate::util::is_xai_api_bearer_url(base_url),` (bearer-safe canonical predicate; update the field's doc comment).
- `subagent/mod.rs:766`: `crate::util::is_xai_api_url(base_url)` → `crate::util::is_xai_api_bearer_url(base_url)`.

**Step 4: GREEN + blast radius**

```bash
cargo test -p xai-grok-shell --lib session_token_auth_gate
cargo test -p xai-grok-shell --lib agent::subagent 2>&1 | tail -10
cargo test -p xai-grok-shell --lib session 2>&1 | tail -15   # run_in_background
```
**Unverified:** `session/acp_session_tests/auth_error_no_retry_tests.rs` (e.g. `reconstruct_full_config_wires_bearer_resolver_for_session_method_despite_api_key_auth_type`, :905) may use loopback/`example.com` base_urls that previously classified first-party via `is_xai_api_url`'s loopback acceptance. Triage per each test's doc comment: subject = resolver wiring → move the fixture to `https://api.x.ai/v1` or the prod proxy URL; subject = refusal → assert the new refusal.

**Step 5: Commit** — `fix(security): attach session bearer resolvers only for bearer-safe first-party origins (#110)`

---

## Task 8: Sampler defense-in-depth — construction-time assertion (Layer 3)

**Files:**
- Modify: `crates/codegen/xai-grok-sampler/src/client.rs` (`SamplingClient::new` — insert right after `let endpoint_trust = resolve_endpoint_trust(&config);` at ~796)

**Step 1: Failing tests** (client.rs tests mod; check `fn minimal_config()`'s `base_url`/`auth_scheme` defaults before reusing):

```rust
#[test]
fn ambient_xai_credential_cannot_construct_for_non_first_party_endpoint() {
    use crate::config::CredentialSource;
    for source in [
        CredentialSource::XaiSession,
        CredentialSource::XaiApiKeyEnv,
        CredentialSource::XaiDeploymentKey,
    ] {
        for base_url in ["https://api.openai.com/v1", "http://127.0.0.1:11434/v1"] {
            let err = SamplingClient::new(SamplerConfig {
                api_key: Some("XAI_SESSION_SENTINEL".into()),
                base_url: base_url.into(),
                credential_source: Some(source.clone()),
                ..minimal_config()
            })
            .expect_err("ambient credential + non-first-party endpoint must not construct");
            let rendered = format!("{err}");
            assert!(!rendered.contains("SENTINEL"), "error leaked the secret");
        }
    }
}

#[test]
fn non_ambient_sources_and_first_party_ambient_still_construct() {
    use crate::config::CredentialSource;
    SamplingClient::new(SamplerConfig {
        api_key: Some("sk-provider".into()),
        base_url: "https://api.openai.com/v1".into(),
        credential_source: Some(CredentialSource::ModelApiKey),
        ..minimal_config()
    })
    .expect("model-owned key on external endpoint");
    SamplingClient::new(SamplerConfig {
        api_key: Some("session".into()),
        base_url: "https://api.x.ai/v1".into(),
        credential_source: Some(CredentialSource::XaiSession),
        ..minimal_config()
    })
    .expect("ambient credential on first-party endpoint");
    SamplingClient::new(SamplerConfig {
        api_key: Some("k".into()),
        base_url: "https://api.openai.com/v1".into(),
        credential_source: None,
        ..minimal_config()
    })
    .expect("legacy config unaffected");
}
```

**Step 2: Prove RED** — `cargo test -p xai-grok-sampler --lib ambient_xai_credential_cannot_construct 2>&1 | tail -8` (`expect_err` panics; construction currently succeeds).

**Step 3: Implement**

```rust
        // Defense in depth (#110, Layer 3): an ambient first-party xAI
        // credential bound to a non-first-party endpoint is an invalid
        // configuration, even if an upstream caller regresses later.
        // Secret-free error. (An explicit endpoint_trust override still
        // wins above — it is an internal test facility; see the
        // explicit_endpoint_trust_override test at ~:3441.)
        if !matches!(endpoint_trust, EndpointTrustClass::FirstPartyXai)
            && config
                .credential_source
                .as_ref()
                .is_some_and(xai_grok_sampling_types::CredentialSource::is_ambient_xai)
        {
            return Err(SamplingError::InvalidConfiguration(
                "ambient xAI credential is not allowed for a non-first-party endpoint",
            ));
        }
```

**Step 4: Wire-capture coverage.** Extend the existing axum harness (`client.rs:3746` pattern) with one non-Codex captured test: BYOK config (`api_key: Some("provider-key-sentinel")`, `credential_source: Some(ModelApiKey)`, loopback mock, ChatCompletions) → captured `Authorization == "Bearer provider-key-sentinel"` and no header/body value contains `XAI_SESSION_SENTINEL`/`XAI_API_KEY_SENTINEL`. And one `auth_scheme: AuthScheme::None` case → the captured request has NO `authorization` and NO `x-api-key` header (acceptance criterion). State in a comment: for the repro model, ambient absence is proven by Task 3 (the config arrives keyless) + this task (an ambient-source config cannot construct at all) — the request cannot exist, which is stronger than capturing an empty header.

**Step 5: GREEN + full sampler suite** — `cargo test -p xai-grok-sampler --lib 2>&1 | tail -5`.

**Step 6: Commit** — `fix(security): reject ambient xAI credentials for non-first-party endpoints at client construction (#110)`

---

## Task 9: `EffectiveModelRoute` — the typed seam PR 2 consumes

PR 1 produces the typed route object; PR 2 renders it. No UI here.

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (new items near `sampling_config_for_model`)

**Step 1: Failing test**

```rust
#[test]
fn effective_model_route_is_secret_free_and_matches_the_sampler_inputs() {
    use xai_grok_sampler::{CredentialSource, EndpointTrustClass};
    const SECRET: &str = "route-secret-0123456789abcdef";
    let mut m = test_model_entry(
        "wire-model",
        &format!("https://user:{SECRET}@api.example.com:8443/v1/x?api_key={SECRET}#frag"),
        Some(SECRET),
        None,
        None,
    );
    m.info.api_backend = ApiBackend::ChatCompletions;
    let creds = resolve_credentials(&m, None);
    let route = effective_model_route("my-model", &m, &creds);
    assert_eq!(route.catalog_id, "my-model");
    assert_eq!(route.wire_model, "wire-model");
    assert_eq!(route.sanitized_origin, "https://api.example.com:8443/v1/x");
    assert_eq!(route.endpoint_trust, EndpointTrustClass::External);
    assert_eq!(route.credential_source, CredentialSource::ModelApiKey);
    assert!(route.ready);
    let json = serde_json::to_string(&route).unwrap();
    let debug = format!("{route:?}");
    for rendered in [json, debug] {
        for window in SECRET.as_bytes().windows(8) {
            let w = std::str::from_utf8(window).unwrap();
            assert!(!rendered.contains(w), "route leaked secret window {w}");
        }
    }
}

/// Route trust classification agrees with the sampler's documented
/// derivation (prod proxy / xAI-with-auth → FirstPartyXai, loopback →
/// Local, else External) so the two aligned copies cannot drift silently.
#[test]
fn endpoint_trust_for_url_parity_with_sampler_semantics() {
    use xai_grok_sampler::EndpointTrustClass::*;
    let cases = [
        (crate::env::PROD_CLI_CHAT_PROXY_BASE_URL, FirstPartyXai),
        ("https://api.x.ai/v1", FirstPartyXai),
        ("http://127.0.0.1:11434/v1", Local),
        ("http://localhost:8080/v1", Local),
        ("https://api.openai.com/v1", External),
        ("not a url", External),
    ];
    for (url, want) in cases {
        assert_eq!(endpoint_trust_for_url(url), want, "{url}");
    }
}
```

**Step 2: Prove RED** — compile error (functions not found).

**Step 3: Implement** — composed ONLY from existing predicates (no new hostname rules; the sampler keeps its own aligned private copy — a pre-existing duplication the issue permits deferring: "If sharing the endpoint classifier requires a broader refactor, split the work into reviewable PRs"):

```rust
/// Scheme + host [+ port] [+ non-secret path]. Userinfo, query, and
/// fragment are dropped by construction (#110).
pub fn sanitized_origin(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return "<invalid-url>".to_owned();
    };
    let mut origin = format!(
        "{}://{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or("<no-host>")
    );
    if let Some(port) = parsed.port() {
        origin.push_str(&format!(":{port}"));
    }
    let path = parsed.path().trim_end_matches('/');
    if !path.is_empty() && path != "/" {
        origin.push_str(path);
    }
    origin
}

/// Display-trust for the route object, composed from the existing canonical
/// predicate (`is_xai_api_bearer_url`) plus the same loopback match
/// `provider_hint_for_url` already uses. Parity with the sampler's private
/// derivation is pinned by endpoint_trust_for_url_parity_with_sampler_semantics.
pub(crate) fn endpoint_trust_for_url(url: &str) -> xai_grok_sampler::EndpointTrustClass {
    use xai_grok_sampler::EndpointTrustClass;
    if crate::util::is_xai_api_bearer_url(url) {
        return EndpointTrustClass::FirstPartyXai;
    }
    if let Ok(parsed) = reqwest::Url::parse(url) {
        match parsed.host() {
            Some(url::Host::Domain("localhost")) => return EndpointTrustClass::Local,
            Some(url::Host::Ipv4(ip)) if ip.is_loopback() => return EndpointTrustClass::Local,
            Some(url::Host::Ipv6(ip)) if ip.is_loopback() => return EndpointTrustClass::Local,
            _ => {}
        }
    }
    EndpointTrustClass::External
}

/// One secret-free, typed effective model route (#110). PR 2 (startup and
/// switch display, `grok inspect --json`, ACP metadata) must consume this
/// instead of re-deriving labels. Derive it from the SAME `ModelEntry` +
/// `ResolvedCredentials` used to build the `SamplerConfig` so the reported
/// route cannot drift from the sampled route.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EffectiveModelRoute {
    pub catalog_id: String,
    pub wire_model: String,
    /// Not yet threaded: the `model_provider` id is consumed during config
    /// merging and is not retained on `ModelEntry`. PR 2 threads it if
    /// display needs it.
    pub provider_id: Option<String>,
    pub api_backend: ApiBackend,
    pub sanitized_origin: String,
    pub endpoint_trust: xai_grok_sampler::EndpointTrustClass,
    pub auth_scheme: AuthScheme,
    pub credential_source: xai_grok_sampler::CredentialSource,
    pub ready: bool,
    pub readiness_reason: Option<String>,
}

pub fn effective_model_route(
    catalog_id: &str,
    model: &ModelEntry,
    credentials: &ResolvedCredentials,
) -> EffectiveModelRoute {
    let (ready, readiness_reason) = model_readiness(model);
    EffectiveModelRoute {
        catalog_id: catalog_id.to_owned(),
        wire_model: model.info().model.clone(),
        provider_id: None,
        api_backend: model.info().api_backend.clone(),
        sanitized_origin: sanitized_origin(&credentials.base_url),
        endpoint_trust: endpoint_trust_for_url(&credentials.base_url),
        auth_scheme: credentials.auth_scheme,
        credential_source: classify_credential_source(model, credentials),
        ready,
        readiness_reason,
    }
}
```

**Step 4: GREEN** — `cargo test -p xai-grok-shell --lib effective_model_route ; cargo test -p xai-grok-shell --lib endpoint_trust_for_url`.

**Step 5: Commit** — `feat(shell): add secret-free EffectiveModelRoute derived from resolved credentials (#110)`

---

## Task 10: Non-regression sweep + repo gates

**Step 1: The issue's focused gates plus the touched suites** (long runs → `run_in_background`):

```bash
cargo test -p xai-grok-shell --lib agent::config
cargo test -p xai-grok-shell --lib agent::model_providers
cargo test -p xai-grok-shell --lib agent::subagent
cargo test -p xai-grok-shell --lib session
cargo test -p xai-grok-sampler --lib
cargo test -p xai-grok-sampling-types
cargo test -p xai-grok-shell --lib
cargo fmt --all -- --check
cargo clippy -p xai-grok-shell -p xai-grok-sampler -p xai-grok-pager --lib --no-deps -- -D warnings
```

**Step 2: Codex-preservation checklist** — all green with ZERO edits to Codex code paths (if one seems to need an edit, STOP and re-examine your change):
- `cargo test -p xai-grok-sampler --lib codex` (allowlist `normalize_codex_base_url`, header retention, UA policy)
- `cargo test -p xai-grok-shell --lib openai_codex` (provider isolation; `direct_openai_codex_auth_provider_cannot_authenticate_a_custom_origin`)
- First-party metadata-boundary tests (`enforce_external_metadata_boundary` suite) unchanged.

**Step 3: Fix stragglers, re-run to fully green. Commit** — `test: non-regression sweep for #110 PR1`

---

## Task 11: Core documentation migration (Phase D, PR-1 scope)

**Files:**
- Modify: `crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md`

**Step 1: `auth_scheme` table (~line 118)** — change the `"bearer"` row:

```markdown
| `"bearer"` | Default. Sends `Authorization: Bearer <key>` from `api_key` / `env_key`. The ambient fallback (session token / `XAI_API_KEY`) applies **only when the final effective URL is a first-party xAI origin**. |
```

**Step 2: Rewrite "Credential Resolution" (~lines 130-137)**:

```markdown
Grok resolves the API key in this order (skipped entirely when `auth_scheme = "none"`):

1. The `api_key` field in the model config
2. The environment variable(s) named by `env_key` — a single string or an array of names. The first set, non-empty value wins (for example `env_key = ["ANTHROPIC_AUTH_TOKEN", "LC_ANTHROPIC_AUTH_TOKEN"]` for SSH `LC_*` forwarding)
3. A named `auth_provider` — the provider owns auth for the model; if its credential is unavailable, the request fails closed and **never** falls back to xAI credentials
4. An explicit `Authorization` / `x-api-key` header supplied via `extra_headers` or `env_http_headers` — treated as user-owned auth; no ambient credential is ever added underneath it
5. **First-party xAI origins only:** your signed-in session token (from `grok login`)
6. **First-party xAI origins only:** the `XAI_API_KEY` environment variable (Grok also accepts `GROK_CODE_XAI_API_KEY`)

A model whose final effective URL (after `model_provider` / `base_url` / `api_base_url` merging) is **not** a first-party xAI origin and that declares none of sources 1-4 is marked **unready**: the picker dims it, `/model` and ACP switches reject it, and no request is sent. Grok never silently sends an unauthenticated request in this case, and never attaches your xAI session token or `XAI_API_KEY` to a non-xAI endpoint.

**Migrating a configuration that relied on the old fallback:**

- External provider: set `env_key = "PROVIDER_API_KEY"` (or `api_key`, `auth_provider`, or an explicit credential header).
- Keyless local server (Ollama, LM Studio, llama.cpp): set `auth_scheme = "none"`.
```

**Step 3: Keyless-local note (~line 380)** — replace "Grok may still inherit ambient xAI credentials…" with:

```markdown
Keyless local servers need an explicit `auth_scheme = "none"`. Without it, the entry declares Bearer auth with no credential and is marked **unready** (ambient xAI credentials are never attached to a local endpoint).
```

**Step 4: Security/release note.** Verified: no top-level `CHANGELOG*`/`RELEASE*` file exists. Put the note in the PR description (breaking change: credential-less custom External/Local models become unready; migration = the two bullets above; also state the reconstruct-path `credential_source: None` inertness and the `classify_credential_source` labeling corner from "Items NOT verified" #8) and flag GitHub Releases to the maintainer.

**Step 5: Verify no stale claims remain**

```bash
grep -rn "session / ambient fallback" crates/codegen/xai-grok-pager/docs/ && echo "STALE TEXT REMAINS" || echo "clean"
grep -n "ambient" crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md
```

**Step 6: Commit** — `docs(security): document first-party-only ambient credential fallback and migration (#110)`

---

## Definition of done (PR 1)

- [ ] External/Local + no explicit credential source → unready before any request (Task 4), blocked at selection/restore/switch/spawn via the existing unready enforcement.
- [ ] `XAI_SESSION_SENTINEL` / `XAI_API_KEY_SENTINEL` never reach a non-first-party final: choke-point gate (Task 3), aux/web-search (Task 6), resolver-attach (Task 7), sampler construction (Task 8); resolver sealed `pub(crate)` (Task 5) so no future caller obtains an ungated credential — the owner's option (1) reading of STOP condition 1, compiler-enforced.
- [ ] `resolve_credentials` body byte-identical (visibility keyword + doc comment only); `ResolvedCredentials` struct untouched — upstream-sync friendly (#24/#27).
- [ ] The inherited leak-pinning tests (8175/8191, plus the ~8210 sibling) flipped, renamed, and retargeted at the choke point, with the first-party fall-through order still covered.
- [ ] First-party xAI no-key models keep the existing session/`XAI_API_KEY` flow; `auth_scheme = "none"` stays ready and header-free.
- [ ] `api_key` / `env_key` / `auth_provider` / explicit-header configs still work externally; cold provider never falls to xAI (pre-existing, pinned by existing model_providers tests).
- [ ] Codex profile untouched and green (Task 10 Step 2).
- [ ] No secret values in any new Debug/serde/log/error output (sentinel-window assertions in Tasks 3/8/9).
- [ ] Gate evaluates the FINAL merged URL (provider-inheritance and `api_base_url` split tests in Task 3).
- [ ] PR 2 seam exists: `EffectiveModelRoute` + `effective_model_route` (Task 9).
- [ ] `cargo fmt --check` + the issue's clippy gate pass (Task 10).

## Explicitly out of scope (PR 2 — do not build here)

Startup route line, model-switch confirmation display, `grok inspect` human/JSON route report (`crates/codegen/xai-grok-shell/src/inspect/`), ACP `_meta` route fields beyond the existing `ready`/`readinessReason` (whose VALUES change automatically via Task 4), display-width snapshot tests.

## Items NOT verified against the real code (executor must check before relying on them)

1. **Upstream line numbers** (`config.rs:4689`; tests `:6550`/`:6566`) are quoted from the owner's issue comments, not independently re-verified here. Same for the exact client.rs header-seam ranges (:811-835 etc.) — spot-checked only.
2. **`AuthProviderRef.name` field path** — inferred from `provider.name.as_str()` usage in model_providers.rs tests; confirm when labeling `AuthProvider { name }`.
3. **Exact set of legacy tests that break** in Tasks 3/4/7 beyond the ones enumerated — the listed sweeps enumerate them at execution time; per-task triage rules are given.
4. **`minimal_config()` contents** in sampler client tests (exists; contents unread) — check `base_url`/`auth_scheme` before Task 8.
5. **`EndpointsConfig` literal construction** in Task 6's test (`models_base_url: Some(..)` + `..Default::default()`) — the struct's Default/field visibility was not fully read; also confirm the default inference base used in tests is first-party per `is_xai_api_bearer_url` for `aux_fallback_labels_ambient_source`.
6. **`resolve_models_from_toml` exact signature** (name/line verified; arity inferred from one call site).
7. **Whether any out-of-crate caller of `resolve_credentials` exists** — Task 5 Step 1's grep is the check, with an explicit STOP instruction if one appears.
8. **`classify_credential_source` corner**: after `subagent_auth_type` mutates `auth_type` post-resolve (`subagent/mod.rs:957`), a session-sourced key could in principle be labeled `XaiApiKeyEnv` (or vice-versa). Both are ambient variants, so the GATE treats them identically and security holds; only the label may be imprecise on that path. Note it in the PR description.
