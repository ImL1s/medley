//! Typed UX snapshot shared by /agents, inspect JSON, and consumer adapters.

use serde::{Deserialize, Serialize};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use xai_grok_agent::config::{AgentDefinition, ModelOverride};

use super::resolve::validate_published_receipt;
use super::types::{NativeSubagentRouteResult, RejectionCode};

/// Selection intent shown in compact rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSelectionMode {
    Inherit,
    Exact,
    OrderedCandidates,
}

impl AgentSelectionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Exact => "exact",
            Self::OrderedCandidates => "ordered_candidates",
        }
    }
}

/// Non-color route/readiness status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteStatus {
    Ready,
    Unsupported,
    Unavailable,
    Incompatible,
    Unknown,
    Blocked,
}

impl RouteStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Unsupported => "unsupported",
            Self::Unavailable => "unavailable",
            Self::Incompatible => "incompatible",
            Self::Unknown => "unknown",
            Self::Blocked => "blocked",
        }
    }
}

/// Secret-free snapshot used by TUI, inspect, JSON, and OMG adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRouteUxSnapshot {
    pub generation: u64,
    pub agent_id: String,
    pub display_name: String,
    pub scope: String,
    pub enabled: bool,
    pub active: bool,
    pub default_for_new_sessions: bool,
    pub selection_mode: AgentSelectionMode,
    pub requested_model_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_digest: Option<String>,
    pub route_status: RouteStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_catalog_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_wire_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_floor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_receipt_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_source_receipt: Option<String>,
    pub rejected_candidates: Vec<(String, RejectionCode)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fallback_admitted: Option<bool>,
}

/// Distinct lifecycle labels. Same-route retry is never shown as fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    SelectingRoute,
    RunningAttempt,
    RetryingSameRoute,
    FallingBack,
    FallbackRefused,
    ResumedFromPriorReceipt,
    Completed,
    Failed,
    Cancelled,
}

impl LifecyclePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelectingRoute => "selecting route",
            Self::RunningAttempt => "running attempt",
            Self::RetryingSameRoute => "retrying same route",
            Self::FallingBack => "falling back to another route",
            Self::FallbackRefused => "fallback refused",
            Self::ResumedFromPriorReceipt => "resumed from prior receipt",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Idle configuration snapshots have no lifecycle. `SelectingRoute` is only
/// for an attached in-flight selection, not for opening `/agents`.
pub fn lifecycle_phase_for_snapshot(snapshot: &AgentRouteUxSnapshot) -> Option<LifecyclePhase> {
    if snapshot.resume_source_receipt.is_some() {
        return Some(LifecyclePhase::ResumedFromPriorReceipt);
    }
    match snapshot.last_fallback_admitted {
        Some(false) => return Some(LifecyclePhase::FallbackRefused),
        Some(true) => return Some(LifecyclePhase::FallingBack),
        None => {}
    }
    if snapshot.attempt.is_some_and(|n| n > 1) {
        return Some(LifecyclePhase::RetryingSameRoute);
    }
    if snapshot.attempt.is_some() {
        return Some(LifecyclePhase::RunningAttempt);
    }
    None
}

/// One lifecycle card line. Labels stay distinct; do not parse this string.
pub fn format_lifecycle_line(phase: LifecyclePhase, attempt: Option<u32>) -> String {
    match (phase, attempt) {
        (LifecyclePhase::RunningAttempt, Some(n)) => {
            format!("  Lifecycle: running attempt {n}")
        }
        (LifecyclePhase::RetryingSameRoute, Some(n)) => {
            format!("  Lifecycle: retrying same route (attempt {n})")
        }
        _ => format!("  Lifecycle: {}", phase.as_str()),
    }
}

/// Compact row: identity, status, selection/route, readiness, capability floor.
pub fn format_compact_row(snapshot: &AgentRouteUxSnapshot, max_width: usize) -> String {
    let status = if snapshot.active {
        "active"
    } else if snapshot.default_for_new_sessions {
        "default"
    } else if snapshot.enabled {
        "enabled"
    } else {
        "off"
    };
    let selection = match snapshot.selection_mode {
        AgentSelectionMode::OrderedCandidates => {
            if let Some(id) = &snapshot.selected_catalog_id {
                id.clone()
            } else if snapshot.requested_model_refs.len() >= 2 {
                format!("{} candidates", snapshot.requested_model_refs.len())
            } else {
                "ordered".into()
            }
        }
        AgentSelectionMode::Exact => snapshot
            .selected_catalog_id
            .clone()
            .or_else(|| snapshot.requested_model_refs.first().cloned())
            .unwrap_or_else(|| "exact".into()),
        AgentSelectionMode::Inherit => "inherit".into(),
    };
    let floor = snapshot.capability_floor.as_deref().unwrap_or("-");
    let mut row = format!(
        "{}  {}  {}  {}  {}  {}",
        snapshot.display_name,
        snapshot.scope,
        status,
        selection,
        snapshot.route_status.as_str(),
        floor
    );
    if max_width > 0 {
        row = truncate_to_display_width(&row, max_width);
    }
    row
}

fn truncate_to_display_width(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used.saturating_add(width) > max_width {
            break;
        }
        out.push(ch);
        used += width;
    }
    out
}

/// Expanded details. Never parse these strings to implement another surface.
pub fn format_route_detail(snapshot: &AgentRouteUxSnapshot) -> Vec<String> {
    let mut lines = vec![
        format!("  Selection: {}", snapshot.selection_mode.as_str()),
        format!("  Route status: {}", snapshot.route_status.as_str()),
    ];
    if let Some(id) = &snapshot.selected_catalog_id {
        lines.push(format!("  Selected catalog: {id}"));
    }
    if let Some(wire) = &snapshot.selected_wire_model {
        lines.push(format!("  Selected wire model: {wire}"));
    }
    if !snapshot.requested_model_refs.is_empty() {
        lines.push(format!(
            "  Requested: {}",
            snapshot.requested_model_refs.join(", ")
        ));
    }
    if let Some(digest) = &snapshot.route_receipt_digest {
        lines.push(format!("  Receipt digest: {digest}"));
    }
    if let Some(attempt) = snapshot.attempt {
        lines.push(format!("  Attempt: {attempt}"));
    }
    if let Some(resume) = &snapshot.resume_source_receipt {
        lines.push(format!("  Resume source receipt: {resume}"));
    }
    if !snapshot.rejected_candidates.is_empty() {
        lines.push("  Rejected candidates:".into());
        for (id, code) in &snapshot.rejected_candidates {
            lines.push(format!("    - {id} ({})", code.as_str()));
        }
    }
    if let Some(phase) = lifecycle_phase_for_snapshot(snapshot) {
        lines.push(format_lifecycle_line(phase, snapshot.attempt));
    }
    if snapshot.last_fallback_admitted == Some(false) {
        lines.push("  Last fallback: refused".into());
    } else if snapshot.last_fallback_admitted == Some(true) {
        lines.push("  Last fallback: admitted".into());
    }
    lines
}

/// Snapshot from existing AgentDefinition `model:` inherit/exact only.
pub fn snapshot_from_model_override(
    agent_id: &str,
    display_name: &str,
    scope: &str,
    enabled: bool,
    active: bool,
    default_for_new_sessions: bool,
    model: &ModelOverride,
    capability_floor: Option<&str>,
    generation: u64,
) -> AgentRouteUxSnapshot {
    let (selection_mode, refs, status) = match model {
        ModelOverride::Inherit => (
            AgentSelectionMode::Inherit,
            Vec::new(),
            RouteStatus::Unknown,
        ),
        ModelOverride::Override(id) => (
            AgentSelectionMode::Exact,
            vec![id.clone()],
            RouteStatus::Unknown,
        ),
    };
    AgentRouteUxSnapshot {
        generation,
        agent_id: agent_id.into(),
        display_name: display_name.into(),
        scope: scope.into(),
        enabled,
        active,
        default_for_new_sessions,
        selection_mode,
        requested_model_refs: refs,
        policy_id: None,
        policy_digest: None,
        route_status: status,
        selected_catalog_id: None,
        selected_wire_model: None,
        capability_floor: capability_floor.map(str::to_string),
        route_receipt_digest: None,
        attempt: None,
        resume_source_receipt: None,
        rejected_candidates: Vec::new(),
        last_fallback_admitted: None,
    }
}

/// Snapshot from a real `AgentDefinition`, including `models:` ordered lists.
pub fn snapshot_from_agent_definition(
    agent_id: &str,
    display_name: &str,
    scope: &str,
    enabled: bool,
    active: bool,
    default_for_new_sessions: bool,
    definition: &AgentDefinition,
    capability_floor: Option<&str>,
    generation: u64,
) -> AgentRouteUxSnapshot {
    if definition.models.is_empty() {
        return snapshot_from_model_override(
            agent_id,
            display_name,
            scope,
            enabled,
            active,
            default_for_new_sessions,
            &definition.model,
            capability_floor,
            generation,
        );
    }
    AgentRouteUxSnapshot {
        generation,
        agent_id: agent_id.into(),
        display_name: display_name.into(),
        scope: scope.into(),
        enabled,
        active,
        default_for_new_sessions,
        selection_mode: AgentSelectionMode::OrderedCandidates,
        requested_model_refs: definition.models.clone(),
        policy_id: None,
        policy_digest: None,
        route_status: RouteStatus::Unknown,
        selected_catalog_id: None,
        selected_wire_model: None,
        capability_floor: capability_floor.map(str::to_string),
        route_receipt_digest: None,
        attempt: None,
        resume_source_receipt: None,
        rejected_candidates: Vec::new(),
        last_fallback_admitted: None,
    }
}

pub fn snapshot_from_resolution(
    agent_id: &str,
    display_name: &str,
    scope: &str,
    enabled: bool,
    active: bool,
    default_for_new_sessions: bool,
    result: &NativeSubagentRouteResult,
    capability_floor: Option<&str>,
    generation: u64,
) -> AgentRouteUxSnapshot {
    let mode = match result.receipt.selection_mode.as_str() {
        "exact" => AgentSelectionMode::Exact,
        "ordered_candidates" => AgentSelectionMode::OrderedCandidates,
        "inherit" => AgentSelectionMode::Inherit,
        _ => {
            return AgentRouteUxSnapshot {
                generation,
                agent_id: agent_id.into(),
                display_name: display_name.into(),
                scope: scope.into(),
                enabled,
                active,
                default_for_new_sessions,
                selection_mode: AgentSelectionMode::Inherit,
                requested_model_refs: result.receipt.requested_catalog_ids.clone(),
                policy_id: result.receipt.consumer_policy_id.clone(),
                policy_digest: result.receipt.consumer_policy_digest.clone(),
                route_status: RouteStatus::Incompatible,
                selected_catalog_id: None,
                selected_wire_model: None,
                capability_floor: capability_floor.map(str::to_string),
                route_receipt_digest: None,
                attempt: None,
                resume_source_receipt: None,
                rejected_candidates: Vec::new(),
            };
        }
    };
    if validate_published_receipt(&result.receipt).is_err() {
        return AgentRouteUxSnapshot {
            generation,
            agent_id: agent_id.into(),
            display_name: display_name.into(),
            scope: scope.into(),
            enabled,
            active,
            default_for_new_sessions,
            selection_mode: mode,
            requested_model_refs: result.receipt.requested_catalog_ids.clone(),
            policy_id: result.receipt.consumer_policy_id.clone(),
            policy_digest: result.receipt.consumer_policy_digest.clone(),
            route_status: RouteStatus::Incompatible,
            selected_catalog_id: None,
            selected_wire_model: None,
            capability_floor: capability_floor.map(str::to_string),
            route_receipt_digest: None,
            attempt: None,
            resume_source_receipt: None,
            rejected_candidates: Vec::new(),
            last_fallback_admitted: None,
        };
    }
    AgentRouteUxSnapshot {
        generation,
        agent_id: agent_id.into(),
        display_name: display_name.into(),
        scope: scope.into(),
        enabled,
        active,
        default_for_new_sessions,
        selection_mode: mode,
        requested_model_refs: result.receipt.requested_catalog_ids.clone(),
        policy_id: result.receipt.consumer_policy_id.clone(),
        policy_digest: result.receipt.consumer_policy_digest.clone(),
        route_status: RouteStatus::Ready,
        selected_catalog_id: Some(result.receipt.selected_catalog_id.clone()),
        selected_wire_model: Some(result.receipt.selected_wire_model.clone()),
        capability_floor: capability_floor.map(str::to_string),
        route_receipt_digest: Some(result.receipt.route_digest.clone()),
        attempt: Some(result.receipt.attempt),
        resume_source_receipt: result.receipt.resume_source_receipt.clone(),
        rejected_candidates: result
            .receipt
            .rejected_candidates
            .iter()
            .map(|row| (row.catalog_id.clone(), row.reason_code))
            .collect(),
        last_fallback_admitted: None,
    }
}
