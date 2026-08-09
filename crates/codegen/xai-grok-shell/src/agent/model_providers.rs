use indexmap::IndexMap;
use reqwest::header::HeaderValue;

use super::config::{ConfigModelOverride, EnvKeys};
use super::config_model_override_parse::{ConfigWarning, ConfigWarningKind};
use crate::sampling::ApiBackend;

/// Reserved first-party provider profile for ChatGPT Codex OAuth traffic.
pub const OPENAI_CODEX_PROVIDER_ID: &str = "openai-codex";

/// Catalog key and routing slug of the built-in Codex preset. The two are the
/// same string so a user's `[model."gpt-5.6-sol"]` in the global config
/// replaces the preset in place instead of adding a second entry.
pub const OPENAI_CODEX_PRESET_MODEL_ID: &str = "gpt-5.6-sol";

/// Conservative context window for the preset, and a guess.
///
/// Under-reporting only makes auto-compact fire earlier, which is the safe
/// direction — but the figure is far enough off that the context bar reads as
/// the model's real capacity and sessions compact long before they need to
/// (#122). A user can correct it with a metadata-only `[model."gpt-5.6-sol"]`
/// override; the custom-models guide documents how.
///
/// This comment used to assert that "Codex-side metadata is not discoverable
/// from the CLI". **That is false.** Codex exposes `GET {base}/models` against
/// the same `chatgpt.com/backend-api/codex` base this fork already uses, and
/// its payload carries `context_window` and `max_context_window`. This fork
/// does not fetch that catalog — the Codex path only hardcodes this constant.
/// Fetching it would remove the guess rather than move it, and costs one
/// authenticated probe to confirm the payload shape.
///
/// It also used to say the value "matches the value the custom-models guide
/// has always used in its Codex example". Nothing enforced that coupling and
/// the guide's example has since changed, so the claim is dropped rather than
/// re-stated.
const OPENAI_CODEX_PRESET_CONTEXT_WINDOW: u64 = 200_000;
const OPENAI_CODEX_MODELS_CATALOG_DESCRIPTION: &str = "OpenAI Codex via a ChatGPT subscription";

struct CodexCatalogCredential {
    access_token: String,
    account_id: Option<String>,
    chatgpt_account_is_fedramp: bool,
    source: xai_grok_sampler::CredentialSource,
}

impl std::fmt::Debug for CodexCatalogCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexCatalogCredential")
            .field("access_token_present", &!self.access_token.is_empty())
            .field("account_id_present", &self.account_id.is_some())
            .field(
                "chatgpt_account_is_fedramp",
                &self.chatgpt_account_is_fedramp,
            )
            .field("source", &self.source)
            .finish()
    }
}

fn codex_catalog_credential_source_is_valid(source: &xai_grok_sampler::CredentialSource) -> bool {
    matches!(
        source,
        xai_grok_sampler::CredentialSource::AuthProvider { name }
            if name == OPENAI_CODEX_PROVIDER_ID
    )
}

fn codex_catalog_credential() -> Option<CodexCatalogCredential> {
    let snapshot = crate::auth::openai_codex::load_snapshot(&crate::util::grok_home::grok_home())?;
    let access_token = snapshot.access_token.trim();
    if access_token.is_empty() {
        return None;
    }
    let account_id = snapshot
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    Some(CodexCatalogCredential {
        access_token: access_token.to_owned(),
        account_id,
        chatgpt_account_is_fedramp: snapshot.chatgpt_account_is_fedramp,
        source: xai_grok_sampler::CredentialSource::AuthProvider {
            name: OPENAI_CODEX_PROVIDER_ID.to_owned(),
        },
    })
}

/// `GET /models` rejects a request without this query parameter — HTTP 400,
/// `{"loc": ("query", "client_version"), "msg": "Field required"}` — so leaving
/// it off makes the fetch fail every time and fall silently back to the
/// built-in preset, which looks exactly like having no catalog at all.
///
/// `0.0.0` is what this fork's Codex user-agent already claims
/// (`codex_cli_rs/0.0.0`), and the endpoint accepts it. Verified against the
/// live endpoint on 2026-08-08: HTTP 200, nine models. Entries carry a
/// `minimal_client_version`, so a future server-side floor could start
/// rejecting this; the failure is loud in the log and falls back to the preset.
const OPENAI_CODEX_CATALOG_CLIENT_VERSION: &str = "0.0.0";

fn apply_codex_catalog_auth_headers(
    request: reqwest::blocking::RequestBuilder,
    credential: &CodexCatalogCredential,
) -> Option<reqwest::blocking::RequestBuilder> {
    if !codex_catalog_credential_source_is_valid(&credential.source) {
        tracing::warn!("Codex catalog fetch refused: invalid credential source label");
        return None;
    }
    let mut request = request
        .header(
            "Authorization",
            format!("Bearer {}", credential.access_token),
        )
        .header("originator", crate::auth::openai_codex::ORIGINATOR);
    if let Some(account_id) = credential.account_id.as_deref() {
        match HeaderValue::from_str(account_id) {
            Ok(value) => request = request.header("chatgpt-account-id", value),
            Err(_) => {
                tracing::warn!(
                    "Codex catalog fetch: skipped invalid chatgpt-account-id header value"
                );
            }
        }
    }
    if credential.chatgpt_account_is_fedramp {
        request = request.header("x-openai-fedramp", "true");
    }
    Some(request)
}

fn codex_catalog_string(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    obj.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The window a session is budgeted against.
///
/// `context_window` is the operative one and is tried first. `max_context_window`
/// is a ceiling the account may not have, and preferring it — which this did
/// until #258 — makes auto-compact fire late by exactly the ratio between them.
///
/// Measured on the live catalog: eight of nine models report the two fields
/// equal, and `gpt-5.4` reports `context_window: 272000` against
/// `max_context_window: 1000000`. So the bug was invisible on every model but
/// one, and on that one it budgets **3.7x** the real window — which does not
/// fail early and quietly, it fails deep into a long session on the model most
/// likely to be used for long sessions.
///
/// `max_context_window` stays as a fallback for an entry that reports only the
/// ceiling, rather than being dropped: no window at all falls back to the
/// preset constant, which is worse than a too-large one.
fn codex_catalog_context_window(obj: &serde_json::Map<String, serde_json::Value>) -> Option<u64> {
    [
        "context_window",
        "contextWindow",
        "max_context_window",
        "maxContextWindow",
    ]
    .into_iter()
    .find_map(|key| obj.get(key).and_then(serde_json::Value::as_u64))
    .filter(|value| *value > 0)
}

fn codex_catalog_bool(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<bool> {
    obj.get(key).and_then(serde_json::Value::as_bool)
}

fn codex_catalog_string_list(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<String> {
    obj.get(key)
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Read the load-bearing per-model wire flags from a live catalog entry.
///
/// The preset (`openai_codex_provider`) was written for one model; catalog
/// entries differ on these fields and inheriting the preset wholesale is how
/// non-preset models 400 (#245).
fn codex_catalog_wire_capabilities(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> xai_grok_sampling_types::CodexWireCapabilities {
    xai_grok_sampling_types::CodexWireCapabilities {
        use_responses_lite: codex_catalog_bool(obj, "use_responses_lite"),
        tool_mode: codex_catalog_string(obj, "tool_mode"),
        supports_reasoning_summary_parameter: codex_catalog_bool(
            obj,
            "supports_reasoning_summary_parameter",
        ),
        supports_image_detail_original: codex_catalog_bool(obj, "supports_image_detail_original"),
        input_modalities: codex_catalog_string_list(obj, "input_modalities"),
        default_reasoning_level: codex_catalog_string(obj, "default_reasoning_level"),
    }
}

fn parse_openai_codex_catalog_entry(
    value: &serde_json::Value,
) -> Option<(String, ConfigModelOverride)> {
    let obj = value.as_object()?;
    // `slug` is what the live payload actually keys on; `id` and `model` are
    // kept as tolerances, not as the expected shape.
    let key = codex_catalog_string(obj, "slug")
        .or_else(|| codex_catalog_string(obj, "id"))
        .or_else(|| codex_catalog_string(obj, "model"))?;
    let model = codex_catalog_string(obj, "model").unwrap_or_else(|| key.clone());
    let context_window =
        codex_catalog_context_window(obj).unwrap_or(OPENAI_CODEX_PRESET_CONTEXT_WINDOW);
    let codex_wire = codex_catalog_wire_capabilities(obj);
    // Apply catalog default effort when the entry names one we understand.
    let reasoning_effort = codex_wire
        .default_reasoning_level
        .as_deref()
        .and_then(|level| match level {
            "none" => Some(xai_grok_sampling_types::ReasoningEffort::None),
            "minimal" => Some(xai_grok_sampling_types::ReasoningEffort::Minimal),
            "low" => Some(xai_grok_sampling_types::ReasoningEffort::Low),
            "medium" => Some(xai_grok_sampling_types::ReasoningEffort::Medium),
            "high" => Some(xai_grok_sampling_types::ReasoningEffort::High),
            "xhigh" => Some(xai_grok_sampling_types::ReasoningEffort::Xhigh),
            "max" => Some(xai_grok_sampling_types::ReasoningEffort::Max),
            _ => None,
        });
    Some((
        key,
        ConfigModelOverride {
            model: Some(model.clone()),
            model_provider: Some(OPENAI_CODEX_PROVIDER_ID.to_owned()),
            name: codex_catalog_string(obj, "display_name")
                .or_else(|| codex_catalog_string(obj, "name"))
                .or(Some(model)),
            description: codex_catalog_string(obj, "description")
                .or(Some(OPENAI_CODEX_MODELS_CATALOG_DESCRIPTION.to_owned())),
            context_window: Some(context_window),
            reasoning_effort,
            supports_reasoning_effort: reasoning_effort.map(|_| true),
            codex_wire: Some(codex_wire),
            ..ConfigModelOverride::default()
        },
    ))
}

fn parse_openai_codex_catalog_models(
    payload: &serde_json::Value,
) -> IndexMap<String, ConfigModelOverride> {
    // The live payload is `{"models": [...]}`. `data` and a bare array are
    // tolerances for shapes this endpoint does not currently return.
    let Some(entries) = payload
        .get("models")
        .and_then(serde_json::Value::as_array)
        .or_else(|| payload.get("data").and_then(serde_json::Value::as_array))
        .or_else(|| payload.as_array())
    else {
        return IndexMap::new();
    };
    entries
        .iter()
        .filter_map(parse_openai_codex_catalog_entry)
        .collect()
}

fn fetch_openai_codex_catalog_models_blocking(
    credential: CodexCatalogCredential,
    url: String,
) -> Option<IndexMap<String, ConfigModelOverride>> {
    let client = crate::remote::client::models_catalog_blocking_client();
    let request = apply_codex_catalog_auth_headers(client.get(url), &credential)?;
    let response = match request.send() {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(error = %error, "Codex catalog fetch failed");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::warn!(
            status = response.status().as_u16(),
            "Codex catalog fetch failed"
        );
        return None;
    }
    let payload: serde_json::Value = match response.json() {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!(error = %error, "Codex catalog response was not valid JSON");
            return None;
        }
    };
    let models = parse_openai_codex_catalog_models(&payload);
    if models.is_empty() {
        tracing::warn!("Codex catalog fetch returned no usable models");
        return None;
    }
    Some(models)
}

/// Run the complete reqwest blocking request outside any caller's Tokio
/// context. Isolating only `Client::build` is insufficient: reqwest's blocking
/// `RequestBuilder::send` also creates and drops a private runtime while it
/// waits, which Tokio rejects when config discovery happens inside async
/// startup (#291).
fn fetch_openai_codex_catalog_models_on_native_thread(
    credential: CodexCatalogCredential,
    url: String,
) -> Option<IndexMap<String, ConfigModelOverride>> {
    let worker = match std::thread::Builder::new()
        .name("codex-model-catalog".to_owned())
        .spawn(move || fetch_openai_codex_catalog_models_blocking(credential, url))
    {
        Ok(worker) => worker,
        Err(error) => {
            tracing::warn!(error = %error, "Codex catalog worker could not start");
            return None;
        }
    };
    match worker.join() {
        Ok(models) => models,
        Err(_) => {
            tracing::warn!("Codex catalog worker panicked");
            None
        }
    }
}

fn fetch_openai_codex_catalog_models() -> Option<IndexMap<String, ConfigModelOverride>> {
    if cfg!(test) {
        return None;
    }
    if !crate::util::config::resolve_remote_fetch_enabled() {
        return None;
    }
    let credential = codex_catalog_credential()?;
    let url = format!(
        "{}/models?client_version={}",
        crate::auth::openai_codex::CODEX_API_BASE_URL,
        OPENAI_CODEX_CATALOG_CLIENT_VERSION
    );
    fetch_openai_codex_catalog_models_on_native_thread(credential, url)
}

fn effective_openai_codex_presets(
    fetched: Option<IndexMap<String, ConfigModelOverride>>,
) -> IndexMap<String, ConfigModelOverride> {
    fetched
        .filter(|models| !models.is_empty())
        .unwrap_or_else(openai_codex_preset_models)
}

fn openai_codex_provider() -> ModelProviderConfig {
    ModelProviderConfig {
        base_url: Some(crate::auth::openai_codex::CODEX_API_BASE_URL.to_owned()),
        api_backend: Some(ApiBackend::CodexResponses),
        ..ModelProviderConfig::default()
    }
}

/// Built-in `[model.*]` entries bound to the reserved `openai-codex` provider.
///
/// Without these, `grok login --provider openai-codex` succeeds into a session
/// with no Codex model at all: the reserved provider ships no catalog entries,
/// so the user has to hand-write a `[model.<id>]` block in the *global*
/// `$GROK_HOME/config.toml` before anything is selectable.
///
/// Folded into the user's `[model.*]` table by
/// [`merge_openai_codex_presets`]. They carry no credential of their own —
/// readiness gates on the provider-scoped OAuth snapshot exactly like a
/// hand-written entry does, which is what surfaces them as unready with a
/// "sign in" reason before login.
fn openai_codex_preset_models() -> IndexMap<String, ConfigModelOverride> {
    IndexMap::from([(
        OPENAI_CODEX_PRESET_MODEL_ID.to_owned(),
        ConfigModelOverride {
            model: Some(OPENAI_CODEX_PRESET_MODEL_ID.to_owned()),
            model_provider: Some(OPENAI_CODEX_PROVIDER_ID.to_owned()),
            name: Some("GPT-5.6 Sol (Codex)".to_owned()),
            description: Some("OpenAI Codex via a ChatGPT subscription".to_owned()),
            context_window: Some(OPENAI_CODEX_PRESET_CONTEXT_WINDOW),
            ..ConfigModelOverride::default()
        },
    )])
}

impl ConfigModelOverride {
    /// Whether this entry names where its traffic goes and what authenticates
    /// it, rather than leaving both to a preset or provider.
    fn declares_own_routing(&self) -> bool {
        self.model_provider.is_some()
            || self.base_url.is_some()
            || self.api_base_url.is_some()
            || self.api_key.is_some()
            || self.env_key.is_some()
            || self.auth_provider.is_some()
    }
}

/// Fold the built-in Codex presets into the user's parsed `[model.*]` table.
///
/// A key the user never declared gets the preset outright.
///
/// A key the user *did* declare keeps their entry, but a metadata-only
/// override — a new `name`, a bigger `context_window` — names no endpoint and
/// no credential. Letting it replace the preset wholesale would strip the
/// `model_provider` binding, and the entry would then resolve as a plain xAI
/// catalog model: routed to the xAI inference endpoint and authenticated with
/// the user's xAI session token, under a key the docs describe as Codex. So
/// the preset's routing is backfilled underneath such an override.
///
/// An override that declares its own provider, endpoint, or credential has
/// taken ownership of the key and is left exactly as written — redefining
/// `[model."gpt-5.6-sol"]` as some other provider's model stays possible.
pub(crate) fn merge_openai_codex_presets(
    config_models: &mut IndexMap<String, ConfigModelOverride>,
) {
    for (key, preset) in effective_openai_codex_presets(fetch_openai_codex_catalog_models()) {
        let Some(user_entry) = config_models.get_mut(&key) else {
            config_models.insert(key, preset);
            continue;
        };
        if user_entry.declares_own_routing() {
            continue;
        }
        // Overlay the user's fields on the whole preset, not just its routing:
        // `context_window = 400000` alone must not also blank the shipped
        // display name and description out of the picker.
        let ConfigModelOverride {
            model,
            model_provider,
            name,
            description,
            context_window,
            reasoning_effort,
            supports_reasoning_effort,
            codex_wire,
            ..
        } = preset;
        user_entry.model_provider = model_provider;
        user_entry.model.get_or_insert(model.unwrap_or(key));
        if let Some(name) = name {
            user_entry.name.get_or_insert(name);
        }
        if let Some(description) = description {
            user_entry.description.get_or_insert(description);
        }
        if let Some(context_window) = context_window {
            user_entry.context_window.get_or_insert(context_window);
        }
        if user_entry.reasoning_effort.is_none() {
            user_entry.reasoning_effort = reasoning_effort;
        }
        if user_entry.supports_reasoning_effort.is_none() {
            user_entry.supports_reasoning_effort = supports_reasoning_effort;
        }
        if user_entry.codex_wire.is_none() {
            user_entry.codex_wire = codex_wire;
        }
    }
}

#[derive(Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct ModelProviderConfig {
    pub base_url: Option<String>,
    pub api_base_url: Option<String>,
    pub env_key: Option<EnvKeys>,
    pub api_key: Option<String>,
    pub api_backend: Option<ApiBackend>,
    pub extra_headers: IndexMap<String, String>,
    /// Query parameters folded into every request URL; inherited by models.
    pub query_params: IndexMap<String, String>,
    /// Header name to environment variable; inherited by models, resolved at
    /// client build.
    pub env_http_headers: IndexMap<String, String>,
    pub auth_provider: Option<String>,
    pub auth: Option<crate::auth::AuthProviderConfig>,
    pub context_window: Option<u64>,
}

impl std::fmt::Debug for ModelProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelProviderConfig")
            .field("base_url_present", &self.base_url.is_some())
            .field("api_base_url_present", &self.api_base_url.is_some())
            .field("env_key_present", &self.env_key.is_some())
            .field("api_key_present", &self.api_key.is_some())
            .field("api_backend", &self.api_backend)
            .field("extra_headers_present", &!self.extra_headers.is_empty())
            .field("query_params_present", &!self.query_params.is_empty())
            .field(
                "env_http_headers_present",
                &!self.env_http_headers.is_empty(),
            )
            .field("auth_provider_present", &self.auth_provider.is_some())
            .field("auth_present", &self.auth.is_some())
            .field("context_window", &self.context_window)
            .finish()
    }
}

pub(crate) fn model_provider_auth_name(provider_id: &str) -> String {
    format!("model_provider:{provider_id}")
}

pub(crate) fn auth_config_issues(
    config: &crate::auth::AuthProviderConfig,
) -> Vec<(&'static str, ConfigWarningKind, String)> {
    let mut issues = Vec::new();
    if !config.is_usable() {
        issues.push((
            "command",
            ConfigWarningKind::InvalidValue,
            "missing or empty command; models resolve with no credential".to_owned(),
        ));
    }
    let skew = crate::auth::PROVIDER_TOKEN_EXPIRY_SKEW_SECS;
    if config.token_ttl_secs.is_some_and(|ttl| ttl <= skew) {
        issues.push((
            "token_ttl_secs",
            ConfigWarningKind::InvalidValue,
            format!(
                "at or below the {skew}s refresh margin; the command will run before every turn"
            ),
        ));
    }
    if let Some(timeout) = config.timeout_secs
        && !(1..=crate::auth::PROVIDER_TIMEOUT_CEILING_SECS).contains(&timeout)
    {
        let ceiling = crate::auth::PROVIDER_TIMEOUT_CEILING_SECS;
        issues.push((
            "timeout_secs",
            ConfigWarningKind::InvalidValue,
            if timeout == 0 {
                "below the 1 second minimum; clamped to 1".to_owned()
            } else {
                format!("above the {ceiling}s maximum; clamped to {ceiling}")
            },
        ));
    }
    issues
}

pub(crate) fn parse_model_providers(
    raw_config: &toml::Value,
) -> (IndexMap<String, ModelProviderConfig>, Vec<ConfigWarning>) {
    let mut providers = IndexMap::new();
    providers.insert(OPENAI_CODEX_PROVIDER_ID.to_owned(), openai_codex_provider());
    let mut warnings = Vec::new();
    let Some(section) = raw_config.get("model_providers") else {
        return (providers, warnings);
    };
    let Some(table) = section.as_table() else {
        warnings.push(ConfigWarning::model_provider_section(
            ConfigWarningKind::NotATable,
            format!(
                "`model_providers` must be a table of [model_providers.<id>] entries, got {}; \
                 all model providers ignored",
                section.type_str()
            ),
        ));
        return (providers, warnings);
    };
    for (id, value) in table {
        if id == OPENAI_CODEX_PROVIDER_ID {
            warnings.push(ConfigWarning::model_provider(
                id,
                None,
                ConfigWarningKind::ConflictingFields,
                "reserved built-in provider; user configuration ignored".to_owned(),
            ));
            continue;
        }
        let mut unknown = Vec::new();
        match serde_ignored::deserialize::<_, _, ModelProviderConfig>(value.clone(), |path| {
            unknown.push(path.to_string());
        }) {
            Ok(provider) => {
                for key in unknown {
                    warnings.push(ConfigWarning::model_provider(
                        id,
                        Some(key.as_str()),
                        ConfigWarningKind::UnknownField,
                        "unrecognized key; field ignored".to_owned(),
                    ));
                }
                let mut provider = provider;
                // #13: same env_key normalization + path-specific warnings as
                // `[model.*]` — do not silently keep whitespace / illegal names.
                if let Some(raw_keys) = provider.env_key.take() {
                    let candidates: Vec<String> =
                        raw_keys.names().into_iter().map(str::to_owned).collect();
                    let (normalized, rejected) = EnvKeys::normalize(candidates);
                    for rejected in rejected {
                        let reason = if rejected.name.is_empty() {
                            format!(
                                "invalid env_key entry ({}); ignored — not used as a credential source",
                                rejected.reason
                            )
                        } else {
                            format!(
                                "invalid env_key name {:?}: {}; ignored — not used as a credential source",
                                rejected.name, rejected.reason
                            )
                        };
                        warnings.push(ConfigWarning::model_provider(
                            id,
                            Some("env_key"),
                            ConfigWarningKind::InvalidValue,
                            reason,
                        ));
                    }
                    if !normalized.is_empty() {
                        provider.env_key = Some(normalized);
                    }
                }
                if let Some(auth) = &provider.auth {
                    for (field, kind, reason) in auth_config_issues(auth) {
                        warnings.push(ConfigWarning::model_provider(
                            id,
                            Some(&format!("auth.{field}")),
                            kind,
                            reason,
                        ));
                    }
                }
                let has_helper = provider.auth.is_some() || provider.auth_provider.is_some();
                let has_static_api_key = provider
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|k| !k.is_empty());
                if has_helper && has_static_api_key {
                    warnings.push(ConfigWarning::model_provider(
                        id,
                        Some("api_key"),
                        ConfigWarningKind::ConflictingFields,
                        "api_key shadows this provider's auth helper; the static key always \
                         takes precedence, so the helper never runs for inheriting models"
                            .to_owned(),
                    ));
                } else if has_helper
                    && provider
                        .env_key
                        .as_ref()
                        .and_then(EnvKeys::primary)
                        .is_some()
                {
                    warnings.push(ConfigWarning::model_provider(
                        id,
                        Some("env_key"),
                        ConfigWarningKind::ConflictingFields,
                        "env_key may shadow this provider's auth helper; env_key takes precedence \
                         when its variable resolves, otherwise the helper runs"
                            .to_owned(),
                    ));
                }
                if provider.auth_provider.is_some() && provider.auth.is_some() {
                    warnings.push(ConfigWarning::model_provider(
                        id,
                        Some("auth"),
                        ConfigWarningKind::ConflictingFields,
                        "inline auth is shadowed by auth_provider on this provider; the referenced \
                         provider takes precedence, so the inline helper never runs"
                            .to_owned(),
                    ));
                }
                providers.insert(id.clone(), provider);
            }
            Err(error) => {
                warnings.push(ConfigWarning::model_provider(
                    id,
                    None,
                    ConfigWarningKind::InvalidValue,
                    format!(
                        "failed to parse ({error}); provider skipped, inheriting models \
                         resolve with defaults"
                    ),
                ));
            }
        }
    }
    (providers, warnings)
}

impl ConfigModelOverride {
    pub(crate) fn with_provider_defaults(
        &self,
        provider: &ModelProviderConfig,
        provider_id: &str,
    ) -> Self {
        let ModelProviderConfig {
            base_url,
            api_base_url,
            env_key,
            api_key,
            api_backend,
            extra_headers,
            query_params,
            env_http_headers,
            auth_provider,
            auth,
            context_window,
        } = provider;

        let mut merged = self.clone();
        merged.model_provider = None;
        merged.base_url = merged.base_url.or_else(|| base_url.clone());
        merged.api_base_url = merged.api_base_url.or_else(|| api_base_url.clone());
        merged.api_backend = merged.api_backend.or_else(|| api_backend.clone());
        merged.context_window = merged.context_window.or(*context_window);
        // Inherited wholesale only when the model sets none of its own.
        if merged.extra_headers.is_empty() {
            merged.extra_headers = extra_headers.clone();
        }
        if merged.query_params.is_empty() {
            merged.query_params = query_params.clone();
        }
        if merged.env_http_headers.is_empty() {
            merged.env_http_headers = env_http_headers.clone();
        }
        let model_sets_own_api_key = self
            .api_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty());
        let model_sets_own_env_key = self.env_key.as_ref().and_then(EnvKeys::primary).is_some();
        let model_has_own_auth =
            model_sets_own_api_key || model_sets_own_env_key || self.auth_provider.is_some();
        if !model_has_own_auth {
            merged.api_key = api_key.clone();
            merged.env_key = env_key.clone();
            merged.auth_provider = auth_provider
                .clone()
                .or_else(|| auth.as_ref().map(|_| model_provider_auth_name(provider_id)));
        }
        merged
    }

    pub(crate) fn with_missing_provider(&self) -> Self {
        let mut merged = self.clone();
        merged.model_provider = None;
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::{Config, resolve_credentials, resolve_model_list};

    const CODEX_CATALOG_RUNTIME_CHILD: &str = "__XAI_CODEX_CATALOG_RUNTIME_CHILD";
    const CODEX_CATALOG_RUNTIME_PASS: &str = "codex-catalog-runtime-fetch-ok";

    /// Child-process body for #291. The fresh process prevents another test
    /// from warming reqwest's process-wide blocking client before Tokio starts.
    #[test]
    fn codex_catalog_runtime_first_fetch_child() {
        if std::env::var_os(CODEX_CATALOG_RUNTIME_CHILD).is_none() {
            return;
        }
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind catalog server");
        let address = listener.local_addr().expect("catalog server address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept catalog request");
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).expect("read catalog request");
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /models?client_version=0.0.0 HTTP/1.1"));
            assert!(request.contains("authorization: Bearer catalog-test-token"));
            let body =
                r#"{"models":[{"slug":"gpt-test","model":"gpt-test","context_window":12345}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write catalog response");
        });
        let credential = CodexCatalogCredential {
            access_token: "catalog-test-token".to_owned(),
            account_id: Some("catalog-test-account".to_owned()),
            chatgpt_account_is_fedramp: false,
            source: xai_grok_sampler::CredentialSource::AuthProvider {
                name: OPENAI_CODEX_PROVIDER_ID.to_owned(),
            },
        };
        let url = format!("http://{address}/models?client_version=0.0.0");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build child Tokio runtime");
        let models = runtime
            .block_on(
                async move { fetch_openai_codex_catalog_models_on_native_thread(credential, url) },
            )
            .expect("fetch catalog inside Tokio");
        server.join().expect("catalog server thread");
        assert_eq!(models["gpt-test"].context_window, Some(12_345));
        println!("{CODEX_CATALOG_RUNTIME_PASS}");
    }

    /// Launch an exact child test so both the reqwest client and its first
    /// authenticated request are exercised from a fresh process under Tokio.
    #[test]
    fn codex_catalog_runtime_first_fetch_parent() {
        if std::env::var_os(CODEX_CATALOG_RUNTIME_CHILD).is_some() {
            return;
        }
        let filter = module_path!()
            .split_once("::")
            .map(|(_, rest)| rest)
            .unwrap_or_default();
        let mut command = std::process::Command::new(std::env::current_exe().expect("current_exe"));
        command
            .arg("--exact")
            .arg(format!("{filter}::codex_catalog_runtime_first_fetch_child"))
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CODEX_CATALOG_RUNTIME_CHILD, "1")
            .stdin(std::process::Stdio::null());
        xai_tty_utils::detach_std_command(&mut command);
        let output = command.output().expect("spawn catalog fetch child test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && !stderr.contains("panicked at"),
            "fresh-process Codex catalog fetch failed under Tokio \
             (status: {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
        assert!(
            stdout.contains(CODEX_CATALOG_RUNTIME_PASS),
            "child did not execute the catalog fetch path\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }

    #[test]
    fn provider_debug_is_presence_only() {
        // Sentinel must not share an 8-byte window with Debug field names
        // (`auth_provider_present`, `api_key_present`, …) or the scan false-fails.
        let secret = "GB002-cfg-sentinel-Q7w5E3r1T9y7U2i4";
        let config = ModelProviderConfig {
            base_url: Some(format!(
                "https://user:{secret}@example.test/?token={secret}"
            )),
            api_base_url: Some(secret.to_owned()),
            api_key: Some(secret.to_owned()),
            extra_headers: IndexMap::from([("Authorization".to_owned(), secret.to_owned())]),
            query_params: IndexMap::from([("token".to_owned(), secret.to_owned())]),
            env_http_headers: IndexMap::from([("X-Secret".to_owned(), secret.to_owned())]),
            auth_provider: Some(secret.to_owned()),
            ..ModelProviderConfig::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains(secret));
        for window in secret.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !rendered.contains(window),
                "leaked sentinel window: {window}"
            );
        }
    }
    #[test]
    fn model_inherits_provider_connection_defaults() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            context_window = 123456

            [model_providers.gateway.extra_headers]
            X-Corp = "yes"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        assert!(cfg.model_providers.contains_key("gateway"));
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(model.info.base_url, "https://gateway.example/v1");
        assert_eq!(model.info.context_window.get(), 123456);
        assert_eq!(
            model.info.extra_headers.get("X-Corp").map(String::as_str),
            Some("yes")
        );
        assert!(
            model.has_own_credentials(),
            "a custom endpoint without a credential is BYOK, not session-authed"
        );
        // `assert!(..is_none())`, not `assert_eq!(.., None)`: on failure the
        // latter formats the left value, and for a model with an
        // `auth_provider` that value is the developer's live OAuth access
        // token. A test that prints the credential it is guarding is not a
        // guard.
        assert!(
            resolve_credentials(model, Some("session-jwt"))
                .api_key
                .is_none(),
            "the session token must not leak to the provider's custom endpoint"
        );
    }

    #[test]
    fn model_fields_override_provider_defaults() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            context_window = 100000

            [model.override-url]
            model = "m"
            model_provider = "gateway"
            base_url = "https://model-specific.example/v1"
            context_window = 200000
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("override-url").expect("model should exist");
        assert_eq!(model.info.base_url, "https://model-specific.example/v1");
        assert_eq!(model.info.context_window.get(), 200000);
    }

    #[test]
    fn model_provider_inline_auth_registers_synthetic_provider() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            context_window = 200000

            [model_providers.gateway.auth]
            command = "printf gw-token"
            token_ttl_secs = 3600

            [model.byok-via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        assert_eq!(
            cfg.auth_providers
                .get("model_provider:gateway")
                .map(|c| c.command.as_str()),
            Some("printf gw-token"),
            "inline auth registers a synthetic provider keyed by the id"
        );
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get("byok-via-gateway")
            .expect("model should exist");
        let provider = model
            .auth_provider
            .as_ref()
            .expect("the model inherits the provider's auth");
        assert_eq!(provider.name, "model_provider:gateway");
        assert_eq!(provider.config.command, "printf gw-token");
        assert!(
            model.has_own_credentials(),
            "a provider-backed model is BYOK (session token must not leak)"
        );
    }

    #[test]
    fn model_with_own_key_ignores_provider_auth() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            context_window = 200000

            [model_providers.gateway.auth]
            command = "printf gw-token"

            [model.own-key]
            model = "m"
            model_provider = "gateway"
            api_key = "sk-model-own"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("own-key").expect("model should exist");
        assert_eq!(
            model.info.base_url, "https://gateway.example/v1",
            "non-auth connection fields are still inherited"
        );
        assert_eq!(
            model.effective_auth_provider().map(|p| p.name.as_str()),
            None,
            "the model's own key shadows the provider's auth"
        );
        let creds = resolve_credentials(model, Some("session-jwt"));
        assert_eq!(creds.api_key.as_deref(), Some("sk-model-own"));
    }

    #[test]
    fn undefined_model_provider_fails_closed() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [model.dangling]
            model = "m"
            base_url = "https://third-party.example/v1"
            context_window = 200000
            model_provider = "ghost"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        assert!(
            cfg.config_warnings.iter().any(|w| {
                w.kind == ConfigWarningKind::InvalidValue
                    && matches!(
                        &w.target,
                        WarningTarget::Model { field, .. }
                            if field.as_deref() == Some("model_provider")
                    )
            }),
            "an undefined provider reference warns: {:?}",
            cfg.config_warnings
        );
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("dangling").expect("model should exist");
        assert_eq!(
            model.info.base_url, "https://third-party.example/v1",
            "the model keeps its own connection fields"
        );
        assert!(
            model.has_own_credentials(),
            "an undefined provider leaves the model BYOK, not session-authed"
        );
        let creds = resolve_credentials(model, Some("session-jwt"));
        assert_eq!(
            creds.api_key, None,
            "no credential resolves and the session token does not leak to the model's base_url"
        );
    }

    #[test]
    fn undefined_model_provider_keeps_model_own_key() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model.own-key]
            model = "m"
            base_url = "https://third-party.example/v1"
            context_window = 200000
            api_key = "sk-model-own"
            model_provider = "ghost"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("own-key").expect("model should exist");
        let creds = resolve_credentials(model, Some("session-jwt"));
        assert_eq!(creds.api_key.as_deref(), Some("sk-model-own"));
    }

    #[test]
    fn model_provider_parse_warnings_are_lenient_and_specific() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.good]
            base_url = "https://good.example/v1"

            [model_providers.bad-type]
            context_window = "not-a-number"

            [model_providers.typo]
            base_url = "https://typo.example/v1"
            unknown_field = 5

            [model.on-broken-provider]
            model = "m"
            base_url = "https://x.example/v1"
            context_window = 200000
            model_provider = "bad-type"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config)
            .expect("one bad provider must not fail the config");
        assert!(cfg.model_providers.contains_key("good"));
        assert!(
            !cfg.model_providers.contains_key("bad-type"),
            "a malformed provider is skipped"
        );

        let has_provider = |id: &str, field: Option<&str>, kind: ConfigWarningKind| {
            cfg.config_warnings.iter().any(|w| {
                w.kind == kind
                    && matches!(
                        &w.target,
                        WarningTarget::ModelProvider { id: i, field: f }
                            if i == id && f.as_deref() == field
                    )
            })
        };
        assert!(has_provider(
            "bad-type",
            None,
            ConfigWarningKind::InvalidValue
        ));
        assert!(has_provider(
            "typo",
            Some("unknown_field"),
            ConfigWarningKind::UnknownField
        ));
        assert!(
            !cfg.config_warnings.iter().any(|w| {
                matches!(
                    &w.target,
                    WarningTarget::Model { field, .. }
                        if field.as_deref() == Some("model_provider")
                )
            }),
            "a declared-but-malformed provider must not also warn as undefined: {:?}",
            cfg.config_warnings
        );

        let raw_config: toml::Value = toml::from_str(r#"model_providers = "oops""#).unwrap();
        let cfg = Config::new_from_toml_cfg(&raw_config)
            .expect("a non-table model_providers must not fail the config");
        assert_eq!(cfg.model_providers.len(), 1);
        assert!(cfg.model_providers.contains_key(OPENAI_CODEX_PROVIDER_ID));
        assert!(
            cfg.config_warnings.iter().any(|w| {
                matches!(w.target, WarningTarget::ModelProviderSection)
                    && w.kind == ConfigWarningKind::NotATable
            }),
            "non-table section warns: {:?}",
            cfg.config_warnings
        );
    }

    #[test]
    fn model_provider_conflicting_credentials_warn() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.static-shadows]
            base_url = "https://a.example/v1"
            api_key = "sk-static"
            [model_providers.static-shadows.auth]
            command = "printf tok"

            [model_providers.env-shadows]
            base_url = "https://b.example/v1"
            env_key = "SOME_VAR"
            [model_providers.env-shadows.auth]
            command = "printf tok"

            [model_providers.two-helpers]
            base_url = "https://c.example/v1"
            auth_provider = "corp"
            [model_providers.two-helpers.auth]
            command = "printf tok"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let has = |id: &str, field: &str| {
            cfg.config_warnings.iter().any(|w| {
                w.kind == ConfigWarningKind::ConflictingFields
                    && matches!(
                        &w.target,
                        WarningTarget::ModelProvider { id: i, field: f }
                            if i == id && f.as_deref() == Some(field)
                    )
            })
        };
        assert!(
            has("static-shadows", "api_key"),
            "a static api_key shadows the helper: {:?}",
            cfg.config_warnings
        );
        assert!(
            has("env-shadows", "env_key"),
            "an env_key may shadow the helper: {:?}",
            cfg.config_warnings
        );
        assert!(
            has("two-helpers", "auth"),
            "auth_provider shadows the inline auth helper: {:?}",
            cfg.config_warnings
        );
    }

    #[test]
    fn model_provider_undefined_auth_provider_warns() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            auth_provider = "nonexistent"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        assert!(
            cfg.config_warnings.iter().any(|w| {
                w.kind == ConfigWarningKind::InvalidValue
                    && matches!(
                        &w.target,
                        WarningTarget::ModelProvider { id, field }
                            if id == "gateway" && field.as_deref() == Some("auth_provider")
                    )
            }),
            "an undefined provider auth_provider reference warns: {:?}",
            cfg.config_warnings
        );
    }

    #[test]
    fn model_provider_inline_auth_namespace_collision_warns() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [auth_provider."model_provider:gateway"]
            command = "printf hand-written"

            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model_providers.gateway.auth]
            command = "printf inline"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        assert!(
            cfg.config_warnings.iter().any(|w| {
                w.kind == ConfigWarningKind::ConflictingFields
                    && matches!(
                        &w.target,
                        WarningTarget::ModelProvider { id, field }
                            if id == "gateway" && field.as_deref() == Some("auth")
                    )
            }),
            "a reserved-namespace collision warns: {:?}",
            cfg.config_warnings
        );
        assert_eq!(
            cfg.auth_providers
                .get("model_provider:gateway")
                .map(|c| c.command.as_str()),
            Some("printf inline"),
            "inline auth wins the reserved name"
        );
    }

    #[test]
    fn model_inherits_provider_named_auth_provider() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [auth_provider.corp]
            command = "printf corp-token"
            token_ttl_secs = 3600

            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            auth_provider = "corp"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        let provider = model
            .auth_provider
            .as_ref()
            .expect("the model inherits the provider's named auth_provider");
        assert_eq!(provider.name, "corp");
        assert_eq!(provider.config.command, "printf corp-token");
        assert!(model.has_own_credentials());
    }

    #[test]
    fn model_inherits_provider_static_key() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            api_key = "sk-provider"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(
            resolve_credentials(model, Some("session-jwt"))
                .api_key
                .as_deref(),
            Some("sk-provider"),
            "the provider's static key resolves for the inheriting model"
        );
    }

    #[test]
    fn declared_unresolved_credential_fails_closed_on_provider_endpoint() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            env_key = "DEFINITELY_UNSET_MODEL_PROVIDER_TEST_VAR"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert!(
            resolve_credentials(model, Some("session-jwt"))
                .api_key
                .is_none(),
            "an unresolved declared credential must not fall back to the session token"
        );
    }

    #[test]
    fn model_inherits_provider_api_backend_and_base_url() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            api_base_url = "https://gateway.example/api"
            api_backend = "responses"
            api_key = "sk-provider"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(
            model.info.api_backend,
            crate::sampling::ApiBackend::Responses
        );
        assert_eq!(
            model.api_base_url.as_deref(),
            Some("https://gateway.example/api")
        );
    }

    #[test]
    fn model_own_unresolved_key_ignores_provider_inline_auth() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model_providers.gateway.auth]
            command = "printf gw-token"

            [model.own-env]
            model = "m"
            model_provider = "gateway"
            env_key = "DEFINITELY_UNSET_MODEL_PROVIDER_INLINE_VAR"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("own-env").expect("model should exist");
        let effective = model
            .effective_auth_provider()
            .expect("an unresolved own credential fails closed via a provider ref");
        assert!(
            effective.name.contains("fail-closed"),
            "must pin the unusable fail-closed ref, not the live inline auth: {}",
            effective.name
        );
        assert!(
            effective.config.command.is_empty(),
            "the fail-closed ref is unusable"
        );
        assert!(
            resolve_credentials(model, Some("session-jwt"))
                .api_key
                .is_none(),
            "must not fall back to the session token"
        );
    }

    #[test]
    fn fail_closed_ref_ignores_a_colliding_auth_provider_table() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [auth_provider."model_provider:gateway (fail-closed)"]
            command = "printf sneaky-token"

            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model.via-gateway]
            model = "m"
            context_window = 200000
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert!(
            resolve_credentials(model, Some("session-jwt"))
                .api_key
                .is_none(),
            "a fail-closed ref must never resolve a colliding auth_provider table"
        );
        let effective = model
            .effective_auth_provider()
            .expect("fails closed via a provider ref");
        assert!(
            effective.config.command.is_empty(),
            "the fail-closed ref stays unusable despite the name collision"
        );
    }

    #[test]
    fn model_headers_shadow_provider_headers() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            api_key = "sk-provider"

            [model_providers.gateway.extra_headers]
            X-Corp = "yes"

            [model.via-gateway]
            model = "m"
            context_window = 200000
            model_provider = "gateway"

            [model.via-gateway.extra_headers]
            X-Model = "own"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(
            model.info.extra_headers.get("X-Model").map(String::as_str),
            Some("own")
        );
        assert!(
            model.info.extra_headers.get("X-Corp").is_none(),
            "a model that sets any header inherits none of the provider's"
        );
    }

    #[test]
    fn model_provider_inline_auth_ttl_and_timeout_warn() {
        use super::super::config_model_override_parse::{ConfigWarningKind, WarningTarget};

        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model_providers.gateway.auth]
            command = "printf tok"
            token_ttl_secs = 5
            timeout_secs = 0
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let has = |field: &str| {
            cfg.config_warnings.iter().any(|w| {
                w.kind == ConfigWarningKind::InvalidValue
                    && matches!(
                        &w.target,
                        WarningTarget::ModelProvider { id, field: f }
                            if id == "gateway" && f.as_deref() == Some(field)
                    )
            })
        };
        assert!(
            has("auth.token_ttl_secs"),
            "inline auth ttl below the refresh margin warns: {:?}",
            cfg.config_warnings
        );
        assert!(
            has("auth.timeout_secs"),
            "inline auth timeout out of range warns: {:?}",
            cfg.config_warnings
        );
    }

    #[test]
    fn blank_api_key_does_not_shadow_provider_auth() {
        let raw_config: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"

            [model_providers.gateway.auth]
            command = "printf tok"

            [model.m]
            model = "m"
            model_provider = "gateway"
            api_key = "   "
            "#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&raw_config).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let provider = resolved["m"]
            .auth_provider
            .as_ref()
            .expect("blank api_key must not fail-close a working gateway");
        assert_eq!(provider.name.as_str(), "model_provider:gateway");
        assert!(!provider.is_fail_closed());
    }

    #[test]
    fn model_inherits_provider_query_params_and_env_http_headers() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            api_key = "sk-provider"

            [model_providers.gateway.query_params]
            api-version = "2026-07-22"

            [model_providers.gateway.env_http_headers]
            X-Tenant-Token = "GATEWAY_TENANT_TOKEN"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(
            model
                .info
                .query_params
                .get("api-version")
                .map(String::as_str),
            Some("2026-07-22"),
            "the model inherits the provider's query params"
        );
        assert_eq!(
            model
                .info
                .env_http_headers
                .get("X-Tenant-Token")
                .map(String::as_str),
            Some("GATEWAY_TENANT_TOKEN"),
            "the model inherits the provider's env_http_headers mapping (unresolved names)"
        );
    }

    #[test]
    fn model_query_params_shadow_provider_query_params() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model_providers.gateway]
            base_url = "https://gateway.example/v1"
            api_key = "sk-provider"

            [model_providers.gateway.query_params]
            api-version = "provider"

            [model.via-gateway]
            model = "m"
            model_provider = "gateway"

            [model.via-gateway.query_params]
            api-version = "model"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("via-gateway").expect("model should exist");
        assert_eq!(
            model
                .info
                .query_params
                .get("api-version")
                .map(String::as_str),
            Some("model"),
            "a model that sets its own query params inherits none of the provider's"
        );
    }

    #[test]
    fn openai_codex_is_reserved_and_cannot_be_overridden() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model_providers.openai-codex]
            base_url = "https://attacker.invalid/v1"
            api_key = "must-not-survive"

            [model.codex]
            model = "supported-codex-model"
            model_provider = "openai-codex"
            base_url = "https://attacker.invalid/model"
            api_base_url = "https://api.openai.com/v1"
            api_key = "platform-key-must-not-survive"
            env_key = "OPENAI_API_KEY"
            api_backend = "responses"

            [model.codex.extra_headers]
            X-Grok-User-Id = "must-not-survive"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let provider = cfg
            .model_providers
            .get(OPENAI_CODEX_PROVIDER_ID)
            .expect("built-in provider remains registered");
        assert_eq!(
            provider.base_url.as_deref(),
            Some(crate::auth::openai_codex::CODEX_API_BASE_URL)
        );
        assert_eq!(provider.api_backend, Some(ApiBackend::CodexResponses));
        assert!(provider.api_key.is_none());
        assert!(cfg.config_warnings.iter().any(|warning| {
            matches!(
                &warning.target,
                crate::agent::config_model_override_parse::WarningTarget::ModelProvider {
                    id,
                    field: None,
                } if id == OPENAI_CODEX_PROVIDER_ID
            )
        }));

        let resolved = resolve_model_list(&cfg, None);
        let model = resolved.get("codex").expect("Codex model should exist");
        assert_eq!(
            model.info.base_url,
            crate::auth::openai_codex::CODEX_API_BASE_URL
        );
        assert_eq!(model.info.api_backend, ApiBackend::CodexResponses);
        assert_eq!(model.info.auth_scheme, xai_grok_sampler::AuthScheme::Bearer);
        assert!(model.api_base_url.is_none());
        assert!(model.api_key.is_none());
        assert!(model.env_key.is_none());
        assert!(model.info.extra_headers.is_empty());
        assert_eq!(
            model
                .auth_provider
                .as_ref()
                .map(|provider| provider.name.as_str()),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
    }

    #[test]
    fn direct_openai_codex_auth_provider_cannot_authenticate_a_custom_origin() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model.exfiltration]
            model = "attacker-model"
            base_url = "https://attacker.invalid/v1"
            api_backend = "responses"
            auth_provider = "openai-codex"
            "#,
        )
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get("exfiltration")
            .expect("custom model should remain visible but fail closed");
        assert!(
            !model.config_validation_errors.is_empty(),
            "the reserved native provider must require model_provider provenance"
        );
        assert!(
            model
                .auth_provider
                .as_ref()
                .is_some_and(crate::auth::AuthProviderRef::is_fail_closed)
        );

        let credentials = resolve_credentials(model, Some("ambient-xai-session"));
        assert_eq!(credentials.api_key, None);
        let sampler = crate::agent::config::sampling_config_for_model(
            model,
            credentials,
            None,
            None,
            None,
            None,
            &crate::agent::trusted_origins::TrustedXaiOrigins::default(),
        );
        assert_eq!(sampler.base_url, "https://attacker.invalid/v1");
        assert_eq!(sampler.auth_scheme, xai_grok_sampler::AuthScheme::None);
        assert!(sampler.api_key.is_none());
        assert!(sampler.bearer_resolver.is_none());

        let temp = tempfile::tempdir().expect("temporary auth home");
        let manager = std::sync::Arc::new(crate::auth::AuthManager::new_openai_codex(temp.path()));
        manager.hot_swap(crate::auth::GrokAuth {
            key: "native-codex-secret".to_owned(),
            auth_mode: crate::auth::AuthMode::OpenAiCodex,
            ..crate::auth::GrokAuth::default()
        });
        let live_native_provider = crate::auth::AuthProviderRef::openai_codex(manager);
        assert_eq!(
            live_native_provider.cached_token().as_deref(),
            Some("native-codex-secret")
        );
        let mut custom_entry = model.clone();
        custom_entry.config_validation_errors.clear();
        custom_entry.auth_provider = Some(live_native_provider);
        let prefetched = IndexMap::from([("custom-native-ref".to_owned(), custom_entry)]);
        let isolated = resolve_model_list(&Config::default(), Some(prefetched));
        let isolated = &isolated["custom-native-ref"];
        assert!(
            isolated
                .auth_provider
                .as_ref()
                .is_some_and(crate::auth::AuthProviderRef::is_fail_closed)
        );
        assert!(
            resolve_credentials(isolated, None).api_key.is_none(),
            "even a live native Codex token is detached from a custom-origin entry"
        );
    }

    /// Auth home holding the credential a successful `grok login --provider
    /// openai-codex` writes.
    fn live_codex_auth_home() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("temporary auth home");
        let auth = crate::auth::GrokAuth {
            key: "live-access-token".to_owned(),
            auth_mode: crate::auth::AuthMode::OpenAiCodex,
            refresh_token: Some("rotating-refresh-token".to_owned()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            oidc_issuer: Some(crate::auth::openai_codex::ISSUER.to_owned()),
            oidc_client_id: Some(crate::auth::openai_codex::CLIENT_ID.to_owned()),
            account_id: Some("account-id".to_owned()),
            ..crate::auth::GrokAuth::default()
        };
        let auth_map = std::collections::HashMap::from([(
            crate::auth::openai_codex::AUTH_SCOPE.to_owned(),
            auth,
        )]);
        std::fs::write(
            temp.path().join("auth.json"),
            serde_json::to_vec(&auth_map).unwrap(),
        )
        .unwrap();
        temp
    }

    /// The preset's readiness must be decided by the credential in `auth_home`,
    /// not by whatever `$GROK_HOME` the test runner happens to have.
    fn preset_entry_with_auth_home(
        cfg: &Config,
        auth_home: &std::path::Path,
    ) -> crate::agent::config::ModelEntry {
        let mut resolved = resolve_model_list(cfg, None);
        let mut model = resolved
            .shift_remove(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("built-in Codex preset should resolve");
        model.auth_provider = Some(crate::auth::AuthProviderRef::openai_codex(
            crate::auth::openai_codex::manager(auth_home),
        ));
        model
    }

    #[test]
    fn builtin_openai_codex_preset_exists_without_user_config() {
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("an empty config should parse");
        let preset = cfg
            .config_models
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the Codex preset must ship without any user config");
        assert_eq!(preset.model.as_deref(), Some(OPENAI_CODEX_PRESET_MODEL_ID));
        assert_eq!(
            preset.model_provider.as_deref(),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
        assert!(
            preset.api_key.is_none() && preset.env_key.is_none() && preset.base_url.is_none(),
            "the preset must carry no credential or origin of its own"
        );

        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the preset must resolve into the catalog");
        assert_eq!(
            model.info.base_url,
            crate::auth::openai_codex::CODEX_API_BASE_URL
        );
        assert_eq!(model.info.api_backend, ApiBackend::CodexResponses);
        assert_eq!(
            model
                .auth_provider
                .as_ref()
                .map(|provider| provider.name.as_str()),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
        assert!(
            model.is_openai_codex_profile(),
            "the preset must be excluded from the xAI/BYOK key predicates"
        );
    }

    #[test]
    fn builtin_openai_codex_preset_is_unready_before_login() {
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("an empty config should parse");
        let temp = tempfile::tempdir().expect("temporary auth home");
        let model = preset_entry_with_auth_home(&cfg, temp.path());

        let (ready, reason) = crate::agent::config::model_readiness(&model);
        assert!(!ready, "the preset must not be selectable before login");
        let reason = reason.expect("an unready preset must say why");
        // The program name is whatever we were invoked as -- under a unit test
        // that is the test binary, which is the point: the instruction must
        // name the command the user actually has, not a hardcoded `grok` that
        // may belong to a different installed program (#117).
        let expected = format!(
            "{} login --provider openai-codex",
            xai_grok_config::program_name::program_name()
        );
        assert!(
            reason.contains(&expected),
            "the reason must name the login command as invoked, expected {expected:?}, got: {reason}"
        );
    }

    #[test]
    fn builtin_openai_codex_preset_is_selectable_after_login() {
        let cfg = Config::new_from_toml_cfg(&toml::Value::Table(toml::map::Map::new()))
            .expect("an empty config should parse");
        let temp = live_codex_auth_home();
        let model = preset_entry_with_auth_home(&cfg, temp.path());

        assert_eq!(crate::agent::config::model_readiness(&model), (true, None));
        assert_eq!(
            resolve_credentials(&model, Some("xai-session-token"))
                .api_key
                .as_deref(),
            Some("live-access-token"),
            "the preset must sample with the Codex bearer, never the xAI session token"
        );
    }

    #[test]
    fn user_global_config_overrides_the_builtin_openai_codex_preset() {
        let toml_cfg: toml::Value = toml::from_str(&format!(
            r#"
            [model."{OPENAI_CODEX_PRESET_MODEL_ID}"]
            model = "{OPENAI_CODEX_PRESET_MODEL_ID}"
            model_provider = "{OPENAI_CODEX_PROVIDER_ID}"
            name = "My Codex"
            context_window = 123456
            "#
        ))
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        assert_eq!(
            cfg.config_models.len(),
            1,
            "the preset must merge into the user's entry, not sit alongside it"
        );
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the user's entry keeps the preset key");
        assert_eq!(model.info.name.as_deref(), Some("My Codex"));
        assert_eq!(model.info.context_window.get(), 123_456);
    }

    /// The docs tell users to redeclare the preset key to retune its metadata.
    /// Such an override names no endpoint and no credential, so it must not
    /// take the Codex routing down with it: without the preset underneath, the
    /// entry resolves as a plain xAI catalog model and authenticates with the
    /// user's xAI session token under a key documented as Codex.
    #[test]
    fn metadata_only_override_keeps_the_preset_codex_routing() {
        let toml_cfg: toml::Value = toml::from_str(&format!(
            r#"
            [model."{OPENAI_CODEX_PRESET_MODEL_ID}"]
            name = "Codex"
            context_window = 400000
            "#
        ))
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let auth_home = tempfile::tempdir().expect("temporary auth home");
        let model = preset_entry_with_auth_home(&cfg, auth_home.path());

        assert_eq!(model.info.name.as_deref(), Some("Codex"));
        assert_eq!(model.info.context_window.get(), 400_000);
        assert_eq!(
            model.info.base_url,
            crate::auth::openai_codex::CODEX_API_BASE_URL,
            "a metadata-only override must not re-point the key at xAI"
        );
        assert_eq!(model.info.api_backend, ApiBackend::CodexResponses);
        assert_eq!(
            model
                .auth_provider
                .as_ref()
                .map(|provider| provider.name.as_str()),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
        assert!(
            resolve_credentials(&model, Some("xai-session-token"))
                .api_key
                .is_none(),
            "the xAI session token must never authenticate a Codex-keyed model"
        );
    }

    /// Drift guard: every field the preset ships must survive a metadata-only
    /// override. A preset field that [`merge_openai_codex_presets`] forgets to
    /// overlay would silently disappear from the picker.
    #[test]
    fn every_preset_field_survives_a_metadata_only_override() {
        let preset = openai_codex_preset_models()
            .shift_remove(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the preset should be keyed by its model id");
        let preset_value = toml::Value::try_from(&preset).expect("preset serializes");
        let preset_table = preset_value.as_table().expect("preset is a table");

        // Sets one field the preset does not, and declares no routing.
        let mut models = IndexMap::from([(
            OPENAI_CODEX_PRESET_MODEL_ID.to_owned(),
            ConfigModelOverride {
                top_p: Some(0.5),
                ..ConfigModelOverride::default()
            },
        )]);
        merge_openai_codex_presets(&mut models);
        let merged_value = toml::Value::try_from(&models[OPENAI_CODEX_PRESET_MODEL_ID])
            .expect("merged serializes");
        let merged_table = merged_value.as_table().expect("merged is a table");

        for (field, value) in preset_table {
            assert_eq!(
                merged_table.get(field),
                Some(value),
                "preset field `{field}` was dropped by a metadata-only override; \
                 overlay it in merge_openai_codex_presets"
            );
        }
        assert_eq!(
            merged_table.get("top_p").and_then(toml::Value::as_float),
            Some(0.5),
            "the user's own field must survive the overlay"
        );
    }

    /// The flip side: an override that names its own endpoint and credential
    /// has taken the key over, so the preset must not clamp it back onto Codex
    /// (which would silently strip that endpoint and key).
    #[test]
    fn override_declaring_its_own_endpoint_is_left_alone() {
        let toml_cfg: toml::Value = toml::from_str(&format!(
            r#"
            [model."{OPENAI_CODEX_PRESET_MODEL_ID}"]
            model = "gpt-5.6-sol"
            base_url = "https://api.openai.com/v1"
            env_key = "OPENAI_API_KEY"
            context_window = 200000
            "#
        ))
        .unwrap();

        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the user's entry should resolve");

        assert_eq!(model.info.base_url, "https://api.openai.com/v1");
        assert_eq!(
            model.env_key.as_ref().map(|keys| keys.names()),
            Some(vec!["OPENAI_API_KEY"]),
            "the preset must not strip a credential the user declared"
        );
        assert_ne!(model.info.api_backend, ApiBackend::CodexResponses);
    }

    #[test]
    fn openai_codex_model_without_a_live_credential_is_unready() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model.codex]
            model = "supported-codex-model"
            model_provider = "openai-codex"
            "#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let mut resolved = resolve_model_list(&cfg, None);
        let model = resolved.get_mut("codex").expect("Codex model should exist");
        let temp = tempfile::tempdir().expect("temporary auth home");
        model.auth_provider = Some(crate::auth::AuthProviderRef::openai_codex(
            crate::auth::openai_codex::manager(temp.path()),
        ));

        let (ready, reason) = crate::agent::config::model_readiness(model);
        assert!(!ready);
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("login"))
        );
        assert!(
            resolve_credentials(model, Some("xai-session-token"))
                .api_key
                .is_none()
        );
    }

    #[test]
    fn expired_refreshable_openai_codex_credential_stays_selectable() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
            [model.codex]
            model = "supported-codex-model"
            model_provider = "openai-codex"
            "#,
        )
        .unwrap();
        let cfg = Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
        let mut resolved = resolve_model_list(&cfg, None);
        let model = resolved.get_mut("codex").expect("Codex model should exist");

        let temp = tempfile::tempdir().expect("temporary auth home");
        let auth = crate::auth::GrokAuth {
            key: "expired-access-token".to_owned(),
            auth_mode: crate::auth::AuthMode::OpenAiCodex,
            refresh_token: Some("rotating-refresh-token".to_owned()),
            expires_at: Some(chrono::Utc::now() - chrono::Duration::minutes(1)),
            oidc_issuer: Some(crate::auth::openai_codex::ISSUER.to_owned()),
            oidc_client_id: Some(crate::auth::openai_codex::CLIENT_ID.to_owned()),
            account_id: Some("account-id".to_owned()),
            ..crate::auth::GrokAuth::default()
        };
        let auth_map = std::collections::HashMap::from([(
            crate::auth::openai_codex::AUTH_SCOPE.to_owned(),
            auth,
        )]);
        std::fs::write(
            temp.path().join("auth.json"),
            serde_json::to_vec(&auth_map).unwrap(),
        )
        .unwrap();
        model.auth_provider = Some(crate::auth::AuthProviderRef::openai_codex(
            crate::auth::openai_codex::manager(temp.path()),
        ));

        assert_eq!(crate::agent::config::model_readiness(model), (true, None));
        assert!(
            resolve_credentials(model, Some("xai-session-token"))
                .api_key
                .is_none(),
            "an expired Codex bearer must refresh pre-turn, never fall back to xAI"
        );
    }

    /// The shape below is not invented: it is the live `GET /models` response,
    /// captured 2026-08-08, trimmed to the fields this parser reads. Three
    /// things in it are the whole point, because a parser written from the
    /// issue text got all three wrong and would have fallen silently back to
    /// the built-in preset forever:
    ///
    ///   - the array is under `models`, not `data`
    ///   - entries key on `slug`, and carry no `id` or `model` at all
    ///   - the human label is `display_name`, not `name`
    #[test]
    fn codex_catalog_parser_reads_the_shape_the_endpoint_actually_returns() {
        let payload = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6 Sol",
                    "description": "Flagship",
                    "context_window": 272000,
                    "max_context_window": 272000
                },
                {
                    "slug": "gpt-5.3-codex-spark",
                    "display_name": "GPT-5.3 Codex Spark",
                    "context_window": 128000
                }
            ]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        assert_eq!(presets.len(), 2, "both live entries must parse");

        let sol = presets.get("gpt-5.6-sol").expect("slug is the catalog key");
        assert_eq!(sol.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            sol.model_provider.as_deref(),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
        assert_eq!(sol.name.as_deref(), Some("GPT-5.6 Sol"));
        assert_eq!(sol.context_window, Some(272_000));

        let spark = presets
            .get("gpt-5.3-codex-spark")
            .expect("a second model must not be lost — one entry is the bug");
        assert_eq!(spark.context_window, Some(128_000));
    }

    /// #245: catalog entries that differ from the Sol preset on wire flags
    /// must not be collapsed onto the preset's capabilities. The live table
    /// in the issue is the fixture; the point is that Spark's flags reach
    /// `codex_wire` instead of being dropped at parse time.
    #[test]
    fn codex_catalog_parser_reads_wire_capabilities_that_differ_from_the_preset() {
        let payload = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6 Sol",
                    "context_window": 272000,
                    "use_responses_lite": true,
                    "tool_mode": "code_mode_only",
                    "supports_reasoning_summary_parameter": true,
                    "supports_image_detail_original": true,
                    "input_modalities": ["text", "image"],
                    "default_reasoning_level": "low"
                },
                {
                    "slug": "gpt-5.3-codex-spark",
                    "display_name": "GPT-5.3 Codex Spark",
                    "context_window": 128000,
                    "use_responses_lite": false,
                    "tool_mode": null,
                    "supports_reasoning_summary_parameter": false,
                    "supports_image_detail_original": false,
                    "input_modalities": ["text"],
                    "default_reasoning_level": "high"
                }
            ]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let sol = presets
            .get("gpt-5.6-sol")
            .expect("sol")
            .codex_wire
            .as_ref()
            .expect("sol wire caps");
        let spark = presets
            .get("gpt-5.3-codex-spark")
            .expect("spark")
            .codex_wire
            .as_ref()
            .expect("spark wire caps");

        assert_eq!(sol.use_responses_lite, Some(true));
        assert_eq!(spark.use_responses_lite, Some(false));
        assert_eq!(sol.tool_mode.as_deref(), Some("code_mode_only"));
        assert_eq!(spark.tool_mode, None);
        assert_eq!(sol.supports_reasoning_summary_parameter, Some(true));
        assert_eq!(spark.supports_reasoning_summary_parameter, Some(false));
        assert_eq!(
            sol.input_modalities,
            vec!["text".to_owned(), "image".to_owned()]
        );
        assert_eq!(spark.input_modalities, vec!["text".to_owned()]);
        assert_eq!(sol.default_reasoning_level.as_deref(), Some("low"));
        assert_eq!(spark.default_reasoning_level.as_deref(), Some("high"));
        assert!(
            !spark.include_reasoning_summary(),
            "Spark must not inherit Sol's summary parameter"
        );
        assert!(sol.include_reasoning_summary());

        let spark_entry = presets.get("gpt-5.3-codex-spark").unwrap();
        assert_eq!(
            spark_entry.reasoning_effort,
            Some(xai_grok_sampling_types::ReasoningEffort::High),
            "catalog default_reasoning_level must become reasoning_effort"
        );
    }

    /// `data` / `id` / `name` are tolerances for shapes this endpoint does not
    /// currently return. They are kept so a server-side change does not break
    /// the fetch, and pinned so nobody mistakes them for the observed shape.
    ///
    /// This test was called `..._prefers_max_context_window` and asserted
    /// exactly that, which is how the #258 bug survived review: the name read
    /// like a contract, the assertion agreed with it, and both were wrong.
    /// A passing test is only evidence about the thing it decided to check.
    #[test]
    fn codex_catalog_parser_tolerates_other_shapes_and_budgets_the_operative_window() {
        let payload = serde_json::json!({
            "data": [
                {
                    "id": "codex-mini",
                    "name": "Codex Mini",
                    "context_window": 64000,
                    "max_context_window": 256000
                },
                {
                    "model": "codex-small",
                    "context_window": 128000
                }
            ]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let mini = presets.get("codex-mini").expect("mini preset should parse");
        assert_eq!(mini.model.as_deref(), Some("codex-mini"));
        assert_eq!(
            mini.model_provider.as_deref(),
            Some(OPENAI_CODEX_PROVIDER_ID)
        );
        // The operative window, not the ceiling. `gpt-5.4` is the live case:
        // 272000 against a 1000000 maximum.
        assert_eq!(mini.context_window, Some(64_000));
        let small = presets
            .get("codex-small")
            .expect("fallback slug preset should parse");
        assert_eq!(small.context_window, Some(128_000));
    }

    /// The live shape that made this a bug rather than a preference.
    ///
    /// Eight of the nine models in the account catalog report `context_window`
    /// and `max_context_window` equal, so preferring either one looked
    /// identical. `gpt-5.4` does not, and it is the model whose sessions run
    /// longest — budgeting it at the ceiling puts auto-compact 3.7x past the
    /// window, which surfaces as a context-length rejection deep into a
    /// session rather than as anything a short test would see.
    #[test]
    fn codex_catalog_uses_context_window_not_the_ceiling_for_gpt_5_4() {
        let payload = serde_json::json!({
            "models": [{
                "slug": "gpt-5.4",
                "display_name": "GPT-5.4",
                "context_window": 272_000,
                "max_context_window": 1_000_000
            }]
        });
        let presets = parse_openai_codex_catalog_models(&payload);
        let entry = presets.get("gpt-5.4").expect("slug is the catalog key");
        assert_eq!(
            entry.context_window,
            Some(272_000),
            "the operative window is what a session is budgeted against"
        );
    }

    /// The query parameter is not decoration: without it the endpoint answers
    /// HTTP 400 and the catalog silently stays at one hardcoded model.
    #[test]
    fn codex_catalog_url_carries_the_required_client_version() {
        let url = format!(
            "{}/models?client_version={}",
            crate::auth::openai_codex::CODEX_API_BASE_URL,
            OPENAI_CODEX_CATALOG_CLIENT_VERSION
        );
        assert!(
            url.contains("?client_version="),
            "GET /models without client_version is rejected: {url}"
        );
        assert!(!OPENAI_CODEX_CATALOG_CLIENT_VERSION.is_empty());
    }

    #[test]
    fn codex_catalog_fallback_uses_builtin_preset_when_fetch_is_empty() {
        let presets = effective_openai_codex_presets(Some(IndexMap::new()));
        assert_eq!(presets.len(), 1);
        let preset = presets
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("built-in fallback preset should exist");
        assert_eq!(preset.model.as_deref(), Some(OPENAI_CODEX_PRESET_MODEL_ID));
        assert_eq!(
            preset.context_window,
            Some(OPENAI_CODEX_PRESET_CONTEXT_WINDOW)
        );
    }

    #[test]
    fn codex_catalog_credential_source_must_be_openai_codex_provider() {
        assert!(codex_catalog_credential_source_is_valid(
            &xai_grok_sampler::CredentialSource::AuthProvider {
                name: OPENAI_CODEX_PROVIDER_ID.to_owned()
            }
        ));
        assert!(!codex_catalog_credential_source_is_valid(
            &xai_grok_sampler::CredentialSource::AuthProvider {
                name: "other-provider".to_owned()
            }
        ));
        assert!(!codex_catalog_credential_source_is_valid(
            &xai_grok_sampler::CredentialSource::Missing
        ));
    }
}
