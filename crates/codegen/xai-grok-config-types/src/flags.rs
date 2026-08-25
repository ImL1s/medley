//! Config-value resolution leaf types and per-model laziness config,
//! extracted from xai-grok-shell for dependency inversion.

use xai_grok_config::env_bool;

/// Where a resolved config value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum ConfigSource {
    Requirement,
    Cli,
    Env,
    SystemManagedConfig,
    ManagedConfig,
    UserConfig,
    Config,
    Remote,
    Default,
}

/// A resolved config value with its source for diagnostics.
#[derive(Debug, Clone)]
pub struct Resolved<T> {
    pub value: T,
    pub source: ConfigSource,
}

impl<T> Resolved<T> {
    pub fn new(value: T, source: ConfigSource) -> Self {
        Self { value, source }
    }
}

impl<T: std::fmt::Display> std::fmt::Display for Resolved<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.value, self.source)
    }
}
/// Resolve a boolean feature flag: requirement > cli > env > config > managed > feature flag > default.
pub struct BoolFlag<'a> {
    requirement: Option<bool>,
    cli: Option<bool>,
    env_var: &'a str,
    // `MEDLEY_*` alias checked ahead of `env_var`, for the documented
    // user-facing set enumerated in #426. `None` for every flag not in that
    // set — see `env_alias()` below.
    medley_env_var: Option<&'a str>,
    config: Option<bool>,
    managed: Option<bool>,
    feature_flag: Option<bool>,
    default: bool,
}

impl<'a> BoolFlag<'a> {
    pub fn env(env_var: &'a str) -> Self {
        Self {
            requirement: None,
            cli: None,
            env_var,
            medley_env_var: None,
            config: None,
            managed: None,
            feature_flag: None,
            default: false,
        }
    }

    pub fn requirement(mut self, v: Option<bool>) -> Self {
        self.requirement = v;
        self
    }
    pub fn cli(mut self, v: Option<bool>) -> Self {
        self.cli = v;
        self
    }
    pub fn config(mut self, v: Option<bool>) -> Self {
        self.config = v;
        self
    }
    pub fn managed(mut self, v: Option<bool>) -> Self {
        self.managed = v;
        self
    }
    pub fn feature_flag(mut self, v: Option<bool>) -> Self {
        self.feature_flag = v;
        self
    }
    pub fn default(mut self, v: bool) -> Self {
        self.default = v;
        self
    }
    /// Opt this flag into `MEDLEY_*`-first precedence against its `GROK_*`
    /// env var (#426). Only call this for a name in the fork's documented
    /// user-facing alias set — every other `BoolFlag` caller must stay
    /// `GROK_*`-only, so leave this unset for them.
    pub fn env_alias(mut self, medley_env_var: &'a str) -> Self {
        self.medley_env_var = Some(medley_env_var);
        self
    }

    pub fn resolve(self) -> Resolved<bool> {
        resolve_bool_flag(
            self.requirement,
            self.cli,
            self.env_var,
            self.medley_env_var,
            self.config,
            self.managed,
            self.feature_flag,
            self.default,
        )
    }
}

fn resolve_bool_flag(
    requirement: Option<bool>,
    cli_arg: Option<bool>,
    env_var: &str,
    medley_env_var: Option<&str>,
    config_val: Option<bool>,
    managed_val: Option<bool>,
    feature_flag_val: Option<bool>,
    default: bool,
) -> Resolved<bool> {
    if let Some(val) = requirement {
        return Resolved::new(val, ConfigSource::Requirement);
    }
    if let Some(val) = cli_arg {
        return Resolved::new(val, ConfigSource::Cli);
    }
    let env_val = match medley_env_var {
        Some(medley) => xai_grok_config::resolve_env_bool(medley, env_var),
        None => env_bool(env_var),
    };
    if let Some(val) = env_val {
        return Resolved::new(val, ConfigSource::Env);
    }
    if let Some(val) = config_val {
        return Resolved::new(val, ConfigSource::Config);
    }
    if let Some(val) = managed_val {
        return Resolved::new(val, ConfigSource::ManagedConfig);
    }
    if let Some(val) = feature_flag_val {
        return Resolved::new(val, ConfigSource::Remote);
    }
    Resolved::new(default, ConfigSource::Default)
}

#[cfg(test)]
mod env_alias_tests {
    use super::*;

    /// Test-only var names, distinct from anything real code reads AND from
    /// each other (cargo runs `#[test]` fns in the same binary concurrently
    /// by default, and this crate has no `#[serial]` infrastructure) — a
    /// shared pair across tests was tried first and flaked exactly this way.
    struct EnvGuard {
        medley: &'static str,
        grok: &'static str,
    }
    impl EnvGuard {
        fn set(
            medley: &'static str,
            grok: &'static str,
            medley_val: Option<&str>,
            grok_val: Option<&str>,
        ) -> Self {
            unsafe {
                match medley_val {
                    Some(v) => std::env::set_var(medley, v),
                    None => std::env::remove_var(medley),
                }
                match grok_val {
                    Some(v) => std::env::set_var(grok, v),
                    None => std::env::remove_var(grok),
                }
            }
            Self { medley, grok }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(self.medley);
                std::env::remove_var(self.grok);
            }
        }
    }

    #[test]
    fn env_alias_prefers_medley_over_grok() {
        const MEDLEY_VAR: &str = "MEDLEY_TEST_BOOLFLAG_ALIAS_PREFERS_MEDLEY";
        const GROK_VAR: &str = "GROK_TEST_BOOLFLAG_ALIAS_PREFERS_MEDLEY";
        let _guard = EnvGuard::set(MEDLEY_VAR, GROK_VAR, Some("1"), Some("0"));
        let resolved = BoolFlag::env(GROK_VAR).env_alias(MEDLEY_VAR).resolve();
        assert!(
            resolved.value,
            "MEDLEY_* must win over a conflicting GROK_*"
        );
        assert_eq!(resolved.source, ConfigSource::Env);
    }

    #[test]
    fn env_alias_falls_back_to_grok_when_medley_unset() {
        const MEDLEY_VAR: &str = "MEDLEY_TEST_BOOLFLAG_ALIAS_FALLS_BACK";
        const GROK_VAR: &str = "GROK_TEST_BOOLFLAG_ALIAS_FALLS_BACK";
        let _guard = EnvGuard::set(MEDLEY_VAR, GROK_VAR, None, Some("1"));
        let resolved = BoolFlag::env(GROK_VAR).env_alias(MEDLEY_VAR).resolve();
        assert!(
            resolved.value,
            "GROK_* must still work when MEDLEY_* is unset"
        );
        assert_eq!(resolved.source, ConfigSource::Env);
    }

    #[test]
    fn no_env_alias_is_unaffected_by_a_stray_medley_var() {
        const MEDLEY_VAR: &str = "MEDLEY_TEST_BOOLFLAG_ALIAS_UNAFFECTED";
        const GROK_VAR: &str = "GROK_TEST_BOOLFLAG_ALIAS_UNAFFECTED";
        let _guard = EnvGuard::set(MEDLEY_VAR, GROK_VAR, Some("1"), Some("0"));
        // Every `BoolFlag` caller that never opts in via `.env_alias(...)`
        // must behave exactly as before: only GROK_VAR is consulted.
        let resolved = BoolFlag::env(GROK_VAR).resolve();
        assert!(
            !resolved.value,
            "an unaliased flag must ignore MEDLEY_VAR entirely"
        );
        assert_eq!(resolved.source, ConfigSource::Env);
    }
}

/// Per-model configuration for the Layer-3 LazinessDetector.
///
/// All fields default to the disabled state. Activation is a deliberate
/// two-step opt-in: setting `enabled = true` lets the classifier fire
/// (and emit `LazinessClassifierFired` telemetry), but a nudge is only
/// injected when `max_nudges_per_session > 0` as well. This makes
/// observation-only rollout (classify-but-don't-act) the natural
/// intermediate state.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LazinessDetectorPerModelConfig {
    /// Master switch. When `false` (the default), the classifier never
    /// fires for this model and no per-classification cost is incurred.
    #[serde(default)]
    pub enabled: bool,
    /// Hard cap on `<system-reminder>` nudges injected per session for
    /// this model. Default `0` makes `enabled = true` alone an
    /// observation-only mode (classifier fires, no nudges).
    #[serde(default)]
    pub max_nudges_per_session: u32,
    /// How long the session must be idle before the classifier runs.
    /// `None` defers to the harness default (10 seconds).
    #[serde(default)]
    pub idle_threshold_ms: Option<u64>,
    /// Minimum classifier confidence required to inject a nudge. `None`
    /// defers to the harness default (0.7).
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// When `Some(true)` (or `None` — the default), the classifier sees
    /// the assistant's plain-text reasoning as `[assistant reasoning]`
    /// lines. `Some(false)` drops them (the pre-2026-05 behavior).
    /// `None` defers to the harness default (`LAZINESS_INCLUDE_REASONING`,
    /// currently `true`).
    #[serde(default)]
    pub include_reasoning: Option<bool>,
}
