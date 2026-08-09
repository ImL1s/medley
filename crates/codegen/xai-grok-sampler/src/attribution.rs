//! 401 attribution hook for the sampling client.
//!
//! The caller wires an [`Auth401AttributionCallback`] into
//! [`crate::SamplerConfig::attribution_callback`]; the sampler invokes it at
//! each UNAUTHORIZED arm with the bearer fragment that went on the wire, so an
//! observer can split "sent a stale snapshot" from "sent the live token and
//! was still rejected". `None` (the default) makes the 401 sites silent.
//!
//! `xai-grok-sampler` is intentionally decoupled from `xai-grok-shell`
//! (no shell types, no logging crate, no auth-manager dependency). The
//! caller wires an implementation of [`Auth401AttributionCallback`]
//! into [`crate::SamplerConfig::attribution_callback`]; the sampler
//! invokes the callback at each UNAUTHORIZED arm with only the secret-free
//! relationship between the credential sent on the final request and a fresh
//! current-provider snapshot. Credential bytes never cross this boundary.
//!
//! When the callback is `None` (the default), the 401 sites are silent
//! and return the same `SamplingError::Auth` they would otherwise.

use std::sync::Arc;
use xai_grok_auth::CredentialComparison;

pub use xai_grok_auth::bearer_fragment::BEARER_SUFFIX_LEN;

/// A 401-emitting site in [`crate::SamplingClient`]; its string identifier
/// becomes the `consumer` field so queries can break 401s down by API path.
/// Sampler endpoints only — tool clients use `xai_grok_tools::ToolConsumer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingConsumer {
    /// `chat_completion_stream`: OpenAI-compatible streaming OpenAI Chat Completions API.
    ChatCompletionsStream,
    /// `chat_completion`: OpenAI-compatible non-streaming OpenAI Chat Completions API.
    ChatCompletions,
    /// `create_response_stream`: Responses API streaming.
    ResponsesStream,
    /// `create_response`: Responses API non-streaming.
    Responses,
    /// `messages_stream`: Anthropic Messages API streaming.
    MessagesStream,
    /// `messages`: Anthropic Messages API non-streaming.
    Messages,
}

impl SamplingConsumer {
    /// Stable string identifier for this emit site. Callbacks
    /// typically combine this with a fixed prefix (e.g. the client
    /// type) when building the consumer field of the attribution
    /// event.
    pub fn as_endpoint(self) -> &'static str {
        match self {
            Self::ChatCompletionsStream => "chat_completions_stream",
            Self::ChatCompletions => "chat_completions",
            Self::ResponsesStream => "responses_stream",
            Self::Responses => "responses",
            Self::MessagesStream => "messages_stream",
            Self::Messages => "messages",
        }
    }
}

/// Hook invoked by [`crate::SamplingClient`] at every 401 response site.
///
/// Implementations receive only a secret-free final-attempt comparison.
///
/// Implementations must be cheap to invoke and must not block. They
/// run inside the request's response-handling path and any latency
/// they add is paid by the user-visible 401 error path.
//
// The `Debug` bound keeps callback containers inspectable; SamplerConfig's
// manual Debug implementation reports only whether a callback is configured.
pub trait Auth401AttributionCallback: Send + Sync + std::fmt::Debug {
    /// Record a 401 attribution event for one logical 401 response.
    ///
    /// The comparison contains no credential bytes and describes the exact
    /// final headers after all injectors and auth-stripping steps.
    fn record_401(&self, consumer: SamplingConsumer, comparison: CredentialComparison);
}

/// Shared, cheap-to-clone alias for the attribution callback.
pub type SharedAttributionCallback = Arc<dyn Auth401AttributionCallback>;
