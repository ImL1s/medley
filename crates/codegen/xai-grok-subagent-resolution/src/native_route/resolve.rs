//! Deterministic offline resolver over a synthetic catalog.

use sha2::{Digest, Sha256};

use super::types::{
    AttemptLifecycleFact, CapabilityRequirements, FallbackFailureClass, NativeModelSelection,
    NativeRouteError, NativeSubagentRouteRequest, NativeSubagentRouteResult, RECEIPT_SCHEMA,
    RejectedCandidate, RejectionCode, RouteReceipt, SCHEMA_VERSION, WorkerRoute, reject_secret_opt,
    reject_secret_text,
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
    if receipt.schema_version != SCHEMA_VERSION {
        return Err(NativeRouteError::Rejected(
            RejectionCode::UnsupportedContract,
            "receipt schema_version must be 1".into(),
        ));
    }
    match receipt.selection_mode.as_str() {
        "inherit" | "exact" | "ordered_candidates" => {}
        _ => {
            return Err(NativeRouteError::Rejected(
                RejectionCode::UnsupportedContract,
                "receipt selection_mode must be inherit, exact, or ordered_candidates".into(),
            ));
        }
    }
    let payload = receipt.canonical_payload();
    let blob = serde_json::to_vec(&payload).map_err(|_| NativeRouteError::SecretMaterial)?;
    let text = String::from_utf8_lossy(&blob);
    reject_secret_text(&text, "receipt")?;
    let mut hasher = Sha256::new();
    hasher.update(&blob);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Fail closed before publishing a receipt on inspect JSON.
pub(crate) fn validate_published_receipt(receipt: &RouteReceipt) -> Result<(), NativeRouteError> {
    let expected = digest_receipt(receipt)?;
    if expected != receipt.route_digest {
        return Err(NativeRouteError::Rejected(
            RejectionCode::UnsupportedContract,
            "inspect receipt digest does not match canonical payload".into(),
        ));
    }
    Ok(())
}

/// Replay-safety admission. Cross-route fallback is fail-closed: only a
/// retryable pre-output failure on an ordered same-lane candidate may admit.
/// Exact/inherit never fall over. Live sampler auto-failover is not wired.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FallbackAdmission {
    pub admitted: bool,
    pub reason_code: RejectionCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_catalog_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_candidates: Vec<RejectedCandidate>,
}

impl FallbackAdmission {
    fn refuse(
        reason_code: RejectionCode,
        message: impl AsRef<str>,
        skipped: Vec<RejectedCandidate>,
    ) -> Self {
        Self {
            admitted: false,
            reason_code,
            message: message.as_ref().to_string(),
            next_catalog_id: None,
            skipped_candidates: skipped,
        }
    }

    fn admit(next_catalog_id: String, skipped: Vec<RejectedCandidate>) -> Self {
        Self {
            admitted: true,
            reason_code: RejectionCode::RouteUnready,
            message: format!("admitted same-lane fallback to {next_catalog_id}"),
            next_catalog_id: Some(next_catalog_id),
            skipped_candidates: skipped,
        }
    }
}

/// Inputs for the #18 planner. Candidates independently resolve identity;
/// this function never copies credentials.
pub struct FallbackPlanRequest<'a> {
    pub selection: &'a NativeModelSelection,
    pub current_catalog_id: &'a str,
    pub current_access_profile: &'a str,
    pub remaining_catalog_ids: &'a [String],
    pub catalog: &'a SyntheticCatalog,
    pub requirements: &'a CapabilityRequirements,
    pub facts: &'a [AttemptLifecycleFact],
    pub failure: FallbackFailureClass,
}

/// Fact-only gate used by older call sites. Without remaining candidates this
/// never admits (fail-closed).
pub fn admit_cross_route_fallback(facts: &[AttemptLifecycleFact]) -> FallbackAdmission {
    if let Some(fact) = facts
        .iter()
        .copied()
        .find(|f| f.blocks_cross_route_fallback())
    {
        return FallbackAdmission::refuse(
            RejectionCode::FallbackReplayUnsafe,
            format!(
                "cross-route fallback refused after {}",
                match fact {
                    AttemptLifecycleFact::VisibleOutputCommitted => "visible output",
                    AttemptLifecycleFact::ToolCallEmitted => "a tool call",
                    AttemptLifecycleFact::ToolSideEffectStarted => "a side effect",
                    _ => "an unsafe observation",
                }
            ),
            Vec::new(),
        );
    }
    FallbackAdmission::refuse(
        RejectionCode::RouteUnready,
        "replay-safe fallback requires an ordered remaining same-lane candidate",
        Vec::new(),
    )
}

/// Plan the next same-lane catalog id, or refuse. No network, no credential copy.
pub fn plan_replay_safe_fallback(request: &FallbackPlanRequest<'_>) -> FallbackAdmission {
    if let Some(fact) = request
        .facts
        .iter()
        .copied()
        .find(|f| f.blocks_cross_route_fallback())
    {
        return FallbackAdmission::refuse(
            RejectionCode::FallbackReplayUnsafe,
            format!(
                "cross-route fallback refused after {}",
                match fact {
                    AttemptLifecycleFact::VisibleOutputCommitted => "visible output",
                    AttemptLifecycleFact::ToolCallEmitted => "a tool call",
                    AttemptLifecycleFact::ToolSideEffectStarted => "a side effect",
                    _ => "an unsafe observation",
                }
            ),
            Vec::new(),
        );
    }
    match request.failure {
        FallbackFailureClass::PartialOutput | FallbackFailureClass::ToolSideEffect => {
            return FallbackAdmission::refuse(
                RejectionCode::FallbackReplayUnsafe,
                format!(
                    "cross-route fallback refused after {}",
                    request.failure.as_str()
                ),
                Vec::new(),
            );
        }
        FallbackFailureClass::AuthOrConfig => {
            return FallbackAdmission::refuse(
                RejectionCode::CredentialMissing,
                "auth/config errors require operator correction; no fallback",
                Vec::new(),
            );
        }
        FallbackFailureClass::SafetyPolicy => {
            return FallbackAdmission::refuse(
                RejectionCode::UnsupportedContract,
                "safety/policy rejection is not fallback-eligible",
                Vec::new(),
            );
        }
        FallbackFailureClass::IncompatibleCapability => {
            return FallbackAdmission::refuse(
                RejectionCode::CapabilityMismatch,
                "capability/harness mismatch is not fallback-eligible",
                Vec::new(),
            );
        }
        FallbackFailureClass::ConnectTimeout
        | FallbackFailureClass::RateLimited
        | FallbackFailureClass::RetryableServer
        | FallbackFailureClass::ProviderUnavailable => {}
    }
    match request.selection {
        NativeModelSelection::Exact { .. } => {
            return FallbackAdmission::refuse(
                RejectionCode::FallbackReplayUnsafe,
                "exact mode never cross-route fallbacks",
                Vec::new(),
            );
        }
        NativeModelSelection::Inherit => {
            return FallbackAdmission::refuse(
                RejectionCode::FallbackReplayUnsafe,
                "inherit has no ordered fallback chain",
                Vec::new(),
            );
        }
        NativeModelSelection::OrderedCandidates { .. } => {}
    }
    let mut skipped = Vec::new();
    for catalog_id in request.remaining_catalog_ids {
        if catalog_id == request.current_catalog_id {
            skipped.push(RejectedCandidate {
                catalog_id: catalog_id.clone(),
                wire_model: None,
                route_key: None,
                reason_code: RejectionCode::DuplicateRequest,
                message: "current route is not a fallback target".into(),
            });
            continue;
        }
        let Some(entry) = request.catalog.get(catalog_id) else {
            skipped.push(RejectedCandidate {
                catalog_id: catalog_id.clone(),
                wire_model: None,
                route_key: None,
                reason_code: RejectionCode::ExactModelMissing,
                message: format!("catalog id {catalog_id} is missing"),
            });
            continue;
        };
        if entry.access_profile != request.current_access_profile {
            skipped.push(RejectedCandidate {
                catalog_id: entry.catalog_id.clone(),
                wire_model: Some(entry.wire_model.clone()),
                route_key: Some(entry.route_key.clone()),
                reason_code: RejectionCode::CrossBillingBlocked,
                message: format!(
                    "access profile {} is not the same lane as {}",
                    entry.access_profile, request.current_access_profile
                ),
            });
            continue;
        }
        match eligibility(entry, request.requirements) {
            Ok(()) => return FallbackAdmission::admit(entry.catalog_id.clone(), skipped),
            Err(reason) => skipped.push(reason),
        }
    }
    FallbackAdmission::refuse(
        RejectionCode::RouteUnready,
        format!(
            "no eligible same-lane remaining candidate; skipped={}",
            skipped.len()
        ),
        skipped,
    )
}

/// Generation-bound mutation admission for `/agents` and other TUI actions.
pub fn admit_generation_bound_mutation(
    expected_generation: u64,
    current_generation: u64,
) -> Result<(), NativeRouteError> {
    if expected_generation != current_generation {
        return Err(NativeRouteError::Rejected(
            RejectionCode::StaleGeneration,
            format!(
                "stale generation: expected={expected_generation}, actual={current_generation}"
            ),
        ));
    }
    Ok(())
}
