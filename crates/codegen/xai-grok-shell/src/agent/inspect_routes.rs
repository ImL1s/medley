//! Secret-free inspect routes for standalone `grok doctor` (#15).
//!
//! Thin public wrapper around crate-private `inspect_model_routes` /
//! `EffectiveModelRoute`. Loads config the same way `grok inspect` does
//! (offline, no network). A load or parse failure yields an empty list.

use crate::agent::config::{
    Config, effective_model_route, format_credential_source_label, resolve_credentials,
    resolve_model_list,
};

/// Wire auth header a model route will send. Display/JSON labels only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectedAuthScheme {
    None,
    Bearer,
    #[serde(rename = "x-api-key")]
    XApiKey,
}

impl InspectedAuthScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer => "bearer",
            Self::XApiKey => "x-api-key",
        }
    }
}

/// Sampler-enforced endpoint trust. Exhaustive: never invent `External`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectedEndpointTrust {
    FirstPartyXai,
    External,
    Local,
    UserDeclared,
}

impl InspectedEndpointTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstPartyXai => "first_party_xai",
            Self::External => "external",
            Self::Local => "local",
            Self::UserDeclared => "user_declared",
        }
    }
}

/// Secret-free inspect route the pager maps to `ProviderRouteFact`.
///
/// Fields are names, classes, and a sanitized origin only — never key bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectedModelRoute {
    pub catalog_id: String,
    pub wire_model: String,
    pub sanitized_origin: String,
    pub auth_scheme: InspectedAuthScheme,
    pub credential_source: String,
    pub endpoint_trust: InspectedEndpointTrust,
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unready_reason: Option<String>,
}

/// Offline inspect routes for standalone `grok doctor`.
///
/// Effective config, `compat` stripped so a malformed cell cannot block the
/// rest, then an **offline** `Config` load (no Codex `GET /models`).
/// Empty when the file cannot be loaded or parsed.
pub fn inspect_model_routes_offline() -> Vec<InspectedModelRoute> {
    let Ok(effective) = crate::config::load_effective_config() else {
        return Vec::new();
    };
    inspect_model_routes_from_toml(&strip_compat(effective))
}

/// Secret-free inspect routes from an already-loaded TOML table.
///
/// Used by doctor tests with fixtures. Empty when `Config` parse fails.
/// Never performs a live Codex catalog fetch.
pub fn inspect_model_routes_from_toml(root: &toml::Value) -> Vec<InspectedModelRoute> {
    let Ok(cfg) = Config::new_from_toml_cfg_offline(root) else {
        return Vec::new();
    };
    inspected_routes_from_config(&cfg)
}

fn strip_compat(mut root: toml::Value) -> toml::Value {
    if let Some(table) = root.as_table_mut() {
        table.remove("compat");
    }
    root
}

fn inspected_routes_from_config(cfg: &Config) -> Vec<InspectedModelRoute> {
    resolve_model_list(cfg, None)
        .iter()
        .map(|(id, entry)| {
            let creds = resolve_credentials(entry, None);
            let route = effective_model_route(id, entry, &creds);
            InspectedModelRoute {
                catalog_id: route.catalog_id,
                wire_model: route.wire_model,
                sanitized_origin: route.sanitized_origin,
                auth_scheme: inspect_auth_scheme(&route.credential_source, entry.info.auth_scheme),
                credential_source: format_credential_source_label(&route.credential_source),
                endpoint_trust: inspect_endpoint_trust(route.endpoint_trust),
                ready: route.ready,
                unready_reason: route.unready_reason,
            }
        })
        .collect()
}

/// Display scheme without reading credential bytes.
///
/// `none` when the resolved source is deliberately keyless; `x-api-key` only
/// from a secret-free label (configured `AuthScheme` or an `x-api-key`
/// header name); otherwise `bearer`.
fn inspect_auth_scheme(
    source: &xai_grok_sampler::CredentialSource,
    configured: xai_grok_sampler::AuthScheme,
) -> InspectedAuthScheme {
    use xai_grok_sampler::{AuthScheme, CredentialSource};
    match source {
        CredentialSource::None => InspectedAuthScheme::None,
        CredentialSource::ExplicitHeader { header, .. }
            if header.eq_ignore_ascii_case("x-api-key") =>
        {
            InspectedAuthScheme::XApiKey
        }
        _ => match configured {
            AuthScheme::None => InspectedAuthScheme::None,
            AuthScheme::XApiKey => InspectedAuthScheme::XApiKey,
            AuthScheme::Bearer => InspectedAuthScheme::Bearer,
        },
    }
}

fn inspect_endpoint_trust(trust: xai_grok_sampler::EndpointTrustClass) -> InspectedEndpointTrust {
    use xai_grok_sampler::EndpointTrustClass::*;
    match trust {
        FirstPartyXai => InspectedEndpointTrust::FirstPartyXai,
        External => InspectedEndpointTrust::External,
        Local => InspectedEndpointTrust::Local,
        UserDeclared => InspectedEndpointTrust::UserDeclared,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use xai_grok_test_support::EnvGuard;

    const SECRET: &str = "sk-test-issue15-secret-0123456789";

    fn issue15_fixture_toml() -> String {
        format!(
            r#"
[model.ollama-codellama]
model = "codellama"
base_url = "http://localhost:11434/v1"
name = "CodeLlama (Ollama)"
auth_scheme = "none"
context_window = 16384

[model.gpt-4o]
model = "gpt-4o"
base_url = "https://api.openai.com/v1"
name = "GPT-4o"
env_key = "OPENAI_API_KEY"

[model.claude-opus]
model = "claude-opus-4-6"
base_url = "https://api.anthropic.com/v1"
name = "Claude Opus 4.6"
api_backend = "messages"
auth_scheme = "x_api_key"
env_key = "ANTHROPIC_API_KEY"
extra_headers = {{ "anthropic-version" = "2023-06-01" }}
context_window = 200000

[model.hostile-gateway]
model = "wire-model"
base_url = "https://user:{SECRET}@api.example.com:8443/v1/x?api_key={SECRET}#frag"
api_key = "{SECRET}"
"#
        )
    }

    fn assert_secret_free(rendered: &str) {
        for window in SECRET.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).expect("ascii secret");
            assert!(
                !rendered.contains(window),
                "inspect route leaked secret fragment {window}: {rendered}"
            );
        }
    }

    fn by_id<'a>(routes: &'a [InspectedModelRoute], id: &str) -> &'a InspectedModelRoute {
        routes
            .iter()
            .find(|route| route.catalog_id == id)
            .unwrap_or_else(|| panic!("missing inspect route {id}"))
    }

    #[test]
    #[serial]
    fn issue15_inspect_model_routes_from_toml_covers_keyless_bearer_and_x_api_key() {
        let _openai = EnvGuard::set("OPENAI_API_KEY", SECRET);
        let _anthropic = EnvGuard::set("ANTHROPIC_API_KEY", SECRET);
        let root: toml::Value =
            toml::from_str(&issue15_fixture_toml()).expect("issue15 fixture TOML");
        let routes = inspect_model_routes_from_toml(&root);

        let local = by_id(&routes, "ollama-codellama");
        assert_eq!(local.auth_scheme, InspectedAuthScheme::None);
        assert_eq!(local.credential_source, "none");
        assert_eq!(local.endpoint_trust, InspectedEndpointTrust::Local);
        assert_eq!(local.sanitized_origin, "http://localhost:11434/v1");
        assert!(local.ready, "auth_scheme=none is ready");

        let openai = by_id(&routes, "gpt-4o");
        assert_eq!(openai.auth_scheme, InspectedAuthScheme::Bearer);
        assert_eq!(openai.credential_source, "env:OPENAI_API_KEY");
        assert_eq!(openai.endpoint_trust, InspectedEndpointTrust::External);
        assert!(openai.ready, "OPENAI_API_KEY is set");

        let anthropic = by_id(&routes, "claude-opus");
        assert_eq!(anthropic.auth_scheme, InspectedAuthScheme::XApiKey);
        assert_eq!(anthropic.credential_source, "env:ANTHROPIC_API_KEY");
        assert_eq!(anthropic.endpoint_trust, InspectedEndpointTrust::External);
        assert!(anthropic.ready, "ANTHROPIC_API_KEY is set");

        let hostile = by_id(&routes, "hostile-gateway");
        assert_eq!(hostile.auth_scheme, InspectedAuthScheme::Bearer);
        assert_eq!(hostile.credential_source, "model_api_key");
        assert_eq!(
            hostile.sanitized_origin,
            "https://api.example.com:8443/v1/x"
        );
        assert_eq!(hostile.endpoint_trust, InspectedEndpointTrust::External);

        let json = serde_json::to_string(&routes).expect("routes serialize");
        let debug = format!("{routes:?}");
        for rendered in [json.as_str(), debug.as_str()] {
            assert_secret_free(rendered);
        }
    }

    #[test]
    fn issue15_inspect_model_routes_from_toml_does_not_attempt_live_codex_catalog_fetch() {
        crate::agent::model_providers::reset_live_codex_catalog_fetch_attempts();
        let root: toml::Value =
            toml::from_str(&issue15_fixture_toml()).expect("issue15 fixture TOML");
        let routes = inspect_model_routes_from_toml(&root);
        assert!(
            !routes.is_empty(),
            "offline inspect must still resolve fixture routes"
        );
        assert_eq!(
            crate::agent::model_providers::live_codex_catalog_fetch_attempts(),
            0,
            "standalone doctor inspect must not call GET /models"
        );
    }

    #[test]
    fn issue15_inspect_model_routes_from_toml_is_empty_when_config_parse_fails() {
        let root: toml::Value = toml::from_str(
            r#"
            [models]
            catalog_auth_scheme = "not-a-scheme"
            "#,
        )
        .expect("valid TOML, invalid catalog auth");
        assert!(inspect_model_routes_from_toml(&root).is_empty());
    }
}
