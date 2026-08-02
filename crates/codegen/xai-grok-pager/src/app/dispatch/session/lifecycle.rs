//! New, exit, cloud, and worktree session dispatchers plus trust and startup actions.
use super::fork::{dispatch_startup_fork_session, worktree_persist_options};
use super::load::dispatch_load_session;
use super::modal::remove_agent_and_cleanup;
use crate::acp::model_state::{EffortTokenError, ModelState};
use crate::acp::tracker::AcpUpdateTracker;
use crate::app::actions::{Action, Effect, SwitchModelError};
use crate::app::agent::{AgentCommand, AgentId, AgentSession, AgentState, ModelSwitchRollback};
use crate::app::agent_view::{ActivePane, AgentView, McpInitProgress};
use crate::app::app_view::{ActiveView, AppView, ModelSwitchTransaction, TrustState};
use crate::app::dispatch::ctx::{
    SwitchCause, get_active_agent, reseed_tip_for_new_session, show_welcome, switch_to_agent,
};
use crate::app::dispatch::modes::inherit_auto_mode;
use crate::app::dispatch::prompt::{consume_chat_kind, dispatch_initial_prompt};
use crate::app::dispatch::queue::{QueueDrain, maybe_drain_queue, note_peek_page_flip};
use crate::app::dispatch::router::dispatch;
use crate::app::dispatch::status::notify_session_ready;
use crate::app::dispatch::task_result::unregister_session_effect;
use crate::app::dispatch::transcript::extensions_modal_tab_fetches;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::SessionEvent;
use crate::scrollback::state::ScrollbackState;
use agent_client_protocol as acp;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use xai_grok_shell::sampling::types::ReasoningEffort;
/// A deferred model switch to apply once the session exists, plus any effort
/// error to surface. `switch` is still populated when a `-m` model was stashed
/// even if the effort token failed, so an invalid effort never drops the CLI
/// model override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeferredSwitchOutcome {
    pub switch: Option<(acp::ModelId, Option<ReasoningEffort>)>,
    pub effort_error: Option<EffortTokenError>,
}

static NEXT_MODEL_SWITCH_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Open the pager-side transaction for one model switch. Ordinary and
/// default-model switches share one app-global admission slot.
pub(crate) fn begin_model_switch_request(
    transaction: &mut Option<ModelSwitchTransaction>,
    agent_id: AgentId,
    session: &mut AgentSession,
    app_models: &ModelState,
) -> Option<u64> {
    if session.model_switch_pending {
        return None;
    }
    match transaction.as_ref() {
        Some(active) if active.owner_agent_id == agent_id && active.request_id.is_none() => {}
        Some(_) => return None,
        None => {
            *transaction = Some(ModelSwitchTransaction {
                owner_agent_id: agent_id,
                request_id: None,
                app_models_optimistic: false,
                app_model_id: app_models.current.clone(),
                app_reasoning_effort: app_models.reasoning_effort,
            });
        }
    }
    let request_id = NEXT_MODEL_SWITCH_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    session.model_switch_pending = true;
    session.model_switch_request_id = Some(request_id);
    let rollback = session
        .model_switch_rollback
        .get_or_insert_with(|| ModelSwitchRollback {
            request_id: Some(request_id),
            session_model_id: session.models.current.clone(),
            session_reasoning_effort: session.models.reasoning_effort,
        });
    rollback.request_id.get_or_insert(request_id);
    transaction
        .as_mut()
        .expect("transaction opened above")
        .request_id = Some(request_id);
    Some(request_id)
}

/// Reserve the global slot before an ACP session id exists.
pub(crate) fn begin_deferred_model_switch(
    app: &mut AppView,
    agent_id: AgentId,
    app_models_optimistic: bool,
) -> bool {
    if let Some(transaction) = app.model_switch_transaction.as_ref() {
        return transaction.owner_agent_id == agent_id && transaction.request_id.is_none();
    }
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return false;
    };
    agent
        .session
        .model_switch_rollback
        .get_or_insert_with(|| ModelSwitchRollback {
            request_id: None,
            session_model_id: agent.session.models.current.clone(),
            session_reasoning_effort: agent.session.models.reasoning_effort,
        });
    app.model_switch_transaction = Some(ModelSwitchTransaction {
        owner_agent_id: agent_id,
        request_id: None,
        app_models_optimistic,
        app_model_id: app.models.current.clone(),
        app_reasoning_effort: app.models.reasoning_effort,
    });
    true
}

pub(crate) fn mark_model_switch_app_optimistic(app: &mut AppView, agent_id: AgentId) {
    if let Some(transaction) = app.model_switch_transaction.as_mut()
        && transaction.owner_agent_id == agent_id
    {
        transaction.app_models_optimistic = true;
    }
}

pub(crate) fn abort_model_switch_transaction(app: &mut AppView, agent_id: AgentId) {
    let Some(transaction) = app
        .model_switch_transaction
        .take_if(|transaction| transaction.owner_agent_id == agent_id)
    else {
        return;
    };
    if transaction.app_models_optimistic {
        app.models.current = transaction.app_model_id;
        app.models.reasoning_effort = transaction.app_reasoning_effort;
    }
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.session.model_switch_pending = false;
        agent.session.model_switch_request_id = None;
        agent.session.model_switch_rollback = None;
    }
}

fn handoff_model_switch_transaction(app: &mut AppView, from: AgentId, to: AgentId) {
    if let Some(transaction) = app.model_switch_transaction.as_mut()
        && transaction.owner_agent_id == from
    {
        transaction.owner_agent_id = to;
        transaction.request_id = None;
    }
}
/// Resolve the stashed `-m` switch and/or `cli_effort_token` against the session
/// catalog via [`ModelState::resolve_effort_for_model`] (same gate-first policy
/// as `/effort` and headless).
pub(crate) fn take_deferred_model_switch(
    stashed: Option<(acp::ModelId, Option<ReasoningEffort>)>,
    models: &ModelState,
    cli_effort_token: Option<&str>,
) -> DeferredSwitchOutcome {
    if let Some((model_id, mut effort)) = stashed {
        let effort_error = match cli_effort_token {
            Some(token) if effort.is_none() => {
                match models.resolve_effort_for_model(&model_id, token) {
                    Ok(resolved) => {
                        effort = Some(resolved);
                        None
                    }
                    Err(err) => Some(err),
                }
            }
            _ => None,
        };
        return DeferredSwitchOutcome {
            switch: Some((model_id, effort)),
            effort_error,
        };
    }
    let Some(token) = cli_effort_token else {
        return DeferredSwitchOutcome {
            switch: None,
            effort_error: None,
        };
    };
    let Some(current) = models.current.clone() else {
        return DeferredSwitchOutcome {
            switch: None,
            effort_error: Some(EffortTokenError::NoActiveModel),
        };
    };
    match models.resolve_effort_for_model(&current, token) {
        Ok(effort) if models.reasoning_effort == Some(effort) => DeferredSwitchOutcome {
            switch: None,
            effort_error: None,
        },
        Ok(effort) => DeferredSwitchOutcome {
            switch: Some((current, Some(effort))),
            effort_error: None,
        },
        Err(err) => DeferredSwitchOutcome {
            switch: None,
            effort_error: Some(err),
        },
    }
}
/// Take the stashed deferred switch, resolve any CLI effort token against the
/// session catalog, surface effort errors, and return the switch to apply.
pub(crate) fn apply_deferred_model_switch(
    agent: &mut AgentView,
    cli_effort_token: Option<&str>,
) -> Option<(acp::ModelId, Option<ReasoningEffort>)> {
    let stashed = agent.session.deferred_model_switch.take();
    let outcome = take_deferred_model_switch(stashed, &agent.session.models, cli_effort_token);
    let applied = apply_deferred_switch_outcome(agent, outcome);
    if applied.is_none() {
        agent.session.model_switch_rollback = None;
    }
    applied
}
/// Surface effort-token errors and return the model/effort switch (if any).
pub(crate) fn apply_deferred_switch_outcome(
    agent: &mut AgentView,
    outcome: DeferredSwitchOutcome,
) -> Option<(acp::ModelId, Option<ReasoningEffort>)> {
    if let Some(err) = outcome.effort_error {
        let msg = format!("--effort/--reasoning-effort: {}", err.message());
        tracing::warn!("{msg}");
        agent.show_toast(&msg);
        agent.scrollback.push_block(RenderBlock::system(msg));
    }
    let (model_id, effort) = outcome.switch?;
    // Re-check at apply time: catalog/meta may have flipped unready between
    // stash (CLI / pre-session) and session hydrate.
    if let Some(reason) =
        crate::slash::commands::model::model_not_ready_reason(&agent.session.models, &model_id)
    {
        tracing::warn!("deferred model switch blocked: {reason}");
        agent.show_toast(&reason);
        return None;
    }
    Some((model_id, effort))
}
/// Top-level `/new` dispatcher. If the working directory is inside a
/// git repository, consults the persisted `new_session_worktree_mode`
/// preference: `Always` skips the popup and creates a worktree, `Never`
/// skips the popup and stays in-cwd, `Ask` opens the worktree question
/// modal (mirroring `/fork`'s UX). Otherwise proceeds directly via
/// [`dispatch_new_session_inner`].
///
/// When no active agent exists (e.g. from the welcome screen), the
/// git-repo check falls back to `app.cwd_has_git_ancestor` so the
/// persisted `Always` / `Never` modes still take effect. The `Ask`
/// path falls through to `dispatch_new_session_inner` in that case
/// because `open_new_session_question` requires an active agent.
pub(in crate::app::dispatch) fn dispatch_new_session(app: &mut AppView) -> Vec<Effect> {
    use crate::app::app_view::WorktreeMode;
    if !app.session_startup_allowed() {
        app.deferred_startup.new_session = true;
        return vec![];
    }
    #[cfg(feature = "local-workspace")]
    if matches!(app.active_view, ActiveView::Welcome) {
        let skip_apply = app.welcome_session_local_workspace.is_some();
        if !skip_apply && let Err(effects) = apply_welcome_workspace_on_new_session(app) {
            return effects;
        }
    }
    let in_git_repo = get_active_agent(app)
        .map(|a| a.current_branch.is_some())
        .unwrap_or(app.cwd_has_git_ancestor);
    if in_git_repo {
        match app.new_session_worktree_mode {
            WorktreeMode::Always => {
                dispatch_new_worktree_session(app, None, None, None, None, None, None)
            }
            WorktreeMode::Never => dispatch_new_session_inner(app, None),
            WorktreeMode::Ask => {
                if matches!(app.active_view, ActiveView::Agent(_)) {
                    open_new_session_question(app)
                } else {
                    dispatch_new_session_inner(app, None)
                }
            }
        }
    } else {
        dispatch_new_session_inner(app, None)
    }
}
/// Open the local worktree question modal for `/new`. Mirrors
/// [`open_fork_question`] but uses [`LocalQuestionKind::NewSession`] so
/// the answer routes to [`dispatch_new_session_inner`] or
/// [`dispatch_new_worktree_session`].
pub(in crate::app::dispatch) fn open_new_session_question(app: &mut AppView) -> Vec<Effect> {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use xai_grok_tools::implementations::grok_build::ask_user_question::{
        Question, QuestionOption,
    };
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if agent.question_view.is_some() {
        app.show_toast("Finish answering the current question first");
        return vec![];
    }
    let mut options = vec![
        QuestionOption {
            label: "Yes".into(),
            description: "New session in a new isolated git worktree".into(),
            preview: None,
            id: None,
        },
        QuestionOption {
            label: "No".into(),
            description: "New session in the current cwd".into(),
            preview: None,
            id: None,
        },
    ];
    options.extend(worktree_persist_options());
    let question = Question {
        question: "Start the new session in an isolated git worktree?".into(),
        id: None,
        options,
        multi_select: Some(false),
    };
    let agent = app.agents.get_mut(&id).expect("agent present (re-borrow)");
    let stashed = agent.prompt.stash();
    let state = QuestionViewState::new(
        format!("new-session-{}", uuid::Uuid::new_v4()),
        vec![question],
        stashed,
    )
    .with_local_kind(LocalQuestionKind::NewSession)
    .with_no_freeform();
    agent.question_view = Some(state);
    agent.prompt.set_text("");
    vec![]
}
/// Open a local QuestionView asking whether to start a new session for a
/// model that requires a different agent harness. Mirrors
/// [`open_new_session_question`] but uses
/// [`LocalQuestionKind::AgentTypeMismatch`] so the answer routes to
/// [`dispatch_new_session_inner`] with a deferred model switch.
pub(in crate::app::dispatch) fn open_agent_type_mismatch_question(
    app: &mut AppView,
    agent_id: AgentId,
    model_id: acp::ModelId,
    effort: Option<xai_grok_shell::sampling::types::ReasoningEffort>,
    model_name: &str,
) -> Vec<Effect> {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use xai_grok_tools::implementations::grok_build::ask_user_question::{
        Question, QuestionOption,
    };
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    if agent.question_view.is_some() {
        agent.session.model_switch_pending = false;
        agent.session.model_switch_request_id = None;
        agent.session.model_switch_rollback = None;
        agent.show_toast("Finish answering the current question first");
        let drain = maybe_drain_queue(agent);
        let page_flip_entry = drain.page_flip_entry;
        let effects = drain.effects;
        note_peek_page_flip(app, agent_id, page_flip_entry);
        return effects;
    }
    let question = Question {
        question: format!("Switching to {model_name} requires starting a new session. Continue?"),
        id: None,
        options: vec![
            QuestionOption {
                label: "Yes".into(),
                description: format!("Start a new session with {model_name}"),
                preview: None,
                id: None,
            },
            QuestionOption {
                label: "No".into(),
                description: "Continue the current session".into(),
                preview: None,
                id: None,
            },
        ],
        multi_select: Some(false),
    };
    let agent = app
        .agents
        .get_mut(&agent_id)
        .expect("agent present (re-borrow)");
    let stashed = agent.prompt.stash();
    let state = QuestionViewState::new(
        format!("agent-type-mismatch-{}", uuid::Uuid::new_v4()),
        vec![question],
        stashed,
    )
    .with_local_kind(LocalQuestionKind::AgentTypeMismatch {
        source_id: agent_id,
        model_id,
        effort,
    })
    .with_no_freeform();
    agent.question_view = Some(state);
    agent.prompt.set_text("");
    vec![]
}

/// Soft-confirm when switching models across auth classes (`none` ↔
/// `session` / `env`). Mirrors [`open_agent_type_mismatch_question`].
pub(in crate::app::dispatch) fn open_auth_class_switch_question(
    app: &mut AppView,
    model_id: acp::ModelId,
    effort: Option<xai_grok_shell::sampling::types::ReasoningEffort>,
    persist_default: bool,
    model_name: &str,
    from_class: &str,
    to_class: &str,
) -> Vec<Effect> {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use xai_grok_tools::implementations::grok_build::ask_user_question::{
        Question, QuestionOption,
    };
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if agent.question_view.is_some() {
        app.show_toast("Finish answering the current question first");
        return vec![];
    }
    let question = Question {
        question: format!(
            "Switching to {model_name} changes auth from {from_class} to {to_class}. Continue?"
        ),
        id: None,
        options: vec![
            QuestionOption {
                label: "Yes".into(),
                description: format!("Switch to {model_name}"),
                preview: None,
                id: None,
            },
            QuestionOption {
                label: "No".into(),
                description: "Keep the current model".into(),
                preview: None,
                id: None,
            },
        ],
        multi_select: Some(false),
    };
    let agent = app.agents.get_mut(&id).expect("agent present (re-borrow)");
    let stashed = agent.prompt.stash();
    let state = QuestionViewState::new(
        format!("auth-class-switch-{}", uuid::Uuid::new_v4()),
        vec![question],
        stashed,
    )
    .with_local_kind(LocalQuestionKind::AuthClassSwitch {
        model_id,
        effort,
        persist_default,
    })
    .with_no_freeform();
    agent.question_view = Some(state);
    agent.prompt.set_text("");
    vec![]
}

/// Core new-session logic: create a placeholder agent, push the
/// `/dashboard` tip, and return the `CreateSession` effect.
///
/// Apply welcome workspace selection before creating a session from Welcome.
///
/// Returns `Err(effects)` when session creation should abort (ACK pending or
/// empty effects). `Ok(())` means continue into the normal new-session path.
#[cfg(feature = "local-workspace")]
fn apply_welcome_workspace_on_new_session(app: &mut AppView) -> Result<(), Vec<Effect>> {
    use crate::views::welcome::workspace_mode::{
        WelcomeWorkspaceMode, WelcomeWorkspacePrepare, prepare_welcome_workspace_for_new_session,
    };
    match prepare_welcome_workspace_for_new_session(
        app.welcome_workspace_mode,
        app.local_workspace_startup_locked,
        app.chat_mode,
        &app.cwd,
        false,
    ) {
        Ok(WelcomeWorkspacePrepare::Continue {
            session_override,
            warning,
        }) => {
            if let Some(msg) = warning {
                tracing::warn!("{msg}");
                app.show_toast(&msg);
            }
            if let Some(override_cfg) = session_override {
                app.welcome_session_local_workspace = Some(override_cfg);
            }
            Ok(())
        }
        Ok(WelcomeWorkspacePrepare::AwaitAck) => {
            app.welcome_local_workspace_ack_pending = true;
            app.session_picker_entries = None;
            app.session_picker_loading = false;
            app.session_picker_list_seq = app.session_picker_list_seq.saturating_add(1);
            Err(vec![])
        }
        Err(err) => {
            tracing::warn!("welcome workspace mode: {err}");
            app.show_toast(&format!(
                "Local workspace unavailable ({err}); using sandbox"
            ));
            app.welcome_session_local_workspace = Some(None);
            app.welcome_workspace_mode = WelcomeWorkspaceMode::Sandbox;
            Ok(())
        }
    }
}
/// Factored out of [`dispatch_new_session`] so the worktree-question
/// "No" path can call it directly without re-opening the modal.
pub(in crate::app::dispatch) fn dispatch_new_session_inner(
    app: &mut AppView,
    model_id: Option<acp::ModelId>,
) -> Vec<Effect> {
    let (_id, effects) = dispatch_new_session_inner_with_id(app, model_id);
    effects
}
/// Sibling that returns the new `AgentId` alongside
/// the effects. Used by `dispatch_dashboard_dispatch` so it doesn't
/// rely on the (correct-but-brittle) `app.agents.last()` lookup.
pub(in crate::app::dispatch) fn dispatch_new_session_inner_with_id(
    app: &mut AppView,
    model_id: Option<acp::ModelId>,
) -> (AgentId, Vec<Effect>) {
    let mut effects =
        unregister_session_effect(get_active_agent(app).and_then(|a| a.session.session_id.clone()));
    let (effective_cwd, inherit_worktree) = get_active_agent(app)
        .map(|a| (a.session.cwd.clone(), a.session.is_worktree))
        .unwrap_or_else(|| (app.cwd.clone(), false));
    reseed_tip_for_new_session(app);
    let agent_id = AgentId(app.next_agent_id);
    app.next_agent_id += 1;
    let mut scrollback = ScrollbackState::new();
    scrollback.set_appearance(app.appearance.clone());
    let agent = AgentView::new(
        AgentSession {
            id: agent_id,
            acp_tx: app.acp_tx.clone(),
            session_id: None,
            models: app.models.clone(),
            state: AgentState::Idle,
            tracker: AcpUpdateTracker::new(),
            cwd: effective_cwd.clone(),
            is_worktree: inherit_worktree,
            forked_from: None,
            pending_prompts: std::collections::VecDeque::new(),
            next_queue_id: 0,
            yolo_mode: app.default_yolo,
            auto_mode: inherit_auto_mode(app),
            prompt_history: Vec::new(),
            prompt_history_loading: false,
            loading_replay: false,
            restore_degree: None,
            rate_limited: false,
            model_incompatible: false,
            credit_limit_blocked: false,
            free_usage_blocked: false,
            available_commands: app.bootstrap_acp_commands.clone(),
            available_commands_generation: 1,
            available_tools: None,
            model_switch_pending: false,
            model_switch_request_id: None,
            model_switch_rollback: None,
            model_switch_queue_handoff_from: None,
            user_model_preference: None,
            deferred_model_switch: app.deferred_model_switch_from_cli(),
            bg_tasks: std::collections::BTreeMap::new(),
            bg_tool_call_to_task: std::collections::HashMap::new(),
            scheduled_tasks: std::collections::HashMap::new(),
            in_flight_prompt: None,
            compact_held_prompt: None,
            current_prompt_id: None,
            created_via_new: true,
        },
        scrollback,
    );
    app.agents.insert(agent_id, agent);
    {
        let agent = app.agents.get_mut(&agent_id).unwrap();
        agent.prompt.set_compact(app.appearance.prompt.compact);
        agent.prompt.adopt_slash_mru(app.slash_mru.clone());
        agent.prompt.adopt_command_tags(app.command_tags.clone());
        agent
            .prompt
            .set_contextual_hints(app.contextual_hints.undo, app.contextual_hints.plan_mode);
        agent.set_session_recap_available(app.session_recap_available);
        agent.set_voice_mode_available(app.voice_mode_enabled);
        agent.apply_app_scoped_gates(
            app.sharing_enabled,
            app.usage_visible,
            !app.has_external_auth_provider,
            app.chat_mode,
            app.screen_mode,
            &app.active_announcements,
            &app.tier_restricted_commands,
        );
        agent.apply_credit_balance(app.credit_balance.clone(), app.auto_topup.clone());
        agent
            .prompt
            .slash_controller
            .registry_mut()
            .set_plugins_visible(!app.appearance.disable_plugins);
        agent.active_pane = ActivePane::Prompt;
    }
    switch_to_agent(app, agent_id, SwitchCause::New);
    if app.screen_mode.is_minimal() {
        app.minimal_state.welcome_pending = true;
    }
    if !app.needs_project_picker() {
        let chat_kind = consume_chat_kind(app);
        if let Some(agent) = app.agents.get_mut(&agent_id) {
            agent.chat_kind = chat_kind;
            #[cfg(feature = "local-workspace")]
            {
                let local_intent = match &app.welcome_session_local_workspace {
                    Some(Some(_)) => true,
                    Some(None) => false,
                    None => crate::app::session_startup::active_local_workspace()
                        .ok()
                        .flatten()
                        .is_some(),
                };
                let (mode, locked) =
                    crate::views::welcome::workspace_mode::indicator_for_opening_session(
                        agent.chat_kind,
                        false,
                        app.local_workspace_startup_locked,
                        local_intent,
                    );
                agent.workspace_mode = mode;
                agent.workspace_mode_cli_locked = locked;
            }
            agent.apply_credit_balance(app.credit_balance.clone(), app.auto_topup.clone());
            agent.mcp_init_progress = Some(McpInitProgress {
                total: 0,
                connected: 0,
                started_at: Instant::now(),
            });
            agent.session.prompt_history_loading = true;
        }
        let preferred_session_id = app.deferred_startup.preferred_session_id.take();
        effects.push(Effect::CreateSession {
            agent_id,
            cwd: effective_cwd,
            model_id,
            preferred_session_id,
            chat_kind,
        });
    }
    (agent_id, effects)
}
/// Exit the current session and return to the welcome screen.
pub(in crate::app::dispatch) fn dispatch_exit_session(app: &mut AppView) -> Vec<Effect> {
    let effects =
        unregister_session_effect(get_active_agent(app).and_then(|a| a.session.session_id.clone()));
    show_welcome(app);
    app.welcome_prompt_focused = true;
    app.session_picker_entries = None;
    app.session_picker_loading = false;
    app.session_picker_state.selected = 0;
    app.session_picker_content_results = None;
    app.session_picker_content_loading = false;
    app.exit_session_pending = None;
    effects
}
/// Confirm deleting the parent session (not a subagent view).
pub(in crate::app::dispatch) fn open_delete_current_session_question(
    app: &mut AppView,
) -> Vec<Effect> {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use xai_grok_tools::implementations::grok_build::ask_user_question::{
        Question, QuestionOption,
    };
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if agent.session.session_id.is_none() {
        app.show_toast("No active session to delete");
        return vec![];
    }
    if agent.question_view.is_some() {
        app.show_toast("Finish answering the current question first");
        return vec![];
    }
    let question = Question {
        question: "Delete this session permanently?".into(),
        id: None,
        options: vec![
            QuestionOption {
                label: "Delete".into(),
                description: "Remove history and return home".into(),
                preview: None,
                id: None,
            },
            QuestionOption {
                label: "Cancel".into(),
                description: "Keep the session".into(),
                preview: None,
                id: None,
            },
        ],
        multi_select: Some(false),
    };
    let stashed = agent.prompt.stash();
    agent.question_view = Some(
        QuestionViewState::new(
            format!("delete-session-{}", uuid::Uuid::new_v4()),
            vec![question],
            stashed,
        )
        .with_local_kind(LocalQuestionKind::DeleteCurrentSession)
        .with_no_freeform(),
    );
    agent.prompt.set_text("");
    vec![]
}
pub(in crate::app::dispatch) fn dispatch_delete_current_session_answered(
    app: &mut AppView,
    confirmed: bool,
) -> Vec<Effect> {
    if !confirmed {
        return vec![];
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some((session_id, cwd, running_bg_tasks)) = app.agents.get(&id).and_then(|agent| {
        let session_id = agent.session.session_id.clone()?;
        let cwd = agent.session.cwd.display().to_string();
        let running_bg_tasks: Vec<String> = agent
            .session
            .bg_tasks
            .values()
            .filter(|t| t.status == crate::app::agent::BgTaskStatus::Running)
            .map(|t| t.task_id.clone())
            .collect();
        Some((session_id, cwd, running_bg_tasks))
    }) else {
        app.show_toast("No active session to delete");
        return vec![];
    };
    let mut effects = vec![Effect::CancelTurn {
        session_id: session_id.clone(),
        cancel_subagents: true,
        trigger: None,
        rewind_if_pristine: false,
    }];
    effects.extend(
        running_bg_tasks
            .into_iter()
            .map(|task_id| Effect::KillBgTask {
                session_id: session_id.clone(),
                task_id,
            }),
    );
    app.show_toast("Deleting session\u{2026}");
    effects.push(Effect::DeleteSession {
        source: "current".into(),
        session_id: session_id.to_string(),
        cwd,
        after: crate::app::actions::AfterSessionDelete::Welcome,
    });
    effects
}
/// Handle the user accepting the folder-trust question: persist the grant for
/// the workspace (writes `~/.grok/trusted_folders.toml`), mark trust resolved,
/// then replay any deferred session startup (only if auth is also done).
pub(in crate::app::dispatch) fn dispatch_trust_folder(app: &mut AppView) -> Vec<Effect> {
    if let TrustState::Pending { workspace } = &app.trust_state {
        xai_grok_workspace::folder_trust::grant_folder_trust(workspace);
    }
    finish_trust(app)
}
/// Tail of accepting the folder-trust question (via [`dispatch_trust_folder`];
/// declining quits instead): resolve `trust_state` to `Done`, focus the welcome
/// prompt, and replay the deferred session startup once auth is also resolved.
/// Drains iff [`AppView::session_startup_allowed`] -- the same predicate
/// `AuthComplete` uses, so whichever gate resolves last drains exactly once.
pub(in crate::app::dispatch) fn finish_trust(app: &mut AppView) -> Vec<Effect> {
    app.trust_state = TrustState::Done;
    app.welcome_prompt_focused = !app.is_access_blocked();
    if app.session_startup_allowed() {
        drain_startup_actions(app)
    } else {
        vec![]
    }
}
/// Clear EVERY deferred `startup_*` action without replaying any. Used on the
/// paths that must not run startup at all — ZDR-blocked login, and mid-session
/// re-auth (`auth_return_view`) where a session already exists — so a stash
/// (including an incidental mid-session `Ctrl+N` that the chokepoint deferred)
/// can never linger and fire later. Atomic counterpart to `drain_startup_actions`.
pub(in crate::app::dispatch) fn clear_startup_actions(app: &mut AppView) {
    let _ = app.deferred_startup.take();
}
/// Replay the session-startup actions deferred until auth + trust both resolved
/// (`--resume` / `--worktree` / initial-prompt / `grok dashboard`). Extracted
/// from the `AuthComplete` handler so the folder-trust answer can run the SAME
/// machinery; whichever gate resolves last drains it (each call site guards on
/// the other gate being `Done`, so it runs exactly once).
pub(in crate::app::dispatch) fn drain_startup_actions(app: &mut AppView) -> Vec<Effect> {
    use crate::app::session_startup::{DeferredSessionStartup, DeferredStartupActions};
    debug_assert!(
        app.session_startup_allowed(),
        "drain_startup_actions must run only with the startup gate open"
    );
    let DeferredStartupActions {
        session: deferred,
        preferred_session_id: preferred_id,
        worktree,
        worktree_label,
        worktree_ref,
        new_session,
        prompt,
        open_dashboard,
        pending_chat,
        #[cfg(feature = "local-workspace")]
        history_load_as_build,
    } = app.deferred_startup.take();
    let mut effects = Vec::new();
    match deferred {
        Some(DeferredSessionStartup::Fork {
            parent_session_id,
            parent_cwd,
            new_session_id,
        }) => {
            effects.extend(dispatch_startup_fork_session(
                app,
                parent_session_id,
                parent_cwd,
                new_session_id,
            ));
        }
        Some(DeferredSessionStartup::Load {
            session_id,
            session_cwd,
            chat_kind,
        }) => {
            #[cfg(feature = "local-workspace")]
            {
                app.welcome_history_load_as_build = history_load_as_build;
            }
            if worktree {
                if chat_kind || pending_chat {
                    app.deferred_startup.pending_chat = true;
                }
                effects.extend(dispatch_new_worktree_session(
                    app,
                    Some(session_id),
                    worktree_label,
                    None,
                    None,
                    worktree_ref,
                    None,
                ));
            } else {
                effects.extend(dispatch_load_session(
                    app,
                    session_id,
                    session_cwd,
                    chat_kind,
                ));
            }
        }
        Some(DeferredSessionStartup::NewWithId { session_id }) => {
            if pending_chat {
                app.deferred_startup.pending_chat = true;
            }
            if worktree {
                effects.extend(dispatch_new_worktree_session(
                    app,
                    None,
                    worktree_label,
                    None,
                    None,
                    worktree_ref,
                    Some(session_id),
                ));
            } else {
                effects.extend(dispatch_new_session_with_id(app, session_id));
            }
        }
        Some(DeferredSessionStartup::ForeignResume { tool, native_id }) => {
            effects.extend(dispatch_new_session_inner(app, None));
            effects.extend(dispatch(
                Action::SendPrompt(
                    crate::app::foreign_sessions::ForeignPickerSource::from_tool(tool)
                        .resume_prompt(&native_id),
                ),
                app,
            ));
        }
        None => {
            if pending_chat {
                app.deferred_startup.pending_chat = true;
            }
            if let Some(sid) = preferred_id {
                if worktree {
                    effects.extend(dispatch_new_worktree_session(
                        app,
                        None,
                        worktree_label,
                        None,
                        None,
                        worktree_ref,
                        Some(sid),
                    ));
                } else {
                    effects.extend(dispatch_new_session_with_id(app, sid));
                }
            } else if worktree {
                effects.extend(dispatch_new_worktree_session(
                    app,
                    None,
                    worktree_label,
                    None,
                    None,
                    worktree_ref,
                    None,
                ));
            } else if new_session {
                effects.extend(dispatch_new_session(app));
            } else {
                app.deferred_startup.pending_chat = false;
            }
        }
    }
    if let Some(prompt) = prompt {
        effects.extend(dispatch_initial_prompt(app, prompt));
    }
    if open_dashboard {
        effects.extend(dispatch(Action::OpenDashboard, app));
    }
    effects
}
/// Create a placeholder agent in a new git worktree, then create a session.
pub(in crate::app::dispatch) fn dispatch_new_worktree_session(
    app: &mut AppView,
    load_session_id: Option<String>,
    label: Option<String>,
    prompt: Option<String>,
    model_id: Option<acp::ModelId>,
    git_ref: Option<String>,
    preferred_session_id: Option<String>,
) -> Vec<Effect> {
    #[cfg(feature = "local-workspace")]
    if load_session_id.is_none()
        && matches!(app.active_view, crate::app::app_view::ActiveView::Welcome)
    {
        let skip_apply = app.welcome_session_local_workspace.is_some();
        if !skip_apply && let Err(effects) = apply_welcome_workspace_on_new_session(app) {
            app.deferred_startup.worktree = true;
            if let Some(ref label) = label {
                app.deferred_startup.worktree_label = Some(label.clone());
            }
            if let Some(ref git_ref) = git_ref {
                app.deferred_startup.worktree_ref = Some(git_ref.clone());
            }
            if let Some(sid) = load_session_id.clone() {
                app.deferred_startup.session =
                    Some(crate::app::session_startup::DeferredSessionStartup::Load {
                        session_id: sid,
                        session_cwd: None,
                        chat_kind: app.deferred_startup.pending_chat,
                    });
            }
            if let Some(id) = preferred_session_id.clone() {
                app.deferred_startup.preferred_session_id = Some(id);
            }
            return effects;
        }
    }
    let preferred_session_id =
        preferred_session_id.or_else(|| app.deferred_startup.preferred_session_id.take());
    if !app.session_startup_allowed() {
        debug_assert!(
            prompt.is_none() && model_id.is_none(),
            "worktree prompt/model_id only flow from an active session (gate open)"
        );
        if let Some(sid) = load_session_id {
            app.deferred_startup.session =
                Some(crate::app::session_startup::DeferredSessionStartup::Load {
                    session_id: sid,
                    session_cwd: None,
                    chat_kind: app.deferred_startup.pending_chat,
                });
        }
        app.deferred_startup.worktree = true;
        if label.is_some() {
            app.deferred_startup.worktree_label = label;
        }
        if git_ref.is_some() {
            app.deferred_startup.worktree_ref = git_ref;
        }
        if preferred_session_id.is_some() {
            app.deferred_startup.preferred_session_id = preferred_session_id;
        }
        return vec![];
    }
    if !app.cwd_has_git_ancestor {
        let msg: String = "Not inside a git repository. Navigate to a git repo \
                      or run 'git init' first."
            .into();
        if !app.startup_warnings.iter().any(|w| w.message == msg) {
            app.startup_warnings.push(crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: msg,
                action: None,
            });
        }
        #[cfg(feature = "local-workspace")]
        {
            app.welcome_session_local_workspace = None;
            app.welcome_history_load_as_build = false;
        }
        return vec![];
    }
    if load_session_id.is_none() {
        reseed_tip_for_new_session(app);
    }
    let agent_id = AgentId(app.next_agent_id);
    app.next_agent_id += 1;
    let mut scrollback = ScrollbackState::new();
    scrollback.set_appearance(app.appearance.clone());
    let mut agent = AgentView::new(
        AgentSession {
            id: agent_id,
            acp_tx: app.acp_tx.clone(),
            session_id: None,
            models: app.models.clone(),
            state: AgentState::Idle,
            tracker: AcpUpdateTracker::new(),
            cwd: app.cwd.clone(),
            is_worktree: false,
            forked_from: None,
            pending_prompts: std::collections::VecDeque::new(),
            next_queue_id: 0,
            yolo_mode: app.default_yolo,
            auto_mode: inherit_auto_mode(app),
            prompt_history: Vec::new(),
            prompt_history_loading: false,
            loading_replay: false,
            restore_degree: None,
            rate_limited: false,
            model_incompatible: false,
            credit_limit_blocked: false,
            free_usage_blocked: false,
            available_commands: app.bootstrap_acp_commands.clone(),
            available_commands_generation: 1,
            available_tools: None,
            model_switch_pending: false,
            model_switch_request_id: None,
            model_switch_rollback: None,
            model_switch_queue_handoff_from: None,
            user_model_preference: None,
            deferred_model_switch: app.deferred_model_switch_from_cli(),
            bg_tasks: std::collections::BTreeMap::new(),
            bg_tool_call_to_task: std::collections::HashMap::new(),
            scheduled_tasks: std::collections::HashMap::new(),
            in_flight_prompt: None,
            compact_held_prompt: None,
            current_prompt_id: None,
            created_via_new: false,
        },
        scrollback,
    );
    let cmd = if load_session_id.is_some() {
        AgentCommand::RestoreWorktree
    } else {
        AgentCommand::CreateWorktree
    };
    agent.session.start_command(cmd);
    agent.turn_started_at = Some(Instant::now());
    app.agents.insert(agent_id, agent);
    let chat_kind = if load_session_id.is_none() {
        consume_chat_kind(app)
    } else {
        app.deferred_startup.pending_chat
    };
    {
        let agent = app.agents.get_mut(&agent_id).unwrap();
        agent.prompt.set_compact(app.appearance.prompt.compact);
        agent.prompt.adopt_slash_mru(app.slash_mru.clone());
        agent.prompt.adopt_command_tags(app.command_tags.clone());
        agent
            .prompt
            .set_contextual_hints(app.contextual_hints.undo, app.contextual_hints.plan_mode);
        agent.set_session_recap_available(app.session_recap_available);
        agent.set_voice_mode_available(app.voice_mode_enabled);
        agent.apply_app_scoped_gates(
            app.sharing_enabled,
            app.usage_visible,
            !app.has_external_auth_provider,
            app.chat_mode,
            app.screen_mode,
            &app.active_announcements,
            &app.tier_restricted_commands,
        );
        agent.chat_kind = chat_kind;
        #[cfg(feature = "local-workspace")]
        {
            let local_intent = match &app.welcome_session_local_workspace {
                Some(Some(_)) => true,
                Some(None) => false,
                None => crate::app::session_startup::active_local_workspace()
                    .ok()
                    .flatten()
                    .is_some(),
            };
            let (mode, locked) =
                crate::views::welcome::workspace_mode::indicator_for_opening_session(
                    agent.chat_kind,
                    app.welcome_history_load_as_build,
                    app.local_workspace_startup_locked,
                    local_intent,
                );
            agent.workspace_mode = mode;
            agent.workspace_mode_cli_locked = locked;
        }
        agent.apply_credit_balance(app.credit_balance.clone(), app.auto_topup.clone());
        agent
            .prompt
            .slash_controller
            .registry_mut()
            .set_plugins_visible(!app.appearance.disable_plugins);
    }
    if let Some(prompt) = prompt
        && let Some(agent) = app.agents.get_mut(&agent_id)
    {
        agent.session.enqueue_prompt(prompt);
    }
    switch_to_agent(app, agent_id, SwitchCause::New);
    let effects = vec![Effect::CreateWorktreeSession {
        agent_id,
        load_session_id,
        label,
        git_ref,
        model_id,
        preferred_session_id,
        chat_kind,
    }];
    effects
}
pub(in crate::app::dispatch) fn dispatch_new_session_with_id(
    app: &mut AppView,
    session_id: String,
) -> Vec<Effect> {
    app.deferred_startup.preferred_session_id = Some(session_id.clone());
    if !app.session_startup_allowed() {
        app.deferred_startup.session =
            Some(crate::app::session_startup::DeferredSessionStartup::NewWithId { session_id });
        return vec![];
    }
    let (agent_id, mut effects) = dispatch_new_session_inner_with_id(app, None);
    for e in &mut effects {
        if let Effect::CreateSession {
            preferred_session_id,
            ..
        } = e
        {
            *preferred_session_id = Some(session_id.clone());
        }
    }
    let _ = agent_id;
    effects
}
/// Tear down a placeholder agent that must not proceed under sticky `--chat`
/// (local Build refuse). Never leave a half-loaded slot with a bound session id.
pub(in crate::app::dispatch) fn refuse_chat_mode_build_agent(
    app: &mut AppView,
    agent_id: AgentId,
) -> Vec<Effect> {
    app.show_toast(crate::app::session_startup::CHAT_MODE_LOCAL_BUILD_REFUSAL);
    let fallback = app.agents.keys().copied().find(|id| *id != agent_id);
    let effects = remove_agent_and_cleanup(app, agent_id);
    if let Some(target) = fallback {
        switch_to_agent(app, target, SwitchCause::Picker);
    } else {
        show_welcome(app);
        app.welcome_prompt_focused = true;
        app.session_picker_entries = None;
        app.session_picker_loading = false;
        app.session_picker_state.selected = 0;
        app.session_picker_content_results = None;
        app.session_picker_content_loading = false;
        let msg = crate::app::session_startup::CHAT_MODE_LOCAL_BUILD_REFUSAL.to_string();
        if !app.startup_warnings.iter().any(|w| w.message == msg) {
            app.startup_warnings.push(crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: msg,
                action: None,
            });
        }
    }
    effects
}
/// Dismiss the project picker and create a session in the current directory.
pub(in crate::app::dispatch) fn skip_picker_and_create_session(
    app: &mut AppView,
    agent_id: AgentId,
) -> Vec<Effect> {
    if app
        .agents
        .get(&agent_id)
        .is_some_and(|a| a.session.session_id.is_some() || a.mcp_init_progress.is_some())
    {
        return vec![];
    }
    app.mark_project_picker_done();
    let chat_kind = consume_chat_kind(app);
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.chat_kind = chat_kind;
        #[cfg(feature = "local-workspace")]
        {
            let local_intent = match &app.welcome_session_local_workspace {
                Some(Some(_)) => true,
                Some(None) => false,
                None => crate::app::session_startup::active_local_workspace()
                    .ok()
                    .flatten()
                    .is_some(),
            };
            let (mode, locked) =
                crate::views::welcome::workspace_mode::indicator_for_opening_session(
                    agent.chat_kind,
                    false,
                    app.local_workspace_startup_locked,
                    local_intent,
                );
            agent.workspace_mode = mode;
            agent.workspace_mode_cli_locked = locked;
        }
        agent.apply_credit_balance(app.credit_balance.clone(), app.auto_topup.clone());
        agent.mcp_init_progress = Some(McpInitProgress {
            total: 0,
            connected: 0,
            started_at: Instant::now(),
        });
        agent.session.prompt_history_loading = true;
        if let Some(qv) = agent.question_view.take() {
            agent.prompt.restore(qv.stashed_prompt);
        }
    }
    let preferred_session_id = app.deferred_startup.preferred_session_id.take();
    vec![Effect::CreateSession {
        agent_id,
        cwd: app.cwd.clone(),
        model_id: None,
        preferred_session_id,
        chat_kind,
    }]
}
pub(in crate::app::dispatch) fn handle_session_created(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: acp::SessionId,
    new_models: Option<acp::SessionModelState>,
    scheduler_background_loops: Option<bool>,
) -> Vec<Effect> {
    let agent_count = app.agents.len();
    let app_chat_mode = app.chat_mode;
    let switch_hint =
        crate::views::dashboard::session_switch_hint_command(app.screen_mode.is_minimal());
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        let session_id_clone = session_id.clone();
        if agent.session.created_via_new
            && agent_count > 1
            && let Some(cmd) = switch_hint
        {
            agent.scrollback.push_block(RenderBlock::system(format!(
                "Session {} \u{2014} use {cmd} to switch between sessions",
                session_id_clone.0,
            )));
        } else if agent_count > 1 {
            agent.scrollback.push_block(RenderBlock::system(format!(
                "Session: {}",
                session_id_clone.0,
            )));
        }
        agent.bind_session_id(session_id);
        agent.scheduler_background_loops = scheduler_background_loops;
        if let Some(m) = new_models {
            app.models = Some(m).into();
            agent.session.models = app.models.clone();
        }
        let handoff_requires_deferred_switch =
            agent.session.model_switch_queue_handoff_from.is_some()
                && agent.session.deferred_model_switch.is_some();
        // An incompatible default-model switch can be adopted by the model
        // chosen in CreateSession itself. Preserve that confirmed default
        // intent before `apply_deferred_model_switch` clears the rollback;
        // unlike an effort override, this path must not issue a redundant ACP
        // switch merely to obtain a persistence receipt.
        let replacement_default = (!app_chat_mode
            && !agent.chat_kind
            && !agent.app_chat_mode
            && agent.session.deferred_model_switch.is_none()
            && app
                .model_switch_transaction
                .as_ref()
                .is_some_and(|transaction| {
                    transaction.owner_agent_id == agent_id && transaction.app_models_optimistic
                }))
        .then(|| {
            agent
                .session
                .models
                .current
                .clone()
                .map(|model_id| (model_id, agent.session.models.reasoning_effort))
        })
        .flatten();
        let deferred = apply_deferred_model_switch(agent, app.cli_effort_token.as_deref());
        let deferred_mode = agent.deferred_session_mode.take();
        let cwd = agent.session.cwd.clone();
        let deferred_request_id = deferred.as_ref().and_then(|_| {
            begin_model_switch_request(
                &mut app.model_switch_transaction,
                agent_id,
                &mut agent.session,
                &app.models,
            )
        });
        let failed_handoff = (handoff_requires_deferred_switch && deferred_request_id.is_none())
            .then(|| {
                let source_id = agent
                    .session
                    .model_switch_queue_handoff_from
                    .take()
                    .expect("handoff gate checked above");
                let prompts = std::mem::take(&mut agent.session.pending_prompts);
                agent.session.model_switch_pending = false;
                agent.session.model_switch_request_id = None;
                agent.session.model_switch_rollback = None;
                if let Some(transaction) = app.model_switch_transaction.take()
                    && transaction.app_models_optimistic
                {
                    app.models.current = transaction.app_model_id;
                    app.models.reasoning_effort = transaction.app_reasoning_effort;
                }
                (source_id, prompts)
            });
        // A handoff without a deferred effort switch is fully adopted by the
        // newly-created target session. Otherwise retain the source marker
        // until the target switch commits, so a failed preflight or session
        // close can still restore the queue instead of draining it under an
        // unverified model configuration.
        if !handoff_requires_deferred_switch || failed_handoff.is_some() {
            agent.session.model_switch_queue_handoff_from = None;
        }
        let mut drain = if app.reconnect_pending {
            QueueDrain {
                effects: vec![],
                page_flip_entry: None,
            }
        } else {
            maybe_drain_queue(agent)
        };
        let mut effects = std::mem::take(&mut drain.effects);
        if let Some((model_id, reasoning_effort)) = replacement_default {
            agent.session.user_model_preference = Some(model_id.clone());
            if app.models.available.contains_key(&model_id) {
                app.models.set_current(model_id.clone(), reasoning_effort);
            }
            if app
                .model_switch_transaction
                .as_ref()
                .is_some_and(|transaction| transaction.owner_agent_id == agent_id)
            {
                app.model_switch_transaction = None;
            }
            effects.push(Effect::PersistPreferredModel {
                model_id,
                reasoning_effort,
            });
        }
        agent.session.prompt_history_loading = true;
        effects.push(Effect::FetchPromptHistory {
            agent_id,
            cwd: cwd.clone(),
            session_id: session_id_clone.to_string(),
        });
        effects.push(Effect::FetchSessionAgentName {
            agent_id,
            session_id: session_id_clone.clone(),
        });
        effects.push(Effect::RefreshAvailableCommands {
            agent_id,
            session_id: session_id_clone.clone(),
        });
        effects.push(Effect::CheckMarketplaceUpdates {
            agent_id,
            session_id: session_id_clone.clone(),
        });
        if app.plugin_cta_enabled {
            effects.push(Effect::FetchPluginCtaCatalog {
                agent_id,
                session_id: session_id_clone.clone(),
            });
        }
        effects.push(Effect::FetchBilling {
            agent_id,
            silent: true,
        });
        if let Some((model_id, effort)) = deferred
            && let Some(request_id) = deferred_request_id
        {
            effects.push(Effect::SwitchModel {
                agent_id,
                session_id: session_id_clone.clone(),
                model_id,
                effort,
                request_id,
                prev_model_id: None,
            });
        }
        if let Some(mode) = deferred_mode {
            effects.push(Effect::SetSessionMode {
                session_id: session_id_clone.clone(),
                mode_id: acp::SessionModeId::new(mode.as_id()),
            });
        }
        if std::mem::take(&mut agent.pending_extensions_fetch)
            && let Some(modal) = agent.extensions_modal.as_mut()
        {
            effects.extend(extensions_modal_tab_fetches(
                modal,
                agent_id,
                session_id_clone.clone(),
            ));
        }
        effects.push(Effect::RegisterActiveSession {
            session_id: session_id_clone,
            cwd: agent.session.cwd.display().to_string(),
        });
        notify_session_ready(&app.notification_service, agent);
        note_peek_page_flip(app, agent_id, drain.page_flip_entry);
        if let Some((source_id, prompts)) = failed_handoff
            && let Some(source) = app.agents.get_mut(&source_id)
        {
            source.session.prepend_pending_prompts(prompts);
            source.session.model_switch_pending = false;
            source.session.model_switch_request_id = None;
            let source_drain = maybe_drain_queue(source);
            effects.extend(source_drain.effects);
            note_peek_page_flip(app, source_id, source_drain.page_flip_entry);
        }
        return effects;
    }
    vec![]
}
pub(in crate::app::dispatch) fn handle_worktree_session_created(
    app: &mut AppView,
    agent_id: AgentId,
    session_id: acp::SessionId,
    worktree_path: std::path::PathBuf,
    session_cwd: std::path::PathBuf,
    new_models: Option<acp::SessionModelState>,
    scheduler_background_loops: Option<bool>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.session.finish_command();
        agent.mark_turn_finished();
        let session_id_clone = session_id.clone();
        agent.bind_session_id(session_id);
        agent.scheduler_background_loops = scheduler_background_loops;
        agent.session.cwd = session_cwd.clone();
        agent.session.is_worktree = true;
        if let Some(m) = new_models {
            app.models = Some(m).into();
            agent.session.models = app.models.clone();
        }
        agent.prompt.file_search.retarget(&session_cwd);
        agent.scrollback.push_block(RenderBlock::system(format!(
            "Worktree ready: {}",
            worktree_path.display()
        )));
        let deferred = apply_deferred_model_switch(agent, app.cli_effort_token.as_deref());
        let deferred_mode = agent.deferred_session_mode.take();
        let cwd = agent.session.cwd.clone();
        let deferred_request_id = deferred.as_ref().and_then(|_| {
            begin_model_switch_request(
                &mut app.model_switch_transaction,
                agent_id,
                &mut agent.session,
                &app.models,
            )
        });
        let mut drain = if app.reconnect_pending {
            QueueDrain {
                effects: vec![],
                page_flip_entry: None,
            }
        } else {
            maybe_drain_queue(agent)
        };
        let mut effects = std::mem::take(&mut drain.effects);
        agent.session.prompt_history_loading = true;
        effects.push(Effect::FetchPromptHistory {
            agent_id,
            cwd: cwd.clone(),
            session_id: session_id_clone.to_string(),
        });
        effects.push(Effect::FetchSessionAgentName {
            agent_id,
            session_id: session_id_clone.clone(),
        });
        effects.push(Effect::RefreshAvailableCommands {
            agent_id,
            session_id: session_id_clone.clone(),
        });
        effects.push(Effect::CheckMarketplaceUpdates {
            agent_id,
            session_id: session_id_clone.clone(),
        });
        if app.plugin_cta_enabled {
            effects.push(Effect::FetchPluginCtaCatalog {
                agent_id,
                session_id: session_id_clone.clone(),
            });
        }
        effects.push(Effect::FetchBilling {
            agent_id,
            silent: true,
        });
        if let Some((model_id, effort)) = deferred
            && let Some(request_id) = deferred_request_id
        {
            effects.push(Effect::SwitchModel {
                agent_id,
                session_id: session_id_clone.clone(),
                model_id,
                effort,
                request_id,
                prev_model_id: None,
            });
        }
        if let Some(mode) = deferred_mode {
            effects.push(Effect::SetSessionMode {
                session_id: session_id_clone.clone(),
                mode_id: acp::SessionModeId::new(mode.as_id()),
            });
        }
        if std::mem::take(&mut agent.pending_extensions_fetch)
            && let Some(modal) = agent.extensions_modal.as_mut()
        {
            effects.extend(extensions_modal_tab_fetches(
                modal,
                agent_id,
                session_id_clone.clone(),
            ));
        }
        effects.push(Effect::RegisterActiveSession {
            session_id: session_id_clone,
            cwd: agent.session.cwd.display().to_string(),
        });
        notify_session_ready(&app.notification_service, agent);
        note_peek_page_flip(app, agent_id, drain.page_flip_entry);
        return effects;
    }
    vec![]
}
/// Surface a session-creation failure on the welcome screen (no toast sink).
fn push_session_create_failure_warning(app: &mut AppView, msg: &str) {
    if !app.startup_warnings.iter().any(|w| w.message == msg) {
        app.startup_warnings.push(crate::startup::StartupWarning {
            severity: crate::startup::WarningSeverity::Warning,
            message: msg.to_string(),
            action: None,
        });
    }
}
/// Failed plain `CreateSession`: drop orphan placeholders, clear the
/// starting-session spinner, and surface the error (toast when an agent
/// remains; startup warning on the welcome screen, which has no toast).
pub(in crate::app::dispatch) fn handle_session_failed(
    app: &mut AppView,
    agent_id: AgentId,
    error: String,
) -> Vec<Effect> {
    tracing::error!(agent = ?agent_id, error = %error, "Session creation failed");
    let msg = format!("Session creation failed: {error}");
    let is_orphan = app
        .agents
        .get(&agent_id)
        .is_some_and(|a| a.session.session_id.is_none() && a.session.forked_from.is_none());
    let mut effects = vec![];
    if is_orphan {
        let failed_was_active = matches!(app.active_view, ActiveView::Agent(id) if id == agent_id);
        let fallback = app.agents.keys().copied().find(|id| *id != agent_id);
        effects = remove_agent_and_cleanup(app, agent_id);
        if let Some(target) = fallback {
            if failed_was_active {
                switch_to_agent(app, target, SwitchCause::Picker);
            }
            if matches!(app.active_view, ActiveView::Welcome) {
                push_session_create_failure_warning(app, &msg);
            } else {
                app.show_toast(&msg);
            }
        } else {
            show_welcome(app);
            app.welcome_prompt_focused = true;
            app.session_picker_entries = None;
            app.session_picker_loading = false;
            app.session_picker_state.selected = 0;
            app.session_picker_content_results = None;
            app.session_picker_content_loading = false;
            push_session_create_failure_warning(app, &msg);
        }
    } else if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.pending_extensions_fetch = false;
        agent.session.prompt_history_loading = false;
        agent.mcp_init_progress = None;
        agent.session.finish_command();
        let elapsed = agent.turn_elapsed();
        agent.mark_turn_finished();
        agent.pending_first_prompt = None;
        agent.pending_fork_banner = None;
        agent.show_toast(&msg);
        agent
            .scrollback
            .push_block(RenderBlock::session_event(SessionEvent::TurnFailed {
                error,
                elapsed,
            }));
    }
    effects
}
pub(in crate::app::dispatch) fn handle_worktree_session_failed(
    app: &mut AppView,
    agent_id: AgentId,
    error: String,
) -> Vec<Effect> {
    tracing::error!(agent = ?agent_id, error = %error, "Worktree session creation failed");
    let is_orphan = app
        .agents
        .get(&agent_id)
        .is_some_and(|a| a.session.session_id.is_none() && a.session.forked_from.is_none());
    let mut effects = vec![];
    if is_orphan {
        let fallback = app.agents.keys().copied().find(|id| *id != agent_id);
        effects = remove_agent_and_cleanup(app, agent_id);
        if let Some(target) = fallback {
            switch_to_agent(app, target, SwitchCause::Picker);
        } else {
            show_welcome(app);
            app.welcome_prompt_focused = true;
            app.session_picker_entries = None;
            app.session_picker_loading = false;
            app.session_picker_state.selected = 0;
            app.session_picker_content_results = None;
            app.session_picker_content_loading = false;
        }
        let msg = format!("Cannot create worktree: {error}");
        if !app.startup_warnings.iter().any(|w| w.message == msg) {
            app.startup_warnings.push(crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: msg,
                action: None,
            });
        }
    } else if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.pending_extensions_fetch = false;
        agent.session.prompt_history_loading = false;
        agent.mcp_init_progress = None;
        agent.session.finish_command();
        let elapsed = agent.turn_elapsed();
        agent.mark_turn_finished();
        agent.pending_first_prompt = None;
        agent.pending_fork_banner = None;
        agent
            .scrollback
            .push_block(RenderBlock::session_event(SessionEvent::TurnFailed {
                error,
                elapsed,
            }));
    }
    effects
}
fn restore_model_switch_mirrors(
    agent: &mut AgentView,
    app_models: &mut ModelState,
    rollback: Option<&ModelSwitchRollback>,
    transaction: &ModelSwitchTransaction,
    fallback_model_id: Option<&acp::ModelId>,
) {
    if let Some(rollback) = rollback {
        agent.session.models.current = rollback.session_model_id.clone();
        agent.session.models.reasoning_effort = rollback.session_reasoning_effort;
    } else if let Some(previous) = fallback_model_id {
        agent.session.models.set_current(previous.clone(), None);
    }
    if transaction.app_models_optimistic {
        app_models.current = transaction.app_model_id.clone();
        app_models.reasoning_effort = transaction.app_reasoning_effort;
    }
}

pub(in crate::app::dispatch) fn handle_switch_model_complete(
    app: &mut AppView,
    agent_id: AgentId,
    model_id: acp::ModelId,
    effort: Option<ReasoningEffort>,
    request_id: u64,
    result: Result<(), SwitchModelError>,
    prev_model_id: Option<acp::ModelId>,
) -> Vec<Effect> {
    let Some(transaction) = app
        .model_switch_transaction
        .as_ref()
        .filter(|transaction| {
            transaction.owner_agent_id == agent_id && transaction.request_id == Some(request_id)
        })
        .cloned()
    else {
        tracing::debug!(
            agent = ?agent_id,
            request_id,
            "ignoring completion outside the active app model-switch transaction"
        );
        return vec![];
    };
    let app_chat_mode = app.chat_mode;
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        let persist_preferred_model = !app_chat_mode && !agent.chat_kind && !agent.app_chat_mode;
        if !agent.session.model_switch_pending {
            tracing::debug!(
                agent = ?agent_id,
                request_id,
                "ignoring model-switch completion after its transaction closed"
            );
            return vec![];
        }
        if agent.session.model_switch_request_id != Some(request_id) {
            tracing::debug!(
                agent = ?agent_id,
                request_id,
                expected = ?agent.session.model_switch_request_id,
                "ignoring stale model-switch completion"
            );
            return vec![];
        }

        agent.session.model_switch_pending = false;
        agent.session.model_switch_request_id = None;
        let rollback = agent.session.model_switch_rollback.take();
        let mut effects = match result {
            Ok(()) => {
                agent.session.model_switch_queue_handoff_from = None;
                agent.session.user_model_preference = Some(model_id.clone());
                let display_name = agent
                    .session
                    .models
                    .available
                    .get(&model_id)
                    .map(|info| info.name.clone())
                    .unwrap_or_else(|| model_id.0.to_string());
                let auth_class = agent
                    .session
                    .models
                    .available
                    .get(&model_id)
                    .map(|info| {
                        crate::slash::commands::model::auth_class_from_model_meta(
                            info.meta.as_ref(),
                        )
                    })
                    .unwrap_or_default();
                let live_prev_model = agent.session.models.current.clone();
                let live_prev_effort = agent.session.models.reasoning_effort;
                agent.session.models.set_current(model_id.clone(), effort);
                if app.models.available.contains_key(&model_id) {
                    app.models.set_current(model_id.clone(), effort);
                }
                let resolved_effort = agent.session.models.reasoning_effort;
                // A default-model selection updates both pager mirrors before the ACP
                // request starts. Prefer its receipt/snapshot over that optimistic
                // live value when deciding whether to persist and notify.
                let model_changed = prev_model_id.as_ref().map_or_else(
                    || {
                        rollback.as_ref().map_or_else(
                            || live_prev_model.as_ref() != Some(&model_id),
                            |snapshot| snapshot.session_model_id.as_ref() != Some(&model_id),
                        )
                    },
                    |previous| previous != &model_id,
                );
                let previous_effort = rollback.as_ref().map_or(live_prev_effort, |snapshot| {
                    snapshot.session_reasoning_effort
                });
                let unchanged = !model_changed && previous_effort == resolved_effort;
                if !unchanged {
                    let msg = if let Some(eff) = resolved_effort {
                        format!("Switched to {display_name} ({eff} effort)")
                    } else {
                        format!("Switched to {display_name}")
                    };
                    agent.scrollback.push_block(RenderBlock::system(msg));
                    let toast = match auth_class.as_str() {
                        "none" => format!(
                            "Switched to {display_name} (auth_scheme=none; session credentials not sent)"
                        ),
                        "session" => format!("Switched to {display_name} (xAI session)"),
                        _ => format!("Switched to {display_name}"),
                    };
                    agent.show_toast(&toast);
                }
                if unchanged || !persist_preferred_model {
                    vec![]
                } else {
                    vec![Effect::PersistPreferredModel {
                        model_id: model_id.clone(),
                        reasoning_effort: resolved_effort,
                    }]
                }
            }
            Err(SwitchModelError::IncompatibleAgent {
                prev_model_id: error_prev,
                ..
            }) => {
                let fallback = error_prev.as_ref().or(prev_model_id.as_ref());
                restore_model_switch_mirrors(
                    agent,
                    &mut app.models,
                    rollback.as_ref(),
                    &transaction,
                    fallback,
                );
                // The decision modal is still part of this transaction. Keep
                // the queue gated until the user either declines (drain here)
                // or accepts (handoff to the replacement placeholder).
                agent.session.model_switch_pending = true;
                agent.session.model_switch_rollback = rollback.clone();
                if let Some(active) = app.model_switch_transaction.as_mut() {
                    active.request_id = None;
                }
                agent.active_modal = None;
                let display_name = agent.session.models.display_name_for(&model_id);
                return open_agent_type_mismatch_question(
                    app,
                    agent_id,
                    model_id,
                    effort,
                    &display_name,
                );
            }
            Err(SwitchModelError::HarnessUnavailable {
                error,
                prev_model_id: error_prev,
            }) => {
                let fallback = error_prev.as_ref().or(prev_model_id.as_ref());
                let message = format!(
                    "Couldn't switch to model '{}': required harness '{}' is unavailable ({}).",
                    error.model_id, error.required_agent_type, error.reason,
                );
                agent.scrollback.push_block(RenderBlock::system(message));
                restore_model_switch_mirrors(
                    agent,
                    &mut app.models,
                    rollback.as_ref(),
                    &transaction,
                    fallback,
                );
                vec![]
            }
            Err(SwitchModelError::Other(msg)) => {
                restore_model_switch_mirrors(
                    agent,
                    &mut app.models,
                    rollback.as_ref(),
                    &transaction,
                    prev_model_id.as_ref(),
                );
                agent
                    .scrollback
                    .push_block(RenderBlock::system(format!("Couldn't switch model: {msg}")));
                vec![]
            }
        };
        app.model_switch_transaction = None;
        let drain = maybe_drain_queue(agent);
        effects.extend(drain.effects);
        note_peek_page_flip(app, agent_id, drain.page_flip_entry);
        effects
    } else {
        vec![]
    }
}
pub(in crate::app::dispatch) fn dispatch_agent_type_mismatch_answered(
    app: &mut AppView,
    source_id: AgentId,
    start_new: bool,
    model_id: acp::ModelId,
    effort: Option<ReasoningEffort>,
) -> Vec<Effect> {
    let Some(source) = app.agents.get_mut(&source_id) else {
        return vec![];
    };
    let carried_rollback = source.session.model_switch_rollback.take();
    source.session.model_switch_pending = false;
    source.session.model_switch_request_id = None;

    if start_new {
        let handed_off = std::mem::take(&mut source.session.pending_prompts);
        let (new_aid, effects) = dispatch_new_session_inner_with_id(app, Some(model_id.clone()));
        handoff_model_switch_transaction(app, source_id, new_aid);
        if let Some(agent) = app.agents.get_mut(&new_aid) {
            agent.session.model_switch_rollback = carried_rollback.map(|mut rollback| {
                rollback.request_id = None;
                rollback
            });
            if !handed_off.is_empty() {
                agent.session.replace_pending_prompts(handed_off);
                agent.session.model_switch_queue_handoff_from = Some(source_id);
            }
            agent.session.models.set_current(model_id.clone(), effort);
            if effort.is_some() {
                agent.session.deferred_model_switch = Some((model_id, effort));
            }
        }
        effects
    } else {
        abort_model_switch_transaction(app, source_id);
        let agent = app
            .agents
            .get_mut(&source_id)
            .expect("source agent checked above");
        let drain = maybe_drain_queue(agent);
        note_peek_page_flip(app, source_id, drain.page_flip_entry);
        drain.effects
    }
}

pub(in crate::app::dispatch) fn dispatch_auth_class_switch_answered(
    app: &mut AppView,
    proceed: bool,
    model_id: acp::ModelId,
    effort: Option<ReasoningEffort>,
    persist_default: bool,
) -> Vec<Effect> {
    if !proceed {
        return vec![];
    }
    if let ActiveView::Agent(id) = app.active_view
        && let Some(reason) = app.agents.get(&id).and_then(|agent| {
            crate::slash::commands::model::model_not_ready_reason(&agent.session.models, &model_id)
        })
    {
        app.show_toast(&reason);
        return vec![];
    }
    if persist_default && effort.is_none() {
        return crate::app::dispatch::settings::setters::set_default_model_confirmed(app, model_id);
    }
    // Session-scoped switch (effort path or explicit non-persist).
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        agent.session.deferred_model_switch = Some((model_id, effort));
        return vec![];
    };
    let Some(request_id) = begin_model_switch_request(
        &mut app.model_switch_transaction,
        id,
        &mut agent.session,
        &app.models,
    ) else {
        app.show_toast("Wait for the current model switch to finish");
        return vec![];
    };
    vec![Effect::SwitchModel {
        agent_id: id,
        session_id,
        model_id,
        effort,
        request_id,
        prev_model_id: None,
    }]
}
