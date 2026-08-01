//! Trusted capability configuration for external MCP/custom tools.
//!
//! This module deliberately reads only local config layers. Remote tool `_meta`
//! and remote campaign/settings payloads are not authority for capability
//! classification. Project entries are considered only after the caller has
//! resolved folder trust for the current working directory.

use std::path::Path;

use serde::{Deserialize, Serialize};
use xai_grok_tools::capability::{
    TrustedToolCapabilities, UnclassifiedToolOverride, validate_exact_tool_id,
};
use xai_grok_tools::types::config_source::ConfigSource;
use xai_tool_types::SubagentCapabilityMode;
use xai_tool_types::capability::ToolCapabilityDescriptor;

/// Resolved, trusted exact-ID metadata ready to convert into the tools-layer
/// `CapabilityPolicy`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedTrustedToolCapabilities {
    #[serde(flatten)]
    pub trusted: TrustedToolCapabilities,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<ToolCapabilityConfigDiagnostic>,
}

impl ResolvedTrustedToolCapabilities {
    /// Runtime integration seam: the tools layer consumes this catalog without
    /// re-parsing or consulting remote metadata.
    pub fn into_trusted(self) -> TrustedToolCapabilities {
        self.trusted
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCapabilityConfigDiagnosticKind {
    InvalidEntry,
    UnclassifiedOverrideActive,
    ConfigLoadFailed,
}

/// Structured warning shown by `grok inspect` and suitable for runtime logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCapabilityConfigDiagnostic {
    pub kind: ToolCapabilityConfigDiagnosticKind,
    pub path: String,
    pub reason: String,
    pub source: ConfigSource,
}

impl ToolCapabilityConfigDiagnostic {
    pub fn is_warning(&self) -> bool {
        true
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUnclassifiedToolOverride {
    modes: Vec<SubagentCapabilityMode>,
    reason: String,
}

/// Load trusted disk config and, only when trusted, repo-local project config.
///
/// Each disk layer is merged directly, intentionally bypassing
/// `load_effective_config`: remote campaigns/settings must never grant tool
/// capabilities. Project files are merged repo-root first so nearer files win,
/// matching the established project-config precedence.
pub fn load_trusted_tool_capabilities(
    cwd: &Path,
    project_trusted: bool,
) -> ResolvedTrustedToolCapabilities {
    let layers = match super::ConfigLayers::load() {
        Ok(layers) => layers,
        Err(error) => {
            return ResolvedTrustedToolCapabilities {
                diagnostics: vec![ToolCapabilityConfigDiagnostic {
                    kind: ToolCapabilityConfigDiagnosticKind::ConfigLoadFailed,
                    path: "subagents".to_owned(),
                    reason: format!("trusted config layers could not be loaded: {error}"),
                    source: ConfigSource::Managed { path: None },
                }],
                ..Default::default()
            };
        }
    };

    let mut resolved = ResolvedTrustedToolCapabilities::default();
    merge_source(
        &layers.system_managed,
        ConfigSource::Managed { path: None },
        &mut resolved,
    );
    merge_source(
        &layers.managed,
        ConfigSource::Managed { path: None },
        &mut resolved,
    );
    let user_config_path = super::user_grok_home()
        .map(|path| path.join("config.toml"))
        .unwrap_or_else(|| "<user-config>".into());
    merge_source(
        &layers.user,
        ConfigSource::ConfigToml {
            path: user_config_path,
        },
        &mut resolved,
    );
    for requirements in [
        layers.user_requirements.as_ref(),
        layers.system_requirements.as_ref(),
        layers.mdm_requirements.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        merge_source(
            requirements,
            ConfigSource::Managed { path: None },
            &mut resolved,
        );
    }

    let mut load_diagnostics = Vec::new();
    if project_trusted {
        for path in super::find_project_configs(cwd) {
            match super::load_config_file(&path) {
                Ok(value) => merge_source(
                    &value,
                    ConfigSource::Project { path: path.clone() },
                    &mut resolved,
                ),
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, "trusted tool capability project config could not be loaded");
                    load_diagnostics.push(ToolCapabilityConfigDiagnostic {
                        kind: ToolCapabilityConfigDiagnosticKind::ConfigLoadFailed,
                        path: "subagents".to_owned(),
                        reason: format!("project config could not be loaded: {error}"),
                        source: ConfigSource::Project { path },
                    });
                }
            }
        }
    }
    resolved.diagnostics.splice(0..0, load_diagnostics);
    append_active_override_diagnostics(&mut resolved);

    for diagnostic in &resolved.diagnostics {
        tracing::warn!(
            kind = ?diagnostic.kind,
            path = %diagnostic.path,
            source = ?diagnostic.source,
            reason = %diagnostic.reason,
            "trusted tool capability configuration warning"
        );
    }
    resolved
}

/// Pure resolver used by tests and by the runtime integration seam.
pub fn resolve_trusted_tool_capabilities_from_values<'a>(
    global: &toml::Value,
    global_source: ConfigSource,
    project_values: impl IntoIterator<Item = (ConfigSource, &'a toml::Value)>,
    project_trusted: bool,
) -> ResolvedTrustedToolCapabilities {
    let mut resolved = ResolvedTrustedToolCapabilities::default();
    merge_source(global, global_source, &mut resolved);
    if project_trusted {
        for (source, value) in project_values {
            merge_source(value, source, &mut resolved);
        }
    }

    append_active_override_diagnostics(&mut resolved);
    resolved
}

fn append_active_override_diagnostics(resolved: &mut ResolvedTrustedToolCapabilities) {
    let mut overrides = resolved
        .trusted
        .unclassified_overrides
        .iter()
        .collect::<Vec<_>>();
    overrides.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (tool_id, override_entry) in overrides {
        resolved.diagnostics.push(ToolCapabilityConfigDiagnostic {
            kind: ToolCapabilityConfigDiagnosticKind::UnclassifiedOverrideActive,
            path: format!("subagents.unclassified_tool_overrides.\"{tool_id}\""),
            reason: format!(
                "explicit unclassified-tool exception is active for {}: {}",
                override_entry
                    .modes
                    .iter()
                    .map(SubagentCapabilityMode::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                override_entry.reason
            ),
            source: override_entry.source.clone(),
        });
    }
}

fn merge_source(
    value: &toml::Value,
    source: ConfigSource,
    resolved: &mut ResolvedTrustedToolCapabilities,
) {
    let Some(subagents) = value.get("subagents") else {
        return;
    };
    let Some(subagents) = subagents.as_table() else {
        resolved.diagnostics.push(invalid(
            "subagents",
            &source,
            format!("expected a table, got {}", subagents.type_str()),
        ));
        return;
    };

    if let Some(section) = subagents.get("tool_capabilities") {
        match section.as_table() {
            Some(entries) => {
                for (tool_id, raw) in entries {
                    let path = format!("subagents.tool_capabilities.\"{tool_id}\"");
                    if tool_id.trim().is_empty() {
                        resolved.diagnostics.push(invalid(
                            path,
                            &source,
                            "tool ID must not be empty or whitespace".to_owned(),
                        ));
                        continue;
                    }
                    if let Err(reason) = validate_exact_tool_id(tool_id) {
                        resolved.diagnostics.push(invalid(path, &source, reason));
                        continue;
                    }
                    match raw.clone().try_into::<ToolCapabilityDescriptor>() {
                        Ok(descriptor) => match resolved.trusted.insert_classification(
                            tool_id.clone(),
                            descriptor,
                            source.clone(),
                        ) {
                            Ok(()) => {}
                            Err(reason) => {
                                resolved.diagnostics.push(invalid(path, &source, reason));
                            }
                        },
                        Err(error) => resolved.diagnostics.push(invalid(
                            path,
                            &source,
                            format!("invalid capability descriptor: {error}"),
                        )),
                    }
                }
            }
            None => resolved.diagnostics.push(invalid(
                "subagents.tool_capabilities",
                &source,
                format!("expected a table, got {}", section.type_str()),
            )),
        }
    }

    if let Some(section) = subagents.get("unclassified_tool_overrides") {
        match section.as_table() {
            Some(entries) => {
                for (tool_id, raw) in entries {
                    let path = format!("subagents.unclassified_tool_overrides.\"{tool_id}\"");
                    if tool_id.trim().is_empty() {
                        resolved.diagnostics.push(invalid(
                            path,
                            &source,
                            "tool ID must not be empty or whitespace".to_owned(),
                        ));
                        continue;
                    }
                    if let Err(reason) = validate_exact_tool_id(tool_id) {
                        resolved.diagnostics.push(invalid(path, &source, reason));
                        continue;
                    }
                    let parsed = match raw.clone().try_into::<RawUnclassifiedToolOverride>() {
                        Ok(parsed) => parsed,
                        Err(error) => {
                            resolved.diagnostics.push(invalid(
                                path,
                                &source,
                                format!("invalid override: {error}"),
                            ));
                            continue;
                        }
                    };
                    let reason = parsed.reason.trim();
                    if reason.is_empty() {
                        resolved.diagnostics.push(invalid(
                            path,
                            &source,
                            "override reason must not be empty or whitespace".to_owned(),
                        ));
                        continue;
                    }
                    let mut modes = Vec::new();
                    for mode in parsed.modes {
                        if mode != SubagentCapabilityMode::All && !modes.contains(&mode) {
                            modes.push(mode);
                        }
                    }
                    if modes.is_empty() {
                        resolved.diagnostics.push(invalid(
                            path,
                            &source,
                            "override must name at least one restricted mode".to_owned(),
                        ));
                        continue;
                    }
                    let override_entry = UnclassifiedToolOverride {
                        modes,
                        reason: reason.to_owned(),
                        source: source.clone(),
                    };
                    if let Err(reason) = resolved
                        .trusted
                        .insert_unclassified_override(tool_id.clone(), override_entry)
                    {
                        resolved.diagnostics.push(invalid(path, &source, reason));
                    }
                }
            }
            None => resolved.diagnostics.push(invalid(
                "subagents.unclassified_tool_overrides",
                &source,
                format!("expected a table, got {}", section.type_str()),
            )),
        }
    }
}

fn invalid(
    path: impl Into<String>,
    source: &ConfigSource,
    reason: String,
) -> ToolCapabilityConfigDiagnostic {
    ToolCapabilityConfigDiagnostic {
        kind: ToolCapabilityConfigDiagnosticKind::InvalidEntry,
        path: path.into(),
        reason,
        source: source.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_tool_types::SubagentCapabilityMode;
    use xai_tool_types::capability::{ToolCapabilityDescriptor, ToolEffect};

    fn parse(input: &str) -> toml::Value {
        toml::from_str(input).unwrap()
    }

    fn global_source() -> ConfigSource {
        ConfigSource::ConfigToml {
            path: "/home/test/.grok/config.toml".into(),
        }
    }

    fn project_source() -> ConfigSource {
        ConfigSource::Project {
            path: "/repo/.grok/config.toml".into(),
        }
    }

    #[test]
    fn subagent_trusted_exact_id_descriptors_and_overrides_resolve() {
        let global = parse(
            r#"
            [subagents.tool_capabilities."mcp__docs__read"]
            classification = "classified"
            effects = ["local-read", "network-read"]

            [subagents.unclassified_tool_overrides."mcp__legacy__read"]
            modes = ["read-only", "execute"]
            reason = "Audited legacy connector until it publishes a descriptor"
            "#,
        );

        let resolved = resolve_trusted_tool_capabilities_from_values(
            &global,
            global_source(),
            std::iter::empty::<(ConfigSource, &toml::Value)>(),
            false,
        );

        assert_eq!(
            resolved
                .trusted
                .classifications
                .get("mcp__docs__read")
                .map(|capability| &capability.descriptor),
            Some(&ToolCapabilityDescriptor::classified([
                ToolEffect::LocalRead,
                ToolEffect::NetworkRead,
            ]))
        );
        let override_entry = resolved
            .trusted
            .unclassified_overrides
            .get("mcp__legacy__read")
            .unwrap();
        assert_eq!(
            override_entry.modes,
            vec![
                SubagentCapabilityMode::ReadOnly,
                SubagentCapabilityMode::Execute,
            ]
        );
        assert!(!override_entry.reason.trim().is_empty());
        assert_eq!(override_entry.source, global_source());
        assert_eq!(resolved.diagnostics.len(), 1, "every override warns");
    }

    #[test]
    fn subagent_invalid_overrides_are_rejected_with_structured_diagnostics() {
        let global = parse(
            r#"
            [subagents.unclassified_tool_overrides."mcp__blank__reason"]
            modes = ["read-only"]
            reason = "  "

            [subagents.unclassified_tool_overrides."mcp__empty__modes"]
            modes = []
            reason = "documented"

            [subagents.unclassified_tool_overrides."mcp__all__is_not_an_override"]
            modes = ["all"]
            reason = "All already permits unclassified tools"
            "#,
        );

        let resolved = resolve_trusted_tool_capabilities_from_values(
            &global,
            global_source(),
            std::iter::empty::<(ConfigSource, &toml::Value)>(),
            false,
        );

        assert!(resolved.trusted.unclassified_overrides.is_empty());
        assert_eq!(resolved.diagnostics.len(), 3);
        assert!(resolved.diagnostics.iter().all(|d| d.is_warning()));
        assert!(
            resolved
                .diagnostics
                .iter()
                .any(|d| d.path.contains("mcp__blank__reason") && d.reason.contains("reason"))
        );
        assert!(
            resolved
                .diagnostics
                .iter()
                .any(|d| d.path.contains("mcp__empty__modes") && d.reason.contains("mode"))
        );
        assert!(resolved.diagnostics.iter().any(|d| {
            d.path.contains("mcp__all__is_not_an_override") && d.reason.contains("restricted")
        }));
    }

    #[test]
    fn subagent_malformed_descriptor_does_not_classify_the_tool() {
        let global = parse(
            r#"
            [subagents.tool_capabilities."mcp__unsafe__claim"]
            classification = "classified"
            effects = ["not-a-real-effect"]

            [subagents.tool_capabilities."mcp__*"]
            classification = "classified"
            effects = ["local-read"]
            "#,
        );

        let resolved = resolve_trusted_tool_capabilities_from_values(
            &global,
            global_source(),
            std::iter::empty::<(ConfigSource, &toml::Value)>(),
            false,
        );

        assert!(resolved.trusted.classifications.is_empty());
        assert_eq!(resolved.diagnostics.len(), 2);
        assert!(resolved.diagnostics.iter().all(|diagnostic| {
            diagnostic.kind == ToolCapabilityConfigDiagnosticKind::InvalidEntry
        }));
        assert!(resolved.diagnostics.iter().any(|diagnostic| {
            diagnostic.path.contains("mcp__*") && diagnostic.reason.contains("exact ID")
        }));
    }

    #[test]
    fn subagent_project_entries_require_folder_trust_and_remote_meta_is_ignored() {
        let global = parse(
            r#"
            [_meta.subagents.tool_capabilities."mcp__remote__claimed_safe"]
            classification = "classified"
            effects = ["local-read"]

            [subagents.tool_capabilities."mcp__project__read"]
            classification = "classified"
            effects = ["network-read"]
            "#,
        );
        let project = parse(
            r#"
            [subagents.tool_capabilities."mcp__project__read"]
            classification = "classified"
            effects = ["local-read"]

            [subagents.unclassified_tool_overrides."mcp__project__legacy"]
            modes = ["read-only"]
            reason = "Trusted project audit"
            "#,
        );

        let untrusted = resolve_trusted_tool_capabilities_from_values(
            &global,
            global_source(),
            [(project_source(), &project)],
            false,
        );
        assert_eq!(
            untrusted
                .trusted
                .classifications
                .get("mcp__project__read")
                .map(|capability| &capability.descriptor),
            Some(&ToolCapabilityDescriptor::classified([
                ToolEffect::NetworkRead
            ]))
        );
        assert!(untrusted.trusted.unclassified_overrides.is_empty());

        let trusted = resolve_trusted_tool_capabilities_from_values(
            &global,
            global_source(),
            [(project_source(), &project)],
            true,
        );
        assert_eq!(trusted.trusted.classifications.len(), 1);
        assert_eq!(
            trusted
                .trusted
                .classifications
                .get("mcp__project__read")
                .map(|capability| &capability.descriptor),
            Some(&ToolCapabilityDescriptor::classified([
                ToolEffect::LocalRead
            ]))
        );
        assert_eq!(
            trusted.trusted.unclassified_overrides["mcp__project__legacy"].source,
            project_source()
        );
        assert!(
            !trusted
                .trusted
                .classifications
                .contains_key("mcp__remote__claimed_safe")
        );
    }
}
