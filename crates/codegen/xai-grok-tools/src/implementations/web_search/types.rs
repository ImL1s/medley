use crate::types::SharedApiKeyProvider;
use indexmap::IndexMap;

/// Configuration for the web search tool.
///
/// Use `Disabled` when no API key is available or web search should be turned off.
/// Use `Enabled { … }` to provide credentials and endpoint configuration.
#[derive(Clone, Default, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WebSearchConfig {
    #[default]
    Disabled,
    Enabled {
        api_key: String,
        base_url: String,
        model: String,
        #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
        extra_headers: IndexMap<String, String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        alpha_test_key: Option<String>,
        /// Optional provider scoped to this web-search model. Runtime-only:
        /// deserialized configs continue to use the caller's default provider.
        #[serde(skip)]
        api_key_provider: Option<SharedApiKeyProvider>,
    },
}

impl std::fmt::Debug for WebSearchConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("WebSearchConfig::Disabled"),
            Self::Enabled {
                api_key,
                base_url,
                model,
                extra_headers,
                alpha_test_key,
                api_key_provider,
            } => f
                .debug_struct("WebSearchConfig::Enabled")
                .field("api_key_present", &!api_key.is_empty())
                .field("base_url_present", &!base_url.is_empty())
                .field("model_present", &!model.is_empty())
                .field("extra_headers_present", &!extra_headers.is_empty())
                .field("alpha_test_key_present", &alpha_test_key.is_some())
                .field("provider_scoped", &api_key_provider.is_some())
                .finish(),
        }
    }
}

impl WebSearchConfig {
    /// Returns `true` when the config is the `Enabled` variant.
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }

    /// Return a copy safe for returning to clients.
    ///
    /// The `api_key` is replaced with `"***REDACTED***"` and the optional
    /// extra access key field is stripped.
    pub fn redacted(&self) -> Self {
        match self {
            Self::Disabled => Self::Disabled,
            Self::Enabled {
                base_url,
                model,
                extra_headers,
                ..
            } => Self::Enabled {
                api_key: "***REDACTED***".to_string(),
                base_url: base_url.clone(),
                model: model.clone(),
                extra_headers: extra_headers.clone(),
                alpha_test_key: None,
                api_key_provider: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default_is_disabled() {
        let config = WebSearchConfig::default();
        assert!(!config.is_enabled());
    }

    #[test]
    fn test_config_enabled() {
        let config = WebSearchConfig::Enabled {
            api_key: "test-key".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "test-web-search-model".to_string(),
            extra_headers: IndexMap::new(),
            alpha_test_key: None,
            api_key_provider: None,
        };
        assert!(config.is_enabled());
    }

    #[test]
    fn test_config_redacted() {
        let mut headers = IndexMap::new();
        headers.insert("X-Custom".to_string(), "value".to_string());
        let config = WebSearchConfig::Enabled {
            api_key: "secret-key-12345".to_string(),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "test-web-search-model".to_string(),
            extra_headers: headers,
            alpha_test_key: Some("alpha-secret".to_string()),
            api_key_provider: None,
        };
        let redacted = config.redacted();
        match redacted {
            WebSearchConfig::Enabled {
                api_key,
                base_url,
                model,
                extra_headers,
                alpha_test_key,
                api_key_provider,
            } => {
                assert_eq!(api_key, "***REDACTED***");
                assert_eq!(base_url, "https://api.x.ai/v1");
                assert_eq!(model, "test-web-search-model");
                assert_eq!(extra_headers.get("X-Custom").unwrap(), "value");
                assert!(alpha_test_key.is_none());
                assert!(api_key_provider.is_none());
            }
            _ => panic!("Expected Enabled variant"),
        }
    }

    #[test]
    fn debug_does_not_leak_secret_fields_or_windows() {
        let sentinel = "GB002-web-search-secret-0123456789abcdef";
        let config = WebSearchConfig::Enabled {
            api_key: sentinel.to_string(),
            base_url: format!("https://user:{sentinel}@example.test/?token={sentinel}"),
            model: "test-web-search-model".to_string(),
            extra_headers: IndexMap::from([("Authorization".to_string(), sentinel.to_string())]),
            alpha_test_key: Some(sentinel.to_string()),
            api_key_provider: None,
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains(sentinel));
        for window in sentinel.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !rendered.contains(window),
                "leaked sentinel window: {window}"
            );
        }
    }

    #[test]
    fn test_config_deserialize_from_set_options_payload() {
        let json = r#"{
            "status": "enabled",
            "api_key": "xai-abc123",
            "base_url": "https://api.x.ai/v1",
            "model": "test-web-search-model"
        }"#;
        let config: WebSearchConfig = serde_json::from_str(json).unwrap();
        assert!(config.is_enabled());
    }
}
