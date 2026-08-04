//! Secret-free 401 attribution for shell-owned HTTP consumers.
//!
//! Credential bytes never cross callback boundaries or enter logs, spans,
//! telemetry, or support exports. HTTP clients compare the exact credential
//! used by their final request attempt with one current credential snapshot and
//! pass only [`CredentialComparison`] into this module.

use std::sync::Arc;

use serde_json::Value as JsonValue;
use xai_grok_auth::{CredentialComparison, SentCredentialRelation};
use xai_grok_sampler::{Auth401AttributionCallback, SamplingConsumer};
use xai_grok_telemetry::unified_log::CredentialDiagnosticConsumer;
use xai_grok_tools::{Auth401AttributionCallback as ToolAuth401AttributionCallback, ToolConsumer};

use crate::auth::{AuthManager, TOKEN_TTL};

#[cfg(test)]
static EMIT_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn test_emit_count() -> u64 {
    EMIT_COUNT.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn reset_test_emit_count() {
    EMIT_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) struct ShellAttribution {
    auth_manager: Arc<AuthManager>,
    session_id: Option<String>,
}

impl std::fmt::Debug for ShellAttribution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellAttribution")
            .field("auth_manager", &"<configured>")
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl ShellAttribution {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        auth_manager: Arc<AuthManager>,
        session_id: Option<String>,
    ) -> Arc<dyn Auth401AttributionCallback> {
        Arc::new(Self {
            auth_manager,
            session_id,
        })
    }

    /// Tool-side counterpart of [`Self::new`]: returns
    /// `Arc<dyn xai_grok_tools::Auth401AttributionCallback>` for the
    /// `with_attribution_callback(...)` builder on each tool HTTP
    /// client (`ImageGenClient`, `VideoGenClient`, `WebSearchClient`).
    /// The two callbacks share the same underlying impl and emit the
    /// same `auth_401_attribution` event format -- only the trait
    /// signature differs (`SamplingConsumer` vs. `ToolConsumer`).
    pub(crate) fn new_tool_callback(
        auth_manager: Arc<AuthManager>,
        session_id: Option<String>,
    ) -> Arc<dyn ToolAuth401AttributionCallback> {
        Arc::new(Self {
            auth_manager,
            session_id,
        })
    }
}

impl Auth401AttributionCallback for ShellAttribution {
    fn record_401(&self, consumer: SamplingConsumer, comparison: CredentialComparison) {
        let consumer = sampling_diagnostic_consumer(consumer);
        record_consumer_401(
            self.auth_manager.as_ref(),
            self.session_id.as_deref(),
            consumer,
            comparison,
        );
    }
}

impl ToolAuth401AttributionCallback for ShellAttribution {
    fn record_401(&self, consumer: ToolConsumer, comparison: CredentialComparison) {
        let consumer = tool_diagnostic_consumer(consumer);
        record_consumer_401(
            self.auth_manager.as_ref(),
            self.session_id.as_deref(),
            consumer,
            comparison,
        );
    }
}

fn sampling_diagnostic_consumer(consumer: SamplingConsumer) -> CredentialDiagnosticConsumer {
    match consumer {
        SamplingConsumer::ChatCompletionsStream => {
            CredentialDiagnosticConsumer::OaiCompatChatCompletionsStream
        }
        SamplingConsumer::ChatCompletions => CredentialDiagnosticConsumer::OaiCompatChatCompletions,
        SamplingConsumer::ResponsesStream => CredentialDiagnosticConsumer::OaiCompatResponsesStream,
        SamplingConsumer::Responses => CredentialDiagnosticConsumer::OaiCompatResponses,
        SamplingConsumer::MessagesStream => CredentialDiagnosticConsumer::OaiCompatMessagesStream,
        SamplingConsumer::Messages => CredentialDiagnosticConsumer::OaiCompatMessages,
    }
}

fn tool_diagnostic_consumer(consumer: ToolConsumer) -> CredentialDiagnosticConsumer {
    match consumer {
        ToolConsumer::ImageGen => CredentialDiagnosticConsumer::ImageGen,
        ToolConsumer::VideoGenStart => CredentialDiagnosticConsumer::VideoGenStart,
        ToolConsumer::VideoGenPoll => CredentialDiagnosticConsumer::VideoGenPoll,
        ToolConsumer::WebSearch => CredentialDiagnosticConsumer::WebSearch,
    }
}

pub(crate) fn record_consumer_401(
    auth_manager: &AuthManager,
    session_id: Option<&str>,
    consumer: CredentialDiagnosticConsumer,
    comparison: CredentialComparison,
) {
    record_auth_401(auth_manager, session_id, consumer, comparison);
}

pub(crate) fn record_auth_401(
    auth_manager: &AuthManager,
    session_id: Option<&str>,
    consumer: CredentialDiagnosticConsumer,
    comparison: CredentialComparison,
) {
    let payload = compute_attribution_payload(auth_manager, consumer, comparison);

    xai_grok_telemetry::unified_log::emit_credential_attribution(
        consumer,
        comparison,
        payload["mint_age_seconds"].as_i64().unwrap_or(-1),
        payload["expires_at_seconds_from_now"].as_i64().unwrap_or(0),
        session_id,
    );

    let _attribution_span = tracing::warn_span!(
        "auth_401_attribution",
        sent_credential_relation = payload["sent_credential_relation"]
            .as_str()
            .unwrap_or("current_unavailable"),
        sent_credential_present = payload["sent_credential_present"]
            .as_bool()
            .unwrap_or(false),
        current_credential_present = payload["current_credential_present"]
            .as_bool()
            .unwrap_or(false),
        consumer = consumer.as_str(),
        session_id = session_id.unwrap_or(""),
        mint_age_seconds = payload["mint_age_seconds"].as_i64().unwrap_or(-1),
        expires_at_seconds_from_now = payload["expires_at_seconds_from_now"].as_i64().unwrap_or(0),
        is_stale_snapshot = payload["is_stale_snapshot"].as_bool().unwrap_or(false),
    )
    .entered();

    #[cfg(test)]
    EMIT_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

fn compute_attribution_payload(
    auth_manager: &AuthManager,
    consumer: CredentialDiagnosticConsumer,
    comparison: CredentialComparison,
) -> JsonValue {
    let now = chrono::Utc::now();
    let current_auth = auth_manager.current_or_expired();
    let (mint_age_seconds, expires_at_seconds_from_now) = match current_auth {
        Some(auth) => {
            let mint_age = now.signed_duration_since(auth.create_time).num_seconds();
            let expiry = auth.expires_at.unwrap_or(auth.create_time + TOKEN_TTL);
            (mint_age, expiry.signed_duration_since(now).num_seconds())
        }
        None => (-1_i64, 0_i64),
    };

    serde_json::json!({
        "sent_credential_relation": comparison.relation.as_str(),
        "sent_credential_present": comparison.sent_credential_present(),
        "current_credential_present": comparison.current_credential_present,
        "mint_age_seconds": mint_age_seconds,
        "expires_at_seconds_from_now": expires_at_seconds_from_now,
        "consumer": consumer.as_str(),
        "is_stale_snapshot": comparison.relation == SentCredentialRelation::DifferentFromCurrent,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{Duration, Utc};

    use crate::auth::{AuthManager, GrokAuth, GrokComConfig};

    use super::*;

    fn empty_auth_manager() -> (tempfile::TempDir, AuthManager) {
        let dir = tempfile::tempdir().expect("tempdir");
        let am = AuthManager::new(dir.path(), GrokComConfig::default());
        (dir, am)
    }

    fn fresh_auth(key: &str) -> GrokAuth {
        GrokAuth {
            key: key.to_string(),
            create_time: Utc::now(),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            ..GrokAuth::test_default()
        }
    }

    fn payload_field<'a>(payload: &'a JsonValue, key: &str) -> &'a JsonValue {
        payload
            .get(key)
            .unwrap_or_else(|| panic!("payload missing field {key:?}: {payload:?}"))
    }

    #[test]
    fn relation_matrix_is_secret_free() {
        let (_dir, am) = empty_auth_manager();
        am.hot_swap(fresh_auth("GB002-current-secret-0123456789"));

        let cases = [
            (CredentialComparison::not_sent(true), "not_sent", false),
            (
                CredentialComparison::current_unavailable(),
                "current_unavailable",
                false,
            ),
            (
                CredentialComparison::same_as_current(),
                "same_as_current",
                false,
            ),
            (
                CredentialComparison::different_from_current(),
                "different_from_current",
                true,
            ),
        ];

        for (comparison, relation, stale) in cases {
            let payload = compute_attribution_payload(
                &am,
                CredentialDiagnosticConsumer::OaiCompatMessages,
                comparison,
            );
            assert_eq!(
                payload_field(&payload, "sent_credential_relation"),
                relation
            );
            assert_eq!(payload_field(&payload, "is_stale_snapshot"), stale);
            let rendered = payload.to_string();
            assert!(!rendered.contains("GB002-current-secret-0123456789"));
            for window in b"GB002-current-secret-0123456789".windows(8) {
                assert!(!rendered.contains(std::str::from_utf8(window).unwrap()));
            }
        }
    }

    #[test]
    fn hard_expired_age_metadata_remains_available_without_credential_bytes() {
        let (_dir, am) = empty_auth_manager();
        am.hot_swap(GrokAuth {
            key: "GB002-expired-secret-0123456789".into(),
            create_time: Utc::now() - Duration::hours(2),
            expires_at: Some(Utc::now() - Duration::hours(1)),
            ..GrokAuth::test_default()
        });

        let payload = compute_attribution_payload(
            &am,
            CredentialDiagnosticConsumer::OaiCompatMessages,
            CredentialComparison::same_as_current(),
        );
        let mint = payload_field(&payload, "mint_age_seconds")
            .as_i64()
            .unwrap();
        let expires = payload_field(&payload, "expires_at_seconds_from_now")
            .as_i64()
            .unwrap();
        assert!((7195..=7210).contains(&mint));
        assert!((-3610..=-3590).contains(&expires));
        assert!(!payload.to_string().contains("GB002-expired-secret"));
    }

    #[test]
    fn absent_current_uses_safe_sentinels() {
        let (_dir, am) = empty_auth_manager();
        let payload = compute_attribution_payload(
            &am,
            CredentialDiagnosticConsumer::OaiCompatMessages,
            CredentialComparison::current_unavailable(),
        );
        assert_eq!(payload_field(&payload, "mint_age_seconds"), -1);
        assert_eq!(payload_field(&payload, "expires_at_seconds_from_now"), 0);
        assert_eq!(payload_field(&payload, "current_credential_present"), false);
    }

    #[test]
    fn consumer_mapping_is_finite_and_secret_free() {
        let sampling_cases = [
            (
                SamplingConsumer::ChatCompletionsStream,
                "OaiCompatClient.chat_completions_stream",
            ),
            (
                SamplingConsumer::ChatCompletions,
                "OaiCompatClient.chat_completions",
            ),
            (
                SamplingConsumer::ResponsesStream,
                "OaiCompatClient.responses_stream",
            ),
            (SamplingConsumer::Responses, "OaiCompatClient.responses"),
            (
                SamplingConsumer::MessagesStream,
                "OaiCompatClient.messages_stream",
            ),
            (SamplingConsumer::Messages, "OaiCompatClient.messages"),
        ];
        for (consumer, expected) in sampling_cases {
            assert_eq!(sampling_diagnostic_consumer(consumer).as_str(), expected);
        }

        let tool_cases = [
            (ToolConsumer::ImageGen, "ImageGen"),
            (ToolConsumer::VideoGenStart, "VideoGen.start"),
            (ToolConsumer::VideoGenPoll, "VideoGen.poll"),
            (ToolConsumer::WebSearch, "WebSearch"),
        ];
        for (consumer, expected) in tool_cases {
            assert_eq!(tool_diagnostic_consumer(consumer).as_str(), expected);
        }
    }

    mod span_capture {
        use std::sync::Mutex;

        use tracing::Subscriber;
        use tracing::field::{Field, Visit};
        use tracing::span::Attributes;
        use tracing_subscriber::layer::{Context, Layer};
        use tracing_subscriber::registry::LookupSpan;

        #[derive(Debug, Default, Clone)]
        pub(crate) struct CapturedSpan {
            pub name: String,
            pub fields_str: std::collections::BTreeMap<String, String>,
            pub fields_i64: std::collections::BTreeMap<String, i64>,
            pub fields_bool: std::collections::BTreeMap<String, bool>,
        }

        pub(crate) struct SpanCollector {
            pub spans: std::sync::Arc<Mutex<Vec<CapturedSpan>>>,
        }

        impl SpanCollector {
            pub fn new() -> (Self, std::sync::Arc<Mutex<Vec<CapturedSpan>>>) {
                let spans = std::sync::Arc::new(Mutex::new(Vec::new()));
                (
                    Self {
                        spans: spans.clone(),
                    },
                    spans,
                )
            }
        }

        impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for SpanCollector {
            fn on_new_span(&self, attrs: &Attributes<'_>, _id: &tracing::Id, _ctx: Context<'_, S>) {
                let mut captured = CapturedSpan {
                    name: attrs.metadata().name().to_string(),
                    ..Default::default()
                };
                attrs.record(&mut FieldVisitor {
                    captured: &mut captured,
                });
                self.spans.lock().unwrap().push(captured);
            }
        }

        struct FieldVisitor<'a> {
            captured: &'a mut CapturedSpan,
        }

        impl Visit for FieldVisitor<'_> {
            fn record_str(&mut self, field: &Field, value: &str) {
                self.captured
                    .fields_str
                    .insert(field.name().to_string(), value.to_string());
            }

            fn record_i64(&mut self, field: &Field, value: i64) {
                self.captured
                    .fields_i64
                    .insert(field.name().to_string(), value);
            }

            fn record_u64(&mut self, field: &Field, value: u64) {
                self.record_i64(field, value as i64);
            }

            fn record_bool(&mut self, field: &Field, value: bool) {
                self.captured
                    .fields_bool
                    .insert(field.name().to_string(), value);
            }

            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.captured
                    .fields_str
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }
    }

    fn assert_secret_absent(rendered: &str, secret: &str) {
        assert!(!rendered.contains(secret), "leaked full secret: {rendered}");
        for window in secret.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).expect("ASCII sentinel window");
            assert!(
                !rendered.contains(window),
                "leaked secret window {window:?}: {rendered}"
            );
        }
    }

    #[test]
    #[serial_test::serial(attribution_emit_count)]
    fn record_auth_401_emits_safe_otel_span_with_attribution_fields() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let (collector, captured) = span_capture::SpanCollector::new();
        let _guard = tracing_subscriber::registry().with(collector).set_default();

        reset_test_emit_count();
        let access_secret = "GB002-access-secret-0123456789abcdef";
        let current_secret = "GB002-current-secret-fedcba9876543210";
        let (_dir, am) = empty_auth_manager();
        am.hot_swap(fresh_auth(current_secret));
        let comparison = CredentialComparison::compare(Some(access_secret), Some(current_secret));

        record_auth_401(
            &am,
            Some("sid-otel-span"),
            CredentialDiagnosticConsumer::OaiCompatChatCompletionsStream,
            comparison,
        );

        let spans = captured.lock().unwrap();
        let attribution = spans
            .iter()
            .find(|span| span.name == "auth_401_attribution")
            .expect("expected one auth_401_attribution span");

        assert_eq!(
            attribution
                .fields_str
                .get("sent_credential_relation")
                .map(String::as_str),
            Some("different_from_current")
        );
        assert_eq!(
            attribution.fields_str.get("consumer").map(String::as_str),
            Some("OaiCompatClient.chat_completions_stream")
        );
        assert_eq!(
            attribution.fields_str.get("session_id").map(String::as_str),
            Some("sid-otel-span")
        );
        assert_eq!(
            attribution.fields_bool.get("sent_credential_present"),
            Some(&true)
        );
        assert_eq!(
            attribution.fields_bool.get("current_credential_present"),
            Some(&true)
        );
        assert_eq!(
            attribution.fields_bool.get("is_stale_snapshot"),
            Some(&true)
        );
        let mint = attribution.fields_i64["mint_age_seconds"];
        assert!((0..5).contains(&mint), "unexpected mint age: {mint}");
        let expires = attribution.fields_i64["expires_at_seconds_from_now"];
        assert!(
            (3590..=3600).contains(&expires),
            "unexpected expiry delta: {expires}"
        );

        let rendered = format!("{attribution:?}");
        assert_secret_absent(&rendered, access_secret);
        assert_secret_absent(&rendered, current_secret);
        assert_eq!(test_emit_count(), 1);
    }

    #[test]
    #[serial_test::serial(attribution_emit_count)]
    fn callbacks_route_safe_comparisons() {
        reset_test_emit_count();
        let (_dir, am) = empty_auth_manager();
        let am = Arc::new(am);
        let sampler = ShellAttribution::new(am.clone(), Some("sid".into()));
        sampler.record_401(
            SamplingConsumer::Messages,
            CredentialComparison::same_as_current(),
        );
        let tools = ShellAttribution::new_tool_callback(am, Some("sid".into()));
        tools.record_401(
            ToolConsumer::ImageGen,
            CredentialComparison::different_from_current(),
        );
        assert_eq!(test_emit_count(), 2);
    }

    #[test]
    #[serial_test::serial(attribution_emit_count)]
    fn sampler_callback_survives_arc_clone_and_routes_every_variant() {
        reset_test_emit_count();
        let (_dir, am) = empty_auth_manager();
        let callback = ShellAttribution::new(Arc::new(am), Some("parent-sid".into()));
        let inherited = callback.clone();
        let consumers = [
            SamplingConsumer::ChatCompletionsStream,
            SamplingConsumer::ChatCompletions,
            SamplingConsumer::ResponsesStream,
            SamplingConsumer::Responses,
            SamplingConsumer::MessagesStream,
            SamplingConsumer::Messages,
        ];

        for consumer in consumers {
            inherited.record_401(consumer, CredentialComparison::different_from_current());
        }
        callback.record_401(
            SamplingConsumer::Messages,
            CredentialComparison::same_as_current(),
        );

        assert_eq!(test_emit_count(), consumers.len() as u64 + 1);
    }

    #[test]
    #[serial_test::serial(attribution_emit_count)]
    fn record_auth_401_bumps_emit_counter() {
        reset_test_emit_count();
        let (_dir, am) = empty_auth_manager();
        record_auth_401(
            &am,
            None,
            CredentialDiagnosticConsumer::OaiCompatMessages,
            CredentialComparison::not_sent(false),
        );
        assert_eq!(test_emit_count(), 1);
    }
}
