//! Inspect JSON and declarative plugin/agent syntax.

use serde::{Deserialize, Serialize};

use super::resolve::validate_published_receipt;
use super::types::{
    CapabilityRequirements, DiscoveredCapability, INSPECT_SCHEMA, NativeModelSelection,
    NativeRouteError, NativeSubagentRouteRequest, RejectionCode, RouteReceipt, SCHEMA_VERSION,
    discover_capabilities, reject_secret_text,
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
