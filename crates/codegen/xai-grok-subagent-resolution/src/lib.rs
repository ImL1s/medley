//! Subagent configuration resolution crate.
//!
//! Extracts the pure-logic "resolution" phase of subagent spawning from
//! `xai-grok-shell` into a reusable library. Given a spawn request and a
//! resolution context (roles, personas, parent state), this crate resolves:
//!
//! - Effective runtime config (model, persona, capability mode, isolation)
//!   via precedence: explicit override > role > persona > parent.
//! - Persona instruction loading (inline `instructions` + `instructions_file`).
//! - Role prompt file loading.
//! - Resume identity validation (type/persona match checks; model is soft-ignored).
//!
//! This crate has no dependency on session, coordinator, or transport types.
//! Designed to be consumed by local hosts (e.g. `xai-grok-shell`) and any
//! future remote spawn path that only needs pure resolution logic.
//!
//! Definition discovery, gating, prompt context, runtime defaults, and
//! capability/depth tool policy are shared here. Model catalog selection and
//! workspace materialization remain host adapters.
//!
//! [`native_route`] is the optional plugin-facing native subagent route
//! contract (#287): capability negotiation, ordered catalog candidates, and
//! secret-free receipts. Live child spawn maps the session catalog through
//! this resolver when `AgentDefinition.models` is set or when exact `model:`
//! is declared; unknown exact ids fail closed instead of inheriting.

pub mod config;
pub mod context;
pub mod definition;
pub mod native_route;
pub mod overrides;
pub mod resume;
pub mod types;

pub use config::{PersonaIOField, SubagentPersona, SubagentRole};
pub use definition::{
    DefinitionResolutionContext, DefinitionValidationContext, HarnessToolsetContext,
    apply_child_tool_policy, apply_definition_runtime_defaults, apply_harness_toolset,
    available_agent_names, discover_agent_definition, gate_agent_definition,
    render_subagent_initial_user_message, render_subagent_system_prompt, resolve_agent_definition,
    resolve_runtime_config, select_role, subagent_harness_flavor_is_representable,
    validate_agent_name,
};
pub use native_route::{
    AgentRouteUxSnapshot, CapabilityRequirements, CapabilityState, DeclarativeNativeRouteSpec,
    FallbackAdmission, LifecyclePhase, NativeModelSelection, NativeRouteError,
    NativeSubagentRouteRequest, NativeSubagentRouteResult, RejectionCode, RouteReceipt,
    SyntheticCatalog, WorkerRoute, discover_capabilities, inspect_document, parse_declarative_spec,
    request_from_agent_definition, resolve_native_route, snapshot_from_receipt,
    usage_facts_from_receipt,
};
pub use overrides::{
    intersect_capability_mode_ceiling, intersect_capability_modes, resolve_effective_overrides,
};
pub use resume::{ResumeValidationError, validate_resume_identity};
pub use types::{ContextSource, EffectiveRuntimeConfig, ResolutionError, ResumeSourceData};
pub use xai_grok_agent::config::AgentDefinition;
