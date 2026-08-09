//! Sampler-turn pipeline for `SessionActor`: tool definitions, model auth
//! facts/gates and retry, sampler config reconstruction, sampling-failure
//! recovery, and per-response usage recording.
use super::*;

/// Whether a resolved [`ModelAuthFacts`] may be frozen in
/// [`SessionActor::model_auth_memo`].
///
/// Only `byok = Unknown` is non-cacheable. Production resolution binds
/// incomplete knowledge (`CatalogUnavailable` / empty id → `UnidentifiedModel`)
/// to `byok = Unknown` already, so a readiness-side clause would be redundant
/// for live resolves and would only disarm hand-seeded memos that deliberately
/// pair a definite `NotByok` with a transient readiness for regression coverage
/// (#159 F2). A genuine authoritative `NotInCatalog` stays `NotByok` and may
/// cache; catalog-generation invalidation (F1) prevents that freeze from
/// outliving a refresh that restores the model.
fn model_auth_facts_are_cacheable(facts: &crate::agent::config::ModelAuthFacts) -> bool {
    use crate::agent::auth_method::ModelByok;
    facts.byok != ModelByok::Unknown
}

/// Auth-failure detector for tool errors. Matches strictly on HTTP 401
/// when the error carries a structured status code, mirroring
/// `SamplingError::is_auth_error` in xai-grok-sampling-types: 403 is
/// deliberately excluded because it means "authenticated but forbidden"
/// (content-safety blocks, ZDR-gated requests, remote settings gates), where
/// a token refresh would be a no-op and would surface to the client as
/// a spurious auth_required teardown.
///
/// String fallbacks remain for tools that surface auth failures without
/// going through the structured `HttpFailure` path (e.g. JSON-only
/// `invalid_token` payloads, BYOK key-validation messages).
pub(super) fn is_auth_tool_error(err: &xai_tool_runtime::ToolError) -> bool {
    if let Some(details) = &err.details {
        if details
            .get(xai_grok_tools::types::PROVIDER_AUTH_RETRY_HANDLED_DETAILS_KEY)
            .and_then(|handled| handled.as_bool())
            == Some(true)
        {
            return false;
        }
        if let Some(status) = details
            .get(HTTP_STATUS_DETAILS_KEY)
            .and_then(|s| s.as_u64())
        {
            return status == 401;
        }
    }
    let lower = err.to_string().to_ascii_lowercase();
    lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("invalid_token")
}
/// Gate inputs bundled with the composed decision so the 401-recovery log can
/// report the components.
#[derive(Clone, Copy)]
struct SessionTokenAuthGate {
    is_session_based: bool,
    model_byok: crate::agent::auth_method::ModelByok,
    /// Whether the request targets a first-party host. Lets an `Unknown`
    /// BYOK status still refresh against cli-chat-proxy / `*.x.ai` without
    /// risking a session-token leak to a third-party BYOK endpoint.
    endpoint_is_first_party: bool,
    /// Active model's transport auth scheme. `AuthScheme::None` forces the
    /// gate off so session bearers are never attached or refreshed after a
    /// model switch to a keyless local endpoint.
    auth_scheme: xai_grok_sampler::AuthScheme,
    /// The model is authenticated by a credential header the user declared.
    /// That is terminal auth: the session must neither be attached on top of
    /// it nor invoked to "recover" when the provider rejects it (#110).
    declared_credential_header: bool,
}
impl SessionTokenAuthGate {
    /// Single place `is_session_based` / `endpoint_is_first_party` are derived,
    /// so all call sites assemble the gate identically.
    fn new(
        auth_method_id: Option<&acp::AuthMethodId>,
        model_byok: crate::agent::auth_method::ModelByok,
        base_url: &str,
        endpoint_trust: Option<xai_grok_sampler::EndpointTrustClass>,
        auth_scheme: xai_grok_sampler::AuthScheme,
        extra_headers: &indexmap::IndexMap<String, String>,
        env_http_headers: &indexmap::IndexMap<String, String>,
    ) -> Self {
        Self {
            is_session_based: auth_method_id
                .is_some_and(crate::agent::auth_method::is_session_based_method),
            model_byok,
            // An explicit trust class wins, mirroring `resolve_endpoint_trust`
            // in the sampler, so the two layers cannot disagree about the same
            // config. Otherwise derive it from the URL with the *attach-side*
            // predicate: https required and loopback refused, unlike the
            // refusal-side `is_xai_api_url` this used to call (#110).
            // `UserDeclared` (#123) counts as attach-allowed: the user named
            // the origin in local config precisely so the session bearer may
            // reach it and stay refreshed.
            endpoint_is_first_party: match endpoint_trust {
                Some(trust) => matches!(
                    trust,
                    xai_grok_sampler::EndpointTrustClass::FirstPartyXai
                        | xai_grok_sampler::EndpointTrustClass::UserDeclared
                ),
                None => crate::util::is_xai_api_bearer_url(base_url),
            },
            auth_scheme,
            // Derived here rather than passed in, for the same reason
            // `endpoint_is_first_party` is: a call site that has to remember
            // to compute it is a call site that will eventually forget.
            declared_credential_header: crate::agent::config::explicit_credential_header_in(
                extra_headers,
                env_http_headers,
            )
            .is_some(),
        }
    }
    fn active(self) -> bool {
        if self.auth_scheme == xai_grok_sampler::AuthScheme::None || self.declared_credential_header
        {
            return false;
        }
        crate::agent::auth_method::session_token_auth_gate(
            self.is_session_based,
            self.model_byok,
            self.endpoint_is_first_party,
        )
    }
}
/// Run a tool call; on an auth-shaped failure, attempt recovery via
/// `AuthManager` and one retry. When `shared_recovery` is `Some`, concurrent
/// 401s in the same batch deduplicate via `OnceCell::get_or_init`.
pub(super) async fn call_with_auth_retry<F, Fut>(
    auth_manager: Option<&std::sync::Arc<crate::auth::AuthManager>>,
    shared_recovery: Option<&tokio::sync::OnceCell<bool>>,
    tool_name: &str,
    mut call: F,
) -> Result<xai_grok_tools::types::output::ToolRunResult, xai_tool_runtime::ToolError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<
            Output = Result<
                xai_grok_tools::types::output::ToolRunResult,
                xai_tool_runtime::ToolError,
            >,
        >,
{
    let result = call().await;
    let Err(ref err) = result else { return result };
    if !is_auth_tool_error(err) {
        return result;
    }
    let Some(am) = auth_manager else {
        return result;
    };
    let src = crate::auth::recovery::RecoverySource::Background;
    let recovered = match shared_recovery {
        Some(cell) => *cell.get_or_init(|| am.try_recover_unauthorized(src)).await,
        None => am.try_recover_unauthorized(src).await,
    };
    if recovered {
        tracing::info!(
            tool = tool_name,
            "auth recovery: tool 401, recovered, retrying"
        );
        call().await
    } else {
        tracing::warn!(tool = tool_name, "auth recovery: tool 401, refresh failed");
        xai_grok_telemetry::unified_log::warn(
            "auth recovery: tool 401, refresh failed",
            None,
            Some(serde_json::json!({ "tool": tool_name })),
        );
        result
    }
}
impl SessionActor {
    pub(super) async fn prepare_tool_definitions_timed(&self) -> (Vec<ToolDefinition>, u64) {
        let mcp_wait_start = std::time::Instant::now();
        match self.mcp_strategy {
            McpInitStrategy::Blocking => {
                if !self.mcp_state.lock().await.is_initialized() {
                    tracing::info!(
                        "Blocking strategy: waiting for MCP initialization before first prompt..."
                    );
                    self.wait_for_mcp_initialized().await;
                }
            }
            McpInitStrategy::Progressive => {}
        }
        let mcp_wait_ms = mcp_wait_start.elapsed().as_millis() as u64;
        let defs = self.prepare_tool_definitions_inner().await;
        (defs, mcp_wait_ms)
    }
    pub(super) async fn prepare_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.prepare_tool_definitions_timed().await.0
    }
    /// The exact tool specs a turn sends, BEFORE the turn-specific
    /// structured-output append. Single source of truth shared by the turn
    /// (`acp_session_impl/turn.rs`) and the `SnapshotToolDefinitions` handler, so
    /// a verbatim-fork child's tool prefix can never silently drift from what the
    /// parent turn actually sends. `defs` is the already-resolved tool list
    /// (`prepare_tool_definitions_*`); this applies only the `web_search` drop
    /// under backend search and the `ToolSpec::from` mapping.
    pub(crate) fn turn_base_tool_specs(&self, defs: &[ToolDefinition]) -> Vec<ToolSpec> {
        let backend_search_active = self.backend_search_active();
        defs.iter()
            .filter(|td| !backend_search_active || td.function.name != "web_search")
            .cloned()
            .map(ToolSpec::from)
            .collect()
    }
    /// Hosted tools with overrides applied, plus the applied overrides to echo, in one pass.
    fn resolve_hosted(
        &self,
    ) -> (
        Vec<xai_grok_sampling_types::HostedTool>,
        xai_grok_sampling_types::ToolOverrides,
    ) {
        let mut tools = self.agent.borrow().hosted_tools().to_vec();
        let applied = xai_grok_sampling_types::apply_tool_overrides(
            &mut tools,
            self.tool_overrides.borrow().as_ref(),
        );
        (tools, applied)
    }
    /// Ungated. Prefer [`Self::hosted_tools_for_turn`], which folds in the backend-search gate.
    pub(crate) fn effective_hosted_tools(&self) -> Vec<xai_grok_sampling_types::HostedTool> {
        self.resolve_hosted().0
    }
    pub(crate) fn hosted_tools_for_turn(&self) -> Vec<xai_grok_sampling_types::HostedTool> {
        if self.backend_search_active() {
            self.effective_hosted_tools()
        } else {
            Vec::new()
        }
    }
    /// The applied overrides to echo, or `None` when backend search is off.
    pub(crate) fn effective_tool_overrides(
        &self,
    ) -> Option<xai_grok_sampling_types::ToolOverrides> {
        if !self.backend_search_active() {
            return None;
        }
        let applied = self.resolve_hosted().1;
        (!applied.is_empty()).then_some(applied)
    }
    pub(crate) fn backend_search_active(&self) -> bool {
        self.agent.borrow().backend_search_enabled() && self.supports_backend_search.get()
    }
    /// Set the per-turn override and emit it before any turn runs, so a subagent spawned this turn
    /// inherits it.
    pub(crate) fn set_tool_overrides(&self, overrides: xai_grok_sampling_types::ToolOverrides) {
        *self.tool_overrides.borrow_mut() = Some(overrides);
        self.emit_resolved_tool_overrides();
    }
    /// Fold a per-turn update at promotion: an object sets, `null` clears to the seed, absent leaves.
    pub(crate) fn apply_tool_overrides_update(
        &self,
        update: Option<xai_grok_sampling_types::ToolOverridesUpdate>,
    ) {
        let Some(update) = update else { return };
        {
            let mut slot = self.tool_overrides.borrow_mut();
            *slot = update.apply(slot.take());
        }
        self.emit_resolved_tool_overrides();
    }
    /// Store this session's cutoff in the cell a subagent spawn reads. Not gated on backend search,
    /// so a bounded parent bounds a searching child even if it isn't searching.
    pub(crate) fn emit_resolved_tool_overrides(&self) {
        let seed = self.agent.borrow().definition().tool_overrides.clone();
        let effective = resolve_configured_cutoff(seed, self.tool_overrides.borrow().as_ref());
        self.resolved_tool_overrides
            .store((!effective.is_empty()).then(|| std::sync::Arc::new(effective)));
    }
    pub(super) async fn prepare_tool_definitions_inner(&self) -> Vec<ToolDefinition> {
        let bridge = self.agent.borrow().tool_bridge().clone();
        let defs = bridge.tool_definitions_builtins_only().await;
        let plan_active = self.plan_mode.lock().is_active();
        filter_cursor_tools_by_plan_mode(defs, plan_active)
    }
    pub(super) fn model_auth_facts(&self, model_id: &str) -> crate::agent::config::ModelAuthFacts {
        self.model_auth_state(model_id).0
    }
    pub(super) fn model_auth_provider(
        &self,
        model_id: &str,
    ) -> Option<crate::auth::AuthProviderRef> {
        self.model_auth_state(model_id).1
    }
    /// Drop the memoized per-model auth state; see [`Self::model_auth_memo`]
    /// for why each model/credential chokepoint must call this.
    pub(crate) fn invalidate_model_auth_memo(&self) {
        self.model_auth_memo.replace(None);
    }
    /// Reads and populates [`Self::model_auth_memo`].
    ///
    /// A definite memo for the same model id at the current catalog generation
    /// is returned without re-resolve. Incomplete lookups (`byok = Unknown`)
    /// are never written; they return live so a later complete catalog can
    /// re-classify the model. (There is no "fresh Unknown falls back to last
    /// definite" arm: a same-id definite memo at the current generation would
    /// already have been served above.)
    fn model_auth_state(
        &self,
        model_id: &str,
    ) -> (
        crate::agent::config::ModelAuthFacts,
        Option<crate::auth::AuthProviderRef>,
    ) {
        use crate::session::acp_session::ModelAuthMemo;
        let catalog_generation = self.models_manager.catalog_generation();
        if let Some(memo) = self.model_auth_memo.borrow().as_ref()
            && memo.model_id == model_id
            && memo.catalog_generation == catalog_generation
            && model_auth_facts_are_cacheable(&memo.facts)
        {
            return (memo.facts.clone(), memo.provider.clone());
        }
        // Authoritative session catalog (defaults + prefetched + overrides).
        // Passing this is what keeps a remote-only model from being judged
        // NotInCatalog by a config-only re-resolve (#159).
        // Note: ModelsManager may retain-out `disabled_models` while a bare
        // config-only `resolve_model_list` does not — disabled entries can
        // therefore affect the auth verdict when the runtime catalog is used.
        let runtime_catalog = self.models_manager.models();
        let (fresh, provider) = crate::agent::config::resolve_model_auth_facts_and_provider(
            model_id,
            Some(&runtime_catalog),
        );
        if !model_auth_facts_are_cacheable(&fresh) {
            // Incomplete: do not freeze as a definite memo entry.
            return (fresh, provider);
        }
        *self.model_auth_memo.borrow_mut() = Some(ModelAuthMemo {
            model_id: model_id.to_string(),
            facts: fresh.clone(),
            provider: provider.clone(),
            catalog_generation,
        });
        (fresh, provider)
    }
    /// The single writer of a provider mint/rotation into chat-state credentials.
    ///
    /// Uses [`Credentials::rebind`] rather than [`Credentials::replace_api_key`]:
    /// the secret being written is provider-minted, not a rotation of whatever
    /// label chat-state already holds (which may be `Missing` from a pre-login
    /// spawn). Preserving that label would lie about provenance (#136).
    async fn set_chat_api_key(&self, new_key: String, provider_name: &str) {
        let creds = self.chat_state_handle.get_credentials().await;
        let creds = creds.rebind(
            Some(new_key),
            xai_chat_state::AuthType::ApiKey,
            xai_grok_sampler::CredentialSource::AuthProvider {
                name: provider_name.to_string(),
            },
        );
        self.chat_state_handle.update_credentials(creds);
    }
    /// Pre-turn arm for a provider-backed model: mint on a cold cache,
    /// re-mint near expiry, and adopt a rotation chat-state missed. No-op
    /// when `current_key` is already the fresh cached token.
    async fn refresh_provider_token_pre_turn(
        &self,
        provider: &crate::auth::AuthProviderRef,
        current_key: Option<&str>,
        model_id: &str,
    ) {
        match provider.ensure_fresh_token(current_key).await {
            crate::auth::ProviderRefreshOutcome::Rotated(new_key) => {
                tracing::info!(
                    model = %model_id,
                    provider = %provider.name,
                    cold = current_key.is_none(),
                    "auth provider token rotated pre-turn"
                );
                self.set_chat_api_key(new_key, &provider.name).await;
            }
            crate::auth::ProviderRefreshOutcome::Unchanged => {}
            crate::auth::ProviderRefreshOutcome::MintFailed => {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    provider = %provider.name,
                    model = %model_id,
                    "auth provider pre-turn refresh failed"
                );
                xai_grok_telemetry::unified_log::warn(
                    "auth provider pre-turn refresh failed",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "provider": provider.name,
                        "model": model_id,
                        "cold": current_key.is_none(),
                    })),
                );
            }
            crate::auth::ProviderRefreshOutcome::Unusable => {}
        }
    }
    /// 401 arm for a provider-backed model. Recovery is based on the
    /// credential relationship captured from the actual rejected request,
    /// never chat-state's potentially stale snapshot.
    async fn try_provider_401_recovery(
        &self,
        provider: &crate::auth::AuthProviderRef,
        request_credential: xai_grok_sampling_types::SentCredential,
    ) -> bool {
        let cached = provider.cached_token();
        let action = provider_401_recovery_action(request_credential, cached.is_some());
        let recovered = match action {
            Provider401RecoveryAction::AdoptCached => cached,
            Provider401RecoveryAction::RefreshServerRejected => {
                provider
                    .recover_rejected_token(
                        cached
                            .as_deref()
                            .expect("recovery action requires a current provider credential"),
                    )
                    .await
            }
            Provider401RecoveryAction::EnsureFresh => {
                provider.ensure_fresh_token(None).await.rotated()
            }
        };
        let Some(new_key) = recovered else {
            tracing::warn!(
                session_id = %self.session_info.id.0,
                provider = %provider.name,
                "auth recovery: sampler 401, provider re-mint declined or failed"
            );
            xai_grok_telemetry::unified_log::warn(
                "auth recovery: sampler 401, provider re-mint declined or failed",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({ "provider": provider.name })),
            );
            return false;
        };
        tracing::info!(
            session_id = %self.session_info.id.0,
            provider = %provider.name,
            "auth recovery: sampler 401, auth provider re-mint, retrying"
        );
        xai_grok_telemetry::unified_log::info(
            "auth recovery: sampler 401, auth provider re-mint, retrying",
            Some(self.session_info.id.0.as_ref()),
            None,
        );
        self.set_chat_api_key(new_key, &provider.name).await;
        true
    }
    /// Gate inputs for `model_id` routed to `base_url`. See
    /// [`crate::agent::auth_method::session_token_auth_gate`] for the rationale
    /// (`base_url` keeps an `Unknown` BYOK status refreshable only
    /// against first-party xAI hosts). Also inactive for
    /// [`xai_grok_sampler::AuthScheme::None`].
    fn auth_gate(
        &self,
        model_id: &str,
        base_url: &str,
        endpoint_trust: Option<xai_grok_sampler::EndpointTrustClass>,
        extra_headers: &indexmap::IndexMap<String, String>,
        env_http_headers: &indexmap::IndexMap<String, String>,
    ) -> SessionTokenAuthGate {
        let facts = self.model_auth_facts(model_id);
        let auth_method = self.auth_method_id.load();
        SessionTokenAuthGate::new(
            auth_method.as_deref(),
            facts.byok,
            base_url,
            endpoint_trust,
            facts.auth_scheme,
            extra_headers,
            env_http_headers,
        )
    }
    /// Emit a unified-log breadcrumb whenever the session-token refresh gate is
    /// evaluated with an **`Unknown`** per-model BYOK status on a session-based
    /// method — the condition that (pre-fix) silently demoted live sessions to
    /// stale-token 401s. The uploaded per-turn unified log then shows whether
    /// the first-party-endpoint fallback kept refresh active or withheld it, so
    /// we can confirm the fix works (or catch a residual demotion) per session
    /// even when server-side metrics only show the aggregate 401. No-op for a
    /// definite `Byok`/`NotByok`, so steady-state turns stay quiet — a burst of
    /// these is itself the signal that `Unknown` is being hit in the field.
    fn log_auth_gate_unknown(&self, site: &str, gate: SessionTokenAuthGate, base_url: &str) {
        use crate::agent::auth_method::ModelByok;
        if gate.model_byok != ModelByok::Unknown || !gate.is_session_based {
            return;
        }
        let refresh_active = gate.active();
        let ctx = serde_json::json!({
            "site": site,
            "model_byok": gate.model_byok.as_str(),
            "is_session_based": gate.is_session_based,
            "endpoint_is_first_party": gate.endpoint_is_first_party,
            "refresh_active": refresh_active,
        });
        let sid = Some(self.session_info.id.0.as_ref());
        if refresh_active {
            xai_grok_telemetry::unified_log::info(
                "auth gate: Unknown BYOK on first-party endpoint — session-token refresh kept active",
                sid,
                Some(ctx),
            );
        } else {
            xai_grok_telemetry::unified_log::warn(
                "auth gate: Unknown BYOK on non-first-party endpoint — refresh withheld (may surface stale-token 401)",
                sid,
                Some(ctx),
            );
        }
    }
    /// Reconstruct a full `SamplerConfig` (with credentials) by combining
    /// the actor's `SamplingConfig` and `Credentials`. Folds in the
    /// URL-derived headers (cli-chat-proxy auth, the staging auth header)
    /// so the sampler crate stays URL-agnostic.
    pub(super) async fn reconstruct_full_config(&self) -> SamplingConfig {
        #[allow(clippy::items_after_statements)]
        #[derive(Debug)]
        struct TraceContextInjector;
        impl xai_grok_sampler::HeaderInjector for TraceContextInjector {
            fn inject(&self, headers: &mut reqwest::header::HeaderMap) {
                if let Some(tp) = xai_file_utils::trace_context::current_traceparent()
                    && let Ok(v) = reqwest::header::HeaderValue::from_str(&tp)
                {
                    headers.insert("traceparent", v);
                }
            }
        }
        let cfg = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .unwrap_or_else(|| xai_grok_sampling_types::SamplingConfig {
                base_url: String::new(),
                model: String::new(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                endpoint_trust: None,
                api_backend: Default::default(),
                extra_headers: Default::default(),
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: std::num::NonZeroU64::new(256_000).unwrap(),
                reasoning_effort: None,
                stream_tool_calls: None,
            });
        let creds = self.chat_state_handle.get_credentials().await;
        let catalog_model_id = self.catalog_model_id_str();
        let (model_facts, model_auth_provider) = self.model_auth_state(&catalog_model_id);
        let auth_method = self.auth_method_id.load();
        let gate = SessionTokenAuthGate::new(
            auth_method.as_deref(),
            model_facts.byok,
            &cfg.base_url,
            cfg.endpoint_trust,
            model_facts.auth_scheme,
            &cfg.extra_headers,
            &cfg.env_http_headers,
        );
        // Security boundary: never attach a live session bearer (or rely on
        // gate.active alone) when the active model is keyless — even if the
        // ACP method is still session-based after a model switch.
        //
        // Readiness is tri-state (#133):
        // - `Unusable`: strip ambient credentials; prefer a prepare-time error
        //   over silently converting the turn to keyless on a non-xAI origin.
        // - `Unknown`: do NOT blanket-strip. Preserve a credential already
        //   bound to this model/endpoint (declared header / chat-state key
        //   when not borrowing ambient); refuse to BORROW session/provider
        //   ambient without catalog knowledge.
        let mut auth_scheme = model_facts.auth_scheme;
        // #110 / #136 / #180: provenance comes from `Credentials` alone.
        // After #136 the chat-state secret is bound with its source; after
        // #180 we must not re-derive `ExplicitHeader` from the header maps
        // here. A dual-auth gateway (model `api_key` + declared credential
        // header) is labelled `ModelApiKey` by `classify_credential_source`
        // and keeps its key — inventing `ExplicitHeader` from the maps while
        // leaving `api_key` in place made L3 treat a legitimate route as the
        // post-strip mislabel and refuse it.
        //
        // `declared_credential_header` still drives resolver attach and
        // identity gating (header still ships on the wire). Only the
        // provenance *label* stops coming from it. Without
        // `creds.source_cloned()`, ordinary Ready-model turns would emit
        // `credential_source: None` — the #151 hole on
        // `Unknown(CatalogUnavailable)`.
        let declared_credential_header = crate::agent::config::explicit_credential_header_in(
            &cfg.extra_headers,
            &cfg.env_http_headers,
        );
        let mut use_session_bearer_resolver = gate.active()
            && declared_credential_header.is_none()
            && auth_scheme != xai_grok_sampler::AuthScheme::None;
        let mut use_provider_bearer_resolver = cfg.api_backend
            == xai_grok_sampling_types::ApiBackend::CodexResponses
            && model_auth_provider.is_some()
            && auth_scheme != xai_grok_sampler::AuthScheme::None;
        let mut credential_source = creds.source_cloned();
        match &model_facts.readiness {
            crate::agent::auth_method::ModelReadiness::Ready => {}
            // The catalog answered and does not have this model. Before the
            // tri-state this was `ready = false`, i.e. stripped unconditionally,
            // and it must stay that way: this is the final wire choke point, and
            // "the catalog does not know this model" is exactly as much reason
            // to withhold as "chat state does not persist readiness" was. An
            // earlier version of this arm withheld only for session-based ACP
            // methods, which retained the chat-state key for `XAI_API_KEY`,
            // keyless and unrecognised methods -- and left `credential_source`
            // unset, so `SamplingClient::new` could not catch it either.
            crate::agent::auth_method::ModelReadiness::Unknown(
                crate::agent::auth_method::UnknownReason::NotInCatalog,
            ) => {
                tracing::warn!(
                    model = %catalog_model_id,
                    "reconstruct_full_config: model absent from the catalog; stripping credentials"
                );
                auth_scheme = xai_grok_sampler::AuthScheme::None;
                use_session_bearer_resolver = false;
                use_provider_bearer_resolver = false;
                // Credentials are gone. Do not keep a stored ambient/BYOK
                // source: that would claim a credential the strip just
                // removed. A declared credential header still ships in
                // `extra_headers` and must be labelled `ExplicitHeader`
                // (post-strip meaning); otherwise `Missing`.
                credential_source = match &declared_credential_header {
                    Some((header, env)) => {
                        Some(xai_grok_sampler::CredentialSource::ExplicitHeader {
                            header: header.clone(),
                            env: env.clone(),
                        })
                    }
                    None => Some(xai_grok_sampler::CredentialSource::Missing),
                };
            }
            // Knowledge is temporarily unobtainable, or there is no identified
            // target yet. Both were `ready = true` before the tri-state, and
            // both must stay that way: `session_token_auth_gate` documents that
            // an `Unknown` classification "must not demote a live session to
            // non-refreshable api-key mode", and clearing the resolvers here
            // does worse than that -- it sends no credential at all, so a
            // half-written `config.toml` 401s every turn until restart.
            //
            // This is the distinction the three `UnknownReason` variants exist
            // for. Collapsing them into one arm is what made the strip both
            // fail open (above) and fail closed (here) at the same time.
            //
            // #151: the chat-state key survives here, so the stored source
            // (set above) must survive with it — previously this path left
            // `credential_source: None` and L3 could not refuse ambient bytes
            // on an external origin.
            crate::agent::auth_method::ModelReadiness::Unknown(reason) => {
                tracing::debug!(
                    model = %catalog_model_id,
                    unknown = reason.as_str(),
                    "reconstruct_full_config: readiness unknown but not absent; leaving the session intact"
                );
            }
            crate::agent::auth_method::ModelReadiness::Unusable(reason) => {
                tracing::warn!(
                    model = %catalog_model_id,
                    reason = %reason.as_str(),
                    "reconstruct_full_config: active model unusable; stripping credentials"
                );
                auth_scheme = xai_grok_sampler::AuthScheme::None;
                use_session_bearer_resolver = false;
                use_provider_bearer_resolver = false;
                // Label the gap on Unusable alone, never on Unknown (#133).
                // A declared credential header still ships in `extra_headers`
                // and must be labelled `ExplicitHeader`; any other stored
                // source is overwritten with `Missing`.
                credential_source = match &declared_credential_header {
                    Some((header, env)) => {
                        Some(xai_grok_sampler::CredentialSource::ExplicitHeader {
                            header: header.clone(),
                            env: env.clone(),
                        })
                    }
                    None => Some(xai_grok_sampler::CredentialSource::Missing),
                };
            }
        }
        self.log_auth_gate_unknown("reconstruct_full_config", gate, &cfg.base_url);
        // Refresh before taking the initial bearer snapshot. The dynamic
        // resolver handles later rotations, while this keeps the first request
        // from starting with a stale chat-state credential.
        if use_session_bearer_resolver && let Some(am) = self.auth_manager.as_ref() {
            let _ = am.auth().await;
        }
        let mut extra_headers = cfg.extra_headers;
        crate::agent::config::inject_url_derived_headers(
            &mut extra_headers,
            creds.alpha_test_key(),
            &cfg.base_url,
        );
        let compaction_at_tokens = self.compaction_at_tokens.get();
        let compactions_remaining = self.compactions_remaining.get();
        if compactions_remaining.is_some() || compaction_at_tokens.is_some() {
            let has_compaction_summary = self
                .chat_state_handle
                .get_last_compaction_prompt_index()
                .await
                .is_some();
            if let Some(value) =
                compactions_remaining.and_then(|c| c.resolve(has_compaction_summary))
            {
                extra_headers.insert("x-compactions-remaining".to_string(), value.to_string());
            }
            if !has_compaction_summary
                && let Some(value) = compaction_at_tokens.and_then(|c| {
                    c.resolve(
                        cfg.context_window.get(),
                        self.compaction.threshold_percent.get(),
                    )
                })
            {
                extra_headers.insert("x-compaction-at".to_string(), value.to_string());
            }
        }
        // Security boundary: strip chat-state credentials for keyless/unusable
        // models so a stale session JWT cannot survive onto a custom endpoint.
        // For session-token auth, snapshot the freshly refreshed wire-valid
        // bearer; the resolver below supplies subsequent rotations.
        //
        // `Unknown`: preserve a bound credential (declared header already in
        // `credential_source`, or chat-state key when we are not borrowing
        // ambient). Ambient session/provider borrow was disabled above.
        // The first arm is what makes the strip unconditional: whatever cleared
        // `auth_scheme` above -- Unusable, or absent from the catalog -- meant
        // the credential too, for every auth method. An earlier version added a
        // later arm asking whether the ACP method was session-based instead,
        // which let the chat-state key through for `XAI_API_KEY`, keyless and
        // unrecognised methods.
        let api_key = if auth_scheme == xai_grok_sampler::AuthScheme::None {
            None
        } else if use_provider_bearer_resolver {
            model_auth_provider
                .as_ref()
                .and_then(crate::auth::AuthProviderRef::cached_token)
        } else if use_session_bearer_resolver {
            self.auth_manager
                .as_ref()
                .and_then(|am| am.current_wire_valid().map(|a| a.key))
        } else {
            creds.api_key_cloned()
        };
        // #136 step 4: a bound resolver *is* credential material. L3 refuses
        // `Missing`/`None` labels paired with material, so the label must name
        // the path that actually supplies the secret — not a pre-login gap
        // still sitting in chat state. Overwrite only when a resolver is
        // attached; BYOK / ExplicitHeader turns keep the stored source.
        if use_session_bearer_resolver {
            credential_source = Some(xai_grok_sampler::CredentialSource::XaiSession);
        } else if use_provider_bearer_resolver && let Some(provider) = model_auth_provider.as_ref()
        {
            credential_source = Some(xai_grok_sampler::CredentialSource::AuthProvider {
                name: provider.name.clone(),
            });
        }
        // Identity headers: gate on endpoint + credential-provider scope, not
        // catalog readiness (#133). Omit when no first-party identity is in play.
        let identity_in_scope = gate.endpoint_is_first_party
            && auth_scheme != xai_grok_sampler::AuthScheme::None
            && (use_session_bearer_resolver
                || use_provider_bearer_resolver
                || (api_key.is_some() && declared_credential_header.is_none()));
        let deployment_id = if identity_in_scope {
            crate::managed_config::resolve_deployment_id(
                crate::managed_config::resolve_deployment_key().as_deref(),
            )
        } else {
            None
        };
        let user_id = if identity_in_scope {
            self.auth_manager
                .as_ref()
                .and_then(|am| am.current_or_expired())
                .filter(|a| a.is_xai_auth())
                .map(|a| a.user_id)
        } else {
            None
        };
        SamplingConfig {
            api_key,
            base_url: cfg.base_url,
            model: cfg.model,
            max_completion_tokens: cfg.max_completion_tokens,
            temperature: cfg.temperature,
            top_p: cfg.top_p,
            endpoint_trust: cfg.endpoint_trust,
            credential_source,
            api_backend: cfg.api_backend,
            auth_scheme,
            extra_headers,
            query_params: cfg.query_params.clone(),
            env_http_headers: cfg.env_http_headers.clone(),
            context_window: cfg.context_window.get(),
            client_version: creds.client_version_cloned(),
            reasoning_effort: cfg.reasoning_effort,
            force_http1: false,
            max_retries: Some(self.max_retries),
            stream_tool_calls: cfg.stream_tool_calls.unwrap_or(false),
            idle_timeout_secs: None,
            client_identifier: self.client_identifier.clone(),
            deployment_id,
            user_id,
            origin_client: self.origin_client.clone(),
            attribution_callback: self.attribution_callback.clone(),
            bearer_resolver: if use_provider_bearer_resolver {
                model_auth_provider
                    .as_ref()
                    .map(crate::auth::AuthProviderRef::bearer_resolver)
            } else if use_session_bearer_resolver {
                self.auth_manager.as_ref().map(|am| {
                    crate::auth::credential_provider::WireValidBearerResolver::shared(am.clone())
                })
            } else {
                None
            },
            supports_backend_search: self.supports_backend_search.get(),
            compactions_remaining: self.compactions_remaining.get(),
            compaction_at_tokens: self.compaction_at_tokens.get(),
            doom_loop_recovery: self.doom_loop_recovery,
            header_injector: Some(std::sync::Arc::new(TraceContextInjector)),
            // Live catalog lookup so a model switch's wire caps follow the
            // selected entry rather than the session-start SamplingConfig (#245).
            codex_wire: self
                .models_manager
                .models()
                .get(catalog_model_id.as_str())
                .and_then(|e| e.info.codex_wire.clone()),
        }
    }
    /// Install auto-mode permission classifier with a live LLM side-query
    /// (laziness-classifier pattern: `prepare_chat_completion` +
    /// `conversation_collect` on a LocalSet task; channel bridges the
    /// `Send` permission actor). Heuristic runs only when the side-query
    /// errors or returns unparseable text.
    pub(crate) async fn wire_permission_auto_llm_classifier(self: &Arc<Self>) {
        if !self.permissions.is_auto_mode() {
            return;
        }
        if self.permissions.has_llm_side_query() {
            return;
        }
        let auto_cfg = crate::util::config::resolve_auto_mode_config_from_disk();
        let session_model = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        let aux_classifier_sampler = match auto_cfg.classifier_model.as_deref() {
            Some(slug) => self.resolve_auto_classifier_sampler(slug).await,
            None => None,
        };
        let models = self.models_manager.models();
        let effective_supports_re = crate::agent::config::effective_classifier_supports_re(
            aux_classifier_sampler
                .as_ref()
                .map(|(_, model)| model.as_str()),
            &session_model,
            &models,
        );
        let (prompt_type, classifier_reasoning_effort) =
            crate::util::config::auto_mode_classifier_defaults(&auto_cfg, effective_supports_re);
        let classify_timeout = crate::util::config::auto_mode_classify_timeout(&auto_cfg);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(
            Vec<xai_grok_workspace::permission::ClassifierMessage>,
            tokio::sync::oneshot::Sender<
                Result<String, xai_grok_workspace::permission::ClassifierFailure>,
            >,
        )>();
        let session = Arc::clone(self);
        tokio::task::spawn_local(async move {
            while let Some((messages, respond_to)) = rx.recv().await {
                let result = async {
                    let (sampling_client, model) = match &aux_classifier_sampler {
                        Some((client, model)) => (client.clone(), model.clone()),
                        None => {
                            let client = session
                                .prepare_chat_completion(false)
                                .await
                                .map_err(|e| xai_grok_workspace::permission::ClassifierFailure::TransportError(
                                    e.to_string(),
                                ))?;
                            let model = session
                                .chat_state_handle
                                .get_sampling_config()
                                .await
                                .map(|c| c.model)
                                .unwrap_or_default();
                            (client, model)
                        }
                    };
                    let session_id = session.session_info.id.to_string();
                    let items = messages
                        .into_iter()
                        .map(|m| match m.role {
                            xai_grok_workspace::permission::ClassifierMessageRole::System => {
                                ConversationItem::system(m.text)
                            }
                            xai_grok_workspace::permission::ClassifierMessageRole::User => {
                                ConversationItem::user(m.text)
                            }
                        })
                        .collect::<Vec<_>>();
                    let request = ConversationRequest {
                        items,
                        tools: vec![],
                        hosted_tools: vec![],
                        tool_choice: None,
                        model: Some(model),
                        temperature: None,
                        max_output_tokens: None,
                        json_schema: Some(
                            xai_grok_workspace::permission::classifier_output_json_schema(),
                        ),
                        reasoning_effort: classifier_reasoning_effort,
                        x_grok_conv_id: Some(
                            format!("perm-classifier-{}", uuid::Uuid::new_v4()),
                        ),
                        x_grok_req_id: Some(
                            format!("xai-perm-auto-{}", uuid::Uuid::new_v4()),
                        ),
                        x_grok_session_id: Some(session_id),
                        x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
                        ..ConversationRequest::default()
                    };
                    let fut = sampling_client.conversation_collect(request);
                    let response = tokio::time::timeout(classify_timeout, fut)
                        .await
                        .map_err(|_| {
                            xai_grok_workspace::permission::ClassifierFailure::Timeout
                        })?
                        .map_err(|e| xai_grok_workspace::permission::ClassifierFailure::TransportError(
                            e.to_string(),
                        ))?;
                    Ok(response.assistant_text())
                }
                    .await;
                if let Err(error) = &result {
                    tracing::warn!(%error, "permission auto classifier side-query failed");
                }
                let _ = respond_to.send(result);
            }
        });
        let clf =
            xai_grok_workspace::permission::LlmPermissionClassifier::with_channel(tx, prompt_type);
        debug_assert!(
            clf.has_side_query(),
            "channel-wired classifier must report has_side_query"
        );
        self.permissions.set_classifier_with_side_query(clf, true);
        tracing::info!(
            session_id = %self.session_info.id,
            "Wired live LLM permission auto-mode classifier (session sampling channel)"
        );
    }
    /// Resolve a standalone aux-model `SamplerConfig` for `slug` via the shared
    /// catalog routing (Tier-1 catalog creds / Tier-2 xAI-proxy via session token
    /// / `XAI_API_KEY` / deployment key), gathering the session-local auth context
    /// once. Shared by image-describe and the classifier so the gather can't
    /// drift. `None` ⇒ caller falls back to the session model.
    pub(super) async fn resolve_aux_sampler_config(
        &self,
        slug: &str,
    ) -> Option<xai_grok_sampler::SamplerConfig> {
        let creds = self.chat_state_handle.get_credentials().await;
        let session_key = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired().map(|a| a.key.clone()));
        let models = self.models_manager.models();
        let endpoints = self.models_manager.endpoints();
        let disable_api_key_auth = self
            .auth_manager
            .as_ref()
            .map(|am| am.grok_com_config().api_key_auth_disabled())
            .unwrap_or(false);
        crate::agent::config::resolve_aux_model_sampling_config_preflight(
            slug,
            &models,
            &endpoints,
            session_key.as_deref(),
            disable_api_key_auth,
            creds.alpha_test_key_cloned(),
            creds.client_version_cloned(),
        )
        .await
    }
    /// Resolve a dedicated sampler for the Auto-mode classifier model `slug`,
    /// stamping session-local auth/attribution like image-describe (which relies
    /// on the resolver, not a config override, for `base_url`/`api_backend` so
    /// credentials stay consistent). `None` ⇒ caller falls back to the session
    /// client + model.
    async fn resolve_auto_classifier_sampler(
        &self,
        slug: &str,
    ) -> Option<(xai_grok_sampler::SamplingClient, String)> {
        let active_session_config = self.reconstruct_full_config().await;
        let mut cfg = self.resolve_aux_sampler_config(slug).await?;
        crate::agent::config::stamp_session_local_sampler_fields(
            &mut cfg,
            &active_session_config,
            self.client_identifier.clone(),
            Some(self.max_retries),
        );
        let model = cfg.model.clone();
        let client = xai_grok_sampler::SamplingClient::new(cfg)
            .map_err(|e| {
                tracing::warn!(error = %e, "auto classifier aux sampler build failed; using session model")
            })
            .ok()?;
        Some((client, model))
    }
    #[tracing::instrument(
        name = "session.prepare_chat_completion",
        skip_all,
        fields(force_http1)
    )]
    pub(super) async fn prepare_chat_completion(
        &self,
        force_http1: bool,
    ) -> Result<xai_grok_sampler::SamplingClient, acp::Error> {
        self.refresh_token_if_expired().await;
        let mut full_config = self.reconstruct_full_config().await;
        if let Err(message) = self.unusable_external_route(&full_config) {
            // Deliberately not `fail_turn_unusable_route`. This seam serves
            // compaction, goals, memory-dream and the laziness classifier --
            // background work, not a user turn. A `RetryState::Failed` here
            // would tell the pager a turn failed when none was running and
            // fire the `agent_error` hook for housekeeping. Callers that do
            // owe the client a terminal report send their own, as
            // `compaction.rs` does.
            return Err(acp::Error::invalid_params().data(message));
        }
        full_config.force_http1 = force_http1;
        let sampling_client =
            xai_grok_sampler::SamplingClient::new(full_config).map_err(|e| self.to_acp_error(e))?;
        Ok(sampling_client)
    }
    /// Push a fresh `SamplerConfig` into the per-session sampler actor
    /// before each turn. Mirrors `prepare_chat_completion`'s
    /// auth-refresh + config rebuild, but routes the result to the
    /// `xai-grok-sampler` instead of constructing a new
    /// `OaiCompatClient`.
    ///
    /// Behaviour parity: we run the same `refresh_token_if_expired()`
    /// and `reconstruct_full_config()` so the sampler picks up any
    /// newly issued session token. The previous client cache inside
    /// the sampler actor is invalidated automatically by
    /// `update_config`.
    pub(crate) async fn prepare_sampler_for_turn(&self) -> Result<(), acp::Error> {
        self.refresh_token_if_expired().await;
        let mut sampler_config = self.reconstruct_full_config().await;
        if let Err(message) = self.unusable_external_route(&sampler_config) {
            return Err(self.fail_turn_unusable_route(message).await);
        }
        if self.tool_context.task_output_token_budget.is_some()
            || self.tool_context.sampler_retry_only_before_output
        {
            sampler_config.doom_loop_recovery = None;
        }
        sampler_config.idle_timeout_secs = Some(self.inference_idle_timeout.as_secs());
        self.sampler_handle.update_config(sampler_config);
        Ok(())
    }
    /// #133: a catalogued-but-unusable model pointed at a non-xAI origin must
    /// fail locally before a request is built. Key on `Unusable` alone —
    /// `Unknown` (uncatalogued / unloadable) must still proceed.
    /// Terminal failure for a route refused before any request is built.
    ///
    /// `run_turn_via_sampler` documents that every `Err` it returns has
    /// **already been reported**, and `handle_sampling_failure` is itself the
    /// terminal reporter. Both reach `prepare_sampler_for_turn` through `?`,
    /// so returning unreported from there would break that postcondition at
    /// three call sites.
    ///
    /// The user still sees a turn-failed block either way -- the pager's
    /// PromptResponse arm is a catch-all and none of its suppression flags
    /// fire for this error type. What reporting buys is the rest of the
    /// terminal contract: `log_terminal_failure` emits `turn.terminal_failure`
    /// to the unified log, and `RetryState::Failed` is what raises the
    /// `agent_error` hook. Without it this failure class is the only terminal
    /// one that is invisible to telemetry and to user hooks.
    ///
    /// `model_not_ready` rather than `auth`: `is_reauthable_failure` keys on
    /// `auth`, and an unusable model configuration is not an auth failure --
    /// raising `/login` would send the user to fix the wrong thing.
    async fn fail_turn_unusable_route(&self, message: String) -> acp::Error {
        const ERROR_TYPE: &str = "model_not_ready";
        self.log_terminal_failure(ERROR_TYPE, None, &message);
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Failed {
                error_type: ERROR_TYPE.to_owned(),
                message: message.clone(),
            },
        ))
        .await;
        acp::Error::invalid_params().data(message)
    }
    /// `Err(reason)` when this route must be refused. Returns the message
    /// rather than a built `acp::Error` so the single caller reports it before
    /// propagating -- see [`Self::fail_turn_unusable_route`].
    fn unusable_external_route(&self, config: &SamplingConfig) -> Result<(), String> {
        let catalog_model_id = self.catalog_model_id_str();
        let facts = self.model_auth_facts(&catalog_model_id);
        let Some(reason) = facts.readiness.unusable_reason() else {
            return Ok(());
        };
        let first_party = match config.endpoint_trust {
            // `UserDeclared` (#123) behaves like first-party here: the declared
            // gateway fronts xAI for this user, so an unusable route surfaces
            // the provider's 401 rather than a local refusal — the same
            // behaviour `api.x.ai` gets.
            Some(xai_grok_sampler::EndpointTrustClass::FirstPartyXai)
            | Some(xai_grok_sampler::EndpointTrustClass::UserDeclared) => true,
            Some(_) => false,
            None => crate::util::is_xai_api_bearer_url(&config.base_url),
        };
        if first_party {
            return Ok(());
        }
        tracing::warn!(
            model = %catalog_model_id,
            %reason,
            "refusing unusable model on a non-first-party endpoint"
        );
        Err(format!(
            "model '{catalog_model_id}' is not ready ({reason})"
        ))
    }
    /// Fold an auth remedy into a turn failure: its advice becomes the tail of
    /// the message, and its `turn_error_type` the classification the client
    /// keys its re-auth prompt off.
    fn apply_auth_remedy(
        &self,
        remedy: &crate::auth::AuthRemedy,
        message: String,
        status_code: Option<u16>,
    ) -> (&'static str, String) {
        xai_grok_telemetry::unified_log::info(
            "auth: turn failure classified",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "status_code": status_code,
                "remedy": format!("{remedy:?}"),
            })),
        );
        let message = match remedy.advice() {
            Some(advice) => format!("{message}\n\n{advice}"),
            None => message,
        };
        (remedy.turn_error_type(), message)
    }
    /// Terminal failure for a turn the auth-retry budget gave up on — the one
    /// terminal path that lives outside [`Self::handle_sampling_failure`].
    ///
    /// Every terminal path owes the client one `RetryState::Failed`: it is
    /// what raises the pager's re-auth prompt and its turn-failed block. This
    /// arm used to return its `acp::Error` without one, so a turn that died on
    /// repeated 401s ended in silence.
    pub(crate) async fn fail_turn_auth_budget_exhausted(&self, message: String) -> acp::Error {
        const STATUS: Option<u16> = Some(401);
        let (error_type, message) = match self.auth_manager.as_ref() {
            Some(auth_manager) => self.apply_auth_remedy(
                &auth_manager.auth_remedy().after_retries_exhausted(),
                message,
                STATUS,
            ),
            None => ("auth", message),
        };
        self.log_terminal_failure(error_type, STATUS, &message);
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Failed {
                error_type: error_type.to_owned(),
                message: message.clone(),
            },
        ))
        .await;
        acp::Error::internal_error().data(crate::sampling::error::error_data_with_status(
            message, STATUS,
        ))
    }
    fn log_terminal_failure(&self, error_type: &str, status_code: Option<u16>, message: &str) {
        let auth = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired());
        let reauthable = is_reauthable_failure(Some(error_type), message);
        xai_grok_telemetry::unified_log::warn(
            "turn.terminal_failure",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "error_type": error_type,
                "status_code": status_code,
                "reauthable": reauthable,
                "auth_mode": auth.as_ref().map(|a| format!("{:?}", a.auth_mode)),
                "access_token_present": auth.as_ref().is_some_and(|a| !a.key.is_empty()),
                "refresh_token_present": auth.as_ref().is_some_and(|a| a.refresh_token.is_some()),
                "expires_at": auth
                    .as_ref()
                    .and_then(|a| a.expires_at.map(|e| e.to_rfc3339())),
                "message": crate::util::truncate(message, 300),
            })),
        );
    }
    pub(crate) async fn handle_sampling_failure(
        self: &Arc<Self>,
        error: xai_grok_sampler::SamplingErrorInfo,
    ) -> Result<SamplerFailureRecovery, acp::Error> {
        self.handle_sampling_failure_with_codex_retry_policy(error, true)
            .await
    }

    pub(super) async fn handle_sampling_failure_with_codex_retry_policy(
        self: &Arc<Self>,
        error: xai_grok_sampler::SamplingErrorInfo,
        codex_retry_available: bool,
    ) -> Result<SamplerFailureRecovery, acp::Error> {
        use xai_grok_sampler::SamplingErrorKind;
        // Sampler messages for Api are `API error (status N): <user_facing>`.
        // For HTTP 400, user_facing may carry a truncated secret-scrubbed body
        // preview (#245). Surface that preview here; never invent new text
        // from raw provider bytes at this layer.
        let safe_provider_failure = || match error.status_code {
            Some(status) => {
                let mut out = format!("Provider request failed (HTTP {status}).");
                if status == 400 {
                    const PREFIXES: &[&str] = &[
                        "API error (status 400): Request failed (HTTP 400). ",
                        "API error (status 400): Request failed (HTTP 400).",
                        "Request failed (HTTP 400). ",
                        "Request failed (HTTP 400).",
                    ];
                    for prefix in PREFIXES {
                        if let Some(rest) = error.message.strip_prefix(prefix) {
                            let rest = rest.trim();
                            if !rest.is_empty() {
                                out.push(' ');
                                out.push_str(rest);
                            }
                            break;
                        }
                    }
                }
                out
            }
            None => format!("Provider request failed ({}).", error.kind.as_str()),
        };
        if self.tool_context.task_output_token_budget.is_some() {
            self.tool_context.fail_task_output_usage_closed();
            let message = format!(
                "Budgeted workflow child model request failed; output grant exhausted. {}",
                safe_provider_failure()
            );
            self.log_terminal_failure("output_budget_usage_unknown", error.status_code, &message);
            return Err(acp::Error::internal_error().data(message));
        }
        if self.tool_context.sampler_retry_only_before_output {
            let handle = self.chat_state_handle.clone();
            tokio::spawn(async move {
                let _ = handle.mark_usage_incomplete(true, true).await;
            });
            let message = format!(
                "Workflow child model request failed; usage may understate real spend. {}",
                safe_provider_failure()
            );
            self.log_terminal_failure(
                "workflow_child_sampling_failed",
                error.status_code,
                &message,
            );
            return Err(acp::Error::internal_error().data(message));
        }
        if self.should_compact_on_error(&error).await {
            let cw = error
                .model_metadata
                .as_ref()
                .and_then(|m| m.context_window)
                .expect("should_compact_on_error guarantees context_window");
            {
                let total_tokens = self.chat_state_handle.get_estimated_total_tokens().await;
                let percentage = xai_token_estimation::usage_percentage_u8(total_tokens, cw);
                if let Some(mut cfg) = self.chat_state_handle.get_sampling_config().await
                    && let Some(new_cw) = std::num::NonZeroU64::new(cw)
                    && self.compaction.context_window_override.is_none()
                {
                    cfg.context_window = new_cw;
                    self.chat_state_handle.update_sampling_config(cfg);
                }
                let trigger_info = compaction::AutoCompactTriggerInfo {
                    tokens_used: total_tokens,
                    context_window: cw,
                    percentage,
                };
                if let Err(e) = self.run_compact_only(trigger_info).await {
                    if Self::is_auth_compact_error(&e) {
                        return Err(self.surface_compact_auth_failure(e).await);
                    }
                    return Err(e);
                }
                return Ok(SamplerFailureRecovery::CompactAndResubmit);
            }
        }
        let is_model_404 =
            error.status_code == Some(404) && error.message.contains("does not exist");
        let detailed_message = safe_provider_failure();
        if matches!(error.kind, SamplingErrorKind::Api)
            && error.status_code == Some(400)
            && error.message.contains("encrypted_content")
        {
            self.signals_handle()
                .record_error_typed("encrypted_content_mismatch");
            let friendly = "This session's conversation history is incompatible \
                            with the current model. Please start a new session."
                .to_string();
            self.log_terminal_failure("encrypted_content_mismatch", error.status_code, &friendly);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Failed {
                    error_type: "encrypted_content_mismatch".to_string(),
                    message: friendly.clone(),
                },
            ))
            .await;
            return Err(acp::Error::invalid_params().data(friendly));
        }
        if matches!(error.kind, SamplingErrorKind::RateLimited) {
            self.log_terminal_failure("rate_limited", error.status_code, &detailed_message);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Exhausted {
                    attempts: 0,
                    reason: detailed_message.clone(),
                    is_rate_limited: true,
                },
            ))
            .await;
            let acp_err = acp::Error::new(
                crate::sampling::error::RATE_LIMITED_ERROR_CODE,
                "Rate limited".to_string(),
            )
            .data(detailed_message);
            return Err(acp_err);
        }
        let (failed_model_id, failed_base_url, failed_endpoint_trust, failed_headers) = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| {
                (
                    self.catalog_model_id_str(),
                    c.base_url,
                    c.endpoint_trust,
                    (c.extra_headers, c.env_http_headers),
                )
            })
            .unwrap_or_default();
        let auth_provider =
            if matches!(error.kind, SamplingErrorKind::Auth) || error.status_code == Some(401) {
                self.model_auth_provider(&failed_model_id)
            } else {
                None
            };
        let auth_recovery_eligible = matches!(error.kind, SamplingErrorKind::Auth) && {
            let gate = self.auth_gate(
                &failed_model_id,
                &failed_base_url,
                failed_endpoint_trust,
                &failed_headers.0,
                &failed_headers.1,
            );
            let eligible = gate.active();
            self.log_auth_gate_unknown("handle_sampling_failure", gate, &failed_base_url);
            if !eligible && auth_provider.is_none() {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    is_session_based = gate.is_session_based,
                    model_byok = gate.model_byok.as_str(),
                    endpoint_is_first_party = gate.endpoint_is_first_party,
                    "auth recovery: sampler 401 not refreshable (api-key auth) — surfacing 401",
                );
                xai_grok_telemetry::unified_log::warn(
                    "auth recovery: sampler 401 not eligible (api-key auth)",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "kind": error.kind.as_str(),
                        "status_code": error.status_code,
                        "is_session_based": gate.is_session_based,
                        "model_byok": gate.model_byok.as_str(),
                        "endpoint_is_first_party": gate.endpoint_is_first_party,
                    })),
                );
            }
            eligible
        };
        debug_assert!(
            !(auth_recovery_eligible && auth_provider.is_some()),
            "a provider-backed model must not be session-recovery-eligible"
        );
        if !matches!(error.kind, SamplingErrorKind::Auth)
            && error.status_code == Some(401)
            && auth_provider.is_none()
        {
            xai_grok_telemetry::unified_log::warn(
                "auth recovery: sampler 401 not eligible (non-auth error kind)",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "kind": error.kind.as_str(),
                    "status_code": error.status_code,
                })),
            );
        }
        if auth_recovery_eligible && let Some(ref am) = self.auth_manager {
            if am
                .try_recover_unauthorized(crate::auth::recovery::RecoverySource::Turn)
                .await
            {
                tracing::info!(session_id = %self.session_info.id.0, "auth recovery: sampler 401, recovered, retrying");
                xai_grok_telemetry::unified_log::info(
                    "auth recovery: sampler 401, recovered, retrying",
                    Some(self.session_info.id.0.as_ref()),
                    None,
                );
                self.prepare_sampler_for_turn().await?;
                return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                    credential: error.credential,
                    store: RecoveredStore::SessionToken,
                });
            }
            tracing::warn!(session_id = %self.session_info.id.0, "auth recovery: sampler 401, refresh failed");
            xai_grok_telemetry::unified_log::warn(
                "auth recovery: sampler 401, refresh failed",
                Some(self.session_info.id.0.as_ref()),
                None,
            );
        }
        let is_codex_provider = auth_provider.as_ref().is_some_and(|provider| {
            provider.name == crate::agent::model_providers::OPENAI_CODEX_PROVIDER_ID
        });
        let codex_retry_exhausted = is_codex_provider && !codex_retry_available;
        if let Some(ref provider) = auth_provider {
            if codex_retry_exhausted {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    provider = %provider.name,
                    "auth recovery: Codex 401 retry already consumed; surfacing failure"
                );
            } else if self
                .try_provider_401_recovery(provider, error.credential)
                .await
            {
                self.prepare_sampler_for_turn().await?;
                return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                    credential: error.credential,
                    store: RecoveredStore::AuthProvider,
                });
            }
        }
        if matches!(error.kind, SamplingErrorKind::IdleTimeout) {
            self.signals_handle().record_idle_timeout();
        }
        if matches!(error.kind, SamplingErrorKind::EmptyResponse) {
            if let Some(ref ctx) = error.empty_response_context {
                tracing::warn!(
                    empty_response = true,
                    empty_reason = ctx.reason.as_str(),
                    had_reasoning = ctx.had_reasoning,
                    content_len = ctx.content_len,
                    tool_call_count = ctx.tool_call_count,
                    completion_tokens = ctx.completion_tokens.unwrap_or(0),
                    reasoning_tokens = ctx.reasoning_tokens.unwrap_or(0),
                    finish_reason = ctx.finish_reason_str(),
                    first_choice_seen = ctx.first_choice_seen,
                    model = %ctx.model,
                    "empty response after retries exhausted: {reason}",
                    reason = ctx.reason,
                );
                {
                    let mut cap = self.streaming_turn_capture.lock();
                    cap.reasoning_tokens = ctx.reasoning_tokens;
                    cap.completion_tokens = ctx.completion_tokens;
                    cap.finish_reason = ctx.finish_reason.clone();
                    cap.empty_reason = Some(ctx.reason.as_str().to_owned());
                }
            }
            self.signals_handle().record_error_typed("empty_response");
        }
        let auth_mode = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired())
            .map(|a| a.auth_mode)
            .unwrap_or(crate::auth::AuthMode::ApiKey);
        let auth_mode_str = format!("{auth_mode:?}");
        let client_version = xai_grok_version::VERSION;
        if auth_mode == crate::auth::AuthMode::WebLogin {
            let fix_instruction = crate::auth::with_login_instruction(
                |prog| {
                    format!(
                        "run `{prog} logout` then `{prog} login` to re-authenticate with OAuth2."
                    )
                },
                "log out then sign in again to re-authenticate with OAuth2.",
            );
            let msg = format!(
                "{detailed_message}\n\n\
                 You are using a deprecated authentication method (WebLogin).\n\
                 This auth method is no longer supported and will cause errors.\n\n\
                 To fix: {fix_instruction}\n\n\
                 Version: {client_version}"
            );
            self.log_terminal_failure("legacy_auth", error.status_code, &msg);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Failed {
                    error_type: "legacy_auth".to_string(),
                    message: msg.clone(),
                },
            ))
            .await;
            return Err(acp::Error::internal_error().data(msg));
        }
        let is_auth_401 =
            error.status_code == Some(401) || matches!(error.kind, SamplingErrorKind::Auth);
        let detailed_message = if is_model_404 || is_auth_401 {
            let current_model = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_else(|| "unknown".to_string());
            let available: Vec<String> = self
                .models_manager
                .models()
                .values()
                .map(|m| m.model.clone())
                .collect();
            let mut msg = format!("{detailed_message}\n");
            msg.push_str(&format!("\n  Model:     {current_model}"));
            msg.push_str(&format!("\n  Auth:      {auth_mode_str}"));
            if let Some(ref provider) = auth_provider {
                msg.push_str(
                    &format!(
                    "\n  Provider:  [auth_provider.{}] (check the provider command and the debug log)",
                    provider.name
                ),
                );
            }
            msg.push_str(&format!("\n  Version:   {client_version}"));
            if available.is_empty() {
                msg.push_str("\n  Available: (none)");
            } else {
                msg.push_str(&format!("\n  Available: {}", available.join(", ")));
            }
            if is_model_404 && !available.iter().any(|m| m == &current_model) {
                msg.push_str(&format!(
                    "\n\n  '{}' is not in your available models.",
                    current_model
                ));
                msg.push_str("\n  Switch models with /model or start a new session.");
            }
            msg
        } else {
            detailed_message
        };
        let error_type = if xai_grok_sampling_types::is_context_length_error(&error.message) {
            "context_length"
        } else {
            error.kind.as_str()
        };
        // Codex has its own login verb, and the generic remedy advice does not
        // know it: `provider_login_message` says "Run /login", which cannot
        // renew a Codex credential. Keep these two arms ahead of upstream's
        // classification; everything else falls through to it.
        //
        // The hand-rolled `auth_transient` arm that used to sit here is gone:
        // `AuthRemedy::SelfHealing` carries the same error type and the same
        // message verbatim, on a stricter condition (it also requires a live
        // credential when an external provider mints sessions).
        let (error_type, detailed_message) = if codex_retry_exhausted {
            let sign_in_hint = crate::auth::with_login_instruction(
                |prog| format!("Sign in again with `{prog} login --provider openai-codex`."),
                "Sign in again with the OpenAI Codex login flow.",
            );
            (
                "auth",
                format!(
                    "{detailed_message}\n\nOpenAI Codex rejected the refreshed credential. \
                     {sign_in_hint}"
                ),
            )
        } else if is_codex_provider && error_type == "auth" {
            let sign_in_hint = crate::auth::with_login_instruction(
                |prog| format!("sign in again with `{prog} login --provider openai-codex`."),
                "sign in again with the OpenAI Codex login flow.",
            );
            (
                "auth",
                format!(
                    "{detailed_message}\n\nOpenAI Codex authentication could not recover. \
                     Retry once after a network interruption; if it persists, {sign_in_hint}"
                ),
            )
        } else {
            match self.auth_manager.as_ref() {
                Some(auth_manager) if error_type == "auth" => self.apply_auth_remedy(
                    &auth_manager.auth_remedy(),
                    detailed_message,
                    error.status_code,
                ),
                _ => (error_type, detailed_message),
            }
        };
        self.log_terminal_failure(error_type, error.status_code, &detailed_message);
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Failed {
                error_type: error_type.to_string(),
                message: detailed_message.clone(),
            },
        ))
        .await;
        Err(
            acp::Error::internal_error().data(crate::sampling::error::terminal_error_data(
                detailed_message,
                error.status_code,
                error.kind,
            )),
        )
    }
    /// Drive a single turn through the sampler-based path.
    ///
    /// Calls `prepare_sampler_for_turn` first (auth refresh + config
    /// push), then submits via `SamplerHandle::submit_and_collect` and
    /// returns:
    /// * `Ok(SamplerTurnOutcome::Response(_))` - model responded.
    /// * `Ok(SamplerTurnOutcome::CompactAndResubmit)` - compaction
    ///    ran, the outer turn loop should `continue`.
    /// * `Ok(SamplerTurnOutcome::RefreshAuthAndResubmit)` - auth 401
    ///    recovery succeeded, credentials refreshed, retry once.
    /// * `Err(acp::Error)` - terminal failure already reported via
    ///    `send_xai_notification(RetryState::Failed)`.
    pub(crate) async fn run_turn_via_sampler(
        self: &Arc<Self>,
        request: ConversationRequest,
        codex_retry_available: bool,
    ) -> Result<SamplerTurnOutcome, acp::Error> {
        self.prepare_sampler_for_turn().await?;
        let stream_drained_rx = {
            let (tx, rx) = tokio::sync::oneshot::channel();
            *self.turn_stream_drained.lock() = Some(tx);
            rx
        };
        let request_id = xai_grok_sampler::RequestId::random();
        let request_id_str = request_id.as_str().to_string();
        match self
            .sampler_handle
            .submit_and_collect(request_id, request)
            .await
        {
            Ok((response, metrics)) => {
                let span = tracing::Span::current();
                span.record("request_id", request_id_str.as_str());
                if let Some(ttft) = metrics.time_to_first_token_ms {
                    span.record("ttft_ms", ttft as i64);
                }
                if metrics.attempts > 0 {
                    span.record("attempt", i64::from(metrics.attempts));
                }
                if tokio::time::timeout(std::time::Duration::from_secs(5), stream_drained_rx)
                    .await
                    .is_err()
                {
                    self.turn_stream_drained.lock().take();
                    tracing::warn!(
                        "stream-drain barrier timed out; proceeding to emit tool \
                         calls (eventId ordering may be imperfect this turn)"
                    );
                }
                Ok(SamplerTurnOutcome::Response(
                    Box::new(response),
                    Box::new(metrics),
                ))
            }
            Err(rich_err) => {
                self.turn_stream_drained.lock().take();
                let info = xai_grok_sampler::SamplingErrorInfo::from(&rich_err);
                match self
                    .handle_sampling_failure_with_codex_retry_policy(info, codex_retry_available)
                    .await?
                {
                    SamplerFailureRecovery::CompactAndResubmit => {
                        Ok(SamplerTurnOutcome::CompactAndResubmit)
                    }
                    SamplerFailureRecovery::RefreshAuthAndResubmit { credential, store } => {
                        Ok(SamplerTurnOutcome::RefreshAuthAndResubmit { credential, store })
                    }
                }
            }
        }
    }
    /// Proactively refresh the auth token if near expiry.
    ///
    /// Session-token path is best-effort: on success, update credentials and
    /// return. On failure, do **not** fall through to the JWT/config.toml
    /// branch when the session gate was active — that path is for BYOK JWTs
    /// only. Falling through after a failed session refresh left hard-expired
    /// opaque tokens (External/OIDC) on the wire and guaranteed a 401.
    /// Soft failures with a still-usable access token still return here
    /// (grace / optimistic send); 401 recovery remains the safety net.
    pub(crate) async fn refresh_token_if_expired(&self) {
        let current_model_id = self.catalog_model_id_str();
        if let Some(provider) = self.model_auth_provider(&current_model_id)
            && provider.name == crate::agent::model_providers::OPENAI_CODEX_PROVIDER_ID
        {
            let current_key = self
                .chat_state_handle
                .get_credentials()
                .await
                .api_key_cloned();
            self.refresh_provider_token_pre_turn(
                &provider,
                current_key.as_deref(),
                &current_model_id,
            )
            .await;
            return;
        }
        if let Some(ref am) = self.auth_manager {
            let creds = self.chat_state_handle.get_credentials().await;
            let catalog_model_id = current_model_id.clone();
            let (base_url, endpoint_trust, headers) = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| {
                    (
                        c.base_url,
                        c.endpoint_trust,
                        (c.extra_headers, c.env_http_headers),
                    )
                })
                .unwrap_or_default();
            if self
                .auth_gate(
                    &catalog_model_id,
                    &base_url,
                    endpoint_trust,
                    &headers.0,
                    &headers.1,
                )
                .active()
            {
                match am.get_valid_token().await {
                    Ok(key) => {
                        if creds.api_key() != Some(key.as_str()) {
                            // Ambient xAI session JWT — rebind, do not
                            // `replace_api_key`. Chat-state may still carry
                            // `Missing` from a pre-login spawn; preserving that
                            // label would disarm L3 (#136 / #151).
                            let creds = creds.rebind(
                                Some(key),
                                xai_chat_state::AuthType::SessionToken,
                                xai_grok_sampler::CredentialSource::XaiSession,
                            );
                            self.chat_state_handle.update_credentials(creds);
                        }
                        self.clear_auth_compact_suppression();
                        return;
                    }
                    Err(e) => {
                        let hard_expired = !am.has_usable_token();
                        if hard_expired && creds.api_key().is_some() {
                            let mut cleared = creds;
                            cleared.clear_api_key();
                            self.chat_state_handle.update_credentials(cleared);
                        }
                        tracing::warn!(
                            error = %e,
                            hard_expired,
                            model = %catalog_model_id,
                            "auth: preflight get_valid_token failed"
                        );
                        xai_grok_telemetry::unified_log::warn(
                            "auth.preflight.refresh_failed",
                            Some(self.session_info.id.0.as_ref()),
                            Some(serde_json::json!({
                                "error": format!("{e}"),
                                "hard_expired": hard_expired,
                                "model": catalog_model_id,
                            })),
                        );
                        return;
                    }
                }
            }
        } else {
            xai_grok_telemetry::unified_log::debug(
                "token refresh skipped: no auth manager",
                Some(self.session_info.id.0.as_ref()),
                None,
            );
        }
        use crate::auth::{is_jwt_expired_or_near, parse_jwt_expiration};
        const REFRESH_THRESHOLD: chrono::Duration = chrono::Duration::minutes(5);
        let creds = self.chat_state_handle.get_credentials().await;
        let current_key = creds.api_key_cloned();
        if let Some(provider) = self.model_auth_provider(&current_model_id) {
            self.refresh_provider_token_pre_turn(
                &provider,
                current_key.as_deref(),
                &current_model_id,
            )
            .await;
            return;
        }
        let Some(ref key) = current_key else { return };
        if !is_jwt_expired_or_near(key, REFRESH_THRESHOLD) {
            if let Some(exp) = parse_jwt_expiration(key) {
                let remaining_secs = (exp - chrono::Utc::now()).num_seconds();
                tracing::debug!(
                    model = %current_model_id,
                    remaining_secs,
                    "JWT token valid, no refresh needed"
                );
            } else {
                tracing::debug!(
                    model = %current_model_id,
                    key_len = key.len(),
                    "Token is not a JWT, expiry-based refresh not applicable"
                );
            }
            return;
        }
        let remaining_secs =
            parse_jwt_expiration(key).map_or(0, |exp| (exp - chrono::Utc::now()).num_seconds());
        tracing::info!(
            model = %current_model_id,
            remaining_secs,
            "JWT near expiry, refreshing from config.toml"
        );
        let Some((new_key, source)) = self.reload_api_key_from_config(&current_model_id) else {
            return;
        };
        if key == &new_key {
            tracing::warn!(
                model = %current_model_id,
                "Config.toml returned same token (not yet rotated by external process?)"
            );
            return;
        }
        let new_remaining_secs = parse_jwt_expiration(&new_key)
            .map_or(0, |exp| (exp - chrono::Utc::now()).num_seconds());
        tracing::info!(
            model = %current_model_id,
            new_remaining_secs,
            key_len = new_key.len(),
            "Refreshed API token from config.toml"
        );
        // Honesty, not the ambient-bytes bug fixed above. `reload_api_key_from_config`
        // reads only `[model.*]`'s own `api_key`/`env_key`, so this site can never
        // write ambient session bytes. Under `replace_api_key` its worst case is
        // labelling those non-ambient bytes with whatever provenance the session
        // already held (e.g. `XaiSession` after a first-party model later gains
        // its own key) — over-restricting, the safe direction, and today without
        // behavioural consequence. Rebind so the stored source matches which
        // own-credential arm actually resolved.
        let creds = self.chat_state_handle.get_credentials().await;
        let creds = creds.rebind(Some(new_key), xai_chat_state::AuthType::ApiKey, source);
        self.chat_state_handle.update_credentials(creds);
    }
    /// Resolve the model's own credential from config.toml, plus the
    /// [`CredentialSource`] for whichever `first_own_credential` arm won:
    /// non-empty `api_key` → `ModelApiKey`; else the winning `env_key`
    /// variable → `EnvKey { name }`.
    fn reload_api_key_from_config(
        &self,
        current_model_id: &str,
    ) -> Option<(String, xai_grok_sampler::CredentialSource)> {
        let raw_config = crate::config::load_effective_config()
            .map_err(|e| tracing::warn!(error = %e, "Failed to reload config"))
            .ok()?;
        let config = crate::agent::config::Config::new_from_toml_cfg(&raw_config)
            .map_err(|e| tracing::warn!(error = %e, "Failed to parse reloaded config.toml"))
            .ok()?;
        let config_model = config
            .config_models
            .iter()
            .find(|(k, v)| v.model.as_deref().unwrap_or(k.as_str()) == current_model_id)
            .map(|(_, v)| v);
        let Some(model) = config_model else {
            tracing::warn!(
                model = %current_model_id,
                available = ?config.config_models.keys().collect::<Vec<_>>(),
                "Model not found in config.toml [model.*]"
            );
            return None;
        };
        // Same two arms as `first_own_credential`, with the source that arm implies.
        if let Some(key) = model.api_key.as_deref().filter(|k| !k.trim().is_empty()) {
            return Some((
                key.to_owned(),
                xai_grok_sampler::CredentialSource::ModelApiKey,
            ));
        }
        if let Some(env_keys) = model.env_key.as_ref() {
            for name in env_keys.names() {
                if let Ok(value) = std::env::var(name)
                    && !value.trim().is_empty()
                {
                    return Some((
                        value,
                        xai_grok_sampler::CredentialSource::EnvKey {
                            name: name.to_owned(),
                        },
                    ));
                }
            }
        }
        tracing::warn!(
            model = %current_model_id,
            env_key = ?model.env_key,
            "No api_key or env_key resolved for model"
        );
        None
    }
    /// Propagate the model-reported token usage from a turn response into
    /// chat state, the per-prompt usage ledger, and per-turn signals.
    ///
    /// This is the only place per-turn `total_tokens` is refreshed in the
    /// post-sampler-refactor path; without it `state.total_tokens` would
    /// stay frozen at the `estimate_conversation_tokens` seed from
    /// `ChatState::new`, freezing `/context` and corrupting the resume
    /// restore that reads `meta.totalTokens` from `updates.jsonl`.
    /// Resetting `estimated_tokens_since_model = 0` here also keeps the
    /// preflight-overflow guard accurate against the next turn's
    /// tool-result deltas.
    pub(crate) fn record_response_token_usage(
        &self,
        response: &ConversationResponse,
        api_duration_ms: Option<u64>,
    ) {
        if let Some(ref u) = response.usage {
            self.tool_context
                .record_task_model_output(u64::from(u.completion_tokens));
            self.chat_state_handle
                .record_token_usage(u64::from(u.total_tokens));
            self.chat_state_handle.record_last_turn_usage(u.clone());
            self.chat_state_handle.record_model_call_usage(
                response.assistant().and_then(|a| a.model_id.clone()),
                u.clone(),
                api_duration_ms,
                response.cost_usd_ticks,
            );
            self.signals_handle()
                .record_token_usage(u.completion_tokens, u.reasoning_tokens);
        } else if self.tool_context.task_output_token_budget.is_some() {
            self.tool_context.fail_task_output_usage_closed();
            let handle = self.chat_state_handle.clone();
            tokio::spawn(async move {
                let _ = handle.mark_usage_incomplete(true, true).await;
            });
        } else if self.tool_context.sampler_retry_only_before_output {
            let handle = self.chat_state_handle.clone();
            tokio::spawn(async move {
                let _ = handle.mark_usage_incomplete(true, true).await;
            });
        }
    }
    pub(super) async fn record_assistant_response(&self, assistant_item: ConversationItem) {
        self.signals_handle().record_assistant_message();
        if let ConversationItem::Assistant(ref a) = assistant_item {
            tracing::info!(model_id = ?a.model_id, "DEBUG record_assistant_response model_id");
        }
        if let ConversationItem::Assistant(ref a) = assistant_item
            && let Some(first_call) = a.tool_calls.first()
        {
            tracing::info!("Assistant requested tool call: {}", first_call.id);
        }
        self.chat_state_handle
            .push_assistant_response(assistant_item);
    }
}
/// Per-tool precedence: a non-empty `over` wins, else the non-empty `seed`.
fn prefer_non_empty<T>(
    over: Option<T>,
    seed: Option<T>,
    is_empty: impl Fn(&T) -> bool,
) -> Option<T> {
    over.filter(|o| !is_empty(o))
        .or_else(|| seed.filter(|s| !is_empty(s)))
}
/// The cutoff a subagent inherits: a non-empty per-turn `base` wins per tool, else the `seed`.
fn resolve_configured_cutoff(
    seed: Option<xai_grok_sampling_types::ToolOverrides>,
    base: Option<&xai_grok_sampling_types::ToolOverrides>,
) -> xai_grok_sampling_types::ToolOverrides {
    use xai_grok_sampling_types::{ToolOverrides, WebSearchOptions, XSearchOptions};
    let ToolOverrides {
        x_search: seed_x,
        web_search: seed_w,
    } = seed.unwrap_or_default();
    let (over_x, over_w) =
        base.map_or((None, None), |b| (b.x_search.clone(), b.web_search.clone()));
    ToolOverrides {
        x_search: prefer_non_empty(over_x, seed_x, XSearchOptions::is_empty),
        web_search: prefer_non_empty(over_w, seed_w, WebSearchOptions::is_empty),
    }
}
#[cfg(test)]
mod configured_cutoff_tests {
    use xai_grok_sampling_types::{
        SearchDateBound, ToolOverrides, WebSearchOptions, XSearchOptions,
    };
    fn x_cut(to: &str) -> XSearchOptions {
        XSearchOptions {
            date_bound: Some(SearchDateBound::new(None, Some(to.into())).unwrap()),
        }
    }
    #[test]
    fn seed_only_is_inherited_without_a_per_turn_update() {
        let seed = ToolOverrides {
            x_search: Some(x_cut("2020-01-01")),
            web_search: None,
        };
        assert_eq!(
            super::resolve_configured_cutoff(Some(seed.clone()), None),
            seed
        );
    }
    #[test]
    fn non_empty_base_wins_per_tool_and_empty_reverts_to_seed() {
        let seed = ToolOverrides {
            x_search: Some(x_cut("2020-01-01")),
            web_search: Some(WebSearchOptions {
                allowed_domains: Some(vec!["x.com".into()]),
            }),
        };
        let base = ToolOverrides {
            x_search: Some(x_cut("2019-06-01")),
            web_search: Some(WebSearchOptions {
                allowed_domains: Some(vec![]),
            }),
        };
        let got = super::resolve_configured_cutoff(Some(seed.clone()), Some(&base));
        assert_eq!(got.x_search, Some(x_cut("2019-06-01")));
        assert_eq!(got.web_search, seed.web_search);
    }
    /// The contamination invariant: `resolve_configured_cutoff` (inheritance) must resolve the same
    /// bound the wire/echo path (`apply_tool_overrides`) does for the same seed and per-turn base.
    /// Two independent precedence implementations, so drift on the inherited boundary fails CI.
    #[test]
    fn inherited_cutoff_agrees_with_the_wire_echo() {
        use xai_grok_sampling_types::{HostedTool, apply_tool_overrides};
        let web = WebSearchOptions {
            allowed_domains: Some(vec!["x.com".into()]),
        };
        let cases = [
            (
                Some(ToolOverrides {
                    x_search: Some(x_cut("2020-01-01")),
                    web_search: None,
                }),
                None,
            ),
            (
                Some(ToolOverrides {
                    x_search: Some(x_cut("2020-01-01")),
                    web_search: Some(web.clone()),
                }),
                Some(ToolOverrides {
                    x_search: Some(x_cut("2019-06-01")),
                    web_search: None,
                }),
            ),
            (
                None,
                Some(ToolOverrides {
                    x_search: Some(x_cut("2018-01-01")),
                    web_search: Some(web.clone()),
                }),
            ),
        ];
        for (seed, base) in cases {
            let mut tools = vec![
                HostedTool::WebSearch { options: None },
                HostedTool::XSearch { options: None },
            ];
            apply_tool_overrides(&mut tools, seed.as_ref());
            let wire_echo = apply_tool_overrides(&mut tools, base.as_ref());
            let inherited = super::resolve_configured_cutoff(seed.clone(), base.as_ref());
            assert_eq!(wire_echo, inherited, "seed={seed:?} base={base:?}");
        }
    }
}
