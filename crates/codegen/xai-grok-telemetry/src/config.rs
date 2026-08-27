//! Telemetry-engine configuration.
//!
//! Extracted from `xai-grok-shell::agent::config` so the data-collector
//! engine can construct a [`TelemetryClient`](crate::client::TelemetryClient)
//! without a build-time dependency on the shell.
//!
//! Shell still re-exports these types from their original paths so existing
//! call sites (and `Config` derive impls) compile unchanged.
use serde::{Deserialize, Serialize};
/// Telemetry mode: `true`/`false` (legacy bool) or `"session_metrics"` (string).
///
/// - `Disabled` -- nothing sent (enterprise default)
/// - `SessionMetrics` -- metadata-only lifecycle events, no content
/// - `Enabled` -- full product telemetry (events + Mixpanel)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TelemetryMode {
    #[default]
    Disabled,
    SessionMetrics,
    Enabled,
}
impl TelemetryMode {
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }
    /// True for both `SessionMetrics` and `Enabled`.
    pub fn session_metrics_enabled(&self) -> bool {
        matches!(self, Self::SessionMetrics | Self::Enabled)
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enabled" | "full" => Some(Self::Enabled),
            "0" | "false" | "no" | "off" | "disabled" => Some(Self::Disabled),
            "session-metrics" | "session_metrics" => Some(Self::SessionMetrics),
            _ => None,
        }
    }
}
impl std::fmt::Display for TelemetryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "false"),
            Self::SessionMetrics => write!(f, "session_metrics"),
            Self::Enabled => write!(f, "true"),
        }
    }
}
impl From<bool> for TelemetryMode {
    fn from(b: bool) -> Self {
        if b { Self::Enabled } else { Self::Disabled }
    }
}
impl serde::Serialize for TelemetryMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Disabled => serializer.serialize_bool(false),
            Self::Enabled => serializer.serialize_bool(true),
            Self::SessionMetrics => serializer.serialize_str("session_metrics"),
        }
    }
}
/// Wire format for `[features] telemetry`: accepts `true`, `false`, or `"session_metrics"`.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum TelemetryModeValue {
    Bool(bool),
    Str(String),
}
impl<'de> serde::Deserialize<'de> for TelemetryMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match TelemetryModeValue::deserialize(deserializer)? {
            TelemetryModeValue::Bool(b) => Ok(Self::from(b)),
            TelemetryModeValue::Str(s) => Ok(Self::parse(&s).unwrap_or_else(|| {
                tracing::warn!(
                    value = %s,
                    "TELEMETRY_MODE_UNKNOWN: unrecognized telemetry mode; treating as disabled",
                );
                Self::Disabled
            })),
        }
    }
}
/// Parse an env var as a `TelemetryMode`. Returns `None` if unset or empty.
pub fn env_telemetry_mode(name: &str) -> Option<TelemetryMode> {
    let value = std::env::var(name).ok()?;
    TelemetryMode::parse(&value)
}
/// [`env_telemetry_mode`] with `MEDLEY_*`-first precedence against `grok_name`
/// (#426) — a sibling rather than a parameter on `env_telemetry_mode` itself,
/// which stays untouched for every other caller.
pub fn env_telemetry_mode_alias(medley_name: &str, grok_name: &str) -> Option<TelemetryMode> {
    if let Ok(value) = std::env::var(medley_name)
        && !value.trim().is_empty()
    {
        return TelemetryMode::parse(&value);
    }
    let value = std::env::var(grok_name)
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let parsed = TelemetryMode::parse(&value)?;
    xai_grok_config::note_legacy_hit(grok_name);
    Some(parsed)
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// Declared for `serde_ignored`. Actual toggle is `[features] telemetry`.
    #[serde(default)]
    pub enabled: Option<bool>,
    pub events_url: Option<String>,
    pub events_api_key: Option<String>,
    pub mixpanel_token: Option<String>,
    pub mixpanel_enabled: bool,
    /// `None` = inherit from `[features] telemetry`. `Some(false)` = disable GCS uploads only.
    pub trace_upload: Option<bool>,
    /// External OTEL master switch (`= GROK_EXTERNAL_OTEL`, env wins).
    pub otel_enabled: Option<bool>,
    /// External OTEL metrics exporter: `otlp` | `console` | `none`.
    pub otel_metrics_exporter: Option<String>,
    /// External OTEL logs/events exporter: `otlp` | `console` | `none`.
    pub otel_logs_exporter: Option<String>,
    /// External OTLP base endpoint (`/v1/logs`, `/v1/metrics` appended for HTTP).
    pub otel_endpoint: Option<String>,
    /// External OTLP transport: `http/protobuf` | `grpc`.
    #[serde(alias = "otel_transport")]
    pub otel_protocol: Option<String>,
    /// External OTEL content gate (admins can pin to `false` via requirements).
    pub otel_log_user_prompts: Option<bool>,
    /// External OTEL content gate (admins can pin to `false` via requirements).
    pub otel_log_tool_details: Option<bool>,
}

impl std::fmt::Debug for TelemetryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryConfig")
            .field("enabled", &self.enabled)
            .field("events_url_present", &self.events_url.is_some())
            .field("events_api_key_present", &self.events_api_key.is_some())
            .field("mixpanel_token_present", &self.mixpanel_token.is_some())
            .field("mixpanel_enabled", &self.mixpanel_enabled)
            .field("trace_upload", &self.trace_upload)
            .field("otel_enabled", &self.otel_enabled)
            .field(
                "otel_metrics_exporter_present",
                &self.otel_metrics_exporter.is_some(),
            )
            .field(
                "otel_logs_exporter_present",
                &self.otel_logs_exporter.is_some(),
            )
            .field("otel_endpoint_present", &self.otel_endpoint.is_some())
            .field("otel_protocol_present", &self.otel_protocol.is_some())
            .field("otel_log_user_prompts", &self.otel_log_user_prompts)
            .field("otel_log_tool_details", &self.otel_log_tool_details)
            .finish()
    }
}
fn internal_defaults() -> (Option<String>, Option<String>, Option<String>, bool) {
    (None, None, None, false)
}
fn build_env_default(value: Option<&'static str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}
impl Default for TelemetryConfig {
    fn default() -> Self {
        let (baked_url, baked_key, baked_token, baked_enabled) = internal_defaults();
        let build_url = build_env_default(option_env!("GROK_TELEMETRY_BUILD_EVENTS_URL"));
        let build_key = build_env_default(option_env!("GROK_TELEMETRY_BUILD_EVENTS_API_KEY"));
        let build_token = build_env_default(option_env!("GROK_TELEMETRY_BUILD_MIXPANEL_TOKEN"));
        let mixpanel_enabled = baked_enabled || build_token.is_some();
        let (events_url, events_api_key, mixpanel_token) = (
            build_url.or(baked_url),
            build_key.or(baked_key),
            build_token.or(baked_token),
        );
        Self {
            enabled: None,
            events_url,
            events_api_key,
            mixpanel_token,
            mixpanel_enabled,
            trace_upload: None,
            otel_enabled: None,
            otel_metrics_exporter: None,
            otel_logs_exporter: None,
            otel_endpoint: None,
            otel_protocol: None,
            otel_log_user_prompts: None,
            otel_log_tool_details: None,
        }
    }
}
impl TelemetryConfig {
    pub fn apply_env_overrides(&mut self) {
        self.normalize();
        if let Some(value) = Self::env_override("GROK_TELEMETRY_EVENTS_URL") {
            self.events_url = value;
        }
        if let Some(value) = Self::env_override("GROK_TELEMETRY_EVENTS_API_KEY") {
            self.events_api_key = value;
        }
        if let Some(value) = Self::env_override("GROK_TELEMETRY_MIXPANEL_TOKEN") {
            self.mixpanel_token = value;
        }
        if let Some(value) = xai_grok_config::resolve_env_bool(
            "MEDLEY_TELEMETRY_MIXPANEL_ENABLED",
            "GROK_TELEMETRY_MIXPANEL_ENABLED",
        ) {
            self.mixpanel_enabled = value;
        }
        if let Some(value) = xai_grok_config::resolve_env_bool(
            "MEDLEY_TELEMETRY_TRACE_UPLOAD",
            "GROK_TELEMETRY_TRACE_UPLOAD",
        ) {
            self.trace_upload = Some(value);
        }
    }
    fn normalize(&mut self) {
        self.events_url = Self::normalize_optional_string(self.events_url.take());
        self.events_api_key = Self::normalize_optional_string(self.events_api_key.take());
        self.mixpanel_token = Self::normalize_optional_string(self.mixpanel_token.take());
    }
    fn env_override(name: &str) -> Option<Option<String>> {
        match std::env::var(name) {
            Ok(value) => Some(Self::normalize_optional_string(Some(value))),
            Err(_) => None,
        }
    }
    fn normalize_optional_string(value: Option<String>) -> Option<String> {
        value.and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
    }
}
// The local `env_bool` copy that used to live here (kept so this crate stayed
// free of a shell back-edge) is gone: its only two call sites now go through
// `xai_grok_config::resolve_env_bool` for the MEDLEY_*-first precedence
// (#426) — `xai-grok-config` is a foundational dependency this crate already
// has, not a back-edge, so the local copy has no remaining reason to exist.
#[cfg(test)]
mod tests {
    use super::*;

    fn assert_no_secret_fragments(output: &str, sentinel: &str) {
        assert!(!output.contains(sentinel));
        for window in sentinel.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !output.contains(fragment),
                "credential fragment {fragment:?} leaked in {output:?}"
            );
        }
    }

    /// `env_telemetry_mode_alias` takes both names as parameters, so unique
    /// per-test names avoid any risk of colliding with a concurrently-run
    /// test over shared process env — no lock needed.
    #[test]
    fn env_telemetry_mode_alias_prefers_medley() {
        const MEDLEY: &str = "MEDLEY_TEST_TELEMETRY_MODE_ALIAS_PREFERS";
        const GROK: &str = "GROK_TEST_TELEMETRY_MODE_ALIAS_PREFERS";
        unsafe {
            std::env::set_var(MEDLEY, "session_metrics");
            std::env::set_var(GROK, "true");
        }
        let result = env_telemetry_mode_alias(MEDLEY, GROK);
        unsafe {
            std::env::remove_var(MEDLEY);
            std::env::remove_var(GROK);
        }
        assert_eq!(result, Some(TelemetryMode::SessionMetrics));
    }

    #[test]
    fn env_telemetry_mode_alias_falls_back_to_grok() {
        const MEDLEY: &str = "MEDLEY_TEST_TELEMETRY_MODE_ALIAS_FALLBACK";
        const GROK: &str = "GROK_TEST_TELEMETRY_MODE_ALIAS_FALLBACK";
        unsafe {
            std::env::remove_var(MEDLEY);
            std::env::set_var(GROK, "true");
        }
        let result = env_telemetry_mode_alias(MEDLEY, GROK);
        unsafe { std::env::remove_var(GROK) };
        assert_eq!(result, Some(TelemetryMode::Enabled));
        let notice = xai_grok_config::legacy_notice().unwrap_or_default();
        assert!(
            notice.contains(GROK),
            "a parseable GROK_* value is a real hit, got {notice:?}"
        );
    }

    #[test]
    fn env_telemetry_mode_alias_invalid_legacy_is_not_recorded_as_a_hit() {
        const MEDLEY: &str = "MEDLEY_TEST_TELEMETRY_MODE_ALIAS_INVALID";
        const GROK: &str = "GROK_TEST_TELEMETRY_MODE_ALIAS_INVALID";
        unsafe {
            std::env::remove_var(MEDLEY);
            std::env::set_var(GROK, "maybe");
        }
        let result = env_telemetry_mode_alias(MEDLEY, GROK);
        unsafe { std::env::remove_var(GROK) };
        assert_eq!(result, None);
        let notice = xai_grok_config::legacy_notice().unwrap_or_default();
        assert!(
            !notice.contains(GROK),
            "an unparseable GROK_* value was not honored, got {notice:?}"
        );
    }

    /// `apply_env_overrides` reads the real, hardcoded var names, so both
    /// assertions live in one test — sequential, not concurrent, is what
    /// keeps this collision-free without a lock.
    #[test]
    fn apply_env_overrides_mixpanel_enabled_prefers_medley_then_grok() {
        unsafe {
            std::env::set_var("MEDLEY_TELEMETRY_MIXPANEL_ENABLED", "1");
            std::env::set_var("GROK_TELEMETRY_MIXPANEL_ENABLED", "0");
        }
        let mut cfg = TelemetryConfig::default();
        cfg.apply_env_overrides();
        assert!(
            cfg.mixpanel_enabled,
            "MEDLEY_* must win over a conflicting GROK_*"
        );

        unsafe {
            std::env::remove_var("MEDLEY_TELEMETRY_MIXPANEL_ENABLED");
            std::env::set_var("GROK_TELEMETRY_MIXPANEL_ENABLED", "1");
        }
        let mut cfg = TelemetryConfig::default();
        cfg.apply_env_overrides();
        assert!(
            cfg.mixpanel_enabled,
            "GROK_* must still work when MEDLEY_* is unset"
        );

        unsafe {
            std::env::remove_var("MEDLEY_TELEMETRY_MIXPANEL_ENABLED");
            std::env::remove_var("GROK_TELEMETRY_MIXPANEL_ENABLED");
        }
    }

    #[test]
    fn apply_env_overrides_trace_upload_prefers_medley_then_grok() {
        unsafe {
            std::env::set_var("MEDLEY_TELEMETRY_TRACE_UPLOAD", "0");
            std::env::set_var("GROK_TELEMETRY_TRACE_UPLOAD", "1");
        }
        let mut cfg = TelemetryConfig::default();
        cfg.apply_env_overrides();
        assert_eq!(
            cfg.trace_upload,
            Some(false),
            "MEDLEY_* must win over a conflicting GROK_*"
        );

        unsafe {
            std::env::remove_var("MEDLEY_TELEMETRY_TRACE_UPLOAD");
            std::env::set_var("GROK_TELEMETRY_TRACE_UPLOAD", "1");
        }
        let mut cfg = TelemetryConfig::default();
        cfg.apply_env_overrides();
        assert_eq!(
            cfg.trace_upload,
            Some(true),
            "GROK_* must still work when MEDLEY_* is unset"
        );

        unsafe {
            std::env::remove_var("MEDLEY_TELEMETRY_TRACE_UPLOAD");
            std::env::remove_var("GROK_TELEMETRY_TRACE_UPLOAD");
        }
    }

    #[test]
    fn build_env_default_normalizes() {
        assert_eq!(build_env_default(None), None);
        assert_eq!(build_env_default(Some("")), None);
        assert_eq!(build_env_default(Some(" \t ")), None);
        assert_eq!(build_env_default(Some(" key ")), Some("key".to_owned()));
    }
    #[test]
    fn default_is_build_env_layer_when_feature_off() {
        let cfg = TelemetryConfig::default();
        let url = build_env_default(option_env!("GROK_TELEMETRY_BUILD_EVENTS_URL"));
        let key = build_env_default(option_env!("GROK_TELEMETRY_BUILD_EVENTS_API_KEY"));
        let token = build_env_default(option_env!("GROK_TELEMETRY_BUILD_MIXPANEL_TOKEN"));
        assert_eq!(cfg.mixpanel_enabled, token.is_some());
        assert_eq!(cfg.events_url, url);
        assert_eq!(cfg.events_api_key, key);
        assert_eq!(cfg.mixpanel_token, token);
    }

    #[test]
    fn telemetry_config_debug_is_presence_only() {
        const EVENTS_KEY: &str = "GB002EVENTS-Q7w5E3r1T9y7Z6x4C2v8";
        const MIXPANEL_TOKEN: &str = "GB002MIXPANEL-A7s5D3f1G9h7J6k4L2m8";
        const EVENTS_URL: &str = "https://user:password@example.test/events";

        let config = TelemetryConfig {
            events_url: Some(EVENTS_URL.to_owned()),
            events_api_key: Some(EVENTS_KEY.to_owned()),
            mixpanel_token: Some(MIXPANEL_TOKEN.to_owned()),
            ..TelemetryConfig::default()
        };
        let output = format!("{config:?}");

        for sentinel in [EVENTS_KEY, MIXPANEL_TOKEN, EVENTS_URL] {
            assert_no_secret_fragments(&output, sentinel);
        }
        assert!(output.contains("events_url_present: true"));
        assert!(output.contains("events_api_key_present: true"));
        assert!(output.contains("mixpanel_token_present: true"));
    }
}
