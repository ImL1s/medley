//! Versioned capability, request, receipt, and rejection types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Inspect / request schema version for this slice.
pub const SCHEMA_VERSION: u32 = 1;

/// Capability version string advertised when a capability is implemented.
pub const CAPABILITY_VERSION: &str = "v1";

/// Inspect JSON schema id consumed by orchestration adapters such as OMG.
pub const INSPECT_SCHEMA: &str = "medley.native-subagent-route.inspect/v1";

/// Receipt JSON schema id.
pub const RECEIPT_SCHEMA: &str = "medley.native-route-receipt.v1";

/// Capability identifiers. Single-source names for code, inspect, and docs.
pub const CAP_EXACT_MODEL: &str = "medley.native-exact-model.v1";
pub const CAP_ORDERED_CANDIDATES: &str = "medley.native-ordered-candidates.v1";
pub const CAP_ROUTE_RECEIPT: &str = "medley.native-route-receipt.v1";
pub const CAP_MODEL_FAMILY_METADATA: &str = "medley.native-model-family-metadata.v1";
pub const CAP_REPLAY_SAFE_FALLBACK: &str = "medley.native-replay-safe-fallback.v1";

/// Capability ids in stable order.
pub const CAPABILITY_IDS: [&str; 5] = [
    CAP_EXACT_MODEL,
    CAP_ORDERED_CANDIDATES,
    CAP_ROUTE_RECEIPT,
    CAP_MODEL_FAMILY_METADATA,
    CAP_REPLAY_SAFE_FALLBACK,
];

/// One capability identifier.
pub type CapabilityId = &'static str;

/// Negotiated capability state. Only `Supported` authorizes use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Unavailable,
    Incompatible,
    Unknown,
}

impl CapabilityState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unavailable => "unavailable",
            Self::Incompatible => "incompatible",
            Self::Unknown => "unknown",
        }
    }
}

/// Read-only capability discovery. Performs no inference request.
pub fn discover_capabilities() -> Vec<DiscoveredCapability> {
    CAPABILITY_IDS
        .iter()
        .map(|&id| DiscoveredCapability {
            capability_id: id.to_string(),
            state: implemented_state(id),
            version: match implemented_state(id) {
                CapabilityState::Supported => Some(CAPABILITY_VERSION.to_string()),
                _ => None,
            },
            reason: implemented_reason(id).to_string(),
        })
        .collect()
}

fn implemented_state(id: &str) -> CapabilityState {
    match id {
        CAP_EXACT_MODEL | CAP_ORDERED_CANDIDATES | CAP_ROUTE_RECEIPT => CapabilityState::Supported,
        CAP_MODEL_FAMILY_METADATA | CAP_REPLAY_SAFE_FALLBACK => CapabilityState::Unsupported,
        _ => CapabilityState::Unknown,
    }
}

fn implemented_reason(id: &str) -> &'static str {
    match id {
        CAP_EXACT_MODEL => "exact catalog routes fail closed; never inherit parent",
        CAP_ORDERED_CANDIDATES => "deterministic offline first-eligible catalog selection",
        CAP_ROUTE_RECEIPT => "secret-free immutable receipt + digest",
        CAP_MODEL_FAMILY_METADATA => "not implemented in this slice",
        CAP_REPLAY_SAFE_FALLBACK => {
            "runtime fallback remains Medley #18; this slice refuses replay"
        }
        _ => "unknown capability",
    }
}

/// One discovered capability row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredCapability {
    pub capability_id: String,
    pub state: CapabilityState,
    pub version: Option<String>,
    pub reason: String,
}

/// Model selection modes. Inherit is the only mode that may choose parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NativeModelSelection {
    Inherit,
    Exact { catalog_id: String },
    OrderedCandidates { catalog_ids: Vec<String> },
}

impl NativeModelSelection {
    pub fn mode_name(&self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Exact { .. } => "exact",
            Self::OrderedCandidates { .. } => "ordered_candidates",
        }
    }

    pub fn requested_catalog_ids(&self) -> Vec<String> {
        match self {
            Self::Inherit => Vec::new(),
            Self::Exact { catalog_id } => vec![catalog_id.clone()],
            Self::OrderedCandidates { catalog_ids } => catalog_ids.clone(),
        }
    }
}

/// Hard requirements that unknown/unready state must not satisfy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityRequirements {
    #[serde(default)]
    pub structured_output: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_context_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_harness: Option<String>,
    #[serde(default)]
    pub local_only: bool,
    /// Opaque capability names that must be present on the catalog route.
    #[serde(default)]
    pub required_named_capabilities: Vec<String>,
}

/// Plugin-facing native route request. Contains no credentials or endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSubagentRouteRequest {
    pub schema_version: u32,
    pub selection: NativeModelSelection,
    #[serde(default)]
    pub required_capabilities: CapabilityRequirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_ceiling: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_policy_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_catalog_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ResumePin>,
}

/// Resume pin: source route/receipt stay bound. Same wire slug cannot rebind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumePin {
    pub source_catalog_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_receipt_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_route_key: Option<String>,
}

/// Native versus consumer-owned external executor. Medley owns only Native.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerRoute {
    Native(Box<NativeSubagentRouteRequest>),
    ExternalExecutor { descriptor: String },
}

/// Typed rejection reasons. Do not infer from display strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionCode {
    ExactModelMissing,
    RouteUnready,
    CredentialMissing,
    CapabilityUnknown,
    CapabilityMismatch,
    HarnessIncompatible,
    LocalOnlyViolation,
    AccessScopeBlocked,
    CrossBillingBlocked,
    UnsupportedContract,
    IncompatibleSchema,
    ResumeRoutePinned,
    FallbackReplayUnsafe,
    StaleGeneration,
    EmptyCandidates,
    ConflictingSyntax,
    UnknownReadiness,
    DuplicateRequest,
}

impl RejectionCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactModelMissing => "exact_model_missing",
            Self::RouteUnready => "route_unready",
            Self::CredentialMissing => "credential_missing",
            Self::CapabilityUnknown => "capability_unknown",
            Self::CapabilityMismatch => "capability_mismatch",
            Self::HarnessIncompatible => "harness_incompatible",
            Self::LocalOnlyViolation => "local_only_violation",
            Self::AccessScopeBlocked => "access_scope_blocked",
            Self::CrossBillingBlocked => "cross_billing_blocked",
            Self::UnsupportedContract => "unsupported_contract",
            Self::IncompatibleSchema => "incompatible_schema",
            Self::ResumeRoutePinned => "resume_route_pinned",
            Self::FallbackReplayUnsafe => "fallback_replay_unsafe",
            Self::StaleGeneration => "stale_generation",
            Self::EmptyCandidates => "empty_candidates",
            Self::ConflictingSyntax => "conflicting_syntax",
            Self::UnknownReadiness => "unknown_readiness",
            Self::DuplicateRequest => "duplicate_request",
        }
    }
}

impl std::fmt::Display for RejectionCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One rejected candidate with a typed reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedCandidate {
    pub catalog_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_key: Option<String>,
    pub reason_code: RejectionCode,
    pub message: String,
}

/// Immutable secret-free receipt bound to the selected execution route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteReceipt {
    pub schema: String,
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    pub selection_mode: String,
    pub requested_catalog_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_policy_digest: Option<String>,
    pub selected_catalog_id: String,
    pub selected_wire_model: String,
    pub route_key: String,
    pub access_profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_ceiling: Option<String>,
    pub selection_provenance: String,
    pub rejected_candidates: Vec<RejectedCandidate>,
    pub route_digest: String,
    pub attempt: u32,
    pub created_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_source_receipt: Option<String>,
}

impl RouteReceipt {
    /// Canonical JSON used for the digest. Keys stay sorted via BTreeMap.
    pub fn canonical_payload(&self) -> BTreeMap<String, serde_json::Value> {
        let mut map = BTreeMap::new();
        map.insert(
            "schema".into(),
            serde_json::Value::String(self.schema.clone()),
        );
        map.insert(
            "schema_version".into(),
            serde_json::Value::Number(self.schema_version.into()),
        );
        map.insert(
            "selection_mode".into(),
            serde_json::Value::String(self.selection_mode.clone()),
        );
        map.insert(
            "requested_catalog_ids".into(),
            serde_json::Value::Array(
                self.requested_catalog_ids
                    .iter()
                    .map(|id| serde_json::Value::String(id.clone()))
                    .collect(),
            ),
        );
        map.insert(
            "selected_catalog_id".into(),
            serde_json::Value::String(self.selected_catalog_id.clone()),
        );
        map.insert(
            "selected_wire_model".into(),
            serde_json::Value::String(self.selected_wire_model.clone()),
        );
        map.insert(
            "route_key".into(),
            serde_json::Value::String(self.route_key.clone()),
        );
        map.insert(
            "access_profile".into(),
            serde_json::Value::String(self.access_profile.clone()),
        );
        map.insert(
            "selection_provenance".into(),
            serde_json::Value::String(self.selection_provenance.clone()),
        );
        map.insert(
            "rejected_candidates".into(),
            serde_json::Value::Array(
                self.rejected_candidates
                    .iter()
                    .map(|row| {
                        let mut item = serde_json::Map::new();
                        item.insert(
                            "catalog_id".into(),
                            serde_json::Value::String(row.catalog_id.clone()),
                        );
                        item.insert(
                            "reason_code".into(),
                            serde_json::Value::String(row.reason_code.as_str().into()),
                        );
                        if let Some(wire) = &row.wire_model {
                            item.insert(
                                "wire_model".into(),
                                serde_json::Value::String(wire.clone()),
                            );
                        }
                        if let Some(key) = &row.route_key {
                            item.insert("route_key".into(), serde_json::Value::String(key.clone()));
                        }
                        serde_json::Value::Object(item)
                    })
                    .collect(),
            ),
        );
        map.insert(
            "attempt".into(),
            serde_json::Value::Number(self.attempt.into()),
        );
        if let Some(id) = &self.consumer_policy_id {
            map.insert(
                "consumer_policy_id".into(),
                serde_json::Value::String(id.clone()),
            );
        }
        if let Some(digest) = &self.consumer_policy_digest {
            map.insert(
                "consumer_policy_digest".into(),
                serde_json::Value::String(digest.clone()),
            );
        }
        if let Some(parent) = &self.parent_session_id {
            map.insert(
                "parent_session_id".into(),
                serde_json::Value::String(parent.clone()),
            );
        }
        if let Some(child) = &self.child_session_id {
            map.insert(
                "child_session_id".into(),
                serde_json::Value::String(child.clone()),
            );
        }
        if let Some(harness) = &self.harness {
            map.insert("harness".into(), serde_json::Value::String(harness.clone()));
        }
        if let Some(ceiling) = &self.capability_ceiling {
            map.insert(
                "capability_ceiling".into(),
                serde_json::Value::String(ceiling.clone()),
            );
        }
        if let Some(resume) = &self.resume_source_receipt {
            map.insert(
                "resume_source_receipt".into(),
                serde_json::Value::String(resume.clone()),
            );
        }
        map
    }
}

/// Successful resolution: selected catalog route plus receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSubagentRouteResult {
    pub selected_catalog_id: String,
    pub selected_wire_model: String,
    pub route_key: String,
    pub receipt: RouteReceipt,
    pub rejected_candidates: Vec<RejectedCandidate>,
}

/// Lifecycle facts for later #18 fallback. This slice records them only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptLifecycleFact {
    AttemptStarted,
    FirstProviderByteSeen,
    VisibleOutputCommitted,
    ToolCallEmitted,
    ToolSideEffectStarted,
    AttemptTerminal,
}

/// Fail-closed native-route errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NativeRouteError {
    #[error("unsupported schema_version {0}; expected {SCHEMA_VERSION}")]
    IncompatibleSchema(u32),
    #[error("{0}: {1}")]
    Rejected(RejectionCode, String),
    #[error("external executor routes are consumer-owned; Medley does not resolve them")]
    ExternalExecutorUnsupported,
    #[error("request contained forbidden secret material")]
    SecretMaterial,
}

impl NativeRouteError {
    pub fn code(&self) -> RejectionCode {
        match self {
            Self::IncompatibleSchema(_) => RejectionCode::IncompatibleSchema,
            Self::Rejected(code, _) => *code,
            Self::ExternalExecutorUnsupported => RejectionCode::UnsupportedContract,
            Self::SecretMaterial => RejectionCode::UnsupportedContract,
        }
    }
}

const SECRET_NEEDLES: [&str; 7] = [
    "sk-",
    "bearer ",
    "acct_",
    "-----begin ",
    "api_key",
    "authorization",
    "x-api-key",
];

/// Reject credential/header/query/account sentinels in consumer-supplied text.
pub fn reject_secret_text(value: &str, label: &str) -> Result<(), NativeRouteError> {
    let lower = value.to_ascii_lowercase();
    for needle in SECRET_NEEDLES {
        if lower.contains(needle) {
            return Err(NativeRouteError::Rejected(
                RejectionCode::UnsupportedContract,
                format!("{label} contains forbidden material"),
            ));
        }
    }
    Ok(())
}

pub fn reject_secret_opt(value: Option<&str>, label: &str) -> Result<(), NativeRouteError> {
    if let Some(text) = value {
        reject_secret_text(text, label)?;
    }
    Ok(())
}
