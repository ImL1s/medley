//! Inspect JSON and declarative plugin/agent syntax.

use serde::{Deserialize, Serialize};
use xai_grok_agent::config::{AgentDefinition, ModelOverride};

use super::resolve::validate_published_receipt;
use super::types::{
    CapabilityRequirements, DiscoveredCapability, INSPECT_SCHEMA, NativeModelSelection,
    NativeRouteError, NativeSubagentRouteRequest, RejectionCode, ResumePin, RouteReceipt,
    SCHEMA_VERSION, discover_capabilities, reject_secret_text,
};

/// Versioned inspect document for consumers. Discovery performs no inference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectDocument {
    pub schema: String,
    pub schema_version: u32,
    pub host: String,
    pub capabilities: Vec<DiscoveredCapability>,
    #[serde(default)]
    pub receipts: Vec<RouteReceipt>,
}

pub fn inspect_document(receipts: Vec<RouteReceipt>) -> Result<InspectDocument, NativeRouteError> {
    for receipt in &receipts {
        validate_published_receipt(receipt)?;
    }
    Ok(InspectDocument {
        schema: INSPECT_SCHEMA.into(),
        schema_version: SCHEMA_VERSION,
        host: "medley".into(),
        capabilities: discover_capabilities(),
        receipts,
    })
}

/// Smallest generic declarative extension (`model` / `models` / routingRequirements).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeclarativeNativeRouteSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `None` means omitted (inherit). `Some([])` is invalid, not inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub routing_requirements: CapabilityRequirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_policy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_policy_digest: Option<String>,
}

pub fn parse_declarative_spec(
    spec: DeclarativeNativeRouteSpec,
) -> Result<NativeSubagentRouteRequest, NativeRouteError> {
    if let Some(model) = &spec.model {
        reject_secret_text(model, "model")?;
    }
    if let Some(models) = &spec.models {
        for id in models {
            reject_secret_text(id, "models")?;
        }
    }
    let inherit = spec
        .model
        .as_deref()
        .is_none_or(|value| value.is_empty() || value.eq_ignore_ascii_case("inherit"));
    match spec.models {
        Some(models) if models.is_empty() => {
            return Err(NativeRouteError::Rejected(
                RejectionCode::EmptyCandidates,
                "empty models lists are invalid, not inherit".into(),
            ));
        }
        Some(models) => {
            if !inherit {
                return Err(NativeRouteError::Rejected(
                    RejectionCode::ConflictingSyntax,
                    "model and models cannot both declare a non-inherit exact route".into(),
                ));
            }
            if models.iter().any(|id| id.is_empty()) {
                return Err(NativeRouteError::Rejected(
                    RejectionCode::EmptyCandidates,
                    "empty catalog ids are invalid".into(),
                ));
            }
            return Ok(NativeSubagentRouteRequest {
                schema_version: SCHEMA_VERSION,
                selection: NativeModelSelection::OrderedCandidates {
                    catalog_ids: models,
                },
                required_capabilities: spec.routing_requirements,
                capability_ceiling: None,
                consumer_policy_id: spec.consumer_policy_id,
                consumer_policy_digest: spec.consumer_policy_digest,
                parent_catalog_id: None,
                parent_session_id: None,
                child_session_id: None,
                resume: None,
            });
        }
        None if inherit => {
            return Ok(NativeSubagentRouteRequest {
                schema_version: SCHEMA_VERSION,
                selection: NativeModelSelection::Inherit,
                required_capabilities: spec.routing_requirements,
                capability_ceiling: None,
                consumer_policy_id: spec.consumer_policy_id,
                consumer_policy_digest: spec.consumer_policy_digest,
                parent_catalog_id: None,
                parent_session_id: None,
                child_session_id: None,
                resume: None,
            });
        }
        None => {}
    }
    let catalog_id = spec.model.unwrap_or_default();
    if catalog_id.is_empty() {
        return Err(NativeRouteError::Rejected(
            RejectionCode::ExactModelMissing,
            "exact model must be a non-empty catalog id".into(),
        ));
    }
    Ok(NativeSubagentRouteRequest {
        schema_version: SCHEMA_VERSION,
        selection: NativeModelSelection::Exact { catalog_id },
        required_capabilities: spec.routing_requirements,
        capability_ceiling: None,
        consumer_policy_id: spec.consumer_policy_id,
        consumer_policy_digest: spec.consumer_policy_digest,
        parent_catalog_id: None,
        parent_session_id: None,
        child_session_id: None,
        resume: None,
    })
}

/// Build a native route request from a parsed `AgentDefinition`.
pub fn request_from_agent_definition(
    def: &AgentDefinition,
    parent_catalog_id: Option<String>,
    parent_session_id: Option<String>,
    child_session_id: Option<String>,
    resume: Option<ResumePin>,
) -> Result<NativeSubagentRouteRequest, NativeRouteError> {
    let model = match &def.model {
        ModelOverride::Inherit => None,
        ModelOverride::Override(id) => Some(id.clone()),
    };
    let mut request = parse_declarative_spec(DeclarativeNativeRouteSpec {
        model,
        models: if def.models.is_empty() {
            None
        } else {
            Some(def.models.clone())
        },
        routing_requirements: CapabilityRequirements {
            structured_output: def.routing_requirements.structured_output,
            minimum_context_tokens: def.routing_requirements.minimum_context_tokens,
            required_harness: def.routing_requirements.required_harness.clone(),
            local_only: def.routing_requirements.local_only,
            required_named_capabilities: Vec::new(),
        },
        consumer_policy_id: None,
        consumer_policy_digest: None,
    })?;
    request.parent_catalog_id = parent_catalog_id;
    request.parent_session_id = parent_session_id;
    request.child_session_id = child_session_id;
    request.resume = resume;
    Ok(request)
}

/// Secret-free usage projection keyed by the committed catalog route.
pub fn usage_facts_from_receipt(receipt: &RouteReceipt) -> serde_json::Value {
    serde_json::json!({
        "catalogId": receipt.selected_catalog_id,
        "wireModel": receipt.selected_wire_model,
        "accessProfile": receipt.access_profile,
        "routeDigest": receipt.route_digest,
        "attempt": receipt.attempt,
        "selectionMode": receipt.selection_mode,
    })
}
