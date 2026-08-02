use super::*;
use crate::agent::config;
use crate::agent::mvp_agent::harnesses_are_compatible;
use crate::remote::DEFAULT_CONTEXT_WINDOW;
use crate::session::{AppliedModelSwitch, PreparedModelSwitch};
use xai_chat_state::conversation_util::replace_or_insert_system_head;

#[derive(Clone)]
struct ModelSwitchRollbackState {
    chat: xai_chat_state::ChatStateSnapshot,
}

struct PreparedHarnessRebuild {
    agent: xai_grok_agent::Agent,
    prompt_context: xai_grok_agent::PromptContext,
    system_prompt: String,
}

struct InstalledHarnessRebuild {
    previous_agent: xai_grok_agent::Agent,
    prompt_context: xai_grok_agent::PromptContext,
    system_prompt: String,
}

#[derive(Clone, Copy)]
enum ModelSwitchFailurePhase {
    Validation,
    Rebuild,
    Commit,
}

async fn restore_chat_before_workspace_bind<E>(
    chat_state_handle: &xai_chat_state::ChatStateHandle,
    snapshot: xai_chat_state::ChatStateSnapshot,
    bind_workspace: impl FnOnce() -> Result<(), E>,
) -> Result<(), &'static str> {
    chat_state_handle.restore_snapshot(snapshot);
    if chat_state_handle.snapshot().await.is_none() {
        return Err("chat_state_rollback_failed");
    }
    bind_workspace().map_err(|_| "workspace_rollback_failed")
}

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
        if self.state.lock().await.running_task.is_some() {
            return Err(model_switch_error(
                &catalog_model_id,
                &active_agent_type,
                &required_agent_type,
                ModelSwitchFailurePhase::Validation,
                "turn_in_flight",
            ));
        }
        let model_unchanged = previous_model_id == catalog_model_id;
        let definition_is_compatible = harnesses_are_compatible(
            self.agent.borrow().definition(),
            &required_agent_type,
            required_definition.as_ref(),
        );
        let active_is_strict = self.agent.borrow().definition().is_strict_harness()
            || xai_grok_agent::config::is_strict_harness_agent_type(&active_agent_type);
        let mismatch = !definition_is_compatible
            || (active_is_strict && active_agent_type != required_agent_type);
        if !self
            .notifications
            .gateway_enabled
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(model_switch_error(
                &catalog_model_id,
                &active_agent_type,
                &required_agent_type,
                ModelSwitchFailurePhase::Validation,
                "gateway_closed",
            ));
        }

        let prepared_rebuild = if mismatch {
            let definition = required_definition.ok_or_else(|| {
                model_switch_error(
                    &catalog_model_id,
                    &active_agent_type,
                    &required_agent_type,
                    ModelSwitchFailurePhase::Rebuild,
                    "agent_definition_unresolved",
                )
            })?;
            if !definition_matches_required_identity(&definition, &required_agent_type) {
                return Err(model_switch_error(
                    &catalog_model_id,
                    &active_agent_type,
                    &required_agent_type,
                    ModelSwitchFailurePhase::Rebuild,
                    "agent_definition_mismatch",
                ));
            }
            let turn_count = self
                .signals_handle()
                .snapshot()
                .await
                .ok_or_else(|| {
                    model_switch_error(
                        &catalog_model_id,
                        &active_agent_type,
                        &required_agent_type,
                        ModelSwitchFailurePhase::Validation,
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
            Some(
                self.prepare_harness_rebuild(definition)
                    .await
                    .map_err(|_| {
                        model_switch_error(
                            &catalog_model_id,
                            &active_agent_type,
                            &required_agent_type,
                            ModelSwitchFailurePhase::Rebuild,
                            "agent_build_failed",
                        )
                    })?,
            )
        } else {
            None
        };

        // A background prefire is sampling with the current model and cannot
        // be resumed once cancelled. Reject instead of destroying compaction
        // state on a switch that may still fail later.
        if self.compaction.prefire.is_in_flight() {
            return Err(model_switch_error(
                &catalog_model_id,
                &active_agent_type,
                &required_agent_type,
                ModelSwitchFailurePhase::Validation,
                "compaction_prefire_in_flight",
            ));
        }

        // Finish the old harness's deferred startup prefix before taking the
        // rollback snapshot. That removes the only task which could observe a
        // half-installed agent while preserving its normal prompt contribution
        // if the switch later rolls back.
        if prepared_rebuild.is_some() {
            self.ensure_prefix_ready().await;
        }

        let rollback = ModelSwitchRollbackState {
            chat: self.chat_state_handle.snapshot().await.ok_or_else(|| {
                model_switch_error(
                    &catalog_model_id,
                    &active_agent_type,
                    &required_agent_type,
                    ModelSwitchFailurePhase::Validation,
                    "chat_state_unavailable",
                )
            })?,
        };
        let previous_active_agent_type = active_agent_type.clone();
        let mut installed_rebuild = if let Some(prepared_rebuild) = prepared_rebuild {
            Some(
                self.install_harness_rebuild(prepared_rebuild, &required_agent_type)
                    .await
                    .map_err(|_| {
                        model_switch_error(
                            &catalog_model_id,
                            &active_agent_type,
                            &required_agent_type,
                            ModelSwitchFailurePhase::Rebuild,
                            "agent_build_failed",
                        )
                    })?,
            )
        } else {
            None
        };
        let did_rebuild = installed_rebuild.is_some();

        let updated_model = match self
            .handle_set_session_model_with_rollback(
                catalog_model_id.clone(),
                sampling_config,
                use_concise,
                !self.startup_hints.preserve_inherited_system,
                did_rebuild || model_unchanged,
                auto_compact_threshold_percent,
                &required_agent_type,
                Some(rollback.clone()),
            )
            .await
        {
            Ok(model) => model,
            Err(error) => {
                if let Some(rebuild) = installed_rebuild.take() {
                    *self.agent.borrow_mut() = rebuild.previous_agent;
                    *self.active_agent_type.lock() = Some(previous_active_agent_type.clone());
                    let old_toolset = self.agent.borrow().tool_bridge().toolset();
                    if let Err(reason) = restore_chat_before_workspace_bind(
                        &self.chat_state_handle,
                        rollback.chat.clone(),
                        || {
                            self.workspace_ops.bind_local_session(
                                &self.session_id_string(),
                                self.tool_context.cwd.as_path().to_path_buf(),
                                self.tool_context.hunk_tracker_handle.clone(),
                                old_toolset,
                                None,
                            )
                        },
                    )
                    .await
                    {
                        return Err(model_switch_error(
                            &catalog_model_id,
                            &previous_active_agent_type,
                            &required_agent_type,
                            ModelSwitchFailurePhase::Commit,
                            reason,
                        ));
                    }
                }
                return Err(error);
            }
        };
        self.invalidate_prefire_after_model_switch().await;
        if let Some(rebuild) = installed_rebuild {
            self.commit_rebuilt_harness_side_effects().await;
            save_prompt_context(&self.session_info, &rebuild.prompt_context);
            save_system_prompt(&self.session_info, &rebuild.system_prompt);
            let snapshot = self.chat_state_handle.get_conversation().await;
            persist_chat_history_jsonl_sync(&self.session_info, &snapshot);
            self.mcp_reminder_dirty
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.send_available_commands_update().await;
        }
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
        required_agent_type: &str,
    ) -> Result<acp::ModelId, acp::Error> {
        self.handle_set_session_model_with_rollback(
            catalog_model_id,
            sampling_config,
            use_concise,
            apply_prompt_override,
            skip_prompt_rewrite,
            auto_compact_threshold_percent,
            required_agent_type,
            None,
        )
        .await
    }

    async fn handle_set_session_model_with_rollback(
        &self,
        catalog_model_id: acp::ModelId,
        sampling_config: xai_grok_sampler::SamplerConfig,
        use_concise: bool,
        apply_prompt_override: bool,
        skip_prompt_rewrite: bool,
        auto_compact_threshold_percent: u8,
        required_agent_type: &str,
        rollback: Option<ModelSwitchRollbackState>,
    ) -> Result<acp::ModelId, acp::Error> {
        let active_agent_type = self
            .active_agent_type
            .lock()
            .clone()
            .unwrap_or_else(|| self.agent.borrow().definition().name.clone());
        let unavailable_phase = if rollback.is_some() {
            ModelSwitchFailurePhase::Commit
        } else {
            ModelSwitchFailurePhase::Validation
        };
        let current_chat = self.chat_state_handle.snapshot().await.ok_or_else(|| {
            model_switch_error(
                &catalog_model_id,
                &active_agent_type,
                required_agent_type,
                unavailable_phase,
                "chat_state_unavailable",
            )
        })?;
        let rollback = rollback.unwrap_or_else(|| ModelSwitchRollbackState {
            chat: current_chat.clone(),
        });
        let new_context_window = self.compaction.context_window_override.unwrap_or_else(|| {
            std::num::NonZeroU64::new(sampling_config.context_window).unwrap_or_else(|| {
                std::num::NonZeroU64::new(DEFAULT_CONTEXT_WINDOW)
                    .expect("DEFAULT_CONTEXT_WINDOW is non-zero")
            })
        });
        let mut committed_chat = current_chat.clone();
        committed_chat.sampling_config = xai_grok_sampling_types::SamplingConfig {
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
        };
        let existing = current_chat.credentials.clone();
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
        committed_chat.credentials = xai_chat_state::Credentials {
            api_key,
            auth_type,
            alpha_test_key: existing.alpha_test_key,
            client_version: sampling_config.client_version.clone(),
        };
        if apply_prompt_override && !skip_prompt_rewrite {
            for item in &mut committed_chat.conversation {
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
        self.chat_state_handle
            .restore_snapshot(committed_chat.clone());
        if self.chat_state_handle.snapshot().await.is_none() {
            return Err(model_switch_error(
                &catalog_model_id,
                &active_agent_type,
                required_agent_type,
                ModelSwitchFailurePhase::Commit,
                "chat_state_commit_unavailable",
            ));
        }

        if let Err(reason) = self
            .persist_model_switch_transaction(
                committed_chat.conversation,
                &catalog_model_id,
                &active_agent_type,
                sampling_config.reasoning_effort,
            )
            .await
        {
            return self
                .rollback_model_switch_persistence(
                    catalog_model_id,
                    &active_agent_type,
                    required_agent_type,
                    rollback,
                    reason,
                )
                .await;
        }

        let prev_threshold = self.compaction.threshold_percent.get();
        self.catalog_model_id.set(catalog_model_id.0.to_string());
        self.compaction
            .threshold_percent
            .set(auto_compact_threshold_percent);
        self.supports_backend_search
            .set(sampling_config.supports_backend_search);
        self.compactions_remaining
            .set(sampling_config.compactions_remaining);
        self.compaction_at_tokens
            .set(sampling_config.compaction_at_tokens);
        self.invalidate_model_auth_memo();
        self.signals_handle()
            .record_model_usage(&sampling_config.model);
        if prev_threshold != auto_compact_threshold_percent {
            tracing::info!(
                session_id = %self.session_info.id.0,
                new_model = %sampling_config.model,
                old_threshold = prev_threshold,
                new_threshold = auto_compact_threshold_percent,
                "auto_compact_threshold_percent updated for model switch"
            );
        }
        xai_grok_telemetry::unified_log::info(
            "backend_search: model switch",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "new_model": &sampling_config.model,
                "api_backend": format!("{:?}", sampling_config.api_backend),
                "supports_backend_search": sampling_config.supports_backend_search,
            })),
        );
        Ok(catalog_model_id)
    }

    async fn persist_model_switch_transaction(
        &self,
        conversation: Vec<ConversationItem>,
        model_id: &acp::ModelId,
        agent_type: &str,
        reasoning_effort: Option<xai_grok_sampling_types::ReasoningEffort>,
    ) -> Result<(), &'static str> {
        if self.notifications.persistence_is_noop {
            return Ok(());
        }
        let (respond_to, acknowledgement) = tokio::sync::oneshot::channel();
        self.notifications
            .persistence_tx
            .send(PersistenceMsg::ModelSwitchAndAck {
                messages: conversation,
                model_id: model_id.clone(),
                agent_name: Some(agent_type.to_owned()),
                reasoning_effort,
                respond_to,
            })
            .map_err(|_| "persistence_channel_closed")?;
        match acknowledgement.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) if error.is_committed() => {
                tracing::warn!(
                    ?error,
                    "model-switch intent committed; recovery remains pending"
                );
                Ok(())
            }
            Ok(Err(_)) => Err("persistence_write_failed"),
            Err(_) => Err("persistence_ack_dropped"),
        }
    }

    async fn rollback_model_switch_persistence(
        &self,
        catalog_model_id: acp::ModelId,
        active_agent_type: &str,
        required_agent_type: &str,
        rollback: ModelSwitchRollbackState,
        failure_reason: &'static str,
    ) -> Result<acp::ModelId, acp::Error> {
        self.chat_state_handle.restore_snapshot(rollback.chat);
        let _ = self.chat_state_handle.snapshot().await;
        Err(model_switch_error(
            &catalog_model_id,
            active_agent_type,
            required_agent_type,
            ModelSwitchFailurePhase::Commit,
            failure_reason,
        ))
    }
    /// Build and validate a replacement harness without mutating live session
    /// state. Tool discovery is staged on the replacement bridge so every
    /// fallible prerequisite completes before the transaction snapshot.
    async fn prepare_harness_rebuild(
        &self,
        definition: xai_grok_agent::AgentDefinition,
    ) -> Result<PreparedHarnessRebuild, acp::Error> {
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
                        return Err(acp::Error::internal_error()
                            .data("rebuild_agent: timed out waiting for MCP handshakes"));
                    }
                }
            }
        }
        self.stage_mcp_tools_on_bridge(new_agent.tool_bridge())
            .await?;
        Ok(PreparedHarnessRebuild {
            agent: new_agent,
            prompt_context: new_prompt_context,
            system_prompt: new_system_prompt,
        })
    }

    /// Install an already-prepared harness and rewrite the zero-turn prompt.
    /// Only the agent/workspace/chat mirrors change here; all externally
    /// observable follow-up side effects are deferred until durable model and
    /// chat persistence have both acknowledged success.
    async fn install_harness_rebuild(
        &self,
        prepared: PreparedHarnessRebuild,
        canonical_agent_type: &str,
    ) -> Result<InstalledHarnessRebuild, acp::Error> {
        let PreparedHarnessRebuild {
            agent: new_agent,
            prompt_context,
            system_prompt,
        } = prepared;
        let new_agent_name = new_agent.definition().name.clone();
        self.workspace_ops
            .bind_local_session(
                &self.session_id_string(),
                self.tool_context.cwd.as_path().to_path_buf(),
                self.tool_context.hunk_tracker_handle.clone(),
                new_agent.tool_bridge().toolset(),
                None,
            )
            .map_err(|e| {
                acp::Error::internal_error().data(format!(
                    "rebuild_agent: failed to bind rebuilt toolset: {e}"
                ))
            })?;
        let old_agent = self.agent.replace(new_agent);
        *self.active_agent_type.lock() = Some(canonical_agent_type.to_owned());
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
        let new_user_prefix = self.build_user_message_prefix().await;
        {
            let mut conversation = self.chat_state_handle.get_conversation().await;
            let _ = replace_or_insert_system_head(&mut conversation, &system_prompt);
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
        tracing::info!(
            session_id = %self.session_info.id.0,
            new_agent_type = %new_agent_name,
            "install_harness_rebuild: staged harness installed"
        );
        Ok(InstalledHarnessRebuild {
            previous_agent: old_agent,
            prompt_context,
            system_prompt,
        })
    }

    async fn invalidate_prefire_after_model_switch(&self) {
        if let Some(handle) = self.compaction.prefire.take_handle() {
            let _ = handle.await;
        }
        self.compaction.prefire.clear();
    }

    async fn commit_rebuilt_harness_side_effects(&self) {
        self.emit_resolved_tool_overrides();
        self.queue_exit_reminder_on_approved_exit.store(
            self.is_cursor_harness(),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.refresh_mcp_snapshot_and_schedule_reminder().await;
    }

    #[cfg(test)]
    pub(super) async fn handle_rebuild_agent_for_definition(
        &self,
        definition: xai_grok_agent::AgentDefinition,
        canonical_agent_type: &str,
    ) -> Result<(xai_grok_agent::Agent, xai_grok_agent::PromptContext, String), acp::Error> {
        let prepared = self.prepare_harness_rebuild(definition).await?;
        self.ensure_prefix_ready().await;
        let installed = self
            .install_harness_rebuild(prepared, canonical_agent_type)
            .await?;
        self.commit_rebuilt_harness_side_effects().await;
        Ok((
            installed.previous_agent,
            installed.prompt_context,
            installed.system_prompt,
        ))
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

fn model_switch_error(
    model_id: &acp::ModelId,
    active_agent_type: &str,
    required_agent_type: &str,
    phase: ModelSwitchFailurePhase,
    reason: &str,
) -> acp::Error {
    let harness_error = |code: &str| {
        config::ModelSwitchHarnessError {
            code: code.to_owned(),
            active_agent_type: active_agent_type.to_owned(),
            required_agent_type: required_agent_type.to_owned(),
            model_id: model_id.0.to_string(),
            reason: reason.to_owned(),
        }
        .into_acp_error()
    };
    match phase {
        ModelSwitchFailurePhase::Validation => {
            harness_error(config::MODEL_SWITCH_VALIDATION_FAILED)
        }
        ModelSwitchFailurePhase::Rebuild => harness_error(config::MODEL_SWITCH_REBUILD_FAILED),
        ModelSwitchFailurePhase::Commit => config::ModelSwitchCommitError {
            code: config::MODEL_SWITCH_COMMIT_FAILED.to_owned(),
            active_agent_type: active_agent_type.to_owned(),
            required_agent_type: required_agent_type.to_owned(),
            model_id: model_id.0.to_string(),
            reason: reason.to_owned(),
        }
        .into_acp_error(),
    }
}

/// Discovery preserves a plugin agent's bare frontmatter `name` while the
/// caller/session identity may be qualified (`plugin:name`). Keep those two
/// concepts separate: validate the namespace when one was requested, without
/// requiring the definition's display name to contain it.
fn definition_matches_required_identity(
    definition: &xai_grok_agent::AgentDefinition,
    required_agent_type: &str,
) -> bool {
    match required_agent_type.split_once(':') {
        Some((plugin, name)) => {
            definition.plugin_name.as_deref() == Some(plugin) && definition.name == name
        }
        None => definition.name == required_agent_type,
    }
}

#[cfg(test)]
mod model_switch_transaction_tests {
    use super::super::support::{create_test_actor_ex, running_task_stub};
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

    fn assert_model_switch_phase(error: &acp::Error, code: &'static str, reason: &str) {
        assert_eq!(config::model_switch_error_code(error), Some(code));
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(serde_json::Value::as_str),
            Some(reason)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn workspace_bind_failure_still_restores_chat_snapshot_first() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                let old_chat = actor.chat_state_handle.snapshot().await.expect("snapshot");
                let mut switched_chat = old_chat.clone();
                switched_chat.sampling_config.model = "new-wire-model".to_owned();
                actor.chat_state_handle.restore_snapshot(switched_chat);

                let error = restore_chat_before_workspace_bind(
                    &actor.chat_state_handle,
                    old_chat.clone(),
                    || Err::<(), ()>(()),
                )
                .await
                .expect_err("injected workspace bind failure");

                assert_eq!(error, "workspace_rollback_failed");
                assert_eq!(
                    serde_json::to_value(actor.chat_state_handle.snapshot().await.unwrap())
                        .unwrap(),
                    serde_json::to_value(old_chat).unwrap(),
                    "bind failure must not return before the old chat generation is restored"
                );
            })
            .await;
    }

    fn prefire_cache(model_slug: &str) -> crate::session::compaction_config::AsyncCompactionCache {
        crate::session::compaction_config::AsyncCompactionCache {
            note1: "cached prefire summary".to_owned(),
            prefix_len: 1,
            fingerprint: 7,
            model_slug: model_slug.to_owned(),
            pass1_latency_ms: 1,
        }
    }

    #[test]
    fn qualified_plugin_identity_matches_bare_definition_name() {
        let mut definition = xai_grok_agent::AgentDefinition::default_grok_build();
        definition.name = "reviewer".to_owned();
        definition.plugin_name = Some("quality".to_owned());
        assert!(definition_matches_required_identity(
            &definition,
            "quality:reviewer"
        ));
        assert!(!definition_matches_required_identity(
            &definition,
            "other:reviewer"
        ));
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
                assert_model_switch_phase(
                    &err,
                    config::MODEL_SWITCH_REBUILD_FAILED,
                    "agent_definition_unresolved",
                );
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
                assert_model_switch_phase(
                    &err,
                    config::MODEL_SWITCH_VALIDATION_FAILED,
                    "gateway_closed",
                );
                assert!(config::ModelSwitchHarnessError::from_acp_error(&err).is_none());
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
                assert_model_switch_phase(
                    &err,
                    config::MODEL_SWITCH_VALIDATION_FAILED,
                    "gateway_closed",
                );
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
    async fn same_harness_switch_rejects_while_turn_is_in_flight() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                *actor.active_agent_type.lock() = Some("grok-build".to_owned());
                actor.state.lock().await.running_task = Some(running_task_stub("running-turn"));
                let previous_sampling = actor
                    .chat_state_handle
                    .get_sampling_config()
                    .await
                    .expect("sampling state");

                let err = actor
                    .handle_apply_model_switch(prepared_switch("grok-build", None))
                    .await
                    .expect_err("a live turn must keep its model snapshot stable");
                assert_model_switch_phase(
                    &err,
                    config::MODEL_SWITCH_VALIDATION_FAILED,
                    "turn_in_flight",
                );
                assert_eq!(catalog_model_id(&actor), "test");
                assert_eq!(
                    actor
                        .chat_state_handle
                        .get_sampling_config()
                        .await
                        .expect("sampling state")
                        .model,
                    previous_sampling.model
                );
                assert_eq!(actor.compaction.threshold_percent.get(), 85);
                assert!(
                    persistence_rx.try_recv().is_err(),
                    "the rejected switch must not enqueue persistence"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_zero_turn_rebuild_commits_harness_and_model_together() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                tokio::task::spawn_local(async move {
                    while let Some(message) = persistence_rx.recv().await {
                        match message {
                            PersistenceMsg::ModelSwitchAndAck { respond_to, .. } => {
                                let _ = respond_to.send(Ok(()));
                            }
                            PersistenceMsg::CurrentModelAndAck { respond_to, .. }
                            | PersistenceMsg::ReplaceChatHistoryAndAck { respond_to, .. } => {
                                let _ = respond_to.send(Ok(()));
                            }
                            _ => {}
                        }
                    }
                });
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                *actor.active_agent_type.lock() = Some("codex".to_owned());
                actor.compaction.prefire.store(prefire_cache("test"));

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
                assert!(
                    !actor.compaction.prefire.has_cache(),
                    "a successful model switch must invalidate the old-model prefire cache"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn noop_persistence_allows_live_model_switch_without_durable_writes() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let persistence = crate::session::persistence::PersistenceHandle::noop();
                let (mut actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence.tx.clone()).await;
                actor.notifications.persistence_is_noop = persistence.is_noop();
                *actor.active_agent_type.lock() = Some("grok-build".to_owned());

                let receipt = actor
                    .handle_apply_model_switch(prepared_switch("grok-build", None))
                    .await
                    .expect("an explicitly no-op persistence handle must not block a live switch");

                assert!(!receipt.did_rebuild);
                assert_eq!(receipt.catalog_model_id.0.as_ref(), "target-model");
                assert_eq!(catalog_model_id(&actor), "target-model");
                assert_eq!(
                    actor
                        .chat_state_handle
                        .get_sampling_config()
                        .await
                        .expect("sampling state")
                        .model,
                    "target-wire-model"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closed_real_persistence_channel_still_rejects_live_model_switch() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                drop(persistence_rx);
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                *actor.active_agent_type.lock() = Some("grok-build".to_owned());
                let previous_chat = actor.chat_state_handle.snapshot().await.expect("snapshot");

                assert_eq!(
                    actor
                        .persist_model_switch_transaction(
                            previous_chat.conversation.clone(),
                            &acp::ModelId::new("test"),
                            "grok-build",
                            previous_chat.sampling_config.reasoning_effort,
                        )
                        .await,
                    Err("persistence_channel_closed"),
                    "a dropped real receiver must not be treated as explicit no-op persistence"
                );

                let error = actor
                    .handle_apply_model_switch(prepared_switch("grok-build", None))
                    .await
                    .expect_err("a closed real persistence channel must fail closed");
                assert_model_switch_phase(
                    &error,
                    config::MODEL_SWITCH_COMMIT_FAILED,
                    "persistence_channel_closed",
                );
                let payload = config::ModelSwitchCommitError::from_acp_error(&error)
                    .expect("structured commit error");
                assert_eq!(payload.reason, "persistence_channel_closed");
                assert!(config::ModelSwitchHarnessError::from_acp_error(&error).is_none());
                assert_eq!(catalog_model_id(&actor), "test");
                assert_eq!(
                    actor
                        .chat_state_handle
                        .get_sampling_config()
                        .await
                        .expect("sampling state")
                        .model,
                    previous_chat.sampling_config.model
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_flight_prefire_rejects_switch_without_mutating_state() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                *actor.active_agent_type.lock() = Some("codex".to_owned());
                assert!(actor.compaction.prefire.try_begin());
                let previous_chat = actor.chat_state_handle.snapshot().await.expect("snapshot");

                let error = actor
                    .handle_apply_model_switch(prepared_switch(
                        "grok-build",
                        Some(xai_grok_agent::AgentDefinition::default_grok_build()),
                    ))
                    .await
                    .expect_err("an in-flight prefire cannot be atomically replaced");
                assert_model_switch_phase(
                    &error,
                    config::MODEL_SWITCH_VALIDATION_FAILED,
                    "compaction_prefire_in_flight",
                );
                assert_eq!(catalog_model_id(&actor), "test");
                assert_eq!(actor.active_agent_type.lock().as_deref(), Some("codex"));
                assert!(actor.compaction.prefire.is_in_flight());
                assert_eq!(
                    serde_json::to_value(actor.chat_state_handle.snapshot().await.unwrap())
                        .unwrap(),
                    serde_json::to_value(previous_chat).unwrap()
                );
                assert!(persistence_rx.try_recv().is_err());
                actor.compaction.prefire.finish();
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persistence_failure_rolls_back_rebuilt_harness_and_chat_snapshot() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let persisted_generations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let receiver_generations = persisted_generations.clone();
                tokio::task::spawn_local(async move {
                    while let Some(message) = persistence_rx.recv().await {
                        match message {
                            PersistenceMsg::ModelSwitchAndAck {
                                messages,
                                model_id,
                                respond_to,
                                ..
                            } => {
                                receiver_generations
                                    .lock()
                                    .unwrap()
                                    .push((model_id.0.to_string(), messages));
                                let _ = respond_to.send(Err(
                                    crate::session::storage::ModelSwitchCommitError::NotCommitted(
                                        std::io::Error::other("injected failure"),
                                    ),
                                ));
                            }
                            _ => {}
                        }
                    }
                });
                let (actor, _event_rx) =
                    create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
                *actor.active_agent_type.lock() = Some("codex".to_owned());
                actor.compaction.prefire.store(prefire_cache("test"));
                let previous_agent = actor.agent.borrow().definition().name.clone();
                let previous_chat = actor.chat_state_handle.snapshot().await.expect("snapshot");
                let mut target_definition = xai_grok_agent::AgentDefinition::default_grok_build();
                target_definition.tool_overrides = Some(xai_grok_sampling_types::ToolOverrides {
                    x_search: Some(xai_grok_sampling_types::XSearchOptions {
                        date_bound: Some(
                            xai_grok_sampling_types::SearchDateBound::new(
                                None,
                                Some("2020-01-01".to_owned()),
                            )
                            .expect("valid bound"),
                        ),
                    }),
                    web_search: None,
                });

                let error = actor
                    .handle_apply_model_switch(prepared_switch(
                        "grok-build",
                        Some(target_definition),
                    ))
                    .await
                    .expect_err("persistence failure must fail the switch");
                assert_model_switch_phase(
                    &error,
                    config::MODEL_SWITCH_COMMIT_FAILED,
                    "persistence_write_failed",
                );
                let payload = config::ModelSwitchCommitError::from_acp_error(&error)
                    .expect("structured commit error");
                assert_eq!(payload.reason, "persistence_write_failed");
                assert!(config::ModelSwitchHarnessError::from_acp_error(&error).is_none());
                assert_eq!(catalog_model_id(&actor), "test");
                assert_eq!(actor.agent.borrow().definition().name, previous_agent);
                assert_eq!(actor.active_agent_type.lock().as_deref(), Some("codex"));
                assert_eq!(actor.compaction.threshold_percent.get(), 85);
                assert!(
                    actor.compaction.prefire.has_cache(),
                    "a failed switch must preserve the old-model prefire cache"
                );
                assert!(
                    actor.resolved_tool_overrides.load().is_none(),
                    "a failed switch must not publish the replacement harness overrides"
                );
                assert!(
                    !actor
                        .mcp_reminder_dirty
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "a failed switch must not publish replacement MCP reminder state"
                );
                let restored = actor.chat_state_handle.snapshot().await.expect("snapshot");
                assert_eq!(
                    restored.sampling_config.model,
                    previous_chat.sampling_config.model
                );
                assert_eq!(
                    serde_json::to_value(restored.conversation).unwrap(),
                    serde_json::to_value(&previous_chat.conversation).unwrap()
                );
                assert_eq!(
                    restored.credentials.api_key,
                    previous_chat.credentials.api_key
                );
                assert_eq!(
                    restored.credentials.auth_type,
                    previous_chat.credentials.auth_type
                );
                let generations = persisted_generations.lock().unwrap();
                assert_eq!(
                    generations.len(),
                    1,
                    "the switch must use one persistence request"
                );
                assert_eq!(generations[0].0, "target-model");
                assert_ne!(
                    serde_json::to_value(&generations[0].1).unwrap(),
                    serde_json::to_value(&previous_chat.conversation).unwrap(),
                    "the single request carries the complete target chat generation"
                );
            })
            .await;
    }
}
