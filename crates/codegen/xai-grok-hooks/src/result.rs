use std::time::Duration;

/// The outcome of a blocking (`pre_tool_use`) hook dispatch.
#[derive(Clone, PartialEq, Eq)]
pub enum HookDecision {
    Allow,
    Deny { reason: String, hook_name: String },
}

impl std::fmt::Debug for HookDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allow => f.write_str("Allow"),
            Self::Deny { reason, hook_name } => f
                .debug_struct("Deny")
                .field("reason_present", &!reason.trim().is_empty())
                .field("hook_name", hook_name)
                .finish(),
        }
    }
}

/// Parsed output of one `Stop`/`SubagentStop` gate hook. The dispatcher
/// aggregates these across hooks; `force_stop` overrides blocks.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct StopHookOutcome {
    pub block_reason: Option<String>,
    pub additional_context: Option<String>,
    pub force_stop: Option<StopOverride>,
}

impl std::fmt::Debug for StopHookOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StopHookOutcome")
            .field("block_reason_present", &self.block_reason.is_some())
            .field(
                "additional_context_present",
                &self.additional_context.is_some(),
            )
            .field("force_stop_present", &self.force_stop.is_some())
            .finish()
    }
}

/// A `continue: false` force-stop; `reason` is `stopReason`, shown to the user.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct StopOverride {
    pub reason: Option<String>,
}

impl std::fmt::Debug for StopOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StopOverride")
            .field("reason_present", &self.reason.is_some())
            .finish()
    }
}

impl StopHookOutcome {
    pub fn is_empty(&self) -> bool {
        self.block_reason.is_none()
            && self.additional_context.is_none()
            && self.force_stop.is_none()
    }
}

/// HTTP execution details for `"http"` hooks, for scrollback enrichment.
#[derive(Clone)]
pub struct HttpInfo {
    /// Secret-safe endpoint description (for example `https://<configured>`).
    pub url: String,
    /// Retained for compatibility. Production runners leave it unset because
    /// even the pre-expansion source can contain literal credentials.
    pub raw_url: Option<String>,
    pub status: Option<u16>,
    /// Retained for compatibility. Production runners do not retain response
    /// bodies because hooks can echo credentials in arbitrary content.
    pub response_preview: Option<String>,
}

impl std::fmt::Debug for HttpInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpInfo")
            .field("url_configured", &!self.url.is_empty())
            .field("raw_url_present", &self.raw_url.is_some())
            .field("status", &self.status)
            .field("response_preview_present", &self.response_preview.is_some())
            .finish()
    }
}

/// The outcome of a single hook execution.
#[derive(Debug)]
pub enum HookRunResult {
    Success {
        hook_name: String,
        elapsed: Duration,
        http_info: Option<HttpInfo>,
    },
    Skipped {
        hook_name: String,
    },
    /// Ran and blocked: a stop-gate decision, not a failure (distinct from `Failed`).
    Blocked {
        hook_name: String,
        detail: String,
        elapsed: Duration,
        http_info: Option<HttpInfo>,
    },
    /// Hook failed (timeout, crash, bad output): fail-open.
    Failed {
        hook_name: String,
        error: String,
        elapsed: Duration,
        http_info: Option<HttpInfo>,
    },
}
