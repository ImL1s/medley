use super::*;
use crate::agent::config;
use crate::remote::DEFAULT_CONTEXT_WINDOW;
use crate::session::{AppliedModelSwitch, PreparedModelSwitch};
use xai_chat_state::conversation_util::replace_or_insert_system_head;
impl SessionActor {
    /// Validate, prepare, and commit a complete model switch while the session
    /// actor owns the ordering. A required replacement agent is fully built
    /// before either harness or model state is mutated.
    pub(super) async fn handle_apply_model_switch(
        &self,
        prepared: PreparedModelSwitch,
    ) -> Result<AppliedModelSwitch, acp::Error> {
        let PreparedModelSwitch {
            catalog_model_id,
            sampling_config,
            use_concise,
            auto_compact_threshold_percent,
            required_agent_type,
            required_definition,
        } = prepared;
        let active_agent_type = self
            .active_agent_type
            .lock()
            .clone()
            .unwrap_or_else(|| self.agent.borrow().definition().name.clone());
        let previous_model_id = {
            let value = self.catalog_model_id.take();
            self.catalog_model_id.set(value.clone());
            acp::ModelId::new(value)
        };
        let model_unchanged = previous_model_id == catalog_model_id;
        let active_is_strict = self.agent.borrow().definition().is_strict_harness()
            || xai_grok_agent::config::is_strict_harness_agent_type(&active_agent_type);
        let mismatch = active_agent_type != required_agent_type
            && required_definition
                .as_ref()
                .is_none_or(|required| active_is_strict || required.is_strict_harness());
        let mut did_rebuild = false;

        if !self
            .notifications
            .gateway_enabled
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(model_switch_harness_error(
                &catalog_model_id,
                &active_agent_type,
                &required_agent_type,
                "gateway_closed",
            ));
        }

        if mismatch {
            let definition = required_definition.ok_or_else(|| {
                model_switch_harness_error(
                    &catalog_model_id,
                    &active_agent_type,
                    &required_agent_type,
                    "agent_definition_unresolved",
                )
            })?;
            if definition.name != required_agent_type {
                return Err(model_switch_harness_error(
                    &catalog_model_id,
                    &active_agent_type,
                    &required_agent_type,
                    "agent_definition_mismatch",
                ));
            }
            let turn_count = self
                .signals_handle()
                .snapshot()
                .await
                .ok_or_else(|| {
                    model_switch_harness_error(
                        &catalog_model_id,
                        &active_agent_type,
                        &required_agent_type,
                        "turn_count_unavailable",
                    )
                })?
                .turn_count;
            if turn_count > 0 {
                return Err(config::ModelSwitchIncompatibleAgentError {
                    code: config::MODEL_SWITCH_INCOMPATIBLE_AGENT.to_owned(),
                    active_agent_type: active_agent_type.clone(),
                    required_agent_type: required_agent_type.clone(),
                    model_id: catalog_model_id.0.to_string(),
                    suggestion: "start_new_session".to_owned(),
                }
                .into_acp_error());
            }
            self.handle_rebuild_agent_for_definition(definition)
                .await
                .map_err(|_| {
                    model_switch_harness_error(
                        &catalog_model_id,
                        &active_agent_type,
                        &required_agent_type,
                        "agent_build_failed",
                    )
                })?;
            did_rebuild = true;
        }

        let updated_model = self
            .handle_set_session_model(
                catalog_model_id,
                sampling_config,
                use_concise,
                !self.startup_hints.preserve_inherited_system,
                did_rebuild || model_unchanged,
                auto_compact_threshold_percent,
            )
            .await?;
        Ok(AppliedModelSwitch {
            previous_model_id,
            catalog_model_id: updated_model,
            did_rebuild,
            active_agent_type: self.active_agent_type.lock().clone(),
        })
    }

    pub(super) async fn handle_set_session_model(
        &self,
        catalog_model_id: acp::ModelId,
        sampling_config: xai_grok_sampler::SamplerConfig,
        use_concise: bool,
        apply_prompt_override: bool,
        skip_prompt_rewrite: bool,
        auto_compact_threshold_percent: u8,
    ) -> Result<acp::ModelId, acp::Error> {
        self.catalog_model_id.set(catalog_model_id.0.to_string());
        let new_context_window = self.compaction.context_window_override.unwrap_or_else(|| {
            std::num::NonZeroU64::new(sampling_config.context_window).unwrap_or_else(|| {
                std::num::NonZeroU64::new(DEFAULT_CONTEXT_WINDOW)
                    .expect("DEFAULT_CONTEXT_WINDOW is non-zero")
            })
        });
        let prev_threshold = self.compaction.threshold_percent.get();
        if prev_threshold != auto_compact_threshold_percent {
            tracing::info!(
                session_id = %self.session_info.id.0,
                new_model = %sampling_config.model,
                old_threshold = prev_threshold,
                new_threshold = auto_compact_threshold_percent,
                "auto_compact_threshold_percent updated for model switch"
            );
        }
        self.compaction
            .threshold_percent
            .set(auto_compact_threshold_percent);
        self.supports_backend_search
            .set(sampling_config.supports_backend_search);
        self.compactions_remaining
            .set(sampling_config.compactions_remaining);
        self.compaction_at_tokens
            .set(sampling_config.compaction_at_tokens);
        xai_grok_telemetry::unified_log::info(
            "backend_search: model switch",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "new_model": &sampling_config.model,
                "api_backend": format!("{:?}", sampling_config.api_backend),
                "supports_backend_search": sampling_config.supports_backend_search,
            })),
        );
        self.chat_state_handle
            .update_sampling_config(xai_grok_sampling_types::SamplingConfig {
                base_url: sampling_config.base_url.clone(),
                model: sampling_config.model.clone(),
                max_completion_tokens: sampling_config.max_completion_tokens,
                temperature: sampling_config.temperature,
                top_p: sampling_config.top_p,
                api_backend: sampling_config.api_backend.clone(),
                extra_headers: sampling_config.extra_headers.clone(),
                query_params: sampling_config.query_params.clone(),
                env_http_headers: sampling_config.env_http_headers.clone(),
                context_window: new_context_window,
                reasoning_effort: sampling_config.reasoning_effort,
                stream_tool_calls: Some(sampling_config.stream_tool_calls),
            });
        let existing = self.chat_state_handle.get_credentials().await;
        let session_key = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired().map(|a| a.key));
        let (api_key, auth_type) =
            if sampling_config.auth_scheme == xai_grok_sampler::AuthScheme::None {
                // Keyless models must not keep a SessionToken auth_type residue
                // alongside a cleared api_key.
                (None, xai_chat_state::AuthType::ApiKey)
            } else {
                (
                    sampling_config.api_key.clone(),
                    crate::agent::config::resolve_chat_state_auth_type(
                        catalog_model_id.0.as_ref(),
                        session_key.as_deref(),
                        existing.auth_type,
                    ),
                )
            };
        self.chat_state_handle
            .update_credentials(xai_chat_state::Credentials {
                api_key,
                auth_type,
                alpha_test_key: existing.alpha_test_key,
                client_version: sampling_config.client_version.clone(),
            });
        self.invalidate_model_auth_memo();
        self.signals_handle()
            .record_model_usage(&sampling_config.model);
        if apply_prompt_override && !skip_prompt_rewrite {
            let mut conversation = self.chat_state_handle.get_conversation().await;
            for item in conversation.iter_mut() {
                if let ConversationItem::System(sys) = item {
                    if use_concise {
                        sys.content = std::sync::Arc::<str>::from(
                            xai_grok_agent::prompt::template::COMPACT_SYSTEM_PROMPT,
                        );
                    } else {
                        sys.content =
                            std::sync::Arc::<str>::from(self.agent.borrow().system_prompt());
                    }
                    break;
                }
            }
            self.chat_state_handle.replace_conversation(conversation);
        } else if !apply_prompt_override {
            tracing::info!(
                session_id = %self.session_info.id.0,
                model_id = %catalog_model_id.0,
                "handle_set_session_model: skipping prompt override (apply_prompt_override=false)"
            );
        } else {
            tracing::info!(
                session_id = %self.session_info.id.0,
                model_id = %catalog_model_id.0,
                "handle_set_session_model: skipping prompt rewrite (just rebuilt harness)"
            );
        }
        let agent_name = self.agent.borrow().definition().name.clone();
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::CurrentModel {
                model_id: catalog_model_id.clone(),
                agent_name: Some(agent_name),
                reasoning_effort: Some(sampling_config.reasoning_effort),
            });
        Ok(catalog_model_id)
    }
    /// Build and install the harness portion of an actor-owned model switch.
    ///
    /// Builds a fresh [`xai_grok_agent::Agent`] from the cached
    /// [`crate::session::agent_rebuild::AgentRebuildSpec`] + the supplied
    /// [`xai_grok_agent::AgentDefinition`], replaces `self.agent`,
    /// rewrites the system message in the conversation, persists the
    /// new prompt artifacts, and updates `active_agent_type`.
    ///
    /// Triggered from `MvpAgent::set_session_model` only when the new
    /// model's `agent_type` differs from the session's current
    /// `active_agent_type` AND `turn_count == 0` (no user message has
    /// been sent yet). Defense-in-depth: rejects if a turn is in flight.
    pub(super) async fn handle_rebuild_agent_for_definition(
        &self,
        definition: xai_grok_agent::AgentDefinition,
    ) -> Result<(), acp::Error> {
        {
            let state = self.state.lock().await;
            if state.running_task.is_some() {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    new_agent_type = %definition.name,
                    "handle_rebuild_agent_for_definition: turn in flight, rejecting rebuild"
                );
                return Err(acp::Error::internal_error()
                    .data("rebuild_agent: turn in flight, refusing to rebuild harness"));
            }
        }
        let new_agent_name = definition.name.clone();
        tracing::info!(
            session_id = %self.session_info.id.0,
            new_agent_type = %new_agent_name,
            "handle_rebuild_agent_for_definition: rebuilding harness"
        );
        let new_agent = self
            .rebuild_spec
            .build_agent(definition)
            .await
            .map_err(|e| {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    new_agent_type = %new_agent_name,
                    error = %e,
                    "handle_rebuild_agent_for_definition: AgentBuilder::build failed"
                );
                acp::Error::internal_error().data(format!(
                    "rebuild_agent: build failed for agent_type={new_agent_name}: {e}"
                ))
            })?;
        let new_system_prompt = new_agent.system_prompt().to_string();
        let mut new_prompt_context = new_agent.prompt_context().clone();
        new_prompt_context.normalize_for_persistence();
        if let Some(handle) = self.compaction.prefire.take_handle() {
            handle.abort();
            let _ = handle.await;
            self.compaction.prefire.finish();
        }
        self.compaction.prefire.clear();
        *self.agent.borrow_mut() = new_agent;
        *self.active_agent_type.lock() = Some(new_agent_name.clone());
        self.emit_resolved_tool_overrides();
        self.queue_exit_reminder_on_approved_exit.store(
            self.is_cursor_harness(),
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Err(e) = self.workspace_ops.bind_local_session(
            &self.session_id_string(),
            self.tool_context.cwd.as_path().to_path_buf(),
            self.tool_context.hunk_tracker_handle.clone(),
            self.agent.borrow().tool_bridge().toolset(),
            None,
        ) {
            tracing::warn!(error = %e, "failed to rebind local session toolset after agent rebuild");
        }
        {
            let bridge = self.agent.borrow().tool_bridge().clone();
            let snapshot = self.tool_metadata_snapshot.clone();
            let tool_index = crate::session::tool_index::Bm25ToolSearchIndex::new(snapshot);
            bridge
                .update_resource(xai_grok_tools::types::tool_index::ToolIndex(
                    std::sync::Arc::new(tool_index),
                ))
                .await;
            if let Some(client) = self.rebuild_spec.managed_gateway_tool_client.clone() {
                bridge.update_resource(client).await;
            }
            let plan_path = self.plan_mode.lock().plan_file_path().to_path_buf();
            bridge
                .update_resource(xai_grok_tools::types::resources::PlanFilePath(plan_path))
                .await;
            if let Some(display_cwd) = self.display_cwd.get() {
                bridge
                    .set_display_cwd(std::path::PathBuf::from(display_cwd))
                    .await;
            }
            bridge
                .update_resource(
                    xai_grok_tools::implementations::grok_build::workflow::WorkflowLaunchHandle(
                        self.workflow_launch_tx.clone(),
                    ),
                )
                .await;
            if !self.goal_runs_on_workflow_engine() {
                bridge
                    .update_resource(
                        xai_grok_tools::implementations::grok_build::update_goal::GoalUpdateHandle(
                            self.goal_update_tx.clone(),
                        ),
                    )
                    .await;
            }
            if let Some(reservations) = self.tool_context.task_completion_reservations.clone() {
                bridge.update_resource(reservations).await;
            }
            if let Some(gate) = self.tool_context.task_wake_suppressed.clone() {
                bridge.update_resource(gate).await;
            }
            self.inject_deny_read_globs().await;
        }
        {
            let notified = self.mcp_handshakes_done.notified();
            tokio::pin!(notified);
            let needs_wait = {
                let s = self.mcp_state.lock().await;
                !s.configs.is_empty() && !s.is_initialized()
            };
            if needs_wait {
                const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
                tokio::select! {
                    () = &mut notified => {}
                    () = tokio::time::sleep(TIMEOUT) => {
                        tracing::warn!(
                            session_id = %self.session_info.id.0,
                            "handle_rebuild_agent_for_definition: timed out waiting for MCP handshakes"
                        );
                    }
                }
            }
        }
        self.re_register_mcp_tools_on_rebuilt_bridge().await;
        if let Some(old_handle) = self.deferred_prefix.take() {
            old_handle.abort();
        }
        let new_user_prefix = self.build_user_message_prefix().await;
        {
            let mut conversation = self.chat_state_handle.get_conversation().await;
            let _ = replace_or_insert_system_head(&mut conversation, &new_system_prompt);
            let drop_startup_skill_reminder = false;
            Self::rewrite_zero_turn_prefix(
                &mut conversation,
                new_user_prefix,
                drop_startup_skill_reminder,
            );
            if !conversation_has_project_instructions(&conversation)
                && let Some(agents_md_reminder) = self.agent.borrow().agents_md_user_reminder()
            {
                let agents_md_at = conversation.len().min(2);
                conversation.insert(
                    agents_md_at,
                    ConversationItem::project_instructions(agents_md_reminder),
                );
            }
            self.inject_baseline_skill_reminder(&mut conversation).await;
            self.chat_state_handle.replace_conversation(conversation);
        }
        save_prompt_context(&self.session_info, &new_prompt_context);
        save_system_prompt(&self.session_info, &new_system_prompt);
        let snapshot = self.chat_state_handle.get_conversation().await;
        persist_chat_history_jsonl_sync(&self.session_info, &snapshot);
        self.mcp_reminder_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.send_available_commands_update().await;
        tracing::info!(
            session_id = %self.session_info.id.0,
            new_agent_type = %new_agent_name,
            "handle_rebuild_agent_for_definition: harness rebuild complete"
        );
        Ok(())
    }
    /// Apply a client-supplied `systemPromptOverride` on session attach without
    /// wiping user/assistant history: swap only the leading `System` message,
    /// atomically inside the `ChatStateActor` (see
    /// `ChatStateCommand::ReplaceSystemHead` for the serialization guarantees).
    /// `system_prompt.txt` (not owned by the persistence actor) is saved
    /// directly, even on a head no-op, so a previously-diverged secondary
    /// artifact self-heals. Skipped entirely on a verbatim mirror-fork
    /// (`preserve_inherited_system`).
    pub(super) async fn handle_replace_system_prompt(&self, system_prompt: String) {
        if self.startup_hints.preserve_inherited_system {
            tracing::debug!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: skipped (preserve_inherited_system)"
            );
            return;
        }
        let Some(changed) = self
            .chat_state_handle
            .replace_system_head(&system_prompt)
            .await
        else {
            tracing::error!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: chat-state actor unavailable; override not applied"
            );
            return;
        };
        save_system_prompt(&self.session_info, &system_prompt);
        if changed {
            tracing::info!(
                session_id = %self.session_info.id.0,
                prompt_len = system_prompt.len(),
                "handle_replace_system_prompt: client override applied"
            );
        } else {
            tracing::debug!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: head already matches, no-op"
            );
        }
    }
}

fn model_switch_harness_error(
    model_id: &acp::ModelId,
    active_agent_type: &str,
    required_agent_type: &str,
    reason: &str,
) -> acp::Error {
    config::ModelSwitchHarnessError {
        code: config::MODEL_SWITCH_REBUILD_FAILED.to_owned(),
        active_agent_type: active_agent_type.to_owned(),
        required_agent_type: required_agent_type.to_owned(),
        model_id: model_id.0.to_string(),
        reason: reason.to_owned(),
    }
    .into_acp_error()
}

#[cfg(test)]
mod model_switch_transaction_tests {
    use super::super::support::create_test_actor_ex;
    use super::*;

    fn switch_sampling_config(model: &str) -> xai_grok_sampler::SamplerConfig {
        xai_grok_sampler::SamplerConfig {
            api_key: None,
            base_url: "http://127.0.0.1:11434/v1".to_owned(),
            model: model.to_owned(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: crate::sampling::ApiBackend::ChatCompletions,
            auth_scheme: xai_grok_sampler::AuthScheme::None,
            extra_headers: Default::default(),
            query_params: Default::default(),
            env_http_headers: Default::default(),
            context_window: 128_000,
            client_version: None,
            force_http1: false,
            max_retries: None,
            stream_tool_calls: false,
            idle_timeout_secs: None,
            client_identifier: None,
            reasoning_effort: None,
            deployment_id: None,
            user_id: None,
            origin_client: None,
            attribution_callback: None,
            bearer_resolver: None,
            supports_backend_search: true,
            compactions_remaining: None,
            compaction_at_tokens: None,
            doom_loop_recovery: None,
            header_injector: None,
        }
    }

    fn prepared_switch(
        required_agent_type: &str,
        definition: Option<xai_grok_agent::AgentDefinition>,
    ) -> PreparedModelSwitch {
        PreparedModelSwitch {
            catalog_model_id: acp::ModelId::new("target-model"),
            sampling_config: switch_sampling_config("target-wire-model"),
            use_concise: false,
            auto_compact_threshold_percent: 73,
            required_agent_type: required_agent_type.to_owned(),
            required_definition: definition,
        }
    }

    fn catalog_model_id(actor: &SessionActor) -> String {
        let value = actor.catalog_model_id.take();
        actor.catalog_model_id.set(value.clone());
        value
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unresolved_zero_turn_harness_preserves_all_actor_model_state() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                *actor.active_agent_type.lock() = Some("grok-build".to_owned());
                let previous_agent = actor.agent.borrow().definition().name.clone();
                let previous_sampling = actor
                    .chat_state_handle
                    .get_sampling_config()
                    .await
                    .expect("sampling state");
                let previous_credentials = actor.chat_state_handle.get_credentials().await;
                let previous_conversation = actor.chat_state_handle.get_conversation().await;

                let err = actor
                    .handle_apply_model_switch(prepared_switch("missing-custom-harness", None))
                    .await
                    .expect_err("unresolved required harness must fail closed");
                let payload = config::ModelSwitchHarnessError::from_acp_error(&err)
                    .expect("structured harness error");
                assert_eq!(payload.model_id, "target-model");
                assert_eq!(payload.active_agent_type, "grok-build");
                assert_eq!(payload.required_agent_type, "missing-custom-harness");
                assert_eq!(payload.reason, "agent_definition_unresolved");
                assert_eq!(catalog_model_id(&actor), "test");
                assert_eq!(actor.agent.borrow().definition().name, previous_agent);
                assert_eq!(
                    actor.active_agent_type.lock().as_deref(),
                    Some("grok-build")
                );
                let sampling = actor
                    .chat_state_handle
                    .get_sampling_config()
                    .await
                    .expect("sampling state");
                assert_eq!(sampling.model, previous_sampling.model);
                assert_eq!(sampling.base_url, previous_sampling.base_url);
                assert_eq!(sampling.context_window, previous_sampling.context_window);
                let credentials = actor.chat_state_handle.get_credentials().await;
                assert_eq!(credentials.api_key, previous_credentials.api_key);
                assert_eq!(credentials.auth_type, previous_credentials.auth_type);
                assert_eq!(
                    serde_json::to_value(actor.chat_state_handle.get_conversation().await).unwrap(),
                    serde_json::to_value(previous_conversation).unwrap()
                );
                assert_eq!(actor.compaction.threshold_percent.get(), 85);
                assert!(
                    persistence_rx.try_recv().is_err(),
                    "a failed prerequisite must not enqueue persistence"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nonzero_harness_mismatch_uses_incompatible_error() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                *actor.active_agent_type.lock() = Some("codex".to_owned());
                actor.signals_handle().increment_turn();

                let err = actor
                    .handle_apply_model_switch(prepared_switch(
                        "grok-build",
                        Some(xai_grok_agent::AgentDefinition::default_grok_build()),
                    ))
                    .await
                    .expect_err("nonzero mismatch must be rejected");
                let payload = config::ModelSwitchIncompatibleAgentError::from_acp_error(&err)
                    .expect("structured incompatibility error");
                assert_eq!(payload.model_id, "target-model");
                assert_eq!(payload.active_agent_type, "codex");
                assert_eq!(payload.required_agent_type, "grok-build");
                assert_eq!(catalog_model_id(&actor), "test");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gateway_closed_required_rebuild_fails_the_whole_switch() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                *actor.active_agent_type.lock() = Some("codex".to_owned());
                actor
                    .notifications
                    .gateway_enabled
                    .store(false, std::sync::atomic::Ordering::Release);

                let err = actor
                    .handle_apply_model_switch(prepared_switch(
                        "grok-build",
                        Some(xai_grok_agent::AgentDefinition::default_grok_build()),
                    ))
                    .await
                    .expect_err("closed gateway must reject a required rebuild");
                let payload = config::ModelSwitchHarnessError::from_acp_error(&err)
                    .expect("structured harness error");
                assert_eq!(payload.reason, "gateway_closed");
                assert_eq!(catalog_model_id(&actor), "test");
                assert_eq!(actor.active_agent_type.lock().as_deref(), Some("codex"));
                assert_eq!(actor.compaction.threshold_percent.get(), 85);
                assert!(persistence_rx.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gateway_closed_same_harness_switch_preserves_model_state() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                *actor.active_agent_type.lock() = Some("grok-build".to_owned());
                let previous_sampling = actor
                    .chat_state_handle
                    .get_sampling_config()
                    .await
                    .expect("sampling state");
                actor
                    .notifications
                    .gateway_enabled
                    .store(false, std::sync::atomic::Ordering::Release);

                let err = actor
                    .handle_apply_model_switch(prepared_switch("grok-build", None))
                    .await
                    .expect_err("closed gateway must reject even a same-harness switch");
                let payload = config::ModelSwitchHarnessError::from_acp_error(&err)
                    .expect("structured harness error");
                assert_eq!(payload.reason, "gateway_closed");
                assert_eq!(catalog_model_id(&actor), "test");
                assert_eq!(
                    actor.active_agent_type.lock().as_deref(),
                    Some("grok-build")
                );
                assert_eq!(actor.compaction.threshold_percent.get(), 85);
                assert_eq!(
                    actor
                        .chat_state_handle
                        .get_sampling_config()
                        .await
                        .expect("sampling state")
                        .model,
                    previous_sampling.model
                );
                assert!(persistence_rx.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_zero_turn_rebuild_commits_harness_and_model_together() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                *actor.active_agent_type.lock() = Some("codex".to_owned());

                let receipt = actor
                    .handle_apply_model_switch(prepared_switch(
                        "grok-build",
                        Some(xai_grok_agent::AgentDefinition::default_grok_build()),
                    ))
                    .await
                    .expect("valid zero-turn rebuild must succeed");
                assert!(receipt.did_rebuild);
                assert_eq!(receipt.catalog_model_id.0.as_ref(), "target-model");
                assert_eq!(receipt.active_agent_type.as_deref(), Some("grok-build"));
                assert_eq!(catalog_model_id(&actor), "target-model");
                assert_eq!(
                    actor.active_agent_type.lock().as_deref(),
                    Some("grok-build")
                );
                assert_eq!(
                    actor
                        .chat_state_handle
                        .get_sampling_config()
                        .await
                        .expect("sampling state")
                        .model,
                    "target-wire-model"
                );
                assert_eq!(actor.compaction.threshold_percent.get(), 73);
            })
            .await;
    }
}
