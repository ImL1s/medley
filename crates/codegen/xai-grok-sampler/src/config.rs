//! Sampler configuration types.
//!
//! [`SamplerConfig`] is the per-request configuration handed to the
//! sampler. It deliberately does **not** alias
//! `xai_grok_sampling_types::SamplingConfig` so that the sampler crate
//! avoids transitive dependencies on shell-specific types
//! (`xai-grok-tools`, etc.).

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use xai_grok_auth::CredentialComparison;
use xai_grok_sampling_types::{
    ApiBackend, CompactionAtTokens, CompactionsRemaining, DoomLoopRecoveryPolicy, ReasoningEffort,
};

use crate::attribution::SharedAttributionCallback;
use crate::retry::{DEFAULT_MAX_RETRIES, RATE_LIMIT_RETRY_THRESHOLD};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    #[default]
    Bearer,
    XApiKey,
    None,
}

/// All knobs that control a single sampling request.
///
/// The session typically owns one `SamplerConfig` per active model
/// and passes it (or a per-request override) to the actor on every
/// submit.
///
/// # Construction in `xai-grok-shell`
///
/// `SamplerConfig` is the single source of truth for sampler
/// configuration. The shell builds it directly (see
/// `agent::config::resolve_model_to_sampling_config` and
/// `session::acp_session::SessionActor::reconstruct_full_config`) by
/// composing chat-state's `xai_grok_sampling_types::SamplingConfig`
/// with `Credentials` (api key, client version).
///
/// URL-derived request headers (e.g. `X-XAI-Token-Auth` for the
/// cli-chat-proxy) are
/// folded into [`Self::extra_headers`] by
/// `agent::config::inject_url_derived_headers` before the
/// `SamplerConfig` is handed to the actor. Auth is selected separately
/// via `auth_scheme`, while `api_backend` controls only the request/response
/// protocol shape.
#[derive(Clone, Serialize, Deserialize)]
pub struct SamplerConfig {
    pub api_key: Option<String>,
    pub base_url: String,
    pub model: String,
    pub max_completion_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub api_backend: ApiBackend,
    #[serde(default)]
    pub auth_scheme: AuthScheme,
    /// Extra request headers applied verbatim. The sampler never inspects
    /// the URL to derive headers; callers (the session) inject proxy auth
    /// and other access headers here before constructing the config.
    pub extra_headers: IndexMap<String, String>,
    /// Query parameters folded into every request URL (percent-encoded).
    #[serde(default)]
    pub query_params: IndexMap<String, String>,
    /// Header name to environment variable, resolved into request headers at
    /// client build and never persisted.
    #[serde(default)]
    pub env_http_headers: IndexMap<String, String>,
    /// Total context window size in tokens. The sampler does not enforce
    /// it; it is informational metadata used by the session for compaction
    /// decisions.
    pub context_window: u64,
    pub force_http1: bool,
    pub max_retries: Option<u32>,
    pub stream_tool_calls: bool,
    pub idle_timeout_secs: Option<u64>,

    // Reasoning effort
    pub reasoning_effort: Option<ReasoningEffort>,

    // Client identity
    pub origin_client: Option<OriginClientInfo>,
    pub client_identifier: Option<String>,
    pub deployment_id: Option<String>,
    pub user_id: Option<String>,
    pub client_version: Option<String>,

    /// Optional hook invoked at every UNAUTHORIZED (401) response
    /// site. The sampler passes only the secret-free relationship between
    /// the credential actually sent on the final attempt and the current
    /// credential. `None`
    /// (default) is a no-op -- the 401 arm returns the same
    /// `SamplingError::Auth` it always did.
    ///
    /// `Arc<dyn Trait>` is not serializable, so the field is skipped
    /// in (de)serialization. Round-tripping a config through serde
    /// drops the callback; callers that deserialize a `SamplerConfig`
    /// from disk must re-attach the callback before passing it to
    /// [`crate::SamplingClient::new`] or 401 attribution will be
    /// silently disabled for the rebuilt client.
    #[serde(skip)]
    pub attribution_callback: Option<SharedAttributionCallback>,

    /// Live bearer resolve per request. `None` uses construction-time `api_key`.
    #[serde(skip)]
    pub bearer_resolver: Option<SharedBearerResolver>,

    #[serde(default)]
    pub supports_backend_search: bool,

    /// Per-model config for the `x-compactions-remaining` header; `None` disables it.
    #[serde(default)]
    pub compactions_remaining: Option<CompactionsRemaining>,

    /// Per-model config for the `x-compaction-at` header; `None` disables it.
    #[serde(default)]
    pub compaction_at_tokens: Option<CompactionAtTokens>,

    /// Server-side doom-loop check policy; `None` disables it. When set, the
    /// client itself sends the opt-in `x-grok-doom-loop-check` header on
    /// streaming Responses API requests and absorbs the reported trigger
    /// events (unlike the environment headers in [`Self::extra_headers`],
    /// this header gates the client's own decode behavior, so it lives with
    /// the decoder).
    #[serde(default)]
    pub doom_loop_recovery: Option<DoomLoopRecoveryPolicy>,

    /// Per-request header injector (e.g. OTel traceparent). Called in `post()`.
    #[serde(skip)]
    pub header_injector: Option<SharedHeaderInjector>,
}

impl std::fmt::Debug for SamplerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplerConfig")
            .field("api_key_configured", &self.api_key.is_some())
            .field("base_url_configured", &!self.base_url.is_empty())
            .field("model", &self.model)
            .field("auth_scheme", &self.auth_scheme)
            .field("extra_header_count", &self.extra_headers.len())
            .field("query_param_count", &self.query_params.len())
            .field("env_http_header_count", &self.env_http_headers.len())
            .field("context_window", &self.context_window)
            .field("force_http1", &self.force_http1)
            .field("max_retries", &self.max_retries)
            .field("stream_tool_calls", &self.stream_tool_calls)
            .field("idle_timeout_secs", &self.idle_timeout_secs)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("origin_client", &self.origin_client)
            .field("client_identifier", &self.client_identifier)
            .field("deployment_id", &self.deployment_id)
            .field("user_id", &self.user_id)
            .field("client_version", &self.client_version)
            .field(
                "attribution_callback_configured",
                &self.attribution_callback.is_some(),
            )
            .field(
                "bearer_resolver_configured",
                &self.bearer_resolver.is_some(),
            )
            .field(
                "header_injector_configured",
                &self.header_injector.is_some(),
            )
            .field("supports_backend_search", &self.supports_backend_search)
            .finish_non_exhaustive()
    }
}

impl Default for SamplerConfig {
    /// Empty defaults so callers can use `..Default::default()` and
    /// new fields don't ripple through every literal site.
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: String::new(),
            model: String::new(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: ApiBackend::default(),
            auth_scheme: AuthScheme::default(),
            extra_headers: IndexMap::new(),
            query_params: IndexMap::new(),
            env_http_headers: IndexMap::new(),
            context_window: 0,
            force_http1: false,
            max_retries: None,
            stream_tool_calls: false,
            idle_timeout_secs: None,
            reasoning_effort: None,
            origin_client: None,
            client_identifier: None,
            deployment_id: None,
            user_id: None,
            client_version: None,
            attribution_callback: None,
            bearer_resolver: None,
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            doom_loop_recovery: None,
            header_injector: None,
        }
    }
}

/// Cheap sync read of the current bearer for [`SamplerConfig::bearer_resolver`].
pub trait BearerResolver: Send + Sync + std::fmt::Debug {
    fn current_bearer(&self) -> Option<String>;

    /// Capture all provider-scoped request credentials in one read. The
    /// default preserves compatibility with existing bearer-only resolvers.
    fn current_credential(&self) -> Option<ProviderCredentialSnapshot> {
        self.current_bearer()
            .map(ProviderCredentialSnapshot::bearer_only)
    }

    /// Resolve the credential immediately before an async request. Providers
    /// with refresh capability can override this while cache-only resolvers
    /// retain the existing synchronous behavior.
    fn current_credential_async(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Option<ProviderCredentialSnapshot>> + Send + '_>,
    > {
        Box::pin(std::future::ready(self.current_credential()))
    }

    fn compare_sent_credential(&self, sent: Option<&str>) -> CredentialComparison {
        let current = self.current_bearer();
        CredentialComparison::compare(sent, current.as_deref())
    }
}

/// One request's provider credential. Secret bytes intentionally have a
/// redacted `Debug` representation and are never serialized.
#[derive(Clone)]
pub struct ProviderCredentialSnapshot {
    pub access_token: String,
    pub account_id: Option<String>,
}

impl ProviderCredentialSnapshot {
    pub fn bearer_only(access_token: String) -> Self {
        Self {
            access_token,
            account_id: None,
        }
    }
}

impl std::fmt::Debug for ProviderCredentialSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCredentialSnapshot")
            .field("access_token_configured", &!self.access_token.is_empty())
            .field("account_id_configured", &self.account_id.is_some())
            .finish()
    }
}

pub type SharedBearerResolver = std::sync::Arc<dyn BearerResolver>;

/// Per-request header injection (e.g. OTel `traceparent`).
pub trait HeaderInjector: Send + Sync + std::fmt::Debug {
    fn inject(&self, headers: &mut reqwest::header::HeaderMap);
}

pub type SharedHeaderInjector = std::sync::Arc<dyn HeaderInjector>;

/// Retry knobs for the sampler's internal transport-error retry loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of retries before giving up.
    pub max_retries: u32,
    /// After this many rate-limit (429) retries, escalate to the caller.
    /// Lower than `max_retries` because rate-limit waits can be long.
    pub rate_limit_retry_threshold: u32,
    #[serde(default)]
    pub retry_only_before_output: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            rate_limit_retry_threshold: RATE_LIMIT_RETRY_THRESHOLD,
            retry_only_before_output: false,
        }
    }
}

/// Identity of the client that originated the request, used for
/// User-Agent rendering. The shell layer composes this with platform
/// info into a final UA string.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OriginClientInfo {
    pub product: String,
    pub version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_defaults() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(
            policy.rate_limit_retry_threshold,
            RATE_LIMIT_RETRY_THRESHOLD
        );
    }

    #[test]
    fn sampler_config_debug_redacts_all_credential_carriers() {
        let sentinel = "cred_SENTINEL_0123456789abcdef";
        let mut config = SamplerConfig {
            api_key: Some(sentinel.to_owned()),
            base_url: format!("https://example.test/?access_token={sentinel}"),
            ..SamplerConfig::default()
        };
        config
            .extra_headers
            .insert("authorization".to_owned(), format!("Bearer {sentinel}"));
        config
            .query_params
            .insert("api_key".to_owned(), sentinel.to_owned());

        let rendered = format!("{config:?}");
        assert!(rendered.contains("api_key_configured: true"));
        assert!(rendered.contains("extra_header_count: 1"));
        assert!(!rendered.contains(sentinel));
        for window in sentinel.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).unwrap();
            assert!(
                !rendered.contains(window),
                "leaked sentinel window: {window}"
            );
        }
    }

    #[test]
    fn provider_credential_snapshot_debug_redacts_values() {
        let snapshot = ProviderCredentialSnapshot {
            access_token: "access-secret-sentinel".to_owned(),
            account_id: Some("account-secret-sentinel".to_owned()),
        };

        let rendered = format!("{snapshot:?}");
        assert!(rendered.contains("access_token_configured: true"));
        assert!(rendered.contains("account_id_configured: true"));
        assert!(!rendered.contains("access-secret-sentinel"));
        assert!(!rendered.contains("account-secret-sentinel"));
    }

    /// Configs serialized before the field existed must keep deserializing.
    #[test]
    fn config_without_doom_loop_recovery_deserializes_to_none() {
        let mut stripped = serde_json::to_value(SamplerConfig::default()).unwrap();
        stripped
            .as_object_mut()
            .unwrap()
            .remove("doom_loop_recovery");
        let config: SamplerConfig = serde_json::from_value(stripped).unwrap();
        assert!(config.doom_loop_recovery.is_none());

        let with_policy = SamplerConfig {
            doom_loop_recovery: Some(DoomLoopRecoveryPolicy {
                max_threshold: 8,
                max_retries: 2,
            }),
            ..Default::default()
        };
        let round_tripped: SamplerConfig =
            serde_json::from_value(serde_json::to_value(&with_policy).unwrap()).unwrap();
        assert_eq!(
            round_tripped.doom_loop_recovery,
            with_policy.doom_loop_recovery
        );
    }
}
