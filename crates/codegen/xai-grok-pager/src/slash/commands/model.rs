//! `/model` (alias `/m`) — switch model + (optionally) reasoning effort.
//! Chained autocomplete: pick a reasoning-supported model → trailing space
//! re-opens the dropdown into a `low|medium|high|xhigh` sub-menu.

use agent_client_protocol as acp;
use xai_grok_shell::sampling::types::supports_reasoning_effort_meta;

use crate::acp::model_state::ModelNameResolution;
use crate::acp::model_state::ModelState;
use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};
use crate::slash::commands::effort_levels::build_effort_arg_items;

/// Switch the active model (and optionally its reasoning effort).
pub struct ModelCommand;

impl SlashCommand for ModelCommand {
    fn name(&self) -> &str {
        "model"
    }

    fn aliases(&self) -> &[&str] {
        &["m"]
    }

    fn description(&self) -> &str {
        "Switch the active model"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn offered_when_session_less(&self) -> bool {
        // The dashboard offers `/model` to pick the model for the next
        // spawned agent (intercepted in `dispatch_dashboard_dispatch_slash`).
        true
    }

    fn usage(&self) -> &str {
        "/model <name> [effort] [--session]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("<model> [effort] [--session]")
    }

    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        if ctx.models.is_empty() {
            return None;
        }

        // Effort phase if input is "<reasoning-model> ", else model phase.
        if let Some(model_id) = detect_effort_phase(ctx.models, args_query) {
            return Some(build_effort_items(ctx.models, &model_id));
        }
        Some(build_model_items(ctx.models))
    }

    fn initially_preferred_arg(&self, ctx: &AppCtx, item: &ArgItem) -> bool {
        ctx.models
            .current_model_id_str()
            .is_some_and(|current| item.insert_text.trim_end() == current)
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            return CommandResult::Error("Usage: /model <name> [effort] [--session]".into());
        }

        // Parse flags via whitespace tokenization (not string suffix matching).
        // Flags can appear anywhere in the args string.
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        let mut session_only = false;
        let mut model_args_parts: Vec<&str> = Vec::new();

        for token in tokens {
            if token.eq_ignore_ascii_case("--session") {
                session_only = true;
            } else {
                model_args_parts.push(token);
            }
        }

        let model_args = model_args_parts.join(" ");
        if model_args.is_empty() {
            return CommandResult::Error("Usage: /model <name> [effort] [--session]".into());
        }

        // Prefer an exact full-string catalog match first. Model display names
        // often contain spaces ("Grok 4.5"); if we split on the last token
        // first, a shorter catalog entry ("Grok") would steal the prefix and
        // treat "4.5" as an effort level.
        match ctx.models.resolve_unique_by_name_or_id(&model_args) {
            ModelNameResolution::Resolved(id) => {
                if let Some(reason) = model_not_ready_reason(ctx.models, &id) {
                    return CommandResult::Error(reason);
                }
                // Session-only: use SwitchModel with no effort (does not persist).
                // Default: use SetDefaultModel (switches + persists on success).
                return if session_only {
                    CommandResult::Action(Action::SwitchModel {
                        model_id: id,
                        effort: None,
                        session_only: true,
                    })
                } else {
                    CommandResult::Action(Action::SetDefaultModel(id))
                };
            }
            ModelNameResolution::Ambiguous => {
                return ambiguous_model_error(&model_args);
            }
            ModelNameResolution::Unknown => {}
        }

        // Trailing effort token + reasoning model → session-scoped switch
        // (not persisted as default). Resolve via the shared gate so a rejected
        // level (e.g. `none` on grok-4.5) surfaces the effort error with the
        // model's offered ids — not "Unknown model: … none".
        //
        // Reject effort + --session as conflicting. The user must choose one:
        // - `/model <name> <effort>` → session-only with effort
        // - `/model <name> --session` → session-only without effort
        // - `/model <name>` → persist (default behavior)
        if let Some((prefix, _token)) = split_trailing_token(&model_args)
            && matches!(
                ctx.models.resolve_unique_by_name_or_id(prefix),
                ModelNameResolution::Ambiguous
            )
        {
            return ambiguous_model_error(prefix);
        }
        if let Some((prefix, token)) = split_trailing_token(&model_args) {
            // Check if the last token might be an effort level.
            if let Some(id) = resolve_model(ctx.models, prefix)
                && ctx
                    .models
                    .available
                    .get(&id)
                    .map(supports_reasoning_effort)
                    .unwrap_or(false)
            {
                if session_only {
                    return CommandResult::Error(
                        "Cannot use --session with effort level. Use one of:\n  /model <name> <effort> (session-only with effort)\n  /model <name> --session (session-only)\n  /model <name> (persist as default)".into(),
                    );
                }
                if let Some(reason) = model_not_ready_reason(ctx.models, &id) {
                    return CommandResult::Error(reason);
                }
                return match ctx.models.resolve_effort_for_model(&id, token) {
                    Ok(effort) => CommandResult::Action(Action::SwitchModel {
                        model_id: id,
                        effort: Some(effort),
                        session_only: true,
                    }),
                    Err(err) => CommandResult::Error(err.message()),
                };
            }
        }

        CommandResult::Error(format!("Unknown model: {model_args}"))
    }
}

fn ambiguous_model_error(name: &str) -> CommandResult {
    CommandResult::Error(format!(
        "Ambiguous model name: {name}. Use the model id shown in /model"
    ))
}

/// Look up a model by case-insensitive display name OR model id match.
fn resolve_model(models: &ModelState, name: &str) -> Option<acp::ModelId> {
    match models.resolve_unique_by_name_or_id(name) {
        ModelNameResolution::Resolved(id) => Some(id),
        ModelNameResolution::Ambiguous | ModelNameResolution::Unknown => None,
    }
}

fn supports_reasoning_effort(info: &acp::ModelInfo) -> bool {
    supports_reasoning_effort_meta(info.meta.as_ref())
}

/// Parsed readiness fields from ACP `ModelInfo._meta`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelReadinessMeta {
    auth_scheme: String,
    auth_class: String,
    ready: bool,
    readiness_reason: String,
    provider_hint: String,
    catalog_degraded_reason: String,
}

fn parse_model_readiness(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> ModelReadinessMeta {
    let get_str = |key: &str| -> String {
        meta.and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let ready = match meta.and_then(|m| m.get("ready")) {
        None => true,
        Some(serde_json::Value::Bool(ready)) => *ready,
        Some(_) => false,
    };
    ModelReadinessMeta {
        auth_scheme: get_str("authScheme"),
        auth_class: get_str("authClass"),
        ready,
        readiness_reason: get_str("readinessReason"),
        provider_hint: get_str("providerHint"),
        catalog_degraded_reason: get_str("catalogDegradedReason"),
    }
}

/// User-facing reason when a model id is missing from the catalog or not ready.
pub(crate) const MODEL_CATALOG_MISS_REASON: &str = "Model no longer available";

pub(crate) fn model_not_ready_reason(models: &ModelState, id: &acp::ModelId) -> Option<String> {
    let Some(info) = models.available.get(id) else {
        return Some(MODEL_CATALOG_MISS_REASON.into());
    };
    let readiness = parse_model_readiness(info.meta.as_ref());
    if readiness.ready {
        return None;
    }
    Some(if readiness.readiness_reason.is_empty() {
        format!("{} is not ready", info.name)
    } else {
        readiness.readiness_reason
    })
}

/// Why a model can't be selected, from ACP model meta. `None` when it is
/// ready. Unlike [`model_not_ready_reason`] this needs no catalog lookup, so
/// listing commands can annotate rows straight from `ModelInfo._meta`.
pub(crate) fn unready_reason_from_model_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let readiness = parse_model_readiness(meta);
    if readiness.ready {
        return None;
    }
    Some(if readiness.readiness_reason.is_empty() {
        "not ready".to_owned()
    } else {
        readiness.readiness_reason
    })
}

/// Auth class string from ACP model meta (`none` | `env` | `session`).
pub(crate) fn auth_class_from_model_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> String {
    parse_model_readiness(meta).auth_class
}

/// Split `args` into `(prefix, last_token)` on the final whitespace run.
/// Returns `None` when there is no interior whitespace to split on. The token is
/// resolved to an effort against the picked model's options by the caller.
fn split_trailing_token(args: &str) -> Option<(&str, &str)> {
    let (prefix, last) = args.rsplit_once(char::is_whitespace)?;
    let prefix = prefix.trim_end();
    if prefix.is_empty() || last.is_empty() {
        return None;
    }
    Some((prefix, last))
}

/// Returns the matched model id when `args_query` is `"<reasoning-model> ..."`.
/// Longest-name-first to disambiguate names that share a prefix.
fn detect_effort_phase(models: &ModelState, args_query: &str) -> Option<acp::ModelId> {
    let reasoning_models: Vec<_> = models
        .selectable_models()
        .filter(|(_, info)| supports_reasoning_effort(info))
        .collect();
    let mut candidates: Vec<(&acp::ModelId, &str)> = Vec::new();
    for (id, info) in &reasoning_models {
        candidates.push((id, id.0.as_ref()));
        let name_is_unique = models
            .selectable_models()
            .filter(|(_, candidate)| candidate.name.eq_ignore_ascii_case(&info.name))
            .count()
            == 1;
        if name_is_unique && !info.name.eq_ignore_ascii_case(id.0.as_ref()) {
            candidates.push((id, info.name.as_str()));
        }
    }
    candidates.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));

    for (id, name) in candidates {
        if args_query.len() > name.len()
            && args_query.is_char_boundary(name.len())
            && args_query[..name.len()].eq_ignore_ascii_case(name)
            && args_query[name.len()..].starts_with(char::is_whitespace)
        {
            return Some(id.clone());
        }
    }
    None
}

/// One row per logical model. Reasoning models get a trailing space in
/// `insert_text` so the prompt widget chains into the effort sub-menu.
fn build_model_items(models: &ModelState) -> Vec<ArgItem> {
    let current_id = models.current.as_ref();
    let mut items: Vec<ArgItem> = Vec::with_capacity(models.available.len());
    for (id, info) in models.selectable_models() {
        let is_current = current_id == Some(id);
        let supports = supports_reasoning_effort(info);
        let readiness = parse_model_readiness(info.meta.as_ref());

        let duplicate_name = models
            .selectable_models()
            .filter(|(_, candidate)| candidate.name.eq_ignore_ascii_case(&info.name))
            .count()
            > 1;
        let label = if duplicate_name {
            format!("{} ({})", info.name, id.0)
        } else {
            info.name.clone()
        };
        let display = if is_current {
            format!("{label} (current)")
        } else {
            label
        };

        // Trailing space on reasoning models: signals "more input
        // expected" to the prompt widget so Enter advances to effort
        // phase instead of submitting. Unready models stay non-chaining
        // so selection is hard-blocked instead of advancing to effort.
        let insert_text = if supports && readiness.ready {
            format!("{} ", id.0)
        } else {
            id.0.to_string()
        };

        let hint = if readiness.provider_hint.is_empty() {
            "unknown".to_string()
        } else {
            readiness.provider_hint.clone()
        };
        let scheme = if readiness.auth_scheme.is_empty() {
            "bearer".to_string()
        } else {
            readiness.auth_scheme.clone()
        };
        let description = if readiness.catalog_degraded_reason.is_empty() {
            format!("{hint} · {scheme}")
        } else {
            format!("{hint} · {scheme} · {}", readiness.catalog_degraded_reason)
        };

        let badge = if !readiness.ready {
            "missing".to_string()
        } else if !readiness.catalog_degraded_reason.is_empty() {
            "degraded".to_string()
        } else if readiness.auth_scheme == "none" || readiness.auth_class == "none" {
            "none".to_string()
        } else {
            "ready".to_string()
        };

        items.push(ArgItem {
            display,
            match_text: format!("{} {}", info.name, id.0),
            insert_text,
            description,
            badge,
            dimmed: !readiness.ready,
            non_selectable: !readiness.ready,
            blocked_reason: if readiness.ready {
                String::new()
            } else if readiness.readiness_reason.is_empty() {
                format!("{} is not ready", info.name)
            } else {
                readiness.readiness_reason
            },
        });
    }
    items
}

/// One row per effort level for the `/model` chained effort phase.
/// `insert_text` is `"model-id high"` so selecting a row completes both tokens.
fn build_effort_items(models: &ModelState, model_id: &acp::ModelId) -> Vec<ArgItem> {
    if !models.available.contains_key(model_id) {
        return Vec::new();
    }
    let model_id_text = model_id.0.to_string();
    let is_current_model = models.current.as_ref() == Some(model_id);
    let options = models.reasoning_effort_options_for(model_id);
    build_effort_arg_items(
        &options,
        models.reasoning_effort,
        is_current_model,
        |option| format!("{model_id_text} {}", option.id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use xai_grok_shell::sampling::types::ReasoningEffort;

    fn model_with_reasoning(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let mut meta = serde_json::Map::new();
        meta.insert(
            "supportsReasoningEffort".into(),
            serde_json::Value::Bool(true),
        );
        let info = acp::ModelInfo::new(id.clone(), name.to_string())
            .meta(serde_json::Value::Object(meta).as_object().cloned());
        (id, info)
    }

    fn plain_model(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let info = acp::ModelInfo::new(id.clone(), name.to_string());
        (id, info)
    }

    static EMPTY_BUNDLE: crate::app::bundle::BundleState = crate::app::bundle::BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn dummy_exec_ctx(models: &ModelState) -> CommandExecCtx<'_> {
        CommandExecCtx {
            models,
            session_id: None,
            bundle_state: &EMPTY_BUNDLE,
            screen_mode: crate::app::ScreenMode::Inline,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: false,
                yolo_mode: false,
                ..crate::settings::PagerLocalSnapshot::default()
            },
        }
    }

    #[test]
    fn split_trailing_token_splits_on_final_whitespace() {
        assert_eq!(
            split_trailing_token("Reasoning X high"),
            Some(("Reasoning X", "high"))
        );
        assert_eq!(
            split_trailing_token("reasoning-x  xhigh"),
            Some(("reasoning-x", "xhigh"))
        );
        // No interior whitespace → nothing to split off.
        assert!(split_trailing_token("reasoning-x-pro").is_none());
    }

    #[test]
    fn empty_query_returns_one_row_per_logical_model() {
        let mut state = ModelState::default();
        let (rid, rinfo) = model_with_reasoning("reasoning-x", "Reasoning X");
        let (pid, pinfo) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(rid, rinfo);
        state.available.insert(pid, pinfo);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let items = cmd.suggest_args(&ctx, "").unwrap();
        assert_eq!(items.len(), 2, "model phase: one row per logical model");

        // Reasoning model has trailing space in insert_text -- this is the
        // signal the prompt widget reads to keep the dropdown open after
        // Enter so the effort sub-menu can render.
        let reasoning = items
            .iter()
            .find(|i| i.match_text == "Reasoning X reasoning-x")
            .unwrap();
        assert_eq!(reasoning.insert_text, "reasoning-x ");

        // Plain model has no trailing space -- Enter commits immediately.
        let plain = items
            .iter()
            .find(|i| i.match_text == "Grok 4.5 grok-4.5")
            .unwrap();
        assert_eq!(plain.insert_text, "grok-4.5");
    }

    #[test]
    fn unavailable_resident_is_neither_suggested_nor_runnable() {
        let mut state = ModelState::default();
        let (resident_id, resident_info) = model_with_meta(
            "retired",
            "Retired Model",
            serde_json::Map::from_iter([(
                "unavailableResidentModel".into(),
                serde_json::json!(true),
            )]),
        );
        let (ready_id, ready_info) = plain_model("ready", "Ready Model");
        state.available.insert(resident_id.clone(), resident_info);
        state.available.insert(ready_id, ready_info);
        state.current = Some(resident_id);

        let app_ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let items = ModelCommand.suggest_args(&app_ctx, "").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].match_text, "Ready Model ready");

        let mut exec_ctx = dummy_exec_ctx(&state);
        assert!(matches!(
            ModelCommand.run(&mut exec_ctx, "Retired Model"),
            CommandResult::Error(message) if message == "Unknown model: Retired Model"
        ));
        assert!(matches!(
            ModelCommand.run(&mut exec_ctx, "retired"),
            CommandResult::Error(message) if message == "Unknown model: retired"
        ));
    }

    #[test]
    fn trailing_space_after_reasoning_model_enters_effort_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // Args query has a trailing space -> effort phase. Items come out
        // ordered xhigh -> minimal (strongest first) per EFFORT_LEVELS.
        let items = cmd.suggest_args(&ctx, "reasoning-x ").unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].insert_text, "reasoning-x xhigh");
        assert_eq!(items[1].insert_text, "reasoning-x high");
        assert_eq!(items[2].insert_text, "reasoning-x medium");
        assert_eq!(items[3].insert_text, "reasoning-x low");
        assert_eq!(items[4].insert_text, "reasoning-x minimal");
        // Display is just the level so the user sees a clean column.
        assert_eq!(items[0].display, "xhigh");
        // match_text carries the sort-key prefix that forces the matcher's
        // alphabetical tiebreak to render rows in EFFORT_LEVELS order.
        assert!(items[0].match_text.starts_with("a "));
        assert!(items[3].match_text.starts_with("d "));
        assert!(items[4].match_text.starts_with("e "));
    }

    #[test]
    fn unique_display_name_enters_effort_phase_but_duplicate_name_does_not() {
        let mut unique = ModelState::default();
        let (unique_id, unique_info) = model_with_reasoning("reasoning-x", "Reasoning X");
        unique.available.insert(unique_id, unique_info);
        let unique_ctx = AppCtx {
            models: &unique,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let items = ModelCommand
            .suggest_args(&unique_ctx, "Reasoning X ")
            .expect("unique label chains to effort choices");
        assert_eq!(items[0].insert_text, "reasoning-x xhigh");

        let mut duplicate = ModelState::default();
        for id in ["provider-a/shared", "provider-b/shared"] {
            let (id, info) = model_with_reasoning(id, "Shared Model");
            duplicate.available.insert(id, info);
        }
        let duplicate_ctx = AppCtx {
            models: &duplicate,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let items = ModelCommand
            .suggest_args(&duplicate_ctx, "Shared Model ")
            .expect("ambiguous label stays in the model phase");
        assert!(items.iter().all(|item| item.display.contains("provider-")));
        let id_items = ModelCommand
            .suggest_args(&duplicate_ctx, "provider-a/shared ")
            .expect("canonical id chains to effort choices");
        assert_eq!(id_items[0].insert_text, "provider-a/shared xhigh");
    }

    #[test]
    fn partial_effort_query_still_in_effort_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // Still in effort phase; matcher upstream narrows to high / xhigh.
        let items = cmd.suggest_args(&ctx, "reasoning-x h").unwrap();
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn partial_model_query_stays_in_model_phase() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);

        let cmd = ModelCommand;
        let ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        // No trailing space, user is still typing the model name.
        let items = cmd.suggest_args(&ctx, "Reason").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert_text, "reasoning-x ");
    }

    #[test]
    fn run_parses_model_plus_effort_when_supported() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X xhigh");
        match result {
            CommandResult::Action(Action::SwitchModel { model_id, effort, session_only }) => {
                assert_eq!(model_id.0.as_ref(), "reasoning-x");
                assert_eq!(effort, Some(ReasoningEffort::Xhigh));
                assert!(session_only);
            }
            other => panic!("expected SwitchModel with effort, got {other:?}"),
        }
    }

    /// #357: the chained `/model` picker is a distinct UI path from
    /// `/effort`; lock both the Ultra row and the typed action it dispatches.
    #[test]
    fn chained_model_picker_renders_and_dispatches_ultra() {
        let mut state = ModelState::default();
        let (id, info) = model_with_meta(
            "gpt-5.6-sol",
            "GPT-5.6 Sol",
            serde_json::json!({
                "supportsReasoningEffort": true,
                "reasoningEffort": "low",
                "reasoningEfforts": [
                    { "id": "low", "value": "low", "label": "Low", "default": true },
                    { "id": "medium", "value": "medium", "label": "Medium" },
                    { "id": "high", "value": "high", "label": "High" },
                    { "id": "xhigh", "value": "xhigh", "label": "Xhigh" },
                    { "id": "max", "value": "max", "label": "Max" },
                    {
                        "id": "ultra",
                        "value": "ultra",
                        "label": "Ultra",
                        "description": "Maximum reasoning with proactive multi-agent guidance"
                    }
                ]
            })
            .as_object()
            .expect("Codex ACP metadata")
            .clone(),
        );
        state.available.insert(id.clone(), info);

        let app_ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let model_rows = ModelCommand
            .suggest_args(&app_ctx, "")
            .expect("model picker rows");
        assert_eq!(model_rows.len(), 1);
        assert_eq!(model_rows[0].insert_text, "gpt-5.6-sol ");

        let effort_rows = ModelCommand
            .suggest_args(&app_ctx, "gpt-5.6-sol ")
            .expect("chained effort rows");
        let ultra = effort_rows
            .iter()
            .find(|row| row.insert_text == "gpt-5.6-sol ultra")
            .expect("Ultra row");
        assert_eq!(ultra.display, "Ultra");
        assert_eq!(
            ultra.description,
            "Maximum reasoning with proactive multi-agent guidance"
        );

        let mut exec_ctx = dummy_exec_ctx(&state);
        match ModelCommand.run(&mut exec_ctx, "GPT-5.6 Sol ultra") {
            CommandResult::Action(Action::SwitchModel { model_id, effort, session_only }) => {
                assert_eq!(model_id, id);
                assert_eq!(effort, Some(ReasoningEffort::Ultra));
                assert!(session_only);
            }
            other => panic!("expected Ultra SwitchModel action, got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_unoffered_effort_with_effort_error_not_unknown_model() {
        // Regression: previously `resolve_effort_token_for` returned None and
        // the handler fell through to `Unknown model: Reasoning X none`.
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X none");
        match result {
            CommandResult::Error(msg) => {
                assert!(
                    msg.contains("unknown effort level 'none'"),
                    "expected effort error, got {msg}"
                );
                assert!(
                    msg.contains("use one of:"),
                    "expected offered levels in message, got {msg}"
                );
                assert!(
                    !msg.to_lowercase().contains("unknown model"),
                    "must not misreport as unknown model: {msg}"
                );
                let offered = msg.split_once("; ").map(|(_, r)| r).unwrap_or("");
                assert!(
                    !offered.contains("none"),
                    "must not list none as offered: {msg}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn model_not_ready_reason_catalog_miss_is_fail_closed() {
        let state = ModelState::default();
        let missing = acp::ModelId::new(Arc::from("removed-model"));
        assert_eq!(
            model_not_ready_reason(&state, &missing).as_deref(),
            Some(MODEL_CATALOG_MISS_REASON),
        );
    }

    #[test]
    fn run_prefers_full_multi_word_model_name_over_prefix_plus_effort() {
        // Catalog has both "Grok" (reasoning) and "Grok 4.5". `/model Grok 4.5`
        // must select the full name, not treat "4.5" as an effort on "Grok".
        let mut state = ModelState::default();
        let (short_id, short_info) = model_with_reasoning("grok", "Grok");
        let (long_id, long_info) = model_with_reasoning("grok-4.5", "Grok 4.5");
        state.available.insert(short_id, short_info);
        state.available.insert(long_id.clone(), long_info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, long_id);
            }
            other => panic!("expected SetDefaultModel(Grok 4.5), got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_effort_for_non_reasoning_model() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5 high");
        // Falls through to "is the whole string a model name?" — which
        // it isn't, so we get an Unknown error.
        assert!(matches!(result, CommandResult::Error(_)));
    }

    /// The bare `/model <name>` form dispatches
    /// `Action::SetDefaultModel(<ModelId>)` instead of the legacy
    /// `Action::SwitchModel { effort: None }`. The dispatcher routes
    /// the typed setter through both `Effect::SwitchModel`
    /// (session-level mutation) AND `Effect::PersistSetting`
    /// (next-session default).
    ///
    /// The payload is the typed `acp::ModelId` (resolved at the slash
    /// boundary), not a String.
    #[test]
    fn run_bare_model_name_dispatches_set_default_model() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }

    /// `/model <name> --session` dispatches `Action::SwitchModel` with
    /// `effort: None`, which is session-only (does not persist to config).
    /// This distinguishes explicit session-only selection from the default
    /// persist-on-success behavior of `SetDefaultModel`.
    #[test]
    fn run_model_with_session_flag_dispatches_session_only_switch() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5 --session");
        match result {
            CommandResult::Action(Action::SwitchModel {
                model_id,
                effort: None,
                session_only,
            }) => {
                assert_eq!(model_id, id);
                assert!(session_only, "--session flag must set session_only to true");
            }
            other => panic!(
                "expected Action::SwitchModel with effort: None for --session, got {other:?}"
            ),
        }
    }

    /// `/model <name> --session` with an unready model must still hard-block.
    #[test]
    fn run_model_session_flag_rejects_unready() {
        let mut state = ModelState::default();
        let (id, info) = model_with_meta(
            "unready",
            "Unready",
            serde_json::Map::from_iter([
                ("ready".into(), serde_json::json!(false)),
                (
                    "readinessReason".into(),
                    serde_json::json!("missing credential"),
                ),
            ]),
        );
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Unready --session");
        match result {
            CommandResult::Error(msg) => {
                assert_eq!(msg, "missing credential");
            }
            other => panic!("expected Error for unready model with --session, got {other:?}"),
        }
    }

    /// `/model <name> --session` with ambiguous name must surface the ambiguity error.
    #[test]
    fn run_model_session_flag_rejects_ambiguous() {
        let mut state = ModelState::default();
        let (id1, info1) = plain_model("provider-a/shared", "Shared Model");
        let (id2, info2) = plain_model("provider-b/shared", "Shared Model");
        state.available.insert(id1, info1);
        state.available.insert(id2, info2);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Shared Model --session");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("Ambiguous model name"));
            }
            other => panic!("expected Error for ambiguous model with --session, got {other:?}"),
        }
    }

    /// `/model --session <name>` (flag before model) must work (position-independent).
    #[test]
    fn run_model_session_flag_before_name_works() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "--session Grok 4.5");
        match result {
            CommandResult::Action(Action::SwitchModel {
                model_id,
                effort: None,
                session_only,
            }) => {
                assert_eq!(model_id, id);
            }
            other => panic!(
                "expected Action::SwitchModel with --session before name, got {other:?}"
            ),
        }
    }

    /// `/model <name> --SESSION` (uppercase) must work (case-insensitive).
    #[test]
    fn run_model_session_flag_case_insensitive() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5 --SESSION");
        match result {
            CommandResult::Action(Action::SwitchModel {
                model_id,
                effort: None,
                session_only,
            }) => {
                assert_eq!(model_id, id);
            }
            other => panic!(
                "expected Action::SwitchModel with uppercase --SESSION, got {other:?}"
            ),
        }
    }

    /// `/model <name> --session --session` (duplicate flag) must work (idempotent).
    #[test]
    fn run_model_duplicate_session_flag_idempotent() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Grok 4.5 --session --session");
        match result {
            CommandResult::Action(Action::SwitchModel {
                model_id,
                effort: None,
                session_only,
            }) => {
                assert_eq!(model_id, id);
            }
            other => panic!("expected idempotent --session --session, got {other:?}"),
        }
    }

    /// `/model <name> <effort> --session` must reject with clear error.
    /// Effort and --session are conflicting options:
    /// - Effort alone is already session-only
    /// - --session alone is session-only without effort
    #[test]
    fn run_model_effort_with_session_flag_rejects() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Reasoning X high --session");
        match result {
            CommandResult::Error(msg) => {
                assert!(
                    msg.contains("Cannot use --session with effort level"),
                    "expected conflict error, got: {msg}"
                );
            }
            other => panic!("expected Error for effort + --session conflict, got {other:?}"),
        }
    }

    /// `/model --session <name> <effort>` (flag first) must also reject.
    #[test]
    fn run_model_session_flag_before_effort_rejects() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "--session Reasoning X high");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("Cannot use --session with effort level"));
            }
            other => panic!("expected Error for --session + effort conflict, got {other:?}"),
        }
    }

    /// Case-insensitive matching against the catalog: `/model grok 4.5`
    /// resolves to the same `ModelId` as `/model Grok 4.5`.
    #[test]
    fn run_set_default_model_resolves_case_insensitively() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "grok 4.5");
        match result {
            CommandResult::Action(Action::SetDefaultModel(resolved_id)) => {
                assert_eq!(resolved_id, id);
            }
            other => panic!("expected Action::SetDefaultModel(<id>), got {other:?}"),
        }
    }

    fn model_with_meta(
        id: &str,
        name: &str,
        meta: serde_json::Map<String, serde_json::Value>,
    ) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let info = acp::ModelInfo::new(id.clone(), name.to_string()).meta(Some(meta));
        (id, info)
    }

    #[test]
    fn build_model_items_badges_ready_missing_none() {
        let mut state = ModelState::default();
        let (ready_id, ready_info) = model_with_meta(
            "ready-m",
            "Ready Model",
            serde_json::Map::from_iter([
                ("authScheme".into(), serde_json::json!("bearer")),
                ("authClass".into(), serde_json::json!("session")),
                ("ready".into(), serde_json::json!(true)),
                ("providerHint".into(), serde_json::json!("xAI")),
            ]),
        );
        let (missing_id, missing_info) = model_with_meta(
            "missing-m",
            "Missing Model",
            serde_json::Map::from_iter([
                ("authScheme".into(), serde_json::json!("bearer")),
                ("authClass".into(), serde_json::json!("env")),
                ("ready".into(), serde_json::json!(false)),
                (
                    "readinessReason".into(),
                    serde_json::json!("missing OPENAI_API_KEY"),
                ),
                ("providerHint".into(), serde_json::json!("api.openai.com")),
            ]),
        );
        let (none_id, none_info) = model_with_meta(
            "none-m",
            "None Model",
            serde_json::Map::from_iter([
                ("authScheme".into(), serde_json::json!("none")),
                ("authClass".into(), serde_json::json!("none")),
                ("ready".into(), serde_json::json!(true)),
                ("providerHint".into(), serde_json::json!("local")),
            ]),
        );
        state.available.insert(ready_id, ready_info);
        state.available.insert(missing_id, missing_info);
        state.available.insert(none_id, none_info);

        let items = build_model_items(&state);
        let ready = items
            .iter()
            .find(|i| i.match_text == "Ready Model ready-m")
            .unwrap();
        assert_eq!(ready.badge, "ready");
        assert!(!ready.dimmed);
        assert!(!ready.non_selectable);
        assert_eq!(ready.description, "xAI · bearer");

        let missing = items
            .iter()
            .find(|i| i.match_text == "Missing Model missing-m")
            .unwrap();
        assert_eq!(missing.badge, "missing");
        assert!(missing.dimmed);
        assert!(missing.non_selectable);
        assert_eq!(missing.blocked_reason, "missing OPENAI_API_KEY");
        assert_eq!(missing.description, "api.openai.com · bearer");

        let none = items
            .iter()
            .find(|i| i.match_text == "None Model none-m")
            .unwrap();
        assert_eq!(none.badge, "none");
        assert!(!none.non_selectable);
        assert_eq!(none.description, "local · none");
    }

    #[test]
    fn codex_catalog_degraded_state_is_visible_in_model_picker() {
        let mut state = ModelState::default();
        let reason = "live refresh failed; using the last saved catalog for this account";
        let (id, info) = model_with_meta(
            "codex-saved",
            "Codex Saved",
            serde_json::Map::from_iter([
                ("authScheme".into(), serde_json::json!("bearer")),
                ("authClass".into(), serde_json::json!("session")),
                ("ready".into(), serde_json::json!(true)),
                ("providerHint".into(), serde_json::json!("chatgpt.com")),
                ("catalogDegradedReason".into(), serde_json::json!(reason)),
            ]),
        );
        state.available.insert(id, info);

        let item = build_model_items(&state).pop().expect("Codex model row");
        assert_eq!(item.badge, "degraded");
        assert!(!item.dimmed);
        assert!(!item.non_selectable);
        assert!(item.description.contains(reason));
    }

    /// #306 C-min gate 1 (**component presentation** — not cold-boot/ACP
    /// integration; live TUI release capture still required by #306):
    /// Codex-only boot seats ready Codex as the authoritative default; the
    /// prompt footer must paint that name and never fall through to empty /
    /// "unknown" or keep ambient Grok.
    #[test]
    fn codex_only_boot_footer_uses_authoritative_default_never_unknown() {
        use crate::views::prompt_widget::{PromptInfo, PromptStyle, PromptWidget};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let mut state = ModelState::default();
        // Catalog order mirrors historical defaults-first: Grok ambient-ready,
        // then live ready Codex. After #303 the shell seats Codex as current.
        let (grok_id, grok_info) = model_with_meta(
            "grok-4.5",
            "Grok 4.5",
            serde_json::Map::from_iter([
                ("authScheme".into(), serde_json::json!("bearer")),
                ("authClass".into(), serde_json::json!("env")),
                ("ready".into(), serde_json::json!(true)),
                ("providerHint".into(), serde_json::json!("xAI")),
            ]),
        );
        let (codex_id, codex_info) = model_with_meta(
            "gpt-5.6-sol",
            "GPT-5.6 Sol",
            serde_json::Map::from_iter([
                ("authScheme".into(), serde_json::json!("bearer")),
                ("authClass".into(), serde_json::json!("session")),
                ("ready".into(), serde_json::json!(true)),
                ("providerHint".into(), serde_json::json!("chatgpt.com")),
            ]),
        );
        state.available.insert(grok_id.clone(), grok_info);
        state.available.insert(codex_id.clone(), codex_info);
        state.set_current(codex_id.clone(), None);

        let footer_label = state
            .current_model_name()
            .expect("boot footer requires an authoritative current model");
        assert_eq!(footer_label, "GPT-5.6 Sol");
        assert!(
            !footer_label.is_empty() && !footer_label.eq_ignore_ascii_case("unknown"),
            "footer label must never be empty/unknown, got {footer_label:?}"
        );
        assert_ne!(
            footer_label, "Grok 4.5",
            "Codex-only boot must not keep ambient-ready Grok as the footer default"
        );
        assert_eq!(state.current_model_id_str(), Some("gpt-5.6-sol"));
        assert_ne!(state.current.as_ref(), Some(&grok_id));
        assert!(
            model_not_ready_reason(&state, &codex_id).is_none(),
            "seated Codex must be ready for boot"
        );

        // Welcome / agent prompt chrome: same label path as AppView
        // (`current_model_name` → PromptInfo.model_name).
        let mut pw = PromptWidget::new();
        let area = Rect::new(0, 0, 80, 4);
        let mut buf = Buffer::empty(area);
        let info = PromptInfo {
            model_name: &footer_label,
            flags: &[],
            multiline: false,
            usage_warning: None,
            usage_warning_critical: false,
        };
        pw.draw(
            &mut buf,
            area,
            None,
            &PromptStyle::default(),
            Some(&info),
            None,
        );

        let mut painted = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    painted.push_str(cell.symbol());
                }
            }
            painted.push('\n');
        }
        assert!(
            painted.contains("GPT-5.6 Sol"),
            "boot footer buffer must paint ready Codex name, got:\n{painted}"
        );
        assert!(
            !painted.to_ascii_lowercase().contains("unknown"),
            "boot footer must never paint 'unknown':\n{painted}"
        );
        assert!(
            !painted.contains("Grok 4.5"),
            "Codex-only boot footer must not paint Grok:\n{painted}"
        );
    }

    /// #306 C-min gate 2 (**component presentation** — staged ModelState, not
    /// shell/ACP catalog sync; #306 live TUI still open): model picker lists
    /// live ready Codex and presents Grok as unready / missing-cred (dimmed,
    /// non-selectable, blocked).
    #[test]
    fn codex_model_picker_lists_live_codex_ready_and_grok_unready() {
        let mut state = ModelState::default();
        let (grok_id, grok_info) = model_with_meta(
            "grok-4.5",
            "Grok 4.5",
            serde_json::Map::from_iter([
                ("authScheme".into(), serde_json::json!("bearer")),
                ("authClass".into(), serde_json::json!("env")),
                ("ready".into(), serde_json::json!(false)),
                (
                    "readinessReason".into(),
                    serde_json::json!("missing XAI_API_KEY"),
                ),
                ("providerHint".into(), serde_json::json!("xAI")),
            ]),
        );
        let (codex_id, codex_info) = model_with_meta(
            "gpt-5.6-sol",
            "GPT-5.6 Sol",
            serde_json::Map::from_iter([
                ("authScheme".into(), serde_json::json!("bearer")),
                ("authClass".into(), serde_json::json!("session")),
                ("ready".into(), serde_json::json!(true)),
                ("providerHint".into(), serde_json::json!("chatgpt.com")),
            ]),
        );
        state.available.insert(grok_id, grok_info);
        state.available.insert(codex_id.clone(), codex_info);
        state.set_current(codex_id, None);

        let items = build_model_items(&state);
        assert_eq!(items.len(), 2, "picker must list both catalog entries");

        let codex = items
            .iter()
            .find(|i| i.match_text == "GPT-5.6 Sol gpt-5.6-sol")
            .expect("live Codex row in picker");
        assert_eq!(codex.badge, "ready");
        assert!(!codex.dimmed);
        assert!(!codex.non_selectable);
        assert!(codex.blocked_reason.is_empty());
        assert_eq!(codex.description, "chatgpt.com · bearer");
        assert!(
            codex.display.contains("(current)"),
            "seated Codex should be marked current, got {}",
            codex.display
        );

        let grok = items
            .iter()
            .find(|i| i.match_text == "Grok 4.5 grok-4.5")
            .expect("Grok row in picker");
        assert_eq!(grok.badge, "missing");
        assert!(grok.dimmed);
        assert!(grok.non_selectable);
        assert_eq!(grok.blocked_reason, "missing XAI_API_KEY");
        assert_eq!(grok.description, "xAI · bearer");

        // Selecting unready Grok must hard-block with the readiness reason.
        let mut ctx = dummy_exec_ctx(&state);
        match ModelCommand.run(&mut ctx, "Grok 4.5") {
            CommandResult::Error(msg) => assert_eq!(msg, "missing XAI_API_KEY"),
            other => panic!("expected Error for unready Grok, got {other:?}"),
        }
    }

    /// #306 gate 3 (**component presentation** — synthetic two-model fixture,
    /// not authoritative account-catalog cardinality; #306 live TUI still
    /// open): every ready live Codex in the catalog is selectable and seats
    /// via `SetDefaultModel`; final footer paints the last selected Codex name
    /// (never empty/unknown/sibling Grok).
    #[test]
    fn codex_model_picker_selects_every_live_catalog_model() {
        use crate::views::prompt_widget::{PromptInfo, PromptStyle, PromptWidget};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let mut state = ModelState::default();
        let (grok_id, grok_info) = model_with_meta(
            "grok-4.5",
            "Grok 4.5",
            serde_json::Map::from_iter([
                ("authScheme".into(), serde_json::json!("bearer")),
                ("authClass".into(), serde_json::json!("env")),
                ("ready".into(), serde_json::json!(false)),
                (
                    "readinessReason".into(),
                    serde_json::json!("missing XAI_API_KEY"),
                ),
                ("providerHint".into(), serde_json::json!("xAI")),
            ]),
        );
        let ready_models = [("gpt-5.6-sol", "GPT-5.6 Sol"), ("gpt-5.4", "GPT-5.4")];
        let mut ready_ids: Vec<(acp::ModelId, String)> = Vec::new();
        for (id, name) in ready_models {
            let (mid, info) = model_with_meta(
                id,
                name,
                serde_json::Map::from_iter([
                    ("authScheme".into(), serde_json::json!("bearer")),
                    ("authClass".into(), serde_json::json!("session")),
                    ("ready".into(), serde_json::json!(true)),
                    ("providerHint".into(), serde_json::json!("chatgpt.com")),
                ]),
            );
            ready_ids.push((mid.clone(), name.to_string()));
            state.available.insert(mid, info);
        }
        state.available.insert(grok_id, grok_info);

        let items = build_model_items(&state);
        for (id, name) in &ready_ids {
            let row = items
                .iter()
                .find(|i| i.match_text == format!("{name} {}", id.0))
                .expect("ready Codex row in picker");
            assert_eq!(row.badge, "ready");
            assert!(!row.non_selectable);
        }

        let mut last_name = String::new();
        for (id, name) in &ready_ids {
            let mut ctx = dummy_exec_ctx(&state);
            match ModelCommand.run(&mut ctx, name) {
                CommandResult::Action(Action::SetDefaultModel(resolved)) => {
                    assert_eq!(&resolved, id);
                }
                other => panic!("expected SetDefaultModel for {name}, got {other:?}"),
            }
            state.set_current(id.clone(), None);
            assert_eq!(state.current_model_id_str(), Some(id.0.as_ref()));
            assert_eq!(state.current_model_name().as_deref(), Some(name.as_str()));
            assert_eq!(state.current.as_ref(), Some(id));
            last_name = name.clone();
        }

        // Paint footer like gate 1 — label is last Codex name.
        let footer_label = state
            .current_model_name()
            .expect("footer requires an authoritative current model after last selection");
        assert_eq!(footer_label, last_name);
        assert!(
            !footer_label.is_empty() && !footer_label.eq_ignore_ascii_case("unknown"),
            "footer label must never be empty/unknown, got {footer_label:?}"
        );
        assert_ne!(
            footer_label, "Grok 4.5",
            "last selection footer must not paint sibling unready Grok"
        );

        let mut pw = PromptWidget::new();
        let area = Rect::new(0, 0, 80, 4);
        let mut buf = Buffer::empty(area);
        let info = PromptInfo {
            model_name: &footer_label,
            flags: &[],
            multiline: false,
            usage_warning: None,
            usage_warning_critical: false,
        };
        pw.draw(
            &mut buf,
            area,
            None,
            &PromptStyle::default(),
            Some(&info),
            None,
        );

        let mut painted = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    painted.push_str(cell.symbol());
                }
            }
            painted.push('\n');
        }
        assert!(
            painted.contains(&last_name),
            "footer must paint last Codex name, got:\n{painted}"
        );
        assert!(
            !painted.contains("Grok 4.5"),
            "footer must not paint unready Grok:\n{painted}"
        );
        assert!(
            !painted.to_ascii_lowercase().contains("unknown"),
            "footer must never paint 'unknown':\n{painted}"
        );
    }

    #[test]
    fn duplicate_display_names_show_ids_and_require_id_to_run() {
        let mut state = ModelState::default();
        let (first_id, first_info) = plain_model("provider-a/shared", "Shared Model");
        let (second_id, second_info) = plain_model("provider-b/shared", "Shared Model");
        state.available.insert(first_id.clone(), first_info);
        state.available.insert(second_id, second_info);

        let items = build_model_items(&state);
        assert!(items.iter().any(|item| {
            item.display == "Shared Model (provider-a/shared)"
                && item.insert_text == "provider-a/shared"
        }));

        let mut ctx = dummy_exec_ctx(&state);
        assert!(matches!(
            ModelCommand.run(&mut ctx, "Shared Model"),
            CommandResult::Error(message)
                if message.contains("Ambiguous model name") && message.contains("model id")
        ));
        assert!(matches!(
            ModelCommand.run(&mut ctx, "Shared Model high"),
            CommandResult::Error(message)
                if message.contains("Ambiguous model name") && message.contains("model id")
        ));
        assert!(matches!(
            ModelCommand.run(&mut ctx, "provider-a/shared"),
            CommandResult::Action(Action::SetDefaultModel(id)) if id == first_id
        ));
    }

    #[test]
    fn run_hard_blocks_invalid_auth_scheme_model() {
        let reason = r#"invalid auth_scheme "not-a-scheme": expected bearer, x_api_key, or none"#;
        let mut state = ModelState::default();
        let (id, info) = model_with_meta(
            "bad-auth",
            "Bad Auth",
            serde_json::Map::from_iter([
                ("ready".into(), serde_json::json!(false)),
                ("readinessReason".into(), serde_json::json!(reason)),
            ]),
        );
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = ModelCommand.run(&mut ctx, "Bad Auth");
        match result {
            CommandResult::Error(msg) => assert_eq!(msg, reason),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn malformed_present_readiness_metadata_fails_closed() {
        let mut state = ModelState::default();
        let (id, info) = model_with_meta(
            "malformed-ready",
            "Malformed Ready",
            serde_json::Map::from_iter([("ready".into(), serde_json::json!("yes"))]),
        );
        state.available.insert(id, info);

        let items = build_model_items(&state);
        assert_eq!(items.len(), 1);
        assert!(items[0].non_selectable);
        assert_eq!(items[0].badge, "missing");
        assert!(matches!(
            ModelCommand.run(&mut dummy_exec_ctx(&state), "malformed-ready"),
            CommandResult::Error(message) if message.contains("not ready")
        ));
    }
}
