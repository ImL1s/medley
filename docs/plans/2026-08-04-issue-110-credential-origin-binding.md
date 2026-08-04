# Issue #110 PR 1 — Ambient xAI Credential Origin-Binding (Security Invariant) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Ambient xAI credentials (signed-in session token, `XAI_API_KEY`, managed deployment key) become usable ONLY when the final effective URL of a model is a canonically classified first-party xAI origin; a custom External/Local model with no explicit credential source becomes unready before any request is built, with defense-in-depth re-checks in the sampler.

**Architecture:** The repo is a Rust cargo workspace (branch `providers`). Three crates are touched: `xai-grok-sampling-types` (new `CredentialSource` enum next to the existing `EndpointTrustClass`), `xai-grok-sampler` (promote the private `resolve_endpoint_trust` logic to a public canonical classifier; add a construction-time assertion), and `xai-grok-shell` (gate the ambient branches of `resolve_credentials`, fail-closed `model_readiness`, thread the typed source through the `sampling_config_for_model` choke point, harden auxiliary/web-search/bearer-resolver paths, add the `EffectiveModelRoute` seam for PR 2). Dependency direction (verified): `xai-grok-shell` → `xai-grok-sampler` → `xai-grok-sampling-types`, so the canonical classifier lives in the sampler and the shell imports it. **No new external dependencies are needed, so the generated root `Cargo.toml` is never edited.**

**Tech stack:** Rust, cargo, serde, reqwest::Url, axum (already a dev-dependency where needed), `serial_test` + `xai_grok_test_support::EnvGuard` for env-var tests.

**Scope guard (from the issue):**
- PR 1 ONLY: typed credential source, canonical final-origin gate, readiness/blocking, main + auxiliary path coverage, sampler defense-in-depth, core documentation migration.
- PR 2 (startup/switch UI, `grok inspect`, ACP metadata) is NOT in scope. PR 1 leaves it exactly one seam: `EffectiveModelRoute` + `effective_model_route()` (Task 9).
- NEVER weaken the OpenAI Codex allowlist (`normalize_codex_base_url` in `crates/codegen/xai-grok-sampler/src/client.rs:166` stays untouched; Codex readiness arm in `model_readiness` stays untouched).
- `auth_scheme = "none"` models stay ready and send no auth header (existing behavior — regression-tested in Task 4).
- No secret value may appear in any new struct Debug/serde output, log, or error (only NAMES of env vars / providers / headers).

**TDD discipline:** the issue mandates "pin the regression before changing behavior". Every behavior-changing task below starts by writing the failing test, running it to capture the RED failure for the intended reason, then implementing, then re-running GREEN, then committing. Commits happen only at green (test+impl land together per task); the RED run in the middle of each task is the Phase-A evidence.

**Key verified facts an engineer must know (do not re-derive):**
- Every `SamplerConfig` in the shell is built by `sampling_config_for_model` (`crates/codegen/xai-grok-shell/src/agent/config.rs:5679`), the documented "fail-closed choke point". Main chat / new session / `/model` & ACP switch / restore all route through `prepare_sampling_config_for_model` (`crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs:2271`); subagents through `crates/codegen/xai-grok-shell/src/agent/subagent/mod.rs:956`; aux models through `resolve_aux_model_sampling_config` (`config.rs:5461`); web search through `resolve_web_search_sampling_config` (`config.rs:5871`).
- `resolve_credentials` (`config.rs:5252`) resolves: own api_key/env_key → cached auth_provider token → **session token (UNGATED — the bug)** → **`XAI_API_KEY` (UNGATED — the bug)** → none. The `XAI_API_KEY` branch uses `model.api_base_url.unwrap_or(info.base_url)`; the session branch uses `info.base_url`.
- `model_readiness` (`config.rs:6025`) returns `(true, None)` for a bearer/x_api_key model with no credential at all — even on an external/loopback URL (the readiness half of the bug). Unready models are already rejected everywhere: `/model` + ACP switch (`crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs:154-163`), prepare boundary (`agent_ops.rs:2289-2296`), turn-time reconstruct (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs:481-498`, via `ModelAuthFacts.ready`), subagent spawn (`subagent/mod.rs:945-947`), picker/ACP metadata (`to_acp_model_info`, `config.rs:6103`). **Making the model unready therefore blocks selection/restore/switch/spawn with no new blocking code.**
- Provider-inherited URLs: `[model_providers.*]` merge writes into `merged.base_url` (`crates/codegen/xai-grok-shell/src/agent/model_providers.rs:316-364`) BEFORE `resolve_credentials` runs, so `info.base_url` already IS the final effective URL after provider/preset/config merging. Gating on `info.base_url` (and `api_base_url` for the env-key branch) satisfies the issue's "final effective URL" requirement.
- The sampler already owns trust classification: `resolve_endpoint_trust` (`crates/codegen/xai-grok-sampler/src/client.rs:74`): explicit override → exact prod cli-chat-proxy (`https://cli-chat-proxy.grok.com/v1`) → FirstPartyXai; any other loopback → Local; `auth_scheme != None && is_xai_api_url` (host `x.ai`/`*.x.ai` or cli-chat-proxy match) → FirstPartyXai; else External. This is the classifier to canonicalize — do NOT invent new hostname rules.
- `EndpointTrustClass` already exists in `crates/codegen/xai-grok-sampling-types/src/types.rs:1052` and is re-exported by `xai-grok-sampler/src/config.rs:52`.
- The session bearer-RESOLVER (turn-time refresh) attach is a second leak path independent of `api_key`: `session_token_auth_gate` (`crates/codegen/xai-grok-shell/src/agent/auth_method.rs:432`) returns `true` for `ModelByok::NotByok` regardless of endpoint; `SessionTokenAuthGate::new` (`sampler_turn.rs:55-68`) and `session_bearer_resolver` (`subagent/mod.rs:757`) feed it `crate::util::is_xai_api_url(base_url)`, which ACCEPTS loopback. Task 7 hardens this.
- Existing tests that assert the CURRENT VULNERABLE behavior and must be migrated to first-party URLs in Task 3 (all in `config.rs` tests mod, which starts at line 6320): `resolve_credentials_empty_env_key_falls_through_to_session` (~8175, uses `https://inference.example/v1` + session-jwt), `resolve_credentials_empty_env_key_falls_through_to_global_key` (~8191), `resolve_credentials_empty_api_key_falls_through_to_session` (~8210), `resolve_credentials_sets_auth_type` (~8240, `https://example.com/v1`).
- Test helpers that exist and should be reused: `test_model_entry(model, base_url, api_key, env_key, api_base_url)` (`config.rs:7906`), `resolve_models_from_toml(toml_str, ...)` (`config.rs:9969`, see usage at ~8225), `EnvGuard` from `xai_grok_test_support`, `#[serial]`/`#[serial_test::serial]` for env-var tests, axum mock-server harness patterns in `crates/codegen/xai-grok-sampler/src/client.rs:3746` (`codex_mock_request_has_exact_path_and_no_xai_extensions`) and `crates/codegen/xai-grok-shell/src/session/acp_session_tests/web_search_e2e_tests.rs`.

**Run all commands from the repository root.** Test-name filters below are substring filters; `cargo test -p xai-grok-shell --lib agent::config` runs the whole module.

---

## Task 1: `CredentialSource` type + canonical `classify_endpoint_trust` export

No behavior change; pure addition. Green from the start (write tests alongside).

**Files:**
- Modify: `crates/codegen/xai-grok-sampling-types/src/types.rs` (append after `EndpointTrustClass`, which ends at line 1062)
- Modify: `crates/codegen/xai-grok-sampler/src/client.rs` (refactor `resolve_endpoint_trust` at line 74)
- Modify: `crates/codegen/xai-grok-sampler/src/config.rs` (re-export, line 52 area)
- Modify: `crates/codegen/xai-grok-sampler/src/lib.rs` (export list, lines 38-42)

**Step 1: Add the enum to `xai-grok-sampling-types`**

Append after the `EndpointTrustClass` definition in `types.rs`:

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

Note: `XaiDeploymentKey` is an addition relative to the issue's sketch (the issue says "exact names are flexible"); it is needed because the auxiliary fallback chain (`config.rs:5502-5505`) can select `endpoints.deployment_key`.

**Step 2: Extract the canonical classifier in the sampler**

In `crates/codegen/xai-grok-sampler/src/client.rs`, replace the body of `resolve_endpoint_trust` (line 74) with a delegation and a new public function directly above it:

```rust
/// Canonical endpoint-trust classification (#110), shared by config-time
/// resolution in `xai-grok-shell` and request construction here. Order:
/// exact production cli-chat-proxy → FirstPartyXai; any other loopback →
/// Local; xAI-operated host with real auth → FirstPartyXai; else External.
/// Do NOT duplicate these hostname rules elsewhere.
pub fn classify_endpoint_trust(base_url: &str, auth_scheme: AuthScheme) -> EndpointTrustClass {
    if crate::util::is_prod_cli_chat_proxy_url(base_url) {
        return EndpointTrustClass::FirstPartyXai;
    }
    if is_loopback_url(base_url) {
        return EndpointTrustClass::Local;
    }
    if should_send_xai_identity_headers(auth_scheme, base_url) {
        return EndpointTrustClass::FirstPartyXai;
    }
    EndpointTrustClass::External
}

fn resolve_endpoint_trust(config: &SamplerConfig) -> EndpointTrustClass {
    if let Some(explicit) = config.endpoint_trust {
        return explicit;
    }
    classify_endpoint_trust(&config.base_url, config.auth_scheme)
}
```

Keep the existing doc comment content of `resolve_endpoint_trust` merged into the new function's docs.

**Step 3: Re-export**

- `crates/codegen/xai-grok-sampler/src/config.rs` line 52: change `pub use xai_grok_sampling_types::EndpointTrustClass;` to `pub use xai_grok_sampling_types::{CredentialSource, EndpointTrustClass};`
- `crates/codegen/xai-grok-sampler/src/lib.rs`: add `classify_endpoint_trust` to the `pub use client::{...}` list (line 38) and ensure `CredentialSource`, `EndpointTrustClass` are in the `pub use config::{...}` list (lines 39-42; open the file to see the current list before editing).

**Step 4: Classifier unit tests** (in the existing `#[cfg(test)] mod tests` of `client.rs`; mirror the style of `explicit_endpoint_trust_override_wins_over_derivation` at line 3441):

```rust
#[test]
fn classify_endpoint_trust_matrix() {
    use crate::config::{AuthScheme, EndpointTrustClass::*};
    let cases = [
        ("https://cli-chat-proxy.grok.com/v1", AuthScheme::Bearer, FirstPartyXai),
        ("https://api.x.ai/v1", AuthScheme::Bearer, FirstPartyXai),
        ("https://api.x.ai/v1", AuthScheme::None, External), // keyless: no identity
        ("http://127.0.0.1:11434/v1", AuthScheme::Bearer, Local),
        ("http://localhost:8080/v1", AuthScheme::None, Local),
        ("http://[::1]:9999/v1", AuthScheme::Bearer, Local),
        ("https://api.openai.com/v1", AuthScheme::Bearer, External),
        ("https://evil.example/x.ai", AuthScheme::Bearer, External), // path, not host
        ("not a url", AuthScheme::Bearer, External),
    ];
    for (url, scheme, want) in cases {
        assert_eq!(classify_endpoint_trust(url, scheme), want, "{url}");
    }
}
```

(The `AuthScheme::None` + `api.x.ai` → `External` row documents the EXISTING `should_send_xai_identity_headers` semantics — verify it passes; if it fails, the existing semantics differ and the row must be fixed to match, not the function.)

**Step 5: Run**

```bash
cargo test -p xai-grok-sampling-types --lib
cargo test -p xai-grok-sampler --lib classify_endpoint_trust
cargo test -p xai-grok-sampler --lib   # full crate: no regressions from the refactor
```
Expected: all PASS.

**Step 6: Commit** — `feat(sampler): expose canonical classify_endpoint_trust and typed CredentialSource`

---

## Task 2: Thread `CredentialSource` through `ResolvedCredentials` and `SamplerConfig` (labeling only, no gating yet)

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` — `ResolvedCredentials` (5229), `resolve_credentials` (5252), `enforce_disable_api_key_auth` (5320), `sampling_config_for_model` (5679)
- Modify: `crates/codegen/xai-grok-sampler/src/config.rs` — `SamplerConfig` struct + `Default` + `Debug`

**Step 1: Write labeling tests first** (in `config.rs` tests mod; they will fail to COMPILE until the field exists — that is the red state for a type-threading task):

```rust
#[test]
fn resolve_credentials_labels_the_winning_source() {
    use xai_grok_sampler::CredentialSource;
    // static api_key
    let m = test_model_entry("m", "https://api.example.com/v1", Some("sk-own"), None, None);
    assert_eq!(resolve_credentials(&m, None).source, CredentialSource::ModelApiKey);
    // session token on a first-party origin
    let m = test_model_entry("m", "https://api.x.ai/v1", None, None, None);
    assert_eq!(
        resolve_credentials(&m, Some("session-jwt")).source,
        CredentialSource::XaiSession
    );
    // auth_scheme = none
    let mut m = test_model_entry("m", "http://127.0.0.1:1/v1", None, None, None);
    m.info.auth_scheme = AuthScheme::None;
    assert_eq!(resolve_credentials(&m, Some("s")).source, CredentialSource::None);
}

#[test]
#[serial]
fn resolve_credentials_labels_env_key_name() {
    use xai_grok_sampler::CredentialSource;
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::set("GROK_TEST_SOURCE_LABEL_KEY", "v");
    let m = test_model_entry("m", "https://api.example.com/v1", None, Some("GROK_TEST_SOURCE_LABEL_KEY"), None);
    assert_eq!(
        resolve_credentials(&m, None).source,
        CredentialSource::EnvKey { name: "GROK_TEST_SOURCE_LABEL_KEY".into() }
    );
}
```

Run: `cargo test -p xai-grok-shell --lib resolve_credentials_labels` → expected: COMPILE ERROR `no field 'source'`.

**Step 2: Add the field to `ResolvedCredentials`**

```rust
pub struct ResolvedCredentials {
    pub api_key: Option<String>,
    pub base_url: String,
    pub auth_type: xai_chat_state::AuthType,
    pub auth_scheme: AuthScheme,
    /// Which source won (#110). Reflects the statically resolved credential;
    /// bearer-resolver attachment is gated separately at its attach sites.
    pub source: xai_grok_sampler::CredentialSource,
}
```

**Step 3: Label every arm of `resolve_credentials`** (still NO gating — behavior identical). Rework the `(api_key, base_url, auth_type)` tuple into `(api_key, base_url, auth_type, source)`:
- `AuthScheme::None` early return → `source: CredentialSource::None`.
- own-credential arm → `ModelApiKey` when `model.api_key` is non-blank, else `EnvKey { name }` where `name` is the winning env var. NOTE: `first_own_credential` (`config.rs:5238`) does not report WHICH name won; compute it as `model.env_key.as_ref().and_then(|k| k.names().into_iter().find(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()).is_some())).unwrap_or_default().to_owned()` or extend `EnvKeys` with `resolve_name_value()` returning `(name, value)` — prefer the small `EnvKeys::resolve_name_value` extension and reuse it in `first_own_credential` so name/value can't disagree.
- auth-provider arm → `AuthProvider { name: provider.name.clone() }` (verify the field is `name` — used at `model_providers.rs` tests as `p.name`).
- session arm → `XaiSession`.
- `XAI_API_KEY` arm → `XaiApiKeyEnv`.
- final none arm → `Missing` (bearer/x_api_key model with nothing resolved; keep the existing env-key warning).

**Step 4: `enforce_disable_api_key_auth` (5320)**: when it swaps to session, also set `creds.source = if session_key.is_some() { CredentialSource::XaiSession } else { CredentialSource::Missing };`

**Step 5: Add `credential_source` to `SamplerConfig`** (`crates/codegen/xai-grok-sampler/src/config.rs`):

```rust
    /// Typed source of `api_key` (#110), carried for the construction-time
    /// origin assertion. `None` = legacy/deserialized config (not asserted).
    /// Never contains secret bytes — names only.
    #[serde(default)]
    pub credential_source: Option<CredentialSource>,
```

Add `credential_source: None` to `impl Default for SamplerConfig` (line ~185) and `.field("credential_source", &self.credential_source)` to the manual `Debug` (line ~146; the enum's own Debug prints names only — safe). Old serialized session snapshots deserialize with `None` via `serde(default)`; the existing test `config_without_doom_loop_recovery_deserializes_to_none` (config.rs:394) is the pattern — add a sibling test `config_without_credential_source_deserializes_to_none`.

**Step 6: Thread through the choke point** — in `sampling_config_for_model` (`shell config.rs:5679`): in the `!ready` strip block also `credentials.source = xai_grok_sampler::CredentialSource::Missing;`, and set `credential_source: Some(credentials.source.clone())` in the `SamplerConfig` literal (the field order must match the struct; place near `auth_scheme`).

**Step 7: Fix remaining literal sites** (compiler-driven). Verified list of `ResolvedCredentials { .. }` literals to update with a `source:` field: `config.rs` 5255, 5310 (prod) and tests at 8008, 8034, 8321 (`api_key_creds` helper), 8349, 9463. Then:

```bash
cargo build -p xai-grok-shell -p xai-grok-sampler 2>&1 | head -50
```
Fix every "missing field" error the compiler lists (there may be `SamplerConfig` full-literals without `..Default::default()` in other files — the compiler enumerates them; add `credential_source: None` there).

**Step 8: Run**

```bash
cargo test -p xai-grok-shell --lib resolve_credentials_labels
cargo test -p xai-grok-shell --lib agent::config
cargo test -p xai-grok-sampler --lib
```
Expected: PASS (labeling changes no behavior).

**Step 9: Commit** — `feat(shell): record which credential source won in ResolvedCredentials/SamplerConfig`

---

## Task 3: Gate the ambient branches of `resolve_credentials` on first-party origin (regression pin #1)

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (`resolve_credentials`, new helpers)
- Modify existing tests in the same file + `crates/codegen/xai-grok-shell/src/agent/model_providers.rs` tests as needed

**Step 1: Write the failing matrix test** (config.rs tests mod). Use the issue's sentinel names:

```rust
/// #110 regression pin: ambient xAI credentials must never resolve for a
/// non-first-party final origin.
#[test]
#[serial]
fn ambient_credential_gate_blocks_non_first_party_origins() {
    use xai_grok_sampler::CredentialSource;
    use xai_grok_test_support::EnvGuard;
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    const SESSION: &str = "XAI_SESSION_SENTINEL";
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    // external HTTPS + session token → refused
    let m = test_model_entry("ext", "https://api.openai.com/v1", None, None, None);
    let creds = resolve_credentials(&m, Some(SESSION));
    assert_eq!(creds.api_key, None, "session token leaked to external origin");
    assert_eq!(creds.source, CredentialSource::Missing);

    // loopback + bearer + session token → refused (issue's repro shape)
    let m = test_model_entry("local", "http://127.0.0.1:11434/v1", None, None, None);
    let creds = resolve_credentials(&m, Some(SESSION));
    assert_eq!(creds.api_key, None, "session token leaked to loopback origin");

    // first-party xAI + session token → still allowed (existing behavior)
    let m = test_model_entry("xai", "https://api.x.ai/v1", None, None, None);
    let creds = resolve_credentials(&m, Some(SESSION));
    assert_eq!(creds.api_key.as_deref(), Some(SESSION));
    assert_eq!(creds.source, CredentialSource::XaiSession);

    // prod cli-chat-proxy + session token → still allowed
    let m = test_model_entry("proxy", crate::env::PROD_CLI_CHAT_PROXY_BASE_URL, None, None, None);
    assert_eq!(resolve_credentials(&m, Some(SESSION)).api_key.as_deref(), Some(SESSION));
}

#[test]
#[serial]
fn ambient_xai_api_key_gate_follows_the_branch_effective_url() {
    use xai_grok_sampler::CredentialSource;
    use xai_grok_test_support::EnvGuard;
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    const KEY: &str = "XAI_API_KEY_SENTINEL";
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, KEY);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    // external base_url, no session → XAI_API_KEY refused
    let m = test_model_entry("ext", "https://api.openai.com/v1", None, None, None);
    let creds = resolve_credentials(&m, None);
    assert_eq!(creds.api_key, None, "XAI_API_KEY leaked to external origin");
    assert_eq!(creds.source, CredentialSource::Missing);

    // external base_url BUT first-party api_base_url → allowed on api_base_url
    // (api_base_url is the URL the API-key branch actually uses; verified at
    // config.rs:5281-5286)
    let m = test_model_entry("split", "https://third.example/v1", None, None, Some("https://api.x.ai/v1"));
    let creds = resolve_credentials(&m, None);
    assert_eq!(creds.api_key.as_deref(), Some(KEY));
    assert_eq!(creds.base_url, "https://api.x.ai/v1");
    assert_eq!(creds.source, CredentialSource::XaiApiKeyEnv);

    // session refused on external base_url must FALL THROUGH to a permitted
    // first-party XAI_API_KEY api_base_url, not dead-end
    let creds = resolve_credentials(&m, Some("XAI_SESSION_SENTINEL"));
    assert_eq!(creds.api_key.as_deref(), Some(KEY), "gated session must not block the api_base_url branch");
}

/// Provider-inherited base_url is the FINAL url; the gate must see it.
#[test]
fn ambient_credential_gate_covers_provider_inherited_base_url() {
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
    let creds = resolve_credentials(model, Some("XAI_SESSION_SENTINEL"));
    assert_eq!(creds.api_key, None, "session token leaked to provider-inherited external origin");
}

/// extra_headers/env_http_headers Authorization or x-api-key is explicit
/// user-owned auth: recognized, and no ambient credential underneath.
#[test]
fn explicit_credential_header_owns_auth_and_blocks_ambient() {
    use xai_grok_sampler::CredentialSource;
    let mut m = test_model_entry("hdr", "https://api.example.com/v1", None, None, None);
    m.info.extra_headers.insert("Authorization".into(), "Bearer user-owned".into());
    let creds = resolve_credentials(&m, Some("XAI_SESSION_SENTINEL"));
    assert_eq!(creds.api_key, None, "ambient credential added underneath explicit header");
    assert_eq!(
        creds.source,
        CredentialSource::ExplicitHeader { header: "authorization".into(), env: None }
    );

    let mut m = test_model_entry("hdr-env", "https://api.example.com/v1", None, None, None);
    m.info.env_http_headers.insert("x-api-key".into(), "MY_PROVIDER_KEY".into());
    let creds = resolve_credentials(&m, Some("XAI_SESSION_SENTINEL"));
    assert_eq!(
        creds.source,
        CredentialSource::ExplicitHeader { header: "x-api-key".into(), env: Some("MY_PROVIDER_KEY".into()) }
    );
}
```

Note on `resolve_models_from_toml`: verified to exist at `config.rs:9969` and used as `let (_, models) = resolve_models_from_toml(&toml_string, None)` (see ~line 8225). Match the real signature when writing the test.

**Step 2: Run to prove RED**

```bash
cargo test -p xai-grok-shell --lib ambient_credential_gate 2>&1 | tail -20
cargo test -p xai-grok-shell --lib explicit_credential_header_owns 2>&1 | tail -8
```
Expected failures, for the intended reason: `assertion 'left == right' failed: session token leaked to external origin; left: Some("XAI_SESSION_SENTINEL"), right: None` (and the equivalent for `XAI_API_KEY_SENTINEL` / provider-inherited / explicit-header cases). If a case fails for a DIFFERENT reason, stop and re-read the current code — do not "fix" the test.

**Step 3: Implement the gate.** In `config.rs`, add near `resolve_credentials`:

```rust
/// #110: ambient first-party credentials (session token, XAI_API_KEY,
/// deployment key) are only usable against a canonically classified
/// first-party xAI origin. One classifier — the sampler's — no local rules.
fn ambient_xai_credential_allowed(final_url: &str, auth_scheme: AuthScheme) -> bool {
    xai_grok_sampler::classify_endpoint_trust(final_url, auth_scheme)
        == xai_grok_sampler::EndpointTrustClass::FirstPartyXai
}

/// Explicit user-owned credential header declared on the model config
/// (`extra_headers` / `env_http_headers`): (lowercased header name, env NAME).
/// Values are never read here.
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
```

Then rewrite the branch chain of `resolve_credentials` (order: own credential → auth_provider → explicit header → gated session → gated `XAI_API_KEY` → missing):

```rust
    let (api_key, base_url, auth_type, source) = if let Some(key) = model.own_credential() {
        // ... Task 2 labeling, unchanged ...
    } else if let Some(provider) = model.auth_provider.as_ref() {
        // ... unchanged: provider-owned, never falls through to xAI ...
    } else if let Some((header, env)) = explicit_credential_header(info) {
        // Explicit user-owned auth: never add an ambient xAI credential
        // underneath it (#110). Headers flow via extra_headers at the client.
        (
            None,
            info.base_url.clone(),
            xai_chat_state::AuthType::ApiKey,
            CredentialSource::ExplicitHeader { header, env },
        )
    } else if let Some(key) =
        session_key.filter(|_| ambient_xai_credential_allowed(&info.base_url, info.auth_scheme))
    {
        (
            Some(key.to_owned()),
            info.base_url.clone(),
            xai_chat_state::AuthType::SessionToken,
            CredentialSource::XaiSession,
        )
    } else if let Some((key, url)) = crate::agent::auth_method::read_xai_api_key_env()
        .ok()
        .map(|key| {
            let url = model
                .api_base_url
                .clone()
                .unwrap_or_else(|| info.base_url.clone());
            (key, url)
        })
        .filter(|(_, url)| ambient_xai_credential_allowed(url, info.auth_scheme))
    {
        (Some(key), url, xai_chat_state::AuthType::ApiKey, CredentialSource::XaiApiKeyEnv)
    } else {
        // ... existing env-key warning ...
        (None, info.base_url.clone(), xai_chat_state::AuthType::ApiKey, CredentialSource::Missing)
    };
```

Keep the trailing `tracing::debug!` and extend it with `source = ?source` (names only — safe).

**Step 4: Run new tests GREEN, then the whole module to find inverted legacy tests**

```bash
cargo test -p xai-grok-shell --lib ambient_credential_gate
cargo test -p xai-grok-shell --lib agent::config 2>&1 | grep -E "^test .* FAILED|failures:" -A 20
cargo test -p xai-grok-shell --lib agent::model_providers 2>&1 | grep -E "FAILED|failures:" -A 20
```

Migrate the four verified legacy tests that asserted the vulnerable fall-through on external URLs by changing their `base_url` to `https://api.x.ai/v1` (their INTENT — empty env_key/api_key falls through to session/global — remains valid only on first-party): `resolve_credentials_empty_env_key_falls_through_to_session`, `resolve_credentials_empty_env_key_falls_through_to_global_key`, `resolve_credentials_empty_api_key_falls_through_to_session`, `resolve_credentials_sets_auth_type`. **Unverified:** other tests in `agent::config`, `agent::model_providers`, `agent::subagent`, or `session::` may also assert session/`XAI_API_KEY` resolution on non-first-party URLs — the two greps above enumerate them. For each: if it tests the fall-through itself, move it to a first-party URL; if it tests external behavior, flip the assertion to the gated result. Never delete a test without replacing its intent.

**Step 5: Full-crate compile+test sanity** — `cargo test -p xai-grok-shell --lib 2>&1 | tail -5` (long; use `run_in_background` if needed). All green.

**Step 6: Commit** — `fix(security): bind ambient xAI credentials to first-party origins in resolve_credentials (#110)`

---

## Task 4: Fail-closed `model_readiness` for credential-less non-first-party models (regression pin #2)

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (`model_readiness`, 6025)

**Step 1: Failing tests**

```rust
/// #110: a bearer/x_api_key model with no explicit credential source whose
/// final origin is External/Local can never be satisfied → unready BEFORE
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

**Step 2: Prove RED**

```bash
cargo test -p xai-grok-shell --lib readiness_gate_fails_credential_less 2>&1 | tail -15
```
Expected: `external no-key model must be unready` assertion failure (current code returns `(true, None)`).

**Step 3: Implement.** In `model_readiness` (6025), insert AFTER the declared-but-unset `env_key` check (~6089-6099) and BEFORE the final `(true, None)`:

```rust
    // #110: no explicit credential source and the final effective URL is not
    // a first-party xAI origin → ambient session / XAI_API_KEY are not
    // usable here; fail closed before any request exists. `api_base_url` is
    // checked too because the XAI_API_KEY branch targets it when set.
    if explicit_credential_header(&model.info).is_none()
        && !ambient_xai_credential_allowed(&model.info.base_url, model.info.auth_scheme)
        && !model
            .api_base_url
            .as_deref()
            .is_some_and(|url| ambient_xai_credential_allowed(url, model.info.auth_scheme))
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

(`provider_hint_for_url` exists at `config.rs:5996` and is already secret-free; reaching this point implies `has_own_credentials()` is false and `auth_scheme != None` because those return `(true, None)` earlier at 6083-6088.)

**Step 4: GREEN + downstream sweep.** Readiness feeds picker rows, ACP `_meta.ready`, model switch, prepare, reconstruct, subagent — all fail-closed paths that now ALSO cover this model class with zero new code. Run and repair any fixture that used a credential-less external model and expected it ready:

```bash
cargo test -p xai-grok-shell --lib readiness_gate_fails_credential_less
cargo test -p xai-grok-shell --lib 2>&1 | tail -30   # run_in_background; triage failures
```
**Unverified:** roster/picker/ACP-metadata tests may pin `ready: true` snapshots for external no-key fixtures. Fix fixtures by giving them a static `api_key = "test-key"` (preferred — keeps the test's original subject) or asserting the new unready reason where readiness IS the subject.

**Step 5: Commit** — `fix(security): mark credential-less non-first-party models unready (#110)`

---

## Task 5: Choke-point verification + captured-wire proof

No production change expected beyond Tasks 3-4; this task pins the end-to-end result at the `SamplerConfig` boundary and at real captured requests.

**Files:**
- Test: `crates/codegen/xai-grok-shell/src/agent/config.rs` (tests mod)
- Test: `crates/codegen/xai-grok-sampler/src/client.rs` (tests mod)

**Step 1: Choke-point matrix (shell)** — for the issue's repro entry, prove the config that reaches the sampler is credential-free across all three generic backends:

```rust
#[test]
#[serial]
fn sampling_config_for_external_no_key_model_is_credential_free() {
    use xai_grok_sampler::CredentialSource;
    use xai_grok_test_support::EnvGuard;
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, "XAI_API_KEY_SENTINEL");
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
    for backend in [ApiBackend::ChatCompletions, ApiBackend::Responses, ApiBackend::Messages] {
        let mut m = test_model_entry("ext", "http://127.0.0.1:9/v1", None, None, None);
        m.info.api_backend = backend.clone();
        let config = sampling_config_for_model(
            &m,
            resolve_credentials(&m, Some("XAI_SESSION_SENTINEL")),
            None, None,
            Some("deployment-sentinel".into()),
            Some("user-sentinel".into()),
        );
        assert_eq!(config.api_key, None, "{backend:?}");
        assert_eq!(config.credential_source, Some(CredentialSource::Missing), "{backend:?}");
        // unready strip also clears identity metadata (existing behavior)
        assert_eq!(config.deployment_id, None);
        assert_eq!(config.user_id, None);
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("SENTINEL"), "debug leaked a sentinel: {backend:?}");
    }
}
```

**Step 2: Captured-request tests (sampler).** In `client.rs` tests, mirror the axum harness of `codex_mock_request_has_exact_path_and_no_xai_extensions` (line 3746) for the CHAT COMPLETIONS backend with a BYOK config against a loopback mock: assert captured `Authorization == "Bearer provider-key-sentinel"` AND no header value anywhere contains `XAI_SESSION_SENTINEL`/`XAI_API_KEY_SENTINEL` (they were never configured — this pins that the boundary keeps the intended provider credential while Task 8 below proves ambient ones cannot even construct). **Note honestly in the test comment:** the ambient-credential absence for the repro model is enforced upstream (config arrives with `api_key: None`, proven in Step 1) plus construction rejection (Task 8) — a captured request for the repro entry cannot exist at all, which is stronger than capturing an empty header. Also add the `auth_scheme = "none"` wire case: config with `auth_scheme: AuthScheme::None` + loopback mock → captured request has NO `authorization` and NO `x-api-key` header (acceptance criterion).

Run: `cargo test -p xai-grok-sampler --lib 2>&1 | tail -5` and `cargo test -p xai-grok-shell --lib sampling_config_for_external_no_key` → all PASS (these should be green immediately; if the shell test is RED, Tasks 3-4 are incomplete — fix there, not here).

**Step 3: Commit** — `test(security): pin credential-free SamplerConfig and wire boundary for external origins (#110)`

---

## Task 6: Auxiliary + web-search path hardening

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` — `resolve_aux_model_sampling_config` (5461, ambient fallback at 5502-5560), `resolve_web_search_sampling_config` (5871)

**Step 1: Failing tests**

```rust
/// #110: the aux fallback mints an ambient xAI bearer (session → XAI_API_KEY
/// → deployment_key) onto resolve_inference_base_url(). If a custom
/// models_base_url points that at a non-first-party origin, refuse.
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

/// The aux fallback labels its ambient source truthfully (it stuffs the
/// bearer into a synthetic entry's api_key, which would otherwise mislabel
/// as ModelApiKey and dodge the sampler assertion).
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

**Check the real `EnvKeys`/`EndpointsConfig` field names before writing** (`models_base_url` verified at `config.rs:411-415` via `resolve_inference_base_url`; `EndpointsConfig::default()` usage verified in existing tests, e.g. `none_aux_model_resolves_without_api_key`).

**Step 2: Prove RED** — `cargo test -p xai-grok-shell --lib aux_fallback_refuses -- --nocapture; cargo test -p xai-grok-shell --lib web_search_disables_for_unready 2>&1 | tail -8`. Expected: fallback currently returns `Some` with the sentinel in `api_key`; web search currently returns `Some` (with the session key pre-Task-3, credential-stripped after Task 4 — either way `Some`, so RED).

**Step 3: Implement.**

In `resolve_aux_model_sampling_config`, replace the `xai_bearer` fallback block (5502-5560):

```rust
    // #110: this fallback attaches an ambient xAI bearer; it may only target
    // a first-party xAI origin. Classify the URL it will actually use.
    let inference_base = endpoints.resolve_inference_base_url();
    if !ambient_xai_credential_allowed(&inference_base, AuthScheme::Bearer) {
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

In `resolve_web_search_sampling_config`, at the top of the catalog-entry branch (5880):

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

**Step 4: GREEN + neighbors** — `cargo test -p xai-grok-shell --lib aux_ ; cargo test -p xai-grok-shell --lib web_search 2>&1 | tail -10` (existing `none_aux_model_resolves_without_api_key`, web_search e2e, and `resolve_web_search_sampling_config_preflight` Codex tests must stay green — the Codex preflight path is untouched).

**Step 5: Commit** — `fix(security): gate auxiliary/web-search sampling on origin and readiness (#110)`

---

## Task 7: Session bearer-RESOLVER attach gate (turn-time refresh path)

The resolver path can attach a live session bearer independent of `api_key`. Today `session_token_auth_gate` (`auth_method.rs:432`) allows `NotByok` on ANY endpoint, and both attach sites classify first-party with loopback-permissive `is_xai_api_url`.

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/auth_method.rs` (`session_token_auth_gate`, 432)
- Modify: `crates/codegen/xai-grok-shell/src/session/acp_session_impl/sampler_turn.rs` (`SessionTokenAuthGate::new`, ~55-68)
- Modify: `crates/codegen/xai-grok-shell/src/agent/subagent/mod.rs` (`session_bearer_resolver`, 757)

**Step 1: Failing test** (in `auth_method.rs` tests, or `config.rs` tests if `auth_method.rs` has none — check for an existing `#[cfg(test)]` there first):

```rust
#[test]
fn session_token_auth_gate_requires_first_party_for_not_byok() {
    use crate::agent::auth_method::{session_token_auth_gate, ModelByok};
    // NotByok on a non-first-party endpoint must NOT attach a session resolver
    assert!(!session_token_auth_gate(true, ModelByok::NotByok, false));
    // First-party keeps the existing behavior
    assert!(session_token_auth_gate(true, ModelByok::NotByok, true));
    assert!(!session_token_auth_gate(true, ModelByok::Byok, true));
    assert!(session_token_auth_gate(true, ModelByok::Unknown, true));
    assert!(!session_token_auth_gate(true, ModelByok::Unknown, false));
    assert!(!session_token_auth_gate(false, ModelByok::NotByok, true));
}
```

Run to prove RED: `cargo test -p xai-grok-shell --lib session_token_auth_gate_requires 2>&1 | tail -6` — expected failure on the first assertion (currently `NotByok => true`).

**Step 2: Implement**

- `auth_method.rs:432`: change the match arm `ModelByok::NotByok => true,` to `ModelByok::NotByok => endpoint_is_first_party,` and update the function's doc comment.
- `sampler_turn.rs` `SessionTokenAuthGate::new`: replace `endpoint_is_first_party: crate::util::is_xai_api_url(base_url),` with

```rust
            endpoint_is_first_party: xai_grok_sampler::classify_endpoint_trust(base_url, auth_scheme)
                == xai_grok_sampler::EndpointTrustClass::FirstPartyXai,
```

  (`auth_scheme` is already a parameter of `new` — verified at sampler_turn.rs:55-61.)
- `subagent/mod.rs:757` `session_bearer_resolver`: replace `crate::util::is_xai_api_url(base_url)` with the same classifier call using `xai_grok_sampler::AuthScheme::Bearer` (the caller at mod.rs:967 already refuses `AuthScheme::None` before calling, so Bearer is the only relevant scheme here — say so in a comment).

**Step 3: GREEN + blast radius**

```bash
cargo test -p xai-grok-shell --lib session_token_auth_gate
cargo test -p xai-grok-shell --lib agent::subagent 2>&1 | tail -10
cargo test -p xai-grok-shell --lib session 2>&1 | tail -15   # run_in_background
```
**Unverified:** `session/acp_session_tests/auth_error_no_retry_tests.rs` (e.g. `reconstruct_full_config_wires_bearer_resolver_for_session_method_despite_api_key_auth_type` at line 905) may use loopback or example.com base_urls that previously counted as first-party via `is_xai_api_url` loopback-acceptance. If any now refuse the resolver: fixtures whose SUBJECT is resolver wiring should move to `https://api.x.ai/v1` or the prod proxy URL; fixtures whose subject is refusal should assert the new refusal. Judge each by its doc comment.

**Step 4: Commit** — `fix(security): require canonical first-party origin before attaching session bearer resolvers (#110)`

---

## Task 8: Sampler defense-in-depth — construction-time assertion

**Files:**
- Modify: `crates/codegen/xai-grok-sampler/src/client.rs` (`SamplingClient::new`, insert right after `let endpoint_trust = resolve_endpoint_trust(&config);` at line ~796)

**Step 1: Failing tests** (client.rs tests mod; `minimal_config()` helper already exists there):

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
    // BYOK on external: fine
    SamplingClient::new(SamplerConfig {
        api_key: Some("sk-provider".into()),
        base_url: "https://api.openai.com/v1".into(),
        credential_source: Some(CredentialSource::ModelApiKey),
        ..minimal_config()
    })
    .expect("model-owned key on external endpoint");
    // ambient on first-party: fine
    SamplingClient::new(SamplerConfig {
        api_key: Some("session".into()),
        base_url: "https://api.x.ai/v1".into(),
        credential_source: Some(CredentialSource::XaiSession),
        ..minimal_config()
    })
    .expect("ambient credential on first-party endpoint");
    // legacy config without a source: unchanged behavior
    SamplingClient::new(SamplerConfig {
        api_key: Some("k".into()),
        base_url: "https://api.openai.com/v1".into(),
        credential_source: None,
        ..minimal_config()
    })
    .expect("legacy config unaffected");
}
```

Check what `minimal_config()` sets for `base_url`/`auth_scheme` before relying on it (it exists — grep `fn minimal_config` in client.rs).

**Step 2: Prove RED** — `cargo test -p xai-grok-sampler --lib ambient_xai_credential_cannot_construct 2>&1 | tail -8`. Expected: `expect_err` panics because construction currently succeeds.

**Step 3: Implement** — in `SamplingClient::new`, immediately after `endpoint_trust` is computed:

```rust
        // Defense in depth (#110): an ambient first-party xAI credential
        // bound to a non-first-party endpoint is an invalid configuration,
        // even if an upstream resolver regresses later. Secret-free error.
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

(`SamplingError::InvalidConfiguration(&'static str)` verified in use at client.rs:168. Note: an explicit `endpoint_trust: Some(FirstPartyXai)` override still bypasses — that override is an internal test facility, and the existing test at 3441 depends on it; do not remove it.)

**Step 4: GREEN + full sampler suite** — `cargo test -p xai-grok-sampler --lib 2>&1 | tail -5`.

**Step 5: Commit** — `fix(security): reject ambient xAI credentials for non-first-party endpoints at client construction (#110)`

---

## Task 9: `EffectiveModelRoute` — the typed seam PR 2 consumes

PR 1 produces the typed route object; PR 2 renders it (startup line, `/model` switch, `grok inspect`, ACP metadata). No UI here.

**Files:**
- Modify: `crates/codegen/xai-grok-shell/src/agent/config.rs` (new items near `ResolvedCredentials`)

**Step 1: Failing test**

```rust
#[test]
fn effective_model_route_is_secret_free_and_matches_the_sampler_url() {
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
    // route derives from the SAME URL the sampler will use
    let config = sampling_config_for_model(&m, resolve_credentials(&m, None), None, None, None, None);
    assert!(config.base_url.starts_with("https://user:"), "sampler keeps the raw url");
    // no secret window in any serialized form
    let json = serde_json::to_string(&route).unwrap();
    let debug = format!("{route:?}");
    for rendered in [json, debug] {
        for window in SECRET.as_bytes().windows(8) {
            let w = std::str::from_utf8(window).unwrap();
            assert!(!rendered.contains(w), "route leaked secret window {w}");
        }
    }
}
```

**Step 2: Prove RED** — compile error (`effective_model_route` not found).

**Step 3: Implement**

```rust
/// Scheme + host [+ port] [+ non-secret path]. Userinfo, query, and fragment
/// are dropped by construction (#110).
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

/// One secret-free, typed effective model route (#110). This is the object
/// PR 2 (startup/switch display, `grok inspect --json`, ACP metadata) must
/// consume instead of re-deriving labels. Derive it from the SAME
/// `ModelEntry` + `ResolvedCredentials` used to build the `SamplerConfig`
/// so the reported route cannot drift from the sampled route.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EffectiveModelRoute {
    pub catalog_id: String,
    pub wire_model: String,
    /// Not yet threaded: the `model_provider` id is consumed during config
    /// merging and is not retained on `ModelEntry`. PR 2 threads it if the
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
        endpoint_trust: xai_grok_sampler::classify_endpoint_trust(
            &credentials.base_url,
            credentials.auth_scheme,
        ),
        auth_scheme: credentials.auth_scheme,
        credential_source: credentials.source.clone(),
        ready,
        readiness_reason,
    }
}
```

**Step 4: GREEN** — `cargo test -p xai-grok-shell --lib effective_model_route`.

**Step 5: Commit** — `feat(shell): add secret-free EffectiveModelRoute derived from resolved credentials (#110)`

---

## Task 10: Non-regression sweep + repo gates

**Step 1: Run the issue's focused gates plus the touched suites** (long runs → `run_in_background`):

```bash
cargo test -p xai-grok-shell --lib agent::config
cargo test -p xai-grok-shell --lib agent::model_providers
cargo test -p xai-grok-shell --lib agent::subagent
cargo test -p xai-grok-shell --lib session
cargo test -p xai-grok-sampler --lib
cargo test -p xai-grok-sampling-types
cargo test -p xai-grok-shell --lib   # full lib
cargo fmt --all -- --check
cargo clippy -p xai-grok-shell -p xai-grok-sampler -p xai-grok-pager --lib --no-deps -- -D warnings
```

**Step 2: Codex-preservation checklist** (all must be green with ZERO edits to Codex code paths — if any needs an edit, STOP and re-examine your change instead):
- `cargo test -p xai-grok-sampler --lib codex` (endpoint allowlist, header retention, UA policy)
- `cargo test -p xai-grok-shell --lib openai_codex` (provider isolation, custom-origin refusal — e.g. `direct_openai_codex_auth_provider_cannot_authenticate_a_custom_origin` in model_providers.rs)
- First-party metadata boundary tests in the sampler (`enforce_external_metadata_boundary` suite) unchanged.

**Step 3: Fix stragglers, re-run until fully green. Commit** — `test: non-regression sweep for #110 PR1`

---

## Task 11: Core documentation migration (Phase D, PR-1 scope)

**Files:**
- Modify: `crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md`

**Step 1: Update the `auth_scheme` table** (~line 118). Change the `"bearer"` row to:

```markdown
| `"bearer"` | Default. Sends `Authorization: Bearer <key>` from `api_key` / `env_key`. The ambient fallback (session token / `XAI_API_KEY`) applies **only when the final effective URL is a first-party xAI origin**. |
```

**Step 2: Rewrite the "Credential Resolution" section** (~lines 130-137). Replace the 4-item list with:

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

**Step 3: Update the keyless-local note** (~line 380) from "Grok may still inherit ambient xAI credentials..." to:

```markdown
Keyless local servers need an explicit `auth_scheme = "none"`. Without it, the entry declares Bearer auth with no credential and is marked **unready** (ambient xAI credentials are never attached to a local endpoint).
```

**Step 4: Security/release note.** Verified: the repo has NO top-level `CHANGELOG*`/`RELEASE*` file. Put the security note in the PR description (breaking change: credential-less custom External/Local models become unready; migration = the two bullets above) and add one sentence to the doc section from Step 2. If the maintainer keeps release notes elsewhere (e.g. GitHub Releases), flag it in the PR body for them.

**Step 5: Verify docs render + no stale claims:**

```bash
grep -n "ambient" crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md
grep -rn "session / ambient fallback" crates/codegen/xai-grok-pager/docs/ && echo "STALE TEXT REMAINS" || echo "clean"
```

**Step 6: Commit** — `docs(security): document first-party-only ambient credential fallback and migration (#110)`

---

## Definition of done (PR 1)

Every checkbox maps to an acceptance criterion of #110 that PR 1 owns:

- [ ] External/Local endpoint + no explicit credential source → unready before any request (Task 4), blocked at selection/restore/switch/spawn via the existing unready enforcement (verified call sites listed in "Key verified facts").
- [ ] `XAI_SESSION_SENTINEL` / `XAI_API_KEY_SENTINEL` never resolve for non-first-party finals — resolver level (Task 3), choke point (Task 5), resolver-attach (Task 7), sampler construction (Task 8), aux/web-search (Task 6).
- [ ] First-party xAI no-key models keep the existing session/`XAI_API_KEY` flow (Tasks 3-4 first-party rows).
- [ ] `auth_scheme = "none"` stays ready and header-free (Task 4 ready row + Task 5 wire case).
- [ ] `api_key` / `env_key` / `auth_provider` / explicit-header configs still work externally (Tasks 3-4 rows); cold provider never falls to xAI (pre-existing behavior, pinned by existing model_providers tests + Task 3 chain order).
- [ ] Codex profile untouched and green (Task 10 Step 2).
- [ ] No secret values in any new type's Debug/serde/log/error output (Tasks 5/8/9 sentinel-window assertions).
- [ ] Gate evaluates the FINAL merged URL (provider-inheritance test in Task 3; `api_base_url` split test in Task 3).
- [ ] PR 2 seam exists: `EffectiveModelRoute` + `effective_model_route` (Task 9) — PR 2 must consume it, not re-derive.
- [ ] `cargo fmt --check` and the issue's clippy gate pass (Task 10).

## Explicitly out of scope (PR 2 — do not build here)

Startup route line, model-switch confirmation display, `grok inspect` human/JSON route report (`crates/codegen/xai-grok-shell/src/inspect/`), ACP `_meta` route fields beyond the existing `ready`/`readinessReason` (which change value automatically via Task 4), snapshot tests for display widths.

## Items I could NOT verify against the real code (executor must check before relying on them)

1. **`EnvKeys::primary()`-vs-winning-name**: Task 2 proposes `EnvKeys::resolve_name_value()`; confirm no existing equivalent before adding (only `resolve_value`/`resolve_value_with` were verified at config.rs:113-130).
2. **`AuthProviderRef.name` field**: inferred from `provider.name.as_str()` usage in model_providers.rs tests; confirm the field path when labeling `CredentialSource::AuthProvider`.
3. **Exact set of legacy tests that break** in Tasks 3/4/7 beyond the four enumerated `resolve_credentials_*` tests — the listed grep sweeps enumerate them at execution time; triage rules are given per task.
4. **`minimal_config()` contents** in sampler client.rs tests (exists, contents unread) — check `base_url`/`auth_scheme` defaults before reusing in Task 8.
5. **`EndpointsConfig` literal construction** in Task 6's test (`models_base_url: Some(...)` + `..Default::default()`) — confirm the struct derives `Default` and the field is public (usage `EndpointsConfig::default()` and `endpoints.deployment_key` verified; the full field list was not read).
6. **`resolve_models_from_toml` exact signature** (name/line verified; arity inferred from one call site).
7. **Whether any `SamplerConfig` full literal outside the files read** lacks `..Default::default()` — the compiler enumerates these in Task 2 Step 7.
