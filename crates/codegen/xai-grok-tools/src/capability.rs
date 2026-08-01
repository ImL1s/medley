//! Runtime policy for enforcing subagent tool capability boundaries.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use xai_tool_types::{
    SubagentCapabilityMode, ToolCapabilityDenial, ToolCapabilityDescriptor, ToolEffect,
};

use crate::registry::types::{ToolConfig, ToolServerConfig};
use crate::types::config_source::ConfigSource;
use crate::types::tool::ToolKind;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "camelCase")]
pub enum CapabilityOrigin {
    Builtin,
    TrustedConfig { source: ConfigSource },
    UntrustedExternal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedToolCapability {
    pub descriptor: ToolCapabilityDescriptor,
    pub origin: CapabilityOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnclassifiedToolOverride {
    pub modes: Vec<SubagentCapabilityMode>,
    pub reason: String,
    pub source: ConfigSource,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedToolCapabilities {
    pub classifications: HashMap<String, ResolvedToolCapability>,
    pub unclassified_overrides: HashMap<String, UnclassifiedToolOverride>,
}

impl TrustedToolCapabilities {
    pub fn insert_classification(
        &mut self,
        exact_tool_id: impl Into<String>,
        descriptor: ToolCapabilityDescriptor,
        source: ConfigSource,
    ) -> Result<(), String> {
        let exact_tool_id = exact_tool_id.into();
        validate_exact_tool_id(&exact_tool_id)?;
        if matches!(descriptor, ToolCapabilityDescriptor::Unclassified) {
            return Err(
                "trusted classification must declare at least an explicit classified descriptor"
                    .into(),
            );
        }
        self.classifications.insert(
            exact_tool_id,
            ResolvedToolCapability {
                descriptor,
                origin: CapabilityOrigin::TrustedConfig { source },
            },
        );
        Ok(())
    }

    pub fn insert_unclassified_override(
        &mut self,
        exact_tool_id: impl Into<String>,
        override_entry: UnclassifiedToolOverride,
    ) -> Result<(), String> {
        let exact_tool_id = exact_tool_id.into();
        validate_exact_tool_id(&exact_tool_id)?;
        if override_entry.modes.is_empty() {
            return Err("unclassified override modes must not be empty".into());
        }
        if override_entry.reason.trim().is_empty() {
            return Err("unclassified override reason must not be empty".into());
        }
        if override_entry.reason.chars().any(char::is_control) {
            return Err("unclassified override reason must not contain control characters".into());
        }
        self.unclassified_overrides
            .insert(exact_tool_id, override_entry);
        Ok(())
    }
}

pub fn validate_exact_tool_id(id: &str) -> Result<(), String> {
    if id.trim() != id || id.is_empty() {
        return Err(
            "tool capability key must be a non-empty exact ID without surrounding whitespace"
                .into(),
        );
    }
    if id.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']')) {
        return Err(
            "tool capability key must be an exact ID; glob patterns are not allowed".into(),
        );
    }
    if id.chars().any(char::is_control) {
        return Err("tool capability key must not contain control characters".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "camelCase")]
pub enum CapabilityDropReason {
    Unclassified,
    EffectDenied { effect: ToolEffect },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "camelCase")]
pub enum CapabilityDecision {
    Keep,
    KeepByExplicitOverride {
        reason: String,
        source: ConfigSource,
    },
    Drop {
        reason: CapabilityDropReason,
    },
}

impl CapabilityDecision {
    pub fn is_kept(&self) -> bool {
        !matches!(self, Self::Drop { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDiagnostic {
    pub tool_id: String,
    pub mode: SubagentCapabilityMode,
    pub capability: ResolvedToolCapability,
    pub decision: CapabilityDecision,
}

#[derive(Debug, Clone)]
pub struct CapabilityPolicy {
    mode: SubagentCapabilityMode,
    trusted: Arc<TrustedToolCapabilities>,
    warned_overrides: Arc<Mutex<HashSet<String>>>,
}

impl Default for CapabilityPolicy {
    fn default() -> Self {
        Self::unrestricted()
    }
}

impl CapabilityPolicy {
    pub fn unrestricted() -> Self {
        Self::new(
            SubagentCapabilityMode::All,
            TrustedToolCapabilities::default(),
        )
    }

    pub fn new(mode: SubagentCapabilityMode, trusted: TrustedToolCapabilities) -> Self {
        Self {
            mode,
            trusted: Arc::new(trusted),
            warned_overrides: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn mode(&self) -> SubagentCapabilityMode {
        self.mode
    }

    pub fn trusted_capabilities(&self) -> &TrustedToolCapabilities {
        self.trusted.as_ref()
    }

    pub fn resolve_builtin(&self, canonical_id: &str, kind: ToolKind) -> ResolvedToolCapability {
        let descriptor = descriptor_for_kind(kind);
        if matches!(descriptor, ToolCapabilityDescriptor::Unclassified)
            && let Some(trusted) = self.trusted.classifications.get(canonical_id)
        {
            return trusted.clone();
        }
        ResolvedToolCapability {
            descriptor,
            origin: CapabilityOrigin::Builtin,
        }
    }

    /// Resolve a dynamically registered MCP/custom tool. Its remotely supplied
    /// `ToolKind` or `_meta` is deliberately ignored; only exact-ID metadata
    /// already present in the trusted local catalog can classify it.
    pub fn resolve_external(&self, canonical_id: &str) -> ResolvedToolCapability {
        self.trusted
            .classifications
            .get(canonical_id)
            .cloned()
            .unwrap_or(ResolvedToolCapability {
                descriptor: ToolCapabilityDescriptor::Unclassified,
                origin: CapabilityOrigin::UntrustedExternal,
            })
    }

    pub fn evaluate(
        &self,
        canonical_id: &str,
        capability: &ResolvedToolCapability,
    ) -> CapabilityDecision {
        match self.mode.denied_tool_capability(&capability.descriptor) {
            None => CapabilityDecision::Keep,
            Some(ToolCapabilityDenial::Unclassified) => {
                if let Some(override_entry) = self
                    .trusted
                    .unclassified_overrides
                    .get(canonical_id)
                    .filter(|entry| {
                        !entry.reason.trim().is_empty() && entry.modes.contains(&self.mode)
                    })
                {
                    CapabilityDecision::KeepByExplicitOverride {
                        reason: override_entry.reason.clone(),
                        source: override_entry.source.clone(),
                    }
                } else {
                    CapabilityDecision::Drop {
                        reason: CapabilityDropReason::Unclassified,
                    }
                }
            }
            Some(ToolCapabilityDenial::Effect(effect)) => CapabilityDecision::Drop {
                reason: CapabilityDropReason::EffectDenied { effect },
            },
        }
    }

    pub fn evaluate_tool_config(&self, tool: &ToolConfig) -> CapabilityDiagnostic {
        let capability = tool.kind.map_or_else(
            || self.resolve_external(&tool.id),
            |kind| self.resolve_builtin(&tool.id, kind),
        );
        let decision = self.evaluate(&tool.id, &capability);
        CapabilityDiagnostic {
            tool_id: tool.id.clone(),
            mode: self.mode,
            capability,
            decision,
        }
    }

    pub fn filter_tool_config(&self, config: &mut ToolServerConfig) -> Vec<CapabilityDiagnostic> {
        let mut diagnostics = Vec::new();
        config.tools.retain(|tool| {
            let diagnostic = self.evaluate_tool_config(tool);
            let kept = diagnostic.decision.is_kept();
            if !matches!(diagnostic.decision, CapabilityDecision::Keep) {
                self.emit_diagnostic(&diagnostic);
                diagnostics.push(diagnostic);
            }
            kept
        });
        diagnostics
    }

    pub fn authorize(
        &self,
        canonical_id: &str,
        capability: &ResolvedToolCapability,
    ) -> Result<(), xai_tool_runtime::ToolError> {
        let decision = self.evaluate(canonical_id, capability);
        if decision.is_kept() {
            if !matches!(decision, CapabilityDecision::Keep) {
                self.emit_diagnostic(&CapabilityDiagnostic {
                    tool_id: canonical_id.to_owned(),
                    mode: self.mode,
                    capability: capability.clone(),
                    decision,
                });
            }
            return Ok(());
        }
        Err(xai_tool_runtime::ToolError::custom(
            "tool_capability_denied",
            format!(
                "Tool {canonical_id} is not permitted in {} capability mode",
                self.mode.as_str()
            ),
        ))
    }

    fn emit_diagnostic(&self, diagnostic: &CapabilityDiagnostic) {
        match &diagnostic.decision {
            CapabilityDecision::Keep => {}
            CapabilityDecision::KeepByExplicitOverride { reason, source } => {
                if self
                    .warned_overrides
                    .lock()
                    .insert(diagnostic.tool_id.clone())
                {
                    tracing::warn!(
                        tool_id = %diagnostic.tool_id,
                        mode = %diagnostic.mode.as_str(),
                        reason = %reason,
                        source = %source.display_short(),
                        "allowing unclassified tool through explicit capability override"
                    );
                }
            }
            CapabilityDecision::Drop { reason } => {
                tracing::debug!(
                    tool_id = %diagnostic.tool_id,
                    mode = %diagnostic.mode.as_str(),
                    reason = ?reason,
                    "removed tool from restricted capability mode"
                );
            }
        }
    }
}

/// Exhaustive mapping from built-in taxonomy to security effects.
pub fn descriptor_for_kind(kind: ToolKind) -> ToolCapabilityDescriptor {
    use ToolEffect as Effect;
    use ToolKind::*;

    match kind {
        Read | ListDir | Search | Lsp | List | MemorySearch | MemoryGet | Skill => {
            ToolCapabilityDescriptor::classified([Effect::LocalRead])
        }
        WebSearch => ToolCapabilityDescriptor::classified([Effect::NetworkRead]),
        WebFetch => ToolCapabilityDescriptor::classified([Effect::NetworkRead, Effect::LocalWrite]),
        Edit | Delete | Write | Move => ToolCapabilityDescriptor::classified([Effect::LocalWrite]),
        Execute | BackgroundTaskAction | WaitTasksAction | KillTaskAction | Monitor => {
            ToolCapabilityDescriptor::classified([Effect::Execute])
        }
        ImageGen | VideoGen | ImageToVideo | ReferenceToVideo | DeployApp => {
            ToolCapabilityDescriptor::classified([Effect::ExternalMutation])
        }
        Task => ToolCapabilityDescriptor::classified([Effect::SubagentSpawn]),
        Workflow => ToolCapabilityDescriptor::classified([Effect::Execute, Effect::SubagentSpawn]),
        Plan | EnterPlan | ExitPlan | AskUser | GoalUpdate => {
            ToolCapabilityDescriptor::classified([])
        }
        SearchTool => {
            ToolCapabilityDescriptor::classified([Effect::LocalRead, Effect::NetworkRead])
        }
        // `use_tool` is only a forwarding boundary. It has no direct effect;
        // the exact target is independently resolved and authorized before
        // either local or managed-gateway dispatch.
        UseTool => ToolCapabilityDescriptor::classified([]),
        Other => ToolCapabilityDescriptor::Unclassified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(id: &str, kind: Option<ToolKind>) -> ToolConfig {
        let mut config = ToolConfig::from_id(id);
        config.kind = kind;
        config
    }

    #[test]
    fn restricted_policy_drops_unclassified_and_all_keeps_it() {
        let opaque = config("server__opaque", None);
        assert!(
            !CapabilityPolicy::new(
                SubagentCapabilityMode::ReadOnly,
                TrustedToolCapabilities::default(),
            )
            .evaluate_tool_config(&opaque)
            .decision
            .is_kept()
        );
        assert!(
            CapabilityPolicy::unrestricted()
                .evaluate_tool_config(&opaque)
                .decision
                .is_kept()
        );
    }

    #[test]
    fn trusted_exact_id_classification_and_override_are_separate() {
        let source = ConfigSource::User {
            path: "/tmp/config.toml".into(),
        };
        let mut trusted = TrustedToolCapabilities::default();
        trusted
            .insert_classification(
                "server__read",
                ToolCapabilityDescriptor::classified([ToolEffect::NetworkRead]),
                source.clone(),
            )
            .unwrap();
        trusted
            .insert_unclassified_override(
                "server__legacy",
                UnclassifiedToolOverride {
                    modes: vec![SubagentCapabilityMode::ReadOnly],
                    reason: "locally audited connector".into(),
                    source,
                },
            )
            .unwrap();
        let policy = CapabilityPolicy::new(SubagentCapabilityMode::ReadOnly, trusted);
        assert!(
            policy
                .evaluate("server__read", &policy.resolve_external("server__read"))
                .is_kept()
        );
        assert!(
            policy
                .evaluate("server__legacy", &policy.resolve_external("server__legacy"))
                .is_kept()
        );
        assert!(
            !policy
                .evaluate("server__other", &policy.resolve_external("server__other"))
                .is_kept()
        );
    }

    #[test]
    fn unclassified_override_cannot_bypass_denied_classified_effect() {
        let source = ConfigSource::User {
            path: "/tmp/config.toml".into(),
        };
        let mut trusted = TrustedToolCapabilities::default();
        trusted
            .insert_unclassified_override(
                "server__write",
                UnclassifiedToolOverride {
                    modes: vec![SubagentCapabilityMode::ReadOnly],
                    reason: "must not override classified effects".into(),
                    source,
                },
            )
            .unwrap();
        let policy = CapabilityPolicy::new(SubagentCapabilityMode::ReadOnly, trusted);
        let classified_write = ResolvedToolCapability {
            descriptor: ToolCapabilityDescriptor::classified([ToolEffect::LocalWrite]),
            origin: CapabilityOrigin::Builtin,
        };
        assert!(
            !policy
                .evaluate("server__write", &classified_write)
                .is_kept()
        );
    }

    #[test]
    fn exact_id_and_override_reason_reject_terminal_control_input() {
        assert!(validate_exact_tool_id("github__get_issue").is_ok());
        assert!(validate_exact_tool_id("github__*").is_err());
        assert!(validate_exact_tool_id("github__\u{1b}[2J").is_err());
        let mut trusted = TrustedToolCapabilities::default();
        assert!(
            trusted
                .insert_unclassified_override(
                    "legacy__tool",
                    UnclassifiedToolOverride {
                        modes: vec![SubagentCapabilityMode::ReadOnly],
                        reason: " ".into(),
                        source: ConfigSource::Builtin,
                    }
                )
                .is_err()
        );
        assert!(
            trusted
                .insert_unclassified_override(
                    "legacy__tool",
                    UnclassifiedToolOverride {
                        modes: vec![SubagentCapabilityMode::ReadOnly],
                        reason: "audited\nspoofed".into(),
                        source: ConfigSource::Builtin,
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn web_fetch_declares_artifact_writes_but_web_search_remains_read_only() {
        let web_search = descriptor_for_kind(ToolKind::WebSearch);
        let web_fetch = descriptor_for_kind(ToolKind::WebFetch);

        assert_eq!(
            web_search,
            ToolCapabilityDescriptor::classified([ToolEffect::NetworkRead])
        );
        assert_eq!(
            web_fetch,
            ToolCapabilityDescriptor::classified(
                [ToolEffect::NetworkRead, ToolEffect::LocalWrite,]
            )
        );
        assert!(SubagentCapabilityMode::ReadOnly.allows_tool_capability(&web_search));
        assert!(!SubagentCapabilityMode::ReadOnly.allows_tool_capability(&web_fetch));
        assert!(SubagentCapabilityMode::ReadWrite.allows_tool_capability(&web_fetch));
    }

    #[test]
    fn wrapper_capabilities_separate_discovery_from_forwarded_target_effects() {
        assert_eq!(
            descriptor_for_kind(ToolKind::SearchTool),
            ToolCapabilityDescriptor::classified([ToolEffect::LocalRead, ToolEffect::NetworkRead])
        );
        assert_eq!(
            descriptor_for_kind(ToolKind::UseTool),
            ToolCapabilityDescriptor::classified([])
        );
        assert_eq!(
            descriptor_for_kind(ToolKind::Other),
            ToolCapabilityDescriptor::Unclassified
        );
    }
}
