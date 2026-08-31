//! Optional plugin-facing native subagent route contract (Medley #287).
//!
//! This module is the generic, secret-free request/receipt seam. It does not
//! own orchestration-product agent names, prompt profiles, or external CLI
//! executors. Live spawn in `xai-grok-shell` maps the session catalog into
//! [`SyntheticCatalog`] and persists [`RouteReceipt`] on child metadata / ACP.
//!
//! Implemented in this slice: capability discovery, exact/inherit/ordered
//! resolution (offline synthetic + live catalog), immutable receipts, inspect
//! JSON, declarative parse, `AgentDefinition.models`, typed UX snapshots,
//! spawn-time receipt persistence, generation-bound `/agents` mutation
//! admission, lifecycle card labels, and a fail-closed replay-safe fallback
//! *planner*. Not implemented: picker/#207, live sampler auto-failover, or
//! qualified model-family metadata.

mod inspect;
mod resolve;
mod types;
mod ux;

pub use inspect::{
    DeclarativeNativeRouteSpec, InspectDocument, inspect_document, parse_declarative_spec,
    request_from_agent_definition, usage_facts_from_receipt,
};
pub use resolve::{
    FallbackAdmission, FallbackPlanRequest, SyntheticCatalog, SyntheticCatalogEntry,
    admit_cross_route_fallback, admit_generation_bound_mutation, plan_replay_safe_fallback,
    resolve_native_route, resolve_worker_route,
};
pub use types::{
    AttemptLifecycleFact, CapabilityId, CapabilityRequirements, CapabilityState,
    FallbackFailureClass, NativeModelSelection, NativeRouteError, NativeSubagentRouteRequest,
    NativeSubagentRouteResult, RejectedCandidate, RejectionCode, ResumePin, RouteReceipt,
    WorkerRoute, discover_capabilities,
};
pub use ux::{
    AgentRouteUxSnapshot, AgentSelectionMode, LifecyclePhase, RouteStatus, format_compact_row,
    format_lifecycle_line, format_route_detail, lifecycle_phase_for_snapshot,
    snapshot_from_agent_definition, snapshot_from_model_override, snapshot_from_receipt,
    snapshot_from_resolution,
};

#[cfg(test)]
mod tests;
