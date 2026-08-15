//! Optional plugin-facing native subagent route contract (Medley #287).
//!
//! This module is the generic, secret-free request/receipt seam. It does not
//! own orchestration-product agent names, prompt profiles, or external CLI
//! executors. Child-session spawn still uses existing exact/inherit
//! `ModelOverride` until a later PR wires this resolver into session
//! construction.
//!
//! Implemented in this slice: capability discovery, exact/inherit/ordered
//! offline resolution over a synthetic catalog, immutable receipts, inspect
//! JSON, declarative parse, and typed UX snapshots. Not implemented: live
//! spawn persistence, ACP/usage projections, generation-bound TUI mutation,
//! or replay-safe runtime fallback (#18).

mod inspect;
mod resolve;
mod types;
mod ux;

pub use inspect::{
    DeclarativeNativeRouteSpec, InspectDocument, inspect_document, parse_declarative_spec,
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
    snapshot_from_model_override, snapshot_from_resolution,
};

#[cfg(test)]
mod tests;
