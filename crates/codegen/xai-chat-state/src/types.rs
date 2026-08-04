//! Shared domain types for the chat state actor.

use std::collections::BTreeSet;
use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};
use xai_grok_sampling_types::{ConversationItem, SamplingConfig};

/// Canonical marker for an injected memory-context block. Shared by the
/// emitter in `xai-grok-shell` and the upsert/detection here — a drift would
/// silently break dedup and let blocks accumulate in the prompt prefix.
/// Detection assumes the literal never appears in a system prompt except as
/// an injected block.
pub const MEMORY_CONTEXT_OPEN_TAG: &str = "<memory-context>";

/// Closing tag paired with [`MEMORY_CONTEXT_OPEN_TAG`].
pub const MEMORY_CONTEXT_CLOSE_TAG: &str = "</memory-context>";

/// Configuration for the ChatStateActor at spawn time.
#[derive(Debug, Clone)]
pub struct ChatStateConfig {
    /// Initial conversation items to populate the state with.
    pub initial_conversation: Vec<ConversationItem>,
    /// Sampling configuration (model, context window, etc.).
    pub sampling_config: SamplingConfig,
}

/// Immutable snapshot of the actor's state (for forking, rewind).
#[derive(Clone, Serialize, Deserialize)]
pub struct ChatStateSnapshot {
    /// The full conversation history.
    pub conversation: Vec<ConversationItem>,
    /// Current sampling configuration.
    #[serde(with = "snapshot_sampling_config")]
    pub sampling_config: SamplingConfig,
    /// Current prompt index (incremented per user turn).
    pub prompt_index: usize,
    /// Accumulated token usage.
    pub total_tokens: u64,
    /// Bytes/4 estimate of the conversation as of the last `record_token_usage`.
    /// `0` means unknown (pre-field snapshot); restore re-estimates instead.
    #[serde(default)]
    pub estimate_at_last_response: u64,
    /// File paths the agent has edited.
    pub agent_edited_paths: BTreeSet<String>,
    /// Cached prompt texts for rewind preview.
    pub prompt_texts: Vec<String>,
    /// Timestamp when the current stream started (epoch ms).
    pub stream_start_ms: Option<i64>,
    /// Timestamp when the current turn started (epoch ms).
    pub turn_start_ms: Option<i64>,
    /// Prompt index at which the last compaction occurred.
    pub last_compaction_prompt_index: Option<usize>,
    /// Opaque runtime credentials. They participate in in-memory clone/restore,
    /// but are never serialized into snapshots or support/export payloads.
    #[serde(skip)]
    pub credentials: Credentials,
}

/// Snapshot serialization is an observability/export boundary, not a way to
/// persist request credentials. In-memory clone/restore keeps the complete
/// [`SamplingConfig`], while serialized snapshots retain only non-secret
/// settings and a presence marker for the endpoint.
mod snapshot_sampling_config {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use xai_grok_sampling_types::SamplingConfig;

    const CONFIGURED_ENDPOINT_MARKER: &str = "[configured]";

    pub fn serialize<S>(config: &SamplingConfig, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut safe = config.clone();
        safe.base_url = if config.base_url.is_empty() {
            String::new()
        } else {
            CONFIGURED_ENDPOINT_MARKER.to_owned()
        };
        safe.extra_headers.clear();
        safe.query_params.clear();
        safe.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SamplingConfig, D::Error>
    where
        D: Deserializer<'de>,
    {
        SamplingConfig::deserialize(deserializer)
    }
}

impl std::fmt::Debug for ChatStateSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatStateSnapshot")
            .field("conversation_len", &self.conversation.len())
            .field("model", &self.sampling_config.model)
            .field("prompt_index", &self.prompt_index)
            .field("total_tokens", &self.total_tokens)
            .field("estimate_at_last_response", &self.estimate_at_last_response)
            .field("agent_edited_paths_count", &self.agent_edited_paths.len())
            .field("prompt_texts_count", &self.prompt_texts.len())
            .field("stream_started", &self.stream_start_ms.is_some())
            .field("turn_started", &self.turn_start_ms.is_some())
            .field(
                "last_compaction_prompt_index",
                &self.last_compaction_prompt_index,
            )
            .field("credentials_present", &!self.credentials.is_empty())
            .finish()
    }
}

/// Metadata for session notifications (timing info).
#[derive(Debug, Clone)]
pub struct NotificationMeta {
    /// Timestamp when the current stream started (epoch ms).
    pub stream_start_ms: Option<i64>,
    /// Timestamp when the current turn started (epoch ms).
    pub turn_start_ms: Option<i64>,
}

/// Configuration for tool-result pruning.
///
/// Prunes old, large tool results from the conversation to reclaim context space.
/// Two modes: soft trim (keep head + tail) and hard clear (replace entirely).
#[derive(Debug, Clone)]
pub struct PruningConfig {
    /// Whether pruning is enabled.
    pub enabled: bool,
    /// Number of recent turns whose tool results are never pruned.
    pub keep_last_n_turns: usize,
    /// Character threshold above which old tool results are soft-trimmed.
    pub soft_trim_threshold: usize,
    /// Characters to keep from the start of a soft-trimmed result.
    pub soft_trim_head: usize,
    /// Characters to keep from the end of a soft-trimmed result.
    pub soft_trim_tail: usize,
    /// Turn age after which tool results are hard-cleared (replaced with placeholder).
    pub hard_clear_age_turns: usize,
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_last_n_turns: 3,
            soft_trim_threshold: 4000,
            soft_trim_head: 1500,
            soft_trim_tail: 1500,
            hard_clear_age_turns: 10,
        }
    }
}

/// Where the session's current api_key came from.
/// Determines whether the key can be refreshed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    /// From AuthManager (grok login, OIDC, external binary). Refreshable.
    #[default]
    SessionToken,
    /// From user config ([model.*] api_key, env_key, XAI_API_KEY). Not refreshable.
    ApiKey,
}

/// Credential/secret fields that the actor stores opaquely.
///
/// These are fields from the shell's full `Config` that aren't part of
/// `xai_grok_sampling_types::SamplingConfig`. Serialized snapshots redact that
/// type's endpoint, extra-header values, and query parameters separately.
/// The actor just stores and returns them — it never interprets them.
#[derive(Clone, Default, Deserialize)]
pub struct Credentials {
    /// API key for authentication.
    pub api_key: Option<String>,
    /// Whether this is a session token (refreshable) or user-provided api key.
    #[serde(default)]
    pub auth_type: AuthType,
    /// Optional extra auth material forwarded with requests when present.
    pub alpha_test_key: Option<String>,
    /// Client version string.
    pub client_version: Option<String>,
}

impl Credentials {
    fn is_empty(&self) -> bool {
        self.api_key.is_none() && self.alpha_test_key.is_none()
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key_present", &self.api_key.is_some())
            .field("auth_type", &self.auth_type)
            .field("alpha_test_key_present", &self.alpha_test_key.is_some())
            .field("client_version_present", &self.client_version.is_some())
            .finish()
    }
}

/// The messages captured during a single conversation turn.
///
/// Produced by `TakeTurnMessages` after a `BeginTurnCapture`/message-push cycle.
#[derive(Debug, Clone)]
pub struct TurnCapture {
    /// The ordered sequence of messages appended during this turn.
    pub messages: Vec<ConversationItem>,
    /// Whether compaction (conversation replacement) occurred mid-turn.
    pub compaction_occurred: bool,
}

/// Item counts for a conversation, broken down by role.
///
/// Returned by `get_conversation_counts()` — avoids cloning the conversation
/// when only role counts and total length are needed (e.g. for telemetry).
#[derive(Debug, Clone, Default)]
pub struct ConversationCounts {
    /// Total number of items in the conversation.
    pub total: usize,
    /// Number of `User` items.
    pub user: usize,
    /// Number of `Assistant` items.
    pub assistant: usize,
    /// Number of `ToolResult` items.
    pub tool_result: usize,
}

/// Info returned when auto-compact threshold is exceeded.
#[derive(Debug, Clone)]
pub struct AutoCompactTrigger {
    /// Current total token count.
    pub total_tokens: u64,
    /// Model's context window size.
    pub context_window: NonZeroU64,
    /// Current utilization as a percentage (0–100).
    pub utilization_percent: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_through_serde_json() {
        let api_key = "ZXQ91vLmN7pR4tK8sW2cY6hF0aD3uB5e";
        let alpha_key = "YWQ82mKoP6sT3rH9vN5bC1xE7fJ4uL0a";
        let snapshot = ChatStateSnapshot {
            conversation: vec![],
            sampling_config: SamplingConfig {
                base_url: format!("https://user:{api_key}@api.example.com/?token={api_key}"),
                model: "test-model".to_string(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: Default::default(),
                endpoint_trust: None,
                extra_headers: indexmap::IndexMap::from([(
                    "authorization".to_owned(),
                    format!("Bearer {alpha_key}"),
                )]),
                query_params: indexmap::IndexMap::from([(
                    "api_key".to_owned(),
                    api_key.to_owned(),
                )]),
                env_http_headers: Default::default(),
                context_window: NonZeroU64::new(128_000).unwrap(),
                reasoning_effort: None,
                stream_tool_calls: None,
            },
            prompt_index: 0,
            total_tokens: 0,
            estimate_at_last_response: 0,
            agent_edited_paths: BTreeSet::new(),
            prompt_texts: vec![],
            stream_start_ms: None,
            turn_start_ms: None,
            last_compaction_prompt_index: None,
            credentials: Credentials::default(),
        };

        let json = serde_json::to_string(&snapshot).expect("serialize");
        let deserialized: ChatStateSnapshot = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.prompt_index, 0);
        assert_eq!(deserialized.total_tokens, 0);
        assert!(deserialized.conversation.is_empty());
        assert!(deserialized.agent_edited_paths.is_empty());
        assert!(deserialized.last_compaction_prompt_index.is_none());
    }

    #[test]
    fn snapshot_round_trips_with_data() {
        use xai_grok_sampling_types::ConversationItem;

        let snapshot = ChatStateSnapshot {
            conversation: vec![
                ConversationItem::system("You are a helpful assistant."),
                ConversationItem::user("Hello!"),
                ConversationItem::assistant("Hi there!"),
            ],
            sampling_config: SamplingConfig {
                base_url: "https://api.example.com".to_string(),
                model: "grok-3".to_string(),
                max_completion_tokens: Some(4096),
                temperature: Some(0.7),
                top_p: None,
                api_backend: Default::default(),
                endpoint_trust: None,
                extra_headers: Default::default(),
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: NonZeroU64::new(128_000).unwrap(),
                reasoning_effort: None,
                stream_tool_calls: None,
            },
            prompt_index: 5,
            total_tokens: 1234,
            estimate_at_last_response: 900,
            agent_edited_paths: BTreeSet::from([
                "src/main.rs".to_string(),
                "src/lib.rs".to_string(),
            ]),
            prompt_texts: vec!["first prompt".to_string(), "second prompt".to_string()],
            stream_start_ms: Some(1234567890),
            turn_start_ms: Some(1234567800),
            last_compaction_prompt_index: Some(2),
            credentials: Credentials::default(),
        };

        let json = serde_json::to_string(&snapshot).expect("serialize");
        let deserialized: ChatStateSnapshot = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.prompt_index, 5);
        assert_eq!(deserialized.total_tokens, 1234);
        assert_eq!(deserialized.conversation.len(), 3);
        assert_eq!(deserialized.agent_edited_paths.len(), 2);
        assert_eq!(deserialized.prompt_texts.len(), 2);
        assert_eq!(deserialized.stream_start_ms, Some(1234567890));
        assert_eq!(deserialized.turn_start_ms, Some(1234567800));
        assert_eq!(deserialized.last_compaction_prompt_index, Some(2));
    }

    #[test]
    fn snapshot_credentials_are_runtime_only_and_debug_is_presence_only() {
        let api_key = "GB002-chat-access-Q7w5E3r1T9y7";
        let alpha_key = "GB002-chat-alpha-A7s5D3f1G9h7";
        let snapshot = ChatStateSnapshot {
            conversation: vec![],
            sampling_config: SamplingConfig {
                base_url: "https://api.example.com".to_string(),
                model: "test-model".to_string(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: Default::default(),
                endpoint_trust: None,
                extra_headers: Default::default(),
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: NonZeroU64::new(128_000).unwrap(),
                reasoning_effort: None,
                stream_tool_calls: None,
            },
            prompt_index: 0,
            total_tokens: 0,
            estimate_at_last_response: 0,
            agent_edited_paths: BTreeSet::new(),
            prompt_texts: vec![],
            stream_start_ms: None,
            turn_start_ms: None,
            last_compaction_prompt_index: None,
            credentials: Credentials {
                api_key: Some(api_key.to_owned()),
                auth_type: AuthType::ApiKey,
                alpha_test_key: Some(alpha_key.to_owned()),
                client_version: Some("test-client".to_owned()),
            },
        };

        let rendered = format!(
            "{:?}\n{}",
            snapshot,
            serde_json::to_string(&snapshot).expect("serialize secret-free snapshot")
        );
        assert!(rendered.contains("credentials_present: true"));
        assert!(!rendered.contains("credentials\""));
        for secret in [api_key, alpha_key] {
            assert!(!rendered.contains(secret));
            for window in secret.as_bytes().windows(8) {
                let window = std::str::from_utf8(window).expect("ASCII sentinel");
                assert!(!rendered.contains(window), "leaked secret window: {window}");
            }
        }

        let restored: ChatStateSnapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        assert!(restored.credentials.api_key.is_none());
        assert!(restored.credentials.alpha_test_key.is_none());
        assert_eq!(restored.sampling_config.base_url, "[configured]");
        assert!(restored.sampling_config.extra_headers.is_empty());
        assert!(restored.sampling_config.query_params.is_empty());
    }
}
