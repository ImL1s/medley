//! Live catalog adapter for the native subagent route resolver.

use indexmap::IndexMap;
use xai_grok_agent::config::AgentDefinition;
use xai_grok_subagent_resolution::native_route::{
    NativeModelSelection, ResumePin, RouteReceipt, SyntheticCatalog, SyntheticCatalogEntry,
    request_from_agent_definition, resolve_native_route,
};

use super::{SubagentMeta, SubagentSpawnContext};
use crate::agent::config::{
    ModelEntry, auth_class_for_entry, catalog_entry_is_local, model_readiness,
};

/// Map the session catalog into the secret-free native-route catalog.
/// Duplicate wire slugs stay distinct via the catalog key as `route_key`.
pub(super) fn synthetic_catalog_from_available_models(
    models: &IndexMap<String, ModelEntry>,
) -> SyntheticCatalog {
    SyntheticCatalog {
        entries: models
            .iter()
            .map(|(catalog_id, entry)| {
                let ready = model_readiness(entry).0;
                SyntheticCatalogEntry {
                    catalog_id: catalog_id.clone(),
                    wire_model: entry.info.model.clone(),
                    route_key: catalog_id.clone(),
                    access_profile: auth_class_for_entry(entry).to_string(),
                    ready,
                    unknown_readiness: false,
                    local_only: catalog_entry_is_local(entry),
                    harness: (!entry.info.agent_type.is_empty())
                        .then(|| entry.info.agent_type.clone()),
                    context_tokens: u32::try_from(entry.info.context_window.get()).ok(),
                    structured_output: true,
                    named_capabilities: Vec::new(),
                }
            })
            .collect(),
    }
}

pub(super) fn stamp_receipt_for_selection(
    definition: &AgentDefinition,
    ctx: &SubagentSpawnContext,
    catalog: &SyntheticCatalog,
    selected_catalog_id: &str,
    child_session_id: Option<&str>,
    resume: Option<ResumePin>,
    now_unix_ms: u64,
) -> Option<RouteReceipt> {
    if catalog.get(selected_catalog_id).is_none() {
        return None;
    }
    let mut request = request_from_agent_definition(
        definition,
        Some(ctx.model_id.0.to_string()),
        Some(ctx.parent_session_id.clone()),
        child_session_id.map(str::to_string),
        resume,
    )
    .ok()?;
    if let Ok(result) = resolve_native_route(&request, catalog, now_unix_ms, 1)
        && result.selected_catalog_id == selected_catalog_id
    {
        return Some(result.receipt);
    }
    let had_resume = request.resume.is_some();
    request.selection = NativeModelSelection::Exact {
        catalog_id: selected_catalog_id.to_string(),
    };
    if had_resume {
        // Keep the pin. Do not fabricate a non-resume receipt for a resumed child.
        return resolve_native_route(&request, catalog, now_unix_ms, 1)
            .ok()
            .filter(|result| result.selected_catalog_id == selected_catalog_id)
            .map(|result| result.receipt);
    }
    request.resume = None;
    resolve_native_route(&request, catalog, now_unix_ms, 1)
        .ok()
        .filter(|result| result.selected_catalog_id == selected_catalog_id)
        .map(|result| result.receipt)
}

/// Resume lineage digest from the *source* child's persisted meta.
/// Never use a receipt stamped for the current spawn.
pub(super) fn resume_source_receipt_digest(source_meta: Option<&SubagentMeta>) -> Option<String> {
    source_meta.and_then(|meta| {
        meta.native_route_receipt
            .as_ref()
            .map(|receipt| receipt.route_digest.clone())
    })
}
