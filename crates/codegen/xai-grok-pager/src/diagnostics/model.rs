//! Shared terminal diagnostic report types.

use crate::clipboard::{ClipboardDelivery, NativeClipboardPreflight, Osc52Capability};
use crate::host::{DisplayServer, HostOs};
use crate::terminal::{ByobuBackend, ModifierDelivery, MultiplexerKind, TerminalName};
use crate::theme::ThemeKind;
use crate::theme::color_support::ColorLevel;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeFact<T> {
    Available(T),
    NoReply,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DiagnosticId {
    pub domain: &'static str,
    pub item: &'static str,
}

impl DiagnosticId {
    pub const fn new(domain: &'static str, item: &'static str) -> Self {
        Self { domain, item }
    }
}

impl std::fmt::Display for DiagnosticId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.domain, self.item)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticReport {
    pub facts: DiagnosticFacts,
    pub findings: Vec<DiagnosticFinding>,
    pub probe_notes: Vec<ProbeNote>,
}

pub(crate) const NOTIFICATION_PROTOCOL_FALLBACK_ID: DiagnosticId =
    DiagnosticId::new("notifications", "protocol-fallback");
pub(crate) const FOCUS_TRACKING_UNAVAILABLE_ID: DiagnosticId =
    DiagnosticId::new("notifications", "focus-tracking-unavailable");
pub(crate) const SANDBOX_PROFILE_CONFLICT_ID: DiagnosticId =
    DiagnosticId::new("sandbox", "profile-conflict");
pub(crate) const CLIPBOARD_DELIVERY_UNVERIFIED_ID: DiagnosticId =
    DiagnosticId::new("clipboard", "delivery-unverified");
pub(crate) const CLIPBOARD_DELIVERY_UNAVAILABLE_ID: DiagnosticId =
    DiagnosticId::new("clipboard", "delivery-unavailable");
pub(crate) const NEWLINE_FALLBACK_ID: DiagnosticId =
    DiagnosticId::new("terminal", "newline-fallback");
pub(crate) const ITERM2_CLIPBOARD_PERMISSION_ID: DiagnosticId =
    DiagnosticId::new("terminal", "iterm2-clipboard-permission");
pub(crate) const VSCODE_SSH_NON_ASCII_ID: DiagnosticId =
    DiagnosticId::new("clipboard", "vscode-ssh-non-ascii");
pub(crate) const VOICE_NO_INPUT_DEVICE_ID: DiagnosticId =
    DiagnosticId::new("voice", "no-input-device");

impl DiagnosticReport {
    pub fn issue_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.disposition == FindingDisposition::Issue)
            .count()
            + usize::from(
                !self.facts.clipboard.delivery.is_confirmed()
                    && !self.findings.iter().any(|finding| {
                        matches!(
                            finding.id,
                            CLIPBOARD_DELIVERY_UNVERIFIED_ID | CLIPBOARD_DELIVERY_UNAVAILABLE_ID
                        )
                    }),
            )
    }

    pub fn recommendation_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.disposition == FindingDisposition::Recommendation)
            .count()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticFacts {
    pub terminal: TerminalName,
    pub xtversion: RuntimeFact<String>,
    pub multiplexer: MultiplexerKind,
    pub byobu: Option<ByobuBackend>,
    pub ssh: bool,
    pub tmux: TmuxFacts,
    pub color: ColorFacts,
    pub keyboard: Option<KeyboardFact>,
    pub newline: Option<NewlineFact>,
    pub clipboard: ClipboardFacts,
    /// Passive mic enumeration when voice capture is available. `None` omits the
    /// Voice section (no-audio builds, or TUI when voice mode is off).
    pub voice: Option<VoiceFacts>,
    /// Offline model-route facts, shaped like inspect's secret-free
    /// `EffectiveModelRoute`. Empty omits the Providers section. Fields are
    /// names, classes, and sanitized origins only — never credential bytes.
    pub providers: Vec<ProviderRouteFact>,
}

/// Wire auth header a model route will send. Display/JSON labels only.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAuthScheme {
    /// `auth_scheme = "none"`: deliberately keyless.
    None,
    Bearer,
    #[serde(rename = "x-api-key")]
    XApiKey,
}

impl ProviderAuthScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bearer => "bearer",
            Self::XApiKey => "x-api-key",
        }
    }
}

/// Endpoint trust class copied from the inspect route (sampler-enforced).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderEndpointTrust {
    FirstPartyXai,
    External,
    Local,
    UserDeclared,
    /// ACP meta omitted or used an unrecognized class. Do not invent External.
    Unknown,
}

impl ProviderEndpointTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstPartyXai => "first_party_xai",
            Self::External => "external",
            Self::Local => "local",
            Self::UserDeclared => "user_declared",
            Self::Unknown => "unknown",
        }
    }
}

/// One secret-free provider/model row for `/doctor` and `grok doctor --json`.
///
/// Mirrors `EffectiveModelRoute` plus the inspect auth scheme. `credential_source`
/// is the already-formatted label (`env:NAME`, `none`, `missing`, …) — env-var
/// **names** are allowed; secret bytes are not.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRouteFact {
    pub catalog_id: String,
    pub wire_model: String,
    /// Scheme + host [+ port] [+ path]. Userinfo, query, and fragment must
    /// already have been stripped by the inspect/route builder.
    pub sanitized_origin: String,
    pub auth_scheme: ProviderAuthScheme,
    pub credential_source: String,
    pub endpoint_trust: ProviderEndpointTrust,
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unready_reason: Option<String>,
}

/// Offline provider rows from the pager's live `ModelState` (TUI `/doctor`).
///
/// Origins and credential bytes are not on ACP `ModelInfo`; those stay
/// empty / source labels from meta only.
pub fn provider_facts_from_model_state(
    models: &crate::acp::model_state::ModelState,
) -> Vec<ProviderRouteFact> {
    models
        .available
        .iter()
        .map(|(id, info)| {
            let meta = info.meta.as_ref();
            let ready = meta
                .and_then(|m| m.get("ready"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let auth = meta
                .and_then(|m| m.get("authScheme"))
                .and_then(|v| v.as_str())
                .unwrap_or("bearer");
            let source = meta
                .and_then(|m| m.get("credentialSource"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            ProviderRouteFact {
                catalog_id: id.0.to_string(),
                wire_model: meta
                    .and_then(|m| m.get("modelSlug"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(info.name.as_str())
                    .to_string(),
                sanitized_origin: meta
                    .and_then(|m| m.get("sanitizedOrigin"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                auth_scheme: match auth {
                    "none" => ProviderAuthScheme::None,
                    "x-api-key" => ProviderAuthScheme::XApiKey,
                    _ => ProviderAuthScheme::Bearer,
                },
                credential_source: source.to_string(),
                endpoint_trust: match meta
                    .and_then(|m| m.get("endpointTrust"))
                    .and_then(|v| v.as_str())
                {
                    Some("first_party_xai") => ProviderEndpointTrust::FirstPartyXai,
                    Some("local") => ProviderEndpointTrust::Local,
                    Some("user_declared") => ProviderEndpointTrust::UserDeclared,
                    Some("external") => ProviderEndpointTrust::External,
                    _ => ProviderEndpointTrust::Unknown,
                },
                ready,
                unready_reason: if ready {
                    None
                } else {
                    meta.and_then(|m| m.get("readinessReason"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                },
            }
        })
        .collect()
}

/// Result of a passive input-device lookup (does not open a capture stream).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoiceFacts {
    /// Device (or Linux recorder) capture would open.
    Device { name: String, detail: String },
    /// Audio is compiled in but no default input / recorder exists.
    Missing { error: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxFacts {
    pub extended_keys: TmuxOptionFact,
    pub set_clipboard: TmuxOptionFact,
    pub allow_passthrough_support: TmuxSupportFact,
    pub allow_passthrough: TmuxOptionFact,
    pub color_passthrough: TmuxColorPassthrough,
}

/// Whether the attached tmux client forwards 24-bit color to the terminal.
///
/// tmux resolves a client's features once, at attach time, so this describes
/// the live client and not the config on disk: a config change applies only
/// after that client reattaches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TmuxColorPassthrough {
    /// The client advertises `RGB`, so truecolor SGR reaches the terminal.
    Forwarded,
    /// tmux reduces 24-bit color to the client terminfo's palette, which is
    /// what makes themes look washed out even when Grok emits truecolor.
    Reduced,
    /// No usable evidence: tmux predates `terminal-features` (3.2), no client
    /// is attached, or the query failed. Never treated as a problem.
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TmuxOptionFact {
    Available(String),
    Unsupported,
    Unavailable,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TmuxSupportFact {
    Supported,
    Unsupported,
    Unavailable,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColorFacts {
    pub level: RuntimeFact<ColorLevel>,
    pub available_themes: Vec<ThemeKind>,
    pub total_themes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyboardFact {
    pub modifier_delivery: ModifierDelivery,
    pub os: HostOs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NewlineFact {
    Vte { version: Option<String> },
    XtermJs { terminal: TerminalName },
    NoKittyKeyboardProtocol,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardFacts {
    pub native_route: bool,
    pub native_tool: String,
    pub native_preflight: NativeClipboardPreflight,
    pub tmux_route: bool,
    pub osc52_route: bool,
    pub osc52_capability: Osc52Capability,
    pub wrap_sink: bool,
    pub display_server: DisplayServer,
    pub container_no_display: bool,
    pub data_control: DataControlFact,
    pub delivery: ClipboardDelivery,
    /// Compatibility projection for compact status/JSON consumers. Detailed
    /// policy and remediation live in named findings.
    pub fix: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataControlFact {
    Available,
    Missing,
    Unavailable,
    Error,
    NotApplicable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticFinding {
    pub id: DiagnosticId,
    pub disposition: FindingDisposition,
    pub message: String,
    pub remediation: Option<ManualRemediation>,
    pub automatic_remediation: Option<crate::diagnostics::AutomaticRemediation>,
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FindingDisposition {
    Issue,
    Recommendation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManualRemediation {
    pub fix: String,
    pub config_path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeNote {
    pub probe: &'static str,
    pub status: ProbeStatus,
    pub message: Option<String>,
}

pub(crate) fn probe_requires_live_tui(note: &ProbeNote) -> bool {
    note.status == ProbeStatus::Unavailable
        && matches!(
            note.probe,
            "runtime.fullscreen-active" | "runtime.kitty-flags-pushed" | "runtime.xtversion"
        )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeStatus {
    Unsupported,
    Unavailable,
    Error,
}
