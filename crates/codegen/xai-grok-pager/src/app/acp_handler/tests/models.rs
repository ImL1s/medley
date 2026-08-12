#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    /// Regression: a machine-wide `x.ai/models/update` broadcast
    /// carries each model's static catalog-default effort (`high`), not the
    /// session's chosen `xhigh`, and must not clobber the per-session choice.
    #[test]
    fn models_update_preserves_user_reasoning_effort() {
        use xai_grok_shell::sampling::types::ReasoningEffort;
        let mut app = make_app_with_agent("sess-1");

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        let id = acp::ModelId::new(std::sync::Arc::from("reason-model"));
        let mut info = make_model_info("reason-model");
        info.meta = serde_json::json!({
            "supportsReasoningEffort": true,
            "reasoningEffort": "high",
        })
        .as_object()
        .cloned();
        agent.session.models.available.insert(id.clone(), info);
        agent
            .session
            .models
            .set_current(id, Some(ReasoningEffort::Xhigh));
        assert_eq!(
            agent.session.models.reasoning_effort,
            Some(ReasoningEffort::Xhigh)
        );

        let notif = make_reasoning_models_update_notif("reason-model", "high");
        assert!(handle_models_update(&notif, &mut app));

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.session.models.reasoning_effort,
            Some(ReasoningEffort::Xhigh),
            "models/update broadcast must not clobber a user-set per-session effort"
        );
    }

    #[test]
    fn models_update_preserves_active_agent_model() {
        let mut app = make_app_with_agent("sess-1");

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        let id_3 = acp::ModelId::new(std::sync::Arc::from("grok-3"));
        agent
            .session
            .models
            .available
            .insert(id_3.clone(), make_model_info("grok-3"));
        agent.session.models.current = Some(id_3.clone());

        let notif = make_models_update_notif("grok-4", &["grok-3", "grok-4"]);
        handle_models_update(&notif, &mut app);

        assert_eq!(
            app.models.current.as_ref().map(|id| id.0.as_ref()),
            Some("grok-3"),
            "app.models.current must preserve active agent's model, not remote settings default"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-3"),
            "agent's per-session model must be preserved"
        );
    }

    #[test]
    fn models_update_preserves_unavailable_resident_as_display_only() {
        let mut app = make_app_with_agent("sess-1");
        let resident_id = acp::ModelId::new(std::sync::Arc::from("retired"));
        let resident_info = acp::ModelInfo::new(
            resident_id.clone(),
            "Retired Model (unavailable)".to_string(),
        )
        .meta(
            serde_json::json!({ "unavailableResidentModel": true })
                .as_object()
                .cloned(),
        );
        {
            let models = &mut app
                .agents
                .get_mut(&AgentId(0))
                .expect("active agent")
                .session
                .models;
            models
                .available
                .insert(resident_id.clone(), resident_info.clone());
            models.current = Some(resident_id.clone());
        }

        let notif = make_models_update_notif("ready", &["ready"]);
        assert!(handle_models_update(&notif, &mut app));

        let agent_models = &app.agents[&AgentId(0)].session.models;
        assert_eq!(agent_models.current.as_ref(), Some(&resident_id));
        assert_eq!(agent_models.available.get(&resident_id), Some(&resident_info));
        assert_eq!(
            agent_models.current_model_name().as_deref(),
            Some("Retired Model (unavailable)")
        );
        assert!(agent_models.resolve_by_name_or_id("retired").is_none());
        assert_eq!(
            agent_models.next_model().as_ref().map(|id| id.0.as_ref()),
            Some("ready"),
            "model cycle must leave the placeholder and enter the selectable catalog"
        );
        assert_eq!(
            agent_models
                .selectable_models()
                .map(|(id, _)| id.0.as_ref())
                .collect::<Vec<_>>(),
            vec!["ready"]
        );

        assert_eq!(app.models.current.as_ref(), Some(&resident_id));
        assert_eq!(app.models.available.get(&resident_id), Some(&resident_info));
        let snapshot = crate::app::dispatch::build_pager_snapshot(&app);
        assert_eq!(
            snapshot.current_model_name.as_deref(),
            Some("Retired Model (unavailable)")
        );
        assert_eq!(
            snapshot
                .available_models
                .iter()
                .map(|(_, id)| id.0.as_ref())
                .collect::<Vec<_>>(),
            vec!["ready"],
            "settings picker must exclude the display-only resident"
        );
    }

    #[test]
    fn models_update_refreshes_open_settings_model_rows() {
        use crate::views::modal::ActiveModal;
        use crate::views::settings_modal::SettingsModalState;

        let mut app = make_app_with_agent("sess-1");
        seed_models(
            app.agents.get_mut(&AgentId(0)).expect("active agent"),
            "old",
            &["old", "removed"],
        );
        let registry = app.settings_registry.clone();
        let ui = app.current_ui.clone();
        let snapshot = crate::app::dispatch::build_pager_snapshot(&app);
        app.agents.get_mut(&AgentId(0)).unwrap().active_modal =
            Some(ActiveModal::Settings {
                state: Box::new(SettingsModalState::new(registry, ui, snapshot)),
            });

        assert!(handle_models_update(
            &make_models_update_notif("ready", &["ready"]),
            &mut app,
        ));

        let Some(ActiveModal::Settings { state }) = app
            .agents
            .get(&AgentId(0))
            .and_then(|agent| agent.active_modal.as_ref())
        else {
            panic!("settings modal must remain open");
        };
        let ids = state
            .pager_snapshot
            .available_models
            .iter()
            .map(|(_, id)| id.0.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["ready"]);
    }

    #[test]
    fn models_update_preserves_resident_until_session_confirms_a_switch() {
        let mut app = make_app_with_agent("sess-1");

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        let id_3 = acp::ModelId::new(std::sync::Arc::from("grok-3"));
        agent
            .session
            .models
            .available
            .insert(id_3.clone(), make_model_info("grok-3"));
        agent.session.models.current = Some(id_3.clone());

        // grok-3 removed from catalog.
        let notif = make_models_update_notif("grok-4.3", &["grok-4.3", "grok-4.5"]);
        handle_models_update(&notif, &mut app);

        assert_eq!(
            app.models.current.as_ref().map(|id| id.0.as_ref()),
            Some("grok-3"),
            "a machine-wide catalog update must not invent a per-session switch"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-3"),
            "the resident actor remains authoritative until a session notification switches it"
        );
        assert!(crate::acp::model_state::is_unavailable_resident_model(
            agent
                .session
                .models
                .available
                .get(&id_3)
                .expect("removed resident should remain as a display-only placeholder")
        ));
    }

    #[test]
    fn models_update_without_active_agent_uses_shell_default() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = AppView::new(tx, ModelState::default(), Vec::new());

        let notif = make_models_update_notif("grok-4", &["grok-3", "grok-4"]);
        handle_models_update(&notif, &mut app);

        assert_eq!(
            app.models.current.as_ref().map(|id| id.0.as_ref()),
            Some("grok-4"),
            "without an active agent, shell default must be used"
        );
    }

    #[test]
    fn models_update_noop_when_agent_matches_shell_default() {
        let mut app = make_app_with_agent("sess-1");

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        let id_4 = acp::ModelId::new(std::sync::Arc::from("grok-4"));
        agent
            .session
            .models
            .available
            .insert(id_4.clone(), make_model_info("grok-4"));
        agent.session.models.current = Some(id_4);

        let notif = make_models_update_notif("grok-4", &["grok-3", "grok-4"]);
        handle_models_update(&notif, &mut app);

        assert_eq!(
            app.models.current.as_ref().map(|id| id.0.as_ref()),
            Some("grok-4"),
            "app.models.current must be grok-4 when agent and shell agree"
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-4"),
            "agent model must remain grok-4"
        );
    }

    #[test]
    fn models_update_preserves_each_live_agent_resident_independently() {
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        let id_5 = acp::ModelId::new(std::sync::Arc::from("grok-4.5"));

        {
            let agent_a = app.agents.get_mut(&AgentId(0)).unwrap();
            let id_3 = acp::ModelId::new(std::sync::Arc::from("grok-3"));
            agent_a
                .session
                .models
                .available
                .insert(id_3.clone(), make_model_info("grok-3"));
            agent_a.session.models.current = Some(id_3);
        }

        {
            let agent_b = app.agents.get_mut(&AgentId(1)).unwrap();
            agent_b
                .session
                .models
                .available
                .insert(id_5.clone(), make_model_info("grok-4.5"));
            agent_b.session.models.current = Some(id_5.clone());
        }

        // grok-5 removed from catalog.
        let notif = make_models_update_notif("grok-4", &["grok-3", "grok-4"]);
        handle_models_update(&notif, &mut app);

        assert_eq!(
            app.models.current.as_ref().map(|id| id.0.as_ref()),
            Some("grok-3"),
        );
        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent_a
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-3"),
            "agent A's model must be preserved"
        );

        // B's grok-5 was removed, but the machine-wide notification cannot
        // claim B's resident actor switched to either global/default model.
        let agent_b = app.agents.get(&AgentId(1)).unwrap();
        assert_eq!(
            agent_b
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-4.5"),
            "inactive live sessions must also preserve their resident model"
        );
        assert!(crate::acp::model_state::is_unavailable_resident_model(
            agent_b
                .session
                .models
                .available
                .get(&id_5)
                .expect("removed resident should remain display-only")
        ));
    }

    #[test]
    fn model_auto_switched_confirms_the_new_resident_model() {
        let mut app = make_app_with_agent("sess-1");
        seed_models(
            app.agents.get_mut(&AgentId(0)).expect("active agent"),
            "m-old",
            &["m-old", "m-new"],
        );

        assert!(handle_ext_notification(
            &xai_model_switch_notif("sess-1", "model-switch-1"),
            &mut app,
        ));

        assert_eq!(
            app.agents[&AgentId(0)]
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("m-new")
        );
        assert_eq!(
            app.models.current.as_ref().map(|id| id.0.as_ref()),
            Some("m-new"),
            "the active status model must follow the authoritative session switch"
        );
    }

    #[test]
    fn model_auto_switched_waits_for_catalog_without_display_drift() {
        let mut app = make_app_with_agent("sess-1");
        seed_models(
            app.agents.get_mut(&AgentId(0)).expect("active agent"),
            "m-old",
            &["m-old"],
        );

        assert!(handle_ext_notification(
            &xai_model_switch_notif("sess-1", "model-switch-missing"),
            &mut app,
        ));

        let switched = &app.agents[&AgentId(0)].session.models;
        let new_id = acp::ModelId::new(std::sync::Arc::from("m-new"));
        assert_eq!(switched.current.as_ref(), Some(&new_id));
        assert!(crate::acp::model_state::is_unavailable_resident_model(
            switched
                .available
                .get(&new_id)
                .expect("confirmed resident remains displayable while catalog catches up")
        ));
        assert_eq!(app.models.current.as_ref(), Some(&new_id));

        assert!(handle_models_update(
            &make_models_update_notif("m-new", &["m-new"]),
            &mut app,
        ));
        let refreshed = &app.agents[&AgentId(0)].session.models;
        assert_eq!(refreshed.current.as_ref(), Some(&new_id));
        assert!(!crate::acp::model_state::is_unavailable_resident_model(
            refreshed
                .available
                .get(&new_id)
                .expect("catalog refresh replaces the temporary placeholder")
        ));
    }

    /// A follower client (no in-flight switch of its own) receives the
    /// leader's `ModelChanged` broadcast and silently mirrors the new model
    /// into its local state — no scrollback entry, no toast, just enough
    /// state for the status bar / `/model` dropdown to render correctly.
    #[test]
    fn model_changed_updates_state_silently_on_follower() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "grok-3", &["grok-3", "grok-4"]);
        let scrollback_before = agent.scrollback.len();
        // Follower: no local switch in flight.
        assert!(!agent.session.model_switch_pending);

        let notif = model_changed_ext("sess-1", "grok-4", None);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(
            changed,
            "follower's state changed → handler must request a redraw"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-4"),
            "follower must mirror the remote switch into its local model state",
        );
        assert_eq!(
            agent.scrollback.len(),
            scrollback_before,
            "follower must NOT push a 'Switched to' scrollback entry — that is \
             the invoking client's job (SwitchModelComplete owns the system message)"
        );
        assert!(
            !agent.session.model_switch_pending,
            "follower's pending flag must stay false (no local switch was issued)"
        );
    }

    /// A live remote `ModelChanged` (leader-mode fan-out from another client)
    /// must apply even when this client already has a local
    /// `user_model_preference` — otherwise the status bar desyncs from the
    /// gateway session. Preference is updated to track the new live model.
    /// (History-replay silent-revert is suppressed on the shell side via
    /// `ReconnectState::user_selected_model`, not by permanently blocking
    /// remote ModelChanged here.)
    #[test]
    fn model_changed_applies_and_updates_user_model_preference() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "heavy", &["auto", "heavy"]);
        agent.session.user_model_preference =
            Some(acp::ModelId::new(std::sync::Arc::from("heavy")));
        assert!(!agent.session.model_switch_pending);

        let notif = model_changed_ext("sess-1", "auto", None);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(
            changed,
            "remote live ModelChanged must apply despite prior local preference"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("auto"),
            "selector must mirror the remote switch"
        );
        assert_eq!(
            agent
                .session
                .user_model_preference
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("auto"),
            "preference must track the applied remote switch"
        );
    }

    /// The invoking client is also a subscriber to its own session and so
    /// receives the broadcast it triggered. Its in-flight
    /// `SetSessionModelResponse` is the authority for its local state +
    /// the single "Switched to X" scrollback entry, so the broadcast handler
    /// must be a no-op here — gated on `model_switch_pending == true`.
    ///
    /// Concretely we verify the broadcast does NOT touch
    /// `models.current` (preserving the pre-response snapshot) — that
    /// snapshot is what `SwitchModelComplete`'s `unchanged` check compares
    /// against to decide whether to render the "Switched to X" message. If
    /// the broadcast optimistically updated state here, the response
    /// handler would see `prev == new`, mark it unchanged, and suppress the
    /// user-facing message entirely.
    #[test]
    fn model_changed_skipped_when_local_switch_in_flight() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "grok-3", &["grok-3", "grok-4"]);
        // Invoker: a local switch is in flight (set by Action::SwitchModel /
        // set_default_model before the SetSessionModelRequest is sent).
        agent.session.model_switch_pending = true;
        let scrollback_before = agent.scrollback.len();

        let notif = model_changed_ext("sess-1", "grok-4", None);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(
            !changed,
            "broadcast must be a no-op while local switch is pending"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-3"),
            "models.current must stay at the pre-response snapshot — \
             SwitchModelComplete owns the final apply + system message"
        );
        assert_eq!(
            agent.scrollback.len(),
            scrollback_before,
            "broadcast must not push any scrollback entry on the invoker"
        );
        assert!(
            agent.session.model_switch_pending,
            "pending flag must remain set until SwitchModelComplete arrives"
        );
    }

    /// A per-session `ModelChanged` is authoritative even when it races ahead
    /// of the machine-wide catalog update. Keep a non-selectable placeholder
    /// so the status bar follows the real resident without offering a stale
    /// catalog row, then resolve it when the catalog catches up.
    #[test]
    fn model_changed_before_catalog_update_uses_resident_placeholder() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "grok-3", &["grok-3", "grok-4"]);

        let notif = model_changed_ext("sess-1", "grok-99-unknown", None);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(changed, "authoritative session change must redraw");

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let target = acp::ModelId::new(std::sync::Arc::from("grok-99-unknown"));
        assert_eq!(
            agent.session.models.current.as_ref(),
            Some(&target),
            "displayed resident must follow the authoritative session event"
        );
        assert!(crate::acp::model_state::is_unavailable_resident_model(
            agent
                .session
                .models
                .available
                .get(&target)
                .expect("catalog-lag placeholder")
        ));
        assert_eq!(app.models.current.as_ref(), Some(&target));

        assert!(handle_models_update(
            &make_models_update_notif("grok-99-unknown", &["grok-99-unknown"]),
            &mut app,
        ));
        assert!(!crate::acp::model_state::is_unavailable_resident_model(
            app.agents[&AgentId(0)]
                .session
                .models
                .available
                .get(&target)
                .expect("real catalog row replaces placeholder")
        ));
    }

    /// `reasoning_effort` round-trips through the broadcast: the follower
    /// applies it alongside the model id so the prompt header / status bar
    /// show the right effort without waiting for a subsequent
    /// `x.ai/models/update`.
    #[test]
    fn model_changed_applies_reasoning_effort_on_follower() {
        use xai_grok_shell::sampling::types::ReasoningEffort;
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "grok-3", &["grok-3", "grok-4"]);

        let notif = model_changed_ext("sess-1", "grok-4", Some("high"));
        assert!(handle_ext_notification(&notif, &mut app));

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.session.models.reasoning_effort,
            Some(ReasoningEffort::High),
            "follower must mirror the broadcast's reasoning_effort"
        );
    }

    /// `ModelChanged` for a session this client doesn't own / hasn't loaded
    /// must be dropped — `find_session_match` returns `None`. The bug-flavored
    /// version of this would be: leader-mode A switches model on session X
    /// (which this client never opened) and we accidentally apply the change
    /// to the active agent.
    #[test]
    fn model_changed_dropped_for_unknown_session_id() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "grok-3", &["grok-3", "grok-4"]);

        let notif = model_changed_ext("sess-OTHER", "grok-4", None);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(!changed);

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-3"),
            "unrelated-session broadcast must not touch this agent's model"
        );
    }
