//! Optional plugin-facing native subagent route contract (Medley #287).
//!
//! This module is the generic, secret-free request/receipt seam. It does not
//! own orchestration-product agent names, prompt profiles, or external CLI
//! executors. Live spawn in `xai-grok-shell` maps the session catalog into
//! [`SyntheticCatalog`] and persists [`RouteReceipt`] on child metadata / ACP.
//!
//! Implemented in this slice: capability discovery, exact/inherit/ordered
//! resolution (offline synthetic + live catalog), immutable receipts, inspect
//! JSON, declarative parse, `AgentDefinition.models`, typed UX snapshots, and
//! spawn-time receipt persistence. Not implemented: generation-bound TUI
//! mutation / lifecycle cards / a11y matrix (#290), or replay-safe runtime
//! fallback (#18).

mod inspect;
mod resolve;
mod types;
mod ux;

pub use inspect::{
    DeclarativeNativeRouteSpec, InspectDocument, inspect_document, parse_declarative_spec,
    request_from_agent_definition, usage_facts_from_receipt,
};
pub use resolve::{
    FallbackAdmission, SyntheticCatalog, SyntheticCatalogEntry, admit_cross_route_fallback,
    resolve_native_route, resolve_worker_route,
};
pub use types::{
    AttemptLifecycleFact, CapabilityId, CapabilityRequirements, CapabilityState,
    NativeModelSelection, NativeRouteError, NativeSubagentRouteRequest, NativeSubagentRouteResult,
    RejectedCandidate, RejectionCode, ResumePin, RouteReceipt, WorkerRoute, discover_capabilities,
};
pub use ux::{
    AgentRouteUxSnapshot, AgentSelectionMode, RouteStatus, format_compact_row, format_route_detail,
    snapshot_from_agent_definition, snapshot_from_model_override, snapshot_from_resolution,
};

#[cfg(test)]
mod tests;
