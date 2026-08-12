#![cfg_attr(rustfmt, rustfmt::skip)]
#![allow(unused_imports)]
//! [`acp::Agent`] trait implementation for [`MvpAgent`].
//! Co-located child of `mvp_agent` (`use super::*`).
use super::*;
use crate::auth::SilentRefresh;
use crate::leader::protocol::InternalMethod;

#[cfg(test)]
type PromptDispatchBoundaryHook = Box<dyn FnOnce() + 'static>;

#[cfg(test)]
type PromptRecoveryBoundaryHook = Box<dyn FnOnce(&acp::ModelId) + 'static>;

#[cfg(test)]
thread_local! {
    static PROMPT_DISPATCH_BOUNDARY_HOOKS: std::cell::RefCell<
        std::collections::HashMap<String, PromptDispatchBoundaryHook>
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    static PROMPT_RECOVERY_BOUNDARY_HOOKS: std::cell::RefCell<
        std::collections::HashMap<String, PromptRecoveryBoundaryHook>
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

#[cfg(test)]
pub(crate) fn install_prompt_dispatch_boundary_hook(
    session_id: &acp::SessionId,
    hook: impl FnOnce() + 'static,
) {
    PROMPT_DISPATCH_BOUNDARY_HOOKS.with(|hooks| {
        hooks
            .borrow_mut()
            .insert(session_id.0.to_string(), Box::new(hook));
    });
}

#[cfg(test)]
fn run_prompt_dispatch_boundary_hook(session_id: &acp::SessionId) {
    let hook = PROMPT_DISPATCH_BOUNDARY_HOOKS
        .with(|hooks| hooks.borrow_mut().remove(session_id.0.as_ref()));
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
pub(crate) fn install_prompt_recovery_boundary_hook(
    session_id: &acp::SessionId,
    hook: impl FnOnce(&acp::ModelId) + 'static,
) {
    PROMPT_RECOVERY_BOUNDARY_HOOKS.with(|hooks| {
        hooks
            .borrow_mut()
            .insert(session_id.0.to_string(), Box::new(hook));
    });
}

#[cfg(test)]
fn run_prompt_recovery_boundary_hook(
    session_id: &acp::SessionId,
    restore_model_id: &acp::ModelId,
) {
    let hook = PROMPT_RECOVERY_BOUNDARY_HOOKS
        .with(|hooks| hooks.borrow_mut().remove(session_id.0.as_ref()));
    if let Some(hook) = hook {
        hook(restore_model_id);
    }
}

pub(super) fn normalize_resident_model_if_unchanged(
    resident: &mut SessionHandle,
    expected_model: &acp::ModelId,
    normalized_model: &acp::ModelId,
) -> bool {
    if resident.model_id != *expected_model {
        return false;
    }
    resident.model_id = normalized_model.clone();
    true
}

pub(super) fn has_advertised_auth_provider_command(
    config: &crate::auth::GrokComConfig,
) -> bool {
    crate::auth::has_nonblank_auth_provider_command(config.auth_provider_command.as_deref())
}

/// The single model-restore edge used by `session/load` after registration and
/// before its load guard is released. Keeping this wrapper in the load module
/// prevents restore callers from accidentally taking the external wait path.
/// The bypass is bound to the load's own guard, so a superseded duplicate
/// load cannot resolve its handle through a newer load's marker.
pub(super) async fn restore_registered_session_model(
    agent: &MvpAgent,
    request: acp::SetSessionModelRequest,
    load_guard: &SessionLoadGuard<'_>,
    restored_model: Option<(
        xai_chat_state::CatalogIdentity,
        crate::agent::config::ModelEntry,
    )>,
) -> Result<acp::SetSessionModelResponse, acp::Error> {
    crate::agent::handlers::model_switch::apply_during_session_load(
        agent,
        request,
        load_guard,
        restored_model,
    )
    .await
}

/// Which `x_search` sub-tools enforce the date cutoff, sent in `initialize`. `x_user_search` and
/// `x_thread_fetch` are `false`: they don't honor it yet.
#[derive(serde::Serialize)]
struct ToolOverridesCapability {
    x_keyword_search: bool,
    x_semantic_search: bool,
    x_user_search: bool,
    x_thread_fetch: bool,
}
const TOOL_OVERRIDES_CAPABILITY: ToolOverridesCapability = ToolOverridesCapability {
    x_keyword_search: true,
    x_semantic_search: true,
    x_user_search: false,
    x_thread_fetch: false,
};
fn tool_overrides_capability() -> serde_json::Value {
    serde_json::to_value(TOOL_OVERRIDES_CAPABILITY)
        .expect("ToolOverridesCapability is always serializable")
}
async fn read_applied_tool_overrides(
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<SessionCommand>,
) -> Option<xai_grok_sampling_types::ToolOverrides> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(SessionCommand::GetToolOverrides {
            respond_to: tx,
        })
        .is_err()
    {
        tracing::warn!("tool-overrides echo: session actor command channel closed");
        return None;
    }
    match rx.await {
        Ok(overrides) => overrides,
        Err(_) => {
            tracing::warn!("tool-overrides echo: session actor dropped the response channel");
            None
        }
    }
}
fn insert_applied_tool_overrides(
    meta: &mut serde_json::Map<String, serde_json::Value>,
    echo: Option<&xai_grok_sampling_types::ToolOverrides>,
) {
    if let Some(overrides) = echo {
        meta.insert(
            "toolOverrides".to_string(),
            serde_json::to_value(overrides)
                .expect("ToolOverrides is always serializable"),
        );
    }
}

pub(super) fn auth_init_disk_refresh_context(
    pre: Option<&crate::auth::GrokAuth>,
    post: Option<&crate::auth::GrokAuth>,
) -> serde_json::Value {
    let access_relation = crate::auth::refresh::TriedDiskRelation::compare(
        pre.map(|auth| auth.key.as_str()),
        post.map(|auth| auth.key.as_str()),
    );
    let refresh_relation = crate::auth::refresh::TriedDiskRelation::compare(
        pre.and_then(|auth| auth.refresh_token.as_deref()),
        post.and_then(|auth| auth.refresh_token.as_deref()),
    );
    serde_json::json!({
        "access_relation": access_relation.as_str(),
        "access_pre_present": access_relation.tried_present(),
        "access_post_present": access_relation.disk_present(),
        "refresh_relation": refresh_relation.as_str(),
        "refresh_pre_present": refresh_relation.tried_present(),
        "refresh_post_present": refresh_relation.disk_present(),
    })
}
#[async_trait::async_trait(?Send)]
impl acp::Agent for MvpAgent {
    /// In the meta, we provide
    ///   - model_state: the model state, useful for the client to display available models and the default model.
    ///
    /// SINGLE-CALL INVARIANT: this method is the sole writer of
    /// `self.auth_method_id` during initialization. It is called exactly once
    /// per agent process by the ACP server before any session-creating
    /// requests, while `auth_method_id` is still `None` (initialized at
    /// `MvpAgent::new`). The auth-method block below relies on that
    /// invariant when it unconditionally writes the default id returned by
    /// `auth_method::build_auth_methods`. If you ever need to call
    /// `initialize()` more than once, restore an `is_none()` guard around
    /// the `auth_method_id` write at the call site so a re-init doesn't
    /// silently downgrade an api-key user to a session-token user.
    async fn initialize(
        &self,
        arguments: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        tracing::debug!(target: "sampling_log", "Received initialize request");
        xai_grok_telemetry::unified_log::info("agent initialized", None, None);
        if xai_grok_telemetry::startup::agent_owned().is_some() {
            xai_grok_telemetry::startup::clear();
        }
        self.start_subagent_coordinator();
        if self.cfg.borrow().remote_settings.is_none() {
            self.spawn_settings_reapply();
        }
        let (auto_gc_policy, run_auto_gc) = {
            let cfg = self.cfg.borrow();
            let has_remote = cfg.remote_settings.is_some();
            let run = has_remote || !crate::util::config::resolve_remote_fetch_enabled();
            (cfg.resolve_worktree_auto_gc(), run)
        };
        if !run_auto_gc {
            tracing::debug!(
                "auto worktree gc deferred until remote_settings are available"
            );
        }
        tokio::task::spawn_blocking(move || {
            crate::session::worktree_pool::cleanup_stale_pool_worktrees(None);
            if !run_auto_gc {
                return;
            }
            let opts = xai_fast_worktree::AutoGcOptions::from_resolved(auto_gc_policy);
            if let Err(e) = xai_fast_worktree::WorktreeDb::open_default()
                .and_then(|db| xai_fast_worktree::maybe_auto_gc(&db, &opts))
            {
                tracing::warn!(error = %e, "auto worktree gc failed");
            }
        });
        tokio::task::spawn_blocking(|| {
            crate::session::persistence::cleanup_stale_sessions(None);
        });
        {
            let root = crate::util::grok_home::grok_home();
            crate::session::storage::search::SEARCH_INDEX_MANAGER.bootstrap_once(root);
        }
        const PERMISSION_CLEANUP_TTL_DAYS: u64 = 30;
        static CLEANUP_PERMISSIONS_ONCE: std::sync::Once = std::sync::Once::new();
        CLEANUP_PERMISSIONS_ONCE
            .call_once(|| {
                tokio::task::spawn(
                    xai_grok_workspace::permission::cleanup_stale_permission_state(
                        std::time::Duration::from_secs(
                            PERMISSION_CLEANUP_TTL_DAYS * 24 * 60 * 60,
                        ),
                    ),
                );
            });
        xai_grok_workspace::trust::migrate_legacy_hook_trust();
        if let Some(auth) = self.auth_manager.current() {
            let user_id = auth.user_id.trim();
            let needs_user_info = user_id.is_empty()
                || user_id.eq_ignore_ascii_case("unknown");
            xai_grok_telemetry::unified_log::info(
                "auth init user_info check",
                None,
                Some(
                    serde_json::json!({
                    "user_id_present": !user_id.is_empty(),
                    "needs_user_info": needs_user_info,
                    "access_token_present": !auth.key.is_empty(),
                    "refresh_token_present": auth.refresh_token.is_some(),
                }),
                ),
            );
            if needs_user_info && let Err(e) = self.auth_manager.update(auth).await {
                tracing::warn!(
                    "Failed to refresh user info from proxy during new_session: {}",
                    e
                );
            }
        }
        if !self.tier_allowed.get() && let Some(auth) = self.auth_manager.current() {
            self.enforce_grok_code_access(&auth).await;
        }
        self.maybe_sync_bundle_in_background(false);
        let mut client_type = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientType"))
            .and_then(|v| serde_json::from_value::<ClientType>(v.clone()).ok())
            .unwrap_or_default();
        let client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(ref id) = client_identifier {
            tracing::info!("Client identifier set to: {}", id);
        }
        if client_type == ClientType::Generic {
            match client_identifier.as_deref() {
                Some("grok-web") => client_type = ClientType::GrokWeb,
                Some("nebula") => client_type = ClientType::Nebula,
                Some("grok-code-extension") => client_type = ClientType::Extension,
                Some("grok-desktop") => client_type = ClientType::Desktop,
                _ => {}
            }
        }
        *self.client_type.borrow_mut() = client_type;
        tracing::info!("Client type set to: {:?}", client_type);
        let code_nav_enabled = Self::parse_code_nav_capability(&arguments);
        self.code_nav_enabled.set(code_nav_enabled);
        tracing::info!(
            code_nav_enabled,
            client_type = ?client_type,
            event = "code_nav_capability_parsed",
            "code-nav capability initialized from initialize request; \
             index will start lazily on first x.ai/code/* request if eligible"
        );
        let interactive_trust_client = Self::parse_interactive_trust_capability(
            &arguments,
        );
        self.interactive_trust_client.set(interactive_trust_client);
        let client_supports_mcp_apps = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("mcpApps"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if client_supports_mcp_apps {
            tracing::info!("Client supports MCP Apps");
        }
        let buffering_settings = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("bufferingSettings"))
            .map(|value| serde_json::from_value::<
                update_chunk_merge::BufferingSettings,
            >(value.clone()))
            .transpose()
            .map_err(|err| {
                tracing::warn!(
                    error = ?err,
                    "Failed to parse buffering settings from init meta"
                );
                err
            })
            .unwrap_or(None);
        tracing::info!(?buffering_settings, "Buffering settings from init");
        *self.buffering_settings.borrow_mut() = buffering_settings;
        if self.initialize_request.set(arguments).is_err() {
            tracing::info!("Initialize called on reconnect (already initialized)");
        }
        let pre = self.auth_manager.current();
        self.auth_manager.force_reload_from_disk();
        let post = self.auth_manager.current();
        xai_grok_telemetry::unified_log::info(
            "auth init disk refresh",
            None,
            Some(auth_init_disk_refresh_context(pre.as_ref(), post.as_ref())),
        );
        xai_grok_telemetry::unified_log::info(
            "auth: initialize() refreshed auth state from disk",
            None,
            Some(
                serde_json::json!({
                "has_current": self.auth_manager.current().is_some(),
                "is_expired": self.auth_manager.is_expired(),
                "auth_mode": self.auth_manager.current().map(|a| format!("{:?}", a.auth_mode)),
            }),
            ),
        );
        if !self.cfg.borrow().grok_com_config.api_key_auth_disabled()
            && auth_method::read_xai_api_key_env().is_err()
            && let Some(api_key) = crate::auth::read_api_key(
                &crate::util::grok_home::grok_home(),
            )
        {
            unsafe { std::env::set_var("XAI_API_KEY", &api_key) };
            tracing::info!("auth: loaded API key from auth.json (xai::api_key scope)");
            xai_grok_telemetry::unified_log::info(
                "auth: loaded API key from auth.json (xai::api_key scope)",
                None,
                None,
            );
        }
        let disable_api_key_auth = self
            .cfg
            .borrow()
            .grok_com_config
            .api_key_auth_disabled();
        {
            let cfg = self.cfg.borrow();
            let gc = &cfg.grok_com_config;
            if disable_api_key_auth || gc.force_login_team_uuid.is_some() {
                xai_grok_telemetry::unified_log::info(
                    "auth: enterprise login policy active",
                    None,
                    Some(
                        serde_json::json!({
                        "force_login_team_uuid": gc.force_login_team_uuid.as_ref().map(|t| format!("{t:?}")),
                        "disable_api_key_auth_knob": gc.disable_api_key_auth,
                        "api_key_auth_disabled": disable_api_key_auth,
                    }),
                    ),
                );
            }
        }
        let preferred_method_early = self.cfg.borrow().grok_com_config.preferred_method;
        let xai_api_base_url = self.cfg.borrow().endpoints.xai_api_base_url.clone();
        let has_byok = self
            .models_manager
            .models()
            .values()
            .any(|model| model.has_own_credentials() && !model.is_openai_codex_profile());
        let first_party_env_ok = if crate::auth::should_probe_first_party_env_key(
            disable_api_key_auth,
            has_byok,
            auth_method::has_xai_api_key_env(),
            preferred_method_early.is_some(),
        ) {
            crate::auth::first_party_env_key_allows_advertise(
                    &xai_api_base_url,
                    crate::auth::DEFAULT_PROBE_TIMEOUT,
                )
                .await
        } else {
            true
        };
        self.models_manager
            .apply_first_party_env_api_key_probe_result(first_party_env_ok);
        let has_external_api_key = auth_method::should_advertise_xai_api_key_with_env_ok(
            disable_api_key_auth,
            self.models_manager.models().values(),
            first_party_env_ok,
        );
        let init_has_current = self.auth_manager.current().is_some();
        let init_is_expired = self.auth_manager.is_expired();
        xai_grok_telemetry::unified_log::info(
            "auth init token state",
            None,
            Some(
                serde_json::json!({
                "has_current": init_has_current,
                "is_expired": init_is_expired,
            }),
            ),
        );
        let mut has_cached_token = init_has_current;
        if !init_has_current && init_is_expired {
            has_cached_token = match self.auth_manager.silent_refresh().await {
                SilentRefresh::Renewed(_) => true,
                SilentRefresh::Failed(remedy) => remedy.is_self_healing(),
            };
        }
        let (
            login_label,
            has_auth_provider,
            has_enterprise_oidc,
            enterprise_oidc_issuer,
        ) = {
            let cfg = self.cfg.borrow();
            let issuer = cfg.grok_com_config.oidc.as_ref().map(|o| o.issuer.clone());
            (
                cfg.grok_com_config.auth_provider_label.clone(),
                has_advertised_auth_provider_command(&cfg.grok_com_config),
                cfg.grok_com_config.oidc.is_some(),
                issuer,
            )
        };
        if has_enterprise_oidc {
            let issuer = enterprise_oidc_issuer
                .as_deref()
                .expect(
                    "enterprise_oidc_issuer must be Some when has_enterprise_oidc is true",
                );
            tracing::info!(issuer_present = !issuer.is_empty(), "auth: advertising enterprise OIDC auth method");
            xai_grok_telemetry::unified_log::info(
                "auth: advertising enterprise OIDC auth method",
                None,
                Some(serde_json::json!({ "issuer_present": !issuer.is_empty() })),
            );
        } else {
            tracing::info!(
                label = ?login_label,
                has_auth_provider,
                "auth: advertising grok.com auth method",
            );
        }
        let preferred_method = preferred_method_early;
        let has_external_api_key = match preferred_method {
            Some(crate::auth::PreferredAuthMethod::Oidc) => false,
            _ => has_external_api_key,
        };
        let has_cached_token = match preferred_method {
            Some(crate::auth::PreferredAuthMethod::ApiKey) => false,
            _ => has_cached_token,
        };
        let selected_model_is_no_auth = self
            .models_manager
            .models()
            .get(self.models_manager.current_model_id().0.as_ref())
            .map(|e| e.info.auth_scheme == xai_grok_sampler::AuthScheme::None)
            .unwrap_or(false);
        // Catalog-wide, unlike the selected-model check above: a Codex-only
        // user reaches their model with `/model` once the session is up. It
        // must therefore be a model `/model` can actually offer, not merely
        // one that exists in the catalog.
        let has_openai_codex_credential = self.models_manager.has_selectable_openai_codex_model();
        let built = auth_method::build_auth_methods(auth_method::AuthMethodsBuildInputs {
            has_external_api_key,
            has_cached_token,
            has_enterprise_oidc,
            enterprise_oidc_issuer: enterprise_oidc_issuer.as_deref(),
            login_label: login_label.as_deref(),
            has_auth_provider_command: has_auth_provider,
            preferred_method,
            selected_model_is_no_auth,
            has_openai_codex_credential,
        });
        let auth_methods = built.methods;
        xai_grok_telemetry::unified_log::info(
            "auth: initialize() built auth_methods for ACP response",
            None,
            Some(
                serde_json::json!({
                "grok_home": crate::util::grok_home::grok_home().display().to_string(),
                "HOME": std::env::var("HOME").unwrap_or_else(|_| "(unset)".into()),
                "has_external_api_key": has_external_api_key,
                "first_party_env_api_key_ok": first_party_env_ok,
                "disable_api_key_auth": disable_api_key_auth,
                "has_cached_token": has_cached_token,
                "has_enterprise_oidc": has_enterprise_oidc,
                "selected_model_is_no_auth": selected_model_is_no_auth,
                "has_openai_codex_credential": has_openai_codex_credential,
                "init_has_current": init_has_current,
                "init_is_expired": init_is_expired,
                "auth_mode": self.auth_manager.current().map(|a| format!("{:?}", a.auth_mode)),
                "methods": auth_methods.iter().map(|m| m.id().0.as_ref()).collect::<Vec<_>>(),
                "default_auth_method_id": built.default_auth_method_id.as_ref().map(|id| id.0.as_ref()),
            }),
            ),
        );
        debug_assert!(
            !has_external_api_key
                || selected_model_is_no_auth
                || matches!(
                    auth_methods
                        .first()
                        .map(|m| auth_method::AuthMethodKind::from_id(m.id())),
                    Some(auth_method::AuthMethodKind::XaiApiKey)
                ),
            "BYOK invariant violated: xai.api_key MUST be auth_methods.first() \
             when has_external_api_key is true (unless selected model is no-auth); got {:?}",
            auth_methods.first().map(|m| m.id()),
        );
        let default_auth_method_id_wire: Option<String> = built
            .default_auth_method_id
            .as_ref()
            .map(|id| id.0.to_string());
        if let Some(default_id) = built.default_auth_method_id {
            xai_grok_telemetry::unified_log::info(
                "auth method selection",
                None,
                Some(
                    serde_json::json!({
                    "default_auth_method_id": default_id.0.as_ref(),
                    "has_external_api_key": has_external_api_key,
                    "has_cached_token": has_cached_token,
                    "methods_first": auth_methods.first().map(|m| m.id().0.as_ref()),
                    "methods_count": auth_methods.len(),
                }),
                ),
            );
            self.set_auth_method(default_id);
        }
        self.sync_process_static_api_key(None);
        let current_working_directory = self.launch_cwd.clone();
        let hostname = gethostname::gethostname();
        let mcp_servers: Vec<crate::extensions::mcp::McpServerEntry> = Vec::new();
        let fetch_managed_mcps = self.cfg.borrow().managed_mcps_enabled
            && self.can_fetch_managed_mcps();
        if self.cfg.borrow().managed_mcps_enabled && !fetch_managed_mcps {
            tracing::info!("Managed MCP fetch: DISABLED");
        }
        self.spawn_initialize_launch_mcp_setup(fetch_managed_mcps);
        self.spawn_managed_gateway_tool_catalog_fetch();
        {
            let agent_ref = LocalRef::new(self);
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                agent_ref.get().emit_announcements(AnnouncementsPushMode::SeedNewClient);
            });
        }
        self.spawn_announcements_refresh();
        self.spawn_heap_profile_monitor();
        let init_model_state = if crate::agent::chat_modes::process_chat_mode_enabled() {
            self.chat_modes.model_state().await
        } else {
            self.model_state(None)
        };
        let session_capabilities = acp::SessionCapabilities::new()
            .close(acp::SessionCloseCapabilities::new());
        let session_capabilities = if crate::agent::chat_modes::process_chat_mode_enabled() {
            session_capabilities
        } else {
            session_capabilities
                    .list(acp::SessionListCapabilities::new())
                    .resume(acp::SessionResumeCapabilities::new())
        };
        Ok(
            acp::InitializeResponse::new(acp::ProtocolVersion::V1)
                .agent_capabilities(
                    acp::AgentCapabilities::new()
                        .load_session(true)
                        .meta(
                            serde_json::json!({
                    "x.ai/fs_notify": true,
                    // Advertised so SDKs can warn when a registration depends on
                    // hook behavior this agent doesn't honor.
                    "x.ai/hooks": {
                        "blockingEvents": crate::extensions::hooks::ADVERTISED_BLOCKING_EVENTS,
                        "decisions": crate::extensions::hooks::ADVERTISED_DECISIONS,
                        "stopSignals": crate::extensions::hooks::ADVERTISED_STOP_SIGNALS,
                    },
                    "x.ai/capabilities": {
                        "toolOverrides": tool_overrides_capability(),
                    },
                })
                                .as_object()
                                .cloned(),
                        )
                        .prompt_capabilities(
                            acp::PromptCapabilities::new().embedded_context(true),
                        )
                        .mcp_capabilities(
                            acp::McpCapabilities::new().http(true).sse(true),
                        )
                        .session_capabilities(session_capabilities),
                )
                .auth_methods(auth_methods)
                .meta({
                    let metadata = parse_json_object_env("GROK_AGENT_METADATA");
                    // #131. Only set when the configured default was not seated;
                    // a preference that was honoured — including one kept while
                    // unready — leaves this omitted rather than sent as null.
                    // Republished (or cleared as null) on `x.ai/models/update`
                    // when the catalog self-corrects.
                    let mut init_meta = serde_json::json!({
                    "grokShell": true,
                    // Re-deriving this precedence client-side has regressed OIDC
                    // refresh, so clients consume the agent's choice from here.
                    "defaultAuthMethodId": default_auth_method_id_wire,
                    // The agent can drive in-process SDK MCP servers over the ACP reverse
                    // channel (`x.ai/mcp/sdk_call`); the SDK reads this to enable transport="acp".
                    (xai_grok_mcp::wire::MCP_SDK): true,
                    // `session/new` / `session/load` accept per-session plugin roots in
                    // `_meta.pluginDirs`; the SDKs gate `GrokOptions.plugins` on this.
                    (SESSION_PLUGIN_DIRS_CAPABILITY_KEY): true,
                    "currentWorkingDirectory": current_working_directory.to_string_lossy().to_string(),
                    "agentVersion": xai_grok_version::VERSION,
                    "agentId": agent_id(),
                    "agentInstanceId": agent_instance_id(),
                    "hostname": hostname.to_string_lossy().to_string(),
                    "modelState": init_model_state,
                    "mcpServers": mcp_servers,
                    "mcpApps": client_supports_mcp_apps,
                    "metadata": metadata,
                    "availableCommands": crate::session::slash_commands::builtin_commands(self.command_availability()),
                    "cancelRewind": self.cfg.borrow().resolve_cancel_rewind().value,
                    // Resolved session-recap state (remote settings / config / env;
                    // default ON). The client gates BOTH its automatic
                    // away-recap poll and the manual `/recap` on this so a
                    // disabled feature produces zero `x.ai/recap` traffic.
                    "sessionRecap": self.cfg.borrow().is_session_recap_enabled(),
                    "voiceMode": self.cfg.borrow().is_voice_mode_enabled(),
                })
                        .as_object()
                        .cloned();
                    if let Some(map) = init_meta.as_mut() {
                        // #131: top-level response `_meta` (sibling of
                        // `modelState`). `x.ai/models/update` carries the same
                        // key on `SessionModelState._meta` instead — see
                        // `ModelsManager::notify_models_updated`.
                        self.models_manager
                            .write_substituted_default_model_meta(map, false);
                    }
                    init_meta
                }),
        )
    }
    async fn authenticate(
        &self,
        arguments: acp::AuthenticateRequest,
    ) -> Result<AuthenticateResponse, acp::Error> {
        tracing::info!(method = %arguments.method_id.0, "auth: authenticate request");
        xai_grok_telemetry::unified_log::info(
            "auth started",
            None,
            Some(serde_json::json!({"method": arguments.method_id.0.as_ref()})),
        );
        if let Some(preferred) = self.cfg.borrow().grok_com_config.preferred_method {
            let kind = auth_method::AuthMethodKind::from_id(&arguments.method_id);
            let allowed = match preferred {
                crate::auth::PreferredAuthMethod::ApiKey => kind.is_api_key(),
                crate::auth::PreferredAuthMethod::Oidc => kind.is_session_based(),
            };
            if !allowed {
                let msg = match preferred {
                    crate::auth::PreferredAuthMethod::ApiKey => {
                        auth_method::PREFERRED_API_KEY_UNAVAILABLE
                    }
                    crate::auth::PreferredAuthMethod::Oidc => {
                        "preferred_method=oidc; API-key auth is not allowed."
                    }
                };
                emit_login_span(
                    false,
                    arguments.method_id.0.as_ref(),
                    None,
                    Some("preferred_method_mismatch"),
                );
                return Err(acp::Error::auth_required().data(msg));
            }
        }
        match arguments.method_id.0.as_ref() {
            auth_method::LOCAL_NONE_METHOD_ID => {
                // Keyless selected model: succeed without reading or storing a key.
                self.set_auth_method(arguments.method_id.clone());
                self.sync_process_static_api_key(None);
                self.ensure_telemetry_client();
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    self.chat_modes.warm_in_background();
                }
                emit_login_span(true, "local_none", None, None);
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: "local.none".to_string(),
                    user_id: None,
                });
                Ok(Default::default())
            }
            auth_method::XAI_API_KEY_METHOD_ID => {
                if self.cfg.borrow().grok_com_config.api_key_auth_disabled() {
                    emit_login_span(false, "api_key", None, Some("disabled_by_admin"));
                    return Err(
                        acp::Error::auth_required()
                            .data("API-key auth is disabled by your administrator."),
                    );
                }
                let mut sampling_config = self.sampling_config.borrow_mut();
                if sampling_config.api_key.is_none() {
                    if let Ok(api_key) = auth_method::read_xai_api_key_env() {
                        sampling_config.api_key = Some(api_key.clone());
                        if let Err(e) = crate::auth::store_api_key(
                            &crate::util::grok_home::grok_home(),
                            &api_key,
                        ) {
                            tracing::warn!("failed to persist API key to auth.json: {e}");
                            xai_grok_telemetry::unified_log::warn(
                                "failed to persist API key to auth.json",
                                None,
                                Some(serde_json::json!({ "error": e.to_string() })),
                            );
                        }
                    } else if !self
                        .models_manager
                        .models()
                        .values()
                        .any(|m| m.has_own_credentials())
                    {
                        emit_login_span(false, "api_key", None, Some("no_credentials"));
                        return Err(
                            acp::Error::auth_required()
                                .data(
                                    "Set XAI_API_KEY or add api_key/env_key to config.toml.",
                                ),
                        );
                    }
                }
                self.set_auth_method(arguments.method_id.clone());
                self.sync_process_static_api_key(None);
                self.ensure_telemetry_client();
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    self.chat_modes.warm_in_background();
                }
                emit_login_span(true, "api_key", None, None);
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: "api_key".to_string(),
                    user_id: None,
                });
                Ok(Default::default())
            }
            auth_method::CACHED_TOKEN_AUTH_METHOD_ID => {
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                if auth_meta.force_interactive {
                    return self
                        .authenticate(
                            acp::AuthenticateRequest::new(
                                    acp::AuthMethodId::new(auth_method::OIDC_METHOD_ID),
                                )
                                .meta(arguments.meta),
                        )
                        .await;
                }
                let current_auth = self.auth_manager.current();
                let has_current = current_auth.is_some();
                let is_expired = self.auth_manager.is_expired();
                let is_devbox = crate::auth::devbox_login::is_devbox_environment();
                let is_legacy = current_auth
                    .as_ref()
                    .is_some_and(|a| a.auth_mode == crate::auth::AuthMode::WebLogin);
                xai_grok_telemetry::unified_log::info(
                    "auth cached_token check",
                    None,
                    Some(
                        serde_json::json!({
                        "has_current": has_current,
                        "is_expired": is_expired,
                        "is_devbox": is_devbox,
                        "is_legacy": is_legacy,
                    }),
                    ),
                );
                let pin_blocks_oidc_mint = matches!(
                    self.cfg.borrow().grok_com_config.preferred_method,
                    Some(crate::auth::PreferredAuthMethod::ApiKey)
                );
                if is_devbox && is_legacy && !pin_blocks_oidc_mint {
                    xai_grok_telemetry::unified_log::info(
                        "auth cached_token: devbox legacy migration starting",
                        None,
                        None,
                    );
                    match crate::auth::devbox_login::mint_devbox_auth(&self.auth_manager)
                        .await
                    {
                        Ok(new_auth) => {
                            match self
                                .auth_manager
                                .save_without_enrichment(new_auth)
                                .await
                            {
                                Ok(_) => {
                                    if let Err(e) = self
                                        .auth_manager
                                        .remove_scope(crate::auth::LEGACY_AUTH_SCOPE)
                                    {
                                        tracing::warn!(error = ?e, "auth: failed to remove legacy scope (non-fatal)");
                                    }
                                    xai_grok_telemetry::unified_log::info(
                                        "auth cached_token: devbox legacy migration succeeded",
                                        None,
                                        None,
                                    );
                                }
                                Err(e) => {
                                    xai_grok_telemetry::unified_log::warn(
                                        "auth cached_token: devbox migration save failed",
                                        None,
                                        Some(serde_json::json!({ "error": e.to_string() })),
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            xai_grok_telemetry::unified_log::warn(
                                "auth cached_token: devbox mint failed, will reject legacy token",
                                None,
                                Some(serde_json::json!({ "error": format!("{e}") })),
                            );
                        }
                    }
                }
                let resolved = match self.auth_manager.current() {
                    Some(auth) => Some(auth),
                    None if !self.auth_manager.is_expired() => None,
                    None => {
                        match self.auth_manager.silent_refresh().await {
                            SilentRefresh::Renewed(auth) => Some(*auth),
                            SilentRefresh::Failed(remedy) if remedy.is_self_healing() => {
                                self.auth_manager.current_or_expired()
                            }
                            SilentRefresh::Failed(_) => None,
                        }
                    }
                };
                let Some(auth) = resolved else {
                    let message = if self.auth_manager.is_expired() {
                        "Session expired, re-authentication required"
                    } else {
                        "No cached auth token found"
                    };
                    tracing::info!(%message, "cached_token missing/expired, falling through");
                    xai_grok_telemetry::unified_log::warn(
                        "auth cached_token fallthrough",
                        None,
                        Some(serde_json::json!({ "reason": message })),
                    );
                    return self
                        .authenticate_after_cached_token_unavailable(arguments)
                        .await;
                };
                if auth.auth_mode == crate::auth::AuthMode::WebLogin {
                    tracing::info!("auth: rejecting legacy WebLogin token");
                    xai_grok_telemetry::unified_log::warn(
                        "auth cached_token legacy rejected",
                        None,
                        Some(
                            serde_json::json!({ "auth_mode": format!("{:?}", auth.auth_mode) }),
                        ),
                    );
                    self.auth_manager.clear_in_memory();
                    if let Err(e) = self
                        .auth_manager
                        .remove_scope(crate::auth::LEGACY_AUTH_SCOPE)
                    {
                        tracing::warn!(error = ?e, "auth: failed to remove legacy scope during WebLogin rejection (non-fatal)");
                    }
                    return self
                        .authenticate_after_cached_token_unavailable(arguments)
                        .await;
                }
                self.enforce_grok_code_access(&auth).await;
                self.maybe_sync_bundle_in_background(false);
                let auth_for_settings = auth.clone();
                {
                    let mut sampling_config = self.sampling_config.borrow_mut();
                    sampling_config.api_key = Some(auth.key);
                    tracing::debug!("auth: cached_token handler set api_key (SessionToken)");
                    xai_grok_telemetry::unified_log::debug(
                        "auth: cached_token handler set api_key (SessionToken)",
                        None,
                        None,
                    );
                }
                self.set_auth_method(arguments.method_id.clone());
                self.ensure_telemetry_client();
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    self.chat_modes.warm_in_background();
                }
                let uid = self.auth_manager.current().map(|a| a.user_id);
                emit_login_span(true, "cached_token", uid.as_deref(), None);
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: "cached_token".to_string(),
                    user_id: uid,
                });
                self.spawn_post_auth_settings(auth_for_settings);
                Ok(self.auth_response_with_meta())
            }
            auth_method::GROK_COM_METHOD_ID | auth_method::OIDC_METHOD_ID => {
                let grok_ctx = self.auth_manager.grok_com_config();
                let auth_meta = AuthRequestMeta::from_json(arguments.meta.as_ref());
                tracing::info!(
                    method = arguments.method_id.0.as_ref(),
                    headless = auth_meta.headless,
                    reauth = auth_meta.reauth,
                    use_oauth = auth_meta.use_oauth,
                    "auth: inline auth flow",
                );
                xai_grok_telemetry::unified_log::info(
                    "auth: inline auth flow",
                    None,
                    Some(
                        serde_json::json!({
                        "method": arguments.method_id.0.as_ref(),
                        "headless": auth_meta.headless,
                        "reauth": auth_meta.reauth,
                        "use_oauth": auth_meta.use_oauth,
                    }),
                    ),
                );
                if auth_meta.reauth {
                    let _ = self.auth_manager.clear();
                }
                let cli_oauth = auth_meta.use_oauth.then_some(true);
                let use_oidc = self.cfg.borrow().resolve_grok_oauth(cli_oauth);
                tracing::debug!(resolved = use_oidc.value, source = ?use_oidc.source, "auth: method resolved");
                xai_grok_telemetry::unified_log::debug(
                    "auth: method resolved",
                    None,
                    Some(
                        serde_json::json!({
                        "use_oidc": use_oidc.value,
                        "source": format!("{:?}", use_oidc.source),
                    }),
                    ),
                );
                let login_override = auth_meta.login_override();
                let mut cancelled = false;
                let client_seq = auth_meta.request_seq;
                let auth_result = if !auth_meta.headless {
                    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
                    let (code_tx, code_rx) = tokio::sync::mpsc::channel(1);
                    let (cancel, _guard) = self
                        .interactive_auth
                        .begin(
                            Some(
                                crate::auth::single_flight::AttemptChannels::new(
                                    code_tx,
                                    url_rx,
                                ),
                            ),
                            client_seq,
                        );
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = crate::auth::run_auth_flow_with_stderr_bridge(
                            &self.auth_manager,
                            grok_ctx,
                            crate::auth::AuthChannels {
                                url_tx: Some(url_tx),
                                code_rx,
                            },
                            auth_meta.reauth,
                            auth_meta.force_interactive,
                            login_override,
                        ) => r,
                    }
                } else {
                    let (cancel, _guard) = self.interactive_auth.begin(None, client_seq);
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            Err(anyhow::anyhow!("Authentication cancelled"))
                        }
                        r = crate::auth::run_auth_flow(
                            &self.auth_manager,
                            grok_ctx,
                            auth_meta.reauth,
                            None,
                            None,
                            None,
                            login_override,
                        ) => r,
                    }
                };
                let (auth, _did_auth) = auth_result
                    .map_err(|e| {
                        emit_login_span(
                            false,
                            arguments.method_id.0.as_ref(),
                            None,
                            Some(
                                if cancelled {
                                    "login_cancelled"
                                } else {
                                    "login_flow_failed"
                                },
                            ),
                        );
                        let mut err = acp::Error::auth_required();
                        err.message = e.to_string();
                        err
                    })?;
                {
                    let mut sampling_config = self.sampling_config.borrow_mut();
                    sampling_config.api_key = Some(auth.key.clone());
                    tracing::debug!("auth: grok.com/oidc handler set api_key (SessionToken)");
                    xai_grok_telemetry::unified_log::debug(
                        "auth: grok.com/oidc handler set api_key (SessionToken)",
                        None,
                        None,
                    );
                }
                self.auth_manager.hot_swap(auth.clone());
                self.enforce_grok_code_access(&auth).await;
                self.maybe_sync_bundle_in_background(false);
                tokio::task::spawn_local(
                    crate::managed_config::post_login_sync(Some(auth.clone())),
                );
                self.set_auth_method(arguments.method_id.clone());
                self.models_manager.on_auth_changed().await;
                if crate::agent::chat_modes::process_chat_mode_enabled() {
                    self.chat_modes.warm_in_background();
                }
                emit_login_span(
                    true,
                    arguments.method_id.0.as_ref(),
                    Some(auth.user_id.as_str()),
                    None,
                );
                log_event(xai_grok_telemetry::events::Login {
                    auth_method: arguments.method_id.0.as_ref().to_string(),
                    user_id: Some(auth.user_id.clone()),
                });
                self.spawn_post_auth_settings(auth);
                Ok(self.auth_response_with_meta())
            }
            _ => {
                Err(
                    acp::Error::invalid_params()
                        .data(
                            format!(
                "unsupported auth method: {}",
                arguments.method_id.0
            ),
                        ),
                )
            }
        }
    }
    async fn new_session(
        &self,
        arguments: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        reject_chat_kind_without_feature(arguments.meta.as_ref())?;
        tracing::debug!(
            mcp_server_count = arguments.mcp_servers.len(),
            meta_present = arguments.meta.is_some(),
            "received new session request"
        );
        let init = self
            .initialize_request
            .get()
            .ok_or_else(|| {
                acp::Error::invalid_params()
                    .data("initialize must be called before new_session")
            })?;
        self.seed_client_config_auth_if_available();
        self.spawn_settings_reapply();
        let cwd = AbsPathBuf::new(arguments.cwd.clone())
            .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        folder_trust::resolve_and_record(cwd.as_path(), remote_settings.as_ref(), false);
        let client_session_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("sessionId"))
            .and_then(|v| v.as_str());
        let session_id = match client_session_id {
            Some(s) => {
                uuid::Uuid::try_parse(s).map_err(|e| {
                    acp::Error::invalid_params().data(format!(
                        "Invalid UUID format for _meta.sessionId '{}': {}",
                        s, e
                    ))
                })?;
                acp::SessionId::new(s.to_string())
            }
            None => acp::SessionId::new(uuid::Uuid::now_v7().to_string()),
        };
        // Claim before the first request-specific await.  This prevents two
        // concurrent `/new` calls from both reaching persistence with the same
        // caller-supplied id, and the RAII guard can only settle its own claim.
        let new_session_claim = self.begin_new_session_claim(&session_id)?;
        if crate::session::persistence::find_persisted_session_dir_by_id_result(
            session_id.0.as_ref(),
        )
        .map_err(|error| {
            acp::Error::internal_error()
                .data(format!("Failed to check requested sessionId: {error}"))
        })?
        .is_some()
        {
            return Err(acp::Error::invalid_params()
                .data("A persisted session with the requested sessionId already exists"));
        }
        let (initial_client_mcp_servers, mcp_servers, managed_mcp_expires_at) = self
            .resolve_mcp_servers(arguments.mcp_servers, cwd.as_path())
            .await;
        let mcp_meta_config_map = parse_mcp_meta_config(arguments.meta.as_ref());
        let custom_model_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("modelId").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty());
        #[allow(unused_mut)]
        let mut session_meta_for_stamp = arguments.meta.clone();
        #[cfg(all(feature = "local-workspace", unix))]
        let pending_local_workspace = self
            .start_own_local_workspace_if_needed(
                &mut session_meta_for_stamp,
                cwd.as_path(),
            )
            .await?;
        #[cfg(all(feature = "local-workspace", unix))]
        let mut pending_local_workspace =
            PendingLocalWorkspaceGuard::new(pending_local_workspace);
        #[cfg(all(feature = "local-workspace", not(unix)))]
        {
            use crate::gateway_bridge::local_workspace_supervisor::parse_local_workspace_intent;
            use crate::gateway_bridge::local_workspace_supervisor::LocalWorkspaceIntent;
            use crate::gateway_bridge::local_workspace_supervisor::SupervisorError;
            if matches!(
                parse_local_workspace_intent(session_meta_for_stamp.as_ref()),
                Some(LocalWorkspaceIntent::Own { .. })
            ) {
                return Err(SupervisorError::UnsupportedPlatform.into_acp_error());
            }
        }
        #[allow(unused_variables)]
        let session_computer_sessions = resolve_session_computer_sessions(
            arguments.meta.as_ref(),
        )?;
        let is_chat_kind = wants_chat_session_kind(arguments.meta.as_ref());
        let session_yolo_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("yoloMode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(self.default_yolo_mode);
        let session_auto_mode = resolve_session_auto_mode(
            arguments.meta.as_ref(),
            self.default_auto_mode,
            session_yolo_mode,
        );
        let mut session_timer = crate::instrumentation_timer!("session.new_session");
        let client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                self
                    .initialize_request
                    .get()
                    .and_then(|req| req.meta.as_ref())
                    .and_then(|m| m.get("clientIdentifier"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });
        let session_info = SessionInfo {
            id: session_id.clone(),
            cwd: cwd.as_str().to_owned(),
        };
        let session_initial_model = chat_initial_model(is_chat_kind, custom_model_id);
        let origin_client = self.origin_client_info_from_meta(arguments.meta.as_ref());
        let prepared_model_plan = if is_chat_kind {
            None
        } else {
            Some(self.prepare_new_session_model_plan(
                custom_model_id,
                origin_client.clone(),
            )?)
        };
        let model_agent_type = prepared_model_plan
            .as_ref()
            .and_then(|plan| plan.model_agent_type.clone());
        let disallowed_custom = prepared_model_plan
            .as_ref()
            .and_then(|plan| plan.disallowed_custom.clone());
        let auth_hidden_custom = prepared_model_plan
            .as_ref()
            .and_then(|plan| plan.auth_hidden_custom.clone());
        let unreadiness_custom = prepared_model_plan
            .as_ref()
            .and_then(|plan| plan.unreadiness_custom.clone());
        let publication_gate = prepared_model_plan
            .as_ref()
            .map(|_| crate::session::SessionPublicationGate::pending());
        let fallback_model_id = prepared_model_plan
            .as_ref()
            .map(|plan| plan.session_model_id.clone())
            .unwrap_or_else(|| self.models_manager.current_model_id());
        // An exact model-declared harness is a prerequisite, not a best-effort
        // hint. Stock non-strict harnesses intentionally allow an explicit
        // agent profile (including its own ready model pin) to keep its prompt
        // and tool configuration. Validate strict, plugin/file-backed, and
        // unresolved harnesses before persistence or actor creation.
        if let Some(required_agent_type) = model_agent_type.as_deref() {
            let plugin_registry = self.plugin_registry_handle.snapshot();
            let selected_agent = {
                let cfg = self.cfg.borrow();
                Self::resolve_agent_definition_with_plugins(
                    cwd.as_path(),
                    cfg.agent_profile_path.as_deref(),
                    &cfg.agent,
                    parse_agent_profile_from_meta(arguments.meta.as_ref()),
                    Some(required_agent_type),
                    plugin_registry.as_deref(),
                )
            };
            let required_definition =
                xai_grok_agent::discovery::by_name_in_cwd_with_plugins(
                    required_agent_type,
                    cwd.as_path(),
                    plugin_registry.as_deref(),
                );
            let requires_exact_harness = required_definition
                .as_ref()
                .is_none_or(definition_requires_exact_harness);
            if requires_exact_harness && !harnesses_are_compatible(
                &selected_agent,
                required_agent_type,
                required_definition.as_ref(),
            ) {
                let requested_model = fallback_model_id.0.to_string();
                return Err(crate::agent::config::ModelSwitchHarnessError {
                    code: crate::agent::config::MODEL_SWITCH_REBUILD_FAILED.to_owned(),
                    active_agent_type: selected_agent.name,
                    required_agent_type: required_agent_type.to_owned(),
                    model_id: requested_model,
                    reason: if required_definition.is_some() {
                        "incompatible_agent".to_owned()
                    } else {
                        "agent_definition_unresolved".to_owned()
                    },
                }
                .into_acp_error());
            }
        }
        let session_sampling = prepared_model_plan
            .as_ref()
            .map(|plan| plan.sampling_config.clone())
            .unwrap_or_else(|| {
                self.resolve_sampling_config_for_model(
                    &fallback_model_id,
                    origin_client.clone(),
                )
            });
        let (summary_client, summary_model) = self
            .build_summary_client(&session_sampling)
            .await?;
        let relay_sync = publication_gate.as_ref().and_then(|gate| {
            self.create_deferred_relay_sync(
                &session_id.0,
                &session_info,
                gate.clone(),
            )
        });
        let deferred_relay_state_rx = relay_sync
            .as_ref()
            .map(crate::relay::RelaySync::subscribe_state);
        let model_id = session_initial_model
            .as_ref()
            .map(|chat_model| acp::ModelId::new(chat_model.clone()))
            .unwrap_or_else(|| fallback_model_id.clone());
        let session_model_id = model_id.clone();
        let requested_storage_mode = self.storage_mode.get();
        let persistence = if is_chat_kind {
            crate::session::persistence::PersistenceHandle::noop()
        } else {
            let _timer = crate::instrumentation_timer!("session.persistence_init");
            let registry_title_sync = self
                .session_registry_client()
                .map(|client| crate::session::persistence::RegistryGeneratedTitleSync {
                    client,
                    suppress_for_zdr: self
                        .auth_manager
                        .current_or_expired()
                        .is_some_and(|a| a.is_zdr_team()),
                });
            crate::session::persistence::new(
                    &session_info,
                    model_id,
                    summary_client,
                    StorageMode::Local,
                    Some(self.auth_manager.clone()),
                    relay_sync,
                    Some(self.gateway.clone()),
                    summary_model,
                    registry_title_sync,
                )
                .await
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        acp::Error::invalid_params().data(
                            "A persisted session with the requested sessionId already exists",
                        )
                    } else {
                        crate::session::persistence::io_error_to_acp(&error)
                    }
                })?
        };
        let chat_history = vec![];
        let client_code_nav_enabled = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("codeNavEnabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| self.code_nav_enabled.get());
        let (client_terminal, client_fs_read, client_fs_write) = Self::resolve_client_io_caps(
            arguments.meta.as_ref(),
            init,
        );
        let spawn_res = {
            // Keep provisional identity out of timing telemetry. The outer
            // session timer receives session_id/cwd only after publication.
            let _timer = crate::instrumentation_timer!("session.spawn_session_actor");
            let spawn_opts = if is_chat_kind {
                chat_session_spawn_options(
                    session_info.clone(),
                    cwd.clone(),
                    session_meta_for_stamp.as_ref(),
                    model_agent_type.as_deref(),
                    session_model_id.clone(),
                    session_yolo_mode,
                )
            } else {
                SessionSpawnOptions {
                        session_info: session_info.clone(),
                        cwd: cwd.clone(),
                        mcp_servers,
                        initial_client_mcp_servers,
                        mcp_meta_config_map,
                        persistence,
                        chat_history,
                        rewind_points_file_path: None,
                        initial_total_tokens: 0,
                        origin_client: origin_client.clone(),
                        client_code_nav_enabled,
                        client_terminal,
                        client_fs_read,
                        client_fs_write,
                        preloaded_envrc: None,
                        persisted_signals: None,
                        persisted_plan_mode: None,
                        persisted_goal_mode: None,
                        persisted_workflow_runs: Vec::new(),
                        persisted_announcement_state: None,
                        session_meta: session_meta_for_stamp.as_ref(),
                        managed_mcp_expires_at,
                        model_agent_type: model_agent_type.as_deref(),
                        prepared_sampling_config: prepared_model_plan
                            .as_ref()
                            .map(|plan| plan.sampling_config.clone()),
                        prepared_catalog_identity: prepared_model_plan
                            .as_ref()
                            .map(|plan| plan.catalog_identity.clone()),
                        prepared_model_entry: prepared_model_plan
                            .as_ref()
                            .map(|plan| plan.model_entry.clone()),
                        new_session_auth_authority: prepared_model_plan
                            .as_ref()
                            .map(|plan| plan.auth_authority.clone()),
                        publication_gate,
                        deferred_relay_state_rx,
                        upgrade_persistence_to_writeback: requested_storage_mode
                            == StorageMode::Writeback,
                        persisted_catalog_identity: None,
                        session_model_id: session_model_id.clone(),
                        session_yolo_mode,
                        session_auto_mode: session_auto_mode && !session_yolo_mode,
                        prompt_display_cwd: None,
                        is_chat_kind: false,
                    }
            };
            self.spawn_and_register_session(init, spawn_opts).await
        };
        #[cfg(all(feature = "local-workspace", unix))]
        if spawn_res.is_err() {
            self.shutdown_gateway_bridge(&session_id);
        }
        let (spawned_session_model_id, mut prepared_session) = match spawn_res? {
            SpawnedSession::Committed(model_id) => (model_id, None),
            SpawnedSession::Prepared(prepared) => (prepared.model_id().clone(), Some(prepared)),
        };
        let bridge_attach = BridgeAttach::NotAttached;
        let indexed_roots = self.indexed_roots_for(cwd.as_path());
        let (git_root, is_git_repo, discovery_failed) = match xai_grok_workspace::session::git::discover_git_root(
            cwd.as_path(),
        ) {
            GitDiscoveryResult::Found(root) => {
                let root_str = root.to_string_lossy().trim_end_matches('/').to_string();
                (Some(root_str), true, false)
            }
            GitDiscoveryResult::NotARepo => {
                tracing::debug!("new_session: not a git repository");
                (None, false, false)
            }
            GitDiscoveryResult::DiscoveryFailed(e) => {
                tracing::warn!(
                        error = %e,
                        "new_session: git repo discovery failed unexpectedly"
                    );
                (None, false, true)
            }
        };
        let (show_non_git_warning, feedback_enabled) = {
            let cfg = self.cfg.borrow();
            let show_non_git_warning = !is_git_repo && !discovery_failed
                && cfg
                    .remote_settings
                    .as_ref()
                    .and_then(|s| s.non_git_warning)
                    .unwrap_or(cfg.features.non_git_warning);
            let feedback_enabled = cfg.is_feedback_enabled();
            (show_non_git_warning, feedback_enabled)
        };
        let (models, model_presentation) = if is_chat_kind {
            (
                chat_new_session_model_state(
                    self.chat_modes.model_state().await,
                    session_initial_model
                        .filter(|_| matches!(bridge_attach, BridgeAttach::Spawned)),
                ),
                self.models_manager.presentation_snapshot(),
            )
        } else if let Some(prepared) = prepared_session.as_deref() {
            self.prepared_model_state_with_presentation(prepared)
        } else {
            self.model_state_with_presentation(Some(&session_id))
        };
        // `/new` still owns the provisional claim through response assembly so
        // racing session-scoped requests cannot observe a session before its
        // creation response. Use the owner-bound lookup rather than waiting on
        // our own marker, which would self-deadlock until this future returns.
        let applied_tool_overrides = if let Some(prepared) = prepared_session.as_deref() {
            prepared
                .handle()
                .resolved_tool_overrides
                .load_full()
                .map(|overrides| (*overrides).clone())
        } else {
            match self.session_handle_during_load(&session_id, &new_session_claim) {
                Some(handle) => read_applied_tool_overrides(&handle.cmd_tx).await,
                None => {
                tracing::warn!(
                    session_id = %session_id.0,
                    "session/new toolOverrides echo: session handle not found"
                );
                None
                }
            }
        };
        let mut meta = serde_json::json!({
            "currentWorkingDirectory": cwd.as_str().to_owned(),
            "codebaseIndexed": indexed_roots,
            "isGitRepo": is_git_repo,
            "gitRoot": git_root,
            "showNonGitWarning": show_non_git_warning,
            "feedbackEnabled": feedback_enabled,
        });
        if let Some(obj) = meta.as_object_mut() {
            if let Some(prepared) = prepared_session.as_deref() {
                self.insert_prepared_session_config_meta(
                    obj,
                    prepared,
                    cwd.as_str().to_owned(),
                    &models,
                    &model_presentation,
                );
            } else {
                self.insert_session_config_meta_with_presentation(
                    obj,
                    &session_id,
                    cwd.as_str().to_owned(),
                    None,
                    &models,
                    &model_presentation,
                );
            }
            insert_applied_tool_overrides(obj, applied_tool_overrides.as_ref());
        }
        let response = acp::NewSessionResponse::new(session_id.clone())
            .models(Some(models))
            .meta(meta.as_object().cloned());
        let response = if let Some(prepared) = prepared_session.take() {
            match self.commit_prepared_new_session(prepared, response) {
                Ok(response) => response,
                Err((error, prepared)) => {
                    self.abort_prepared_new_session(prepared).await?;
                    return Err(error);
                }
            }
        } else {
            response
        };

        // Publication above is the final state transition. Everything below is
        // synchronous or detached; there is no cancellation point before the
        // already-built response is returned.
        #[cfg(all(feature = "local-workspace", unix))]
        if let Some(handle) = pending_local_workspace.take() {
            self.register_local_workspace_supervisor(session_id.clone(), handle);
        }
        session_timer.with_field("session_id", session_id.0.as_ref());
        session_timer.with_field("cwd", cwd.as_str());
        xai_grok_telemetry::unified_log::info(
            "session created",
            Some(session_id.0.as_ref()),
            Some(serde_json::json!({"cwd": cwd.as_str()})),
        );
        self.set_turn_number(&session_id, 0u64);
        xai_grok_telemetry::session_ctx::log_session_event(
            crate::agent::session_metrics::SessionStarted {
                session_id: session_id.0.to_string(),
            },
        );
        tracing::debug!(session_id = %session_id.0, "new_session: published session actor");
        #[cfg(feature = "local-workspace")]
        if local_workspace_intent_present(arguments.meta.as_ref()) {
            self.mark_local_workspace_bound(session_id.clone());
        }
        self.maybe_spawn_interactive_trust_prompt(
            &session_id,
            cwd.as_path(),
            remote_settings.as_ref(),
        );
        let product_analytics = self.product_analytics_enabled();
        if product_analytics || xai_grok_telemetry::external::is_active() {
            let sid = session_id.0.to_string();
            let ci = client_identifier;
            let cv = self.client_version();
            let cwd_str = cwd.as_str().to_owned();
            let perm = if session_yolo_mode {
                xai_grok_telemetry::enums::PermissionMode::AlwaysApprove
            } else if session_auto_mode
                && crate::util::config::auto_permission_mode_enabled_from_disk()
            {
                xai_grok_telemetry::enums::PermissionMode::Auto
            } else {
                xai_grok_telemetry::enums::PermissionMode::Ask
            };
            tokio::spawn(async move {
                let git = xai_grok_telemetry::context::collect_git_context(&cwd_str);
                let ev = xai_grok_telemetry::events::SessionNew {
                    session_id: sid,
                    client_identifier: ci,
                    client_version: cv,
                    is_git_repo: git.is_git_repo,
                    permission_mode: perm,
                };
                xai_grok_telemetry::session_ctx::log_event_dual(product_analytics, ev);
            });
        }
        let auto_switch = disallowed_custom
            .map(|requested| {
                let reason = format!(
                    "\"{requested}\" isn't allowed by your allowed_models setting, so this session is using \"{}\".",
                    spawned_session_model_id.0
                );
                (requested, reason)
            })
            .or_else(|| auth_hidden_custom.map(|requested| {
                let reason = format!(
                    "\"{requested}\" is unavailable for the current authentication mode, so this session is using \"{}\".",
                    spawned_session_model_id.0
                );
                (requested, reason)
            }))
            .or_else(|| unreadiness_custom.map(|(requested, readiness_reason)| {
                let reason = format!(
                    "\"{requested}\" isn't ready ({readiness_reason}), so this session is using \"{}\".",
                    spawned_session_model_id.0
                );
                (requested, reason)
            }));
        if let Some((requested, reason)) = auto_switch {
            let previous = acp::ModelId::new(requested);
            let current = spawned_session_model_id;
            let notify_session_id = session_id.clone();
            let notification = crate::extensions::notification::SessionNotification {
                session_id: notify_session_id,
                update: crate::extensions::notification::SessionUpdate::ModelAutoSwitched {
                    previous_model_id: previous.0.to_string(),
                    new_model_id: current.0.to_string(),
                    reason,
                },
                meta: None,
            };
            if let Ok(params) = serde_json::value::to_raw_value(&notification) {
                self.gateway.forward_fire_and_forget(acp::ExtNotification::new(
                    "x.ai/session_notification",
                    params.into(),
                ));
            }
        }
        Ok(response)
    }
    async fn load_session(
        &self,
        arguments: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let load_guard = self.begin_session_load(&arguments.session_id)?;
        reject_chat_kind_without_feature(arguments.meta.as_ref())?;
        self.sweep_dead_sessions();
        self.drain_old_session_thread(&arguments.session_id).await;
        tracing::debug!("Received load session request {arguments:?}");
        let init = self
            .initialize_request
            .get()
            .ok_or_else(|| {
                acp::Error::invalid_params()
                    .data("initialize must be called before load_session")
            })?;
        self.seed_client_config_auth_if_available();
        let persist_data = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/persist"))
            .cloned();
        let target_client_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("x.ai/leaderClientId"))
            .cloned();
        let acp::LoadSessionRequest {
            session_id,
            cwd,
            mcp_servers: client_mcp_servers,
            meta: request_meta,
            ..
        } = arguments;
        let cwd = AbsPathBuf::new(cwd)
            .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
        let remote_settings = self.cfg.borrow().remote_settings.clone();
        folder_trust::resolve_and_record(cwd.as_path(), remote_settings.as_ref(), false);
        let (initial_client_mcp_servers, mcp_servers, managed_mcp_expires_at) = self
            .resolve_mcp_servers(client_mcp_servers, cwd.as_path())
            .await;
        let mcp_meta_config_map = parse_mcp_meta_config(request_meta.as_ref());
        let mut load_timer = crate::instrumentation_timer!("session.load_session");
        load_timer.with_field("session_id", session_id.0.as_ref());
        load_timer.with_field("cwd", cwd.as_str());
        let git_root = xai_grok_workspace::session::git::find_git_root_from_path(
                cwd.as_path(),
            )
            .ok();
        if let Some(root) = git_root {
            tokio::task::spawn_blocking(move || {
                crate::session::worktree_pool::cleanup_stale_pool_worktrees(Some(&root));
            });
        }
        xai_grok_telemetry::session_ctx::log_session_event(crate::agent::session_metrics::SessionStarted {
            session_id: session_id.0.to_string(),
        });
        let session_info = SessionInfo {
            id: session_id.clone(),
            cwd: cwd.as_str().to_owned(),
        };
        let current_session_dir = crate::session::persistence::session_dir(
            &session_info,
        );
        tokio::task::spawn_blocking(move || {
            crate::session::persistence::cleanup_stale_sessions(
                Some(&current_session_dir),
            );
        });
        let session_exists = self.resident_handle(&session_id).is_some();
        let mut cold_spawn_selection = None;
        let mut ambiguous_persisted_slug_matches: Option<Vec<acp::ModelId>> = None;
        if session_exists {
            tracing::info!(
                session_id = %session_id.0,
                "Reconnect detected: flushing persistence buffer before replay"
            );
            if let Some(handle) = self.resident_handle(&session_id) {
                handle
                    .gateway_enabled
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
            let mut flush_timer = crate::instrumentation_timer!("session.reconnect_flush");
            flush_timer.with_field("session_id", session_id.0.as_ref());
            if let Err(reason) = self.flush_session(&session_id).await {
                tracing::warn!(
                    session_id = %session_id.0,
                    reason,
                    "Reconnect flush failed"
                );
            }
            drop(flush_timer);
        }
        let origin_client = self.origin_client_info_from_meta(request_meta.as_ref());
        let load_session_sampling = self
            .resolve_sampling_config_for_model(
                &self.models_manager.current_model_id(),
                origin_client.clone(),
            );
        let (summary_client, summary_model) = self
            .build_summary_client(&load_session_sampling)
            .await?;
        let relay_sync = if let Some(sync) = self
            .create_relay_sync(&session_id.0, &session_info)
        {
            Self::spawn_relay_state_forwarder(
                sync.subscribe_state(),
                sync.session_id().to_owned(),
                self.gateway.clone(),
            );
            Some(sync)
        } else {
            None
        };
        let mut persistence_timer = crate::instrumentation_timer!("session.load_light");
        persistence_timer.with_field("session_id", session_id.0.as_ref());
        let backend = if self.build_registry_config().is_some() {
            Some(
                crate::remote::BackendClient::new()
                    .with_auth_manager(self.auth_manager.clone()),
            )
        } else {
            None
        };
        let registry_title_sync = self
            .session_registry_client()
            .map(|client| crate::session::persistence::RegistryGeneratedTitleSync {
                client,
                suppress_for_zdr: self
                    .auth_manager
                    .current_or_expired()
                    .is_some_and(|a| a.is_zdr_team()),
            });
        let (persistence_info, persistence) = crate::session::persistence::load_light(
                &session_info,
                summary_client,
                self.storage_mode.get(),
                Some(self.auth_manager.clone()),
                backend.as_ref(),
                relay_sync,
                Some(self.gateway.clone()),
                summary_model,
                registry_title_sync,
            )
            .await
            .map_err(|e| crate::session::persistence::io_error_to_acp(&e))?;
        drop(persistence_timer);
        let crate::session::persistence::PersistedInfoLight {
            summary,
            chat_history,
            plan_state: _,
            plan_mode_state: persisted_plan_mode,
            updates_file_path,
            rewind_points_file_path,
            signals: persisted_signals,
            announcement_state: persisted_announcement_state,
            goal_mode_state: _persisted_goal_mode,
            workflow_runs: persisted_workflow_runs,
        } = persistence_info;
        let restored_compaction_count = persisted_signals
            .as_ref()
            .map(|s| s.compaction_count as u64)
            .unwrap_or(0);
        let restored_turn_count = persisted_signals
            .as_ref()
            .map(|s| s.turn_count as u64)
            .unwrap_or(0);
        let restored_tool_call_count = persisted_signals
            .as_ref()
            .map(|s| s.tool_call_count as u64)
            .unwrap_or(0);
        let restored_plan_mode_state = match &persisted_plan_mode {
            Some(s) => {
                match s.state {
                    crate::session::plan_mode::PlanModeState::Inactive => {
                        xai_grok_telemetry::events::PlanModeState::Inactive
                    }
                    crate::session::plan_mode::PlanModeState::Pending => {
                        xai_grok_telemetry::events::PlanModeState::Pending
                    }
                    crate::session::plan_mode::PlanModeState::Active
                    | crate::session::plan_mode::PlanModeState::ExitPending => {
                        xai_grok_telemetry::events::PlanModeState::Active
                    }
                }
            }
            None => xai_grok_telemetry::events::PlanModeState::Inactive,
        };
        let restored_awaiting_plan_approval = persisted_plan_mode
            .as_ref()
            .is_some_and(|s| s.awaiting_plan_approval);
        self.set_turn_number(&session_id, summary.next_trace_turn);
        tracing::info!(
            session_id = %session_id.0,
            next_trace_turn = summary.next_trace_turn,
            "Loaded session telemetry turn counter from persistence"
        );
        let no_replay = parse_no_replay(request_meta.as_ref());
        let cursor = request_meta
            .as_ref()
            .and_then(|m| m.get("cursor"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let session_yolo_mode = request_meta
            .as_ref()
            .and_then(|m| m.get("yoloMode"))
            .and_then(|v| v.as_bool())
            .unwrap_or(self.default_yolo_mode);
        let session_auto_mode = resolve_session_auto_mode(
            request_meta.as_ref(),
            self.default_auto_mode,
            session_yolo_mode,
        );
        #[allow(unused_variables)]
        let session_computer_sessions = resolve_session_computer_sessions(
            request_meta.as_ref(),
        )?;
        let restore_code_requested = request_meta
            .as_ref()
            .and_then(|m| m.get("x.ai/restore_code"))
            .and_then(|v| v.as_bool())
            .unwrap_or(self.restore_code);
        let registry_client_for_restore = self.session_registry_client();
        if restore_code_requested && registry_client_for_restore.is_none() {
            xai_grok_workspace::session::git::warn_registry_disabled_restore(
                session_id.0.as_ref(),
            );
        }
        let restore_checkout_allowed = xai_grok_workspace::session::git::restore_code_checkout_allowed(
            cwd.as_path(),
            Some(summary.info.cwd.as_str()),
        );
        if restore_code_requested && !restore_checkout_allowed
            && let Some(ref target_sha) = summary.head_commit
        {
            tracing::warn!(
                target: xai_grok_workspace::session::git::RESTORE_CODE_LOG,
                session_id = %session_id.0,
                supplied_cwd = %cwd.as_str(),
                persisted_cwd = %summary.info.cwd,
                target_sha = %target_sha,
                "restore_code: skipping session HEAD checkout — supplied cwd is neither a grok worktree nor the session's persisted cwd (refusing to detach the source repo)"
            );
            xai_grok_telemetry::unified_log::warn(
                "restore_code: skipped session HEAD checkout (unsafe cwd)",
                Some(session_id.0.as_ref()),
                Some(
                    serde_json::json!({
                    "supplied_cwd": cwd.as_str(),
                    "persisted_cwd": summary.info.cwd,
                    "target_sha": target_sha,
                }),
                ),
            );
        }
        let mut code_restore_info: Option<serde_json::Value> = None;
        if restore_code_requested && restore_checkout_allowed
            && let Some(ref target_sha) = summary.head_commit
        {
            use xai_grok_workspace::session::git::RestoreKind;
            let outcome = xai_grok_workspace::session::git::checkout_session_commit(
                    cwd.as_path(),
                    target_sha,
                    true,
                    session_id.0.as_ref(),
                )
                .await;
            let kind = if !outcome.checked_out {
                RestoreKind::CheckoutFailed
            } else {
                match registry_client_for_restore {
                        None => RestoreKind::RegistryOff,
                        Some(registry_client) => {
                            let _ = registry_client;
                            RestoreKind::RegistryOff
                        }
                    }
            };
            code_restore_info = crate::agent::restore_code::build_code_restore_meta(
                target_sha,
                &outcome,
                kind,
            );
        }
        let load_envrc = {
            let skip_envrc = request_meta
                .as_ref()
                .and_then(|m| m.get("x.ai/skip_envrc"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if skip_envrc {
                false
            } else {
                self.cfg.borrow().session.load_envrc.unwrap_or(true)
            }
        };
        let (initial_total_tokens, delta_completions, unfinished_subagents) = if no_replay {
            tracing::info!(
                session_id = %session_id.0,
                "Skipping session replay (noReplay flag set by relay)"
            );
            (
                Self::extract_initial_tokens_from_updates(&updates_file_path),
                Vec::new(),
                Vec::new(),
            )
        } else {
            let (tokens, replay_end_offset, unfinished_subagents) = self
                .replay_session_updates(
                    &session_id,
                    &cwd,
                    &updates_file_path,
                    persist_data.as_ref(),
                    target_client_id.as_ref(),
                    cursor.as_deref(),
                )
                .await?;
            let cursor_mark_replay = cursor.is_none();
            let _timer = crate::instrumentation_timer!("session.delta_flush_replay");
            let completions = match self.flush_session(&session_id).await {
                Ok(()) => {
                    self.replay_session_updates_from_offset_enqueue(
                        &session_id,
                        &updates_file_path,
                        replay_end_offset,
                        persist_data.as_ref(),
                        target_client_id.as_ref(),
                        cursor_mark_replay,
                    )
                }
                Err(reason) => {
                    tracing::warn!(
                        session_id = %session_id.0,
                        reason,
                        "Post-replay flush failed, skipping delta replay"
                    );
                    Vec::new()
                }
            };
            (tokens, completions, unfinished_subagents)
        };
        if let Some(handle) = self.resident_handle(&session_id) {
            handle.gateway_enabled.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        for rx in delta_completions {
            let _ = rx.await;
        }
        let reconcile_completions = {
            let _timer = crate::instrumentation_timer!("session.reconcile_stale_tasks");
            self.reconcile_stale_background_tasks(&session_id, &updates_file_path)
        };
        for rx in reconcile_completions {
            let _ = rx.await;
        }
        let preloaded_envrc = xai_grok_workspace::envrc::load_envrc_or_empty_when_trusted(
            cwd.as_path(),
            load_envrc && folder_trust::project_scope_allowed(cwd.as_path()),
        );
        let client_code_nav_enabled = request_meta
            .as_ref()
            .and_then(|m| m.get("codeNavEnabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| self.code_nav_enabled.get());
        let (client_terminal, client_fs_read, client_fs_write) = Self::resolve_client_io_caps(
            request_meta.as_ref(),
            init,
        );
        let prompt_display_cwd = request_meta
            .as_ref()
            .and_then(|m| m.get("x.ai/display_cwd"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| summary.prompt_display_cwd.clone());
        if self.resident_handle(&session_id).is_none() {
            tracing::info!(
                session_id = %session_id.0,
                "load_session: spawning new session actor (session not in memory)"
            );
            let mut spawn_timer = crate::instrumentation_timer!("session.spawn_and_register_session");
            spawn_timer.with_field("session_id", session_id.0.as_ref());
            let (models, available) = self.models_manager.models_and_available();
            let persisted_catalog_identity = summary
                .catalog_identity
                .as_ref()
                .filter(|identity| identity.model_id == summary.current_model_id.0.as_ref());
            let reconciled_catalog_identity = persisted_catalog_identity.and_then(|identity| {
                crate::agent::models::reconcile_persisted_catalog_identity(&models, identity)
            });
            let persisted_identity_unresolved =
                should_reject_unresolved_persisted_identity(
                    &models,
                    persisted_catalog_identity,
                    reconciled_catalog_identity.as_ref(),
                );
            let persisted_resolution = if persisted_catalog_identity.is_some() {
                reconciled_catalog_identity
                    .as_ref()
                    .filter(|identity| {
                        available.contains_key(&acp::ModelId::new(identity.model_id.clone()))
                    })
                    .map(|identity| {
                        crate::agent::models::PersistedCatalogKeyResolution::Resolved(
                            acp::ModelId::new(identity.model_id.clone()),
                        )
                    })
                    .unwrap_or(crate::agent::models::PersistedCatalogKeyResolution::Missing)
            } else {
                crate::agent::models::selectable_catalog_resolution_for_persisted(
                    &models,
                    &available,
                    &summary.current_model_id,
                )
            };
            let resolved_persisted_catalog_id = match &persisted_resolution {
                crate::agent::models::PersistedCatalogKeyResolution::Resolved(id) => {
                    Some(id.clone())
                }
                crate::agent::models::PersistedCatalogKeyResolution::AmbiguousSlug { matches, .. } => {
                    ambiguous_persisted_slug_matches = Some(matches.clone());
                    None
                }
                crate::agent::models::PersistedCatalogKeyResolution::Missing => None,
            };
            let persisted_model_for_spawn = resolved_persisted_catalog_id
                .clone()
                .unwrap_or_else(|| summary.current_model_id.clone());
            let persisted_agent_name: Option<String> = summary
                .agent_name
                .clone()
                .or_else(|| {
                    self
                        .resolve_model_id(&persisted_model_for_spawn)
                        .ok()
                        .map(|m| m.info().agent_type.clone())
                });
            // Fail-closed before spawn: never build the actor on an unready
            // persisted model (would attach ambient Bearer via sampling_config).
            let plugin_registry = self.plugin_registry_handle.snapshot();
            let active_definition = {
                let cfg = self.cfg.borrow();
                Self::resolve_agent_definition_with_plugins(
                    cwd.as_path(),
                    cfg.agent_profile_path.as_deref(),
                    &cfg.agent,
                    None,
                    persisted_agent_name.as_deref(),
                    plugin_registry.as_deref(),
                )
            };
            let mut ready_compatible_fallback = |candidates: Vec<acp::ModelId>| {
                first_ready_compatible_model(
                    candidates,
                    &active_definition,
                    |id| self.resolve_model_id(id).ok(),
                    |agent_type| {
                        xai_grok_agent::discovery::by_name_in_cwd_with_plugins(
                            agent_type,
                            cwd.as_path(),
                            plugin_registry.as_deref(),
                        )
                    },
                )
            };
            if persisted_catalog_identity.is_some()
                && persisted_model_for_spawn != summary.current_model_id
                && ready_compatible_fallback(vec![persisted_model_for_spawn.clone()]).is_none()
            {
                return Err(acp::Error::invalid_params().data(format!(
                    "reconciled model '{}' is not ready or is incompatible with persisted harness '{}'",
                    persisted_model_for_spawn.0,
                    active_definition.name
                )));
            }
            let spawn_selection = if persisted_identity_unresolved {
                return Err(acp::Error::invalid_params().data(format!(
                    "persisted model '{}' no longer resolves to its committed catalog route",
                    summary.current_model_id.0
                )));
            } else if let Some(matches) = ambiguous_persisted_slug_matches.as_ref() {
                tracing::warn!(
                    session_id = %session_id.0,
                    persisted = %summary.current_model_id.0,
                    matching_catalog_ids = ?matches.iter().map(|id| id.0.as_ref()).collect::<Vec<_>>(),
                    "load_session: persisted legacy model slug is ambiguous; requiring explicit model selection"
                );
                cold_spawn_fallback_selection(
                    &summary.current_model_id,
                    None,
                    ready_compatible_fallback(vec![self.models_manager.current_model_id()]),
                )
            } else {
                match self.resolve_model_id(&persisted_model_for_spawn) {
                    Ok(entry) if crate::agent::config::model_readiness(&entry).0 => {
                        ColdSpawnModelSelection {
                            model_id: persisted_model_for_spawn.clone(),
                            unavailable_model: None,
                        }
                    }
                    Ok(entry) => {
                        let reason = crate::agent::config::model_readiness(&entry)
                            .1
                            .unwrap_or_else(|| "model is not ready".to_owned());
                        if let Some(fallback) =
                            ready_compatible_fallback(available.keys().cloned().collect())
                        {
                            tracing::warn!(
                                session_id = %session_id.0,
                                previous = %summary.current_model_id.0,
                                new = %fallback.0,
                                %reason,
                                "load_session: persisted model not ready before spawn; using ready fallback"
                            );
                            cold_spawn_fallback_selection(
                                &summary.current_model_id,
                                Some(fallback),
                                None,
                            )
                        } else if let Some(current) = ready_compatible_fallback(vec![
                            self.models_manager.current_model_id(),
                        ])
                        {
                            tracing::warn!(
                                session_id = %session_id.0,
                                previous = %summary.current_model_id.0,
                                new = %current.0,
                                %reason,
                                "load_session: persisted model not ready; spawning on current ready default and latching"
                            );
                            cold_spawn_fallback_selection(
                                &summary.current_model_id,
                                None,
                                Some(current),
                            )
                        } else {
                            tracing::warn!(
                                session_id = %session_id.0,
                                previous = %summary.current_model_id.0,
                                %reason,
                                "load_session: persisted model not ready and no ready fallback; latching prompts"
                            );
                            cold_spawn_fallback_selection(&summary.current_model_id, None, None)
                        }
                    }
                    Err(_) if !available.is_empty() => {
                        if let Some(fallback) =
                            ready_compatible_fallback(available.keys().cloned().collect())
                        {
                            tracing::warn!(
                                session_id = %session_id.0,
                                previous = %summary.current_model_id.0,
                                new = %fallback.0,
                                "load_session: persisted model unresolved; spawning on ready fallback"
                            );
                            cold_spawn_fallback_selection(
                                &summary.current_model_id,
                                Some(fallback),
                                None,
                            )
                        } else {
                            cold_spawn_fallback_selection(&summary.current_model_id, None, None)
                        }
                    }
                    Err(_) => {
                        // Catalog empty / still loading: latch so prompts cannot run
                        // on an unverified persisted model with ambient credentials.
                        tracing::warn!(
                            session_id = %session_id.0,
                            persisted = %summary.current_model_id.0,
                            "load_session: catalog empty at spawn; latching until a ready model is confirmed"
                        );
                        cold_spawn_fallback_selection(&summary.current_model_id, None, None)
                    }
                }
            };
            let spawn_model_id = spawn_selection.model_id.clone();
            let latch_persisted_unready = spawn_selection.unavailable_model.is_some();

            // The fallback selection above is not authorization to use a
            // mismatched harness. Validate the exact definition before
            // transferring persistence into an actor or registering session
            // state. Preserve the existing unavailable-model latch when the
            // catalog is absent or no ready fallback exists: prompts remain
            // blocked until the later restore path can validate a ready model.
            let required_agent_type = match self.resolve_model_id(&spawn_model_id) {
                Ok(spawn_model) => {
                    let (ready, reason) = crate::agent::config::model_readiness(&spawn_model);
                    if !ready && !latch_persisted_unready {
                        return Err(acp::Error::invalid_params().data(reason.unwrap_or_else(|| {
                            format!("load_session model '{}' is not ready", spawn_model_id.0)
                        })));
                    }
                    spawn_model.info().agent_type.clone()
                }
                Err(_) if latch_persisted_unready => persisted_agent_name
                    .clone()
                    .unwrap_or_else(|| active_definition.name.clone()),
                Err(_) => {
                    return Err(acp::Error::invalid_params().data(format!(
                        "load_session model '{}' is not present in the catalog",
                        spawn_model_id.0
                    )));
                }
            };
            let required_definition =
                xai_grok_agent::discovery::by_name_in_cwd_with_plugins(
                    &required_agent_type,
                    cwd.as_path(),
                    plugin_registry.as_deref(),
                );
            if !harnesses_are_compatible(
                &active_definition,
                &required_agent_type,
                required_definition.as_ref(),
            ) {
                return Err(crate::agent::config::ModelSwitchHarnessError {
                    code: crate::agent::config::MODEL_SWITCH_REBUILD_FAILED.to_owned(),
                    active_agent_type: active_definition.name,
                    required_agent_type,
                    model_id: spawn_model_id.0.to_string(),
                    reason: if required_definition.is_some() {
                        "incompatible_agent".to_owned()
                    } else {
                        "agent_definition_unresolved".to_owned()
                    },
                }
                .into_acp_error());
            }
            // `load_light` starts persistence before cold-spawn model selection is
            // complete. Rebind only the inherited summary lane to the operative
            // restored model before any session content can reach that actor.
            // Explicit summary pins were resolved correctly during `load_light`.
            if self.cfg.borrow().session_summary_follows_default {
                let summary_sampling = self.resolve_sampling_config_for_model(
                    &spawn_model_id,
                    origin_client.clone(),
                );
                let _ = persistence.tx.send(
                    crate::session::persistence::PersistenceMsg::ReplaceSummarySamplingConfig(
                        summary_sampling,
                    ),
                );
            }
            cold_spawn_selection = Some(spawn_selection);
            let spawned = self.spawn_and_register_session(
                    init,
                    SessionSpawnOptions {
                        session_info,
                        cwd: cwd.clone(),
                        mcp_servers,
                        initial_client_mcp_servers,
                        mcp_meta_config_map,
                        persistence,
                        chat_history,
                        rewind_points_file_path,
                        initial_total_tokens,
                        origin_client: origin_client.clone(),
                        client_code_nav_enabled,
                        client_terminal,
                        client_fs_read,
                        client_fs_write,
                        preloaded_envrc: Some(preloaded_envrc),
                        persisted_signals,
                        persisted_plan_mode,
                        persisted_goal_mode: _persisted_goal_mode,
                        persisted_workflow_runs,
                        persisted_announcement_state,
                        session_meta: request_meta.as_ref(),
                        managed_mcp_expires_at,
                        model_agent_type: persisted_agent_name.as_deref(),
                        prepared_sampling_config: None,
                        prepared_catalog_identity: None,
                        prepared_model_entry: None,
                        new_session_auth_authority: None,
                        publication_gate: None,
                        deferred_relay_state_rx: None,
                        upgrade_persistence_to_writeback: false,
                        persisted_catalog_identity: reconciled_catalog_identity,
                        session_model_id: spawn_model_id,
                        session_yolo_mode,
                        session_auto_mode: session_auto_mode && !session_yolo_mode,
                        prompt_display_cwd,
                        is_chat_kind: false,
                    },
                )
                .await?;
            if !matches!(spawned, SpawnedSession::Committed(_)) {
                return Err(acp::Error::internal_error()
                    .data("load_session unexpectedly produced a provisional session"));
            }
            if latch_persisted_unready {
                self.session_registry
                    .set_unavailable_model_with_identity(
                        &session_id,
                        summary.current_model_id.clone(),
                        summary.catalog_identity.clone(),
                        persisted_agent_name.clone(),
                    );
            }
            drop(spawn_timer);
        } else {
            tracing::info!(
                session_id = %session_id.0,
                mcp_server_count = mcp_servers.len(),
                "load_session: reconnecting to existing session, updating MCP servers"
            );
            self.with_resident_mut(&session_id, |handle| {
                handle.initial_client_mcp_servers = initial_client_mcp_servers;
                let (tx, _rx) = tokio::sync::oneshot::channel();
                let _ = handle
                    .cmd_tx
                    .send(crate::session::SessionCommand::UpdateMcpServers {
                        mcp_servers,
                        respond_to: tx,
                    });
            });
        }
        {
            let init_meta = self
                .initialize_request
                .get()
                .and_then(|init| init.meta.as_ref());
            if let Some(handle) = self.resident_handle(&session_id) {
                enqueue_replace_system_prompt_override(
                    &handle.cmd_tx,
                    request_meta.as_ref(),
                    init_meta,
                );
            }
        }
        if session_exists
            && let Some(hooks) = crate::extensions::hooks::reconnect_client_hooks(
                request_meta.as_ref(),
            ) && let Some(handle) = self.resident_handle(&session_id)
        {
            handle.set_client_hooks(hooks);
        }
        #[allow(unused_variables)]
        let local_transcript_rendered = !no_replay
            && updates_file_path
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .is_some_and(|m| m.len() > 0);
        self.with_resident_mut(&session_id, |handle| {
            handle.code_nav_enabled = client_code_nav_enabled;
            if session_yolo_mode && !handle.yolo_mode {
                tracing::debug!(
                    session_id = %session_id.0,
                    "Setting YOLO mode on reconnect from load_session request metadata"
                );
                handle.yolo_mode = true;
                let _ = handle
                    .cmd_tx
                    .send(crate::session::SessionCommand::SetYoloMode {
                        enabled: true,
                    });
            }
            if session_auto_mode && !session_yolo_mode
                && crate::util::config::auto_permission_mode_enabled_from_disk()
            {
                tracing::debug!(
                    session_id = %session_id.0,
                    "Setting auto mode on reconnect from load_session request metadata"
                );
                handle.yolo_mode = false;
                let _ = handle
                    .cmd_tx
                    .send(SessionCommand::SetAutoMode {
                        enabled: true,
                    });
            }
        });
        self.maybe_spawn_interactive_trust_prompt(
            &session_id,
            cwd.as_path(),
            remote_settings.as_ref(),
        );
        let orphan_parent = self
            .resident_handle(&session_id)
            .map(|handle| (handle.cmd_tx.clone(), handle.info.cwd.clone()));
        if let Some((parent_cmd_tx, session_cwd)) = orphan_parent {
            let session_dir = crate::session::persistence::session_dir(
                &SessionInfo {
                    id: session_id.clone(),
                    cwd: session_cwd,
                },
            );
            crate::agent::subagent::reconcile_orphaned_subagents_with_backend(
                    &unfinished_subagents,
                    &xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend::new(
                        self.subagent_event_tx.clone(),
                    ),
                    &session_dir,
                    session_id.0.as_ref(),
                    &self.gateway,
                    Some(&parent_cmd_tx),
                )
                .await;
        }
        let persisted_model = summary.current_model_id.clone();
        let (models, available) = self.models_manager.models_and_available();
        if cold_spawn_selection.is_none() {
            self.session_registry.take_unavailable_model(&session_id);
        }
        let resolved_catalog_key = resolve_catalog_key(&models, &persisted_model);
        let selectable_resolution = crate::agent::models::selectable_catalog_resolution_for_persisted(
            &models,
            &available,
            &persisted_model,
        );
        tracing::debug!(
            session_id = %session_id.0,
            persisted = %persisted_model.0,
            resolved_catalog_key = ?resolved_catalog_key.as_ref().map(|k| k.0.as_ref()),
            selectable_resolution = ?selectable_resolution,
            available_count = available.len(),
            contains_persisted = available.contains_key(&persisted_model),
            available_keys = ?available.keys().take(10).collect::<Vec<_>>(),
            "load_session: restoring persisted model (debug)"
        );
        let is_grok_build = persisted_model.0.starts_with("grok-build");
        let same_family_fallback = if is_grok_build {
            available.keys().find(|id| id.0.starts_with("grok-build")).cloned()
        } else {
            available.keys().find(|id| !id.0.starts_with("grok-build")).cloned()
        };
        let model_id = if let Some(preflight) = cold_spawn_selection.as_ref() {
            preflight.replace_unavailable_latch(
                &self.session_registry,
                &session_id,
                summary.catalog_identity.clone(),
                summary.agent_name.clone().or_else(|| {
                    self.resident_handle(&session_id)
                        .map(|handle| handle.agent_name)
                }),
            );
            if preflight.unavailable_model.is_some() {
                let reason = if let Some(matches) = ambiguous_persisted_slug_matches.as_ref() {
                    let options = matches
                        .iter()
                        .map(|id| id.0.as_ref())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "Model slug \"{}\" matches multiple configured models ({}). \
                         Use /model with one of those catalog IDs to resume this session.",
                        persisted_model.0, options,
                    )
                } else {
                    format!(
                        "Model \"{}\" is unavailable. Please start a new session or switch models.",
                        persisted_model.0,
                    )
                };
                self.send_model_auto_switched(
                    &session_id,
                    &persisted_model,
                    &acp::ModelId::new(String::new()),
                    &reason,
                )
                .await;
            } else if preflight.model_id != persisted_model {
                let reason = format!(
                    "Model \"{}\" could not be restored. Switched to \"{}\".",
                    persisted_model.0, preflight.model_id.0,
                );
                self.send_model_auto_switched(
                    &session_id,
                    &persisted_model,
                    &preflight.model_id,
                    &reason,
                )
                .await;
            }
            preflight.model_id.clone()
        } else {
            match selectable_resolution {
                crate::agent::models::PersistedCatalogKeyResolution::Resolved(catalog_key) => {
                    if catalog_key != persisted_model {
                        tracing::info!(
                            session_id = %session_id.0,
                            persisted = %persisted_model.0,
                            catalog_key = %catalog_key.0,
                            "load_session: mapped persisted routing slug to catalog key"
                        );
                        xai_grok_telemetry::unified_log::info(
                            "load_session: mapped persisted routing slug to catalog key",
                            Some(session_id.0.as_ref()),
                            Some(
                                serde_json::json!({
                                "persisted_model": persisted_model.0.as_ref(),
                                "catalog_key": catalog_key.0.as_ref(),
                            }),
                            ),
                        );
                    }
                    catalog_key
                }
                crate::agent::models::PersistedCatalogKeyResolution::AmbiguousSlug {
                    matches,
                    ..
                } => {
                    let options = matches
                        .iter()
                        .map(|id| id.0.as_ref())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let fallback = available
                        .keys()
                        .next()
                        .cloned()
                        .unwrap_or_else(|| self.models_manager.current_model_id());
                    tracing::warn!(
                        session_id = %session_id.0,
                        persisted = %persisted_model.0,
                        matching_catalog_ids = ?matches.iter().map(|id| id.0.as_ref()).collect::<Vec<_>>(),
                        fallback = %fallback.0,
                        "load_session: persisted legacy model slug is ambiguous; blocking prompts for explicit model selection"
                    );
                    let reason = format!(
                        "Model slug \"{}\" matches multiple configured models ({}). \
                         Use /model with one of those catalog IDs to resume this session.",
                        persisted_model.0, options,
                    );
                    self.send_model_auto_switched(
                            &session_id,
                            &persisted_model,
                            &acp::ModelId::new(String::new()),
                            &reason,
                        )
                        .await;
                    self.session_registry
                        .set_unavailable_model_with_identity(
                            &session_id,
                            persisted_model.clone(),
                            summary.catalog_identity.clone(),
                            summary.agent_name.clone().or_else(|| {
                                self.resident_handle(&session_id)
                                    .map(|handle| handle.agent_name)
                            }),
                        );
                    fallback
                }
                crate::agent::models::PersistedCatalogKeyResolution::Missing => {
                    if available.is_empty() {
                        tracing::warn!(
                            session_id = %session_id.0,
                            persisted = %persisted_model.0,
                            "load_session: model catalog empty at load; keeping persisted model unverified (catalog fetch may still be in flight)"
                        );
                        xai_grok_telemetry::unified_log::warn(
                            "load_session: model catalog empty, keeping persisted model unverified",
                            Some(session_id.0.as_ref()),
                            Some(
                                serde_json::json!({
                                "persisted_model": persisted_model.0.as_ref(),
                            }),
                            ),
                        );
                        persisted_model
                    } else if let Some(fallback) = same_family_fallback {
                        tracing::warn!(
                            session_id = %session_id.0,
                            previous = %persisted_model.0,
                            new = %fallback.0,
                            "Persisted model no longer available, auto-switching within family"
                        );
                        let reason = format!(
                            "Model \"{}\" is no longer available for your account.",
                            persisted_model.0,
                        );
                        self.send_model_auto_switched(
                                &session_id,
                                &persisted_model,
                                &fallback,
                                &reason,
                            )
                            .await;
                        fallback
                    } else {
                        let fallback = available
                            .keys()
                            .next()
                            .cloned()
                            .unwrap_or_else(|| persisted_model.clone());
                        tracing::warn!(
                            session_id = %session_id.0,
                            previous = %persisted_model.0,
                            fallback = %fallback.0,
                            available_count = available.len(),
                            available_keys = ?available.keys().take(10).collect::<Vec<_>>(),
                            "Persisted model no longer available, no same-family fallback — blocking prompts for this session"
                        );
                        xai_grok_telemetry::unified_log::warn(
                            "load_session: persisted model unavailable, no same-family fallback",
                            Some(session_id.0.as_ref()),
                            Some(
                                serde_json::json!({
                                "persisted_model": persisted_model.0.as_ref(),
                                "fallback_model": fallback.0.as_ref(),
                                "available_count": available.len(),
                            }),
                            ),
                        );
                        let reason = format!(
                            "Model \"{}\" is no longer available. Please start a new session.",
                            persisted_model.0,
                        );
                        let empty_id = acp::ModelId::new(String::new());
                        self.send_model_auto_switched(
                                &session_id,
                                &persisted_model,
                                &empty_id,
                                &reason,
                            )
                            .await;
                        self.session_registry
                            .set_unavailable_model_with_identity(
                                &session_id,
                                persisted_model.clone(),
                                summary.catalog_identity.clone(),
                                summary.agent_name.clone().or_else(|| {
                                    self.resident_handle(&session_id)
                                        .map(|handle| handle.agent_name)
                                }),
                            );
                        fallback
                    }
                }
            }
        };
        // Fail-closed: never apply an unready catalog entry (invalid auth_scheme,
        // missing BYOK key, etc.) — that would attach ambient Bearer to the session.
        let model_id = match self.resolve_model_id(&model_id) {
            Ok(_) if cold_spawn_selection.is_some() => model_id,
            Ok(entry) => {
                let (ready, reason) = crate::agent::config::model_readiness(&entry);
                if ready {
                    model_id
                } else {
                    let reason = reason.unwrap_or_else(|| "model is not ready".to_owned());
                    let ready_fallback = available.keys().find(|id| {
                        self.resolve_model_id(id)
                            .ok()
                            .is_some_and(|m| crate::agent::config::model_readiness(&m).0)
                    }).cloned();
                    if let Some(fallback) = ready_fallback {
                        tracing::warn!(
                            session_id = %session_id.0,
                            previous = %model_id.0,
                            new = %fallback.0,
                            %reason,
                            "load_session: model not ready; switching to a ready fallback"
                        );
                        let msg = format!(
                            "Model \"{}\" isn't ready ({reason}). Switched to \"{}\".",
                            model_id.0, fallback.0,
                        );
                        self.send_model_auto_switched(
                                &session_id,
                                &model_id,
                                &fallback,
                                &msg,
                            )
                            .await;
                        fallback
                    } else {
                        tracing::warn!(
                            session_id = %session_id.0,
                            model_id = %model_id.0,
                            %reason,
                            "load_session: model not ready and no ready fallback; blocking prompts"
                        );
                        xai_grok_telemetry::unified_log::warn(
                            "load_session: model not ready, blocking prompts",
                            Some(session_id.0.as_ref()),
                            Some(serde_json::json!({
                                "model_id": model_id.0.as_ref(),
                                "reason": reason,
                            })),
                        );
                        let msg = format!(
                            "Model \"{}\" isn't ready ({reason}). Please start a new session or switch models.",
                            model_id.0,
                        );
                        let empty_id = acp::ModelId::new(String::new());
                        self.send_model_auto_switched(
                                &session_id,
                                &model_id,
                                &empty_id,
                                &msg,
                            )
                            .await;
                        self.session_registry
                            .set_unavailable_model_with_identity(
                                &session_id,
                                model_id.clone(),
                                summary.catalog_identity.clone().filter(|identity| {
                                    identity.model_id == model_id.0.as_ref()
                                }),
                                summary.agent_name.clone().or_else(|| {
                                    self.resident_handle(&session_id)
                                        .map(|handle| handle.agent_name)
                                }),
                            );
                        model_id
                    }
                }
            }
            Err(_) => model_id,
        };
        tracing::debug!(
            session_id = %session_id.0,
            final_model_id = %model_id.0,
            "load_session: resolved final model_id for set_session_model"
        );
        let persisted_restore_identity = summary
            .catalog_identity
            .as_ref()
            .filter(|identity| identity.model_id == summary.current_model_id.0.as_ref());
        let restored_model = persisted_restore_identity
            .and_then(|identity| {
                crate::agent::models::reconcile_persisted_catalog_identity(&models, identity)
            })
            .filter(|identity| identity.model_id == model_id.0.as_ref())
            .and_then(|identity| {
                models
                    .get(identity.model_id.as_str())
                    .cloned()
                    .map(|model| (identity, model))
            });
        {
            let _timer = crate::instrumentation_timer!("session.restore_model");
            let restore_meta = summary
                .reasoning_effort
                .map(|effort| {
                    let mut map = acp::Meta::new();
                    map.insert(
                        REASONING_EFFORT_META_KEY.to_string(),
                        reasoning_effort_meta_value(effort),
                    );
                    map
                });
            let apply_result = if persisted_restore_identity.is_some() && restored_model.is_none() {
                Err(acp::Error::invalid_params()
                    .data("persisted catalog identity changed before model restore"))
            } else {
                restore_registered_session_model(
                    self,
                    acp::SetSessionModelRequest::new(session_id.to_owned(), model_id.clone())
                        .meta(restore_meta),
                    &load_guard,
                    restored_model,
                )
                .await
            };
            if let Err(e) = apply_result {
                tracing::warn!(
                    session_id = %session_id.0,
                    model_id = %model_id.0,
                    error = ?e,
                    "load_session: model restore apply failed; latching prompts"
                );
                self.session_registry
                    .set_unavailable_model_with_identity(
                        &session_id,
                        model_id.clone(),
                        summary.catalog_identity.clone().filter(|identity| {
                            identity.model_id == model_id.0.as_ref()
                        }),
                        summary.agent_name.clone().or_else(|| {
                            self.resident_handle(&session_id)
                                .map(|handle| handle.agent_name)
                        }),
                    );
            }
        }
        let mut response_meta_map = serde_json::Map::new();
        response_meta_map.insert("sessionId".to_string(), serde_json::json!(session_id));
        if let Some(persist) = persist_data {
            response_meta_map.insert("x.ai/persist".to_string(), persist);
        }
        let session_cwd = self
            .resident_handle(&session_id)
            .map(|h| h.info.cwd.clone());
        let indexed_roots = session_cwd
            .as_deref()
            .map(|c| self.indexed_roots_for(std::path::Path::new(c)))
            .unwrap_or_default();
        response_meta_map
            .insert("codebaseIndexed".to_string(), serde_json::json!(indexed_roots));
        if summary.head_commit.is_some() && let Some(ref cwd) = session_cwd
            && summary
                .git_root_dir
                .as_deref()
                .is_none_or(|root| {
                    xai_grok_workspace::session::git::find_git_root_from_path(
                            std::path::Path::new(cwd.as_str()),
                        )
                        .ok()
                        .is_some_and(|current_root| {
                            current_root == std::path::Path::new(root)
                        })
                })
        {
            let _timer = crate::instrumentation_timer!("session.git_divergence");
            let cwd_path = std::path::Path::new(cwd.as_str());
            let current_head = xai_grok_workspace::session::git::git_cli(
                    cwd_path,
                    &["rev-parse", "HEAD"],
                )
                .await
                .ok();
            if let Some(divergence) = xai_grok_workspace::session::git::detect_head_divergence(
                summary.head_commit.as_deref(),
                summary.head_branch.as_deref(),
                current_head.as_deref(),
            ) {
                response_meta_map
                    .insert("gitDivergence".to_string(), serde_json::json!(divergence));
            }
        }
        if let Some(info) = code_restore_info {
            response_meta_map.insert("codeRestore".to_string(), info);
        }
        if let Some(running_prompt_id) = self
            .resident_handle(&session_id)
            .and_then(|h| h.current_prompt_id.lock().ok().and_then(|g| g.clone()))
        {
            response_meta_map
                .insert(
                    "x.ai/runningPromptId".to_string(),
                    serde_json::json!(running_prompt_id),
                );
        }
        if session_exists {
            self
                .recompute_web_search_disable_notice_for_session(&session_id)
                .await;
        }
        let (model_state, model_presentation) =
            self.model_state_with_presentation(Some(&session_id));
        self.insert_session_config_meta_with_presentation(
            &mut response_meta_map,
            &session_id,
            session_cwd.clone().unwrap_or_default(),
            summary.display_title_opt(),
            &model_state,
            &model_presentation,
        );
        let applied_tool_overrides = {
            let cmd_tx = self
                .resident_handle(&session_id)
                .map(|handle| handle.cmd_tx.clone());
            match cmd_tx {
                Some(cmd_tx) => read_applied_tool_overrides(&cmd_tx).await,
                None => {
                    tracing::warn!(
                        session_id = %session_id.0,
                        "session/load toolOverrides echo: session handle not found"
                    );
                    None
                }
            }
        };
        insert_applied_tool_overrides(
            &mut response_meta_map,
            applied_tool_overrides.as_ref(),
        );
        let response_meta = serde_json::Value::Object(response_meta_map);
        xai_grok_telemetry::unified_log::info(
            "session loaded",
            Some(session_id.0.as_ref()),
            None,
        );
        let response = acp::LoadSessionResponse::new()
            .models(Some(model_state))
            .meta(response_meta.as_object().cloned());
        if let Some(handle) = self.resident_handle(&session_id) {
            let _ = handle.cmd_tx.send(SessionCommand::AdvertiseCommands);
            if restored_awaiting_plan_approval {
                let _ = handle.cmd_tx.send(SessionCommand::RestorePlanApproval);
            }
        }
        if self.product_analytics_enabled() {
            log_event(xai_grok_telemetry::events::SessionLoad {
                session_id: session_id.0.to_string(),
                compaction_count: restored_compaction_count,
                turn_count: restored_turn_count,
                tool_call_count: restored_tool_call_count,
                plan_mode_state: restored_plan_mode_state,
                permission_mode: if session_yolo_mode {
                    xai_grok_telemetry::enums::PermissionMode::AlwaysApprove
                } else if session_auto_mode
                    && crate::util::config::auto_permission_mode_enabled_from_disk()
                {
                    xai_grok_telemetry::enums::PermissionMode::Auto
                } else {
                    xai_grok_telemetry::enums::PermissionMode::Ask
                },
                model_id: summary.current_model_id.0.to_string(),
                restored_from_disk: true,
            });
        }
        Ok(response)
    }
    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        crate::agent::handlers::session::handle_list_sessions(self, args).await
    }
    async fn resume_session(
        &self,
        args: acp::ResumeSessionRequest,
    ) -> Result<acp::ResumeSessionResponse, acp::Error> {
        self.resume_session_inner(args).await
    }
    async fn close_session(
        &self,
        args: acp::CloseSessionRequest,
    ) -> Result<acp::CloseSessionResponse, acp::Error> {
        self.close_session_inner(args).await
    }
    #[tracing::instrument(
        name = "agent.prompt",
        skip_all,
        fields(session_id = %arguments.session_id.0, turn_number = tracing::field::Empty)
    )]
    #[allow(unused_mut)]
    async fn prompt(
        &self,
        mut arguments: acp::PromptRequest,
    ) -> Result<acp::PromptResponse, acp::Error> {
        use crate::session::plan_mode::PromptMode;
        if let Some(meta) = arguments.meta.as_ref() {
            xai_file_utils::trace_context::link_current_span_to_meta(
                &serde_json::Value::Object(meta.clone()),
            );
        }
        tracing::debug!(
            target: "sampling_log",
            session_id = %arguments.session_id.0,
            "Received prompt request"
        );
        xai_grok_telemetry::unified_log::info(
            "prompt received",
            Some(arguments.session_id.0.as_ref()),
            None,
        );
        let handle = self
            .session_handle_waiting_for_load(&arguments.session_id)
            .await
            .ok_or_else(|| acp::Error::invalid_params().data("unknown session id"))?;
        if self.models_manager.allowlist_excludes_all() {
            self.send_model_auto_switched(
                    &arguments.session_id,
                    &acp::ModelId::new(String::new()),
                    &acp::ModelId::new(String::new()),
                    "None of your models are allowed by allowed_models. \
                 Broaden it or remove it from your config, then restart.",
                )
                .await;
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        if self
            .session_registry
            .unavailable_model(&arguments.session_id)
            .is_none()
        {
            let presentation = self.models_manager.presentation_snapshot();
            let resident_model = handle.model_id.clone();
            let resolved_model = crate::agent::models::resolve_catalog_key(
                &presentation.catalog,
                &resident_model,
            );
            let normalized_model = resolved_model
                .clone()
                .unwrap_or_else(|| resident_model.clone());
            let resident_snapshot_is_current = self
                .with_resident_mut(&arguments.session_id, |resident| {
                    normalize_resident_model_if_unchanged(
                        resident,
                        &resident_model,
                        &normalized_model,
                    )
                })
                .unwrap_or(false);
            if resident_snapshot_is_current {
                let visible_model = resolved_model.as_ref().and_then(|model_id| {
                    presentation
                        .available
                        .contains_key(model_id)
                        .then(|| presentation.catalog.get(model_id.0.as_ref()))
                        .flatten()
                });
                let ready = visible_model
                    .is_some_and(|model| crate::agent::config::model_readiness(model).0);
                if !ready {
                    let catalog_identity = resolved_model.as_ref().and_then(|model_id| {
                        crate::agent::models::resolve_catalog_identity(
                            &presentation.catalog,
                            model_id,
                        )
                    });
                    self.session_registry.set_unavailable_model_with_identity(
                        &arguments.session_id,
                        normalized_model.clone(),
                        catalog_identity,
                        Some(handle.agent_name.clone()),
                    );
                    tracing::warn!(
                        session_id = %arguments.session_id.0,
                        resident_model_id = %resident_model.0,
                        normalized_model_id = %normalized_model.0,
                        present = resolved_model.is_some(),
                        auth_visible = resolved_model
                            .as_ref()
                            .is_some_and(|model_id| presentation.available.contains_key(model_id)),
                        "prompt: resident model became unavailable; latching before actor dispatch"
                    );
                }
            }
        }
        let latched_recovery = self
            .session_registry
            .unavailable_recovery_snapshot(&arguments.session_id);
        if let Some(recovery_snapshot) = latched_recovery {
            let unavailable_model = recovery_snapshot.unavailable_model.clone();
            let presentation = self.models_manager.presentation_snapshot();
            let models = presentation.catalog;
            let available = presentation.available;
            let latched_identity = recovery_snapshot.catalog_identity.clone();
            let mut reconciled_snapshot = latched_identity.as_ref().and_then(|identity| {
                reconcile_latched_catalog_snapshot(&models, &available, identity)
            });
            let persisted_agent_name = recovery_snapshot.agent_name.clone();
            if !latched_recovery_has_required_harness(
                latched_identity.as_ref(),
                persisted_agent_name.as_deref(),
            ) {
                tracing::warn!(
                    session_id = %arguments.session_id.0,
                    "prompt: identity-backed recovery lacks a persisted harness; keeping block"
                );
                reconciled_snapshot = None;
            } else if let (Some(persisted_agent_name), Some((_, _, model))) =
                (persisted_agent_name, reconciled_snapshot.as_ref())
            {
                let plugin_registry = self.plugin_registry_handle.snapshot();
                let active_definition = {
                    let cfg = self.cfg.borrow();
                    Self::resolve_agent_definition_with_plugins(
                        std::path::Path::new(&handle.info.cwd),
                        cfg.agent_profile_path.as_deref(),
                        &cfg.agent,
                        None,
                        Some(&persisted_agent_name),
                        plugin_registry.as_deref(),
                    )
                };
                let required_agent_type = model.info().agent_type.as_str();
                let required_definition =
                    xai_grok_agent::discovery::by_name_in_cwd_with_plugins(
                        required_agent_type,
                        std::path::Path::new(&handle.info.cwd),
                        plugin_registry.as_deref(),
                    );
                if !recovered_model_harness_is_compatible(
                    &active_definition,
                    model,
                    required_definition.as_ref(),
                ) {
                    tracing::warn!(
                        session_id = %arguments.session_id.0,
                        persisted_agent_name,
                        required_agent_type,
                        "prompt: recovered model requires an incompatible persisted harness; keeping block"
                    );
                    reconciled_snapshot = None;
                }
            }
            let resolution = if latched_identity.is_some() {
                reconciled_snapshot
                    .as_ref()
                    .map(|(model_id, _, _)| {
                        crate::agent::models::PersistedCatalogKeyResolution::Resolved(
                            model_id.clone(),
                        )
                    })
                    .unwrap_or(crate::agent::models::PersistedCatalogKeyResolution::Missing)
            } else {
                crate::agent::models::selectable_catalog_resolution_for_persisted(
                    &models,
                    &available,
                    &unavailable_model,
                )
            };
            match resolution {
                crate::agent::models::PersistedCatalogKeyResolution::Resolved(restore_model_id) => {
                    let restore_ready = reconciled_snapshot
                        .as_ref()
                        .map(|(_, _, model)| crate::agent::config::model_readiness(model).0)
                        .unwrap_or_else(|| {
                            self.resolve_model_id(&restore_model_id)
                                .ok()
                                .is_some_and(|m| crate::agent::config::model_readiness(&m).0)
                        });
                    if !restore_ready {
                        tracing::warn!(
                            session_id = %arguments.session_id.0,
                            model_id = %restore_model_id.0,
                            "prompt: previously-unavailable model is back but still not ready; keeping block"
                        );
                        self.send_model_auto_switched(
                                &arguments.session_id,
                                &acp::ModelId::new(String::new()),
                                &acp::ModelId::new(String::new()),
                                "Your previous model is still not ready (missing credentials or invalid auth_scheme).",
                            )
                            .await;
                        return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
                    }
                    tracing::info!(
                        session_id = %arguments.session_id.0,
                        model_id = %restore_model_id.0,
                        "prompt: previously-unavailable model is back in the catalog; attempting recovery"
                    );
                    let request = acp::SetSessionModelRequest::new(
                        arguments.session_id.clone(),
                        restore_model_id.clone(),
                    );
                    #[cfg(test)]
                    run_prompt_recovery_boundary_hook(
                        &arguments.session_id,
                        &restore_model_id,
                    );
                    let restored_model = reconciled_snapshot
                        .map(|(_, identity, model)| (identity, model));
                    match crate::agent::handlers::model_switch::apply_recovery(
                        self,
                        request,
                        recovery_snapshot,
                        restored_model,
                    )
                    .await
                    {
                        Ok(Some(_)) => {
                            tracing::info!(
                                session_id = %arguments.session_id.0,
                                model_id = %restore_model_id.0,
                                "prompt: restored previously-unavailable model and unblocked the session"
                            );
                            xai_grok_telemetry::unified_log::info(
                                "prompt: previously-unavailable model recovered, unblocking session",
                                Some(arguments.session_id.0.as_ref()),
                                Some(serde_json::json!({
                                    "model_id": restore_model_id.0.as_ref(),
                                })),
                            );
                        }
                        Ok(None) => {
                            tracing::info!(
                                session_id = %arguments.session_id.0,
                                model_id = %restore_model_id.0,
                                "prompt: stale unavailable-model recovery was superseded"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                session_id = %arguments.session_id.0,
                                model_id = %restore_model_id.0,
                                error = ?e,
                                "prompt: failed to restore previously-unavailable model; keeping block"
                            );
                            self.send_model_auto_switched(
                                &arguments.session_id,
                                &acp::ModelId::new(String::new()),
                                &acp::ModelId::new(String::new()),
                                "Could not restore your previous model; prompts stay blocked until a successful switch.",
                            )
                            .await;
                            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
                        }
                    }
                }
                crate::agent::models::PersistedCatalogKeyResolution::AmbiguousSlug {
                    matches,
                    ..
                } => {
                    let options = matches
                        .iter()
                        .map(|id| id.0.as_ref())
                        .collect::<Vec<_>>()
                        .join(", ");
                    tracing::warn!(
                        session_id = %arguments.session_id.0,
                        unavailable_model = %unavailable_model.0,
                        matching_catalog_ids = ?matches.iter().map(|id| id.0.as_ref()).collect::<Vec<_>>(),
                        "prompt blocked: persisted legacy model slug is ambiguous"
                    );
                    self.send_model_auto_switched(
                            &arguments.session_id,
                            &acp::ModelId::new(String::new()),
                            &acp::ModelId::new(String::new()),
                            &format!(
                                "Your previous model slug \"{}\" matches multiple configured models ({}). \
                                 Use /model with one of those catalog IDs to continue.",
                                unavailable_model.0, options
                            ),
                        )
                        .await;
                    return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
                }
                crate::agent::models::PersistedCatalogKeyResolution::Missing => {
                    let auth_hidden = crate::agent::models::resolve_catalog_key(
                        &models,
                        &unavailable_model,
                    )
                    .is_some();
                    let user_notice = if auth_hidden {
                        "Your session model is not available for the current authentication mode. Sign in with the required account or choose another visible model."
                    } else {
                        "Your previous model is no longer available and could not be switched to a compatible model. Please start a new session."
                    };
                    tracing::warn!(
                        session_id = %arguments.session_id.0,
                        unavailable_model = %unavailable_model.0,
                        auth_hidden,
                        available_count = available.len(),
                        available_keys = ?available.keys().take(10).collect::<Vec<_>>(),
                        "prompt blocked: resident model is absent from the auth-visible catalog"
                    );
                    xai_grok_telemetry::unified_log::warn(
                        "prompt blocked: model unavailable",
                        Some(arguments.session_id.0.as_ref()),
                        Some(
                            serde_json::json!({
                            "unavailable_model": unavailable_model.0.as_ref(),
                            "available_count": available.len(),
                        }),
                        ),
                    );
                    self.send_model_auto_switched(
                            &arguments.session_id,
                            &acp::ModelId::new(String::new()),
                            &acp::ModelId::new(String::new()),
                            user_notice,
                        )
                        .await;
                    return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
                }
            }
        }
        let dispatch_lock = self.dispatch_lock(&arguments.session_id);
        let dispatch_guard = dispatch_lock.lock().await;
        #[cfg(test)]
        run_prompt_dispatch_boundary_hook(&arguments.session_id);
        if self.session_load_in_flight(&arguments.session_id) {
            tracing::warn!(
                session_id = %arguments.session_id.0,
                "prompt: session load started before serialized dispatch"
            );
            drop(dispatch_guard);
            self.send_model_auto_switched(
                &arguments.session_id,
                &acp::ModelId::new(String::new()),
                &acp::ModelId::new(String::new()),
                "This session is still restoring; retry the prompt after restoration completes.",
            )
            .await;
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        if let Some(blocked_model) = self
            .session_registry
            .unavailable_model(&arguments.session_id)
        {
            tracing::warn!(
                session_id = %arguments.session_id.0,
                unavailable_model = %blocked_model.0,
                "prompt: unavailable-model block is still current after serialized recovery"
            );
            drop(dispatch_guard);
            self.send_model_auto_switched(
                &arguments.session_id,
                &acp::ModelId::new(String::new()),
                &acp::ModelId::new(String::new()),
                "Your session model is still unavailable; choose a ready model or restore its credentials.",
            )
            .await;
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        // A model switch may have completed while this prompt waited for the
        // dispatch lock. Re-read the resident under serialization and capture
        // auth/catalog authority before sending any actor command.
        let handle = self
            .resident_handle(&arguments.session_id)
            .ok_or_else(|| acp::Error::invalid_params().data("unknown session id"))?;
        let prompt_dispatch_authority = match self
            .models_manager
            .model_dispatch_authority(&handle.model_id)
        {
            Ok(authority) => authority,
            Err(reason) => {
                let catalog_identity = crate::agent::models::resolve_catalog_identity(
                    &self.models_manager.models(),
                    &handle.model_id,
                );
                self.session_registry.set_unavailable_model_with_identity(
                    &arguments.session_id,
                    handle.model_id.clone(),
                    catalog_identity,
                    Some(handle.agent_name.clone()),
                );
                drop(dispatch_guard);
                self.send_model_auto_switched(
                    &arguments.session_id,
                    &acp::ModelId::new(String::new()),
                    &acp::ModelId::new(String::new()),
                    &format!("Prompt blocked because the resident model is unavailable: {reason}"),
                )
                .await;
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }
        };
        let meta_prompt_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("mode"))
            .and_then(|v| v.as_str())
            .map(PromptMode::from_meta_str);
        let prompt_mode = if let Some(mode) = meta_prompt_mode {
            mode
        } else {
            let (mode_tx, mode_rx) = oneshot::channel();
            let _ = handle
                .cmd_tx
                .send(crate::session::SessionCommand::GetCurrentPromptMode {
                    responds_to: mode_tx,
                });
            mode_rx.await.unwrap_or_default()
        };
        let turn_started_at = chrono::Utc::now().to_rfc3339();
        let prompt_id = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("promptId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let turn_number = self.allocate_turn_number(&arguments.session_id);
        tracing::Span::current().record("turn_number", turn_number);
        tracing::info!("Setting up prompt tracing");
        let trace_context = self.get_trace_context(&handle.info, turn_number).await;
        let (harness_block_for_upload, upload_flush_timeout) = crate::util::config::load_blocking_upload_config_sync();
        let block_for_upload = self.cfg.borrow().mode == config::AgentMode::Headless
            || harness_block_for_upload;
        let (model_tx, model_rx) = oneshot::channel();
        let _ = handle
            .cmd_tx
            .send(crate::session::SessionCommand::GetCurrentModel {
                responds_to: model_tx,
            });
        let model = model_rx
            .await
            .unwrap_or_else(|_| self.sampling_config.borrow().model.clone());
        let mut parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>> = None;
        let verbatim = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("verbatim"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let send_now = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("sendNow"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(ctx) = trace_context.clone() {
            let (tx, parsed_prompt_rx) = oneshot::channel::<ParsedPromptInfo>();
            parsed_prompt_tx = Some(tx);
            let auth = self.auth_manager.current();
            let user_id = auth.as_ref().map(|a| a.user_id.clone());
            let team_id = auth.as_ref().and_then(|a| a.team_id.clone());
            let user_email = auth.and_then(|a| a.email);
            let init_meta = self
                .initialize_request
                .get()
                .and_then(|req| req.meta.as_ref());
            let client_source = init_meta
                .and_then(|meta| {
                    meta
                        .get("clientSource")
                        .or_else(|| meta.get("clientType"))
                        .or_else(|| meta.get("clientIdentifier"))
                })
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let client_version = init_meta
                .and_then(|meta| meta.get("clientVersion"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| self.cfg.borrow().client_version.clone());
            let plugin_registry = self.plugin_registry_snapshot();
            let prompt_images: Vec<agent_client_protocol::ImageContent> = arguments
                .prompt
                .iter()
                .filter_map(|block| {
                    if let agent_client_protocol::ContentBlock::Image(img) = block {
                        Some(img.clone())
                    } else {
                        None
                    }
                })
                .collect();
            let mut prompt_metadata = PromptMetadata {
                schema_version: GCS_SCHEMA_VERSION.to_string(),
                session_id: ctx.session_info.id.0.to_string(),
                turn_number: ctx.turn_number,
                request_id: prompt_id.clone(),
                turn_started_at: turn_started_at.clone(),
                repo_root: None,
                remote_url: None,
                user_id,
                user_email,
                team_id,
                client_source,
                client_version,
                model: model.to_owned(),
                reasoning_effort: ctx
                    .session_handle
                    .reasoning_effort
                    .map(|e| e.as_str().to_string()),
                experiment_id: None,
                host_os: std::env::consts::OS.to_string(),
                host_arch: std::env::consts::ARCH.to_string(),
                prompt_has_image: Some(!prompt_images.is_empty()),
                prompt_was_truncated: Some(false),
                prompt_verbatim: if verbatim { Some(true) } else { None },
                cwd: Some(ctx.session_info.cwd.clone()),
                agent_type: Some(ctx.session_handle.agent_name.clone()),
                shell_version: Some(xai_grok_version::VERSION.to_string()),
                workspace_type: None,
                sandbox: local_sandbox_telemetry(),
            };
            let (session_copy_tx, session_copy_rx) = oneshot::channel();
            let copy_sent = ctx
                .session_handle
                .cmd_tx
                .send(SessionCommand::CopyFile {
                    respond_to: session_copy_tx,
                })
                .is_ok();
            if !copy_sent {
                tracing::warn!(
                    session_id = %ctx.session_info.id.0,
                    turn_number = ctx.turn_number,
                    "Failed to send CopyFile command, skipping session state upload"
                );
            }
            tokio::spawn({
                let ctx = ctx.clone();
                async move {
                    if let Ok(Ok(info)) = tokio::time::timeout(
                            std::time::Duration::from_secs(120),
                            parsed_prompt_rx,
                        )
                        .await && !info.text.is_empty()
                    {
                        prompt_metadata.prompt_was_truncated = Some(
                            info.full_text.is_some(),
                        );
                        if let Some(full_text) = &info.full_text {
                            upload_full_prompt_txt(&ctx, full_text).await;
                        }
                    }
                    upload_metadata(&ctx, prompt_metadata).await;
                }
            });
            spawn_upload_task(
                "before_uploads",
                async move {
                    let before_workspace_fut = async {};
                    futures::join!(
                    upload_session_state(&ctx, "before", session_copy_rx, UploadWait::Confirm),
                    before_workspace_fut,
                    upload_images(&ctx, &prompt_images),
                    upload_plugin_state(&ctx, plugin_registry.as_deref()),
                );
                },
            );
        }
        let next_trace_turn = self
            .session_turn_number(&arguments.session_id)
            .unwrap_or_else(|| turn_number.saturating_add(1));
        let _ = handle
            .cmd_tx
            .send(crate::session::SessionCommand::SetNextTraceTurn {
                next_trace_turn,
                request_id: Some(prompt_id.clone()),
            });
        let (tx, rx) = oneshot::channel();
        let prompt_client_identifier = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("clientIdentifier"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let prompt_screen_mode = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("screenMode"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let json_schema = arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("outputSchema"))
            .cloned();
        if json_schema.as_ref().is_some_and(|schema| !schema.is_object()) {
            return Err(
                acp::Error::invalid_params()
                    .data("outputSchema must be a JSON object describing a JSON Schema"),
            );
        }
        let tool_overrides_update = match arguments
            .meta
            .as_ref()
            .and_then(|m| m.get("toolOverrides"))
        {
            None => None,
            Some(value) => {
                match xai_grok_sampling_types::ToolOverridesUpdate::parse(value) {
                    Ok(update) => Some(update),
                    Err(reason) => {
                        return Err(
                            acp::Error::invalid_params()
                                .data(format!("toolOverrides: {reason}")),
                        );
                    }
                }
            }
        };
        if self.session_load_in_flight(&arguments.session_id) {
            tracing::warn!(
                session_id = %arguments.session_id.0,
                "prompt: session load started during prompt preparation"
            );
            drop(dispatch_guard);
            self.send_model_auto_switched(
                &arguments.session_id,
                &acp::ModelId::new(String::new()),
                &acp::ModelId::new(String::new()),
                "This session began restoring while the prompt was prepared; retry after restoration completes.",
            )
            .await;
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        if let Some(blocked_model) = self
            .session_registry
            .unavailable_model(&arguments.session_id)
        {
            tracing::warn!(
                session_id = %arguments.session_id.0,
                unavailable_model = %blocked_model.0,
                "prompt: unavailable-model block changed during prompt preparation"
            );
            drop(dispatch_guard);
            self.send_model_auto_switched(
                &arguments.session_id,
                &acp::ModelId::new(String::new()),
                &acp::ModelId::new(String::new()),
                "Your session model became unavailable while preparing this prompt; choose a ready model or restore its credentials.",
            )
            .await;
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        let prompt_command = SessionCommand::Prompt {
                prompt_id: prompt_id.clone(),
                prompt_blocks: arguments.prompt.clone(),
                prompt_mode,
                artifact_upload_ctx: trace_context
                    .as_ref()
                    .map(|ctx| ctx.artifact_upload_context()),
                client_identifier: prompt_client_identifier,
                screen_mode: prompt_screen_mode,
                verbatim,
                traceparent: xai_file_utils::trace_context::current_traceparent(),
                json_schema,
                send_now,
                admission: None,
                tool_overrides_update,
                respond_to: tx,
                persist_ack: None,
                parsed_prompt_tx,
            };
        let prompt_dispatch = self
            .models_manager
            .commit_model_dispatch(&prompt_dispatch_authority, || {
                handle.cmd_tx.send(prompt_command).map_err(|_| ())
            });
        match prompt_dispatch {
            Ok(Ok(())) => {}
            Ok(Err(())) => {
                return Err(acp::Error::internal_error()
                    .data("failed to dispatch prompt: session actor closed"));
            }
            Err(reason) => {
                self.session_registry.set_unavailable_model_with_identity(
                    &arguments.session_id,
                    handle.model_id.clone(),
                    Some(prompt_dispatch_authority.catalog_identity.clone()),
                    Some(handle.agent_name.clone()),
                );
                drop(dispatch_guard);
                self.send_model_auto_switched(
                    &arguments.session_id,
                    &acp::ModelId::new(String::new()),
                    &acp::ModelId::new(String::new()),
                    &format!("Prompt blocked because model authorization changed: {reason}"),
                )
                .await;
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }
        }
        drop(dispatch_guard);
        self.push_roster_activity_delta(
            &arguments.session_id,
            crate::agent::roster::RosterActivity::Working,
        );
        let stop_result = rx
            .await
            .map_err(|_| {
                acp::Error::internal_error().data("session failed to respond")
            })?;
        let last_turn_usage_for_meta = handle
            .chat_state_handle
            .get_last_turn_usage()
            .await;
        let applied_tool_overrides = stop_result
            .as_ref()
            .ok()
            .and_then(|ok| ok.tool_overrides.clone());
        if matches!(
            stop_result,
            Ok(crate::session::commands::PromptTurnOk {
                completion_kind: crate::session::commands::PromptCompletionKind::RemovedFromQueue,
                ..
            })
        ) {
            return Ok(
                acp::PromptResponse::new(acp::StopReason::Cancelled)
                    .meta(
                        build_prompt_response_meta(PromptResponseMetaArgs {
                                session_id: &arguments.session_id.to_string(),
                                prompt_id: &prompt_id,
                                total_tokens: 0,
                                model_id: &model,
                                last_turn_usage: None,
                                prompt_usage: None,
                                cancellation_category: None,
                                cancel_trigger: None,
                                structured_output: None,
                                tool_overrides: applied_tool_overrides.clone(),
                            })
                            .as_object()
                            .cloned(),
                    ),
            );
        }
        let cancel_trigger: Option<String> = stop_result
            .as_ref()
            .ok()
            .and_then(|ok| match &ok.completion_kind {
                crate::session::commands::PromptCompletionKind::Cancelled {
                    context: Some(ctx),
                    ..
                } => ctx.trigger.clone(),
                _ => None,
            });
        {
            let mapped = stop_result
                .as_ref()
                .map(|ok| ok.stop_reason)
                .map_err(Clone::clone);
            let (stop_reason_value, agent_result_value) = crate::sampling::error::prompt_complete_fields(
                &mapped,
            );
            let turn_id = arguments
                .meta
                .as_ref()
                .and_then(|m| m.get("turnId"))
                .and_then(|v| v.as_u64());
            let mut payload = serde_json::json!({
                "sessionId": arguments.session_id.to_string(),
                "promptId": prompt_id.as_str(),
                "stopReason": stop_reason_value,
                "agentResult": agent_result_value,
            });
            if let Some(tid) = turn_id {
                payload["turnId"] = serde_json::json!(tid);
            }
            if let Some(ref t) = cancel_trigger {
                payload["cancelTrigger"] = serde_json::json!(t);
            }
            let params = serde_json::value::to_raw_value(&payload)
                .expect("prompt_complete params serialization");
            self.gateway
                .forward_fire_and_forget(
                    acp::ExtNotification::new(
                        "x.ai/session/prompt_complete",
                        params.into(),
                    ),
                );
        }
        {
            let end_activity = if handle
                .pending_interactions
                .lock()
                .map(|g| !g.is_empty())
                .unwrap_or(false)
            {
                crate::agent::roster::RosterActivity::NeedsInput
            } else {
                crate::agent::roster::RosterActivity::Idle
            };
            self.push_roster_activity_delta(&arguments.session_id, end_activity);
        }
        let resolved_model = handle.get_model_metadata().await.resolved_model_id;
        let harness_trace_turns = {
            let (tx, rx) = oneshot::channel();
            if handle
                .cmd_tx
                .send(SessionCommand::TakeHarnessTraceTurns {
                    respond_to: tx,
                })
                .is_ok()
            {
                rx.await.ok().unwrap_or_default()
            } else {
                Vec::new()
            }
        };
        if trace_context.is_some() && !harness_trace_turns.is_empty() {
            self.upload_harness_trace_turns(
                    &arguments.session_id,
                    &handle.info,
                    &handle.cmd_tx,
                    &model,
                    harness_trace_turns,
                )
                .await;
        }
        match stop_result {
            Ok(turn_ok) => {
                let crate::session::commands::PromptTurnOk {
                    stop_reason,
                    total_tokens,
                    turn_snapshot,
                    completion_kind,
                    structured_output,
                    usage: prompt_usage,
                    tool_overrides: _,
                } = turn_ok;
                let subagent_refs = self
                    .spawned_subagent_refs_for_prompt(
                        arguments.session_id.0.as_ref(),
                        &prompt_id,
                    )
                    .await;
                let permission_events = self
                    .collect_permission_events(&arguments.session_id);
                let turn_messages: Option<xai_chat_state::TurnCapture> = {
                    let (tx, rx) = oneshot::channel();
                    if handle
                        .cmd_tx
                        .send(SessionCommand::TakeTurnMessages {
                            respond_to: tx,
                        })
                        .is_ok()
                    {
                        rx.await.ok().flatten()
                    } else {
                        None
                    }
                };
                let streaming_partial = crate::upload::turn::take_streaming_partial(
                        &handle.cmd_tx,
                        prompt_id.clone(),
                        matches!(stop_reason, acp::StopReason::EndTurn),
                        Some(model.clone()),
                    )
                    .await
                    .map(|mut cap| {
                        cap.reason
                            .get_or_insert_with(|| match &completion_kind {
                                crate::session::commands::PromptCompletionKind::Cancelled {
                                    category,
                                    ..
                                } => {
                                    match category {
                                        Some(cat) => format!("cancelled:{cat:?}"),
                                        None => "cancelled".to_string(),
                                    }
                                }
                                _ => "non_completed".to_string(),
                            });
                        cap
                    });
                let upload_deadline = block_for_upload
                    .then(|| tokio::time::Instant::now() + upload_flush_timeout);
                if let Some(ctx) = trace_context.clone() {
                    let request_id = prompt_id.clone();
                    let (input_tokens, cached_input_tokens, output_tokens) = turn_snapshot
                        .as_ref()
                        .map(|s| (
                            Some(s.turn_input_tokens),
                            Some(s.turn_cached_input_tokens),
                            Some(s.turn_output_tokens),
                        ))
                        .unwrap_or((None, None, None));
                    if let Some(deadline) = upload_deadline {
                        let completed = matches!(stop_reason, acp::StopReason::EndTurn);
                        let start_for_upload = turn_snapshot
                            .as_ref()
                            .and_then(|s| s.start_prompt_mode.clone())
                            .or_else(|| Some(prompt_mode.to_string()));
                        let end_for_upload = turn_snapshot
                            .as_ref()
                            .and_then(|s| s.end_prompt_mode.clone());
                        let result = TurnResultMetadata {
                            schema_version: GCS_SCHEMA_VERSION,
                            request_id,
                            completed,
                            stop_reason: Some(format!("{stop_reason:?}")),
                            total_tokens: Some(total_tokens),
                            input_tokens,
                            cached_input_tokens,
                            output_tokens,
                            error: None,
                            finished_at: chrono::Utc::now().to_rfc3339(),
                            signals: turn_snapshot.as_ref().map(|s| s.current.clone()),
                            turn_delta: turn_snapshot.as_ref().map(|s| s.delta.clone()),
                            start_prompt_mode: start_for_upload,
                            end_prompt_mode: end_for_upload,
                            resolved_model: resolved_model.clone(),
                            subagents_spawned: subagent_refs.clone(),
                        };
                        upload_turn_result(&ctx, &result, UploadWait::Defer { deadline })
                            .await;
                    } else {
                        let snapshot_clone = turn_snapshot.clone();
                        let resolved_model = resolved_model.clone();
                        tokio::spawn(async move {
                            let completed = matches!(stop_reason, acp::StopReason::EndTurn);
                            let start_for_upload = snapshot_clone
                                .as_ref()
                                .and_then(|s| s.start_prompt_mode.clone())
                                .or_else(|| Some(prompt_mode.to_string()));
                            let end_for_upload = snapshot_clone
                                .as_ref()
                                .and_then(|s| s.end_prompt_mode.clone());
                            let result = TurnResultMetadata {
                                schema_version: GCS_SCHEMA_VERSION,
                                request_id,
                                completed,
                                stop_reason: Some(format!("{stop_reason:?}")),
                                total_tokens: Some(total_tokens),
                                input_tokens,
                                cached_input_tokens,
                                output_tokens,
                                error: None,
                                finished_at: chrono::Utc::now().to_rfc3339(),
                                signals: snapshot_clone.as_ref().map(|s| s.current.clone()),
                                turn_delta: snapshot_clone
                                    .as_ref()
                                    .map(|s| s.delta.clone()),
                                start_prompt_mode: start_for_upload,
                                end_prompt_mode: end_for_upload,
                                resolved_model,
                                subagents_spawned: subagent_refs.clone(),
                            };
                            upload_turn_result(&ctx, &result, UploadWait::Confirm).await;
                        });
                    }
                }
                if let Some(ctx) = trace_context {
                    let (session_copy_tx, session_copy_rx) = oneshot::channel();
                    let copy_sent = ctx
                        .session_handle
                        .cmd_tx
                        .send(SessionCommand::CopyFile {
                            respond_to: session_copy_tx,
                        })
                        .is_ok();
                    if !copy_sent {
                        tracing::warn!(
                            session_id = %ctx.session_info.id.0,
                            turn_number = ctx.turn_number,
                            "Failed to send CopyFile command, skipping session state upload"
                        );
                    }
                    if turn_number == 0
                        && let Some(client) = self.session_registry_client()
                    {
                        let cwd_str = handle.info.cwd.clone();
                        let model = self.models_manager.current_model_id().0.to_string();
                        let hostname = gethostname::gethostname()
                            .to_string_lossy()
                            .to_string();
                        let suppress = self
                            .auth_manager
                            .current_or_expired()
                            .is_some_and(|a| a.is_zdr_team());
                        let device_id = if suppress { None } else { Some(agent_id()) };
                        let first_prompt = if suppress {
                            None
                        } else {
                            arguments
                                    .prompt
                                    .iter()
                                    .find_map(|b| {
                                        if let acp::ContentBlock::Text(t) = b {
                                            Some(t.text.clone())
                                        } else {
                                            None
                                        }
                                    })
                        };
                        let sid = arguments.session_id.to_string();
                        tokio::spawn(async move {
                            let git_out = |args: &[&str]| -> Option<String> {
                                xai_tty_utils::git_command()
                                    .current_dir(&cwd_str)
                                    .args(args)
                                    .output()
                                    .ok()
                                    .filter(|o| o.status.success())
                                    .map(|o| {
                                        String::from_utf8_lossy(&o.stdout).trim().to_string()
                                    })
                                    .filter(|s| !s.is_empty())
                            };
                            let repo_remote_url = git_out(
                                &["remote", "get-url", "origin"],
                            );
                            let repo_branch = git_out(
                                &["rev-parse", "--abbrev-ref", "HEAD"],
                            );
                            let repo_head_at_start = git_out(&["rev-parse", "HEAD"]);
                            let reg_req = crate::agent::session_registry_client::RegisterRequest {
                                session_id: sid.clone(),
                                cwd: cwd_str,
                                gcs_trace_prefix: sid,
                                model_id: Some(model),
                                repo_remote_url,
                                repo_branch,
                                repo_head_at_start,
                                hostname: Some(hostname),
                                device_id,
                                parent_session_id: None,
                                session_kind: None,
                                subagent_type: None,
                                subagent_persona: None,
                                subagent_role: None,
                                fork_context_source: None,
                                subagent_depth: None,
                            };
                            if let Err(e) = client.register(&reg_req).await {
                                tracing::warn!(
                                    error = %e,
                                    "session registry register failed (non-fatal)"
                                );
                            }
                            let info = crate::session::info::Info {
                                id: agent_client_protocol::SessionId::new(
                                    reg_req.session_id.clone(),
                                ),
                                cwd: reg_req.cwd.clone(),
                            };
                            let summary_path = crate::session::persistence::session_dir(
                                    &info,
                                )
                                .join("summary.json");
                            let summary = if suppress {
                                None
                            } else {
                                std::fs::read(&summary_path)
                                        .ok()
                                        .and_then(|bytes| {
                                            serde_json::from_slice::<
                                                crate::session::persistence::Summary,
                                            >(&bytes)
                                                .ok()
                                        })
                                        .map(|s| s.session_summary)
                                        .filter(|s| !s.is_empty())
                            };
                            if first_prompt.is_some() || summary.is_some() {
                                let upd_req = crate::agent::session_registry_client::UpdateRequest {
                                    summary,
                                    first_prompt,
                                    last_turn_number: None,
                                    repo_head_at_end: None,
                                    restorable_turn_number: None,
                                };
                                tracing::debug!(
                                    session_id = %reg_req.session_id,
                                    has_summary = upd_req.summary.is_some(),
                                    "session registry post-register update"
                                );
                                if let Err(e) = client
                                    .update(&reg_req.session_id, &upd_req)
                                    .await
                                {
                                    tracing::warn!(
                                        error = %e,
                                        "session registry first-prompt update failed (non-fatal)"
                                    );
                                }
                            }
                        });
                    }
                    let registry_turn = i32::try_from(turn_number).unwrap_or(i32::MAX);
                    let cwd_for_git = handle.info.cwd.clone();
                    /// Advances `last_turn_number` immediately after a turn completes.
                    ///
                    /// Fired right after the session turn finishes, before any artifact uploads.
                    /// Sets `last_turn_number` with `repo_head_at_end` and does not wait for
                    /// session-state uploads.
                    async fn advance_last_turn(
                        client: crate::agent::session_registry_client::SessionRegistryClient,
                        session_id: String,
                        turn: i32,
                        cwd: String,
                    ) {
                        let repo_head_at_end = xai_tty_utils::git_command()
                            .current_dir(&cwd)
                            .args(["rev-parse", "HEAD"])
                            .output()
                            .ok()
                            .filter(|o| o.status.success())
                            .map(|o| {
                                String::from_utf8_lossy(&o.stdout).trim().to_string()
                            })
                            .filter(|s| !s.is_empty());
                        let req = crate::agent::session_registry_client::UpdateRequest {
                            summary: None,
                            first_prompt: None,
                            last_turn_number: Some(turn),
                            repo_head_at_end,
                            restorable_turn_number: None,
                        };
                        if let Err(e) = client.update(&session_id, &req).await {
                            tracing::warn!(
                                error = %e,
                                "session registry last_turn_number update failed (non-fatal)"
                            );
                        }
                    }
                    /// Advances `restorable_turn_number` after required restore artifacts are
                    /// confirmed durable.
                    ///
                    /// Called after the post-turn session archive is confirmed in cloud storage.
                    async fn advance_restorable_turn(
                        client: crate::agent::session_registry_client::SessionRegistryClient,
                        session_id: String,
                        turn: i32,
                    ) {
                        let req = crate::agent::session_registry_client::UpdateRequest {
                            summary: None,
                            first_prompt: None,
                            last_turn_number: None,
                            repo_head_at_end: None,
                            restorable_turn_number: Some(turn),
                        };
                        if let Err(e) = client.update(&session_id, &req).await {
                            tracing::warn!(
                                error = %e,
                                "session registry restorable_turn_number update failed (non-fatal)"
                            );
                        }
                    }
                    if let Some(client) = self.session_registry_client() {
                        let sid = arguments.session_id.to_string();
                        let cwd = cwd_for_git.clone();
                        tokio::spawn(async move {
                            advance_last_turn(client, sid, registry_turn, cwd).await;
                        });
                    }
                    {
                        let cwd = cwd_for_git.clone();
                        let cmd_tx = handle.cmd_tx.clone();
                        tokio::spawn(async move {
                            let head = xai_grok_workspace::session::git::get_current_commit(
                                    std::path::Path::new(&cwd),
                                )
                                .await;
                            let branch = xai_grok_workspace::session::git::get_branch(
                                    std::path::Path::new(&cwd),
                                )
                                .await;
                            let _ = cmd_tx
                                .send(crate::session::SessionCommand::PersistGitHead {
                                    commit: head,
                                    branch,
                                });
                        });
                    }
                    let registry_client_for_restorable = self.session_registry_client();
                    let registry_sid_for_restorable = arguments.session_id.to_string();
                    let err_ctx = ctx.clone();
                    if let Some(deadline) = upload_deadline {
                        match complete_prompt_trace(
                                ctx,
                                permission_events,
                                session_copy_rx,
                                turn_messages,
                                streaming_partial,
                                UploadWait::Defer { deadline },
                            )
                            .await
                        {
                            Ok(true) => {
                                if let Some(client) = registry_client_for_restorable {
                                    advance_restorable_turn(
                                            client,
                                            registry_sid_for_restorable,
                                            registry_turn,
                                        )
                                        .await;
                                }
                            }
                            Ok(false) => {
                                tracing::debug!(
                                    "session state unconfirmed within the flush budget; \
                                     skipping restorable_turn_number advance"
                                );
                            }
                            Err(e) => {
                                tracing::warn!("Failed to complete prompt trace: {e:?}");
                                crate::upload::trace::flush_then_write_error_manifest(
                                        &err_ctx,
                                        deadline,
                                    )
                                    .await;
                            }
                        }
                    } else {
                        spawn_upload_task(
                            "after_uploads",
                            async move {
                                match complete_prompt_trace(
                                        ctx,
                                        permission_events,
                                        session_copy_rx,
                                        turn_messages,
                                        streaming_partial,
                                        UploadWait::Confirm,
                                    )
                                    .await
                                {
                                    Ok(true) => {
                                        if let Some(client) = registry_client_for_restorable {
                                            advance_restorable_turn(
                                                    client,
                                                    registry_sid_for_restorable,
                                                    registry_turn,
                                                )
                                                .await;
                                        }
                                    }
                                    Ok(false) => {
                                        tracing::warn!(
                                        "Session state upload failed; skipping registry \
                                         restorable_turn_number advance"
                                    );
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to complete prompt trace: {e:?}");
                                        write_error_manifest(&err_ctx).await;
                                    }
                                }
                            },
                        );
                    }
                }
                let last_turn_usage = last_turn_usage_for_meta;
                let cancellation_category = match &completion_kind {
                    crate::session::commands::PromptCompletionKind::Cancelled {
                        category: Some(cat),
                        ..
                    } => Some(format!("{cat:?}")),
                    crate::session::commands::PromptCompletionKind::MaxTurnsReached {
                        ..
                    } => Some("max_turns_reached".to_string()),
                    crate::session::commands::PromptCompletionKind::StationarityEnded => {
                        Some("action_stationarity".to_string())
                    }
                    _ => None,
                };
                Ok(
                    acp::PromptResponse::new(stop_reason)
                        .meta(
                            build_prompt_response_meta(PromptResponseMetaArgs {
                                    session_id: &arguments.session_id.to_string(),
                                    prompt_id: &prompt_id,
                                    total_tokens,
                                    model_id: &model,
                                    last_turn_usage: last_turn_usage.as_ref(),
                                    prompt_usage,
                                    cancellation_category,
                                    cancel_trigger,
                                    structured_output,
                                    tool_overrides: applied_tool_overrides,
                                })
                                .as_object()
                                .cloned(),
                        ),
                )
            }
            Err(err) => {
                let subagent_refs = self
                    .spawned_subagent_refs_for_prompt(
                        arguments.session_id.0.as_ref(),
                        &prompt_id,
                    )
                    .await;
                let turn_messages: Option<xai_chat_state::TurnCapture> = {
                    let (tx, rx) = oneshot::channel();
                    if handle
                        .cmd_tx
                        .send(SessionCommand::TakeTurnMessages {
                            respond_to: tx,
                        })
                        .is_ok()
                    {
                        rx.await.ok().flatten()
                    } else {
                        None
                    }
                };
                let err_kind_str = format!("{:?}", err.code);
                let streaming_partial = crate::upload::turn::take_streaming_partial(
                        &handle.cmd_tx,
                        prompt_id.clone(),
                        false,
                        Some(model.clone()),
                    )
                    .await
                    .map(|mut cap| {
                        cap.reason = Some(format!("sampler_error:{err_kind_str}"));
                        cap
                    });
                if let Some(ctx) = trace_context.clone() {
                    let request_id = prompt_id.clone();
                    let err_str = format!("{err:?}");
                    let stop_reason = crate::sampling::error::stop_reason_for_turn_error(
                            &err,
                        )
                        .to_string();
                    let upload_unified = matches!(
                        crate::sampling::error::http_status_from_error(&err),
                        Some(401 | 404),
                    );
                    let upload_deadline = block_for_upload
                        .then(|| tokio::time::Instant::now() + upload_flush_timeout);
                    if let Some(deadline) = upload_deadline {
                        let result = TurnResultMetadata {
                            schema_version: GCS_SCHEMA_VERSION,
                            request_id,
                            completed: false,
                            stop_reason: Some(stop_reason),
                            total_tokens: None,
                            input_tokens: None,
                            cached_input_tokens: None,
                            output_tokens: None,
                            error: Some(err_str),
                            finished_at: chrono::Utc::now().to_rfc3339(),
                            signals: None,
                            turn_delta: None,
                            start_prompt_mode: Some(prompt_mode.to_string()),
                            end_prompt_mode: None,
                            resolved_model: resolved_model.clone(),
                            subagents_spawned: subagent_refs.clone(),
                        };
                        let wait = UploadWait::Defer { deadline };
                        upload_turn_result(&ctx, &result, wait).await;
                        if let Some(capture) = turn_messages {
                            upload_turn_messages(&ctx, capture, wait).await;
                        }
                        if let Some(ref capture) = streaming_partial {
                            crate::upload::trace::upload_streaming_partial(
                                    &ctx,
                                    capture,
                                    wait,
                                )
                                .await;
                        }
                        if upload_unified {
                            upload_unified_log(&ctx, wait).await;
                        }
                        crate::upload::trace::flush_then_write_error_manifest(
                                &ctx,
                                deadline,
                            )
                            .await;
                    } else {
                        let resolved_model = resolved_model.clone();
                        spawn_upload_task(
                            "error_turn_result",
                            async move {
                                let result = TurnResultMetadata {
                                    schema_version: GCS_SCHEMA_VERSION,
                                    request_id,
                                    completed: false,
                                    stop_reason: Some(stop_reason),
                                    total_tokens: None,
                                    input_tokens: None,
                                    cached_input_tokens: None,
                                    output_tokens: None,
                                    error: Some(err_str),
                                    finished_at: chrono::Utc::now().to_rfc3339(),
                                    signals: None,
                                    turn_delta: None,
                                    start_prompt_mode: Some(prompt_mode.to_string()),
                                    end_prompt_mode: None,
                                    resolved_model,
                                    subagents_spawned: subagent_refs.clone(),
                                };
                                upload_turn_result(&ctx, &result, UploadWait::Confirm)
                                    .await;
                                if let Some(capture) = turn_messages {
                                    upload_turn_messages(&ctx, capture, UploadWait::Confirm)
                                        .await;
                                }
                                if let Some(ref capture) = streaming_partial {
                                    crate::upload::trace::upload_streaming_partial(
                                            &ctx,
                                            capture,
                                            UploadWait::Confirm,
                                        )
                                        .await;
                                }
                                if upload_unified {
                                    upload_unified_log(&ctx, UploadWait::Confirm).await;
                                }
                                write_error_manifest(&ctx).await;
                            },
                        );
                    }
                }
                let err = if crate::sampling::error::prompt_usage_from_error(&err)
                    .is_some()
                {
                    err
                } else {
                    let prompt_id = handle
                        .current_prompt_id
                        .lock()
                        .ok()
                        .and_then(|g| g.clone());
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let usage = if handle
                        .cmd_tx
                        .send(crate::session::commands::SessionCommand::ErrorPathUsageFallback {
                            prompt_id,
                            respond_to: tx,
                        })
                        .is_ok()
                    {
                        rx.await.ok().flatten()
                    } else {
                        None
                    };
                    crate::sampling::error::attach_prompt_usage(err, usage)
                };
                Err(err)
            }
        }
    }
    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        tracing::info!("Received cancel request {args:?}");
        let handle = self.session_handle_waiting_for_load(&args.session_id).await;
        let cancel_trigger = args
            .meta
            .as_ref()
            .and_then(|m| m.get("cancelTrigger"))
            .and_then(|v| v.as_str())
            .map(crate::session::CancelTrigger::from_client);
        xai_grok_telemetry::unified_log::info(
            "shell.cancel.received",
            Some(args.session_id.0.as_ref()),
            Some(
                serde_json::json!({
                "session_found": handle.is_some(),
                "trigger": cancel_trigger.as_ref().map(crate::session::CancelTrigger::as_str),
            }),
            ),
        );
        if let Some(handle) = handle {
            let cancel_subagents = args
                .meta
                .as_ref()
                .and_then(|m| m.get("cancelSubagents"))
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let rewind_if_no_output = args
                .meta
                .as_ref()
                .and_then(|m| {
                    m.get("rewindIfNoOutput").or_else(|| m.get("rewindIfPristine"))
                })
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let dispatch_lock = self.dispatch_lock(&args.session_id);
            let _dispatch_guard = dispatch_lock.lock().await;
            let _ = handle
                .cmd_tx
                .send(
                    SessionCommand::Cancel(crate::session::CancelOptions {
                        cancel_subagents,
                        rewind_if_no_output,
                        trigger: cancel_trigger,
                        user_initiated: true,
                        ..Default::default()
                    }),
                );
        }
        Ok(())
    }
    async fn set_session_mode(
        &self,
        args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        tracing::info!("Received set session mode request {args:?}");
        let handle = self.session_handle_waiting_for_load(&args.session_id).await;
        let (tx, rx) = oneshot::channel();
        if let Some(handle) = handle {
            let _ = handle
                .cmd_tx
                .send(SessionCommand::SessionMode {
                    session_mode: args.mode_id,
                    responds_to: tx,
                });
        }
        let _ = rx
            .await
            .map_err(|_| {
                acp::Error::internal_error().data("response to set session failed")
            })?;
        Ok(acp::SetSessionModeResponse::new())
    }
    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        // Own failure telemetry until the public ACP validation gates pass.
        // `apply` creates the actor-phase guard after this handoff, so every
        // rejection emits exactly once without misclassifying validation as a
        // harness rebuild failure.
        let mut validation_failure_telemetry =
            crate::agent::handlers::model_switch::FailureTelemetry::new(
                &args.session_id,
                &args.model_id,
            );
        // Authorization is deliberately deferred to `apply`, after it owns
        // the session dispatch lock. A picker snapshot here would race auth,
        // allowlist, and catalog mutation before actor dispatch.
        validation_failure_telemetry.disarm();
        crate::agent::handlers::model_switch::apply(self, args).await
    }
    #[tracing::instrument(
        name = "agent.ext_method",
        skip_all,
        fields(method = %args.method)
    )]
    async fn ext_method(
        &self,
        args: acp::ExtRequest,
    ) -> Result<acp::ExtResponse, acp::Error> {
        let request_meta = serde_json::from_str::<serde_json::Value>(args.params.get())
            .ok()
            .and_then(|v| v.get("_meta").cloned());
        if let Some(meta) = &request_meta {
            xai_file_utils::trace_context::link_current_span_to_meta(meta);
        }
        tracing::info!("Received extension method call: method={}", args.method);
        #[allow(unused_mut)]
        let mut backend_no_bridge_err: Option<acp::Error> = None;
        let method = args.method.clone();
        let result = match method.as_ref() {
            "x.ai/getApiKey" | "x.ai/setApiKey" => {
                crate::extensions::auth::handle(self, &args).await
            }
            "x.ai/session/info" | "x.ai/session/close" | "x.ai/session/list"
            | "x.ai/sessions/list" => {
                crate::agent::handlers::session::handle(self, &args).await
            }
            "x.ai/workspaces/list" => {
                crate::agent::handlers::workspaces::handle(self, &args).await
            }
            "x.ai/session/updates" => {
                crate::extensions::session_updates::handle(&args, &self.gateway).await
            }
            "x.ai/session/state" => {
                crate::extensions::session_state::handle_state(&args).await
            }
            "x.ai/session/import" => {
                crate::extensions::session_state::handle_import(&args).await
            }
            "x.ai/session/load_history" => {
                crate::extensions::chat_conversation_history::handle(self, &args).await
            }
            "x.ai/session/search" => {
                crate::extensions::session_search::handle(&args).await
            }
            "x.ai/session/resolve_local_for_worktree_resume"
            | "x.ai/session/rehydrate" => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::worktree::handle(self, &ops, &args).await
            }
            #[cfg(feature = "local-workspace")]
            "x.ai/session/add_local_workspace" => {
                crate::extensions::session_admin::handle(self, &args).await
            }
            "x.ai/session/rename" | "x.ai/session/delete"
            | "x.ai/session/update_mcp_servers" | "x.ai/session/fork"
            | "x.ai/plugins/reload" | "x.ai/commands/list" => {
                crate::extensions::session_admin::handle(self, &args).await
            }
            m if InternalMethod::from_name(m).is_some() => {
                crate::extensions::session_admin::handle(self, &args).await
            }
            "x.ai/session/repair" => crate::extensions::repair::handle(self, &args).await,
            "x.ai/session/usage" => crate::extensions::usage::handle(self, &args).await,
            "x.ai/memory/flush" | "x.ai/memory/rewrite" => {
                crate::extensions::memory::handle(self, &args).await
            }
            "x.ai/skills/refresh-baseline" => {
                self.refresh_skill_baseline_for_all_sessions();
                crate::extensions::to_ext_response(
                    Ok(serde_json::json!({"ok": true})),
                )
            }
            "x.ai/interject" => crate::extensions::interject::handle(self, &args).await,
            "x.ai/feedback" | "x.ai/feedback/dismiss" | "x.ai/btw" => {
                crate::extensions::feedback::handle(self, &args).await
            }
            "x.ai/recap" => crate::extensions::recap::handle(self, &args).await,
            "x.ai/cloud/terminate" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    crate::auth::with_login_instruction(
                        |prog| format!("Run `{prog} login` to authenticate."),
                        "Sign in again to authenticate.",
                    ),
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let sandbox_id = params
                    .get("sandbox_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::invalid_params().data("missing sandbox_id")
                    })?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                sandbox_client
                    .terminate_session(
                        sandbox_id,
                        &crate::remote::SandboxTerminateRequest {
                            environment_id: None,
                        },
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to terminate sandbox: {e}"))
                    })?;
                crate::extensions::to_raw_response(&serde_json::json!({ "ok": true }))
            }
            "x.ai/cloud/env/list" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    crate::auth::with_login_instruction(
                        |prog| format!("Run `{prog} login` to authenticate."),
                        "Sign in again to authenticate.",
                    ),
                )?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                let resp = sandbox_client
                    .list_environments(
                        &crate::remote::SandboxListEnvironmentsRequest::default(),
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to list environments: {e}"))
                    })?;
                crate::extensions::to_raw_response(
                    &serde_json::json!({
                    "environments": resp.environments,
                }),
                )
            }
            "x.ai/cloud/env/create" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    crate::auth::with_login_instruction(
                        |prog| format!("Run `{prog} login` to authenticate."),
                        "Sign in again to authenticate.",
                    ),
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                let resp = sandbox_client
                    .create_environment(
                        &crate::remote::SandboxCreateEnvironmentRequest {
                            name: params
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            description: params
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            repository: params
                                .get("repository")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            default_branch: params
                                .get("default_branch")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            container_image: params
                                .get("container_image")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            setup_script: params
                                .get("setup_script")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            workspace_directory: Some("/workspace".to_string()),
                            internet_enabled: Some(true),
                            domain_allowlist_preset: Some("common".to_string()),
                            allowed_http_methods: Some("all".to_string()),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to create environment: {e}"))
                    })?;
                crate::extensions::to_raw_response(
                    &serde_json::json!({
                    "environment": resp.environment,
                }),
                )
            }
            "x.ai/cloud/env/update" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    crate::auth::with_login_instruction(
                        |prog| format!("Run `{prog} login` to authenticate."),
                        "Sign in again to authenticate.",
                    ),
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let environment_id = params
                    .get("environment_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::invalid_params().data("missing environment_id")
                    })?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                let resp = sandbox_client
                    .update_environment(
                        environment_id,
                        &crate::remote::SandboxUpdateEnvironmentRequest {
                            name: params
                                .get("name")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            description: params
                                .get("description")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            repository: params
                                .get("repository")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            default_branch: params
                                .get("default_branch")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            container_image: params
                                .get("container_image")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            setup_script: params
                                .get("setup_script")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to update environment: {e}"))
                    })?;
                crate::extensions::to_raw_response(
                    &serde_json::json!({
                    "environment": resp.environment,
                }),
                )
            }
            "x.ai/cloud/env/delete" => {
                crate::extensions::auth_gate::require_xai_auth(
                    &self.auth_manager,
                    "Authentication required",
                    crate::auth::with_login_instruction(
                        |prog| format!("Run `{prog} login` to authenticate."),
                        "Sign in again to authenticate.",
                    ),
                )?;
                let params: serde_json::Value = serde_json::from_str(args.params.get())
                    .map_err(|e| acp::Error::invalid_params().data(e.to_string()))?;
                let environment_id = params
                    .get("environment_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        acp::Error::invalid_params().data("missing environment_id")
                    })?;
                let sandbox_client = crate::remote::SandboxClient::new(
                    self.cli_chat_proxy_base_url(),
                    self.auth_manager.clone(),
                );
                sandbox_client
                    .delete_environment(environment_id)
                    .await
                    .map_err(|e| {
                        acp::Error::internal_error()
                            .data(format!("Failed to delete environment: {e}"))
                    })?;
                crate::extensions::to_raw_response(&serde_json::json!({ "ok": true }))
            }
            "x.ai/billing" => crate::extensions::billing::handle(self, &args).await,
            "x.ai/auto-topup-rule" => {
                crate::extensions::billing::handle(self, &args).await
            }
            "x.ai/share_session" => crate::extensions::share::handle(self, &args).await,
            "x.ai/privacy/setCodingDataRetention" => {
                crate::extensions::privacy::handle(self, &args).await
            }
            "x.ai/rollout/survey" => {
                crate::extensions::rollout::handle(self, &args).await
            }
            "x.ai/prompt_history" => {
                crate::extensions::prompt_history::handle(self, &args).await
            }
            "x.ai/suggest" => crate::extensions::suggest::handle(self, &args).await,
            "x.ai/suggestPrompt" => crate::extensions::suggest::handle(self, &args).await,
            s if s.starts_with("x.ai/auth/") => {
                crate::extensions::auth::handle(self, &args).await
            }
            s if s.starts_with("x.ai/session_summaries/") => {
                crate::agent::handlers::session::handle(self, &args).await
            }
            s if s.starts_with("x.ai/git/worktree/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::worktree::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/git/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::git::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/compact_conversation") => {
                crate::extensions::memory::handle(self, &args).await
            }
            s if s.starts_with("x.ai/plugins/") => {
                crate::extensions::plugins::handle(self, &args).await
            }
            s if s.starts_with("x.ai/marketplace/") => {
                crate::extensions::marketplace::handle(self, &args).await
            }
            s if s.starts_with("x.ai/hooks/") => {
                crate::extensions::hooks::handle(self, &args).await
            }
            s if s.starts_with("x.ai/hunk-tracker/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::hunk_tracker::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/pr/") => {
                crate::extensions::pr::handle(self, &args).await
            }
            s if s.starts_with(crate::extensions::mcp::mcp_methods::PREFIX) => {
                crate::extensions::mcp::handle(self, &args).await
            }
            s if s.starts_with("x.ai/task/") => {
                crate::extensions::task::handle(self, &args).await
            }
            s if s.starts_with("x.ai/scheduler/") => {
                crate::extensions::task::handle_scheduler(self, &args).await
            }
            s if s.starts_with("x.ai/subagent/") => {
                crate::extensions::task::handle_subagent(self, &args).await
            }
            s if s.starts_with("x.ai/terminal/") => {
                crate::extensions::terminal::handle(self, &args).await
            }
            s if crate::extensions::fs::is_fs_method(s) => {
                crate::extensions::fs::handle(self, &args).await
            }
            s if s.starts_with("x.ai/search/") => {
                crate::extensions::search::handle(self, &args).await
            }
            s if s.starts_with("x.ai/bundle/") => {
                crate::extensions::bundle::handle(self, &args).await
            }
            s if s.starts_with("x.ai/code/") => {
                let ops = self.resolve_workspace_ops()?;
                crate::extensions::code_nav::handle(self, &ops, &args).await
            }
            s if s.starts_with("x.ai/skills/") || s == "x.ai/workflows/list" => {
                let compat = self.cfg.borrow().compat_resolved;
                crate::extensions::skills::handle(
                        self,
                        &args,
                        self.plugin_registry_handle.snapshot().as_deref(),
                        compat,
                    )
                    .await
            }
            s if s.starts_with("x.ai/review") => {
                crate::extensions::feedback::handle(self, &args).await
            }
            s if s.starts_with("x.ai/debug/") => {
                crate::extensions::debug::handle(self, &args).await
            }
            s if s.starts_with("x.ai/rewind") => {
                crate::extensions::rewind::handle(self, &args).await
            }
            other => {
                Err(
                    acp::Error::method_not_found()
                        .data(format!("unknown ACP extension method: {other}")),
                )
            }
        };
        if let Some(err) = backend_no_bridge_err
            && matches!(&result, Err(e) if e.code == acp::Error::method_not_found().code)
        {
            return Err(err);
        }
        result
    }
    async fn ext_notification(
        &self,
        args: acp::ExtNotification,
    ) -> Result<(), acp::Error> {
        tracing::info!("Received extension notification: method={}", args.method);
        if args.method.as_ref() == "x.ai/yolo_mode_changed"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let sender_id = params.get("clientIdentifier").and_then(|v| v.as_str());
            let permission_mode = params
                .get("permission_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let yolo_signal = params.get("yolo_mode").and_then(|v| v.as_bool());
            if let Some(yolo_mode) = yolo_signal {
                let mut updated_sessions = 0;
                self.session_registry
                    .for_each_resident_mut(|_, handle| {
                        updated_sessions
                            += apply_yolo_mode_to_matching_sessions(
                                std::iter::once(handle),
                                sender_id,
                                yolo_mode,
                            );
                    });
                tracing::info!(
                    yolo_mode,
                    sender = ?sender_id,
                    target_sessions = updated_sessions,
                    total_sessions = self.resident_count(),
                    "Setting YOLO mode for matching sessions"
                );
            }
            let auto_mode_explicit = params.get("auto_mode").and_then(|v| v.as_bool());
            let want_auto = auto_mode_explicit == Some(true)
                || permission_mode == "auto";
            let clear_auto = auto_mode_explicit == Some(false)
                || (matches!(permission_mode, "always-approve" | "ask" | "default")
                    && !want_auto);
            let enable_auto = want_auto && yolo_signal != Some(true);
            if enable_auto || clear_auto {
                let enabled = enable_auto;
                let matches_sender = |h: &crate::session::SessionHandle| -> bool {
                    sender_id.is_none()
                        || h.origin_client.as_ref().map(|c| c.product.as_str())
                            == sender_id
                };
                let total_sessions = self.resident_count();
                let mut updated = 0;
                self.session_registry
                    .for_each_resident_mut(|_, h| {
                        if !matches_sender(h) {
                            return;
                        }
                        if h
                            .cmd_tx
                            .send(crate::session::SessionCommand::SetAutoMode {
                                enabled,
                            })
                            .is_ok()
                        {
                            if enabled {
                                h.yolo_mode = false;
                            }
                            updated += 1;
                        }
                    });
                tracing::info!(
                    auto_mode = enabled,
                    sender = ?sender_id,
                    target_sessions = updated,
                    total_sessions,
                    "Setting auto permission mode for matching sessions"
                );
            }
        }
        if args.method.as_ref() == "x.ai/permissions/reset" {
            let mut updated = 0;
            self.session_registry
                .for_each_resident(|_, h| {
                    if h
                        .cmd_tx
                        .send(crate::session::SessionCommand::ResetPermissionState)
                        .is_ok()
                    {
                        updated += 1;
                    }
                });
            tracing::info!(
                target_sessions = updated,
                total_sessions = self.resident_count(),
                "Permission state reset for matching sessions"
            );
        }
        if args.method.as_ref() == InternalMethod::EvictSessions.name() {
            self.handle_evict_sessions(&args.params).await;
        }
        if args.method.as_ref() == "x.ai/toggle_plan_mode"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let session_id_str = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let handle = self.resident_handle(&acp::SessionId::new(session_id_str));
            if let Some(handle) = handle {
                let is_engaged = handle.plan_mode.lock().state()
                    != crate::session::plan_mode::PlanModeState::Inactive;
                let next_mode_id = acp::SessionModeId::new(
                    if is_engaged { "default" } else { "plan" },
                );
                let (tx, rx) = oneshot::channel();
                let _ = handle
                    .cmd_tx
                    .send(SessionCommand::SessionMode {
                        session_mode: next_mode_id.clone(),
                        responds_to: tx,
                    });
                if rx.await.is_err() {
                    tracing::warn!(
                        session_id = %session_id_str,
                        mode_id = %next_mode_id.0,
                        "toggle_plan_mode: session mode update failed"
                    );
                }
            } else {
                tracing::warn!(
                    session_id = %session_id_str,
                    "toggle_plan_mode: session not found"
                );
            }
        }
        if args.method.as_ref().starts_with("x.ai/queue/")
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            let owner = params
                .get("owner")
                .or_else(|| params.get("clientIdentifier"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(cmd) = crate::agent::ext_parsers::parse_queue_edit_command(
                args.method.as_ref(),
                &params,
                owner,
            ) {
                let session_id_str = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(handle) = self
                    .resident_handle(&acp::SessionId::new(session_id_str))
                {
                    if handle.cmd_tx.send(cmd).is_err() {
                        tracing::warn!(
                            session_id = %session_id_str,
                            method = %args.method,
                            "queue edit: failed to forward SessionCommand (session actor gone)"
                        );
                    }
                } else {
                    tracing::warn!(
                        session_id = %session_id_str,
                        method = %args.method,
                        "queue edit: session not found"
                    );
                }
            }
        }
        if args.method.as_ref() == "x.ai/terminal/pty/input"
            && let Ok(params) = serde_json::from_str::<
                serde_json::Value,
            >(args.params.get())
        {
            crate::extensions::terminal::handle_pty_input(&params).await;
        }
        if args.method.as_ref() == "_x.ai/session/update" {
            if let Ok(notification) = serde_json::from_str::<
                SessionNotification,
            >(args.params.get()) {
                tracing::info!(
                    "Storing xAI session notification: session_id={}",
                    notification.session_id.0
                );
                if let Some(handle) = self.resident_handle(&notification.session_id) {
                    let _ = handle
                        .cmd_tx
                        .send(crate::session::SessionCommand::XaiSessionNotification {
                            notification,
                        });
                } else {
                    tracing::warn!(
                        "Received xAI session notification for unknown session: {}",
                        notification.session_id.0
                    );
                }
            } else {
                tracing::warn!("Failed to parse xAI session notification params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/non_git_decision" {
            #[derive(serde::Deserialize)]
            struct NonGitDecisionParams {
                decision: String,
                session_id: String,
                #[serde(default)]
                client_version: Option<String>,
            }
            if let Ok(params) = serde_json::from_str::<
                NonGitDecisionParams,
            >(args.params.get()) {
                tracing::info!(
                    decision = %params.decision,
                    session_id = %params.session_id,
                    client_version = ?params.client_version,
                    "non_git_decision",
                );
                xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::NonGitDecisionEvent {
                    decision: params.decision,
                    session_id: params.session_id,
                    client_version: params.client_version,
                });
            } else {
                tracing::warn!("Failed to parse non_git_decision telemetry params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/multi_agent_followup" {
            #[derive(serde::Deserialize)]
            struct MultiAgentFollowupParams {
                preferred_agent_label: char,
                preferred_agent_session_id: Option<String>,
                preferred_agent_model_id: Option<String>,
                /// (label, session_id, model_id)
                other_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentFollowupParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent followup telemetry: preferred_agent={}",
                    params.preferred_agent_label
                );
                let total_agents = 1 + params.other_agents.len();
                xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::MultiAgentFollowup {
                    preferred_agent_label: params.preferred_agent_label.to_string(),
                    preferred_agent_session_id: params.preferred_agent_session_id,
                    preferred_agent_model_id: params.preferred_agent_model_id,
                    other_agents: params
                        .other_agents
                        .into_iter()
                        .map(|(l, s, m)| xai_grok_telemetry::events::AgentInfo {
                            label: l.to_string(),
                            session_id: s,
                            model_id: m,
                        })
                        .collect(),
                    total_agents,
                });
            } else {
                tracing::warn!("Failed to parse multi-agent followup telemetry params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/multi_agent_apply" {
            #[derive(serde::Deserialize)]
            struct MultiAgentApplyParams {
                applied_agent_label: char,
                applied_agent_session_id: Option<String>,
                applied_agent_model_id: Option<String>,
                /// (label, session_id, model_id)
                discarded_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentApplyParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent apply telemetry: applied_agent={}",
                    params.applied_agent_label
                );
                let total_agents = 1 + params.discarded_agents.len();
                xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::MultiAgentApply {
                    applied_agent_label: params.applied_agent_label.to_string(),
                    applied_agent_session_id: params.applied_agent_session_id,
                    applied_agent_model_id: params.applied_agent_model_id,
                    discarded_agents: params
                        .discarded_agents
                        .into_iter()
                        .map(|(l, s, m)| xai_grok_telemetry::events::AgentInfo {
                            label: l.to_string(),
                            session_id: s,
                            model_id: m,
                        })
                        .collect(),
                    total_agents,
                });
            } else {
                tracing::warn!("Failed to parse multi-agent apply telemetry params");
            }
        }
        if args.method.as_ref() == "x.ai/telemetry/multi_agent_discard" {
            #[derive(serde::Deserialize)]
            struct MultiAgentDiscardParams {
                /// (label, session_id, model_id)
                discarded_agents: Vec<(char, Option<String>, Option<String>)>,
            }
            if let Ok(params) = serde_json::from_str::<
                MultiAgentDiscardParams,
            >(args.params.get()) {
                tracing::info!(
                    "Logging multi-agent discard telemetry: {} agents discarded",
                    params.discarded_agents.len()
                );
                let total = params.discarded_agents.len();
                xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::MultiAgentDiscard {
                    discarded_agents: params
                        .discarded_agents
                        .into_iter()
                        .map(|(l, s, m)| xai_grok_telemetry::events::AgentInfo {
                            label: l.to_string(),
                            session_id: s,
                            model_id: m,
                        })
                        .collect(),
                    total_agents_discarded: total,
                });
            } else {
                tracing::warn!("Failed to parse multi-agent discard telemetry params");
            }
        }
        if args.method.as_ref() == xai_grok_telemetry::unified_log::LOG_METHOD
            && let Ok(params) = serde_json::from_str::<
                xai_grok_telemetry::unified_log::LogNotificationParams,
            >(args.params.get())
        {
            xai_grok_telemetry::unified_log::ingest_client_entries(
                params.src,
                &params.entries,
            );
        }
        Ok(())
    }
}
#[cfg(test)]
mod tool_overrides_capability_tests {
    use super::tool_overrides_capability;
    #[test]
    fn capability_wire_shape_is_pinned() {
        assert_eq!(
            tool_overrides_capability(),
            serde_json::json!({
                "x_keyword_search": true,
                "x_semantic_search": true,
                "x_user_search": false,
                "x_thread_fetch": false,
            }),
        );
    }
}
