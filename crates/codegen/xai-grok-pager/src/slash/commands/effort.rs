//! `/effort` — set reasoning effort on the current model without re-picking it.
//!
//! Thin wrapper over `Action::SwitchModel` with the session's current model
//! id and the chosen effort (same wire path as `/model <name> <effort>`).

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};
use crate::slash::commands::effort_levels::build_effort_arg_items;

/// Set reasoning effort for the active model.
pub struct EffortCommand;

impl SlashCommand for EffortCommand {
    fn name(&self) -> &str {
        "effort"
    }

    fn description(&self) -> &str {
        "Set reasoning effort for the current model"
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        // Levels are model-specific; empty-args and UnknownToken errors list
        // the active model's offered option ids instead of a hardcoded set.
        "/effort <level>"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("<level>")
    }

    fn suggest_args(&self, ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        let options = ctx.models.reasoning_effort_options();
        if options.is_empty() {
            return None;
        }
        Some(build_effort_arg_items(
            &options,
            ctx.models.reasoning_effort,
            true,
            |option| option.id.clone(),
        ))
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let trimmed = args.trim();
        let Some(model_id) = ctx.models.current.clone() else {
            return CommandResult::Error("No active model".into());
        };

        if trimmed.is_empty() {
            let offered: Vec<String> = ctx
                .models
                .reasoning_effort_options_for(&model_id)
                .into_iter()
                .map(|opt| opt.id)
                .collect();
            let current = ctx
                .models
                .reasoning_effort
                .map(|e| format!(" (current: {e})"))
                .unwrap_or_default();
            let levels = if offered.is_empty() {
                "<level>".to_string()
            } else {
                offered.join("|")
            };
            return CommandResult::Error(format!("Usage: /effort <{levels}>{current}"));
        }

        // Same gate-first policy as the CLI (`--effort`) and headless.
        match ctx.models.resolve_effort_for_model(&model_id, trimmed) {
            Ok(effort) => CommandResult::Action(Action::SwitchModel {
                model_id,
                effort: Some(effort),
                session_only: true,
            }),
            Err(err) => CommandResult::Error(err.message()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::slash::commands::effort_levels::EFFORT_LEVELS;
    use agent_client_protocol as acp;
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
    fn empty_args_errors_with_usage() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        state.reasoning_effort = Some(ReasoningEffort::Medium);
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("Usage: /effort"));
                // Legacy menu option ids only — `none` still requires an
                // explicit server menu, while Minimal is a legacy level.
                assert!(msg.contains("xhigh|high|medium|low|minimal"), "msg={msg}");
                assert!(msg.contains("current: medium"));
                assert!(!msg.contains("none"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_level_errors() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "turbo");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("unknown effort level 'turbo'"), "msg={msg}");
                assert!(msg.contains("use one of:"), "msg={msg}");
                assert!(msg.contains("xhigh"), "msg={msg}");
                assert!(!msg.contains("none"), "msg={msg}");
                assert!(msg.contains("minimal"), "msg={msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn valid_level_dispatches_switch_model_on_current() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        state.current = Some(id.clone());
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "high");
        match result {
            CommandResult::Action(Action::SwitchModel {
                model_id,
                effort,
                session_only,
            }) => {
                assert_eq!(model_id, id);
                assert_eq!(effort, Some(ReasoningEffort::High));
                assert!(session_only, "effort switch must be session-only");
            }
            other => panic!("expected SwitchModel with effort, got {other:?}"),
        }
    }

    #[test]
    fn menu_less_reasoning_model_accepts_minimal_but_rejects_none_in_effort_command() {
        // The menu-less fallback preserves legacy Minimal. `none` remains
        // server-advertised only, so it is still rejected here.
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        state.current = Some(id.clone());
        let mut ctx = dummy_exec_ctx(&state);
        match EffortCommand.run(&mut ctx, "minimal") {
            CommandResult::Action(Action::SwitchModel { model_id, effort, session_only }) => {
                assert_eq!(model_id, id);
                assert_eq!(effort, Some(ReasoningEffort::Minimal));
            }
            other => panic!("expected SwitchModel with minimal, got {other:?}"),
        }

        let CommandResult::Error(msg) = EffortCommand.run(&mut ctx, "none") else {
            panic!("expected Error for none");
        };
        assert!(msg.contains("unknown effort level 'none'"), "msg={msg}");
        let after_prefix = msg
            .split_once("; ")
            .map(|(_, rest)| rest)
            .unwrap_or(msg.as_str());
        assert!(!after_prefix.contains("none"), "msg={msg}");
        assert!(!msg.contains("unset"), "msg={msg}");
    }

    #[test]
    fn none_accepted_when_model_menu_offers_it() {
        let mut state = ModelState::default();
        let id = acp::ModelId::new(Arc::from("voice-dual"));
        let info = acp::ModelInfo::new(id.clone(), "Voice Dual".to_string()).meta(
            serde_json::json!({
                "supportsReasoningEffort": true,
                "reasoningEfforts": [
                    { "value": "none", "label": "None", "default": true },
                    { "value": "high", "label": "High" },
                ],
            })
            .as_object()
            .cloned(),
        );
        state.available.insert(id.clone(), info);
        state.current = Some(id.clone());
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "none");
        match result {
            CommandResult::Action(Action::SwitchModel { model_id, effort, session_only }) => {
                assert_eq!(model_id, id);
                assert_eq!(effort, Some(ReasoningEffort::None));
            }
            other => panic!("expected SwitchModel with none, got {other:?}"),
        }
    }

    #[test]
    fn ultra_is_rendered_and_dispatched_when_model_menu_offers_it() {
        let mut state = ModelState::default();
        let id = acp::ModelId::new(Arc::from("gpt-5.6-sol"));
        let info = acp::ModelInfo::new(id.clone(), "GPT-5.6 Sol".to_string()).meta(
            serde_json::json!({
                "supportsReasoningEffort": true,
                "reasoningEfforts": [{
                    "value": "ultra",
                    "label": "Ultra",
                    "description": "Maximum reasoning with proactive multi-agent guidance",
                    "default": true
                }],
            })
            .as_object()
            .cloned(),
        );
        state.available.insert(id.clone(), info);
        state.current = Some(id.clone());

        let app_ctx = AppCtx {
            models: &state,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        let items = EffortCommand
            .suggest_args(&app_ctx, "")
            .expect("Ultra model has a menu");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert_text, "ultra");
        assert_eq!(items[0].display, "Ultra");
        assert_eq!(
            items[0].description,
            "Maximum reasoning with proactive multi-agent guidance"
        );

        let mut exec_ctx = dummy_exec_ctx(&state);
        match EffortCommand.run(&mut exec_ctx, "ultra") {
            CommandResult::Action(Action::SwitchModel { model_id, effort, session_only }) => {
                assert_eq!(model_id, id);
                assert_eq!(effort, Some(ReasoningEffort::Ultra));
            }
            other => panic!("expected SwitchModel Ultra, got {other:?}"),
        }
    }

    #[test]
    fn remap_id_dispatches_mapped_canonical_effort() {
        let mut state = ModelState::default();
        let id = acp::ModelId::new(Arc::from("reasoning-x"));
        let info = acp::ModelInfo::new(id.clone(), "Reasoning X".to_string()).meta(
            serde_json::json!({
                "supportsReasoningEffort": true,
                "reasoningEfforts": [{ "id": "deep", "value": "xhigh", "label": "Deep" }],
            })
            .as_object()
            .cloned(),
        );
        state.available.insert(id.clone(), info);
        state.current = Some(id.clone());
        let mut ctx = dummy_exec_ctx(&state);
        // The rendered row inserts the id; `/effort deep` must send `xhigh`.
        match EffortCommand.run(&mut ctx, "deep") {
            CommandResult::Action(Action::SwitchModel { model_id, effort, session_only }) => {
                assert_eq!(model_id, id);
                assert_eq!(effort, Some(ReasoningEffort::Xhigh));
            }
            other => panic!("expected SwitchModel with remapped effort, got {other:?}"),
        }
    }

    #[test]
    fn non_reasoning_model_errors() {
        let mut state = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "high");
        assert!(matches!(
            result,
            CommandResult::Error(msg) if msg.contains("does not support reasoning effort")
        ));
    }

    #[test]
    fn no_current_model_errors() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id, info);
        let mut ctx = dummy_exec_ctx(&state);
        let result = EffortCommand.run(&mut ctx, "high");
        assert!(matches!(result, CommandResult::Error(msg) if msg.contains("No active model")));
    }

    #[test]
    fn suggest_args_none_without_current_or_support() {
        let cmd = EffortCommand;
        let empty = ModelState::default();
        let ctx = AppCtx {
            models: &empty,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        assert!(cmd.suggest_args(&ctx, "").is_none());

        let mut plain = ModelState::default();
        let (id, info) = plain_model("grok-4.5", "Grok 4.5");
        plain.available.insert(id.clone(), info);
        plain.current = Some(id);
        let ctx = AppCtx {
            models: &plain,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            screen_mode: crate::app::ScreenMode::Fullscreen,
        };
        assert!(cmd.suggest_args(&ctx, "").is_none());
    }

    #[test]
    fn suggest_args_lists_levels_with_active_marker() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("reasoning-x", "Reasoning X");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        state.reasoning_effort = Some(ReasoningEffort::High);

        let cmd = EffortCommand;
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
        assert_eq!(items.len(), EFFORT_LEVELS.len());
        assert_eq!(items[0].insert_text, "xhigh");
        assert_eq!(items[1].insert_text, "high");
        assert_eq!(items[1].display, "high (active)");
        assert_eq!(items[2].insert_text, "medium");
        assert_eq!(items[3].insert_text, "low");
        assert!(items[0].match_text.starts_with("a "));
        assert!(items[3].match_text.starts_with("d "));
    }

    /// #306 gate 4 (**component presentation** — fabricated metadata, not shell
    /// dispatch; #306 live TUI still open): effort menu is model-specific; none
    /// only when advertised;
    /// menu-less models still accept minimal and reject none.
    #[test]
    fn codex_effort_picker_obeys_each_model_menu_including_minimal_and_none() {
        let mut state = ModelState::default();
        // Model A: catalog menu with none
        let id_a = acp::ModelId::new(Arc::from("codex-with-none"));
        let info_a = acp::ModelInfo::new(id_a.clone(), "Codex With None".to_string()).meta(
            serde_json::json!({
                "supportsReasoningEffort": true,
                "reasoningEfforts": [
                    { "value": "none", "label": "None", "default": true },
                    { "value": "high", "label": "High" },
                ],
            })
            .as_object()
            .cloned(),
        );
        // Model B: menu-less reasoning
        let (id_b, info_b) = model_with_reasoning("reasoning-menu-less", "Reasoning Menu Less");
        state.available.insert(id_a.clone(), info_a);
        state.available.insert(id_b.clone(), info_b);

        let cmd = EffortCommand;

        // Seat A
        state.current = Some(id_a.clone());
        let suggest_a = {
            let ctx = AppCtx {
                models: &state,
                cwd: std::path::Path::new("."),
                has_session_announcements: false,
                billing_surface_visible: true,
                usage_command_visible: true,
                workflows_available: true,
                screen_mode: crate::app::ScreenMode::Fullscreen,
            };
            cmd.suggest_args(&ctx, "").expect("A has menu")
        };
        let ids_a: Vec<String> = suggest_a.iter().map(|i| i.insert_text.clone()).collect();
        assert!(
            ids_a.iter().any(|s| s == "none"),
            "A must offer none: {ids_a:?}"
        );
        assert!(
            ids_a.iter().any(|s| s == "high"),
            "A must offer high: {ids_a:?}"
        );
        {
            let mut ctx = dummy_exec_ctx(&state);
            match EffortCommand.run(&mut ctx, "none") {
                CommandResult::Action(Action::SwitchModel { model_id, effort, session_only }) => {
                    assert_eq!(model_id, id_a);
                    assert_eq!(effort, Some(ReasoningEffort::None));
                }
                other => panic!("expected SwitchModel none, got {other:?}"),
            }
        }

        // Seat B
        state.current = Some(id_b.clone());
        let suggest_b = {
            let ctx = AppCtx {
                models: &state,
                cwd: std::path::Path::new("."),
                has_session_announcements: false,
                billing_surface_visible: true,
                usage_command_visible: true,
                workflows_available: true,
                screen_mode: crate::app::ScreenMode::Fullscreen,
            };
            cmd.suggest_args(&ctx, "").expect("B has legacy menu")
        };
        let ids_b: Vec<String> = suggest_b.iter().map(|i| i.insert_text.clone()).collect();
        assert_ne!(ids_a, ids_b, "suggest lists must differ between A and B");
        assert!(
            !ids_b.iter().any(|s| s == "none"),
            "B must not offer none: {ids_b:?}"
        );
        assert!(
            ids_b.iter().any(|s| s == "minimal"),
            "B must offer minimal: {ids_b:?}"
        );
        {
            let mut ctx = dummy_exec_ctx(&state);
            match EffortCommand.run(&mut ctx, "minimal") {
                CommandResult::Action(Action::SwitchModel { model_id, effort, session_only }) => {
                    assert_eq!(model_id, id_b);
                    assert_eq!(effort, Some(ReasoningEffort::Minimal));
                }
                other => panic!("expected minimal, got {other:?}"),
            }
            let CommandResult::Error(msg) = EffortCommand.run(&mut ctx, "none") else {
                panic!("expected Error for none on B");
            };
            assert!(msg.contains("unknown effort level 'none'"), "msg={msg}");
        }
    }
}
