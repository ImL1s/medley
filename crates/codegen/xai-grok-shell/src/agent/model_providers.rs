use indexmap::IndexMap;

use super::config::{ConfigModelOverride, EnvKeys};
use super::config_model_override_parse::{ConfigWarning, ConfigWarningKind};
use crate::sampling::ApiBackend;

/// Reserved first-party provider profile for ChatGPT Codex OAuth traffic.
pub const OPENAI_CODEX_PROVIDER_ID: &str = "openai-codex";

/// Catalog key and routing slug of the built-in Codex preset. The two are the
/// same string so a user's `[model."gpt-5.6-sol"]` in the global config
/// replaces the preset in place instead of adding a second entry.
pub const OPENAI_CODEX_PRESET_MODEL_ID: &str = "gpt-5.6-sol";

/// Conservative context window for the preset. Codex-side metadata is not
/// discoverable from the CLI, and under-reporting only makes auto-compact fire
/// earlier, so this matches the value the custom-models guide has always used
/// in its Codex example.
const OPENAI_CODEX_PRESET_CONTEXT_WINDOW: u64 = 200_000;

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
    for (key, preset) in openai_codex_preset_models() {
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

    #[test]
    fn provider_debug_is_presence_only() {
        let secret = "GB002-provider-config-secret-0123456789abcdef";
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
        assert_eq!(
            resolve_credentials(model, Some("session-jwt")).api_key,
            None,
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
        assert_eq!(
            resolve_credentials(model, Some("session-jwt")).api_key,
            None,
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
        assert_eq!(
            resolve_credentials(model, Some("session-jwt")).api_key,
            None,
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
        assert_eq!(
            resolve_credentials(model, Some("session-jwt")).api_key,
            None,
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
        assert_eq!(
            resolve_credentials(isolated, None).api_key,
            None,
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
        let resolved = resolve_model_list(&cfg, None);
        let model = resolved
            .get(OPENAI_CODEX_PRESET_MODEL_ID)
            .expect("the preset key should resolve");

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
        assert_eq!(
            resolve_credentials(model, Some("xai-session-token")).api_key,
            None,
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
}
