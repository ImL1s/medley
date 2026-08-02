//! Applies a model switch to a session.
//!
//! `set_session_model` enforces the `allowed_models` gate before delegating
//! here. Credential / config readiness is enforced inside `apply` so internal
//! callers (`new_session`, `load_session`, prompt restore) cannot attach an
//! unready model (e.g. invalid `auth_scheme` fail-open → ambient Bearer).
use crate::agent::config;
use crate::agent::mvp_agent::{
    MvpAgent, agent_name_after_model_switch, apply_session_cli_clamps, harnesses_are_compatible,
    resolve_required_agent_type,
};
use crate::session::SessionCommand;
use agent_client_protocol::{self as acp};
use tokio::sync::oneshot;
use xai_grok_sampling_types::parse_reasoning_effort_meta;

pub(crate) struct FailureTelemetry {
    armed: bool,
    session_id: String,
    previous_model_id: String,
    new_model_id: String,
    error_code: &'static str,
    required_agent_type: Option<String>,
    current_agent_type: Option<String>,
}

impl FailureTelemetry {
    pub(crate) fn new(session_id: &acp::SessionId, model_id: &acp::ModelId) -> Self {
        Self {
            armed: true,
            session_id: session_id.0.to_string(),
            previous_model_id: String::new(),
            new_model_id: model_id.0.to_string(),
            error_code: config::MODEL_SWITCH_VALIDATION_FAILED,
            required_agent_type: None,
            current_agent_type: None,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }

    fn mark_rebuild_phase(&mut self) {
        self.error_code = config::MODEL_SWITCH_REBUILD_FAILED;
    }

    fn mark_incompatible(&mut self) {
        self.error_code = config::MODEL_SWITCH_INCOMPATIBLE_AGENT;
    }
}

impl Drop for FailureTelemetry {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        emit_failure_telemetry(xai_grok_telemetry::events::ModelSwitched {
            session_id: self.session_id.clone(),
            previous_model_id: self.previous_model_id.clone(),
            new_model_id: self.new_model_id.clone(),
            success: false,
            error_code: Some(self.error_code.to_owned()),
            required_agent_type: self.required_agent_type.clone(),
            current_agent_type: self.current_agent_type.clone(),
        });
    }
}

fn emit_failure_telemetry(event: xai_grok_telemetry::events::ModelSwitched) {
    #[cfg(test)]
    if let Ok(value) = serde_json::to_value(&event) {
        CAPTURED_FAILURE_TELEMETRY.lock().unwrap().push(value);
    }
    xai_grok_telemetry::session_ctx::log_event(event);
}

#[cfg(test)]
static CAPTURED_FAILURE_TELEMETRY: std::sync::LazyLock<std::sync::Mutex<Vec<serde_json::Value>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

#[cfg(test)]
pub(crate) fn take_captured_failure_telemetry(session_id: &str) -> Vec<serde_json::Value> {
    let mut captured = CAPTURED_FAILURE_TELEMETRY.lock().unwrap();
    let mut matching = Vec::new();
    captured.retain(|event| {
        if event.get("session_id").and_then(serde_json::Value::as_str) == Some(session_id) {
            matching.push(event.clone());
            false
        } else {
            true
        }
    });
    matching
}
/// Apply a model switch to a session.
///
/// Always fail-closed on `model_readiness`. The ACP `allowed_models` gate still
/// lives in `set_session_model` only.
pub(crate) async fn apply(
    agent: &MvpAgent,
    args: acp::SetSessionModelRequest,
) -> Result<acp::SetSessionModelResponse, acp::Error> {
    tracing::info!("Received set session model request {args:?}");
    tracing::debug!("session_session_model::mvp_agent: {:?}", &args);
    let effort_override = parse_reasoning_effort_meta(args.meta.as_ref());
    let acp::SetSessionModelRequest {
        session_id,
        model_id,
        ..
    } = args;
    // Armed until the complete actor receipt and outer mirrors have committed.
    // Every early return therefore emits exactly one sanitized failure event.
    let mut failure_telemetry = FailureTelemetry::new(&session_id, &model_id);
    let handle = agent
        .session_handle_waiting_for_load(&session_id)
        .await
        .ok_or_else(|| acp::Error::invalid_params().data("unknown session id"))?;
    // Resolve an in-flight load before taking the per-session dispatch lock.
    // `load_session` applies its restored model while its load guard is alive;
    // holding this lock while waiting for that guard would deadlock the restore
    // path until the bounded load wait expired.
    //
    // Once the session exists, serialize the complete prepare/actor-commit/
    // outer-handle sequence with prompt intake and other model switches.
    let dispatch_lock = agent.dispatch_lock(&session_id);
    let _dispatch_guard = dispatch_lock.lock().await;
    let model = agent.resolve_model_id(&model_id)?;
    let (ready, reason) = config::model_readiness(&model);
    if !ready {
        tracing::warn!(
            session_id = %session_id.0,
            model_id = %model_id.0,
            reason = ?reason,
            "model_switch::apply: rejecting unready model (fail-closed)"
        );
        return Err(acp::Error::invalid_params()
            .data(reason.unwrap_or_else(|| "model is not ready".to_owned())));
    }
    let use_concise = model.info().use_concise;
    let session_default = handle
        .session_default_agent_profile
        .as_deref()
        .unwrap_or(&handle.agent_name);
    let required_agent_type =
        resolve_required_agent_type(Some(model.info().agent_type.as_str()), session_default);
    let previous_model_id = handle.model_id.0.clone();
    failure_telemetry.previous_model_id = previous_model_id.to_string();
    failure_telemetry.required_agent_type = Some(required_agent_type.clone());
    let (agent_tx, agent_rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(SessionCommand::GetActiveAgent {
            responds_to: agent_tx,
        })
        .map_err(|_| acp::Error::internal_error().data("model_switch: session actor closed"))?;
    let active_agent_type = agent_rx
        .await
        .map_err(|_| acp::Error::internal_error().data("model_switch: session actor closed"))?;
    let observed_active_agent_type = active_agent_type
        .clone()
        .unwrap_or_else(|| handle.agent_name.clone());
    failure_telemetry.current_agent_type = Some(observed_active_agent_type.clone());
    // Resolve every differing target before compatibility classification.
    // In particular, an unknown custom name is not evidence of a stock
    // harness; it is an unavailable prerequisite and must fail closed.
    // Always resolve the required definition. Equal canonical names are not
    // sufficient for plugin/file-backed harnesses because their prompt/source
    // identity can differ while the display name remains unchanged.
    let raw_required_definition = xai_grok_agent::discovery::by_name_in_cwd_with_plugins(
        &required_agent_type,
        handle.tool_context.cwd.as_path(),
        agent.plugin_registry_handle.snapshot().as_deref(),
    );
    let observed_definition = xai_grok_agent::discovery::by_name_in_cwd_with_plugins(
        &observed_active_agent_type,
        handle.tool_context.cwd.as_path(),
        agent.plugin_registry_handle.snapshot().as_deref(),
    )
    .unwrap_or_else(|| {
        let mut definition = xai_grok_agent::AgentDefinition::default_grok_build();
        definition.name.clone_from(&observed_active_agent_type);
        definition
    });
    let is_mismatch = !harnesses_are_compatible(
        &observed_definition,
        &required_agent_type,
        raw_required_definition.as_ref(),
    );
    tracing::info!(
        session_id = %session_id.0,
        model_id = %model_id.0,
        ?required_agent_type,
        active_agent_type = %observed_active_agent_type,
        is_mismatch,
        definition_resolved = raw_required_definition.is_some(),
        "set_session_model: prepared actor-owned model switch"
    );
    // The active agent already carries the main session's authoritative CLI
    // clamps. Apply the same clamps to the target before the actor compares or
    // rebuilds it; otherwise --tools/--disallowed-tools/--permission-mode can
    // cause a false mismatch and a zero-turn rebuild can silently drop them.
    let required_definition = {
        let cfg = agent.cfg.borrow();
        apply_session_cli_clamps(raw_required_definition, &cfg.cli_agent_overrides)
    };
    let mut model_sampling =
        agent.prepare_sampling_config_for_model(&model, handle.origin_client.clone());
    if let Some(eff) = effort_override {
        if agent
            .models_manager
            .model_supports_reasoning_effort(model_id.0.as_ref())
        {
            tracing::info!(
                session_id = %session_id.0,
                effort = %eff,
                "set_session_model: applying reasoning_effort override from meta"
            );
            model_sampling.reasoning_effort = Some(eff);
        } else {
            tracing::warn!(
                session_id = %session_id.0,
                model_id = %model_id.0,
                effort = %eff,
                "set_session_model: ignoring reasoning_effort override — model does not support it"
            );
        }
    }
    let applied_effort = model_sampling.reasoning_effort;
    let new_threshold = {
        let cfg = agent.cfg.borrow();
        let models = agent.models_manager.models();
        let model = config::find_model_by_id(&models, model_sampling.model.as_str());
        crate::util::config::resolve_auto_compact_threshold_percent(
            &cfg,
            model_sampling.model.as_str(),
            model.map(|e| &e.info),
        )
    };
    let (tx, rx) = oneshot::channel();
    // Validation and preparation are complete. Failures from this point are
    // actor/rebuild failures unless the actor returns the typed mismatch gate.
    failure_telemetry.mark_rebuild_phase();
    handle
        .cmd_tx
        .send(SessionCommand::ApplyModelSwitch {
            prepared: Box::new(crate::session::PreparedModelSwitch {
                catalog_model_id: model_id.clone(),
                sampling_config: model_sampling,
                use_concise,
                auto_compact_threshold_percent: new_threshold,
                required_agent_type: required_agent_type.clone(),
                required_definition,
            }),
            responds_to: tx,
        })
        .map_err(|_| acp::Error::internal_error().data("model_switch: session actor closed"))?;
    let receipt = match rx.await {
        Ok(Ok(receipt)) => receipt,
        Ok(Err(err)) => {
            if config::ModelSwitchIncompatibleAgentError::from_acp_error(&err).is_some() {
                failure_telemetry.mark_incompatible();
            }
            return Err(err);
        }
        Err(_) => {
            return Err(acp::Error::internal_error().data("model_switch: session actor closed"));
        }
    };
    let did_rebuild = receipt.did_rebuild;
    let committed_previous_model_id = receipt.previous_model_id.0.to_string();
    let updated_model = receipt.catalog_model_id;
    if let Some(handle) = agent.sessions.borrow_mut().get_mut(&session_id) {
        handle.model_id = model_id.clone();
        handle.reasoning_effort = applied_effort;
        handle.agent_name =
            agent_name_after_model_switch(did_rebuild, &required_agent_type, &handle.agent_name);
    }
    broadcast_model_changed(
        agent,
        &session_id,
        model_id.0.as_ref(),
        applied_effort.map(|eff| eff.to_string()),
    );
    xai_grok_telemetry::unified_log::info(
        "model changed",
        Some(session_id.0.as_ref()),
        Some(serde_json::json!({"model": model_id.0.as_ref()})),
    );
    xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::ModelSwitched {
        session_id: session_id.0.to_string(),
        previous_model_id: committed_previous_model_id,
        new_model_id: model_id.0.to_string(),
        success: true,
        error_code: None,
        required_agent_type: Some(required_agent_type.clone()),
        current_agent_type: receipt.active_agent_type,
    });
    if agent.cfg.borrow().mode != config::AgentMode::Leader {
        agent.models_manager.set_current_model_id(model_id.clone());
        agent
            .models_manager
            .set_current_reasoning_effort(applied_effort);
    }
    agent.sync_process_static_api_key(Some(model_id.0.as_ref()));
    failure_telemetry.disarm();
    Ok(acp::SetSessionModelResponse::new().meta(
        serde_json::json!({
            "model": updated_model,
        })
        .as_object()
        .cloned(),
    ))
}
/// Broadcast a `ModelChanged` to every client subscribed to this session so
/// followers mirror the new model. The originating client ignores its own echo
/// (gated by `model_switch_pending`). Broadcast-only — no eventId, not persisted.
fn broadcast_model_changed(
    agent: &MvpAgent,
    session_id: &acp::SessionId,
    model_id: &str,
    reasoning_effort: Option<String>,
) {
    let notification = crate::extensions::notification::SessionNotification {
        session_id: session_id.clone(),
        update: crate::extensions::notification::SessionUpdate::ModelChanged {
            model_id: model_id.to_owned(),
            reasoning_effort,
        },
        meta: None,
    };
    if let Ok(params) = serde_json::value::to_raw_value(&notification) {
        agent
            .gateway
            .forward_fire_and_forget(acp::ExtNotification::new(
                "x.ai/session_notification",
                params.into(),
            ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_telemetry_tracks_validation_rebuild_and_mismatch_phases() {
        let session_id = acp::SessionId::new("session");
        let model_id = acp::ModelId::new("target-model");
        let mut telemetry = FailureTelemetry::new(&session_id, &model_id);

        assert_eq!(telemetry.error_code, config::MODEL_SWITCH_VALIDATION_FAILED);
        telemetry.mark_rebuild_phase();
        assert_eq!(telemetry.error_code, config::MODEL_SWITCH_REBUILD_FAILED);
        telemetry.mark_incompatible();
        assert_eq!(
            telemetry.error_code,
            config::MODEL_SWITCH_INCOMPATIBLE_AGENT
        );

        telemetry.disarm();
    }
}
