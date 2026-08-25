/// Default auto-compact threshold (% of context window) when no source sets it.
pub const DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT: u8 = 85;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompactionToolChoice {
    #[default]
    Auto,
    None,
}

impl std::str::FromStr for CompactionToolChoice {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "none" => Ok(Self::None),
            _ => Err(()),
        }
    }
}

pub(crate) const ENV_COMPACTION_TOOL_CHOICE: &str = "GROK_COMPACTION_TOOL_CHOICE";

pub(crate) fn resolve_compaction_tool_choice_from(
    env: Option<&str>,
    config: Option<&str>,
    remote: Option<&str>,
) -> CompactionToolChoice {
    env.and_then(|s| s.parse().ok())
        .or_else(|| config.and_then(|s| s.parse().ok()))
        .or_else(|| remote.and_then(|s| s.parse().ok()))
        .unwrap_or_default()
}

/// Env-var override for `auto_compact_threshold_percent`. Parsed as `u8`;
/// out-of-range or unparseable values are ignored.
pub(crate) const ENV_AUTO_COMPACT_THRESHOLD_PERCENT: &str = "GROK_AUTO_COMPACT_THRESHOLD_PERCENT";

/// Resolve auto-compact threshold percent (0-100) for the given model.
///
/// Two scopes (per-model and global) across two tiers (user TOML and
/// remote settings). User-tier always wins over remote; within a tier, per-model
/// wins over global. Env var sits on top as a per-process override.
///
/// Precedence (highest first):
///   1. env `GROK_AUTO_COMPACT_THRESHOLD_PERCENT`
///   2. user TOML `[model.<id>].auto_compact_threshold_percent`
///      (read from `cfg.config_models`; the effective merge of user +
///      managed `[model.<id>]` sections)
///   3. user TOML `[session].auto_compact_threshold_percent`
///      (read from `cfg.session.auto_compact_threshold_percent: Option<u8>`)
///   4. catalog `effective_context_window_percent` (Codex live catalog = 95)
///      — stored on `ConfigModelOverride.effective_context_window_percent`,
///      not `auto_compact_threshold_percent`, so it cannot masquerade as
///      a user per-model TOML override (#264)
///   5. remote settings per-model `ModelInfo.auto_compact_threshold_percent`
///      (populated from `grok_build_models[i].auto_compact_threshold_percent`;
///      intentionally NOT collapsed via `ConfigModelOverride::apply` so the
///      user-vs-GB per-model distinction is preserved)
///   6. remote settings global `RemoteSettings.auto_compact_threshold_percent`
///      (populated from `grok_build_settings.auto_compact_threshold_percent`)
///   7. default `DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT` (85)
///
/// Values outside `0..=100` from the env var are ignored with a debug log and
/// the resolver falls through to the next tier. TOML/remote fields are typed
/// `u8` and so naturally constrained.
pub(crate) fn resolve_auto_compact_threshold_percent(
    cfg: &crate::agent::config::Config,
    model_id: &str,
    model: Option<&crate::agent::config::ModelInfo>,
) -> u8 {
    let override_entry = cfg.config_models.get(model_id);
    resolve_auto_compact_threshold_percent_from_tiers(
        override_entry.and_then(|m| m.auto_compact_threshold_percent),
        cfg.session.auto_compact_threshold_percent,
        override_entry.and_then(|m| m.effective_context_window_percent),
        model.and_then(|m| m.auto_compact_threshold_percent),
        cfg.remote_settings
            .as_ref()
            .and_then(|r| r.auto_compact_threshold_percent),
    )
}

/// Lower-level form of [`resolve_auto_compact_threshold_percent`] that takes
/// the four tiers as plain `Option<u8>` values rather than reaching into a
/// `Config`. Useful from sites that don't hold a `Config` reference (e.g.,
/// subagent spawn paths where the parent's config tiers are plumbed in
/// explicitly and the per-model lookup uses the SUBAGENT's resolved model id,
/// not the parent's).
///
/// Precedence: env > `user_per_model` > `user_global` > `catalog_per_model`
/// > `gb_per_model` > `gb_global` > `DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT`.
pub(crate) fn resolve_auto_compact_threshold_percent_from_tiers(
    user_per_model: Option<u8>,
    user_global: Option<u8>,
    catalog_per_model: Option<u8>,
    gb_per_model: Option<u8>,
    gb_global: Option<u8>,
) -> u8 {
    fn clamp_env(raw: i64) -> Option<u8> {
        if (0..=100).contains(&raw) {
            Some(raw as u8)
        } else {
            tracing::debug!(
                source = "env",
                value = raw,
                "auto_compact_threshold_percent out of range 0..=100; ignoring"
            );
            None
        }
    }
    let from_env = || -> Option<u8> {
        xai_grok_config::resolve_env_var(
            "MEDLEY_AUTO_COMPACT_THRESHOLD_PERCENT",
            ENV_AUTO_COMPACT_THRESHOLD_PERCENT,
        )
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(clamp_env)
    };

    from_env()
        .or(user_per_model)
        .or(user_global)
        .or(catalog_per_model)
        .or(gb_per_model)
        .or(gb_global)
        .unwrap_or(DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT)
}

/// Absolute token count at which auto-compact should fire.
///
/// Catalog `auto_compact_token_limit` is an additional absolute trigger
/// (#259). When it is present and positive, the session fires at the
/// earlier of that limit and `percent * context_window`. Percent-only
/// remains when the catalog field is absent.
pub(crate) fn resolve_auto_compact_token_threshold(
    context_window: u64,
    threshold_percent: u8,
    auto_compact_token_limit: Option<u64>,
) -> u64 {
    let percent_tokens = context_window.saturating_mul(u64::from(threshold_percent)) / 100;
    match auto_compact_token_limit.filter(|limit| *limit > 0) {
        Some(limit) => percent_tokens.min(limit),
        None => percent_tokens,
    }
}

/// Whether usage has reached the resolved auto-compact trigger.
///
/// Keeps [`xai_token_estimation::exceeds_threshold`] for the percent path
/// (same integer rounding as the rest of the session) and ORs in the
/// catalog absolute limit when present.
pub(crate) fn exceeds_auto_compact_threshold(
    used: u64,
    context_window: u64,
    threshold_percent: u8,
    auto_compact_token_limit: Option<u64>,
) -> bool {
    if context_window == 0 {
        return false;
    }
    let percent_hit =
        xai_token_estimation::exceeds_threshold(used, context_window, threshold_percent);
    let absolute_hit = auto_compact_token_limit
        .filter(|limit| *limit > 0)
        .is_some_and(|limit| used >= limit);
    percent_hit || absolute_hit
}

/// Client default per-compaction wall-clock budget (seconds). Fleet p99 of
/// successful compactions is ~181s (≈225s at 400K+ input), so 300s clears the
/// legit tail with margin while cutting a runaway from the ~600s deadline.
pub const DEFAULT_COMPACTION_WALL_CLOCK_BUDGET_SECS: u64 = 300;

/// Below this, a configured budget is almost certainly a misconfig (fleet
/// success p99 ~181s); logged at `warn`, not clamped.
const COMPACTION_WALL_CLOCK_BUDGET_WARN_SECS: u64 = 120;

/// Env override for the compaction wall-clock budget (seconds). Parsed as
/// `u64`; unparseable values fall through.
const ENV_COMPACTION_WALL_CLOCK_BUDGET_SECS: &str = "GROK_COMPACTION_WALL_CLOCK_SECS";

/// Resolve the per-compaction wall-clock budget (seconds). Precedence: env
/// `GROK_COMPACTION_WALL_CLOCK_SECS` > remote settings global
/// `RemoteSettings.compaction_wall_clock_budget_secs` >
/// [`DEFAULT_COMPACTION_WALL_CLOCK_BUDGET_SECS`] (a per-model `ModelInfo` tier
/// would slot in ahead of the global one).
///
/// `0` **disables** it. Low values are warned, not clamped — any "safe" clamp
/// (e.g. 30s) would itself cut legit compactions, trading one silent failure for
/// another; ops own the value.
pub(crate) fn resolve_compaction_wall_clock_budget_secs(gb_global: Option<u64>) -> u64 {
    let from_env = std::env::var(ENV_COMPACTION_WALL_CLOCK_BUDGET_SECS)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let resolved = from_env
        .or(gb_global)
        .unwrap_or(DEFAULT_COMPACTION_WALL_CLOCK_BUDGET_SECS);
    if resolved > 0 && resolved < COMPACTION_WALL_CLOCK_BUDGET_WARN_SECS {
        tracing::warn!(
            budget_secs = resolved,
            "compaction wall-clock budget {resolved}s is below {COMPACTION_WALL_CLOCK_BUDGET_WARN_SECS}s \
             and may cut legitimate compactions (fleet success p99 ~181s); set 0 to disable"
        );
    }
    resolved
}

#[cfg(test)]
mod compaction_wall_clock_budget_tests {
    use super::resolve_compaction_wall_clock_budget_secs as resolve;

    // Assumes GROK_COMPACTION_WALL_CLOCK_SECS is unset in the test env.
    #[test]
    fn default_global_disable_and_no_clamp() {
        assert_eq!(resolve(None), 300); // client default
        assert_eq!(resolve(Some(450)), 450); // server global wins
        assert_eq!(resolve(Some(0)), 0); // 0 explicitly disables (no clamp)
        assert_eq!(resolve(Some(5)), 5); // low values pass through (warned, not clamped)
    }
}

#[cfg(test)]
mod compaction_tool_choice_tests {
    use super::{CompactionToolChoice, resolve_compaction_tool_choice_from as resolve};

    #[test]
    fn default_is_auto() {
        assert_eq!(resolve(None, None, None), CompactionToolChoice::Auto);
    }

    #[test]
    fn precedence_env_over_config_over_remote() {
        assert_eq!(
            resolve(Some("none"), Some("auto"), Some("auto")),
            CompactionToolChoice::None
        );
        assert_eq!(
            resolve(None, Some("none"), Some("auto")),
            CompactionToolChoice::None
        );
        assert_eq!(
            resolve(None, None, Some("none")),
            CompactionToolChoice::None
        );
    }

    #[test]
    fn garbage_falls_through() {
        assert_eq!(
            resolve(Some("garbage"), None, Some("none")),
            CompactionToolChoice::None
        );
        assert_eq!(
            resolve(Some("garbage"), Some("also-bad"), None),
            CompactionToolChoice::Auto
        );
    }

    #[test]
    fn from_str_case_insensitive() {
        assert_eq!("AUTO".parse(), Ok(CompactionToolChoice::Auto));
        assert_eq!(" None ".parse(), Ok(CompactionToolChoice::None));
        assert!("required".parse::<CompactionToolChoice>().is_err());
    }
}

#[cfg(test)]
mod resolve_auto_compact_token_threshold_tests {
    use super::{
        DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT, exceeds_auto_compact_threshold,
        resolve_auto_compact_token_threshold,
    };

    #[test]
    fn resolve_auto_compact_token_threshold_uses_catalog_absolute_limit() {
        assert_eq!(
            resolve_auto_compact_token_threshold(272_000, 85, Some(180_000)),
            180_000,
            "catalog absolute limit must fire before 85% of the window"
        );
        assert_eq!(
            resolve_auto_compact_token_threshold(200_000, 85, Some(180_000)),
            170_000,
            "tighter percent-of-window still wins when it is earlier"
        );
        assert_eq!(
            resolve_auto_compact_token_threshold(200_000, 85, None),
            170_000
        );
        assert_eq!(
            resolve_auto_compact_token_threshold(200_000, 85, Some(0)),
            170_000,
            "non-positive catalog limits are absent"
        );
        assert_eq!(
            resolve_auto_compact_token_threshold(
                100_000,
                DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT,
                None,
            ),
            85_000
        );

        assert!(!exceeds_auto_compact_threshold(
            179_999,
            272_000,
            85,
            Some(180_000),
        ));
        assert!(exceeds_auto_compact_threshold(
            180_000,
            272_000,
            85,
            Some(180_000),
        ));
        assert!(
            exceeds_auto_compact_threshold(85_000, 100_000, 85, Some(90_000)),
            "percent path must still fire when it is earlier than the catalog"
        );
        assert!(!exceeds_auto_compact_threshold(84_999, 100_000, 85, None));
        assert!(!exceeds_auto_compact_threshold(1, 0, 85, Some(1)));
    }
}

#[cfg(test)]
mod auto_compact_threshold_env_alias_tests {
    use super::resolve_auto_compact_threshold_percent;

    #[test]
    #[serial_test::serial]
    fn medley_wins_over_grok() {
        unsafe {
            std::env::set_var("MEDLEY_AUTO_COMPACT_THRESHOLD_PERCENT", "10");
            std::env::set_var("GROK_AUTO_COMPACT_THRESHOLD_PERCENT", "90");
        }
        let cfg = crate::agent::config::Config::default();
        let result = resolve_auto_compact_threshold_percent(&cfg, "some-model", None);
        unsafe {
            std::env::remove_var("MEDLEY_AUTO_COMPACT_THRESHOLD_PERCENT");
            std::env::remove_var("GROK_AUTO_COMPACT_THRESHOLD_PERCENT");
        }
        assert_eq!(
            result, 10,
            "MEDLEY_AUTO_COMPACT_THRESHOLD_PERCENT=10 must win over GROK_*=90"
        );
    }

    #[test]
    #[serial_test::serial]
    fn grok_still_works_when_medley_unset() {
        unsafe {
            std::env::remove_var("MEDLEY_AUTO_COMPACT_THRESHOLD_PERCENT");
            std::env::set_var("GROK_AUTO_COMPACT_THRESHOLD_PERCENT", "90");
        }
        let cfg = crate::agent::config::Config::default();
        let result = resolve_auto_compact_threshold_percent(&cfg, "some-model", None);
        unsafe { std::env::remove_var("GROK_AUTO_COMPACT_THRESHOLD_PERCENT") };
        assert_eq!(
            result, 90,
            "GROK_AUTO_COMPACT_THRESHOLD_PERCENT must still work when MEDLEY_* is unset"
        );
    }
}
