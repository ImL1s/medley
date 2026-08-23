//! Sampling error types.
//!
//! TODO: Move from xai-grok-shell/src/sampling/error.rs

use std::fmt;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use xai_circuit_breaker::RetryPolicy;
pub type Result<T> = std::result::Result<T, SamplingError>;

/// Why the model's response was classified as "empty" by [`ConversationResponse::empty_reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmptyReason {
    /// The model emitted reasoning tokens but produced no visible content
    /// and no tool calls. The stream completed normally (has `finish_reason`).
    ReasoningOnly,
    /// The stream carried at least one `choice` but the final assistant
    /// message has empty `content` and no tool calls (and no reasoning).
    NoVisibleContent,
}

impl EmptyReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EmptyReason::ReasoningOnly => "reasoning_only",
            EmptyReason::NoVisibleContent => "no_visible_content",
        }
    }
}

impl fmt::Display for EmptyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured context captured at L2 stream completion time when the
/// response is classified as empty. Carries everything needed to
/// root-cause the issue from a single log line or error payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmptyResponseContext {
    pub reason: EmptyReason,
    /// Whether the response contained reasoning tokens.
    pub had_reasoning: bool,
    /// Byte length of the accumulated `content` string (0 for truly empty).
    pub content_len: usize,
    /// Number of tool calls in the final response.
    pub tool_call_count: usize,
    /// The `finish_reason` from the stream, if any.
    pub finish_reason: Option<String>,
    /// Token usage from the response (when available).
    pub completion_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    pub prompt_tokens: Option<u32>,
    /// Model that produced the response.
    pub model: String,
    /// Whether at least one `choice` was seen in the stream.
    pub first_choice_seen: bool,
}

impl EmptyResponseContext {
    pub fn finish_reason_str(&self) -> &str {
        self.finish_reason.as_deref().unwrap_or("none")
    }
}

/// Model metadata from response headers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseModelMetadata {
    pub context_window: Option<u64>,
    pub max_completion_tokens: Option<u32>,
    /// `x-models-etag` — triggers model catalog refresh when changed.
    pub models_etag: Option<String>,
}

/// Wire-credential provenance of a request that failed authentication.
///
/// A 401 for a request that went out with **no** credential header (a
/// fail-closed send while the bearer resolver had nothing wire-valid) is
/// not evidence against the credential itself; retry policies use this to
/// avoid charging credential-rejection budgets for such sends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SentCredential {
    /// The request carried a credential; the server rejected it.
    Sent,
    /// The request carried the provider's current credential at the instant
    /// the 401 response was classified. This is the credential that must be
    /// refreshed as server-rejected rather than merely re-adopted from cache.
    SameAsCurrent,
    /// The request carried a credential, but the provider had already rotated
    /// to a different current credential. Recovery may adopt that cached
    /// credential without forcing another refresh.
    DifferentFromCurrent,
    /// The request carried a credential, but the provider had no current
    /// credential when the 401 response was classified.
    CurrentUnavailable,
    /// The request went out with no credential header at all.
    Missing,
    /// Provenance unknown (synthesized or legacy errors). Retry policies
    /// treat this like [`SentCredential::Sent`] — fail closed toward
    /// terminating rather than retrying forever.
    #[default]
    Unknown,
}

/// Hand-written so an unrecognized value from a newer peer degrades to
/// `Unknown` instead of failing the whole containing payload
/// (`#[serde(other)]` is not available on externally-tagged enums).
impl<'de> Deserialize<'de> for SentCredential {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Ok(
            match std::borrow::Cow::<str>::deserialize(deserializer)?.as_ref() {
                "sent" => Self::Sent,
                "same_as_current" => Self::SameAsCurrent,
                "different_from_current" => Self::DifferentFromCurrent,
                "current_unavailable" => Self::CurrentUnavailable,
                "missing" => Self::Missing,
                _ => Self::Unknown,
            },
        )
    }
}

impl SentCredential {
    /// Classify from the credential fragment captured when the request was
    /// built (`None` = no credential header was stamped on the wire).
    pub fn from_sent_fragment(fragment: Option<&str>) -> Self {
        if fragment.is_some() {
            Self::Sent
        } else {
            Self::Missing
        }
    }

    pub fn is_missing(self) -> bool {
        matches!(self, Self::Missing)
    }

    /// Whether metadata proves a credential was present on the wire.
    pub fn is_sent(self) -> bool {
        matches!(
            self,
            Self::Sent
                | Self::SameAsCurrent
                | Self::DifferentFromCurrent
                | Self::CurrentUnavailable
        )
    }

    /// By reference so it can serve as a serde `skip_serializing_if`.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

/// Display prefix of [`SamplingError::Serialization`]. Shared with the
/// variant's `#[error(...)]` template so [`SamplingError::serialization_from_rendered`]
/// can never drift from what Display actually emits.
const SERIALIZATION_DISPLAY_PREFIX: &str = "serialization error: ";

#[derive(Debug, Error)]
pub enum SamplingError {
    #[error("{message}")]
    Auth {
        message: String,
        /// Whether the rejected request actually carried a credential.
        credential: SentCredential,
    },
    #[error("invalid client configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("request error: {0}")]
    Http(reqwest::Error),
    #[error("{prefix}{0}", prefix = SERIALIZATION_DISPLAY_PREFIX)]
    Serialization(serde_json::Error),
    #[error("API error (status {status}): {message}")]
    Api {
        status: StatusCode,
        message: String,
        model_metadata: Option<ResponseModelMetadata>,
        /// Parsed from the `Retry-After` response header (seconds).
        retry_after_secs: Option<u64>,
        /// Parsed from the `x-should-retry` response header.
        /// `Some(true)` = transient, retry may help.
        /// `Some(false)` = request-content error, don't retry.
        /// `None` = header absent (old server or non-proxy origin).
        should_retry: Option<bool>,
    },
    #[error("reqwest error stream: {0}")]
    EventStreamError(String),
    /// Server-side stream error (sent as JSON within the SSE stream)
    #[error("stream error ({error_type}): {message}")]
    StreamError { error_type: String, message: String },
    /// Per-chunk idle timeout — no SSE chunk received from the model within the
    /// configured deadline. NOT retryable: the model (or network path) is stuck,
    /// and replaying the same request would likely stall again.
    #[error("inference idle timeout after {elapsed_secs}s with no chunks")]
    IdleTimeout { elapsed_secs: u64 },
    #[error("empty response from model ({})", context.reason)]
    EmptyResponse { context: EmptyResponseContext },
    #[error("response truncated by max_tokens")]
    MaxTokensTruncation,
    /// A confident server-reported doom loop on the attempt (mid-stream or
    /// on the completed response). Retryable on the recovery loop's own
    /// budget, separate from the transport budget. Carries the raw trigger
    /// labels (never generation content) plus, for telemetry only, the
    /// stream chunk index the mid-stream abort fired at (`None` when the
    /// signal was only seen on the completed response).
    #[error("doom loop detected: {}", triggers.join(", "))]
    DoomLoopDetected {
        triggers: Vec<String>,
        aborted_at_chunk: Option<u64>,
    },
}

impl SamplingError {
    /// Preserve reqwest's typed classification while removing its attached
    /// request URL. URLs can contain API keys, signed-query credentials, or
    /// other secrets and reqwest includes them in `Display`/`Debug` by default.
    pub fn http(value: reqwest::Error) -> Self {
        Self::Http(value.without_url())
    }

    /// Auth error of unknown wire provenance — for paths that never sent a
    /// request (config validation, cancellation, actor teardown) or that
    /// lost the provenance (legacy round trips).
    pub fn auth_unknown(message: impl Into<String>) -> Self {
        Self::Auth {
            message: message.into(),
            credential: SentCredential::Unknown,
        }
    }

    /// Rebuild a `Serialization` error from a rendered message for non-`Clone`
    /// contexts; it must stay `Serialization` so it remains non-retryable.
    pub fn serialization_message(msg: impl fmt::Display) -> Self {
        Self::Serialization(serde::de::Error::custom(msg))
    }

    /// Rebuild from this variant's full rendered Display (e.g. a round-tripped
    /// `SamplingErrorInfo` message), stripping the Display prefix so the
    /// rebuilt error does not render it twice.
    pub fn serialization_from_rendered(rendered: &str) -> Self {
        Self::serialization_message(
            rendered
                .strip_prefix(SERIALIZATION_DISPLAY_PREFIX)
                .unwrap_or(rendered),
        )
    }

    pub fn is_auth_error(&self) -> bool {
        // Only 401 Unauthorized means the credentials themselves were rejected
        // and warrant a token refresh / re-auth. 403 Forbidden means the
        // request was authenticated successfully but the action is not
        // permitted (e.g. content-safety blocks, ZDR-blocked operations,
        // or other policy denials unrelated to credentials). Treating 403
        // as an auth error triggers a pointless
        // OIDC refresh and then surfaces as acp::Error::auth_required on
        // the client, which in the desktop app tears down the session and
        // can race with invalid_grant_threshold to wipe auth.json.
        matches!(
            self,
            SamplingError::Auth { .. }
                | SamplingError::Api {
                    status: StatusCode::UNAUTHORIZED,
                    ..
                }
        )
    }

    pub fn is_rate_limited(&self) -> bool {
        matches!(
            self,
            SamplingError::Api {
                status: StatusCode::TOO_MANY_REQUESTS,
                ..
            }
        )
    }

    pub fn is_payload_too_large(&self) -> bool {
        matches!(
            self,
            SamplingError::Api {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                ..
            }
        )
    }

    /// `true` when the error looks like a connection reset or broken pipe
    /// during request upload — the pattern nginx produces when it rejects an
    /// oversized payload by closing the connection instead of responding 413.
    ///
    /// Timeouts and connect failures are excluded: those are unrelated to
    /// payload size and stripping images on them would lose context for no
    /// reason.
    pub fn is_likely_body_rejected(&self) -> bool {
        match self {
            SamplingError::Http(err) => {
                // `is_request()` covers broken-pipe / connection-reset during
                // body upload.  `is_body()` covers stream-write failures.
                // Exclude timeouts and connect errors — those are unrelated.
                (err.is_request() || err.is_body()) && !err.is_timeout() && !err.is_connect()
            }
            _ => false,
        }
    }

    /// The server rejected the request because the conversation history
    /// contains `encrypted_content` from a different model family that the
    /// current model cannot decrypt. Never retryable — the user must start
    /// a new session.
    pub fn is_encrypted_content_error(&self) -> bool {
        matches!(
            self,
            SamplingError::Api {
                status: StatusCode::BAD_REQUEST,
                message,
                ..
            } if message.contains("encrypted_content")
        )
    }

    /// The API rejected the request because an inline image could not be
    /// processed. Matches both direct 400 and proxy-wrapped 500 responses.
    /// Exact-case match — consistent with `is_encrypted_content_error`.
    pub fn is_image_processing_error(&self) -> bool {
        matches!(
            self,
            SamplingError::Api {
                status,
                message,
                ..
            } if matches!(status.as_u16(), 400 | 500) && message.contains("Could not process image")
        )
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            SamplingError::Auth { .. } => false,
            SamplingError::InvalidConfiguration(_) => false,
            SamplingError::Http(err) => is_retryable_reqwest(err),
            SamplingError::Serialization(_) => false,
            SamplingError::Api { status, .. } => is_retryable_api_status(*status),
            SamplingError::EventStreamError(_) => true,
            SamplingError::StreamError { .. } => true,
            SamplingError::IdleTimeout { .. } => false,
            SamplingError::EmptyResponse { .. } => true,
            SamplingError::MaxTokensTruncation => false,
            SamplingError::DoomLoopDetected { .. } => true,
        }
    }

    pub fn model_metadata(&self) -> Option<&ResponseModelMetadata> {
        match self {
            SamplingError::Api { model_metadata, .. } => model_metadata.as_ref(),
            _ => None,
        }
    }

    pub fn retry_after(&self) -> Option<u64> {
        match self {
            SamplingError::Api {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }

    /// Server hint on whether this error is worth retrying.
    pub fn should_retry_header(&self) -> Option<bool> {
        match self {
            SamplingError::Api { should_retry, .. } => *should_retry,
            _ => None,
        }
    }

    /// True when this error is a context-window/size overflow — deterministic,
    /// so retrying the same payload can't help. See [`is_context_length_error`].
    pub fn is_context_length_error(&self) -> bool {
        match self {
            SamplingError::Api { message, .. } | SamplingError::StreamError { message, .. } => {
                is_context_length_error(message)
            }
            _ => false,
        }
    }

    /// Capacity / overload: HTTP 529, a 5xx whose message clearly says
    /// overloaded (proxies wrap stream overloads in a 500), or a stream
    /// error whose parsed `error_type` is a capacity type (`overloaded_error`
    /// / `service_unavailable_error`). Never reachable from a 4xx or a
    /// request-shaped stream error, whatever the message text. Transient —
    /// worth a short, bounded retry at the call site.
    pub fn is_overloaded(&self) -> bool {
        match self {
            SamplingError::Api {
                status, message, ..
            } => {
                status.as_u16() == 529
                    || (status.is_server_error() && message_looks_overloaded(message))
            }
            // `error_type` is already parsed from the stream payload — trust
            // it alone; matching message text here would let a request-shaped
            // error that merely mentions "overloaded" retry.
            SamplingError::StreamError { error_type, .. } => {
                error_type.eq_ignore_ascii_case("overloaded_error")
                    || error_type.eq_ignore_ascii_case("service_unavailable_error")
            }
            _ => false,
        }
    }

    /// Retry vetoes shared by every retry loop — the sampler actor's
    /// `classify_error` and one-shot callers like `/btw`. One definition so
    /// a new veto lands everywhere at once:
    /// - `x-should-retry: false` — the server says the failure is
    ///   request-content-caused, not transient.
    /// - Context-length overflow — deterministic; re-sending the same
    ///   payload always fails.
    pub fn is_retry_vetoed(&self) -> bool {
        self.should_retry_header() == Some(false) || self.is_context_length_error()
    }
}

impl From<reqwest::Error> for SamplingError {
    fn from(value: reqwest::Error) -> Self {
        Self::http(value)
    }
}

impl From<serde_json::Error> for SamplingError {
    fn from(value: serde_json::Error) -> Self {
        tracing::debug!("Serde deserialization error: {:?}", &value);
        Self::Serialization(value)
    }
}

/// OpenAI-standard provider error format: `{"error": {"message": "...", "type": "..."}}`.
#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    message: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

/// Flat error from the Grok proxy/gateway: `{"code": "...", "error": "..."}`.
#[derive(Debug, Deserialize)]
struct FlatErrorResponse {
    error: String,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Debug)]
struct ParsedProviderError {
    kind: Option<String>,
    code: Option<String>,
    message: String,
}

/// Parse a provider error envelope for in-process classification only.
///
/// Every field is provider-controlled and may echo request credentials. Callers
/// must convert it to [`SafeProviderError`] before logging, returning, or
/// attaching it to an error.
fn try_parse_error(data: &str) -> Option<ParsedProviderError> {
    if let Ok(resp) = serde_json::from_str::<ErrorResponse>(data) {
        return Some(ParsedProviderError {
            kind: resp.error.kind,
            code: resp.error.code,
            message: resp.error.message.unwrap_or_default(),
        });
    }
    if let Ok(flat) = serde_json::from_str::<FlatErrorResponse>(data) {
        return Some(ParsedProviderError {
            kind: None,
            code: flat.code,
            message: flat.error,
        });
    }
    None
}

const FREE_USAGE_EXHAUSTED_CODE: &str = "subscription:free-usage-exhausted";
const SAFE_CONTEXT_LENGTH_MESSAGE: &str = "The prompt is too long for this model.";
const SAFE_ENCRYPTED_CONTENT_MESSAGE: &str = "encrypted_content from another model family";
const SAFE_IMAGE_PROCESSING_MESSAGE: &str = "Could not process image";
const SAFE_CREDIT_BLOCK_MESSAGE: &str = "provider credit balance exhausted";
const SAFE_OVERLOADED_MESSAGE: &str = "overloaded_error";
const SAFE_STREAM_ERROR_MESSAGE: &str = "upstream stream error";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderErrorClass {
    FreeUsageExhausted,
    ContextLength,
    EncryptedContent,
    ImageProcessing,
    CreditBlock,
    Overloaded,
    Generic,
}

#[derive(Debug, Clone, Copy)]
struct SafeProviderError {
    error_type: &'static str,
    message: &'static str,
    class: ProviderErrorClass,
}

fn allowlisted_error_type(value: &str) -> Option<&'static str> {
    match value {
        "authentication_error" => Some("authentication_error"),
        "context_length_exceeded" => Some("context_length_exceeded"),
        "invalid_request_error" => Some("invalid_request_error"),
        "overloaded_error" => Some("overloaded_error"),
        "rate_limit_error" => Some("rate_limit_error"),
        "server_error" => Some("server_error"),
        FREE_USAGE_EXHAUSTED_CODE => Some(FREE_USAGE_EXHAUSTED_CODE),
        _ => None,
    }
}

fn classify_provider_error(parsed: &ParsedProviderError) -> SafeProviderError {
    let typed_value = parsed.code.as_deref().or(parsed.kind.as_deref());

    if typed_value == Some(FREE_USAGE_EXHAUSTED_CODE) {
        return SafeProviderError {
            error_type: FREE_USAGE_EXHAUSTED_CODE,
            message: FREE_USAGE_EXHAUSTED_CODE,
            class: ProviderErrorClass::FreeUsageExhausted,
        };
    }
    if [parsed.code.as_deref(), parsed.kind.as_deref()]
        .into_iter()
        .flatten()
        .any(|value| matches!(value, "overloaded_error" | "service_unavailable_error"))
    {
        return SafeProviderError {
            error_type: "overloaded_error",
            message: SAFE_OVERLOADED_MESSAGE,
            class: ProviderErrorClass::Overloaded,
        };
    }
    if typed_value == Some("context_length_exceeded") || is_context_length_error(&parsed.message) {
        return SafeProviderError {
            error_type: "context_length_exceeded",
            message: SAFE_CONTEXT_LENGTH_MESSAGE,
            class: ProviderErrorClass::ContextLength,
        };
    }
    if parsed.message.contains("encrypted_content") {
        return SafeProviderError {
            error_type: "invalid_request_error",
            message: SAFE_ENCRYPTED_CONTENT_MESSAGE,
            class: ProviderErrorClass::EncryptedContent,
        };
    }
    if parsed.message.contains("Could not process image") {
        return SafeProviderError {
            error_type: "invalid_request_error",
            message: SAFE_IMAGE_PROCESSING_MESSAGE,
            class: ProviderErrorClass::ImageProcessing,
        };
    }
    if is_credit_block_message(&parsed.message) {
        return SafeProviderError {
            error_type: "credit_balance_exhausted",
            message: SAFE_CREDIT_BLOCK_MESSAGE,
            class: ProviderErrorClass::CreditBlock,
        };
    }

    SafeProviderError {
        error_type: typed_value
            .and_then(allowlisted_error_type)
            .unwrap_or("unknown"),
        message: SAFE_STREAM_ERROR_MESSAGE,
        class: ProviderErrorClass::Generic,
    }
}

fn safe_structured_error(bytes: &[u8]) -> Option<SafeProviderError> {
    let parsed = std::str::from_utf8(bytes).ok().and_then(try_parse_error)?;
    Some(classify_provider_error(&parsed))
}

/// Short status-based copy when the body is not a structured JSON error.
///
/// Edge proxies (Cloudflare 52x, 502/503/504) return HTML pages; we never
/// sniff body text — only the HTTP status drives this fallback.
pub fn status_user_message(status: StatusCode) -> String {
    match status.as_u16() {
        code @ 502..=504 => {
            format!("Grok is temporarily unavailable. Please try again in a moment. (HTTP {code}).")
        }
        // Upstream capacity, not an edge failure — see [`SamplingError::is_overloaded`].
        code @ 529 => {
            format!("Grok is temporarily overloaded. Please try again in a moment. (HTTP {code}).")
        }
        // Cloudflare edge: origin unreachable or timed out (520–524), or an
        // edge-side 1xxx failure (530).
        code @ 520..=524 | code @ 530 => {
            format!(
                "Connection to Grok timed out or was interrupted. Please try again. (HTTP {code})."
            )
        }
        // Cloudflare origin TLS (handshake / invalid certificate) — not transient.
        code @ 525 | code @ 526 => {
            format!("Secure connection to Grok failed. (HTTP {code}).")
        }
        code if status.is_server_error() => {
            format!("Something went wrong on the server (HTTP {code}).")
        }
        code => format!("Request failed (HTTP {code})."),
    }
}

/// Parse an API error body into a short string.
///
/// Provider-controlled messages and types are never surfaced because they may
/// echo all or part of a credential. Only fixed semantic markers derived from
/// narrowly classified conditions are returned.
pub fn parse_error_bytes(bytes: &[u8]) -> String {
    safe_structured_error(bytes)
        .map(|safe| match safe.class {
            ProviderErrorClass::FreeUsageExhausted
            | ProviderErrorClass::ContextLength
            | ProviderErrorClass::EncryptedContent
            | ProviderErrorClass::ImageProcessing
            | ProviderErrorClass::CreditBlock
            | ProviderErrorClass::Overloaded => safe.message,
            ProviderErrorClass::Generic => "upstream error",
        })
        .unwrap_or("upstream error")
        .to_string()
}

/// Max chars retained from a provider error body in diagnostics (#245).
///
/// Provider bodies are untrusted and may echo request credentials; anything
/// that surfaces them must bound length and say when it truncated.
pub const PROVIDER_ERROR_BODY_PREVIEW_MAX: usize = 256;

/// Whether a short identifier is safe to surface from a provider body.
///
/// Rejects free-form text and long high-entropy tokens; keeps field names
/// like `client_version`, `reasoning.summary`, `invalid_request_error`.
fn is_safe_diagnostic_token(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | ':' | '/'))
    {
        return false;
    }
    // Charset and length alone do not separate a field *name* from a secret:
    // `sk-...`, a JWT segment and a UUID are all "alphanumerics and dashes,
    // under 64 characters". A hostile `param` was passing through verbatim
    // until a test put the sentinel in every position rather than only the two
    // an obvious hostile body would use.
    //
    // The two rules below are what a rejected-field name looks like in the
    // APIs this talks to, and both are already used elsewhere in this file --
    // `safe_validation_message` rejects long alnum runs carrying a digit for
    // the same reason. Applied here at 16 rather than 24, because these are
    // identifiers, not prose.
    if has_credential_marker(s) {
        return false;
    }
    if s.chars().any(|c| c.is_ascii_uppercase()) {
        // `client_version`, `max_tokens`, `input.0.content`. Credentials are
        // mixed-case far more often than parameter names are.
        return false;
    }
    !s.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|run| run.len() >= 16 && run.bytes().any(|b| b.is_ascii_digit()))
}

fn has_credential_marker(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("bearer ")
        || lower.contains("authorization:")
        || lower.contains("x-api-key")
        || lower.contains("sk-")
        || lower.contains("api_key=")
        || lower.contains("access_token")
}

/// Safe validation-style message: short, no credential markers, no long
/// high-entropy runs. Returns `None` when the text is not safe to surface.
fn safe_validation_message(message: &str) -> Option<String> {
    let message = message.trim();
    if message.is_empty() || message.len() > 200 {
        return None;
    }
    if has_credential_marker(message) {
        return None;
    }
    // Drop long alnum runs that look like tokens/keys (not ordinary words).
    for word in message.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
        if word.len() >= 24 && word.bytes().any(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some(message.to_string())
}

fn truncate_with_notice(s: &str, max_chars: usize) -> String {
    let char_len = s.chars().count();
    if char_len <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{truncated} (truncated)")
}

/// Build a diagnostic summary from known error envelope shapes without
/// echoing free-form provider text that may contain credentials.
fn structured_diagnostic_summary(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let mut parts: Vec<String> = Vec::new();

    if let Some(err) = value.get("error") {
        if let Some(kind) = err
            .get("type")
            .and_then(serde_json::Value::as_str)
            .and_then(allowlisted_error_type)
        {
            parts.push(format!("type={kind}"));
        }
        if let Some(param) = err
            .get("param")
            .and_then(serde_json::Value::as_str)
            .filter(|s| is_safe_diagnostic_token(s))
        {
            parts.push(format!("param={param}"));
        }
        if let Some(code) = err
            .get("code")
            .and_then(serde_json::Value::as_str)
            .filter(|s| is_safe_diagnostic_token(s) || allowlisted_error_type(s).is_some())
        {
            parts.push(format!("code={code}"));
        }
        if let Some(message) = err
            .get("message")
            .and_then(serde_json::Value::as_str)
            .and_then(safe_validation_message)
        {
            parts.push(format!("message={message}"));
        }
    }

    // Flat gateway: {"code":"...","error":"..."} — only surface allowlisted code.
    if parts.is_empty()
        && let Some(code) = value
            .get("code")
            .and_then(serde_json::Value::as_str)
            .and_then(allowlisted_error_type)
    {
        parts.push(format!("code={code}"));
    }

    // FastAPI / pydantic style used by Codex catalog probes (#188):
    // {"loc": ["query", "client_version"], "msg": "Field required"}
    if let Some(loc) = value.get("loc") {
        let loc_text = loc.to_string();
        if loc_text.len() <= 120 && !has_credential_marker(&loc_text) {
            parts.push(format!("loc={loc_text}"));
        }
    }
    if let Some(msg) = value
        .get("msg")
        .and_then(serde_json::Value::as_str)
        .and_then(safe_validation_message)
    {
        parts.push(format!("msg={msg}"));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Truncated, secret-scrubbed preview of a provider error body for logs and
/// (when safe) user-facing 400 diagnostics (#245).
///
/// Provider bodies are untrusted. This returns either:
/// - a structured summary of allowlisted / short validation fields, or
/// - a length-bounded raw preview when the body has no credential markers, or
/// - `body_len=N (provider body withheld)` when the body looks hostile.
///
/// Always bounds length and appends ` (truncated)` when it cuts.
pub fn provider_error_body_preview(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // Structured summary or nothing. There is deliberately no raw-text
    // fallback: every value that leaves here comes out of
    // `structured_diagnostic_summary`, where each position is gated by a fixed
    // vocabulary (`allowlisted_error_type`) or a charset check
    // (`is_safe_diagnostic_token`, `safe_validation_message`).
    //
    // The fallback this replaces emitted the provider's own bytes whenever a
    // credential *marker* was not spotted in them, which makes marker
    // detection a second classifier standing between a provider and the log —
    // and the one that decides in the permissive direction. A provider that
    // echoes the failing request back does not have to name its credential in
    // a way a marker list recognises. Absence of evidence was doing the work
    // of evidence of absence.
    //
    // The cost is real and accepted: a 400 whose body is not JSON we can parse
    // now contributes nothing beyond its status. That is the same information
    // the caller had before #245, and it is the direction to fail in.
    structured_diagnostic_summary(bytes)
        .map(|summary| truncate_with_notice(&summary, PROVIDER_ERROR_BODY_PREVIEW_MAX))
        .unwrap_or_default()
}

/// Max chars of a structured (JSON) error message shown to users.
///
/// Upstream's, and used by its `provider_error.rs`. Kept as its own item: the
/// sync landed it *inside* the doc comment below, which compiles and reads as
/// two half-sentences.
pub const MAX_USER_ERROR_BODY_CHARS: usize = 280;

/// User-facing message for a failed API call.
///
/// Recovery-critical conditions map to fixed semantic markers. For HTTP 400,
/// a truncated secret-scrubbed body preview is appended when available so a
/// rejected field name is visible without a proxy (#245). Every other status
/// (including structured JSON on 429/5xx and Cloudflare HTML) maps only from
/// the HTTP status — free-form provider text may echo credentials.
pub fn user_facing_api_error_message(status: StatusCode, bytes: &[u8]) -> String {
    match safe_structured_error(bytes) {
        Some(safe)
            if matches!(
                safe.class,
                ProviderErrorClass::FreeUsageExhausted
                    | ProviderErrorClass::ContextLength
                    | ProviderErrorClass::EncryptedContent
                    | ProviderErrorClass::ImageProcessing
                    | ProviderErrorClass::CreditBlock
                    | ProviderErrorClass::Overloaded
            ) =>
        {
            safe.message.to_string()
        }
        _ if status == StatusCode::BAD_REQUEST => {
            let base = status_user_message(status);
            let preview = provider_error_body_preview(bytes);
            if preview.is_empty() {
                base
            } else {
                format!("{base} {preview}")
            }
        }
        _ => status_user_message(status),
    }
}

pub fn try_parse_stream_error(data: &str) -> Option<SamplingError> {
    let parsed = try_parse_error(data)?;
    let safe = classify_provider_error(&parsed);
    tracing::warn!(error_type = safe.error_type, "Server-side stream error");
    Some(SamplingError::StreamError {
        error_type: safe.error_type.to_string(),
        message: safe.message.to_string(),
    })
}

/// True when an error message indicates a context-window overflow. Backends report
/// this inconsistently with no stable error code, so we match the message text; it's
/// deterministic (re-sending the same payload always fails), so callers must not retry.
pub fn is_context_length_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("too long for this model")
        || m.contains("prompt is too long")
        || m.contains("maximum prompt length")
        || m.contains("maximum context length")
        || m.contains("context_length_exceeded")
        || (m.contains("current message") && m.contains("exceeds budget"))
}

/// Classify provider-controlled credit exhaustion text before it is replaced
/// by a fixed marker. Only the boolean result crosses the trust boundary.
fn is_credit_block_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("spending-limit")
        || message.contains("spending limit")
        || message.contains("out of credits")
        || message.contains("run out of credits")
        || message.contains("usage balance exhausted")
        || message.contains("usage limit reached")
}

/// Whether an HTTP status is worth retrying: the same 429 + any 5xx rule CCP
/// publishes in `x-should-retry`, minus Cloudflare's origin-TLS 525/526
/// (requests reach CCP through the Cloudflare edge, which answers with its
/// own 52x pages when the origin is unreachable).
pub fn is_retryable_api_status(status: StatusCode) -> bool {
    RetryPolicy::edge_client().should_retry(status.as_u16())
}

/// Decide whether a [`reqwest::Error`] is worth retrying.
pub fn is_retryable_reqwest(err: &reqwest::Error) -> bool {
    if err.is_timeout() || err.is_connect() {
        return true;
    }

    if err.is_status() {
        return err.status().is_some_and(is_retryable_api_status);
    }

    if err.is_request() || err.is_body() {
        return true;
    }

    false
}

/// Capacity-style provider text: "Overloaded" / `overloaded_error` (possibly
/// proxy-wrapped) or `service_unavailable_error` (503-shaped capacity).
fn message_looks_overloaded(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("overloaded") || m.contains("service_unavailable_error")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overloaded_detects_stream_and_api_shapes() {
        assert!(
            SamplingError::StreamError {
                error_type: "overloaded_error".into(),
                message: "Overloaded".into(),
            }
            .is_overloaded()
        );
        assert!(
            SamplingError::Api {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "stream error (overloaded_error): Overloaded".into(),
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
            }
            .is_overloaded()
        );
        assert!(
            SamplingError::Api {
                status: StatusCode::from_u16(529).unwrap(),
                message: "capacity".into(),
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
            }
            .is_overloaded()
        );
        assert!(
            SamplingError::Api {
                status: StatusCode::from_u16(529).unwrap(),
                message: "capacity".into(),
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
            }
            .is_retryable()
        );
        assert!(!SamplingError::auth_unknown("nope").is_overloaded());
        assert!(
            !SamplingError::Api {
                status: StatusCode::BAD_REQUEST,
                message: "invalid json".into(),
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
            }
            .is_overloaded()
        );
        // Only server errors classify on message text — a 4xx that merely
        // mentions "overloaded" is a request error, not capacity.
        assert!(
            !SamplingError::Api {
                status: StatusCode::BAD_REQUEST,
                message: "field `overloaded` is not a valid parameter".into(),
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
            }
            .is_overloaded()
        );
        // Stream errors classify on the parsed error_type only — a
        // request-shaped stream error mentioning "overloaded" is not capacity.
        assert!(
            !SamplingError::StreamError {
                error_type: "invalid_request_error".into(),
                message: "tool result mentions overloaded".into(),
            }
            .is_overloaded()
        );
        assert!(
            SamplingError::StreamError {
                error_type: "service_unavailable_error".into(),
                message: "upstream capacity".into(),
            }
            .is_overloaded()
        );
    }

    #[test]
    fn overloaded_message_matches_backend_variants() {
        // 5xx messages that classify as capacity.
        for msg in [
            "Overloaded",
            "stream error (overloaded_error): Overloaded",
            "overloaded_error",
            "service_unavailable_error: try again",
        ] {
            assert!(
                SamplingError::Api {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: msg.into(),
                    model_metadata: None,
                    retry_after_secs: None,
                    should_retry: None,
                }
                .is_overloaded(),
                "expected overloaded for message: {msg}"
            );
        }
        // 5xx messages that do not.
        for msg in ["upstream connect timeout", "internal error"] {
            assert!(
                !SamplingError::Api {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: msg.into(),
                    model_metadata: None,
                    retry_after_secs: None,
                    should_retry: None,
                }
                .is_overloaded(),
                "expected not overloaded for message: {msg}"
            );
        }
    }

    #[test]
    fn retry_veto_covers_header_and_context_length() {
        let vetoed_by_header = SamplingError::Api {
            status: StatusCode::from_u16(529).unwrap(),
            message: "capacity".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: Some(false),
        };
        assert!(vetoed_by_header.is_retry_vetoed());

        let vetoed_by_context = SamplingError::Api {
            status: StatusCode::from_u16(529).unwrap(),
            message: "prompt is too long: 300000 tokens > 200000 maximum".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(vetoed_by_context.is_retry_vetoed());

        let not_vetoed = SamplingError::Api {
            status: StatusCode::from_u16(529).unwrap(),
            message: "capacity".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(!not_vetoed.is_retry_vetoed());
    }

    #[test]
    fn context_length_error_matches_backend_variants() {
        for msg in [
            "This model's maximum prompt length is 256000 but the request contains 1500000",
            "The prompt is too long for this model's context window.",
            "none: The prompt is too long for this model's context window.",
            "This model's maximum context length is 200000 tokens",
            "invalid_request_error: prompt is too long: 300000 tokens > 200000 maximum",
            "error type: context_length_exceeded",
            "Failed to start sampling: [conversation] Current message (1000000 tokens) exceeds budget (500000 tokens)",
            "API error (status 400 Bad Request): invalid-argument: Failed to start sampling: [conversation] Current message (1000000 tokens) exceeds budget (500000 tokens)",
            "compact failed: API error (status 400 Bad Request): invalid-argument: Failed to start sampling: [conversation] Current message (1000000 tokens) exceeds budget (500000 tokens)",
            "Current message (600000) exceeds budget (500000)",
        ] {
            assert!(is_context_length_error(msg), "should match: {msg}");
        }
        for msg in [
            "rate limited",
            "internal server error",
            "connection reset",
            "Attached file content (300000 tokens) causes message to exceed budget",
            "compact index estimate 2.0 GB exceeds budget 1.0 GB",
        ] {
            assert!(!is_context_length_error(msg), "should not match: {msg}");
        }
        // The method delegates for the Api/StreamError variants.
        let api = SamplingError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "none: The prompt is too long for this model's context window.".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(api.is_context_length_error());
        assert!(
            SamplingError::StreamError {
                error_type: "overloaded_error".into(),
                message: "prompt is too long".into(),
            }
            .is_context_length_error()
        );
        assert!(!SamplingError::auth_unknown("nope").is_context_length_error());
    }

    #[test]
    fn serialization_message_stays_serialization_and_non_retryable() {
        let err = SamplingError::serialization_message("bad payload at line 1 column 7");
        assert!(matches!(err, SamplingError::Serialization(_)));
        assert!(!err.is_retryable());
        assert!(err.to_string().contains("bad payload at line 1 column 7"));
    }

    #[test]
    fn serialization_from_rendered_round_trips_display() {
        // Derived from a REAL error's Display so a template rewording cannot
        // silently desynchronize the strip from the prefix it mirrors.
        let original =
            SamplingError::Serialization(serde_json::from_str::<i32>("not a number").unwrap_err());
        let rendered = original.to_string();
        let rebuilt = SamplingError::serialization_from_rendered(&rendered);
        assert!(matches!(rebuilt, SamplingError::Serialization(_)));
        assert!(!rebuilt.is_retryable());
        assert_eq!(
            rebuilt.to_string(),
            rendered,
            "rendered Display must round-trip without double-prefixing"
        );
        // Bare (non-rendered) input gains the prefix exactly once.
        assert_eq!(
            SamplingError::serialization_from_rendered("bare message").to_string(),
            format!("{SERIALIZATION_DISPLAY_PREFIX}bare message"),
        );
    }

    #[test]
    fn idle_timeout_is_not_retryable() {
        let err = SamplingError::IdleTimeout { elapsed_secs: 300 };
        assert!(
            !err.is_retryable(),
            "IdleTimeout must not be retried — would cause 3× amplification"
        );
    }

    #[test]
    fn event_stream_error_is_retryable() {
        // Verify the existing contract hasn't changed — EventStreamError is retryable.
        let err = SamplingError::EventStreamError("connection reset".into());
        assert!(err.is_retryable());
    }

    #[test]
    fn idle_timeout_display() {
        let err = SamplingError::IdleTimeout { elapsed_secs: 120 };
        let msg = err.to_string();
        assert!(
            msg.contains("120s"),
            "Display should include elapsed_secs: {msg}"
        );
    }

    #[test]
    fn try_parse_stream_error_flat_format() {
        let data = r#"{"code":"The service is currently unavailable","error":"Service temporarily unavailable. The model did not respond to this request."}"#;
        let err = try_parse_stream_error(data).expect("should parse flat error");
        match err {
            SamplingError::StreamError {
                error_type,
                message,
            } => {
                assert_eq!(error_type, "unknown");
                assert_eq!(message, SAFE_STREAM_ERROR_MESSAGE);
            }
            other => panic!("expected StreamError, got {other:?}"),
        }
    }

    #[test]
    fn try_parse_stream_error_valid_chunk_returns_none() {
        let data = r#"{"id":"abc","object":"chat.completion.chunk","created":0,"model":"test","choices":[]}"#;
        assert!(
            try_parse_stream_error(data).is_none(),
            "valid chunk should not be parsed as error"
        );
    }

    #[test]
    fn parse_error_bytes_flat_format() {
        let bytes =
            br#"{"code":"The service is currently unavailable","error":"Service temporarily unavailable."}"#;
        let msg = parse_error_bytes(bytes);
        assert_eq!(msg, "upstream error");
    }

    #[test]
    fn parse_error_bytes_rejects_non_json_body() {
        let html = br#"<!DOCTYPE html>
<html lang="en-US">
<head><title>grok.com | 524: A timeout occurred</title></head>
<body><h1>A timeout occurred Error code 524</h1></body>
</html>"#;
        let msg = parse_error_bytes(html);
        assert_eq!(msg, "upstream error");
        // Plain non-JSON text is also rejected (no body sniffing).
        assert_eq!(
            parse_error_bytes(b"some random gateway text"),
            "upstream error"
        );
    }

    #[test]
    fn user_facing_api_error_message_maps_non_json_by_status() {
        let html = br#"<!DOCTYPE html><html><body>timeout</body></html>"#;
        let msg = user_facing_api_error_message(StatusCode::from_u16(524).unwrap(), html);
        assert_eq!(msg, status_user_message(StatusCode::from_u16(524).unwrap()));

        let msg_503 =
            user_facing_api_error_message(StatusCode::SERVICE_UNAVAILABLE, b"not json either");
        assert_eq!(
            msg_503,
            status_user_message(StatusCode::SERVICE_UNAVAILABLE)
        );
    }

    #[test]
    fn user_facing_discards_provider_controlled_json_message() {
        let bytes = br#"{"error":{"message":"rate limit exceeded","type":"rate_limit_error"}}"#;
        let msg = user_facing_api_error_message(StatusCode::TOO_MANY_REQUESTS, bytes);
        assert_eq!(msg, status_user_message(StatusCode::TOO_MANY_REQUESTS));
    }

    /// #245: a 400 must surface the field the endpoint rejected. Structured
    /// validation envelopes keep `param` / `loc` / short `msg` so a missing
    /// `client_version` is visible without a proxy.
    #[test]
    fn user_facing_400_surfaces_structured_rejected_field() {
        let bytes = br#"{"error":{"message":"Unsupported parameter: 'reasoning.summary' is not supported with this model.","type":"invalid_request_error","param":"reasoning.summary"}}"#;
        let msg = user_facing_api_error_message(StatusCode::BAD_REQUEST, bytes);
        assert!(
            msg.contains("param=reasoning.summary"),
            "400 must name the rejected field: {msg}"
        );
        assert!(
            msg.contains("type=invalid_request_error"),
            "400 must keep the allowlisted type: {msg}"
        );
        assert!(
            msg.contains("Request failed (HTTP 400)"),
            "400 keeps the status copy: {msg}"
        );
    }

    /// #245 trap: provider-controlled bodies may echo credentials. The 400
    /// A body that is not a structured error contributes **nothing**.
    ///
    /// This is the assertion that stops the raw-text fallback coming back. The
    /// version of this function that shipped first emitted the provider's own
    /// bytes whenever `has_credential_marker` did not spot something in them,
    /// which made marker detection a classifier deciding, in the permissive
    /// direction, whether a provider's bytes may be surfaced. A provider
    /// echoing the failing request back need not name its credential in a
    /// shape that list recognises.
    ///
    /// Each case below is a body a reasonable fallback would have surfaced.
    #[test]
    fn provider_error_body_preview_is_empty_for_anything_unstructured() {
        for body in [
            "upstream connect error or disconnect/reset before headers",
            "{\"detail\":\"not a shape we parse\"}",
            "<!DOCTYPE html><html><body>502 Bad Gateway</body></html>",
            "{not json at all",
            "\u{feff}",
        ] {
            assert_eq!(
                provider_error_body_preview(body.as_bytes()),
                "",
                "unstructured body must contribute nothing: {body:?}"
            );
        }
    }

    /// preview path must stay secret-free (same family as
    /// `full_and_partial_credential_echoes_never_escape_provider_errors`) and
    /// must bound length with an explicit truncation notice.
    #[test]
    fn provider_error_body_preview_is_secret_free_and_bounded() {
        const SENTINEL: &str = "GB245-secret-bearer-0123456789abcdef";
        let hostile = format!(
            r#"{{"error":{{"message":"Authorization: Bearer {SENTINEL}","type":"{SENTINEL}","param":"client_version"}}}}"#
        );
        let preview = provider_error_body_preview(hostile.as_bytes());
        assert!(
            !preview.contains(SENTINEL),
            "full credential escaped preview: {preview}"
        );
        for window in SENTINEL.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !preview.contains(fragment),
                "credential fragment {fragment:?} escaped: {preview}"
            );
        }
        // Safe structural field must still diagnose the rejection.
        assert!(
            preview.contains("param=client_version"),
            "safe param must survive scrubbing: {preview}"
        );

        // The sentinel in *every* position a summary can draw from, not just
        // the two an obvious hostile body would use. Each position has its own
        // gate, and a gate that was never asked about is a gate nobody knows
        // the state of.
        let everywhere = format!(
            r#"{{"error":{{"type":"{SENTINEL}","param":"{SENTINEL}","code":"{SENTINEL}","message":"{SENTINEL}"}},
                "code":"{SENTINEL}",
                "detail":[{{"loc":["query","{SENTINEL}"],"msg":"{SENTINEL}","type":"{SENTINEL}"}}]}}"#
        );
        let preview_everywhere = provider_error_body_preview(everywhere.as_bytes());
        for window in SENTINEL.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !preview_everywhere.contains(fragment),
                "credential fragment {fragment:?} escaped via a summary position: \
                 {preview_everywhere}"
            );
        }

        // Truncation is exercised through the structured path, because that is
        // the only path: every position is individually capped (param and code
        // at 64, message at 200) but their sum exceeds the preview budget.
        let long_param = "a".repeat(64);
        let long_code = "b".repeat(64);
        let long_message = "field required ".repeat(13);
        let huge = format!(
            r#"{{"error":{{"type":"invalid_request_error","param":"{long_param}","code":"{long_code}","message":"{long_message}"}}}}"#
        );
        let truncated = provider_error_body_preview(huge.as_bytes());
        assert!(
            truncated.contains("truncated"),
            "unbounded body must announce truncation: {truncated}"
        );
        assert!(
            truncated.chars().count()
                <= PROVIDER_ERROR_BODY_PREVIEW_MAX + " (truncated)".chars().count(),
            "preview must be bounded: len={}",
            truncated.chars().count()
        );

        let user = user_facing_api_error_message(StatusCode::BAD_REQUEST, hostile.as_bytes());
        assert!(!user.contains(SENTINEL));
        for window in SENTINEL.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(!user.contains(fragment), "fragment in user message: {user}");
        }
    }

    #[test]
    fn proxy_wrapped_overload_preserves_safe_classification_marker() {
        const SENTINEL: &str = "GB002-overload-secret-0123456789abcdef";
        let bytes = format!(
            r#"{{"error":{{"message":"Overloaded: {SENTINEL}","type":"overloaded_error"}}}}"#
        );
        let message =
            user_facing_api_error_message(StatusCode::INTERNAL_SERVER_ERROR, bytes.as_bytes());

        assert_eq!(message, SAFE_OVERLOADED_MESSAGE);
        assert!(!message.contains(SENTINEL));
        assert!(
            SamplingError::Api {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message,
                model_metadata: None,
                retry_after_secs: None,
                should_retry: None,
            }
            .is_overloaded()
        );
    }

    #[test]
    fn full_and_partial_credential_echoes_never_escape_provider_errors() {
        const SENTINEL: &str = "GB002-secret-bearer-0123456789abcdef";
        let bytes = format!(
            r#"{{"error":{{"message":"Authorization: Bearer {SENTINEL}","type":"{SENTINEL}"}}}}"#
        );

        let parsed = parse_error_bytes(bytes.as_bytes());
        let user = user_facing_api_error_message(StatusCode::UNAUTHORIZED, bytes.as_bytes());
        let stream = try_parse_stream_error(&bytes)
            .expect("structured envelope")
            .to_string();

        for output in [&parsed, &user, &stream] {
            assert!(
                !output.contains(SENTINEL),
                "full credential escaped: {output}"
            );
            for window in SENTINEL.as_bytes().windows(8) {
                let fragment = std::str::from_utf8(window).expect("ASCII sentinel");
                assert!(
                    !output.contains(fragment),
                    "credential fragment {fragment:?} escaped: {output}"
                );
            }
        }
    }

    #[test]
    fn recovery_critical_classes_use_fixed_safe_markers() {
        let free_usage = br#"{"code":"subscription:free-usage-exhausted","error":"secret echo"}"#;
        assert_eq!(parse_error_bytes(free_usage), FREE_USAGE_EXHAUSTED_CODE);
        assert_eq!(
            user_facing_api_error_message(StatusCode::TOO_MANY_REQUESTS, free_usage),
            FREE_USAGE_EXHAUSTED_CODE
        );

        let context = br#"{"error":{"message":"maximum context length exceeded by secret payload","type":"invalid_request_error"}}"#;
        assert_eq!(parse_error_bytes(context), SAFE_CONTEXT_LENGTH_MESSAGE);

        let encrypted = br#"{"error":{"message":"Could not decrypt encrypted_content: secret payload","type":"invalid_request_error"}}"#;
        assert_eq!(parse_error_bytes(encrypted), SAFE_ENCRYPTED_CONTENT_MESSAGE);

        let image = br#"{"error":{"message":"Could not process image secret payload","type":"invalid_request_error"}}"#;
        assert_eq!(parse_error_bytes(image), SAFE_IMAGE_PROCESSING_MESSAGE);

        const CREDIT_SENTINEL: &str = "GB002-credit-body-secret-0123456789abcdef";
        let credit = format!(
            r#"{{"error":{{"message":"usage balance exhausted {CREDIT_SENTINEL}","type":"billing_error"}}}}"#
        );
        let rendered = parse_error_bytes(credit.as_bytes());
        assert_eq!(rendered, SAFE_CREDIT_BLOCK_MESSAGE);
        assert!(!rendered.contains(CREDIT_SENTINEL));
    }

    /// Regression test: 403 Forbidden must NOT be classified as an auth
    /// error. The proxy returns 403 for policy denials that are unrelated
    /// to the caller's credentials (content-safety blocks, ZDR-gated
    /// operations, or other usage-policy blocks). Misclassifying these as
    /// auth errors triggers a pointless OIDC
    /// refresh and surfaces as acp::Error::auth_required on the client,
    /// tearing down the session and risking an
    /// `invalid_grant_threshold`-triggered wipe of auth.json.
    #[test]
    fn forbidden_is_not_auth_error() {
        let err = SamplingError::Api {
            status: StatusCode::FORBIDDEN,
            message: "Content violates usage guidelines.".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(
            !err.is_auth_error(),
            "403 Forbidden must not be treated as an auth error"
        );
    }

    #[test]
    fn unauthorized_is_auth_error() {
        let err = SamplingError::Api {
            status: StatusCode::UNAUTHORIZED,
            message: "Invalid or expired credentials".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(
            err.is_auth_error(),
            "401 Unauthorized must be an auth error"
        );
    }

    #[test]
    fn auth_variant_is_auth_error() {
        let err = SamplingError::auth_unknown("bad key");
        assert!(err.is_auth_error());
    }

    /// Known values round-trip; an unrecognized value from a newer peer
    /// degrades to `Unknown` instead of failing the containing payload.
    #[test]
    fn sent_credential_wire_compat() {
        for (json, expected) in [
            ("\"sent\"", SentCredential::Sent),
            ("\"same_as_current\"", SentCredential::SameAsCurrent),
            (
                "\"different_from_current\"",
                SentCredential::DifferentFromCurrent,
            ),
            (
                "\"current_unavailable\"",
                SentCredential::CurrentUnavailable,
            ),
            ("\"missing\"", SentCredential::Missing),
            ("\"unknown\"", SentCredential::Unknown),
            ("\"some-future-variant\"", SentCredential::Unknown),
        ] {
            assert_eq!(
                serde_json::from_str::<SentCredential>(json).unwrap(),
                expected
            );
        }
        assert_eq!(
            serde_json::to_string(&SentCredential::Missing).unwrap(),
            "\"missing\""
        );
        assert!(SentCredential::SameAsCurrent.is_sent());
        assert!(SentCredential::DifferentFromCurrent.is_sent());
        assert!(SentCredential::CurrentUnavailable.is_sent());
        assert!(!SentCredential::Missing.is_sent());
        assert!(!SentCredential::Unknown.is_sent());
    }

    #[test]
    fn rate_limited_api_error_is_detected() {
        let err = SamplingError::Api {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "Rate limit exceeded".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(err.is_rate_limited());
        assert!(err.is_retryable(), "429 should be retryable");
        assert!(!err.is_auth_error());
        assert!(!err.is_payload_too_large());
    }

    #[test]
    fn non_rate_limit_errors_are_not_rate_limited() {
        let server_error = SamplingError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(!server_error.is_rate_limited());

        let auth_error = SamplingError::auth_unknown("bad key");
        assert!(!auth_error.is_rate_limited());

        let timeout = SamplingError::IdleTimeout { elapsed_secs: 30 };
        assert!(!timeout.is_rate_limited());
    }

    #[test]
    fn retry_after_returns_header_value() {
        let err = SamplingError::Api {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "slow down".into(),
            model_metadata: None,
            retry_after_secs: Some(42),
            should_retry: None,
        };
        assert_eq!(err.retry_after(), Some(42));
    }

    #[test]
    fn retry_after_returns_none_when_absent() {
        let err = SamplingError::Api {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "slow down".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn retry_after_returns_none_for_non_api_errors() {
        assert_eq!(SamplingError::auth_unknown("x").retry_after(), None);
        assert_eq!(
            SamplingError::IdleTimeout { elapsed_secs: 10 }.retry_after(),
            None
        );
    }

    #[test]
    fn encrypted_content_400_is_detected() {
        let err = SamplingError::Api {
            status: StatusCode::BAD_REQUEST,
            message: "Could not decrypt the provided encrypted_content. Ensure the value is the unmodified encrypted_content from a previous response.".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(err.is_encrypted_content_error());
        assert!(
            !err.is_retryable(),
            "encrypted_content errors must not be retried"
        );
    }

    #[test]
    fn encrypted_content_wrong_status_not_detected() {
        let err = SamplingError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "encrypted_content decryption failed".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(
            !err.is_encrypted_content_error(),
            "only 400 should match, not 500"
        );
    }

    #[test]
    fn encrypted_content_unrelated_400_not_detected() {
        let err = SamplingError::Api {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid model parameter".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(
            !err.is_encrypted_content_error(),
            "unrelated 400 errors must not match"
        );
    }

    #[test]
    fn image_processing_error_direct_400_detected() {
        let err = SamplingError::Api {
            status: StatusCode::BAD_REQUEST,
            message: "Could not process image: unsupported format".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(err.is_image_processing_error());
        assert!(!err.is_encrypted_content_error());
    }

    #[test]
    fn image_processing_error_500_wrapped_detected() {
        let err = SamplingError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "upstream error: 400 Bad Request: Could not process image".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(err.is_image_processing_error());
    }

    #[test]
    fn image_processing_error_unrelated_400_not_detected() {
        let err = SamplingError::Api {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid model parameter".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(!err.is_image_processing_error());
    }

    #[test]
    fn image_processing_error_unrelated_500_not_detected() {
        let err = SamplingError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal server error".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(!err.is_image_processing_error());
    }

    #[test]
    fn image_processing_error_wrong_status_not_detected() {
        let err = SamplingError::Api {
            status: StatusCode::BAD_GATEWAY,
            message: "Could not process image".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(
            !err.is_image_processing_error(),
            "only 400 and 500 should match"
        );
    }

    #[test]
    fn image_processing_error_400_is_not_retryable_standalone() {
        let err = SamplingError::Api {
            status: StatusCode::BAD_REQUEST,
            message: "Could not process image".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        };
        assert!(
            !err.is_retryable(),
            "direct 400 must not be retryable by is_retryable()"
        );
    }

    fn api_status_err(code: u16) -> SamplingError {
        SamplingError::Api {
            status: StatusCode::from_u16(code).unwrap(),
            message: status_user_message(StatusCode::from_u16(code).unwrap()),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
        }
    }

    #[test]
    fn transient_5xx_is_retryable_but_origin_tls_is_not() {
        // Cloudflare edge pages (520-524, 530), upstream overload (529), and
        // non-CF 5xx like 501/507 — the rule is any 5xx, not a code list.
        for code in [501u16, 507, 520, 521, 522, 523, 524, 529, 530] {
            assert!(
                api_status_err(code).is_retryable(),
                "{code} must be retried"
            );
        }
        // Origin TLS: a broken certificate never clears on its own.
        for code in [525u16, 526] {
            assert!(
                !api_status_err(code).is_retryable(),
                "origin-TLS {code} must not be retried"
            );
        }
    }
}
