//! HTTP client for the xAI sampling APIs.
//!
//! Owns the `reqwest::Client`, default request headers, and per-method
//! defaults. Talks to three backend shapes:
//!
//! * Chat Completions (`/chat/completions`)
//! * Responses API (`/responses`)
//! * Anthropic Messages API (`/messages`)
//!
//! All trace-upload and URL-based header injection is intentionally
//! *not* here. The session is responsible for putting any per-request
//! headers (proxy auth, OTel context, etc.)
//! into [`SamplerConfig::extra_headers`] before constructing the client.

use eventsource_stream::{EventStreamError, Eventsource};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use indexmap::IndexMap;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde::Serialize;

use xai_grok_sampling_types::error::{try_parse_stream_error, user_facing_api_error_message};
use xai_grok_sampling_types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ConversationRequest,
    ConversationResponse, CreateResponseWrapper, DOOM_LOOP_CHECK_HEADER, MessagesRequestWrapper,
    ResponseModelMetadata, Result, SamplingError, SentCredential, build_messages_request,
    is_check_event, messages, rs,
};

use crate::config::{AuthScheme, EndpointTrustClass, OriginClientInfo, SamplerConfig};
use crate::events::SamplingErrorInfo;
use xai_grok_auth::{CredentialComparison, SentCredentialRelation};

/// Credential bytes from the final outbound header map. This value stays
/// request-local, never implements `Debug`/`Serialize`, and is projected to a
/// secret-free [`CredentialComparison`] only after a response is received.
struct FinalRequestCredential(Option<String>);

// Re-export ApiBackend from the shared types crate for downstream callers.
pub use xai_grok_sampling_types::ApiBackend;

/// Process-level fallback for the `x-grok-client-identifier` header.
const DEFAULT_CLIENT_IDENTIFIER: &str = "grok-shell";

/// Product identifier baked into User-Agent strings.
const AGENT_PRODUCT: &str = "grok-shell";
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 128_000;
const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const CHATGPT_ACCOUNT_ID: HeaderName = HeaderName::from_static("chatgpt-account-id");
const OPENAI_FEDRAMP: HeaderName = HeaderName::from_static("x-openai-fedramp");
const ORIGINATOR: HeaderName = HeaderName::from_static("originator");
const GROK_BUILD_ORIGINATOR: HeaderValue = HeaderValue::from_static("grok_build");

fn should_send_xai_identity_headers(auth_scheme: AuthScheme, base_url: &str) -> bool {
    !matches!(auth_scheme, AuthScheme::None) && crate::util::is_xai_api_url(base_url)
}

fn strip_xai_identity_headers(headers: &mut HeaderMap) {
    headers.remove(HeaderName::from_static("x-grok-deployment-id"));
    headers.remove(HeaderName::from_static("x-grok-user-id"));
}

fn strip_codex_routing_headers(headers: &mut HeaderMap) {
    headers.remove(CHATGPT_ACCOUNT_ID);
    headers.remove(OPENAI_FEDRAMP);
    headers.remove(ORIGINATOR);
}

/// Resolve the endpoint trust class: explicit config wins, then xAI-operated
/// hosts (including the cli-chat-proxy) with real auth, then loopback, then
/// external. Decided once at client construction and enforced at every
/// request boundary.
///
/// Public because it is the *only* trust classifier: anything that needs to
/// describe or display where a request will go must ask this rather than
/// re-derive it, or the two answers drift and the displayed one starts
/// lying about the enforced one (#110).
pub fn resolve_endpoint_trust(config: &SamplerConfig) -> EndpointTrustClass {
    if let Some(explicit) = config.endpoint_trust {
        return explicit;
    }
    // The production cli-chat-proxy is matched by exact URL, never by host
    // class, so a host that merely resembles it cannot claim first-party
    // trust. It is a public https host — see `PROD_CLI_CHAT_PROXY_BASE_URL` —
    // so it never reaches the loopback branch below, and the ordering here is
    // about exactness, not about rescuing it from that branch.
    if crate::util::is_prod_cli_chat_proxy_url(&config.base_url) {
        return EndpointTrustClass::FirstPartyXai;
    }
    // Any other loopback endpoint is Local regardless of auth scheme: an
    // authenticated Ollama/LM Studio server must not be classified
    // first-party just because `is_xai_api_url` accepts loopback mocks.
    if is_loopback_url(&config.base_url) {
        return EndpointTrustClass::Local;
    }
    if should_send_xai_identity_headers(config.auth_scheme, &config.base_url) {
        return EndpointTrustClass::FirstPartyXai;
    }
    EndpointTrustClass::External
}

fn is_loopback_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| match u.host_str() {
        Some("localhost") => true,
        // `host_str` wraps IPv6 literals in brackets; strip them for parsing.
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    })
}

/// First-party metadata namespaces that must never reach a non-xAI endpoint,
/// regardless of how they were injected (defaults, extra_headers, env
/// headers, or a per-request header injector).
fn is_internal_metadata_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    name.starts_with("x-grok-")
        || name.starts_with("x-xai-")
        || name == "x-compactions-remaining"
        || name == "x-compaction-at"
        || name == "x-authenticateresponse"
        || name == "traceparent"
        || name == "tracestate"
        || name == "baggage"
}

/// Replace a stable first-party session identifier in `prompt_cache_key`
/// with an irreversible digest: non-xAI endpoints keep a stable
/// cache-affinity key without learning the raw session ID.
fn anonymize_prompt_cache_key(body: &mut serde_json::Value) {
    if let Some(key) = body
        .get("prompt_cache_key")
        .and_then(serde_json::Value::as_str)
        .filter(|key| !key.is_empty())
    {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(key.as_bytes());
        body["prompt_cache_key"] = serde_json::Value::String(format!("{digest:x}"));
    }
}

/// Allowlist boundary for `External` / `Local` endpoints: keep protocol
/// headers, the selected credential, the User-Agent, and explicitly
/// configured provider headers; drop everything else. Internal metadata
/// namespaces are denied even when explicitly configured, so a shell-side
/// injection into `extra_headers` cannot re-open the boundary.
fn enforce_external_metadata_boundary(headers: &mut HeaderMap, explicit: &[HeaderName]) {
    let retained: Vec<(HeaderName, HeaderValue)> = headers
        .iter()
        .filter(|(name, _)| {
            if is_internal_metadata_header(name) {
                return false;
            }
            **name == CONTENT_TYPE
                || **name == ACCEPT
                || **name == USER_AGENT
                || **name == AUTHORIZATION
                || name.as_str() == "x-api-key"
                || **name == CHATGPT_ACCOUNT_ID
                || explicit.contains(name)
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    headers.clear();
    for (name, value) in retained {
        headers.append(name, value);
    }
}

fn normalize_codex_base_url(base_url: &str) -> Result<String> {
    let url = reqwest::Url::parse(base_url)
        .map_err(|_| SamplingError::InvalidConfiguration("invalid OpenAI Codex base URL"))?;
    let is_production = url.scheme() == "https"
        && url.host_str() == Some("chatgpt.com")
        && url.port().is_none()
        && url.path().trim_end_matches('/') == "/backend-api/codex"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    if is_production {
        return Ok(CODEX_BASE_URL.to_owned());
    }

    // Unit tests may point the transport at an in-process mock. This branch is
    // compiled out of normal library builds, so production remains fail closed.
    #[cfg(test)]
    {
        let is_loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"))
            && matches!(url.scheme(), "http" | "https")
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none();
        if is_loopback {
            return Ok(base_url.trim_end_matches('/').to_owned());
        }
    }

    Err(SamplingError::InvalidConfiguration(
        "OpenAI Codex traffic must use https://chatgpt.com/backend-api/codex",
    ))
}

/// Origin the built-in Codex provider's credential is valid for (#135).
///
/// An *origin* check — scheme, host, port — because the credential is scoped
/// to the account's origin, not to a path on it. Derived from
/// [`CODEX_BASE_URL`] rather than a re-typed host so the two cannot drift
/// apart. Unlike [`normalize_codex_base_url`] there is deliberately no
/// test-only loopback arm: this predicate runs only when the config declares
/// a provider credential source, and that declaration means real account
/// material is attached in every build.
fn is_codex_credential_origin(base_url: &str) -> bool {
    let url = reqwest::Url::parse(base_url);
    let codex = reqwest::Url::parse(CODEX_BASE_URL);
    match (url, codex) {
        (Ok(url), Ok(codex)) => {
            url.scheme() == "https"
                && url.host_str() == codex.host_str()
                && url.port() == codex.port()
        }
        _ => false,
    }
}

fn retain_codex_headers(
    headers: &mut HeaderMap,
    authorization: Option<HeaderValue>,
    account_id: Option<&str>,
    chatgpt_account_is_fedramp: bool,
    trusted_user_agent: Option<HeaderValue>,
) {
    let content_type = headers.get(CONTENT_TYPE).cloned();
    headers.clear();
    if let Some(value) = content_type {
        headers.insert(CONTENT_TYPE, value);
    }
    if let Some(value) = trusted_user_agent {
        headers.insert(USER_AGENT, value);
    }
    if let Some(value) = authorization {
        headers.insert(AUTHORIZATION, value);
    }
    if let Some(account_id) = account_id
        && let Ok(value) = HeaderValue::from_str(account_id)
    {
        headers.insert(CHATGPT_ACCOUNT_ID, value);
    }
    if chatgpt_account_is_fedramp {
        headers.insert(OPENAI_FEDRAMP, HeaderValue::from_static("true"));
    }
    headers.insert(ORIGINATOR, GROK_BUILD_ORIGINATOR);
}

/// Per-request `x-grok-*` headers. Optional fields are skipped when empty/`None`.
struct GrokRequestHeaders<'a> {
    conv_id: &'a str,
    req_id: &'a str,
    model_id: &'a str,
    session_id: &'a str,
    turn_idx: Option<&'a str>,
    agent_id: &'a str,
    deployment_id: Option<&'a str>,
    user_id: Option<&'a str>,
}

impl GrokRequestHeaders<'_> {
    fn apply(
        &self,
        builder: reqwest::RequestBuilder,
        include_identity: bool,
    ) -> reqwest::RequestBuilder {
        // `include_identity` is the endpoint-trust gate: non-first-party
        // endpoints receive no request-correlation metadata at all. These
        // headers ride the builder (after `post()`), so this early return is
        // the enforcement point — the post-injector boundary cannot see them.
        if !include_identity {
            return builder;
        }
        let mut b = builder
            .header("x-grok-conv-id", self.conv_id)
            .header("x-grok-req-id", self.req_id)
            .header("x-grok-model-override", self.model_id)
            .header("x-grok-session-id", self.session_id)
            .header("x-grok-agent-id", self.agent_id);
        if let Some(idx) = self.turn_idx {
            b = b.header("x-grok-turn-idx", idx);
        }
        if include_identity {
            if let Some(id) = self.deployment_id.filter(|s| !s.is_empty()) {
                b = b.header("x-grok-deployment-id", id);
            }
            if let Some(id) = self.user_id.filter(|s| !s.is_empty()) {
                b = b.header("x-grok-user-id", id);
            }
        }
        b
    }
}

/// Parse the `Retry-After` response header as delta-seconds.
/// Our inference backends only emit integer seconds (never HTTP-date),
/// so we only handle that form. HTTP-dates silently return `None` and
/// the caller falls back to exponential backoff.
/// Capped at 120s to prevent absurdly long sleeps from a misbehaving upstream.
/// Deserialize a Responses API SSE event, with a fallback for xAI-specific
/// tool types (e.g., `x_search`) that `async_openai` can't parse.
///
/// The API echoes the request's `tools` array in `ResponseCompleted` and
/// `ResponseCreated` events. If we sent `{"type": "x_search"}`, the response
/// includes it, and `rs::Tool` deserialization fails. On failure, we strip
/// unrecognized tools from the raw JSON and retry.
///
/// On `response.completed` / `response.incomplete`, this also rewrites
/// `response.usage.total_tokens` in place to the live context length
/// (`context_details.input_tokens + context_details.output_tokens`)
/// when the API emits the xAI-specific `context_details` field.
/// Async-openai's typed `ResponseUsage` doesn't model `context_details`,
/// so we peek the raw JSON for it. The cumulative `input_tokens` /
/// `output_tokens` / `cached_tokens` continue to flow from the typed
/// `ResponseUsage` unchanged so billing telemetry stays correct. When
/// the API doesn't emit `context_details` (older deployments) `total_tokens`
/// passes through unchanged.
fn deserialize_response_event(data: &str) -> Result<rs::ResponseStreamEvent> {
    let mut event = match serde_json::from_str::<rs::ResponseStreamEvent>(data) {
        Ok(event) => event,
        Err(_) => {
            // Try sanitizing: parse as Value, strip unknown tools, retry.
            if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(data) {
                // Strip tools that async_openai's rs::Tool can't deserialize
                // (e.g., xAI-specific "x_search"). Instead of maintaining a
                // hardcoded allowlist, try deserializing each tool entry —
                // if it fails, drop it.
                if let Some(tools) = value
                    .pointer_mut("/response/tools")
                    .and_then(|v| v.as_array_mut())
                {
                    tools.retain(|t| serde_json::from_value::<rs::Tool>(t.clone()).is_ok());
                }
                if let Ok(mut event) = serde_json::from_value::<rs::ResponseStreamEvent>(value) {
                    apply_terminal_event_overrides(&mut event, data);
                    return Ok(event);
                }
            }
            tracing::error!("Failed to deserialize ResponseStreamEvent from stream");
            return Err(SamplingError::serialization_message(
                "invalid provider stream payload",
            ));
        }
    };
    apply_terminal_event_overrides(&mut event, data);
    Ok(event)
}

/// On terminal Responses API events (`response.completed` /
/// `response.incomplete`), rewrite `response.usage.total_tokens` to the
/// live context length when the wire includes
/// `response.usage.context_details.{input_tokens, output_tokens}`.
///
/// `total_tokens` drives the CLI's `/context` bar, the auto-compact
/// threshold, and `meta.totalTokens` on persisted sessions. Under
/// server-side multi-turn loops (e.g. `web_search`, `x_search`) the
/// wire's cumulative total inflates as the loop runs; `context_details`
/// reports the final turn's prompt + output tokens — the real live
/// context the model is sitting in. Billing fields
/// (`input_tokens`, `output_tokens`, `input_tokens_details.cached_tokens`,
/// `output_tokens_details.reasoning_tokens`) stay on the cumulative
/// wire values so telemetry is unaffected.
///
/// No-op when:
/// - the event is not terminal,
/// - `response.usage` is `None`,
/// - `context_details` is absent (older backends / non-loop responses),
/// - or either of `context_details.{input_tokens, output_tokens}` is
///   missing — we don't guess the missing half.
fn apply_terminal_event_overrides(event: &mut rs::ResponseStreamEvent, data: &str) {
    let response = match event {
        rs::ResponseStreamEvent::ResponseCompleted(e) => &mut e.response,
        rs::ResponseStreamEvent::ResponseIncomplete(e) => &mut e.response,
        _ => return,
    };
    // Re-parse for fields async_openai's types omit (context total, cost ticks).
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return;
    };
    // Stash cost ticks in metadata for stream_responses.
    if let Some(ticks) = xai_grok_sampling_types::reported_cost_ticks(
        value
            .pointer("/response/usage/cost_in_usd_ticks")
            .and_then(|v| v.as_i64()),
    ) {
        response
            .metadata
            .get_or_insert_with(Default::default)
            .insert(COST_USD_TICKS_METADATA_KEY.to_owned(), ticks.to_string());
    }
    let Some(usage) = response.usage.as_mut() else {
        return;
    };
    let Some(total) = extract_context_total(&value) else {
        return;
    };
    usage.total_tokens = total;
}

/// Metadata key for cost ticks past typed Response events.
pub(crate) const COST_USD_TICKS_METADATA_KEY: &str = "xai.cost_usd_ticks";

/// Read `response.usage.context_details.{input_tokens, output_tokens}`
/// from the parsed terminal-event JSON and return their sum. Returns `None`
/// if either field is missing or out of `u32` range.
fn extract_context_total(value: &serde_json::Value) -> Option<u32> {
    let cd = value.pointer("/response/usage/context_details")?;
    let i = u32::try_from(cd.get("input_tokens")?.as_u64()?).ok()?;
    let o = u32::try_from(cd.get("output_tokens")?.as_u64()?).ok()?;
    Some(i.saturating_add(o))
}

/// Record `success=false` + `error` on the active inference span when a stream
/// request fails before any response (transport/connect/TLS errors). Without
/// this the `#[instrument]` span closes with both fields Empty, so an outage
/// shows zero `success=false` and error-rate alerts never fire.
fn transport_error_class(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else if err.is_body() {
        "body"
    } else if err.is_decode() {
        "decode"
    } else if err.is_redirect() {
        "redirect"
    } else if err.is_request() {
        "request"
    } else {
        "transport"
    }
}

/// Strip the request URL before a reqwest error reaches logs, spans, or a
/// caller. Configured URLs may carry API keys or signed-query credentials.
fn sanitize_http_error(err: reqwest::Error) -> reqwest::Error {
    err.without_url()
}

fn request_transport_error(err: reqwest::Error) -> SamplingError {
    let err = sanitize_http_error(err);
    tracing::debug!(
        transport_error_class = transport_error_class(&err),
        "HTTP request failed"
    );
    SamplingError::http(err)
}

fn stream_transport_error(err: reqwest::Error) -> SamplingError {
    let err = sanitize_http_error(err);
    record_stream_request_failure(&err);
    tracing::debug!(
        transport_error_class = transport_error_class(&err),
        "HTTP stream request failed"
    );
    SamplingError::http(err)
}

fn request_build_error(err: reqwest::Error) -> SamplingError {
    let err = sanitize_http_error(err);
    tracing::error!(
        transport_error_class = transport_error_class(&err),
        "Failed to build HTTP request"
    );
    SamplingError::http(err)
}

fn safe_event_stream_error<E>(error: EventStreamError<E>) -> SamplingError {
    let class = match error {
        EventStreamError::Utf8(_) => "invalid_utf8",
        EventStreamError::Parser(_) => "invalid_sse",
        EventStreamError::Transport(_) => "transport",
    };
    tracing::debug!(event_stream_error_class = class, "Event stream failed");
    SamplingError::EventStreamError(class.to_string())
}

fn record_stream_request_failure(err: &reqwest::Error) {
    let span = tracing::Span::current();
    span.record("success", false);
    span.record("error", transport_error_class(err));
}

fn extract_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|s| s.min(120))
}

fn extract_should_retry(headers: &reqwest::header::HeaderMap) -> Option<bool> {
    headers
        .get("x-should-retry")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            if s.eq_ignore_ascii_case("true") {
                Some(true)
            } else if s.eq_ignore_ascii_case("false") {
                Some(false)
            } else {
                None // unknown value — treat as absent
            }
        })
}

fn extract_model_metadata(headers: &reqwest::header::HeaderMap) -> Option<ResponseModelMetadata> {
    let context_window = headers
        .get("x-grok-context-window")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let max_completion_tokens = headers
        .get("x-grok-max-completion-tokens")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok());

    let models_etag = headers
        .get("x-models-etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if context_window.is_some() || max_completion_tokens.is_some() || models_etag.is_some() {
        Some(ResponseModelMetadata {
            context_window,
            max_completion_tokens,
            models_etag,
        })
    } else {
        None
    }
}

/// Wrapper for streaming chat completion requests that adds `stream` and
/// `stream_options` fields without modifying the original `ChatCompletionRequest`.
///
/// Uses `#[serde(flatten)]` to inline all fields from the inner request,
/// allowing single-pass serialization instead of the previous two-pass
/// approach (serialize to `Value`, mutate, serialize to bytes).
#[derive(Serialize)]
struct StreamingChatRequest<'a> {
    #[serde(flatten)]
    inner: &'a ChatCompletionRequest,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Resolve `env_http_headers` (`header -> env var`) into `headers` via `getenv`, skipping unset/blank/invalid entries and trimming values.
fn apply_env_http_headers(
    env_http_headers: &IndexMap<String, String>,
    getenv: impl Fn(&str) -> Option<String>,
    headers: &mut HeaderMap,
) {
    for (key, env_var) in env_http_headers {
        let Some(value) = getenv(env_var) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let (Ok(name), Ok(header_value)) = (
            HeaderName::try_from(key.as_str()),
            HeaderValue::from_str(value),
        ) else {
            tracing::warn!(
                header = %key,
                env_var = %env_var,
                "skipping env_http_header with an invalid header name or value"
            );
            continue;
        };
        headers.insert(name, header_value);
    }
}

/// HTTP client for sampling. Cheap to clone; carries an `Arc`-backed
/// `reqwest::Client` and the default headers/request-defaults computed from a
/// [`SamplerConfig`] at construction time.
#[derive(Clone)]
pub struct SamplingClient {
    http: reqwest::Client,
    default_headers: HeaderMap,
    base_url: String,
    defaults: ClientDefaults,
    /// Optional 401-attribution hook. The shell wires this to emit a
    /// structured event at every UNAUTHORIZED arm so 401s can be
    /// bucketed by stale-snapshot vs. live-token-rejected. `None` for
    /// sampler-only callers and tests.
    attribution_callback: Option<crate::attribution::SharedAttributionCallback>,
    /// Per-request bearer override. See `SamplerConfig::bearer_resolver`.
    bearer_resolver: Option<crate::config::SharedBearerResolver>,
    /// Per-request header injection (OTel traceparent).
    header_injector: Option<crate::config::SharedHeaderInjector>,
    /// Endpoint URL builder, resolved once from `base_url` + `query_params`.
    endpoint: EndpointTemplate,
    /// Header names the caller explicitly configured (extra_headers +
    /// env_http_headers), retained across the external metadata boundary.
    explicit_header_names: Vec<HeaderName>,
}

impl std::fmt::Debug for SamplingClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplingClient")
            .field("base_url_present", &!self.base_url.is_empty())
            .field("defaults", &self.defaults)
            .field(
                "has_attribution_callback",
                &self.attribution_callback.is_some(),
            )
            .field("has_bearer_resolver", &self.bearer_resolver.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
struct ClientDefaults {
    model: String,
    max_completion_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    api_backend: ApiBackend,
    auth_scheme: AuthScheme,
    endpoint_trust: EndpointTrustClass,
    stream_tool_calls: bool,
    doom_loop_recovery: Option<xai_grok_sampling_types::DoomLoopRecoveryPolicy>,
}

/// Endpoint URL builder, resolved once at client construction so each request
/// only appends its path.
#[derive(Clone)]
enum EndpointTemplate {
    /// No query params and no query on the base URL (or an unparseable base):
    /// append the path to the base verbatim.
    Plain(String),
    /// Query params configured: `{prefix}/{path}{suffix}`. `suffix` starts with
    /// `?` and folds any base-URL params, with a configured key winning over the
    /// same key in `base_url` (percent-encoded, no duplicates).
    WithQuery { prefix: String, suffix: String },
}

impl std::fmt::Debug for EndpointTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointTemplate")
            .field("configured", &true)
            .finish_non_exhaustive()
    }
}

impl EndpointTemplate {
    fn new(base_url: &str, query_params: &IndexMap<String, String>) -> Self {
        let base = base_url.trim_end_matches('/').to_string();
        // The fast path is safe only when there is nothing to fold: no configured
        // params and no query already on the base (which would otherwise land
        // before the appended path).
        if query_params.is_empty() && !base.contains('?') {
            return Self::Plain(base);
        }
        let mut url = match reqwest::Url::parse(&base) {
            Ok(url) => url,
            Err(error) => {
                tracing::warn!(
                    %error,
                    base_url_present = !base.is_empty(),
                    "failed to parse base URL for endpoint; sending without folded query"
                );
                return Self::Plain(base);
            }
        };
        let overridden: std::collections::HashSet<&str> =
            query_params.keys().map(String::as_str).collect();
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| !overridden.contains(k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let prefix = {
            let mut prefix_url = url.clone();
            prefix_url.set_query(None);
            prefix_url.as_str().trim_end_matches('/').to_string()
        };
        {
            let mut pairs = url.query_pairs_mut();
            pairs.clear();
            for (key, value) in &kept {
                pairs.append_pair(key, value);
            }
            for (key, value) in query_params {
                pairs.append_pair(key, value);
            }
        }
        let suffix = url.query().map(|q| format!("?{q}")).unwrap_or_default();
        Self::WithQuery { prefix, suffix }
    }

    fn url_for_path(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        match self {
            Self::Plain(base) => format!("{base}/{path}"),
            Self::WithQuery { prefix, suffix } => format!("{prefix}/{path}{suffix}"),
        }
    }
}

// =============================================================================
// User-Agent helpers
// =============================================================================

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformInfo {
    os: String,
    arch: String,
}

impl PlatformInfo {
    fn current() -> Self {
        let os = match std::env::consts::OS {
            "macos" => "macos",
            "windows" => "windows",
            other => other,
        }
        .to_string();

        let arch = match std::env::consts::ARCH {
            "arm64" => "aarch64",
            "x86_64" => "x86_64",
            other => other,
        }
        .to_string();

        Self { os, arch }
    }
}

fn agent_version() -> String {
    xai_grok_version::VERSION.to_string()
}

/// Render a User-Agent string for the given origin client.
///
/// Mirrors the shell's `user_agent_string_for` but uses sampler-local
/// constants. The session typically owns the canonical User-Agent
/// rendering for process-wide HTTP clients; this helper is for
/// per-session sampling clients that want to override it.
pub fn user_agent_string_for(origin: &OriginClientInfo) -> String {
    let agent_version = agent_version();
    let platform = PlatformInfo::current();

    if origin.product == AGENT_PRODUCT && origin.version.as_deref() == Some(agent_version.as_str())
    {
        return format!(
            "{}/{} ({}; {})",
            AGENT_PRODUCT, agent_version, platform.os, platform.arch
        );
    }

    match origin.version.as_deref() {
        Some(origin_version) => format!(
            "{}/{} {}/{} ({}; {})",
            origin.product,
            origin_version,
            AGENT_PRODUCT,
            agent_version,
            platform.os,
            platform.arch
        ),
        None => format!(
            "{} {}/{} ({}; {})",
            origin.product, AGENT_PRODUCT, agent_version, platform.os, platform.arch
        ),
    }
}

fn grok_build_user_agent_string() -> String {
    user_agent_string_for(&OriginClientInfo {
        product: AGENT_PRODUCT.to_string(),
        version: Some(agent_version()),
    })
}

/// Build a structured authentication failure from the secret-free comparison
/// captured at the 401 response boundary. Raw request credentials stay local
/// to [`FinalRequestCredential`] and never enter the error/event metadata.
fn auth_rejected(message: impl Into<String>, comparison: CredentialComparison) -> SamplingError {
    let credential = match comparison.relation {
        SentCredentialRelation::NotSent => SentCredential::Missing,
        SentCredentialRelation::CurrentUnavailable => SentCredential::CurrentUnavailable,
        SentCredentialRelation::SameAsCurrent => SentCredential::SameAsCurrent,
        SentCredentialRelation::DifferentFromCurrent => SentCredential::DifferentFromCurrent,
    };
    SamplingError::Auth {
        message: message.into(),
        credential,
    }
}

// =============================================================================
// SamplingClient
// =============================================================================

impl SamplingClient {
    /// Construct a sampling client from a [`SamplerConfig`].
    ///
    /// Grabs the process-wide shared `reqwest::Client` (HTTP/2 by
    /// default, HTTP/1.1 when `config.force_http1` is set) and
    /// pre-computes the default request headers. This does not perform
    /// any network I/O.
    pub fn new(config: SamplerConfig) -> Result<Self> {
        let endpoint_trust = resolve_endpoint_trust(&config);
        let is_first_party = matches!(endpoint_trust, EndpointTrustClass::FirstPartyXai);
        // Defense in depth (#110, Layer 3). The shell's choke point already
        // refuses to emit this pairing; making it unconstructable means a
        // later regression upstream of here cannot quietly reintroduce it.
        //
        // The trust class alone is not enough here. It is scheme-agnostic by
        // design — it decides *refusals*, where failing closed means treating
        // a cleartext xAI host as first-party — so reusing it made this layer
        // weaker than the choke point it backs up: an ambient credential
        // bound to `http://api.x.ai/v1` constructed happily and went out over
        // cleartext. Adding the scheme requirement composes the shell's
        // bearer-safe predicate out of what is already here (the class
        // already excludes loopback) rather than growing a third copy of the
        // host rules. An explicit `endpoint_trust` still wins, because
        // declaring an origin trusted is the supported way to say so.
        //
        // The message names no secret: a refusal that prints what it refused
        // has not refused anything.
        let ambient_origin_allowed = match config.endpoint_trust {
            Some(trust) => trust == EndpointTrustClass::FirstPartyXai,
            None => {
                is_first_party
                    && reqwest::Url::parse(&config.base_url)
                        .is_ok_and(|url| url.scheme() == "https")
            }
        };
        if !ambient_origin_allowed
            && config
                .credential_source
                .as_ref()
                .is_some_and(xai_grok_sampling_types::CredentialSource::is_ambient_xai)
        {
            return Err(SamplingError::InvalidConfiguration(
                "ambient xAI credential is not allowed for a non-first-party endpoint",
            ));
        }
        let is_codex = matches!(config.api_backend, ApiBackend::CodexResponses);
        // Defense in depth (#135), the provider-scoped counterpart of the
        // #110 gate above: a named auth provider's bearer is valid only for
        // that provider's own origin. The built-in Codex provider is the one
        // whose live resolver this crate attaches, so its credential label
        // plus the Codex transport demands the Codex API origin — with no
        // test-only loopback arm, because a config carrying this label is
        // stating that real account material is attached (loopback mocks use
        // unlabeled configs).
        //
        // Its reach is narrower than it looks, and saying so here is the point:
        // it keys on the credential *label*, and the persisted `SamplingConfig`
        // carries no credential at all -- no `credential_source`, no `api_key`,
        // no `auth_scheme` -- so every reconstruction seam re-derives provenance
        // from headers alone and a provider bearer comes back **unlabelled**.
        // On the main request path this refusal therefore cannot fire, and what
        // actually stops that pairing is `normalize_codex_base_url` below, as a
        // transport question. This is a backstop for paths that do carry the
        // label, not for the turn loop. Closing the seam is #136.
        if is_codex
            && config
                .credential_source
                .as_ref()
                .is_some_and(xai_grok_sampling_types::CredentialSource::is_provider_scoped)
            && !is_codex_credential_origin(&config.base_url)
        {
            return Err(SamplingError::InvalidConfiguration(
                "provider-scoped OpenAI Codex credential is not allowed for a non-Codex endpoint",
            ));
        }
        if is_codex && !config.query_params.is_empty() {
            return Err(SamplingError::InvalidConfiguration(
                "OpenAI Codex transport does not accept query parameters",
            ));
        }
        let base_url = if is_codex {
            normalize_codex_base_url(&config.base_url)?
        } else {
            config.base_url.clone()
        };
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(ref api_key) = config.api_key {
            match config.auth_scheme {
                AuthScheme::XApiKey => {
                    let header_value = HeaderValue::from_str(api_key).map_err(|_| {
                        tracing::debug!("Invalid api_key HTTP header value");
                        SamplingError::auth_unknown(
                            "Invalid api_key: cannot be converted to a valid HTTP header",
                        )
                    })?;
                    headers.insert(HeaderName::from_static("x-api-key"), header_value);
                }
                AuthScheme::Bearer => {
                    let bearer = format!("Bearer {}", api_key);
                    let header_value = HeaderValue::from_str(&bearer).map_err(|_| {
                        tracing::debug!("Invalid Authorization header value");
                        SamplingError::auth_unknown(
                            "Invalid api_key: cannot be converted to a valid HTTP Authorization header",
                        )
                    })?;
                    headers.insert(AUTHORIZATION, header_value);
                }
                AuthScheme::None => {
                    // Explicit no-auth: never emit Authorization / x-api-key from api_key.
                }
            }
        }

        // Apply all extra headers verbatim. This is the single
        // injection point for proxy-auth headers and any other URL- or
        // environment-specific headers the session decides to set.
        let mut explicit_header_names: Vec<HeaderName> = Vec::new();
        for (key, value) in &config.extra_headers {
            let header_name = HeaderName::try_from(key.as_str())
                .map_err(|_| SamplingError::InvalidConfiguration("Invalid extra header name"))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|_| SamplingError::InvalidConfiguration("Invalid extra header value"))?;
            explicit_header_names.push(header_name.clone());
            headers.insert(header_name, header_value);
        }
        for key in config.env_http_headers.keys() {
            if let Ok(header_name) = HeaderName::try_from(key.as_str()) {
                explicit_header_names.push(header_name);
            }
        }

        // Resolve here, not into `extra_headers`, so an env-sourced secret stays
        // out of persisted state.
        apply_env_http_headers(
            &config.env_http_headers,
            |var| std::env::var(var).ok(),
            &mut headers,
        );

        // Explicit no-auth wins over both persisted and env-backed headers.
        if matches!(config.auth_scheme, AuthScheme::None) {
            headers.remove(AUTHORIZATION);
            headers.remove(HeaderName::from_static("x-api-key"));
        }

        // Add x-grok-client-version header for version gating at the proxy.
        if let Some(client_version) = config.client_version.as_ref()
            && let Ok(header_value) = HeaderValue::from_str(client_version)
        {
            headers.insert(
                HeaderName::from_static("x-grok-client-version"),
                header_value,
            );
        }

        if is_first_party {
            if let Some(deployment_id) = config.deployment_id.as_ref()
                && let Ok(header_value) = HeaderValue::from_str(deployment_id)
            {
                headers.insert(
                    HeaderName::from_static("x-grok-deployment-id"),
                    header_value,
                );
            }

            if let Some(user_id) = config.user_id.as_ref()
                && let Ok(header_value) = HeaderValue::from_str(user_id)
            {
                headers.insert(HeaderName::from_static("x-grok-user-id"), header_value);
            }
        } else {
            strip_xai_identity_headers(&mut headers);
        }

        if is_first_party {
            let client_id = config
                .client_identifier
                .clone()
                .unwrap_or_else(|| DEFAULT_CLIENT_IDENTIFIER.to_string());
            if let Ok(header_value) = HeaderValue::from_str(&client_id) {
                headers.insert(
                    HeaderName::from_static("x-grok-client-identifier"),
                    header_value,
                );
            }
        }

        // User-Agent policy: Codex traffic truthfully identifies this
        // product with a fixed Grok Build string — public per-session
        // `origin_client` must never impersonate the official Codex CLI
        // (#42). Other first-party endpoints send the per-session origin;
        // external and local providers get the minimal, non-identifying
        // product string (#6).
        {
            let ua_string = if is_codex {
                grok_build_user_agent_string()
            } else if is_first_party {
                match config.origin_client.as_ref() {
                    Some(origin) => user_agent_string_for(origin),
                    None => user_agent_string_for(&OriginClientInfo {
                        product: AGENT_PRODUCT.to_string(),
                        version: Some(agent_version()),
                    }),
                }
            } else {
                AGENT_PRODUCT.to_string()
            };
            if let Ok(v) = HeaderValue::from_str(&ua_string) {
                headers.insert(USER_AGENT, v);
            }
        }

        // No sampling transport has a legitimate cross-origin redirect;
        // following one would forward already-attached credentials and
        // first-party metadata to an origin that was never classified.
        // The no-redirect builders keep the same pooling and keepalive.
        if config.force_http1 && !is_codex {
            tracing::info!("Using HTTP/1.1 for sampling client (force_http1=true)");
        }
        let http = crate::shared_http::client_no_redirect(config.force_http1)
            .map_err(SamplingError::from)?;

        tracing::info!(
            target: crate::sampling_log::TARGET,
            event = "client_new",
            base_url_present = !config.base_url.is_empty(),
            model = %config.model,
            api_backend = ?config.api_backend,
            auth_scheme = ?config.auth_scheme,
            // "unset" (not "none"): `ReasoningEffort::None` is a real wire value;
            // logging the absent Option as "none" looked like we were sending it.
            reasoning_effort = config.reasoning_effort.map_or("unset", |e| e.as_str()),
            has_api_key = config.api_key.is_some(),
            has_bearer_resolver = config.bearer_resolver.is_some(),
            has_authorization_header = headers.get(AUTHORIZATION).is_some(),
            has_x_api_key_header = headers.get(HeaderName::from_static("x-api-key")).is_some(),
        );

        let defaults = ClientDefaults {
            model: config.model,
            max_completion_tokens: config.max_completion_tokens,
            temperature: config.temperature,
            top_p: config.top_p,
            api_backend: config.api_backend,
            auth_scheme: config.auth_scheme,
            endpoint_trust,
            stream_tool_calls: config.stream_tool_calls,
            doom_loop_recovery: config.doom_loop_recovery,
        };

        let endpoint = EndpointTemplate::new(&base_url, &config.query_params);

        Ok(Self {
            http,
            default_headers: headers,
            base_url,
            defaults,
            attribution_callback: config.attribution_callback,
            bearer_resolver: config.bearer_resolver,
            header_injector: config.header_injector,
            endpoint,
            explicit_header_names,
        })
    }

    /// The configured API backend for this client.
    pub fn api_backend(&self) -> ApiBackend {
        self.defaults.api_backend.clone()
    }

    /// Whether this client targets a first-party xAI endpoint and may send
    /// internal request metadata (`x-grok-*`, client identifier, trace
    /// context). External and local providers receive none of it.
    fn sends_xai_identity_headers(&self) -> bool {
        matches!(
            self.defaults.endpoint_trust,
            EndpointTrustClass::FirstPartyXai
        )
    }

    fn is_codex(&self) -> bool {
        matches!(self.defaults.api_backend, ApiBackend::CodexResponses)
    }

    /// POST with default headers, returning the builder plus the request-local
    /// credential from the final header map.
    ///
    /// A wired bearer_resolver is the sole auth source: a missing live
    /// bearer strips default Authorization / x-api-key so a hard-expired
    /// seed key cannot ride on the wire.
    fn post(
        &self,
        url: impl reqwest::IntoUrl,
    ) -> (reqwest::RequestBuilder, FinalRequestCredential) {
        let mut headers = self.default_headers.clone();
        let mut live_credential = None;
        if let Some(resolver) = &self.bearer_resolver {
            headers.remove(AUTHORIZATION);
            headers.remove(HeaderName::from_static("x-api-key"));
            live_credential = resolver.current_credential();
            if let Some(fresh) = live_credential.as_ref().map(|value| &value.access_token) {
                match self.defaults.auth_scheme {
                    AuthScheme::XApiKey => {
                        if let Ok(v) = HeaderValue::from_str(fresh) {
                            headers.insert(HeaderName::from_static("x-api-key"), v);
                        }
                    }
                    AuthScheme::Bearer => {
                        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {fresh}")) {
                            headers.insert(AUTHORIZATION, v);
                        }
                    }
                    AuthScheme::None => {}
                }
            }
        }
        let codex_authorization = self
            .is_codex()
            .then(|| headers.get(AUTHORIZATION).cloned())
            .flatten();
        let codex_user_agent = self
            .is_codex()
            .then(|| headers.get(USER_AGENT).cloned())
            .flatten();
        if let Some(injector) = &self.header_injector {
            injector.inject(&mut headers);
        }
        // Header injection is allowed to add ordinary proxy headers, but may
        // not bypass explicit no-auth or leak xAI identity to third-party APIs.
        if matches!(self.defaults.auth_scheme, AuthScheme::None) {
            headers.remove(AUTHORIZATION);
            headers.remove(HeaderName::from_static("x-api-key"));
        }
        if !self.sends_xai_identity_headers() {
            // Allowlist boundary for non-xAI endpoints, applied after every
            // injector so neither the trace injector nor extra headers can
            // re-add first-party metadata.
            enforce_external_metadata_boundary(&mut headers, &self.explicit_header_names);
        }
        if self.is_codex() {
            retain_codex_headers(
                &mut headers,
                codex_authorization,
                live_credential
                    .as_ref()
                    .and_then(|credential| credential.account_id.as_deref()),
                live_credential
                    .as_ref()
                    .is_some_and(|credential| credential.chatgpt_account_is_fedramp),
                codex_user_agent,
            );
        } else {
            strip_codex_routing_headers(&mut headers);
        }
        let final_credential = FinalRequestCredential(
            Self::sent_credential_from_headers(&headers, self.defaults.auth_scheme)
                .map(str::to_owned),
        );
        tracing::info!(
            target: crate::sampling_log::TARGET,
            event = "client_post",
            base_url_present = !self.base_url.is_empty(),
            model = %self.defaults.model,
            api_backend = ?self.defaults.api_backend,
            auth_scheme = ?self.defaults.auth_scheme,
            has_bearer_resolver = self.bearer_resolver.is_some(),
            has_authorization_header = headers.get(AUTHORIZATION).is_some(),
            has_x_api_key_header = headers.get(HeaderName::from_static("x-api-key")).is_some(),
            sent_credential_present = final_credential.0.is_some(),
            current_credential_present = final_credential.0.is_some(),
        );
        (self.http.post(url).headers(headers), final_credential)
    }

    fn sent_credential_from_headers(headers: &HeaderMap, scheme: AuthScheme) -> Option<&str> {
        match scheme {
            AuthScheme::XApiKey => headers
                .get(HeaderName::from_static("x-api-key"))
                .and_then(|v| v.to_str().ok()),
            AuthScheme::Bearer => headers
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer ")),
            AuthScheme::None => None,
        }
    }

    fn current_credential_present(&self) -> bool {
        if matches!(self.defaults.auth_scheme, AuthScheme::None) {
            return false;
        }
        if let Some(resolver) = &self.bearer_resolver {
            return resolver.current_bearer().is_some();
        }
        Self::sent_credential_from_headers(&self.default_headers, self.defaults.auth_scheme)
            .is_some()
    }

    /// Compare only after the final response so a rotation between request
    /// construction and rejection is classified as stale rather than current.
    fn compare_final_request_credential(
        &self,
        final_credential: &FinalRequestCredential,
    ) -> CredentialComparison {
        if matches!(self.defaults.auth_scheme, AuthScheme::None) {
            return CredentialComparison::not_sent(false);
        }
        match &self.bearer_resolver {
            Some(resolver) => {
                let current = resolver.current_bearer();
                CredentialComparison::compare(final_credential.0.as_deref(), current.as_deref())
            }
            None => CredentialComparison::compare(
                final_credential.0.as_deref(),
                Self::sent_credential_from_headers(
                    &self.default_headers,
                    self.defaults.auth_scheme,
                ),
            ),
        }
    }

    /// Invoke the optional 401 attribution callback for one logical
    /// 401 response. Each of the six UNAUTHORIZED arms in this file
    /// calls this helper immediately before returning
    /// `SamplingError::Auth(...)`. Emit happens at the lowest layer
    /// that saw the status, so higher layers that react to a 401 must
    /// not emit a duplicate event.
    ///
    fn record_401_attribution(
        &self,
        consumer: crate::attribution::SamplingConsumer,
        final_credential: &FinalRequestCredential,
    ) -> CredentialComparison {
        let comparison = self.compare_final_request_credential(final_credential);
        if let Some(cb) = self.attribution_callback.as_ref() {
            cb.record_401(consumer, comparison);
        }
        comparison
    }

    pub fn auth_info(&self) -> crate::sampling_log::AuthInfo {
        let auth_present = self.current_credential_present();
        let auth_type = match (&self.defaults.auth_scheme, auth_present) {
            (AuthScheme::None, _) => "none",
            (AuthScheme::XApiKey, true) => "x-api-key",
            (AuthScheme::Bearer, true) => "bearer",
            (_, false) => "none",
        };
        crate::sampling_log::AuthInfo {
            auth_type,
            auth_present,
        }
    }

    /// Log only aggregate and known-auth presence. Custom header names and
    /// values may both be user-controlled, so neither crosses into tracing.
    fn log_request_headers(request: &reqwest::Request, endpoint_name: &str) {
        tracing::debug!(
            endpoint = endpoint_name,
            header_count = request.headers().len(),
            authorization_present = request.headers().contains_key(AUTHORIZATION),
            x_api_key_present = request
                .headers()
                .contains_key(HeaderName::from_static("x-api-key")),
            "Request header summary"
        );
    }

    fn endpoint(&self, path: &str) -> String {
        self.endpoint.url_for_path(path)
    }

    fn apply_defaults(&self, mut request: ChatCompletionRequest) -> Result<ChatCompletionRequest> {
        if request.model.is_none() {
            request.model = Some(self.defaults.model.clone());
        }

        if request.max_tokens.is_none() {
            request.max_tokens = self.defaults.max_completion_tokens;
        }

        if request.temperature.is_none() {
            request.temperature = self.defaults.temperature;
        }

        if request.top_p.is_none() {
            request.top_p = self.defaults.top_p;
        }

        Ok(request)
    }

    /// `comparison` describes the final request credential without retaining
    /// credential bytes.
    async fn handle_response(
        &self,
        response: reqwest::Response,
        final_credential: FinalRequestCredential,
    ) -> Result<ChatCompletionResponse> {
        let status = response.status();
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                let comparison = self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ChatCompletions,
                    &final_credential,
                );
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401): {server_message}"),
                    comparison,
                ));
            }
            let message = user_facing_api_error_message(status, bytes.as_ref());
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let completion =
            serde_json::from_slice::<ChatCompletionResponse>(&bytes).map_err(|_| {
                tracing::error!("Failed to deserialize ChatCompletionResponse");
                SamplingError::serialization_message("invalid provider response payload")
            })?;
        Ok(completion)
    }

    // =========================================================================
    // Chat Completions API
    // =========================================================================

    pub async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse> {
        let payload = self.apply_defaults(request)?;
        let x_grok_conv_id = &payload.x_grok_conv_id.clone().unwrap_or_default();
        let x_grok_req_id = &payload.x_grok_req_id.clone().unwrap_or_default();
        let model_id = payload.model.clone().unwrap_or_default();

        tracing::debug!(
            base_url_present = !self.base_url.is_empty(),
            model_id = %model_id,
            "Sending chat completion request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: payload.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: payload.x_grok_turn_idx.as_deref(),
            agent_id: payload.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: payload.x_grok_deployment_id.as_deref(),
            user_id: payload.x_grok_user_id.as_deref(),
        };
        let (builder, final_credential) = self.post(self.endpoint("chat/completions"));
        let http_request = grok_headers
            .apply(builder, self.sends_xai_identity_headers())
            .json(&payload);

        let response = http_request.send().await.map_err(request_transport_error)?;

        self.handle_response(response, final_credential).await
    }

    /// Start a streaming chat completion request. Returns a stream of typed chunks.
    #[tracing::instrument(
        name = "http.chat_completion_stream",
        skip_all,
        fields(
            endpoint = "chat_completions",
            model_id = request.model.as_deref().unwrap_or(""),
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    pub async fn chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        let payload = self.apply_defaults(request)?;
        let x_grok_conv_id = &payload.x_grok_conv_id.clone().unwrap_or_default();
        let x_grok_req_id = &payload.x_grok_req_id.clone().unwrap_or_default();
        let model_id = payload.model.clone().unwrap_or_default();

        // Wrap the request with streaming fields and serialize once.
        // Previously this path serialized twice: first to serde_json::Value
        // (to inject `stream` and `stream_options`), then to HTTP body bytes.
        let streaming_request = StreamingChatRequest {
            inner: &payload,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: payload.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: payload.x_grok_turn_idx.as_deref(),
            agent_id: payload.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: payload.x_grok_deployment_id.as_deref(),
            user_id: payload.x_grok_user_id.as_deref(),
        };
        let (builder, final_credential) = self.post(self.endpoint("chat/completions"));
        let http_request = grok_headers
            .apply(builder, self.sends_xai_identity_headers())
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
            .json(&streaming_request);

        let built_request = http_request.build().map_err(request_build_error)?;

        tracing::debug!(
            method = %built_request.method(),
            "Sending chat/completions request"
        );
        Self::log_request_headers(&built_request, "chat/completions");

        let response = self
            .http
            .execute(built_request)
            .await
            .map_err(stream_transport_error)?;

        let status = response.status();
        let span = tracing::Span::current();
        span.record("status_code", status.as_u16() as i64);
        span.record("success", status.is_success());
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span.record("error", "unauthorized (401)");
                let comparison = self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ChatCompletionsStream,
                    &final_credential,
                );
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401): {server_message}"),
                    comparison,
                ));
            }

            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span.record("error", "provider request failed");
            tracing::error!(
                status = %status,
                "chat/completions API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        // Strip UTF-8 BOM if present: eventsource-stream 0.2.3 incorrectly slices BOM at byte 1 instead of 3.
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        // Turn raw bytes into SSE events
        let event_stream = byte_stream.eventsource();

        // Map SSE events into ChatCompletionChunk.
        // Uses `scan` so that `[DONE]` and transport errors both terminate the
        // stream (`None`). The first transport error is emitted to the consumer,
        // then subsequent polls return `None` -- preventing an infinite busy-loop
        // when the HTTP/2 connection drops and h2 keeps producing errors.
        let chunks = event_stream
            .scan(false, |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Err(stream_error))
                        } else {
                            Some(
                                serde_json::from_str::<ChatCompletionChunk>(data).map_err(|_| {
                                    tracing::error!(
                                        "Failed to deserialize ChatCompletionChunk from stream"
                                    );
                                    SamplingError::serialization_message(
                                        "invalid provider stream payload",
                                    )
                                }),
                            )
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Err(safe_event_stream_error(e)))
                    }
                };
                std::future::ready(item)
            })
            .boxed();

        Ok((chunks, model_metadata))
    }

    // =========================================================================
    // Responses API
    // =========================================================================

    /// Apply default configuration to a Responses API request.
    fn apply_response_defaults(&self, request: &mut CreateResponseWrapper) -> Result<()> {
        // Apply model default if not specified
        if request.inner.model.is_none() {
            request.inner.model = Some(self.defaults.model.clone());
        }

        if self.is_codex() {
            // The ChatGPT Codex Responses contract rejects these sampling
            // knobs. Clear both inherited defaults and explicit caller values
            // at the shared wire-preparation boundary used by streaming and
            // non-streaming requests.
            request.inner.temperature = None;
            request.inner.top_p = None;
        } else {
            // Apply generic Responses defaults if not specified.
            if request.inner.temperature.is_none() {
                request.inner.temperature = self.defaults.temperature;
            }
            if request.inner.top_p.is_none() {
                request.inner.top_p = self.defaults.top_p;
            }
        }

        // Apply max_output_tokens default if not specified
        if request.inner.max_output_tokens.is_none() {
            request.inner.max_output_tokens = self.defaults.max_completion_tokens;
        }

        // The ChatGPT Codex Responses contract sends this field explicitly.
        // Preserve an explicit caller choice, but do not depend on a changing
        // server default for the built-in Codex transport.
        if self.is_codex() && request.inner.parallel_tool_calls.is_none() {
            request.inner.parallel_tool_calls = Some(true);
        }

        // Set store to false if not specified (default is true, but that breaks ZDR compliance)
        if request.inner.store.is_none() {
            request.inner.store = Some(false);
        }

        // Include encrypted reasoning content if not specified
        let includes = request.inner.include.get_or_insert_with(Vec::new);
        if !includes.contains(&rs::IncludeEnum::ReasoningEncryptedContent) {
            includes.push(rs::IncludeEnum::ReasoningEncryptedContent);
        }

        Ok(())
    }

    /// Create a response using the Responses API (non-streaming).
    ///
    /// This uses the Responses API format which provides a simpler interface
    /// for multi-turn conversations and tool calling.
    pub async fn create_response(
        &self,
        mut request: CreateResponseWrapper,
    ) -> Result<rs::Response> {
        self.apply_response_defaults(&mut request)?;

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone().unwrap_or_default();

        // The trace field is process-local: it is consumed by upstream
        // session code (which may upload a payload artifact) and is not
        // forwarded by the sampler. Drop it before we send.
        request.trace.take();

        tracing::debug!(
            endpoint = "responses",
            model_present = !model_id.is_empty(),
            "resolved sampling endpoint"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let mut request_body = serde_json::to_value(&request.inner).map_err(|e| {
            tracing::error!("Failed to serialize responses request: {}", e);
            SamplingError::Serialization(e)
        })?;
        if self.is_codex() {
            xai_grok_sampling_types::patch_codex_instructions(&mut request_body);
        }
        if !self.sends_xai_identity_headers() {
            anonymize_prompt_cache_key(&mut request_body);
        }
        // async-openai's ReasoningTextContent struct omits the `type`
        // discriminator that the Responses API requires on input. Patch
        // it in post-serialize. This is the last surviving piece of the
        // old raw_output machinery.
        xai_grok_sampling_types::patch_reasoning_text_types(&mut request_body);
        let (builder, final_credential) = self.post(self.endpoint("responses"));
        let http_request = if self.is_codex() {
            builder
        } else {
            grok_headers.apply(builder, self.sends_xai_identity_headers())
        }
        .json(&request_body);

        let response = http_request.send().await.map_err(request_transport_error)?;

        let status = response.status();
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                let comparison = self.record_401_attribution(
                    crate::attribution::SamplingConsumer::Responses,
                    &final_credential,
                );
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401): {server_message}"),
                    comparison,
                ));
            }

            let message = user_facing_api_error_message(status, bytes.as_ref());
            tracing::warn!(
                status = %status,
                "responses API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let response_obj = serde_json::from_slice::<rs::Response>(&bytes).map_err(|_| {
            tracing::error!("Failed to deserialize rs::Response");
            SamplingError::serialization_message("invalid provider response payload")
        })?;
        Ok(response_obj)
    }

    /// Create a streaming response using the Responses API.
    ///
    /// Returns a stream of `rs::ResponseStreamEvent` which includes events like:
    /// - `response.created` - Initial response object
    /// - `response.output_text.delta` - Text content deltas
    /// - `response.function_call_arguments.delta` - Function call argument deltas
    /// - `response.completed` - Final response with all output
    ///
    /// The third tuple element is a per-request doom-loop signal collector,
    /// `Some` only when `SamplerConfig::doom_loop_recovery` is set — the same
    /// gate that adds the opt-in `x-grok-doom-loop-check` request header, so
    /// header and parse protection cannot drift apart. It is filled by the
    /// SSE decoder as the server reports triggers and is meant to be handed
    /// to `stream_responses` so the signals land on the final
    /// `ConversationResponse`.
    #[tracing::instrument(
        name = "http.create_response_stream",
        skip_all,
        fields(
            endpoint = "responses",
            model_id = request.inner.model.as_deref().unwrap_or(""),
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    #[allow(clippy::type_complexity)]
    pub async fn create_response_stream(
        &self,
        mut request: CreateResponseWrapper,
    ) -> Result<(
        BoxStream<'static, Result<rs::ResponseStreamEvent>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        self.apply_response_defaults(&mut request)?;

        // Enable streaming
        request.inner.stream = Some(true);

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone().unwrap_or_default();

        // Drop process-local trace data (see note in `create_response`).
        request.trace.take();

        tracing::debug!(
            base_url_present = !self.base_url.is_empty(),
            model_id = model_id.as_str(),
            "Sending responses API stream request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let extra_tool_entries = std::mem::take(&mut request.extra_tool_entries);
        let mut request_body = serde_json::to_value(&request.inner).map_err(|e| {
            tracing::error!("Failed to serialize responses request: {}", e);
            SamplingError::Serialization(e)
        })?;
        if self.is_codex() {
            xai_grok_sampling_types::patch_codex_instructions(&mut request_body);
        }
        if !self.sends_xai_identity_headers() {
            anonymize_prompt_cache_key(&mut request_body);
        }
        // Inject xAI-specific fields only for the generic Responses transport.
        if !self.is_codex() && self.defaults.stream_tool_calls {
            request_body["stream_tool_calls"] = serde_json::json!(true);
        }
        // Inject xAI-specific tools (e.g., x_search) that can't be expressed
        // via async_openai's rs::Tool enum.
        if !self.is_codex() && !extra_tool_entries.is_empty() {
            if let Some(tools) = request_body.get_mut("tools").and_then(|v| v.as_array_mut()) {
                tools.extend(extra_tool_entries);
            } else {
                request_body["tools"] = serde_json::Value::Array(extra_tool_entries);
            }
        }
        xai_grok_sampling_types::patch_reasoning_text_types(&mut request_body);
        // Fresh per attempt so signals never leak across retries; `None`
        // (check disabled) sends no header and does no peek work per event.
        // First-party only: the doom-loop check is an xAI server feature and
        // its opt-in header is internal metadata (rides the builder after
        // `post()`, so it must be gated here, not at the boundary).
        let doom_loop = (!self.is_codex() && self.sends_xai_identity_headers())
            .then_some(self.defaults.doom_loop_recovery)
            .flatten()
            .map(crate::doom_loop::DoomLoopSignalCollector::new);
        let (builder, final_credential) = self.post(self.endpoint("responses"));
        let mut http_request = if self.is_codex() {
            builder
        } else {
            grok_headers.apply(builder, self.sends_xai_identity_headers())
        }
        .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if doom_loop.is_some() {
            // Presence opts in; the server ignores the value.
            http_request = http_request.header(DOOM_LOOP_CHECK_HEADER, "true");
        }
        let http_request = http_request.json(&request_body);

        let built_request = http_request.build().map_err(request_build_error)?;

        tracing::debug!(
            method = %built_request.method(),
            "Sending responses API stream request"
        );
        Self::log_request_headers(&built_request, "responses");

        let response = self
            .http
            .execute(built_request)
            .await
            .map_err(stream_transport_error)?;

        let status = response.status();
        let span = tracing::Span::current();
        span.record("status_code", status.as_u16() as i64);
        span.record("success", status.is_success());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span.record("error", "unauthorized (401)");
                let comparison = self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ResponsesStream,
                    &final_credential,
                );
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401): {server_message}"),
                    comparison,
                ));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span.record("error", "provider request failed");
            tracing::error!(
                status = %status,
                "responses API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let model_metadata = extract_model_metadata(response.headers());

        // Strip UTF-8 BOM if present
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        // Turn raw bytes into SSE events
        let event_stream = byte_stream.eventsource();

        let doom_loop_for_stream = doom_loop.clone();

        // The scan item is an `Option`: `Some(None)` skips an absorbed
        // doom-loop event without terminating the stream (`filter_map`
        // below), while an outer `None` still ends it.
        let events = event_stream
            .scan(false, move |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        // Intercept the non-standard doom-loop event before
                        // typed deserialization; async-openai's event enum
                        // does not know it and would fail to parse it. With
                        // the check disabled, the shared name-or-payload-type
                        // predicate guards against a server emitting it
                        // despite no opt-in (rollout skew), named or not.
                        let swallow = match &doom_loop_for_stream {
                            Some(collector) => collector.absorb(&event.event, data),
                            None => is_check_event(&event.event, data),
                        };
                        if swallow {
                            Some(None)
                        } else if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Some(Err(stream_error)))
                        } else {
                            Some(Some(deserialize_response_event(data)))
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Some(Err(safe_event_stream_error(e))))
                    }
                };
                std::future::ready(item)
            })
            .filter_map(std::future::ready)
            .boxed();

        Ok((events, model_metadata, doom_loop))
    }

    // =========================================================================
    // Anthropic Messages API
    // =========================================================================

    /// Apply default configuration to a Messages API request.
    fn apply_message_defaults(&self, request: &mut MessagesRequestWrapper) -> Result<()> {
        // Apply model default if not specified
        if request.inner.model.is_empty() {
            request.inner.model = self.defaults.model.clone();
        }

        if request.inner.max_tokens == 0 {
            request.inner.max_tokens = self
                .defaults
                .max_completion_tokens
                .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);
        }

        // Apply temperature default if not specified
        if request.inner.temperature.is_none() {
            request.inner.temperature = self.defaults.temperature;
        }

        // Apply top_p default if not specified
        if request.inner.top_p.is_none() {
            request.inner.top_p = self.defaults.top_p;
        }

        Ok(())
    }

    /// Create a message using the Anthropic Messages API (non-streaming).
    pub async fn create_message(
        &self,
        mut request: MessagesRequestWrapper,
    ) -> Result<messages::MessagesResponse> {
        self.apply_message_defaults(&mut request)?;

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone();

        // Drop process-local trace data.
        request.trace.take();

        tracing::debug!(
            endpoint = "messages",
            model_present = !model_id.is_empty(),
            "resolved sampling endpoint"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let (builder, final_credential) = self.post(self.endpoint("messages"));
        let http_request = grok_headers
            .apply(builder, self.sends_xai_identity_headers())
            .json(&request.inner);

        let response = http_request.send().await.map_err(request_transport_error)?;

        let status = response.status();
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                let comparison = self.record_401_attribution(
                    crate::attribution::SamplingConsumer::Messages,
                    &final_credential,
                );
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401): {server_message}"),
                    comparison,
                ));
            }

            let message = user_facing_api_error_message(status, bytes.as_ref());
            tracing::warn!(
                status = %status,
                "messages API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let response_obj =
            serde_json::from_slice::<messages::MessagesResponse>(&bytes).map_err(|_| {
                tracing::error!("Failed to deserialize MessagesResponse");
                SamplingError::serialization_message("invalid provider response payload")
            })?;
        Ok(response_obj)
    }

    /// Create a streaming message using the Anthropic Messages API.
    ///
    /// Returns a stream of `MessageStreamEvent` which includes events like:
    /// - `message_start` - Initial message object
    /// - `content_block_start` / `content_block_delta` / `content_block_stop` - Content blocks
    /// - `message_delta` / `message_stop` - Final message with stop reason
    #[tracing::instrument(
        name = "http.create_message_stream",
        skip_all,
        fields(
            endpoint = "messages",
            model_id = request.inner.model.as_str(),
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    pub async fn create_message_stream(
        &self,
        mut request: MessagesRequestWrapper,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_message_defaults(&mut request)?;

        // Enable streaming
        request.inner.stream = Some(true);

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone();

        // Drop process-local trace data.
        request.trace.take();

        tracing::debug!(
            base_url_present = !self.base_url.is_empty(),
            model_id = model_id.as_str(),
            "Sending Messages API stream request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let (builder, final_credential) = self.post(self.endpoint("messages"));
        let http_request = grok_headers
            .apply(builder, self.sends_xai_identity_headers())
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
            .json(&request.inner);

        let built_request = http_request.build().map_err(request_build_error)?;

        tracing::debug!(
            method = %built_request.method(),
            "Sending messages API stream request"
        );
        Self::log_request_headers(&built_request, "messages");

        let response = self
            .http
            .execute(built_request)
            .await
            .map_err(stream_transport_error)?;

        let status = response.status();
        let span = tracing::Span::current();
        span.record("status_code", status.as_u16() as i64);
        span.record("success", status.is_success());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span.record("error", "unauthorized (401)");
                let comparison = self.record_401_attribution(
                    crate::attribution::SamplingConsumer::MessagesStream,
                    &final_credential,
                );
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(auth_rejected(
                    format!("Unauthorized (401): {server_message}"),
                    comparison,
                ));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span.record("error", "provider request failed");
            tracing::error!(
                status = %status,
                "messages API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let model_metadata = extract_model_metadata(response.headers());

        // Strip UTF-8 BOM if present
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        // Turn raw bytes into SSE events
        let event_stream = byte_stream.eventsource();

        // Map SSE events into MessageStreamEvent.
        // Uses `scan` so transport errors terminate the stream after the first
        // error (same pattern as `chat_completion_stream`).
        let events = event_stream
            .scan(false, |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Err(stream_error))
                        } else {
                            Some(
                                serde_json::from_str::<messages::MessageStreamEvent>(data).map_err(
                                    |_| {
                                        tracing::error!(
                                            "Failed to deserialize MessageStreamEvent from stream"
                                        );
                                        SamplingError::serialization_message(
                                            "invalid provider stream payload",
                                        )
                                    },
                                ),
                            )
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Err(safe_event_stream_error(e)))
                    }
                };
                std::future::ready(item)
            })
            .boxed();

        Ok((events, model_metadata))
    }

    // =========================================================================
    // Unified Conversation API
    // =========================================================================

    /// Apply default configuration to a ConversationRequest.
    fn apply_conversation_defaults(&self, request: &mut ConversationRequest) -> Result<()> {
        if request.model.is_none() {
            request.model = Some(self.defaults.model.clone());
        }

        if request.temperature.is_none() {
            request.temperature = self.defaults.temperature;
        }

        if request.top_p.is_none() {
            request.top_p = self.defaults.top_p;
        }

        if request.max_output_tokens.is_none() {
            request.max_output_tokens = self.defaults.max_completion_tokens;
        }

        Ok(())
    }

    /// Send a conversation request using the Chat Completions API (streaming).
    ///
    /// Converts the `ConversationRequest` to `ChatCompletionRequest` internally.
    /// Returns the stream and any model metadata extracted from response headers.
    pub async fn conversation_stream(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let mut chat_request: ChatCompletionRequest = request.into();
        if let Some(trace) = trace {
            chat_request.trace = Some(trace);
        }

        self.chat_completion_stream(chat_request).await
    }

    /// Send a conversation request using the Chat Completions API (non-streaming).
    ///
    /// Converts the `ConversationRequest` to `ChatCompletionRequest` internally.
    pub async fn conversation(
        &self,
        mut request: ConversationRequest,
    ) -> Result<ChatCompletionResponse> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let mut chat_request: ChatCompletionRequest = request.into();
        if let Some(trace) = trace {
            chat_request.trace = Some(trace);
        }

        self.chat_completion(chat_request).await
    }

    /// Send a conversation request using the Responses API (streaming).
    ///
    /// Converts the `ConversationRequest` to Responses API format internally.
    /// The third tuple element is the per-request doom-loop signal collector
    /// (see [`Self::create_response_stream`]); callers that don't consume the
    /// signals can ignore it.
    #[allow(clippy::type_complexity)]
    pub async fn conversation_stream_responses(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<rs::ResponseStreamEvent>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        // Collect xAI-specific tools that can't be expressed via rs::Tool
        // (e.g., x_search). These are injected as raw JSON after serialization.
        let extra_tools = xai_grok_sampling_types::extra_tool_entries(&request.hosted_tools);

        let responses_request: rs::CreateResponse = (&request).into();

        let mut wrapper = CreateResponseWrapper::new(responses_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;
        wrapper.extra_tool_entries = extra_tools;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_response_stream(wrapper).await
    }

    /// Send a conversation request using the Responses API (non-streaming).
    ///
    /// Converts the `ConversationRequest` to Responses API format internally.
    pub async fn conversation_responses(
        &self,
        mut request: ConversationRequest,
    ) -> Result<rs::Response> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        let responses_request: rs::CreateResponse = (&request).into();

        let mut wrapper = CreateResponseWrapper::new(responses_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_response(wrapper).await
    }

    /// Send a conversation request using the Anthropic Messages API (streaming).
    ///
    /// Converts the `ConversationRequest` to Messages API format internally.
    pub async fn conversation_stream_messages(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        let messages_request = build_messages_request(&request);

        let mut wrapper = MessagesRequestWrapper::new(messages_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_message_stream(wrapper).await
    }

    /// Send a conversation request using the Anthropic Messages API (non-streaming).
    ///
    /// Converts the `ConversationRequest` to Messages API format internally.
    pub async fn conversation_messages(
        &self,
        mut request: ConversationRequest,
    ) -> Result<messages::MessagesResponse> {
        self.apply_conversation_defaults(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        let messages_request = build_messages_request(&request);

        let mut wrapper = MessagesRequestWrapper::new(messages_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_message(wrapper).await
    }

    /// Backend-aware streaming call that collects the full response.
    pub async fn conversation_collect(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationResponse> {
        let request_id = crate::types::RequestId::random();
        let idle_timeout = std::time::Duration::from_secs(300);
        let result = match self.api_backend() {
            ApiBackend::ChatCompletions => {
                let (raw, meta) = self.conversation_stream(request).await?;
                let events =
                    crate::stream::stream_chat_completions(raw, meta, request_id, idle_timeout);
                crate::stream::collect_response(events).await
            }
            ApiBackend::Responses | ApiBackend::CodexResponses => {
                let (raw, meta, doom_loop) = self.conversation_stream_responses(request).await?;
                let events =
                    crate::stream::stream_responses(raw, meta, request_id, idle_timeout, doom_loop);
                crate::stream::collect_response(events).await
            }
            ApiBackend::Messages => {
                let (raw, meta) = self.conversation_stream_messages(request).await?;
                let events = crate::stream::stream_messages(raw, meta, request_id, idle_timeout);
                crate::stream::collect_response(events).await
            }
        };
        result
            .map(|(response, _metrics)| response)
            .map_err(stream_collect_error)
    }
}

/// Rebuild `Api` from stream-collected info, preserving status,
/// `Retry-After`, and `x-should-retry` (kind is lost on this path).
fn stream_collect_error(info: SamplingErrorInfo) -> SamplingError {
    SamplingError::Api {
        status: info
            .status_code
            .and_then(|c| reqwest::StatusCode::from_u16(c).ok())
            .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
        message: info.message,
        model_metadata: info.model_metadata,
        retry_after_secs: info.retry_after_secs,
        should_retry: info.should_retry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use xai_grok_sampling_types::types::ChatRequestMessage;

    #[derive(Clone, Default)]
    struct SecretLogCapture {
        output: Arc<Mutex<String>>,
        next_span_id: Arc<AtomicU64>,
    }

    struct SecretFieldVisitor<'a>(&'a Mutex<String>);

    impl tracing::field::Visit for SecretFieldVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let mut output = self.0.lock().expect("capture lock");
            let _ = write!(output, "{}={value:?};", field.name());
        }
    }

    impl tracing::Subscriber for SecretLogCapture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            attrs.record(&mut SecretFieldVisitor(&self.output));
            tracing::span::Id::from_u64(self.next_span_id.fetch_add(1, Ordering::Relaxed) + 1)
        }

        fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
            values.record(&mut SecretFieldVisitor(&self.output));
        }

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            event.record(&mut SecretFieldVisitor(&self.output));
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    fn assert_no_secret_fragments(output: &str, secret: &str) {
        assert!(
            !output.contains(secret),
            "full credential appeared in observable output: {output}"
        );
        for window in secret.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII test sentinel");
            assert!(
                !output.contains(fragment),
                "credential fragment {fragment:?} appeared in observable output: {output}"
            );
        }
    }

    #[test]
    fn stream_collect_error_preserves_should_retry() {
        let info = SamplingErrorInfo {
            kind: crate::events::SamplingErrorKind::Api,
            status_code: Some(529),
            message: "Overloaded".into(),
            is_retryable: true,
            retry_after_secs: Some(3),
            should_retry: Some(false),
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: xai_grok_sampling_types::SentCredential::Unknown,
        };
        // SamplingError is not PartialEq (it carries reqwest/serde errors),
        // so destructure once and compare all fields in a single assert.
        let SamplingError::Api {
            status,
            message,
            model_metadata,
            retry_after_secs,
            should_retry,
        } = stream_collect_error(info)
        else {
            panic!("expected Api");
        };
        assert_eq!(
            (
                status.as_u16(),
                message.as_str(),
                model_metadata.is_none(),
                retry_after_secs,
                should_retry,
            ),
            (529, "Overloaded", true, Some(3), Some(false)),
        );
    }

    fn minimal_config() -> SamplerConfig {
        SamplerConfig {
            api_key: Some("test-key".to_string()),
            base_url: "https://example.test".to_string(),
            model: "test-model".to_string(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: ApiBackend::ChatCompletions,
            endpoint_trust: None,
            credential_source: None,
            auth_scheme: AuthScheme::Bearer,
            extra_headers: IndexMap::new(),
            query_params: IndexMap::new(),
            env_http_headers: IndexMap::new(),
            context_window: 8192,
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

    fn minimal_chat_request() -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: Some("test-model".into()),
            messages: vec![ChatRequestMessage::user("hello")],
            temperature: None,
            max_tokens: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            user: None,
            tools: None,
            tool_choice: None,
            search_parameters: None,
            response_format: None,
            reasoning_effort: None,
            x_grok_conv_id: None,
            x_grok_req_id: None,
            x_grok_session_id: None,
            x_grok_turn_idx: None,
            x_grok_agent_id: None,
            x_grok_deployment_id: None,
            x_grok_user_id: None,
            trace: None,
        }
    }

    #[test]
    fn sampler_request_logs_never_emit_credential_bytes() {
        for (scheme, secret) in [
            (AuthScheme::Bearer, "Z9q7V5n3K1m8R6t4P2s0D8f6"),
            (AuthScheme::XApiKey, "H4w2C0y8B6u3N1j9L7e5A3d1"),
        ] {
            let capture = SecretLogCapture::default();
            let output = capture.output.clone();
            let guard = tracing::subscriber::set_default(capture);

            let mut config = minimal_config();
            config.api_key = Some(secret.to_string());
            config.auth_scheme = scheme;
            config.base_url = format!("https://example.test/v1?embedded={secret}");
            config
                .query_params
                .insert("signed_query".to_string(), secret.to_string());
            config
                .extra_headers
                .insert("x-proxy-credential".to_string(), secret.to_string());

            let client = SamplingClient::new(config).expect("client should build");
            let (builder, _) = client.post(client.endpoint("chat/completions"));
            let request = builder.body("").build().expect("request should build");
            SamplingClient::log_request_headers(&request, "chat/completions");
            let _sampling_span = crate::sampling_log::request_span(
                &crate::types::RequestId::from("request-id"),
                "test-model",
                "chat_completions",
                &format!("https://example.test/v1?signed={secret}"),
                &client.auth_info(),
            );
            let rendered_client = format!("{client:?}");

            drop(guard);
            let captured = output.lock().expect("capture lock").clone();
            assert_no_secret_fragments(&captured, secret);
            assert_no_secret_fragments(&rendered_client, secret);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transport_failure_never_emits_query_credential_bytes() {
        let secret = "Q8z6X4c2V0b9N7m5K3j1H9f7";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind local port");
        let address = listener.local_addr().expect("local address");
        drop(listener);

        let capture = SecretLogCapture::default();
        let output = capture.output.clone();
        let guard = tracing::subscriber::set_default(capture);

        let mut config = minimal_config();
        config.api_key = Some(secret.to_string());
        config.base_url = format!("http://{address}/v1");
        config
            .query_params
            .insert("api_key".to_string(), secret.to_string());
        let client = SamplingClient::new(config).expect("client should build");
        let error = client
            .chat_completion(minimal_chat_request())
            .await
            .expect_err("closed local port must fail");
        let display = error.to_string();
        let debug = format!("{error:?}");

        if let SamplingError::Http(inner) = &error {
            assert!(
                inner.url().is_none(),
                "sanitized reqwest error retained URL"
            );
        } else {
            panic!("expected transport error, got {error:?}");
        }

        drop(guard);
        let captured = output.lock().expect("capture lock").clone();
        assert_no_secret_fragments(&captured, secret);
        assert_no_secret_fragments(&display, secret);
        assert_no_secret_fragments(&debug, secret);
    }

    async fn provider_controlled_event_stream_error(
        secret: &str,
    ) -> EventStreamError<std::io::Error> {
        let source = futures_util::stream::iter([Err::<Vec<u8>, std::io::Error>(
            std::io::Error::other(secret.to_owned()),
        )]);
        let mut events = source.eventsource();
        events
            .next()
            .await
            .expect("transport failure must yield one stream error")
            .expect_err("transport failure must not parse as an event")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn provider_controlled_stream_error_is_secret_free_for_all_stream_apis() {
        let secret = "V6b4N2m0C8x6Z4l2K0j8H6g4";
        for endpoint in ["chat_completions", "responses", "messages"] {
            let raw_error = provider_controlled_event_stream_error(secret).await;
            assert!(
                raw_error.to_string().contains(secret),
                "fixture must prove dependency error retains provider input for {endpoint}"
            );

            let capture = SecretLogCapture::default();
            let output = capture.output.clone();
            let guard = tracing::subscriber::set_default(capture);
            let safe = safe_event_stream_error(raw_error);
            let rendered = format!("{safe} {safe:?}");
            drop(guard);

            assert_no_secret_fragments(&rendered, secret);
            assert_no_secret_fragments(&output.lock().expect("capture lock"), secret);
        }
    }

    /// Verify the serialized shape of StreamingChatRequest matches the
    /// expected wire format: all ChatCompletionRequest fields flattened at
    /// top level, plus `stream: true` and `stream_options.include_usage: true`.
    #[test]
    fn streaming_chat_request_serializes_correctly() {
        let request = ChatCompletionRequest {
            model: Some("test-model".into()),
            messages: vec![ChatRequestMessage::user("hello")],
            temperature: Some(0.7),
            max_tokens: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            user: None,
            tools: None,
            tool_choice: None,
            search_parameters: None,
            response_format: None,
            reasoning_effort: None,
            x_grok_conv_id: None,
            x_grok_req_id: None,
            x_grok_session_id: None,
            x_grok_turn_idx: None,
            x_grok_agent_id: None,
            x_grok_deployment_id: None,
            x_grok_user_id: None,
            trace: None,
        };

        let wrapper = StreamingChatRequest {
            inner: &request,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let json: serde_json::Value = serde_json::to_value(&wrapper).unwrap();
        let obj = json.as_object().unwrap();

        assert_eq!(obj.get("stream").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            obj.get("stream_options")
                .and_then(|v| v.get("include_usage"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );

        assert!(
            obj.get("inner").is_none(),
            "inner field should be flattened"
        );
        assert_eq!(
            obj.get("model").and_then(|v| v.as_str()),
            Some("test-model")
        );
        assert!(obj.get("messages").is_some());
        let temp = obj.get("temperature").and_then(|v| v.as_f64()).unwrap();
        assert!((temp - 0.7).abs() < 0.001, "temperature should be ~0.7");

        assert!(obj.get("max_tokens").is_none());
        assert!(obj.get("tools").is_none());
    }

    #[test]
    fn extract_retry_after_parses_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(30));
    }

    #[test]
    fn extract_retry_after_caps_at_120() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(120));
    }

    #[test]
    fn extract_retry_after_zero_is_valid() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "0".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(0));
    }

    #[test]
    fn extract_retry_after_ignores_http_date() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Fri, 31 Dec 2025 23:59:59 GMT".parse().unwrap(),
        );
        assert_eq!(extract_retry_after(&headers), None);
    }

    #[test]
    fn extract_retry_after_none_when_missing() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(extract_retry_after(&headers), None);
    }

    #[test]
    fn extract_should_retry_true() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "true".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(true));
    }

    #[test]
    fn extract_should_retry_true_case_insensitive() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "TRUE".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(true));
    }

    #[test]
    fn extract_should_retry_false() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "false".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(false));
    }

    #[test]
    fn extract_should_retry_unknown_value_is_none() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "banana".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), None);
    }

    #[test]
    fn extract_should_retry_absent_is_none() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(extract_should_retry(&headers), None);
    }

    #[test]
    fn new_with_minimal_config_succeeds() {
        let client = SamplingClient::new(minimal_config()).expect("client should construct");
        assert_eq!(client.api_backend(), ApiBackend::ChatCompletions);
    }

    #[test]
    fn new_applies_extra_headers() {
        let mut cfg = minimal_config();
        cfg.extra_headers
            .insert("x-test-header".to_string(), "test-value".to_string());
        cfg.extra_headers
            .insert("x-XAI-token-auth".to_string(), "xai-grok-cli".to_string());
        let _client = SamplingClient::new(cfg).expect("client with extra headers should construct");
    }

    #[test]
    fn apply_env_http_headers_resolves_trims_skips_and_overrides() {
        let mut map = IndexMap::new();
        map.insert("x-tenant-token".to_string(), "TENANT".to_string());
        map.insert("x-blank".to_string(), "BLANK".to_string());
        map.insert("x-missing".to_string(), "MISSING".to_string());
        map.insert("x-override".to_string(), "OVERRIDE".to_string());
        map.insert("x invalid".to_string(), "INVALID".to_string());

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-override"),
            HeaderValue::from_static("static"),
        );

        apply_env_http_headers(
            &map,
            |var| match var {
                // Leading space + trailing newline exercises trimming.
                "TENANT" => Some(" tenant-secret\n".to_string()),
                "BLANK" => Some("   ".to_string()),
                "OVERRIDE" => Some("from-env".to_string()),
                "INVALID" => Some("value".to_string()),
                _ => None,
            },
            &mut headers,
        );

        assert_eq!(headers.get("x-tenant-token").unwrap(), "tenant-secret");
        assert!(headers.get("x-blank").is_none());
        assert!(headers.get("x-missing").is_none());
        // A resolved env value overrides an existing header of the same name.
        assert_eq!(headers.get("x-override").unwrap(), "from-env");
        // An invalid header name is skipped rather than panicking.
        assert!(headers.get("x invalid").is_none());
    }

    #[test]
    fn endpoint_appends_path_before_a_base_url_query_without_configured_params() {
        let template =
            EndpointTemplate::new("https://gateway.example/v1?api-version=x", &IndexMap::new());
        let url = template.url_for_path("responses");
        assert!(
            url.starts_with("https://gateway.example/v1/responses?"),
            "url: {url}"
        );
        assert!(url.contains("api-version=x"), "url: {url}");
        assert!(!url.contains("x/responses"), "url: {url}");
    }

    #[test]
    fn messages_plus_anthropic_api_key_uses_x_api_key_and_not_authorization() {
        let cfg = SamplerConfig {
            api_key: Some("anthropic-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-api-key"))
                .is_some()
        );
        assert!(client.default_headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn messages_plus_bearer_uses_authorization_and_not_x_api_key() {
        let cfg = SamplerConfig {
            api_key: Some("bearer-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::Bearer,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(client.default_headers.get(AUTHORIZATION).is_some());
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-api-key"))
                .is_none()
        );
    }

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
    fn none_scheme_auth_info_reports_none_without_prefix() {
        #[derive(Debug)]
        struct LeakResolver;
        impl crate::config::BearerResolver for LeakResolver {
            fn current_bearer(&self) -> Option<String> {
                Some("live-should-not-leak".into())
            }
        }
        let mut cfg = SamplerConfig {
            api_key: Some("should-not-leak".to_string()),
            auth_scheme: AuthScheme::None,
            ..minimal_config()
        };
        cfg.bearer_resolver = Some(std::sync::Arc::new(LeakResolver));
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
        let (builder, final_credential) = client.post("http://localhost/test");
        let req = builder.build().expect("build request");
        assert_eq!(
            client.compare_final_request_credential(&final_credential),
            CredentialComparison::not_sent(false)
        );
        assert!(req.headers().get(AUTHORIZATION).is_none());
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-api-key"))
                .is_none()
        );
    }

    #[test]
    fn final_relation_observes_hostile_injector_replacement_and_removal() {
        #[derive(Debug)]
        struct ReplaceInjector;
        impl crate::config::HeaderInjector for ReplaceInjector {
            fn inject(&self, headers: &mut HeaderMap) {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_static("Bearer replacement"),
                );
            }
        }

        #[derive(Debug)]
        struct RemoveInjector;
        impl crate::config::HeaderInjector for RemoveInjector {
            fn inject(&self, headers: &mut HeaderMap) {
                headers.remove(AUTHORIZATION);
                headers.remove(HeaderName::from_static("x-api-key"));
            }
        }

        let mut replaced = minimal_config();
        replaced.api_key = Some("configured".to_string());
        replaced.header_injector = Some(std::sync::Arc::new(ReplaceInjector));
        let client = SamplingClient::new(replaced).expect("client should build");
        let (_, final_credential) = client.post("http://localhost/test");
        assert_eq!(
            client.compare_final_request_credential(&final_credential),
            CredentialComparison::different_from_current()
        );

        let mut removed = minimal_config();
        removed.api_key = Some("configured".to_string());
        removed.header_injector = Some(std::sync::Arc::new(RemoveInjector));
        let client = SamplingClient::new(removed).expect("client should build");
        let (_, final_credential) = client.post("http://localhost/test");
        assert_eq!(
            client.compare_final_request_credential(&final_credential),
            CredentialComparison::not_sent(true)
        );
    }

    #[test]
    fn none_scheme_post_strips_auth_even_from_extra_headers() {
        let mut cfg = SamplerConfig {
            api_key: None,
            auth_scheme: AuthScheme::None,
            ..minimal_config()
        };
        cfg.extra_headers
            .insert("Authorization".to_string(), "Bearer leaked".to_string());
        cfg.extra_headers
            .insert("x-api-key".to_string(), "leaked-key".to_string());
        let client = SamplingClient::new(cfg).expect("client should build");
        let req = client
            .post("http://localhost/test")
            .0
            .build()
            .expect("build request");
        assert!(
            req.headers().get(AUTHORIZATION).is_none(),
            "AuthScheme::None must strip Authorization even when injected via extra_headers"
        );
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-api-key"))
                .is_none(),
            "AuthScheme::None must strip x-api-key even when injected via extra_headers"
        );
    }

    #[test]
    fn none_and_third_party_url_omits_xai_identity_headers() {
        let cfg = SamplerConfig {
            auth_scheme: AuthScheme::None,
            base_url: "https://api.openai.com/v1".to_string(),
            deployment_id: Some("deploy-must-not-leak".to_string()),
            user_id: Some("user-must-not-leak".to_string()),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let req = client
            .post("https://api.openai.com/v1/chat/completions")
            .0
            .build()
            .expect("build request");
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-grok-deployment-id"))
                .is_none(),
            "None + third-party must omit x-grok-deployment-id"
        );
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-grok-user-id"))
                .is_none(),
            "None + third-party must omit x-grok-user-id"
        );
    }

    #[test]
    fn none_scheme_omits_xai_identity_headers_on_first_party_base_url() {
        let cfg = SamplerConfig {
            auth_scheme: AuthScheme::None,
            base_url: "https://api.x.ai/v1".to_string(),
            deployment_id: Some("deploy-must-not-leak".to_string()),
            user_id: Some("user-must-not-leak".to_string()),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-grok-deployment-id"))
                .is_none(),
            "AuthScheme::None must omit x-grok-deployment-id"
        );
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-grok-user-id"))
                .is_none(),
            "AuthScheme::None must omit x-grok-user-id"
        );
        let req = client
            .post("https://api.x.ai/v1/chat/completions")
            .0
            .build()
            .expect("build request");
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-grok-deployment-id"))
                .is_none()
        );
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-grok-user-id"))
                .is_none()
        );
    }

    #[test]
    /// #110 Layer 3. The choke point in the shell already refuses to emit this
    /// combination; this makes it unrepresentable, so a later regression
    /// upstream of here cannot quietly reintroduce it. The error names no
    /// secret -- a refusal that prints the credential it refused is not a
    /// refusal.
    #[test]
    fn ambient_xai_credential_cannot_construct_for_non_first_party_endpoint() {
        use crate::config::CredentialSource;
        for source in [
            CredentialSource::XaiSession,
            CredentialSource::XaiApiKeyEnv,
            CredentialSource::XaiDeploymentKey,
        ] {
            for base_url in [
                "https://api.openai.com/v1",
                "http://127.0.0.1:11434/v1",
                // An xAI host over cleartext. The trust class calls this
                // first-party -- it is scheme-agnostic on purpose, because it
                // decides *refusals* -- so classifying by trust alone made
                // this layer weaker than the choke point it backs up.
                "http://api.x.ai/v1",
            ] {
                let err = SamplingClient::new(SamplerConfig {
                    api_key: Some("XAI_SESSION_SENTINEL".to_string()),
                    base_url: base_url.to_string(),
                    credential_source: Some(source.clone()),
                    ..minimal_config()
                })
                .expect_err("an ambient credential must not construct here");
                let rendered = format!("{err}");
                assert!(
                    !rendered.contains("SENTINEL"),
                    "the error leaked the credential: {rendered}"
                );
            }
        }
    }
    /// #135 Layer 3. The shell's readiness gate refuses to mark this pairing
    /// ready; making it unconstructable means a path that bypasses readiness
    /// cannot reinstate the leak. The loopback case is the load-bearing one:
    /// `normalize_codex_base_url` accepts loopback in test builds for mock
    /// transports, so only the credential-label gate refuses it here. The
    /// error names no secret -- a refusal that prints the credential it
    /// refused is not a refusal.
    #[test]
    fn provider_scoped_codex_credential_cannot_construct_for_a_non_codex_endpoint() {
        use crate::config::CredentialSource;
        for base_url in [
            "https://vendor.example/v1",
            "http://127.0.0.1:9/backend-api/codex",
        ] {
            let err = SamplingClient::new(SamplerConfig {
                api_key: None,
                base_url: base_url.to_string(),
                api_backend: ApiBackend::CodexResponses,
                credential_source: Some(CredentialSource::AuthProvider {
                    name: "openai-codex".to_owned(),
                }),
                ..minimal_config()
            })
            .expect_err("a provider-scoped credential must not construct for a foreign origin");
            let rendered = format!("{err}");
            assert!(
                rendered.contains("provider-scoped"),
                "unexpected refusal for {base_url}: {rendered}"
            );
        }

        // The same provider label at the credential's own origin is the
        // normal Codex flow and must keep constructing.
        SamplingClient::new(SamplerConfig {
            api_key: None,
            base_url: CODEX_BASE_URL.to_string(),
            api_backend: ApiBackend::CodexResponses,
            credential_source: Some(CredentialSource::AuthProvider {
                name: "openai-codex".to_owned(),
            }),
            ..minimal_config()
        })
        .expect("a provider credential at its own origin is the normal flow");
    }

    /// The other side of the same rule: a credential the model declared is
    /// none of this layer's business, and an ambient one is fine where it
    /// belongs.
    #[test]
    fn non_ambient_sources_and_first_party_ambient_still_construct() {
        use crate::config::CredentialSource;
        SamplingClient::new(SamplerConfig {
            api_key: Some("sk-provider".to_string()),
            base_url: "https://api.openai.com/v1".to_string(),
            credential_source: Some(CredentialSource::ModelApiKey),
            ..minimal_config()
        })
        .expect("a model-owned key on an external endpoint is the BYOK case");

        SamplingClient::new(SamplerConfig {
            api_key: Some("session".to_string()),
            base_url: "https://api.x.ai/v1".to_string(),
            credential_source: Some(CredentialSource::XaiSession),
            ..minimal_config()
        })
        .expect("an ambient credential on a first-party origin is the normal flow");
    }
    /// A model-declared key reaches its own provider untouched, and nothing
    /// ambient rides along. Built rather than sent: the built request carries
    /// exactly the headers that would go on the wire, which is what is being
    /// asserted here.
    ///
    /// For the origin the issue reproduces against, ambient absence is proven
    /// twice over and more strongly than a capture could: the shell's choke
    /// point hands this layer a keyless config, and an ambient-source config
    /// cannot construct a client at all. The request cannot exist to be
    /// captured.
    #[test]
    fn declared_provider_key_reaches_the_wire_without_anything_ambient() {
        use crate::config::CredentialSource;
        let client = SamplingClient::new(SamplerConfig {
            api_key: Some("provider-key-sentinel".to_string()),
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            credential_source: Some(CredentialSource::ModelApiKey),
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        })
        .expect("a declared provider key is this layer's normal BYOK case");
        let req = client
            .post("http://127.0.0.1:11434/v1/chat/completions")
            .0
            .build()
            .expect("build request");
        assert_eq!(
            req.headers()[AUTHORIZATION],
            "Bearer provider-key-sentinel",
            "the model's own key must reach its own provider"
        );
        let rendered = format!("{:?}", req.headers());
        for ambient in ["XAI_SESSION_SENTINEL", "XAI_API_KEY_SENTINEL"] {
            assert!(
                !rendered.contains(ambient),
                "an ambient credential rode along: {rendered}"
            );
        }
    }
    /// `auth_scheme = "none"` is the keyless local-server case and an
    /// acceptance criterion of #110: no credential header of any kind, not an
    /// empty one.
    #[test]
    fn auth_scheme_none_sends_no_credential_header_at_all() {
        let client = SamplingClient::new(SamplerConfig {
            api_key: Some("must-not-be-sent".to_string()),
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            auth_scheme: AuthScheme::None,
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        })
        .expect("keyless local servers stay constructible");
        let req = client
            .post("http://127.0.0.1:11434/v1/chat/completions")
            .0
            .build()
            .expect("build request");
        assert!(
            req.headers().get(AUTHORIZATION).is_none(),
            "auth_scheme = none must send no Authorization header"
        );
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-api-key"))
                .is_none(),
            "auth_scheme = none must send no x-api-key header"
        );
    }
    #[test]
    fn third_party_base_url_omits_xai_identity_headers_even_with_bearer() {
        let cfg = SamplerConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            deployment_id: Some("deploy-must-not-leak".to_string()),
            user_id: Some("user-must-not-leak".to_string()),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-grok-deployment-id"))
                .is_none(),
            "third-party base_url must omit x-grok-deployment-id"
        );
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-grok-user-id"))
                .is_none(),
            "third-party base_url must omit x-grok-user-id"
        );
        let req = client
            .post("https://api.openai.com/v1/chat/completions")
            .0
            .build()
            .expect("build request");
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-grok-deployment-id"))
                .is_none()
        );
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-grok-user-id"))
                .is_none()
        );
    }

    #[test]
    fn xai_transport_strips_codex_only_routing_headers() {
        let mut cfg = minimal_config();
        for (name, value) in [
            ("chatgpt-account-id", "must-not-leak"),
            ("x-openai-fedramp", "true"),
            ("originator", "codex_cli_rs"),
        ] {
            cfg.extra_headers.insert(name.to_owned(), value.to_owned());
        }
        let client = SamplingClient::new(cfg).expect("xAI client should build");
        let request = client
            .post("https://api.x.ai/v1/chat/completions")
            .0
            .build()
            .expect("xAI request should build");
        for name in ["chatgpt-account-id", "x-openai-fedramp", "originator"] {
            assert!(
                request.headers().get(name).is_none(),
                "Codex-only routing header leaked to xAI: {name}"
            );
        }
    }

    #[test]
    fn third_party_base_url_strips_identity_headers_from_extra_headers() {
        let mut cfg = SamplerConfig {
            base_url: "https://api.anthropic.com/v1".to_string(),
            ..minimal_config()
        };
        cfg.extra_headers.insert(
            "x-grok-deployment-id".to_string(),
            "deploy-must-not-leak".to_string(),
        );
        cfg.extra_headers.insert(
            "x-grok-user-id".to_string(),
            "user-must-not-leak".to_string(),
        );
        let client = SamplingClient::new(cfg).expect("client should build");
        let req = client
            .post("https://api.anthropic.com/v1/messages")
            .0
            .build()
            .expect("build request");
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-grok-deployment-id"))
                .is_none()
        );
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-grok-user-id"))
                .is_none()
        );
    }

    // Regression: a past change dropped User-Agent from sampling requests.
    #[test]
    fn sampling_client_always_has_user_agent() {
        let client = SamplingClient::new(minimal_config()).expect("build");
        assert!(client.default_headers.contains_key(USER_AGENT));
    }

    #[test]
    fn none_scheme_post_strips_auth_headers_after_hostile_injector() {
        #[derive(Debug)]
        struct HostileInjector;
        impl crate::config::HeaderInjector for HostileInjector {
            fn inject(&self, headers: &mut HeaderMap) {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_static("Bearer hostile-injector"),
                );
                headers.insert(
                    HeaderName::from_static("x-api-key"),
                    HeaderValue::from_static("hostile-injector-key"),
                );
            }
        }

        let mut cfg = SamplerConfig {
            api_key: None,
            auth_scheme: AuthScheme::None,
            ..minimal_config()
        };
        cfg.header_injector = Some(std::sync::Arc::new(HostileInjector));
        let client = SamplingClient::new(cfg).expect("client should build");
        let req = client
            .post("http://localhost/test")
            .0
            .build()
            .expect("build request");
        assert!(
            req.headers().get(AUTHORIZATION).is_none(),
            "AuthScheme::None must strip Authorization after hostile HeaderInjector"
        );
        assert!(
            req.headers()
                .get(HeaderName::from_static("x-api-key"))
                .is_none(),
            "AuthScheme::None must strip x-api-key after hostile HeaderInjector"
        );
    }

    // Regression: a past change dropped HeaderInjector (traceparent) from sampling requests.
    #[test]
    fn header_injector_is_called_in_post() {
        #[derive(Debug)]
        struct TestInjector;
        impl crate::config::HeaderInjector for TestInjector {
            fn inject(&self, headers: &mut HeaderMap) {
                headers.insert(
                    HeaderName::from_static("traceparent"),
                    HeaderValue::from_static("00-test-trace-id-00"),
                );
            }
        }

        let mut config = minimal_config();
        // First-party endpoint: trace context is kept. (External endpoints
        // strip it at the metadata boundary — covered separately below.)
        config.base_url = "https://api.x.ai/v1".to_string();
        config.header_injector = Some(std::sync::Arc::new(TestInjector));
        let client = SamplingClient::new(config).expect("build");
        let (builder, _final_credential) = client.post("https://api.x.ai/v1/test");
        let req = builder.build().expect("build request");
        assert!(
            req.headers().contains_key("traceparent"),
            "HeaderInjector should inject traceparent into first-party post() requests"
        );
    }

    /// Hostile injector for boundary tests: tries to smuggle every class of
    /// first-party metadata onto an outbound request.
    #[derive(Debug)]
    struct MetadataSmuggler;
    impl crate::config::HeaderInjector for MetadataSmuggler {
        fn inject(&self, headers: &mut HeaderMap) {
            for name in [
                "traceparent",
                "tracestate",
                "baggage",
                "x-grok-conv-id",
                "x-grok-client-identifier",
                "x-grok-client-version",
                "x-compactions-remaining",
                "x-compaction-at",
                "x-authenticateresponse",
                "x-xai-token-auth",
            ] {
                headers.insert(
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_static("must-not-leak"),
                );
            }
        }
    }

    fn boundary_config(base_url: &str) -> SamplerConfig {
        let mut config = minimal_config();
        config.base_url = base_url.to_string();
        config.client_identifier = Some("secret-frontend".to_string());
        config.client_version = Some("9.9.9".to_string());
        config.header_injector = Some(std::sync::Arc::new(MetadataSmuggler));
        config
            .extra_headers
            .insert("x-provider-key".to_string(), "configured".to_string());
        config
    }

    const INTERNAL_METADATA: [&str; 11] = [
        "traceparent",
        "tracestate",
        "baggage",
        "x-grok-conv-id",
        "x-grok-client-identifier",
        "x-grok-client-version",
        "x-compactions-remaining",
        "x-compaction-at",
        "x-authenticateresponse",
        "x-xai-token-auth",
        "x-grok-user-id",
    ];

    #[test]
    fn external_endpoint_strips_all_first_party_metadata() {
        let client =
            SamplingClient::new(boundary_config("https://api.openai.com/v1")).expect("build");
        let req = client
            .post("https://api.openai.com/v1/chat/completions")
            .0
            .build()
            .expect("build request");
        for name in INTERNAL_METADATA {
            assert!(
                req.headers().get(name).is_none(),
                "external endpoint must not receive {name}"
            );
        }
        assert_eq!(req.headers()[AUTHORIZATION], "Bearer test-key");
        assert_eq!(
            req.headers()[HeaderName::from_static("x-provider-key")],
            "configured",
            "explicitly configured provider headers must survive the boundary"
        );
        assert_eq!(
            req.headers()[USER_AGENT],
            AGENT_PRODUCT,
            "external User-Agent must be the minimal product string"
        );
    }

    #[test]
    fn local_endpoint_strips_all_first_party_metadata() {
        let mut config = boundary_config("http://127.0.0.1:11434/v1");
        config.auth_scheme = AuthScheme::None;
        config.api_key = None;
        let client = SamplingClient::new(config).expect("build");
        let req = client
            .post("http://127.0.0.1:11434/v1/chat/completions")
            .0
            .build()
            .expect("build request");
        for name in INTERNAL_METADATA {
            assert!(
                req.headers().get(name).is_none(),
                "local endpoint must not receive {name}"
            );
        }
        assert!(req.headers().get(AUTHORIZATION).is_none());
    }

    /// An authenticated loopback server (Ollama/LM Studio with a key) is
    /// Local, not first-party: `is_xai_api_url` accepts loopback for mock
    /// convenience, but trust derivation must not inherit that.
    #[test]
    fn authenticated_loopback_is_local_not_first_party() {
        let mut config = boundary_config("http://127.0.0.1:11434/v1");
        config.auth_scheme = AuthScheme::Bearer;
        config.api_key = Some("local-server-key".to_string());
        let client = SamplingClient::new(config).expect("build");
        assert!(!client.sends_xai_identity_headers());
        let req = client
            .post("http://127.0.0.1:11434/v1/chat/completions")
            .0
            .build()
            .expect("build request");
        for name in INTERNAL_METADATA {
            assert!(
                req.headers().get(name).is_none(),
                "authenticated loopback must not receive {name}"
            );
        }
        assert_eq!(req.headers()[AUTHORIZATION], "Bearer local-server-key");
    }

    #[test]
    fn prompt_cache_key_anonymized_irreversibly_and_stably() {
        let mut body = serde_json::json!({"prompt_cache_key": "0199-session-uuid"});
        anonymize_prompt_cache_key(&mut body);
        let first = body["prompt_cache_key"].as_str().unwrap().to_owned();
        assert_ne!(first, "0199-session-uuid");
        assert!(!first.contains("0199"), "digest must not embed the raw id");
        assert_eq!(first.len(), 64, "sha-256 hex digest");

        let mut again = serde_json::json!({"prompt_cache_key": "0199-session-uuid"});
        anonymize_prompt_cache_key(&mut again);
        assert_eq!(
            again["prompt_cache_key"].as_str().unwrap(),
            first,
            "same session must keep a stable cache key"
        );

        let mut empty = serde_json::json!({"prompt_cache_key": ""});
        anonymize_prompt_cache_key(&mut empty);
        assert_eq!(empty["prompt_cache_key"].as_str().unwrap(), "");
        let mut absent = serde_json::json!({"model": "m"});
        anonymize_prompt_cache_key(&mut absent);
        assert!(absent.get("prompt_cache_key").is_none());
    }

    #[test]
    fn xai_endpoint_keeps_first_party_metadata() {
        let client = SamplingClient::new(boundary_config("https://api.x.ai/v1")).expect("build");
        let req = client
            .post("https://api.x.ai/v1/chat/completions")
            .0
            .build()
            .expect("build request");
        assert_eq!(req.headers()["traceparent"], "must-not-leak");
        // Values may be overwritten by the (trusted, first-party) injector —
        // what matters is that first-party metadata is present at all.
        assert!(
            req.headers()
                .contains_key(HeaderName::from_static("x-grok-client-identifier"))
        );
        assert!(
            req.headers()
                .contains_key(HeaderName::from_static("x-grok-client-version"))
        );
    }

    #[test]
    fn internal_namespace_denied_even_when_explicitly_configured() {
        let mut config = boundary_config("https://api.openai.com/v1");
        // The shell folds per-turn compaction metadata into extra_headers;
        // explicit configuration must not reopen the boundary for internal
        // namespaces.
        config
            .extra_headers
            .insert("x-compactions-remaining".to_string(), "3".to_string());
        config
            .extra_headers
            .insert("x-grok-session-id".to_string(), "sess-1".to_string());
        let client = SamplingClient::new(config).expect("build");
        let req = client
            .post("https://api.openai.com/v1/chat/completions")
            .0
            .build()
            .expect("build request");
        assert!(
            req.headers().get("x-compactions-remaining").is_none(),
            "internal namespace must be denied even when explicitly configured"
        );
        assert!(req.headers().get("x-grok-session-id").is_none());
        assert_eq!(
            req.headers()[HeaderName::from_static("x-provider-key")],
            "configured"
        );
    }

    #[test]
    fn explicit_endpoint_trust_override_wins_over_derivation() {
        let mut config = boundary_config("https://internal-relay.example");
        config.endpoint_trust = Some(crate::config::EndpointTrustClass::FirstPartyXai);
        let client = SamplingClient::new(config).expect("build");
        let req = client
            .post("https://internal-relay.example/chat/completions")
            .0
            .build()
            .expect("build request");
        assert_eq!(
            req.headers()["traceparent"],
            "must-not-leak",
            "an explicit FirstPartyXai override must keep first-party metadata"
        );
    }

    #[test]
    fn grok_request_headers_skip_non_first_party() {
        let client =
            SamplingClient::new(boundary_config("https://api.openai.com/v1")).expect("build");
        let headers = GrokRequestHeaders {
            conv_id: "conv",
            req_id: "req",
            model_id: "model",
            session_id: "sess",
            turn_idx: Some("0"),
            agent_id: "agent",
            deployment_id: Some("deploy"),
            user_id: Some("user"),
        };
        let req = headers
            .apply(
                client.post("https://api.openai.com/v1/chat/completions").0,
                client.sends_xai_identity_headers(),
            )
            .build()
            .expect("build request");
        for name in [
            "x-grok-conv-id",
            "x-grok-req-id",
            "x-grok-model-override",
            "x-grok-session-id",
            "x-grok-agent-id",
            "x-grok-turn-idx",
            "x-grok-deployment-id",
            "x-grok-user-id",
        ] {
            assert!(
                req.headers().get(name).is_none(),
                "builder-level metadata {name} must be gated off for external endpoints"
            );
        }
    }

    #[derive(Debug)]
    struct CodexCredentialResolver {
        reads: std::sync::atomic::AtomicUsize,
    }

    impl crate::config::BearerResolver for CodexCredentialResolver {
        fn current_bearer(&self) -> Option<String> {
            panic!("Codex requests must use the structured snapshot seam")
        }

        fn current_credential(&self) -> Option<crate::config::ProviderCredentialSnapshot> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(crate::config::ProviderCredentialSnapshot {
                access_token: "live-codex-token".to_owned(),
                account_id: Some("workspace-123".to_owned()),
                chatgpt_account_is_fedramp: true,
            })
        }
    }

    #[derive(Debug)]
    struct HostileCodexInjector;

    impl crate::config::HeaderInjector for HostileCodexInjector {
        fn inject(&self, headers: &mut HeaderMap) {
            headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer attacker"));
            headers.insert(
                HeaderName::from_static("chatgpt-account-id"),
                HeaderValue::from_static("wrong-account"),
            );
            headers.insert(OPENAI_FEDRAMP, HeaderValue::from_static("false"));
            headers.insert(ORIGINATOR, HeaderValue::from_static("codex_cli_rs"));
            headers.insert(USER_AGENT, HeaderValue::from_static("codex_cli_rs/0.0.0"));
            for name in [
                "x-api-key",
                "x-xai-token-auth",
                "x-grok-conv-id",
                "x-grok-client-version",
                "x-grok-doom-loop-check",
                "x-compactions-remaining",
                "proxy-authorization",
                "traceparent",
            ] {
                headers.insert(
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_static("must-not-leak"),
                );
            }
        }
    }

    #[test]
    fn codex_transport_normalizes_endpoint_and_isolates_live_headers() {
        let resolver = std::sync::Arc::new(CodexCredentialResolver {
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let cfg = SamplerConfig {
            api_key: Some("stale-xai-token".to_owned()),
            base_url: "https://chatgpt.com/backend-api/codex/".to_owned(),
            api_backend: ApiBackend::CodexResponses,
            origin_client: Some(OriginClientInfo {
                product: "codex_cli_rs".to_owned(),
                version: Some("999.0.0".to_owned()),
            }),
            bearer_resolver: Some(resolver.clone()),
            header_injector: Some(std::sync::Arc::new(HostileCodexInjector)),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("Codex client should build");

        assert_eq!(
            client.endpoint("responses"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        let request = client
            .post(client.endpoint("responses"))
            .0
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
            .build()
            .expect("request should build");

        assert_eq!(resolver.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(request.headers()[AUTHORIZATION], "Bearer live-codex-token");
        assert_eq!(request.headers()[CHATGPT_ACCOUNT_ID], "workspace-123");
        assert_eq!(request.headers()[OPENAI_FEDRAMP], "true");
        assert_eq!(request.headers()[ORIGINATOR], "grok_build");
        assert_eq!(
            request.headers()[USER_AGENT],
            grok_build_user_agent_string(),
            "neither origin_client nor hostile headers may spoof the Codex user agent"
        );
        for name in [
            "x-api-key",
            "x-xai-token-auth",
            "x-grok-conv-id",
            "x-grok-client-version",
            "x-grok-doom-loop-check",
            "x-compactions-remaining",
            "proxy-authorization",
            "traceparent",
        ] {
            assert!(
                request.headers().get(name).is_none(),
                "leaked header {name}"
            );
        }
        for name in request.headers().keys() {
            assert!(
                matches!(
                    name.as_str(),
                    "authorization"
                        | "chatgpt-account-id"
                        | "content-type"
                        | "accept"
                        | "originator"
                        | "user-agent"
                        | "x-openai-fedramp"
                ),
                "unexpected Codex header {name}"
            );
        }
    }

    #[test]
    fn codex_defaults_enable_parallel_tools_without_overriding_explicit_choice() {
        let resolver = std::sync::Arc::new(CodexCredentialResolver {
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let client = SamplingClient::new(SamplerConfig {
            base_url: CODEX_BASE_URL.to_owned(),
            api_backend: ApiBackend::CodexResponses,
            bearer_resolver: Some(resolver),
            temperature: Some(0.73),
            top_p: Some(0.91),
            ..minimal_config()
        })
        .expect("Codex client should build");

        let mut request = CreateResponseWrapper::default();
        request.inner.temperature = Some(0.42);
        request.inner.top_p = Some(0.57);
        client.apply_response_defaults(&mut request).unwrap();
        assert_eq!(request.inner.parallel_tool_calls, Some(true));
        assert_eq!(request.inner.temperature, None);
        assert_eq!(request.inner.top_p, None);

        request.inner.parallel_tool_calls = Some(false);
        client.apply_response_defaults(&mut request).unwrap();
        assert_eq!(request.inner.parallel_tool_calls, Some(false));
    }

    #[test]
    fn generic_responses_preserve_sampling_parameters() {
        let client = SamplingClient::new(SamplerConfig {
            base_url: "https://api.openai.com/v1".to_owned(),
            api_backend: ApiBackend::Responses,
            temperature: Some(0.73),
            top_p: Some(0.91),
            ..minimal_config()
        })
        .expect("generic Responses client should build");

        let mut request = CreateResponseWrapper::default();
        client.apply_response_defaults(&mut request).unwrap();
        assert_eq!(request.inner.temperature, Some(0.73));
        assert_eq!(request.inner.top_p, Some(0.91));

        request.inner.temperature = Some(0.42);
        request.inner.top_p = Some(0.57);
        client.apply_response_defaults(&mut request).unwrap();
        assert_eq!(request.inner.temperature, Some(0.42));
        assert_eq!(request.inner.top_p, Some(0.57));
    }

    #[test]
    fn codex_transport_rejects_non_allowlisted_origins_and_query_params() {
        for base_url in [
            "https://api.openai.com/v1",
            "https://chatgpt.com.evil.test/backend-api/codex",
            "http://chatgpt.com/backend-api/codex",
            "https://chatgpt.com/backend-api/codex?token=secret",
        ] {
            let result = SamplingClient::new(SamplerConfig {
                base_url: base_url.to_owned(),
                api_backend: ApiBackend::CodexResponses,
                ..minimal_config()
            });
            assert!(result.is_err(), "accepted unsafe Codex origin {base_url}");
        }

        let mut with_query = SamplerConfig {
            base_url: CODEX_BASE_URL.to_owned(),
            api_backend: ApiBackend::CodexResponses,
            ..minimal_config()
        };
        with_query.query_params.insert("key".into(), "value".into());
        assert!(SamplingClient::new(with_query).is_err());
    }

    #[test]
    fn platform_responses_transport_is_not_rerouted_to_codex() {
        let mut config = SamplerConfig {
            base_url: "https://api.openai.com/v1".to_owned(),
            api_backend: ApiBackend::Responses,
            ..minimal_config()
        };
        for (name, value) in [
            ("chatgpt-account-id", "attacker-account"),
            ("x-openai-fedramp", "true"),
            ("originator", "codex_cli_rs"),
        ] {
            config
                .extra_headers
                .insert(name.to_owned(), value.to_owned());
        }
        config.header_injector = Some(std::sync::Arc::new(HostileCodexInjector));
        let client = SamplingClient::new(config).expect("Platform Responses client should build");
        assert_eq!(
            client.endpoint("responses"),
            "https://api.openai.com/v1/responses"
        );
        let request = client
            .post(client.endpoint("responses"))
            .0
            .build()
            .expect("Platform Responses request should build");
        for name in ["chatgpt-account-id", "x-openai-fedramp", "originator"] {
            assert!(
                request.headers().get(name).is_none(),
                "Codex-only routing header leaked to Platform Responses: {name}"
            );
        }
    }

    #[test]
    fn codex_transport_omits_fedramp_header_for_untrusted_snapshot() {
        let mut headers = HeaderMap::new();
        headers.insert(OPENAI_FEDRAMP, HeaderValue::from_static("true"));
        retain_codex_headers(
            &mut headers,
            None,
            Some("workspace-123"),
            false,
            Some(HeaderValue::from_static("grok-shell/test")),
        );
        assert!(headers.get(OPENAI_FEDRAMP).is_none());
        assert_eq!(headers[ORIGINATOR], "grok_build");
        assert_eq!(headers[USER_AGENT], "grok-shell/test");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn codex_mock_request_has_exact_path_and_no_xai_extensions() {
        use axum::Router;
        use axum::body::Bytes;
        use axum::http::Uri;
        use axum::routing::post;
        use tokio::net::TcpListener;

        let captured: Arc<Mutex<Option<(Uri, HeaderMap, Bytes)>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&captured);
        let app = Router::new().route(
            "/backend-api/codex/responses",
            post(move |uri: Uri, headers: HeaderMap, body: Bytes| {
                let sink = Arc::clone(&sink);
                async move {
                    *sink.lock().unwrap() = Some((uri, headers, body));
                    ([(CONTENT_TYPE, "text/event-stream")], "data: [DONE]\n\n")
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let resolver = Arc::new(CodexCredentialResolver {
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let client = SamplingClient::new(SamplerConfig {
            base_url: format!("http://{address}/backend-api/codex"),
            api_backend: ApiBackend::CodexResponses,
            bearer_resolver: Some(resolver),
            temperature: Some(0.73),
            top_p: Some(0.91),
            stream_tool_calls: true,
            doom_loop_recovery: Some(xai_grok_sampling_types::DoomLoopRecoveryPolicy {
                max_threshold: 8,
                max_retries: 2,
            }),
            ..minimal_config()
        })
        .expect("loopback Codex mock should be allowed in unit tests");
        let mut request = CreateResponseWrapper::default();
        request.inner.input = rs::InputParam::Items(vec![
            rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::System,
                content: rs::EasyInputContent::ContentList(vec![rs::InputContent::InputText(
                    rs::InputTextContent {
                        text: "system guidance".to_owned(),
                    },
                )]),
            }),
            rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::User,
                content: rs::EasyInputContent::Text("hello".to_owned()),
            }),
        ]);
        request.x_grok_conv_id = Some("must-not-leak".to_owned());
        request.x_grok_req_id = Some("must-not-leak".to_owned());
        request.extra_tool_entries = vec![serde_json::json!({"type": "x_search"})];

        let (_, _, doom_loop) = client
            .create_response_stream(request)
            .await
            .expect("mock Codex request should start");
        assert!(
            doom_loop.is_none(),
            "Codex must not enable xAI doom-loop mode"
        );

        let (uri, headers, body) = captured.lock().unwrap().take().expect("request captured");
        assert_eq!(uri.path(), "/backend-api/codex/responses");
        assert_eq!(headers[AUTHORIZATION], "Bearer live-codex-token");
        assert_eq!(headers[CHATGPT_ACCOUNT_ID], "workspace-123");
        assert!(headers.get("x-grok-conv-id").is_none());
        assert!(headers.get(DOOM_LOOP_CHECK_HEADER).is_none());
        let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(body["instructions"], "system guidance");
        assert!(
            body["input"]
                .as_array()
                .is_some_and(|items| items.iter().all(|item| item["role"] != "system"))
        );
        assert!(
            body["input"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["role"] == "user"))
        );
        assert_eq!(body["parallel_tool_calls"], true);
        assert!(
            body.get("temperature").is_none(),
            "Codex request must omit unsupported temperature: {body}"
        );
        assert!(
            body.get("top_p").is_none(),
            "Codex request must omit unsupported top_p: {body}"
        );
        assert!(body.get("stream_tool_calls").is_none());
        assert!(
            body.get("tools")
                .and_then(|tools| tools.as_array())
                .is_none_or(|tools| tools.iter().all(|tool| tool["type"] != "x_search"))
        );
    }

    /// `forwards_prompt_cache_key()` claims Codex sends the key. This is the
    /// only place that claim is observable end to end: the sampling-types
    /// invariant test can prove the shared `CreateResponse` conversion copies
    /// the field, but not that Codex *reaches* that conversion -- the dispatch
    /// lives here, in another crate. Without this, a change giving Codex its
    /// own mapping would leave that test green while the key stopped going out.
    ///
    /// The value on the wire is a digest, not the original: Codex resolves to
    /// `EndpointTrustClass::External`, so `anonymize_prompt_cache_key` always
    /// applies on this path. The field reaching the wire is what the predicate
    /// promises, and the digest is an unsalted sha256, so cache affinity holds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn codex_sends_the_prompt_cache_key_the_predicate_promises() {
        use axum::Router;
        use axum::body::Bytes;
        use axum::routing::post;
        use tokio::net::TcpListener;

        assert!(
            ApiBackend::CodexResponses.forwards_prompt_cache_key(),
            "precondition: the predicate under test must claim Codex forwards it"
        );

        let captured: Arc<Mutex<Option<Bytes>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&captured);
        let app = Router::new().route(
            "/backend-api/codex/responses",
            post(move |body: Bytes| {
                let sink = Arc::clone(&sink);
                async move {
                    *sink.lock().unwrap() = Some(body);
                    ([(CONTENT_TYPE, "text/event-stream")], "data: [DONE]\n\n")
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let resolver = Arc::new(CodexCredentialResolver {
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let client = SamplingClient::new(SamplerConfig {
            base_url: format!("http://{address}/backend-api/codex"),
            api_backend: ApiBackend::CodexResponses,
            bearer_resolver: Some(resolver),
            ..minimal_config()
        })
        .expect("loopback Codex mock should be allowed in unit tests");

        let mut request = CreateResponseWrapper::default();
        request.inner.input =
            rs::InputParam::Items(vec![rs::InputItem::EasyMessage(rs::EasyInputMessage {
                r#type: rs::MessageType::Message,
                role: rs::Role::User,
                content: rs::EasyInputContent::Text("hello".to_owned()),
            })]);
        request.inner.prompt_cache_key = Some("session-abc".to_owned());

        client
            .create_response_stream(request)
            .await
            .expect("mock Codex request should start");

        let body = captured.lock().unwrap().take().expect("request captured");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
        let on_wire = body
            .get("prompt_cache_key")
            .and_then(serde_json::Value::as_str)
            .expect("Codex must send prompt_cache_key; the predicate says it does");
        assert_ne!(
            on_wire, "session-abc",
            "the raw key must not reach a non-first-party origin"
        );
        assert!(
            !on_wire.is_empty(),
            "an anonymised key is still a key: an empty value would be a dropped one"
        );
    }

    /// Regression for stale chat-state A / live provider B. The resolver puts
    /// B on the actual request; if B is rejected, the auth error must report
    /// `same_as_current` so the shell forces ServerRejected refresh instead of
    /// consulting stale A and merely re-adopting the already-rejected B.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn codex_401_reports_actual_live_request_credential_relation_without_secrets() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use tokio::net::TcpListener;

        const STALE_CHAT_A: &str = "stale-chat-state-token-a";
        const LIVE_PROVIDER_B: &str = "live-provider-token-b";

        #[derive(Debug)]
        struct RejectedCodexCredentialResolver;

        impl crate::config::BearerResolver for RejectedCodexCredentialResolver {
            fn current_bearer(&self) -> Option<String> {
                Some(LIVE_PROVIDER_B.to_owned())
            }

            fn current_credential(&self) -> Option<crate::config::ProviderCredentialSnapshot> {
                Some(crate::config::ProviderCredentialSnapshot {
                    access_token: LIVE_PROVIDER_B.to_owned(),
                    account_id: Some("workspace-123".to_owned()),
                    chatgpt_account_is_fedramp: false,
                })
            }
        }

        let captured_authorization = Arc::new(Mutex::new(None::<String>));
        let captured = Arc::clone(&captured_authorization);
        let app = Router::new().route(
            "/backend-api/codex/responses",
            post(move |headers: HeaderMap| {
                let captured = Arc::clone(&captured);
                async move {
                    *captured.lock().unwrap() = headers
                        .get(AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    (StatusCode::UNAUTHORIZED, "credential rejected")
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = SamplingClient::new(SamplerConfig {
            api_key: Some(STALE_CHAT_A.to_owned()),
            base_url: format!("http://{address}/backend-api/codex"),
            api_backend: ApiBackend::CodexResponses,
            bearer_resolver: Some(Arc::new(RejectedCodexCredentialResolver)),
            ..minimal_config()
        })
        .expect("loopback Codex mock should be allowed in unit tests");

        let err = match client
            .create_response_stream(CreateResponseWrapper::default())
            .await
        {
            Err(err) => err,
            Ok(_) => panic!("mock server's 401 must surface as an auth error"),
        };
        assert_eq!(
            captured_authorization.lock().unwrap().as_deref(),
            Some("Bearer live-provider-token-b"),
            "the rejected request used live provider B, not stale chat-state A",
        );
        assert!(matches!(
            &err,
            SamplingError::Auth {
                credential: SentCredential::SameAsCurrent,
                ..
            }
        ));

        let info = crate::events::SamplingErrorInfo::from(&err);
        let metadata = serde_json::to_string(&info).expect("auth metadata serializes");
        assert!(metadata.contains("same_as_current"));
        assert_no_secret_fragments(&metadata, STALE_CHAT_A);
        assert_no_secret_fragments(&metadata, LIVE_PROVIDER_B);
    }

    #[test]
    fn user_agent_includes_origin_and_agent_product() {
        let origin = OriginClientInfo {
            product: "my-client".to_string(),
            version: Some("1.2.3".to_string()),
        };
        let ua = user_agent_string_for(&origin);
        assert!(ua.contains("my-client/1.2.3"));
        assert!(ua.contains(AGENT_PRODUCT));
    }

    #[test]
    fn user_agent_omits_origin_version_when_absent() {
        let origin = OriginClientInfo {
            product: "my-client".to_string(),
            version: None,
        };
        let ua = user_agent_string_for(&origin);
        // No slash between product and the grok-shell agent product.
        assert!(ua.starts_with("my-client grok-shell/"));
    }

    #[test]
    fn user_agent_collapses_when_origin_matches_agent() {
        let agent_version = xai_grok_version::VERSION.to_string();
        let origin = OriginClientInfo {
            product: AGENT_PRODUCT.to_string(),
            version: Some(agent_version.clone()),
        };
        let ua = user_agent_string_for(&origin);
        // Single product/version slot when the origin and agent match.
        assert!(ua.starts_with(&format!("{}/{}", AGENT_PRODUCT, agent_version)));
    }

    /// Counts callbacks for assertions in the tests below.
    #[derive(Default, Debug)]
    struct CountingCallback {
        invocations: std::sync::Mutex<
            Vec<(
                crate::attribution::SamplingConsumer,
                xai_grok_auth::CredentialComparison,
            )>,
        >,
    }

    #[derive(Debug)]
    struct StaticBearerResolver(&'static str);

    impl crate::config::BearerResolver for StaticBearerResolver {
        fn current_bearer(&self) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    impl crate::attribution::Auth401AttributionCallback for CountingCallback {
        fn record_401(
            &self,
            consumer: crate::attribution::SamplingConsumer,
            comparison: xai_grok_auth::CredentialComparison,
        ) {
            self.invocations
                .lock()
                .unwrap()
                .push((consumer, comparison));
        }
    }

    /// The final Authorization credential is projected only at response time.
    #[test]
    fn post_compares_bearer_for_openai_compat() {
        let cfg = SamplerConfig {
            api_key: Some("test-bearer-1234567890".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (_builder, final_credential) = client.post("https://example.test/v1/chat/completions");
        assert_eq!(
            client.compare_final_request_credential(&final_credential),
            CredentialComparison::same_as_current()
        );
    }

    /// `post()` captures `x-api-key` for Messages-API backends without
    /// exposing it outside the request-local wrapper.
    #[test]
    fn post_compares_x_api_key_for_messages() {
        let cfg = SamplerConfig {
            api_key: Some("anthropic-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (_builder, final_credential) = client.post("https://example.test/v1/messages");
        assert_eq!(
            client.compare_final_request_credential(&final_credential),
            CredentialComparison::same_as_current()
        );
    }

    /// `post()` captures `None` when the request carries no auth header.
    #[test]
    fn post_captures_none_when_no_header() {
        let cfg = SamplerConfig {
            api_key: None,
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (_builder, final_credential) = client.post("https://example.test/v1/chat/completions");
        assert_eq!(
            client.compare_final_request_credential(&final_credential),
            CredentialComparison::not_sent(false)
        );
    }

    /// A rotation between request construction and the 401 callback must
    /// classify the rejected final credential as different from current.
    #[test]
    fn post_capture_is_immune_to_resolver_rotation_after_build() {
        #[derive(Debug)]
        struct RotatingResolver(std::sync::Mutex<String>);
        impl crate::config::BearerResolver for RotatingResolver {
            fn current_bearer(&self) -> Option<String> {
                Some(self.0.lock().unwrap().clone())
            }
        }

        let resolver = std::sync::Arc::new(RotatingResolver(std::sync::Mutex::new(
            "rejected-token-oldtail1".to_string(),
        )));
        let cfg = SamplerConfig {
            api_key: None,
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(resolver.clone()),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");

        let (_builder, final_credential) = client.post("https://example.test/v1/responses");
        // The 401 kicks recovery; the resolver rotates before the callback runs.
        *resolver.0.lock().unwrap() = "fresh-token-newtail99".to_string();

        assert_eq!(
            client.compare_final_request_credential(&final_credential),
            CredentialComparison::different_from_current()
        );
    }

    #[test]
    fn live_bearer_resolver_uses_authorization_for_messages_plus_bearer() {
        let cfg = SamplerConfig {
            api_key: Some("stale-bearer".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::Bearer,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-bearer"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (builder, _final_credential) = client.post("https://example.test/v1/messages");
        let request = builder.build().expect("request should build");
        let auth = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        assert_eq!(auth, Some("Bearer fresh-bearer"));
        assert!(request.headers().get("x-api-key").is_none());
    }

    /// Regression: when `api_key` (which seeds `default_headers` with an
    /// `Authorization: Bearer ...`) AND a `bearer_resolver` are both set,
    /// `post()` must produce **exactly one** `Authorization` header on the
    /// wire. The pre-fix code used `RequestBuilder::header(AUTHORIZATION, ...)`
    /// which appends rather than replaces, causing two identical
    /// `Authorization` headers and a 400 from cli-chat-proxy.
    #[test]
    fn post_emits_single_authorization_with_api_key_and_bearer_resolver() {
        let cfg = SamplerConfig {
            api_key: Some("stale-bearer".to_string()),
            api_backend: ApiBackend::Responses,
            auth_scheme: AuthScheme::Bearer,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-bearer"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (builder, _final_credential) = client.post("https://example.test/v1/responses");
        let request = builder.build().expect("request should build");
        let auth_count = request.headers().get_all(AUTHORIZATION).iter().count();
        assert_eq!(
            auth_count, 1,
            "expected exactly one Authorization header, got {auth_count}"
        );
        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer fresh-bearer"),
        );
    }

    #[test]
    fn live_bearer_resolver_uses_x_api_key_for_messages_plus_anthropic_api_key() {
        let cfg = SamplerConfig {
            api_key: Some("stale-anthropic".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-anthropic"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (builder, _final_credential) = client.post("https://example.test/v1/messages");
        let request = builder.build().expect("request should build");
        let api_key = request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok());
        assert_eq!(api_key, Some("fresh-anthropic"));
        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    /// The callback receives only the secret-free response-time comparison.
    #[test]
    fn record_401_attribution_invokes_callback_with_captured_bearer() {
        let cb = std::sync::Arc::new(CountingCallback::default());
        let cb_dyn: crate::attribution::SharedAttributionCallback = cb.clone();
        let cfg = SamplerConfig {
            api_key: Some("the-bearer-1234567890-extra-tail".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            attribution_callback: Some(cb_dyn),
            bearer_resolver: None,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (_builder, final_credential) = client.post("https://example.test/v1/chat/completions");
        client.record_401_attribution(
            crate::attribution::SamplingConsumer::ChatCompletionsStream,
            &final_credential,
        );
        let calls = cb.invocations.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            crate::attribution::SamplingConsumer::ChatCompletionsStream
        );
        assert_eq!(calls[0].1, CredentialComparison::same_as_current());
    }

    /// When a bearer_resolver is wired but returns `None`, attribution must
    /// report no sent bearer (not the construction-time default header seed).
    #[test]
    fn bearer_resolver_none_attribution_ignores_default_headers() {
        #[derive(Debug)]
        struct EmptyResolver;
        impl crate::config::BearerResolver for EmptyResolver {
            fn current_bearer(&self) -> Option<String> {
                None
            }
        }

        let cfg = SamplerConfig {
            api_key: Some("stale-seed-token".to_string()),
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(std::sync::Arc::new(EmptyResolver)),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (_, final_credential) = client.post("https://example.test/v1/responses");
        assert_eq!(
            client.compare_final_request_credential(&final_credential),
            CredentialComparison::not_sent(false)
        );
    }

    /// When a bearer_resolver is wired but returns `None` (hard-expired
    /// session with no live AT), default Authorization / x-api-key must be
    /// stripped so a stale seed key cannot ride the wire.
    #[test]
    fn bearer_resolver_none_strips_default_authorization() {
        #[derive(Debug)]
        struct EmptyResolver;
        impl crate::config::BearerResolver for EmptyResolver {
            fn current_bearer(&self) -> Option<String> {
                None
            }
        }

        let cfg = SamplerConfig {
            api_key: Some("stale-token".to_string()),
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(std::sync::Arc::new(EmptyResolver)),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let (builder, final_credential) = client.post("https://example.test/v1/responses");
        let request = builder.body("").build().expect("request should build");
        assert_eq!(
            client.compare_final_request_credential(&final_credential),
            CredentialComparison::not_sent(false)
        );
        assert!(
            request.headers().get(AUTHORIZATION).is_none(),
            "stale default Authorization must not be sent when resolver is empty"
        );
    }

    /// Regression test: when a bearer_resolver is wired, `post()` must
    /// *replace* the Authorization header from `default_headers`, not
    /// append a second one. Duplicate Authorization headers cause
    /// Cloudflare to return 400 Bad Request.
    #[test]
    fn bearer_resolver_replaces_authorization_header() {
        #[derive(Debug)]
        struct StaticResolver(String);
        impl crate::config::BearerResolver for StaticResolver {
            fn current_bearer(&self) -> Option<String> {
                Some(self.0.clone())
            }
        }

        let resolver: crate::config::SharedBearerResolver =
            std::sync::Arc::new(StaticResolver("fresh-token".to_string()));
        let cfg = SamplerConfig {
            api_key: Some("stale-token".to_string()),
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(resolver),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");

        // Build a request to inspect the final headers.
        let (builder, _final_credential) = client.post("https://example.test/v1/responses");
        let request = builder.body("").build().expect("request should build");

        let auth_values: Vec<_> = request.headers().get_all(AUTHORIZATION).iter().collect();
        assert_eq!(
            auth_values.len(),
            1,
            "expected exactly one Authorization header, got {}: {:?}",
            auth_values.len(),
            auth_values
        );
        assert_eq!(
            auth_values[0].to_str().unwrap(),
            "Bearer fresh-token",
            "Authorization header should contain the resolver's fresh token"
        );
    }

    /// `record_401_attribution` is a no-op when `attribution_callback`
    /// is `None` (the BYOK / sampler-only path). The previous tests
    /// in this module construct clients without a callback and rely
    /// on this property holding.
    #[test]
    fn record_401_attribution_is_noop_without_callback() {
        let cfg = SamplerConfig {
            api_key: Some("bearer".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            attribution_callback: None,
            bearer_resolver: None,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        // Must not panic.
        client.record_401_attribution(
            crate::attribution::SamplingConsumer::ChatCompletions,
            &FinalRequestCredential(Some("bearer".to_string())),
        );
    }

    /// `response.completed` carrying
    /// `usage.context_details.{input_tokens, output_tokens}` rewrites
    /// `usage.total_tokens` in place to the live context length
    /// (`ctx.input + ctx.output`). Billing fields stay on the wire's
    /// cumulative values.
    #[test]
    fn deserialize_response_event_overrides_total_tokens_from_context_details() {
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "grok-build",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 6003,
                    "input_tokens_details": { "cached_tokens": 1984 },
                    "output_tokens": 711,
                    "output_tokens_details": { "reasoning_tokens": 388 },
                    "total_tokens": 6714,
                    "context_details": {
                        "input_tokens": 5022,
                        "output_tokens": 571
                    }
                }
            }
        }"#;
        let event = deserialize_response_event(sse).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        // Billing fields stay cumulative — unchanged by context_details.
        assert_eq!(usage.input_tokens, 6003);
        assert_eq!(usage.output_tokens, 711);
        assert_eq!(usage.input_tokens_details.cached_tokens, 1984);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 388);
        // total_tokens rewritten to ctx.input + ctx.output (5022 + 571).
        // NOT the wire's cumulative total (6714).
        assert_eq!(usage.total_tokens, 5_593);
    }

    #[test]
    fn deserialize_response_event_stashes_cost_in_metadata() {
        let make = |ticks: i64| {
            format!(
                r#"{{
                "type": "response.completed",
                "sequence_number": 0,
                "response": {{
                    "id": "resp_1", "object": "response", "created_at": 0,
                    "model": "grok-build", "status": "completed", "output": [],
                    "usage": {{
                        "input_tokens": 10,
                        "input_tokens_details": {{ "cached_tokens": 0 }},
                        "output_tokens": 5,
                        "output_tokens_details": {{ "reasoning_tokens": 0 }},
                        "total_tokens": 15,
                        "cost_in_usd_ticks": {ticks}
                    }}
                }}
            }}"#
            )
        };

        let event = deserialize_response_event(&make(78)).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        assert_eq!(
            e.response
                .metadata
                .as_ref()
                .and_then(|m| m.get(COST_USD_TICKS_METADATA_KEY))
                .map(String::as_str),
            Some("78")
        );

        // The REST mapper backfills 0 for unbilled requests: no stash.
        let event = deserialize_response_event(&make(0)).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        assert!(e.response.metadata.is_none());
    }

    #[test]
    fn deserialize_response_event_total_tokens_unchanged_when_context_details_absent() {
        // Older / non-Responses backends omit `context_details`.
        // `total_tokens` passes through from the wire unchanged.
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "grok-build",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 10000,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens": 100,
                    "output_tokens_details": { "reasoning_tokens": 0 },
                    "total_tokens": 10100
                }
            }
        }"#;
        let event = deserialize_response_event(sse).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        assert_eq!(usage.total_tokens, 10_100);
    }

    #[test]
    fn deserialize_response_event_total_tokens_unchanged_when_context_details_partial() {
        // Defensive: if the backend ever ships only one of the two
        // context_details fields, we don't have a complete picture of
        // the live context size, so leave `total_tokens` on the wire's
        // cumulative value instead of guessing (treating the missing
        // half as 0 would silently under-report).
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "grok-build",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 6003,
                    "input_tokens_details": { "cached_tokens": 1984 },
                    "output_tokens": 711,
                    "output_tokens_details": { "reasoning_tokens": 388 },
                    "total_tokens": 6714,
                    "context_details": {
                        "input_tokens": 5022
                    }
                }
            }
        }"#;
        let event = deserialize_response_event(sse).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        assert_eq!(usage.total_tokens, 6_714);
    }

    #[test]
    fn deserialize_response_event_ignores_context_details_on_non_terminal_events() {
        // Non-terminal events don't carry final usage; even if the backend ever
        // echoed `context_details` on one, we don't touch it.
        let sse = r#"{
            "type": "response.output_text.delta",
            "sequence_number": 0,
            "item_id": "item-1",
            "output_index": 0,
            "content_index": 0,
            "delta": "hello",
            "logprobs": []
        }"#;
        let event = deserialize_response_event(sse).expect("non-terminal event parses");
        assert!(matches!(
            event,
            rs::ResponseStreamEvent::ResponseOutputTextDelta(_)
        ));
    }
}
