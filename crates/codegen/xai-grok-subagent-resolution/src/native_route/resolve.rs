//! Deterministic offline resolver over a synthetic catalog.

use sha2::{Digest, Sha256};

use super::types::{
    AttemptLifecycleFact, NativeModelSelection, NativeRouteError, NativeSubagentRouteRequest,
    NativeSubagentRouteResult, RECEIPT_SCHEMA, RejectedCandidate, RejectionCode, RouteReceipt,
    SCHEMA_VERSION, WorkerRoute, reject_secret_opt, reject_secret_text,
};

/// One catalog route. Duplicate wire slugs stay distinct via `route_key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticCatalogEntry {
    pub catalog_id: String,
    pub wire_model: String,
    pub route_key: String,
    pub access_profile: String,
    pub ready: bool,
    pub unknown_readiness: bool,
    pub local_only: bool,
    pub harness: Option<String>,
    pub context_tokens: Option<u32>,
    pub structured_output: bool,
    pub named_capabilities: Vec<String>,
}

/// Ordered synthetic catalog used by tests and inspect fixtures.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyntheticCatalog {
    pub entries: Vec<SyntheticCatalogEntry>,
}

impl SyntheticCatalog {
    pub fn get(&self, catalog_id: &str) -> Option<&SyntheticCatalogEntry> {
        self.entries
            .iter()
            .find(|entry| entry.catalog_id == catalog_id)
    }
}

/// Resolve a native request. Offline: no network, no task content.
pub fn resolve_native_route(
    request: &NativeSubagentRouteRequest,
    catalog: &SyntheticCatalog,
    now_unix_ms: u64,
    attempt: u32,
) -> Result<NativeSubagentRouteResult, NativeRouteError> {
    validate_request(request)?;
    if let Some(pin) = &request.resume {
        return resolve_resume(request, catalog, pin, now_unix_ms, attempt);
    }
    match &request.selection {
        NativeModelSelection::Inherit => resolve_inherit(request, catalog, now_unix_ms, attempt),
        NativeModelSelection::Exact { catalog_id } => {
            resolve_exact(request, catalog, catalog_id, now_unix_ms, attempt)
        }
        NativeModelSelection::OrderedCandidates { catalog_ids } => {
            resolve_ordered(request, catalog, catalog_ids, now_unix_ms, attempt)
        }
    }
}

/// Medley resolves only native routes. External descriptors stay consumer-owned.
pub fn resolve_worker_route(
    route: &WorkerRoute,
    catalog: &SyntheticCatalog,
    now_unix_ms: u64,
    attempt: u32,
) -> Result<NativeSubagentRouteResult, NativeRouteError> {
    match route {
        WorkerRoute::Native(request) => {
            resolve_native_route(request.as_ref(), catalog, now_unix_ms, attempt)
        }
        WorkerRoute::ExternalExecutor { .. } => Err(NativeRouteError::ExternalExecutorUnsupported),
    }
}

fn validate_request(request: &NativeSubagentRouteRequest) -> Result<(), NativeRouteError> {
    if request.schema_version != SCHEMA_VERSION {
        return Err(NativeRouteError::IncompatibleSchema(request.schema_version));
    }
    reject_secret_opt(request.consumer_policy_id.as_deref(), "consumer_policy_id")?;
    reject_secret_opt(
        request.consumer_policy_digest.as_deref(),
        "consumer_policy_digest",
    )?;
    reject_secret_opt(request.parent_catalog_id.as_deref(), "parent_catalog_id")?;
    reject_secret_opt(request.parent_session_id.as_deref(), "parent_session_id")?;
    reject_secret_opt(request.child_session_id.as_deref(), "child_session_id")?;
    reject_secret_opt(request.capability_ceiling.as_deref(), "capability_ceiling")?;
    if let Some(req) = &request.required_capabilities.required_harness {
        reject_secret_text(req, "required_harness")?;
    }
    for name in &request.required_capabilities.required_named_capabilities {
        reject_secret_text(name, "required_named_capabilities")?;
    }
    if let Some(pin) = &request.resume {
        reject_secret_text(&pin.source_catalog_id, "resume.source_catalog_id")?;
        reject_secret_opt(
            pin.source_receipt_digest.as_deref(),
            "resume.source_receipt_digest",
        )?;
        reject_secret_opt(pin.source_route_key.as_deref(), "resume.source_route_key")?;
        if pin
            .source_route_key
            .as_deref()
            .is_none_or(|key| key.is_empty())
        {
            return Err(NativeRouteError::Rejected(
                RejectionCode::ResumeRoutePinned,
                "resume requires an explicit source route key".into(),
            ));
        }
    }
    match &request.selection {
        NativeModelSelection::Exact { catalog_id } => {
            reject_secret_text(catalog_id, "catalog_id")?;
            if catalog_id.is_empty() {
                return Err(NativeRouteError::Rejected(
                    RejectionCode::ExactModelMissing,
                    "exact catalog_id must be non-empty".into(),
                ));
            }
        }
        NativeModelSelection::OrderedCandidates { catalog_ids } => {
            if catalog_ids.is_empty() {
                return Err(NativeRouteError::Rejected(
                    RejectionCode::EmptyCandidates,
                    "empty candidate lists are invalid, not inherit".into(),
                ));
            }
            for id in catalog_ids {
                reject_secret_text(id, "catalog_id")?;
                if id.is_empty() {
                    return Err(NativeRouteError::Rejected(
                        RejectionCode::EmptyCandidates,
                        "candidate catalog_id must be non-empty".into(),
                    ));
                }
            }
        }
        NativeModelSelection::Inherit => {}
    }
    Ok(())
}

fn resolve_resume(
    request: &NativeSubagentRouteRequest,
    catalog: &SyntheticCatalog,
    pin: &super::types::ResumePin,
    now_unix_ms: u64,
    attempt: u32,
) -> Result<NativeSubagentRouteResult, NativeRouteError> {
    reject_secret_text(&pin.source_catalog_id, "resume.source_catalog_id")?;
    let Some(entry) = catalog.get(&pin.source_catalog_id) else {
        return Err(NativeRouteError::Rejected(
            RejectionCode::ResumeRoutePinned,
            format!(
                "resume pin {} is missing from the catalog; refusing rebind",
                pin.source_catalog_id
            ),
        ));
    };
    let Some(expected_key) = pin
        .source_route_key
        .as_deref()
        .filter(|key| !key.is_empty())
    else {
        return Err(NativeRouteError::Rejected(
            RejectionCode::ResumeRoutePinned,
            "resume requires an explicit source route key".into(),
        ));
    };
    if expected_key != entry.route_key {
        return Err(NativeRouteError::Rejected(
            RejectionCode::ResumeRoutePinned,
            "resume cannot rebind the same wire slug onto another route".into(),
        ));
    }
    if let Err(reason) = eligibility(entry, &request.required_capabilities) {
        return Err(NativeRouteError::Rejected(
            reason.reason_code,
            format!("resume pin is no longer eligible: {}", reason.message),
        ));
    }
    finish(
        request,
        entry,
        Vec::new(),
        "resume",
        now_unix_ms,
        attempt,
        pin.source_receipt_digest.clone(),
    )
}

fn resolve_inherit(
    request: &NativeSubagentRouteRequest,
    catalog: &SyntheticCatalog,
    now_unix_ms: u64,
    attempt: u32,
) -> Result<NativeSubagentRouteResult, NativeRouteError> {
    let Some(parent_id) = request.parent_catalog_id.as_deref() else {
        return Err(NativeRouteError::Rejected(
            RejectionCode::ExactModelMissing,
            "inherit requires an explicit parent catalog id".into(),
        ));
    };
    let Some(entry) = catalog.get(parent_id) else {
        return Err(NativeRouteError::Rejected(
            RejectionCode::ExactModelMissing,
            format!("parent catalog id {parent_id} is missing"),
        ));
    };
    if let Err(reason) = eligibility(entry, &request.required_capabilities) {
        return Err(NativeRouteError::Rejected(
            reason.reason_code,
            format!("parent route is not eligible: {}", reason.message),
        ));
    }
    finish(
        request,
        entry,
        Vec::new(),
        "inherit",
        now_unix_ms,
        attempt,
        None,
    )
}

fn resolve_exact(
    request: &NativeSubagentRouteRequest,
    catalog: &SyntheticCatalog,
    catalog_id: &str,
    now_unix_ms: u64,
    attempt: u32,
) -> Result<NativeSubagentRouteResult, NativeRouteError> {
    let Some(entry) = catalog.get(catalog_id) else {
        return Err(NativeRouteError::Rejected(
            RejectionCode::ExactModelMissing,
            format!("exact catalog id {catalog_id} is missing; refusing parent fallback"),
        ));
    };
    if let Err(reason) = eligibility(entry, &request.required_capabilities) {
        return Err(NativeRouteError::Rejected(
            reason.reason_code,
            format!(
                "exact catalog id {catalog_id} is not eligible; refusing parent fallback: {}",
                reason.message
            ),
        ));
    }
    finish(
        request,
        entry,
        Vec::new(),
        "exact",
        now_unix_ms,
        attempt,
        None,
    )
}

fn resolve_ordered(
    request: &NativeSubagentRouteRequest,
    catalog: &SyntheticCatalog,
    catalog_ids: &[String],
    now_unix_ms: u64,
    attempt: u32,
) -> Result<NativeSubagentRouteResult, NativeRouteError> {
    let mut rejected = Vec::new();
    for catalog_id in catalog_ids {
        let Some(entry) = catalog.get(catalog_id) else {
            rejected.push(RejectedCandidate {
                catalog_id: catalog_id.clone(),
                wire_model: None,
                route_key: None,
                reason_code: RejectionCode::ExactModelMissing,
                message: format!("catalog id {catalog_id} is missing"),
            });
            continue;
        };
        match eligibility(entry, &request.required_capabilities) {
            Ok(()) => {
                return finish(
                    request,
                    entry,
                    rejected,
                    "ordered_candidates",
                    now_unix_ms,
                    attempt,
                    None,
                );
            }
            Err(reason) => rejected.push(reason),
        }
    }
    Err(NativeRouteError::Rejected(
        ordered_exhausted_code(&rejected),
        format!(
            "no eligible candidate in declared order; rejected={}",
            rejected.len()
        ),
    ))
}

fn ordered_exhausted_code(rejected: &[RejectedCandidate]) -> RejectionCode {
    let Some(first) = rejected.first() else {
        return RejectionCode::EmptyCandidates;
    };
    if rejected
        .iter()
        .all(|row| row.reason_code == first.reason_code)
    {
        first.reason_code
    } else {
        RejectionCode::RouteUnready
    }
}

fn eligibility(
    entry: &SyntheticCatalogEntry,
    req: &super::types::CapabilityRequirements,
) -> Result<(), RejectedCandidate> {
    let reject = |code: RejectionCode, message: String| RejectedCandidate {
        catalog_id: entry.catalog_id.clone(),
        wire_model: Some(entry.wire_model.clone()),
        route_key: Some(entry.route_key.clone()),
        reason_code: code,
        message,
    };
    if entry.unknown_readiness {
        return Err(reject(
            RejectionCode::UnknownReadiness,
            "unknown readiness never satisfies a hard requirement".into(),
        ));
    }
    if !entry.ready {
        return Err(reject(
            RejectionCode::RouteUnready,
            "catalog route is not ready".into(),
        ));
    }
    if req.local_only && !entry.local_only {
        return Err(reject(
            RejectionCode::LocalOnlyViolation,
            "local-only policy rejects a cloud route".into(),
        ));
    }
    if let Some(harness) = &req.required_harness {
        match &entry.harness {
            Some(actual) if actual == harness => {}
            Some(_) => {
                return Err(reject(
                    RejectionCode::HarnessIncompatible,
                    "required harness does not match the catalog route".into(),
                ));
            }
            None => {
                return Err(reject(
                    RejectionCode::HarnessIncompatible,
                    "catalog route does not declare a harness".into(),
                ));
            }
        }
    }
    if let Some(min_tokens) = req.minimum_context_tokens {
        match entry.context_tokens {
            Some(actual) if actual >= min_tokens => {}
            _ => {
                return Err(reject(
                    RejectionCode::CapabilityMismatch,
                    "minimumContextTokens is not satisfied".into(),
                ));
            }
        }
    }
    if req.structured_output && !entry.structured_output {
        return Err(reject(
            RejectionCode::CapabilityMismatch,
            "structuredOutput is required but missing".into(),
        ));
    }
    for name in &req.required_named_capabilities {
        if !entry.named_capabilities.iter().any(|item| item == name) {
            let code = RejectionCode::CapabilityUnknown;
            return Err(reject(
                code,
                format!("required capability {name} is unknown or absent"),
            ));
        }
    }
    Ok(())
}

fn finish(
    request: &NativeSubagentRouteRequest,
    entry: &SyntheticCatalogEntry,
    rejected: Vec<RejectedCandidate>,
    provenance: &str,
    now_unix_ms: u64,
    attempt: u32,
    resume_source_receipt: Option<String>,
) -> Result<NativeSubagentRouteResult, NativeRouteError> {
    let mut receipt = RouteReceipt {
        schema: RECEIPT_SCHEMA.into(),
        schema_version: SCHEMA_VERSION,
        parent_session_id: request.parent_session_id.clone(),
        child_session_id: request.child_session_id.clone(),
        selection_mode: request.selection.mode_name().into(),
        requested_catalog_ids: request.selection.requested_catalog_ids(),
        consumer_policy_id: request.consumer_policy_id.clone(),
        consumer_policy_digest: request.consumer_policy_digest.clone(),
        selected_catalog_id: entry.catalog_id.clone(),
        selected_wire_model: entry.wire_model.clone(),
        route_key: entry.route_key.clone(),
        access_profile: entry.access_profile.clone(),
        harness: entry.harness.clone(),
        capability_ceiling: request.capability_ceiling.clone(),
        required_capabilities: request.required_capabilities.clone(),
        selection_provenance: provenance.into(),
        rejected_candidates: rejected.clone(),
        route_digest: String::new(),
        attempt: attempt.max(1),
        created_unix_ms: now_unix_ms,
        resume_source_receipt,
    };
    receipt.route_digest = digest_receipt(&receipt)?;
    Ok(NativeSubagentRouteResult {
        selected_catalog_id: entry.catalog_id.clone(),
        selected_wire_model: entry.wire_model.clone(),
        route_key: entry.route_key.clone(),
        receipt,
        rejected_candidates: rejected,
    })
}

fn digest_receipt(receipt: &RouteReceipt) -> Result<String, NativeRouteError> {
    if receipt.schema != RECEIPT_SCHEMA {
        return Err(NativeRouteError::Rejected(
            RejectionCode::UnsupportedContract,
            "receipt schema must be medley.native-route-receipt.v1".into(),
        ));
    }
    let payload = receipt.canonical_payload();
    let blob = serde_json::to_vec(&payload).map_err(|_| NativeRouteError::SecretMaterial)?;
    let text = String::from_utf8_lossy(&blob);
    reject_secret_text(&text, "receipt")?;
    let mut hasher = Sha256::new();
    hasher.update(&blob);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Replay-safety admission. This slice always refuses cross-route fallback
/// after visible output, a tool call, or a side effect. #18 owns a proven
/// replay API later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackAdmission {
    pub admitted: bool,
    pub reason_code: RejectionCode,
    pub message: String,
}

pub fn admit_cross_route_fallback(facts: &[AttemptLifecycleFact]) -> FallbackAdmission {
    for fact in facts {
        match fact {
            AttemptLifecycleFact::VisibleOutputCommitted
            | AttemptLifecycleFact::ToolCallEmitted
            | AttemptLifecycleFact::ToolSideEffectStarted => {
                return FallbackAdmission {
                    admitted: false,
                    reason_code: RejectionCode::FallbackReplayUnsafe,
                    message: format!(
                        "cross-route fallback refused after {}",
                        match fact {
                            AttemptLifecycleFact::VisibleOutputCommitted => "visible output",
                            AttemptLifecycleFact::ToolCallEmitted => "a tool call",
                            AttemptLifecycleFact::ToolSideEffectStarted => "a side effect",
                            _ => "an unsafe observation",
                        }
                    ),
                };
            }
            _ => {}
        }
    }
    FallbackAdmission {
        admitted: false,
        reason_code: RejectionCode::FallbackReplayUnsafe,
        message: "replay-safe fallback is unsupported in this slice (Medley #18)".into(),
    }
}
