//! Model fetching, resolution, and management.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::RwLock;

use agent_client_protocol as acp;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use indexmap::IndexMap;

use crate::agent::config::{self, ModelEntry, resolve_credentials, sampling_config_for_model};
use crate::auth::{AuthManager, GrokAuth, GrokComConfig};
use crate::remote::{FetchModelsResult, fetch_models_blocking};
use crate::sampling::SamplerConfig as SamplingConfig;
use globset::{Glob, GlobSet, GlobSetBuilder};
use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

const IMPLICIT_SELECTION_RETRY_YIELD_AFTER: usize = 8;

// ── Auth method for model fetching ──────────────────────────────────────────

/// Credential for `/v1/models` fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFetchAuth {
    Session,
    ApiKey,
    Deployment,
    CustomEndpoint,
}

impl ModelFetchAuth {
    /// custom_endpoint > session > deployment > API key.
    pub(crate) fn resolve(endpoints: &config::EndpointsConfig, has_cached_session: bool) -> Self {
        if endpoints.has_custom_endpoint() {
            Self::CustomEndpoint
        } else if has_cached_session {
            Self::Session
        } else if endpoints.deployment_key.is_some() {
            Self::Deployment
        } else if crate::agent::auth_method::has_xai_api_key_env() {
            Self::ApiKey
        } else {
            Self::Session
        }
    }

    fn cache_auth_method(&self) -> CacheAuthMethod {
        match self {
            Self::CustomEndpoint | Self::ApiKey => CacheAuthMethod::ApiKey,
            Self::Session => CacheAuthMethod::Session,
            Self::Deployment => CacheAuthMethod::Deployment,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CacheAuthMethod {
    Session,
    ApiKey,
    Deployment,
}

pub(crate) fn task_model_error_for_catalog(
    requested: &str,
    available: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> Option<String> {
    let is_available = |entry: &ModelEntry| {
        entry.info.user_selectable && entry.info.visible_for_auth(is_session_auth)
    };
    if resolve_catalog_identity(available, &acp::ModelId::new(requested))
        .and_then(|identity| available.get(identity.model_id.as_str()))
        .is_some_and(&is_available)
    {
        return None;
    }

    let ambiguous_route = !available.contains_key(requested)
        && available
            .values()
            .filter(|entry| entry.info().model == requested)
            .take(2)
            .count()
            > 1;

    let mut slugs = available
        .iter()
        .filter(|(_, entry)| is_available(entry))
        .map(|(slug, _)| slug.as_str())
        .collect::<Vec<_>>();
    slugs.sort_unstable();
    let guidance = if slugs.is_empty() {
        "No valid model slugs are currently available. Omit `model` to inherit the parent model."
            .to_string()
    } else {
        format!(
            "Valid model slugs: {}. Omit `model` to inherit the parent model.",
            slugs.join(", ")
        )
    };
    if ambiguous_route {
        Some(format!(
            "Ambiguous Task.model slug '{requested}'. Use an exact catalog key. {guidance}"
        ))
    } else {
        Some(format!("Unknown Task.model slug '{requested}'. {guidance}"))
    }
}

/// Thread-safe model manager.
#[derive(Clone)]
pub struct ModelsManager {
    inner: Arc<Inner>,
}

#[derive(Clone)]
pub(crate) struct ResolvedModelCapabilities {
    pub model_id: acp::ModelId,
    pub byok: crate::agent::auth_method::ModelByok,
    pub agent_type: String,
    pub auth_scheme: xai_grok_sampler::AuthScheme,
    pub supports_reasoning_effort: bool,
    pub reasoning_efforts: Vec<ReasoningEffortOption>,
    pub supports_backend_search: bool,
    pub auto_compact_threshold_percent: Option<u8>,
    pub compactions_remaining: Option<xai_grok_sampling_types::CompactionsRemaining>,
    pub compaction_at_tokens: Option<xai_grok_sampling_types::CompactionAtTokens>,
    pub codex_wire: Option<xai_grok_sampling_types::CodexWireCapabilities>,
}

/// One auth-stable model/catalog authority snapshot for a new-session plan.
///
/// The caller builds its complete synchronous plan from this owned catalog,
/// then seals `auth_generation` before publishing the plan. A failed seal
/// means auth changed and the whole plan must be retried.
pub(crate) struct SessionModelAuthoritySnapshot {
    pub(crate) auth_generation: u64,
    pub(crate) catalog: IndexMap<String, ModelEntry>,
    pub(crate) fallback_model_id: acp::ModelId,
    pub(crate) campaign_eligible: bool,
}

/// Catalog fields written together under one lock, so readers never see a torn mix.
#[derive(Default)]
struct CatalogState {
    prefetched: Option<IndexMap<String, ModelEntry>>,
    models: IndexMap<String, ModelEntry>,
    etag: Option<String>,
    /// Gates whether the apply path reselects the default (first real catalog)
    has_fetched_real_catalog: bool,
    /// `allowed_models` matched nothing; the prompt path blocks instead.
    allowlist_excludes_all: bool,
}

struct Inner {
    catalog: RwLock<CatalogState>,
    /// Linearizes the current model id with whether it was explicitly picked.
    ///
    /// The values keep their existing storage so current-id-only readers stay
    /// cheap, but every current-id publication takes this lock for write and
    /// authority snapshots take it for read. This prevents observing the
    /// explicit-pick bit from one selection with the id from another.
    selection_commit: RwLock<()>,
    current_model_id: RwLock<acp::ModelId>,
    current_reasoning_effort: RwLock<Option<ReasoningEffort>>,
    /// Set when the user's configured default was not seated and a substitute
    /// was used (#131). Refreshed at every `resolve_default_model` call site —
    /// including the early-return of `reselect_current_model_if_missing` — so a
    /// catalog that arrives late corrects a verdict taken against an emptier
    /// one rather than leaving it stale. On the warm-cache path that correction
    /// also reseats the preference when it becomes honourable (unless the user
    /// already picked via `/model`), so clearing the verdict cannot disagree
    /// with `current_model_id`. Republished on `x.ai/models/update` via
    /// [`Self::notify_models_updated`].
    substituted_preference: RwLock<Option<resolution::SubstitutedPreference>>,
    // ── Owned context for self-contained refresh ────────────────
    auth_manager: Arc<AuthManager>,
    cfg: RwLock<config::Config>,
    fetch_auth: RwLock<ModelFetchAuth>,
    gateway: RwLock<Option<xai_acp_lib::AcpAgentGatewaySender>>,
    cache: ModelsCacheManager,
    endpoint: Arc<dyn ModelsEndpoint>,
    /// Guard to prevent overlapping retry loops.
    retry_in_flight: AtomicBool,
    /// Single-flight for the etag-triggered background refresh (`spawn_fetch`).
    refresh_in_flight: AtomicBool,
    /// Model-switch signal: a generation counter bumped when the current model id changes.
    model_switch_watch: tokio::sync::watch::Sender<u64>,
    /// Catalog-content generation: bumped whenever `cat.models` is wholesale
    /// replaced or mutated by a publish path (`apply_catalog`, `apply_config`,
    /// test pokes). Session auth memos key on this so a transient miss that
    /// freezes `NotInCatalog` cannot outlive the refresh that restores the
    /// model (#159 / F1).
    catalog_generation: AtomicU64,
    /// Set once the user explicitly picks a model (`/model`); guards the
    /// first-catalog reselect from clobbering that choice.
    user_selected_model: AtomicBool,
}

/// Clears an in-flight flag on drop so a panicking task can't wedge future refreshes.
struct RetryInFlightGuard(Arc<Inner>);
impl Drop for RetryInFlightGuard {
    fn drop(&mut self) {
        self.0.retry_in_flight.store(false, Ordering::Release);
    }
}
struct RefreshInFlightGuard(Arc<Inner>);
impl Drop for RefreshInFlightGuard {
    fn drop(&mut self) {
        self.0.refresh_in_flight.store(false, Ordering::Release);
    }
}

impl Default for ModelsManager {
    fn default() -> Self {
        let grok_home = crate::util::grok_home::grok_home();
        let auth_manager = Arc::new(AuthManager::new(&grok_home, GrokComConfig::default()));
        Self::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("default"),
            auth_manager,
            config::Config::default(),
        )
    }
}

/// Builder for [`ModelsManager`]; transport and disk cache default to production (tests override them).
pub(crate) struct ModelsManagerBuilder {
    prefetched: Option<IndexMap<String, ModelEntry>>,
    models: IndexMap<String, ModelEntry>,
    current_model_id: acp::ModelId,
    auth_manager: Arc<AuthManager>,
    cfg: config::Config,
    endpoint: Arc<dyn ModelsEndpoint>,
    cache: ModelsCacheManager,
}

impl ModelsManagerBuilder {
    pub(crate) fn new(
        prefetched: Option<IndexMap<String, ModelEntry>>,
        models: IndexMap<String, ModelEntry>,
        current_model_id: acp::ModelId,
        auth_manager: Arc<AuthManager>,
        cfg: config::Config,
    ) -> Self {
        Self {
            prefetched,
            models,
            current_model_id,
            auth_manager,
            cfg,
            endpoint: Arc::new(HttpModelsEndpoint),
            cache: ModelsCacheManager::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn endpoint(mut self, endpoint: Arc<dyn ModelsEndpoint>) -> Self {
        self.endpoint = endpoint;
        self
    }

    #[cfg(test)]
    pub(crate) fn cache(mut self, cache: ModelsCacheManager) -> Self {
        self.cache = cache;
        self
    }

    pub(crate) fn build(self) -> ModelsManager {
        let auth = self.auth_manager.selection_snapshot();
        let fetch_auth = ModelFetchAuth::resolve(&self.cfg.endpoints, auth.has_auth);
        let current_reasoning_effort = self.cfg.models.default_reasoning_effort.filter(|effort| {
            resolve_catalog_key(&self.models, &self.current_model_id)
                .and_then(|model_id| self.models.get(model_id.0.as_ref()))
                .is_some_and(|entry| model_offers_reasoning_effort(&entry.info, *effort))
        });
        ModelsManager {
            inner: Arc::new(Inner {
                catalog: RwLock::new(CatalogState {
                    prefetched: self.prefetched,
                    models: self.models,
                    ..Default::default()
                }),
                selection_commit: RwLock::new(()),
                current_model_id: RwLock::new(self.current_model_id),
                current_reasoning_effort: RwLock::new(current_reasoning_effort),
                substituted_preference: RwLock::new(None),
                auth_manager: self.auth_manager,
                cfg: RwLock::new(self.cfg),
                fetch_auth: RwLock::new(fetch_auth),
                gateway: RwLock::new(None),
                cache: self.cache,
                endpoint: self.endpoint,
                retry_in_flight: AtomicBool::new(false),
                refresh_in_flight: AtomicBool::new(false),
                model_switch_watch: tokio::sync::watch::channel(0u64).0,
                catalog_generation: AtomicU64::new(0),
                user_selected_model: AtomicBool::new(false),
            }),
        }
    }
}

pub(crate) fn capabilities_for_route_in(
    models: &IndexMap<String, ModelEntry>,
    preferred_id: Option<&str>,
    routing_model: &str,
    preferred_id_must_exist: bool,
    alternate_preferred_route: Option<&str>,
) -> Option<ResolvedModelCapabilities> {
    if preferred_id_must_exist && preferred_id.is_some_and(|id| !models.contains_key(id)) {
        return None;
    }
    let preferred = preferred_id
        .filter(|id| models.contains_key(*id))
        .map(acp::ModelId::new)
        .and_then(|key| {
            models
                .get(key.0.as_ref())
                .filter(|entry| {
                    entry.info().model == routing_model
                        || alternate_preferred_route == Some(entry.info().model.as_str())
                })
                .map(|entry| (key.0.to_string(), entry))
        });
    let (model_id, entry) = if preferred_id_must_exist {
        preferred?
    } else if let Some(preferred) = preferred {
        preferred
    } else {
        let mut matches = models
            .iter()
            .filter(|(_, entry)| entry.info().model == routing_model);
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        (first.0.clone(), first.1)
    };
    let info = entry.info();
    Some(ResolvedModelCapabilities {
        model_id: acp::ModelId::new(model_id),
        byok: if entry.has_own_credentials() {
            crate::agent::auth_method::ModelByok::Byok
        } else {
            crate::agent::auth_method::ModelByok::NotByok
        },
        agent_type: info.agent_type.clone(),
        auth_scheme: info.auth_scheme,
        supports_reasoning_effort: info.supports_reasoning_effort,
        reasoning_efforts: info.reasoning_efforts.clone(),
        supports_backend_search: info.supports_backend_search,
        auto_compact_threshold_percent: info.auto_compact_threshold_percent,
        compactions_remaining: info.compactions_remaining,
        compaction_at_tokens: info.compaction_at_tokens,
        codex_wire: info.codex_wire.clone(),
    })
}

impl ModelsManager {
    /// Resolve one routing model and copy all request-shaping facts while
    /// holding a single catalog read lock. An exact key that routes elsewhere
    /// never shadows a unique routing-model match.
    pub(crate) fn capabilities_for_route(
        &self,
        preferred_id: Option<&str>,
        routing_model: &str,
        preferred_id_must_exist: bool,
        alternate_preferred_route: Option<&str>,
    ) -> Option<ResolvedModelCapabilities> {
        let catalog = self.inner.catalog.read();
        capabilities_for_route_in(
            &catalog.models,
            preferred_id,
            routing_model,
            preferred_id_must_exist,
            alternate_preferred_route,
        )
    }
    pub(crate) fn new(
        prefetched: Option<IndexMap<String, ModelEntry>>,
        models: IndexMap<String, ModelEntry>,
        current_model_id: acp::ModelId,
        auth_manager: Arc<AuthManager>,
        cfg: config::Config,
    ) -> Self {
        ModelsManagerBuilder::new(prefetched, models, current_model_id, auth_manager, cfg).build()
    }

    /// Subscribe to model-switch events. Returns a `watch::Receiver`
    pub(crate) fn subscribe_model_switch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.model_switch_watch.subscribe()
    }

    /// Cheap snapshot of the current model-switch generation, for the laziness-check poll loop.
    pub(crate) fn model_switch_generation(&self) -> u64 {
        *self.inner.model_switch_watch.borrow()
    }

    /// Cheap snapshot of the catalog-content generation (see
    /// [`Inner::catalog_generation`]). Auth memos compare against this.
    pub(crate) fn catalog_generation(&self) -> u64 {
        self.inner.catalog_generation.load(Ordering::Acquire)
    }

    fn bump_catalog_generation(&self) {
        self.inner
            .catalog_generation
            .fetch_add(1, Ordering::Release);
    }

    /// Build from a resolved config. Falls back to bundled default if no models available.
    pub(crate) fn from_config(
        cfg: &config::Config,
        prefetched_models: Option<IndexMap<String, ModelEntry>>,
        auth_manager: Arc<AuthManager>,
    ) -> Result<Self, String> {
        Self::from_config_with_remote_fetch(
            cfg,
            prefetched_models,
            auth_manager,
            crate::util::config::resolve_remote_fetch_enabled(),
        )
    }

    fn from_config_with_remote_fetch(
        cfg: &config::Config,
        prefetched_models: Option<IndexMap<String, ModelEntry>>,
        auth_manager: Arc<AuthManager>,
        remote_fetch_enabled: bool,
    ) -> Result<Self, String> {
        #[cfg(test)]
        return Self::from_config_with_remote_fetch_inner(
            cfg,
            prefetched_models,
            auth_manager,
            remote_fetch_enabled,
            || {},
        );
        #[cfg(not(test))]
        Self::from_config_with_remote_fetch_inner(
            cfg,
            prefetched_models,
            auth_manager,
            remote_fetch_enabled,
        )
    }

    fn from_config_with_remote_fetch_inner(
        cfg: &config::Config,
        prefetched_models: Option<IndexMap<String, ModelEntry>>,
        auth_manager: Arc<AuthManager>,
        remote_fetch_enabled: bool,
        #[cfg(test)] mut before_commit: impl FnMut(),
    ) -> Result<Self, String> {
        let mut retries = 0usize;
        loop {
            let auth = auth_manager.selection_snapshot();
            let fetch_auth = ModelFetchAuth::resolve(&cfg.endpoints, auth.has_auth);
            if let Err(error) = crate::remote::validate_models_catalog_auth(
                &cfg.endpoints,
                fetch_auth,
                remote_fetch_enabled,
            ) {
                if auth_manager.selection_generation_is_current(auth.generation) {
                    return Err(error);
                }
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }
            let resolved_prefetched = prefetched_models.clone().or_else(|| {
                let cache = ModelsCacheManager::new();
                cache
                    .load_fresh(
                        &fetch_auth.cache_auth_method(),
                        &crate::remote::models_list_url(&cfg.endpoints, fetch_auth),
                    )
                    .map(|c| c.models)
            });
            let has_prefetched = resolved_prefetched.is_some();
            let catalog = resolve_model_catalog(cfg, resolved_prefetched.clone());

            if has_prefetched && let Err(error) = validate_selectable(cfg, &catalog) {
                if auth_manager.selection_generation_is_current(auth.generation) {
                    return Err(error);
                }
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }

            let usable_xai = Self::usable_ambient_xai_from_snapshot(cfg, auth);
            let (current_model_key, current_model, model_source, unready_default_reason) =
                resolve_default_model_for_catalog_with_usable_xai(
                    cfg,
                    &catalog,
                    auth.is_session_auth,
                    has_prefetched,
                    usable_xai,
                );

            #[cfg(test)]
            before_commit();
            if !auth_manager.selection_generation_is_current(auth.generation) {
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }

            let current_model_id = acp::ModelId::new(Arc::from(current_model_key));
            let mgr = Self::new(
                resolved_prefetched,
                catalog,
                current_model_id,
                auth_manager.clone(),
                cfg.clone(),
            );
            // `ModelsManagerBuilder` serves tests that supply an already chosen
            // model and takes its own auth snapshot. Startup selection must
            // instead keep fetch authority and the chosen default on this one
            // generation.
            *mgr.inner.fetch_auth.write() = fetch_auth;
            if !auth_manager.selection_generation_is_current(auth.generation) {
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }

            mgr.record_substituted_preference(cfg, model_source);
            if has_prefetched {
                mgr.inner.catalog.write().has_fetched_real_catalog = true;
            }
            if let Some(reason) = &unready_default_reason {
                tracing::error!(
                    model_id = %current_model.model,
                    source = %model_source,
                    %reason,
                    "default model resolved to an unusable catalog entry"
                );
            } else {
                tracing::info!(
                    model_id = %current_model.model,
                    source = %model_source,
                    "default model resolved"
                );
            }
            return Ok(mgr);
        }
    }

    #[cfg(test)]
    fn from_config_with_remote_fetch_and_before_commit(
        cfg: &config::Config,
        prefetched_models: Option<IndexMap<String, ModelEntry>>,
        auth_manager: Arc<AuthManager>,
        remote_fetch_enabled: bool,
        before_commit: impl FnOnce(),
    ) -> Result<Self, String> {
        let mut before_commit = Some(before_commit);
        Self::from_config_with_remote_fetch_inner(
            cfg,
            prefetched_models,
            auth_manager,
            remote_fetch_enabled,
            || {
                if let Some(before_commit) = before_commit.take() {
                    before_commit();
                }
            },
        )
    }

    pub(crate) fn set_gateway(&self, gateway: xai_acp_lib::AcpAgentGatewaySender) {
        *self.inner.gateway.write() = Some(gateway);
    }

    /// Swap config, rebuild catalog, and reselect the model.
    pub(crate) fn apply_config(&self, new_config: config::Config) {
        if let Err(e) = new_config.validate_model_filters() {
            tracing::error!(error = %e, "ignoring config reload: invalid model filters");
            return;
        }
        let prefetched = self.inner.catalog.read().prefetched.clone();
        let new_catalog = resolve_model_catalog(&new_config, prefetched);
        let has_real_catalog = self.inner.catalog.read().has_fetched_real_catalog;
        if has_real_catalog && let Err(e) = validate_selectable(&new_config, &new_catalog) {
            tracing::error!(error = %e, "ignoring config reload: allowed_models excludes all models");
            return;
        }

        let (old_preferred, old_default_is_campaign) = {
            let cfg = self.inner.cfg.read();
            (
                cfg.models.default.clone(),
                cfg.models.default_is_campaign_driven,
            )
        };
        let new_preferred = new_config.models.default.clone();
        let has_session = self.inner.auth_manager.current_or_expired().is_some();
        *self.inner.fetch_auth.write() =
            ModelFetchAuth::resolve(&new_config.endpoints, has_session);
        *self.inner.cfg.write() = new_config.clone();
        {
            let mut cat = self.inner.catalog.write();
            if has_real_catalog {
                cat.allowlist_excludes_all = allowlist_matches_nothing(&new_config, &new_catalog);
            }
            cat.models = new_catalog;
        }
        // Catalog contents changed (config-driven re-resolve); invalidate
        // generation-keyed auth memos even when the config-watcher also
        // broadcasts InvalidateModelAuthMemo.
        self.bump_catalog_generation();

        let preferred_changed = new_preferred != old_preferred && new_preferred.is_some();
        let mut campaign_defaults = std::collections::HashSet::new();
        if new_config.models.default_is_campaign_driven
            && let Some(d) = &new_preferred
        {
            campaign_defaults.insert(d.clone());
        }
        if old_default_is_campaign && let Some(d) = &old_preferred {
            campaign_defaults.insert(d.clone());
        }
        let campaign_only_flip =
            is_campaign_only_flip(&old_preferred, &new_preferred, &campaign_defaults);
        let current_still_ok = {
            let cat = self.inner.catalog.read();
            let models = &cat.models;
            let cur = self.inner.current_model_id.read();
            models.get(cur.0.as_ref()).is_some_and(|e| {
                e.info.user_selectable && e.info.visible_for_auth(self.is_session_auth())
            })
        };
        if preferred_changed && !(campaign_only_flip && current_still_ok) {
            self.reselect_default_model(&new_config);
        } else {
            self.reselect_current_model_if_missing(&new_config);
        }
        self.revalidate_current_reasoning_effort();

        self.notify_models_updated();
    }

    /// [`Self::apply_config`] plus an unconditional default re-resolve, for remote-settings arrival while no session exists.
    pub(crate) fn apply_config_reselecting_default(&self, new_config: config::Config) {
        self.apply_config(new_config.clone());
        self.reselect_default_model(&new_config);
        self.revalidate_current_reasoning_effort();
        self.notify_models_updated();
    }

    // ── Accessors ───────────────────────────────────────────────────

    pub fn models(&self) -> IndexMap<String, ModelEntry> {
        self.inner.catalog.read().models.clone()
    }

    /// Return the complete catalog and its ACP-selectable projection from one
    /// catalog generation. Callers that authorize a persisted model must not
    /// combine independently captured `models()` and `available()` snapshots.
    pub(crate) fn models_and_available(
        &self,
    ) -> (
        IndexMap<String, ModelEntry>,
        IndexMap<acp::ModelId, acp::ModelInfo>,
    ) {
        let is_session_auth = self.is_session_auth();
        let models = self.inner.catalog.read().models.clone();
        let selectable = models
            .iter()
            .filter(|(_, entry)| entry.info.user_selectable)
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect();
        let available = available_models(&selectable, is_session_auth);
        (models, available)
    }

    pub fn endpoints(&self) -> config::EndpointsConfig {
        self.inner.cfg.read().endpoints.clone()
    }

    /// Does the current credential grant access to OAuth-only models?
    fn is_session_auth(&self) -> bool {
        self.inner
            .auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_session_auth())
    }

    fn usable_ambient_xai_from_snapshot(
        cfg: &config::Config,
        snapshot: crate::auth::AuthSelectionSnapshot,
    ) -> bool {
        resolution::classify_ambient_xai_auth(
            cfg,
            snapshot.session_eligibility,
            snapshot.first_party_env_api_key_ok,
        )
        .is_usable()
    }

    fn should_reseat_stranded_ambient_grok(
        user_picked: bool,
        usable_xai: bool,
        current: &ModelEntry,
        ready_codex_exists: bool,
    ) -> bool {
        !user_picked
            && !usable_xai
            && resolution::is_first_party_ambient_xai_entry(current)
            && ready_codex_exists
    }

    #[cfg(test)]
    fn usable_ambient_xai(&self, cfg: &config::Config) -> bool {
        Self::usable_ambient_xai_from_snapshot(cfg, self.inner.auth_manager.selection_snapshot())
    }

    fn publish_model_switch(&self) {
        self.inner
            .model_switch_watch
            .send_modify(|generation| *generation += 1);
    }

    /// Publish the first-party env-key probe verdict and repair only the
    /// implicit cold-start state it can invalidate.
    ///
    /// Presence-only startup resolution deliberately treats a non-blank env
    /// key as usable so boot does not block on I/O. If the later `/api-key`
    /// probe disproves that assumption, an implicitly seated ambient Grok may
    /// be stranded even though a ready Codex account route exists. Honourable
    /// CLI/env/config defaults and `/model` picks remain authoritative; other
    /// usable xAI routes (pin, session, deployment key) also keep Grok.
    pub(crate) fn apply_first_party_env_api_key_probe_result(&self, ok: bool) {
        #[cfg(test)]
        self.apply_first_party_env_api_key_probe_result_inner(ok, || {});
        #[cfg(not(test))]
        self.apply_first_party_env_api_key_probe_result_inner(ok);
    }

    fn apply_first_party_env_api_key_probe_result_inner(
        &self,
        ok: bool,
        #[cfg(test)] mut before_commit: impl FnMut(),
    ) {
        self.inner.auth_manager.set_first_party_env_api_key_ok(ok);
        if ok || self.inner.user_selected_model.load(Ordering::Relaxed) {
            return;
        }

        let mut retries = 0usize;
        loop {
            let auth = self.inner.auth_manager.selection_snapshot();
            // Keep one config snapshot locked through target resolution and
            // commit. Auth is deliberately not locked with model state: its
            // even generation is revalidated at the commit boundary instead.
            let cfg = self.inner.cfg.read();
            if Self::usable_ambient_xai_from_snapshot(&cfg, auth) {
                if self
                    .inner
                    .auth_manager
                    .selection_generation_is_current(auth.generation)
                {
                    return;
                }
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }

            let catalog = self.inner.catalog.read();
            let (key, entry, _, _) = resolve_default_model_for_catalog_with_usable_xai(
                &cfg,
                &catalog.models,
                auth.is_session_auth,
                catalog.has_fetched_real_catalog,
                false,
            );
            if !resolution::is_ready_selectable_openai_codex_entry(&entry, auth.is_session_auth) {
                if self
                    .inner
                    .auth_manager
                    .selection_generation_is_current(auth.generation)
                {
                    return;
                }
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }
            let target = acp::ModelId::new(key);

            // In test builds, mark the exact race boundary after target
            // resolution but before current-model commit. The seam is absent
            // from production builds.
            #[cfg(test)]
            before_commit();

            // Lock order is catalog -> current. Holding the catalog read guard
            // keeps the selected Codex target present/ready through commit;
            // rechecking both the user-pick flag and current under the selection
            // lock prevents a concurrent `/model` choice from being overwritten.
            let selection_commit = self.inner.selection_commit.write();
            let mut current = self.inner.current_model_id.write();
            if !self
                .inner
                .auth_manager
                .selection_generation_is_current(auth.generation)
            {
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }
            if self.inner.user_selected_model.load(Ordering::Relaxed)
                || !catalog
                    .models
                    .get(current.0.as_ref())
                    .is_some_and(resolution::is_first_party_ambient_xai_entry)
                || *current == target
            {
                return;
            }
            let old = current.clone();
            *current = target.clone();
            if !self
                .inner
                .auth_manager
                .selection_generation_is_current(auth.generation)
            {
                *current = old;
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }
            self.revalidate_reasoning_effort_locked(&catalog.models, &target);
            drop(current);
            drop(selection_commit);
            drop(catalog);
            drop(cfg);

            tracing::info!(
                old = %old.0,
                new = %target.0,
                "invalid first-party env key left implicit Grok unusable; reseating ready Codex"
            );
            self.publish_model_switch();
            return;
        }
    }

    #[cfg(test)]
    fn apply_first_party_env_api_key_probe_result_with_before_commit(
        &self,
        ok: bool,
        before_commit: impl FnOnce(),
    ) {
        let mut before_commit = Some(before_commit);
        self.apply_first_party_env_api_key_probe_result_inner(ok, || {
            if let Some(before_commit) = before_commit.take() {
                before_commit();
            }
        });
    }

    /// Whether a Codex-backed model is actually reachable: holding a live
    /// provider credential *and* surviving the same `user_selectable` /
    /// `visible_for_auth` filters the picker applies.
    ///
    /// Suppressing the interactive xAI login keys on this, so a Codex entry
    /// that `allowed_models` or `hidden_models` filters out must not count —
    /// otherwise the login screen is skipped for a model `/model` cannot
    /// reach, stranding the session on a default xAI model with no credential.
    ///
    /// Only *known usable* Codex entries count here. Unready entries still
    /// appear in [`Self::available`] labelled via `ready` /
    /// `readinessReason` meta — visible and available are not one bool (#133).
    pub(crate) fn has_selectable_openai_codex_model(&self) -> bool {
        let is_session_auth = self.is_session_auth();
        self.models()
            .values()
            .any(|entry| resolution::is_ready_selectable_openai_codex_entry(entry, is_session_auth))
    }

    /// Whether a campaign may nudge a newly-created session to `model_id`.
    ///
    /// Campaign defaults are soft nudges, not user choices. After a first-party
    /// xAI env key is proven unusable, a later `/new` must not revive an
    /// ambient Grok route when the same catalog snapshot contains a ready
    /// official Codex account route. All other campaign targets and usable xAI
    /// routes retain their existing behavior.
    pub(crate) fn campaign_default_is_eligible(&self, model_id: &str) -> bool {
        self.session_model_authority(Some(model_id))
            .campaign_eligible
    }

    /// Capture the catalog, auth-correct fallback, and campaign verdict from
    /// one stable auth generation. The returned data remains provisional
    /// until its generation is sealed at the session plan commit boundary.
    pub(crate) fn session_model_authority(
        &self,
        campaign_model_id: Option<&str>,
    ) -> SessionModelAuthoritySnapshot {
        #[cfg(test)]
        return self.session_model_authority_inner(campaign_model_id, || {}, || {});
        #[cfg(not(test))]
        self.session_model_authority_inner(campaign_model_id)
    }

    fn session_model_authority_inner(
        &self,
        campaign_model_id: Option<&str>,
        #[cfg(test)] mut before_selection_snapshot: impl FnMut(),
        #[cfg(test)] mut before_commit: impl FnMut(),
    ) -> SessionModelAuthoritySnapshot {
        let mut retries = 0usize;
        loop {
            let auth = self.inner.auth_manager.selection_snapshot();
            // Match the config -> catalog -> current order used by the #303
            // repair paths. Auth is validated independently through its even
            // generation at the decision boundary.
            let cfg = self.inner.cfg.read();
            let usable_xai = Self::usable_ambient_xai_from_snapshot(&cfg, auth);
            let catalog = self.inner.catalog.read();
            let models = &catalog.models;
            let ready_codex_exists = models.values().any(|candidate| {
                resolution::is_ready_selectable_openai_codex_entry(candidate, auth.is_session_auth)
            });
            let campaign_eligible = campaign_model_id.is_none_or(|model_id| {
                resolve_catalog_key(models, &acp::ModelId::new(model_id)).is_none_or(|key| {
                    let entry = models
                        .get(key.0.as_ref())
                        .expect("resolve_catalog_key returns a present key");
                    usable_xai
                        || !resolution::is_first_party_ambient_xai_entry(entry)
                        || !ready_codex_exists
                })
            });

            #[cfg(test)]
            before_selection_snapshot();
            let selection_commit = self.inner.selection_commit.read();
            let current = self.inner.current_model_id.read().clone();
            let user_selected = self.inner.user_selected_model.load(Ordering::Relaxed);
            let current_key = resolve_catalog_key(models, &current);
            let current_is_usable = current_key
                .as_ref()
                .and_then(|key| models.get(key.0.as_ref()))
                .is_some_and(|entry| {
                    entry.info.user_selectable
                        && entry.info.visible_for_auth(auth.is_session_auth)
                        && (user_selected
                            || usable_xai
                            || !resolution::is_first_party_ambient_xai_entry(entry)
                            || !ready_codex_exists)
                });
            let fallback_model_id = if current_is_usable {
                current_key.expect("usable current model has a resolved catalog key")
            } else {
                let (key, _, _, _) = resolve_default_model_for_catalog_with_usable_xai(
                    &cfg,
                    models,
                    auth.is_session_auth,
                    catalog.has_fetched_real_catalog,
                    usable_xai,
                );
                acp::ModelId::new(key)
            };
            let owned_catalog = models.clone();
            drop(selection_commit);
            #[cfg(test)]
            before_commit();
            if self
                .inner
                .auth_manager
                .selection_generation_is_current(auth.generation)
            {
                return SessionModelAuthoritySnapshot {
                    auth_generation: auth.generation,
                    catalog: owned_catalog,
                    fallback_model_id,
                    campaign_eligible,
                };
            }
            drop(catalog);
            drop(cfg);
            retries += 1;
            if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                std::thread::yield_now();
            }
        }
    }

    #[cfg(test)]
    fn campaign_default_is_eligible_with_before_decision(
        &self,
        model_id: &str,
        before_decision: impl FnOnce(),
    ) -> bool {
        let mut before_decision = Some(before_decision);
        self.session_model_authority_inner(
            Some(model_id),
            || {},
            || {
                if let Some(before_decision) = before_decision.take() {
                    before_decision();
                }
            },
        )
        .campaign_eligible
    }

    #[cfg(test)]
    fn session_model_authority_with_before_selection_snapshot(
        &self,
        before_selection_snapshot: impl FnOnce(),
    ) -> SessionModelAuthoritySnapshot {
        let mut before_selection_snapshot = Some(before_selection_snapshot);
        self.session_model_authority_inner(
            None,
            || {
                if let Some(before_selection_snapshot) = before_selection_snapshot.take() {
                    before_selection_snapshot();
                }
            },
            || {},
        )
    }

    /// ACP-visible (non-hidden) projection of the catalog.
    pub fn available(&self) -> IndexMap<acp::ModelId, acp::ModelInfo> {
        self.models_and_available().1
    }

    pub(crate) fn task_model_error(&self, requested: &str) -> Option<String> {
        let is_session_auth = self.is_session_auth();
        let cat = self.inner.catalog.read();
        let models = &cat.models;
        task_model_error_for_catalog(requested, models, is_session_auth)
    }

    pub fn current_model_id(&self) -> acp::ModelId {
        self.inner.current_model_id.read().clone()
    }

    /// The configured default that resolve fell through on (absent from the
    /// catalog, or present but not user-selectable), so a substitute is what
    /// `current_model_id` names (#131). Derived from
    /// [`resolve_default_model`]'s source: an explicit preference that was
    /// seated — ready, or kept-unready under #145 — comes back carrying its
    /// own source, not `Default`. `None` covers every other outcome, including
    /// kept-but-unready (already reported per-model as `readinessReason`).
    ///
    /// After a later catalog makes the preference honourable, the warm-cache
    /// path reseats it (when the user has not `/model`-picked), so a cleared
    /// verdict means the preference is seated — not merely "would be seated if
    /// we reselected".
    pub(crate) fn substituted_preference(&self) -> Option<resolution::SubstitutedPreference> {
        self.inner.substituted_preference.read().clone()
    }

    /// Write the #131 verdict into a `_meta` map.
    ///
    /// - `clear_when_absent = false` (`initialize`): omit the key when there is
    ///   no substitution — the omit-not-null contract.
    /// - `clear_when_absent = true` (`x.ai/models/update`): write JSON `null`
    ///   so a client holding a prior accusation can retract it.
    pub(crate) fn write_substituted_default_model_meta(
        &self,
        map: &mut serde_json::Map<String, serde_json::Value>,
        clear_when_absent: bool,
    ) {
        match self.substituted_preference() {
            Some(pref) => {
                map.insert(
                    resolution::SUBSTITUTED_DEFAULT_MODEL_META_KEY.to_string(),
                    pref.to_meta_value(),
                );
            }
            None if clear_when_absent => {
                map.insert(
                    resolution::SUBSTITUTED_DEFAULT_MODEL_META_KEY.to_string(),
                    serde_json::Value::Null,
                );
            }
            None => {}
        }
    }

    /// Refresh the substitution verdict from a resolution that just ran.
    ///
    /// Called at every `resolve_default_model` site rather than only at
    /// construction: the catalog is populated asynchronously, so a verdict
    /// taken against an empty catalog would otherwise accuse the user of
    /// configuring a model that had simply not loaded yet.
    fn record_substituted_preference(
        &self,
        cfg: &config::Config,
        resolved_source: config::ConfigSource,
    ) {
        *self.inner.substituted_preference.write() =
            resolution::substituted_preference(cfg, resolved_source);
    }

    pub(crate) fn set_current_model_id(&self, id: acp::ModelId) {
        #[cfg(test)]
        self.set_current_model_id_inner(id, None, || {});
        #[cfg(not(test))]
        self.set_current_model_id_inner(id, None);
    }

    pub(crate) fn set_current_model_and_reasoning_effort(
        &self,
        id: acp::ModelId,
        effort: Option<ReasoningEffort>,
    ) {
        #[cfg(test)]
        self.set_current_model_id_inner(id, Some(effort), || {});
        #[cfg(not(test))]
        self.set_current_model_id_inner(id, Some(effort));
    }

    fn set_current_model_id_inner(
        &self,
        id: acp::ModelId,
        effort: Option<Option<ReasoningEffort>>,
        #[cfg(test)] before_id_publish: impl FnOnce(),
    ) {
        let selection_commit = self.inner.selection_commit.write();
        self.inner
            .user_selected_model
            .store(true, Ordering::Relaxed);
        #[cfg(test)]
        before_id_publish();
        let changed = {
            let mut cur = self.inner.current_model_id.write();
            let changed = *cur != id;
            *cur = id;
            if let Some(effort) = effort {
                *self.inner.current_reasoning_effort.write() = effort;
            }
            changed
        };
        drop(selection_commit);
        if changed {
            self.inner
                .model_switch_watch
                .send_modify(|generation| *generation += 1);
        }
    }

    #[cfg(test)]
    fn set_current_model_id_with_before_id_publish(
        &self,
        id: acp::ModelId,
        before_id_publish: impl FnOnce(),
    ) {
        self.set_current_model_id_inner(id, None, before_id_publish);
    }

    #[cfg(test)]
    fn set_current_model_and_reasoning_effort_with_before_id_publish(
        &self,
        id: acp::ModelId,
        effort: Option<ReasoningEffort>,
        before_id_publish: impl FnOnce(),
    ) {
        self.set_current_model_id_inner(id, Some(effort), before_id_publish);
    }

    /// Per-model Layer-3 LazinessDetector config for `model_id` (disabled default when absent).
    pub(crate) fn laziness_detector_for(
        &self,
        model_id: &str,
    ) -> config::LazinessDetectorPerModelConfig {
        self.inner
            .catalog
            .read()
            .models
            .get(model_id)
            .map(|e| e.info().laziness_detector.clone())
            .unwrap_or_default()
    }

    /// Test-only catalog poke: inserts a `ModelEntry` keyed by `id`.
    /// Bumps generation so auth memos keyed on the prior snapshot are not reused.
    /// Prefer [`Self::apply_catalog_for_test`] when the test is about the publish
    /// path itself (etag refresh / fetch_and_apply).
    #[cfg(test)]
    pub(crate) fn insert_test_entry(&self, id: impl Into<String>, entry: ModelEntry) {
        self.inner.catalog.write().models.insert(id.into(), entry);
        self.bump_catalog_generation();
    }

    /// Test-only: publish a prefetched catalog through the real
    /// [`Self::apply_catalog`] path (same as etag refresh / `fetch_and_apply` /
    /// `reload_from_cache_manager`). Uses the manager's current config so
    /// `resolve_model_catalog` and the generation bump match production.
    #[cfg(test)]
    pub(crate) fn apply_catalog_for_test(&self, models: IndexMap<String, ModelEntry>) {
        let cfg = self.inner.cfg.read().clone();
        self.apply_catalog(&cfg, models, None);
    }

    pub(crate) fn current_reasoning_effort(&self) -> Option<ReasoningEffort> {
        *self.inner.current_reasoning_effort.read()
    }

    pub(crate) fn set_current_reasoning_effort(&self, effort: Option<ReasoningEffort>) {
        let selection_commit = self.inner.selection_commit.write();
        let current = self.inner.current_model_id.read();
        *self.inner.current_reasoning_effort.write() = effort;
        drop(current);
        drop(selection_commit);
    }

    fn revalidate_reasoning_effort_locked(
        &self,
        models: &IndexMap<String, ModelEntry>,
        current_model_id: &acp::ModelId,
    ) {
        let mut current_effort = self.inner.current_reasoning_effort.write();
        if current_effort.is_some_and(|effort| {
            !resolve_catalog_key(models, current_model_id)
                .and_then(|model_id| models.get(model_id.0.as_ref()))
                .is_some_and(|entry| model_offers_reasoning_effort(&entry.info, effort))
        }) {
            *current_effort = None;
        }
    }

    fn revalidate_current_reasoning_effort(&self) {
        #[cfg(test)]
        self.revalidate_current_reasoning_effort_inner(|| {});
        #[cfg(not(test))]
        self.revalidate_current_reasoning_effort_inner();
    }

    fn revalidate_current_reasoning_effort_inner(
        &self,
        #[cfg(test)] after_catalog_lock: impl FnOnce(),
    ) {
        // Global nested state order: catalog -> selection -> current -> effort.
        // Catalog publication and clear use the same order, so reasoning
        // revalidation cannot deadlock by owning current while waiting on a
        // catalog writer.
        let catalog = self.inner.catalog.read();
        #[cfg(test)]
        after_catalog_lock();
        let selection_commit = self.inner.selection_commit.read();
        let current_model_id = self.inner.current_model_id.read();
        let mut current_effort = self.inner.current_reasoning_effort.write();
        if current_effort.is_some_and(|effort| {
            !resolve_catalog_key(&catalog.models, &current_model_id)
                .and_then(|model_id| catalog.models.get(model_id.0.as_ref()))
                .is_some_and(|entry| model_offers_reasoning_effort(&entry.info, effort))
        }) {
            *current_effort = None;
        }
        drop(current_effort);
        drop(current_model_id);
        drop(selection_commit);
        drop(catalog);
    }

    #[cfg(test)]
    fn revalidate_current_reasoning_effort_with_after_catalog_lock(
        &self,
        after_catalog_lock: impl FnOnce(),
    ) {
        self.revalidate_current_reasoning_effort_inner(after_catalog_lock);
    }

    /// Whether the given model supports reasoning effort according to the catalog.
    pub(crate) fn model_supports_reasoning_effort(&self, model_id: &str) -> bool {
        self.inner
            .catalog
            .read()
            .models
            .get(model_id)
            .map(|e| e.info().supports_reasoning_effort)
            .unwrap_or(false)
    }

    pub(crate) fn model_offers_reasoning_effort(
        &self,
        model_id: &str,
        effort: ReasoningEffort,
    ) -> bool {
        let catalog = self.inner.catalog.read();
        resolve_catalog_key(&catalog.models, &acp::ModelId::new(model_id))
            .and_then(|resolved_id| catalog.models.get(resolved_id.0.as_ref()))
            .is_some_and(|entry| model_offers_reasoning_effort(&entry.info, effort))
    }

    pub(crate) fn model_default_reasoning_effort(&self, model_id: &str) -> Option<ReasoningEffort> {
        self.inner
            .catalog
            .read()
            .models
            .get(model_id)
            .and_then(|e| e.info().reasoning_effort)
    }

    /// The raw catalog `reasoning_efforts` list for `model_id` with no fallback,
    pub(crate) fn model_reasoning_efforts(&self, model_id: &str) -> Vec<ReasoningEffortOption> {
        self.inner
            .catalog
            .read()
            .models
            .get(model_id)
            .map(|e| e.info().reasoning_efforts.clone())
            .unwrap_or_default()
    }

    pub(crate) fn model_supports_backend_search(&self, model_id: &str) -> bool {
        let catalog = self.inner.catalog.read();
        let models = &catalog.models;
        resolve_catalog_key(models, &acp::ModelId::new(model_id))
            .and_then(|key| models.get(key.0.as_ref()))
            .map(|e| e.info().supports_backend_search)
            .unwrap_or(false)
    }

    /// Catalog wire capabilities for one model.
    ///
    /// Read this instead of copying `codex_wire` off a `SamplingConfig`: the
    /// config's copy belongs to whichever model was selected when it was
    /// built, which is not necessarily the model about to be sampled (#245,
    /// #277).
    pub(crate) fn model_codex_wire(
        &self,
        model_id: &str,
    ) -> Option<xai_grok_sampling_types::CodexWireCapabilities> {
        let catalog = self.inner.catalog.read();
        let models = &catalog.models;
        resolve_catalog_key(models, &acp::ModelId::new(model_id))
            .and_then(|key| models.get(key.0.as_ref()))
            .and_then(|e| e.info().codex_wire.clone())
    }

    /// Whether two model identities resolve to the same catalog entry.
    ///
    /// Exact equality intentionally succeeds even on a catalog miss: runtime
    /// models can be absent from the config-derived catalog (#159).
    pub(crate) fn model_ids_refer_to_same_entry(&self, left: &str, right: &str) -> bool {
        if left == right {
            return true;
        }
        let catalog = self.inner.catalog.read();
        let models = &catalog.models;
        let left = resolve_catalog_key(models, &acp::ModelId::new(left));
        let right = resolve_catalog_key(models, &acp::ModelId::new(right));
        left.is_some() && left == right
    }

    pub(crate) fn model_compactions_remaining(
        &self,
        model_id: &str,
    ) -> Option<xai_grok_sampling_types::CompactionsRemaining> {
        let catalog = self.inner.catalog.read();
        let models = &catalog.models;
        resolve_catalog_key(models, &acp::ModelId::new(model_id))
            .and_then(|key| models.get(key.0.as_ref()))
            .and_then(|e| e.info().compactions_remaining)
    }

    pub(crate) fn model_compaction_at_tokens(
        &self,
        model_id: &str,
    ) -> Option<xai_grok_sampling_types::CompactionAtTokens> {
        let catalog = self.inner.catalog.read();
        let models = &catalog.models;
        resolve_catalog_key(models, &acp::ModelId::new(model_id))
            .and_then(|key| models.get(key.0.as_ref()))
            .and_then(|e| e.info().compaction_at_tokens)
    }

    /// Catalog opt-in to display the served-checkpoint fingerprint for this model.
    pub(crate) fn model_show_model_fingerprint(&self, model_id: &str) -> bool {
        let cat = self.inner.catalog.read();
        let models = &cat.models;
        resolve_catalog_key(models, &acp::ModelId::new(model_id))
            .and_then(|key| models.get(key.0.as_ref()))
            .map(|e| e.info().show_model_fingerprint)
            .unwrap_or(false)
    }

    /// Resolved next-prompt-suggestion model pin from the live config
    pub(crate) fn prompt_suggest_model_pin(&self) -> crate::config::PromptSuggestModelPin {
        self.inner.cfg.read().prompt_suggest_model_pin.clone()
    }

    /// Whether `model_id` resolves in the current catalog — as a config key
    pub(crate) fn model_in_catalog(&self, model_id: &str) -> bool {
        let cat = self.inner.catalog.read();
        let models = &cat.models;
        resolve_catalog_key(models, &acp::ModelId::new(model_id)).is_some()
    }

    #[cfg(test)]
    fn prefetched(&self) -> Option<IndexMap<String, ModelEntry>> {
        self.inner.catalog.read().prefetched.clone()
    }

    #[cfg(test)]
    fn has_fetched_real_catalog(&self) -> bool {
        self.inner.catalog.read().has_fetched_real_catalog
    }

    // ── Mutations ───────────────────────────────────────────────────

    fn rebuild(&self, cfg: &config::Config, prefetched: Option<IndexMap<String, ModelEntry>>) {
        // Mutate, then bump — same order as apply_catalog / apply_config.
        // `on_auth_changed` reaches this via the bundled-fallback branch when a
        // remote fetch fails (or remote_fetch is off) and no real catalog was
        // ever published; without the bump, a session auth memo under the prior
        // generation would outlive a wholesale models replacement that may have
        // dropped its subject (#159 F1).
        self.inner.catalog.write().models = resolve_model_catalog(cfg, prefetched);
        self.bump_catalog_generation();
        self.revalidate_current_reasoning_effort();
    }

    /// Refresh models when the etag changes.
    pub(crate) async fn refresh_if_new_etag(&self, etag: String) {
        let same_etag = {
            let cat = self.inner.catalog.read();
            cat.etag.as_deref() == Some(etag.as_str())
        };
        if same_etag {
            let fetch_auth = *self.inner.fetch_auth.read();
            self.inner
                .cache
                .renew_ttl(&fetch_auth.cache_auth_method(), &self.cache_origin())
                .await;
            return;
        }
        tracing::info!(etag = %etag, "models etag changed, refreshing");
        self.spawn_fetch(Some(etag));
    }

    /// Auth identity changed: invalidate disk cache and refresh the catalog.
    pub async fn on_auth_changed(&self) {
        let config = self.inner.cfg.read().clone();
        crate::agent::init::update_telemetry_config(&config, &self.inner.auth_manager);
        self.inner.cache.invalidate();
        let has_session = self.inner.auth_manager.current_or_expired().is_some();
        let fetch_auth = ModelFetchAuth::resolve(&config.endpoints, has_session);
        *self.inner.fetch_auth.write() = fetch_auth;
        if self.inner.auth_manager.current_or_expired().is_none()
            && fetch_auth == ModelFetchAuth::Session
        {
            self.clear();
            return;
        }

        let remote_fetch_enabled = crate::util::config::resolve_remote_fetch_enabled();
        self.fetch_and_apply_inner(remote_fetch_enabled).await;

        let needs_bundled_fallback = {
            let cat = self.inner.catalog.read();
            !cat.has_fetched_real_catalog && cat.prefetched.is_none()
        };
        if needs_bundled_fallback {
            if remote_fetch_enabled {
                xai_grok_telemetry::unified_log::warn(
                    "model catalog: falling back to bundled defaults only",
                    None,
                    Some(serde_json::json!({
                        "trigger": "on_auth_changed",
                        "had_real_catalog": false,
                    })),
                );
            } else {
                tracing::debug!("model catalog: bundled defaults in use (remote_fetch disabled)");
            }
            self.rebuild(&config, None);
            self.reselect_current_model_if_missing(&config);

            if remote_fetch_enabled {
                self.spawn_catalog_retry();
            }
        }

        self.notify_models_updated();
    }

    fn notify_models_updated(&self) {
        let available = self.available();
        let current = self.current_model_id();
        let count = available.len();
        xai_grok_telemetry::unified_log::info(
            "model catalog: notifying clients",
            None,
            Some(serde_json::json!({
                "model_count": count,
                "current_model_id": current.0.as_ref(),
            })),
        );
        if let Some(ref gw) = *self.inner.gateway.read() {
            // #131: republish (or clear) the substitution verdict on every
            // catalog push. `initialize` is one-shot; without this, a verdict
            // taken against a bundled/empty catalog survives on the only
            // surface a client can see after the remote catalog corrects it.
            //
            // Path asymmetry (intentional until a consumer unifies them):
            // `initialize` writes the key on the *response* top-level `_meta`
            // (sibling of `modelState`); this notification writes it on
            // `SessionModelState._meta` (inside the update params). Same key
            // name, different JSON path — first consumers must branch.
            // JSON `null` means "no longer substituted".
            let mut model_state =
                acp::SessionModelState::new(current, available.values().cloned().collect());
            let mut meta = serde_json::Map::new();
            self.write_substituted_default_model_meta(&mut meta, true);
            model_state = model_state.meta(meta);
            if let Ok(params) = serde_json::value::to_raw_value(&model_state) {
                gw.forward_fire_and_forget(acp::ExtNotification::new(
                    "x.ai/models/update",
                    params.into(),
                ));
            }
        }
    }

    /// Hot-reload the catalog from `~/.grok/models_cache.json` after an external write (config-watcher detected).
    pub(crate) fn reload_from_disk_cache(&self) {
        self.reload_from_cache_manager(&self.inner.cache);
    }

    /// Core of [`Self::reload_from_disk_cache`], parameterized over the cache
    fn reload_from_cache_manager(&self, cache: &ModelsCacheManager) {
        let fetch_auth = *self.inner.fetch_auth.read();
        let Some(cached) = cache.load_fresh(&fetch_auth.cache_auth_method(), &self.cache_origin())
        else {
            tracing::debug!("models cache changed on disk but is not loadable; ignoring");
            return;
        };

        let same_content = {
            let cat = self.inner.catalog.read();
            cat.prefetched.as_ref().is_some_and(|current| {
                serde_json::to_string(current).ok() == serde_json::to_string(&cached.models).ok()
            })
        };
        if same_content {
            if cached.etag.is_some() {
                self.inner.catalog.write().etag = cached.etag;
            }
            tracing::debug!("models cache changed on disk but catalog is identical; skipping");
            return;
        }

        let cfg = self.inner.cfg.read().clone();
        let count = cached.models.len();
        self.apply_catalog(&cfg, cached.models, cached.etag);
        tracing::info!(count, "model catalog hot-reloaded from disk cache");
        xai_grok_telemetry::unified_log::info(
            "model catalog: reloaded from external disk-cache write",
            None,
            Some(serde_json::json!({ "model_count": count })),
        );
        self.notify_models_updated();
    }

    /// Retry model catalog fetch in the background with exponential backoff.
    fn spawn_catalog_retry(&self) {
        self.spawn_catalog_retry_with_backoff(crate::tools::retry::BackoffConfig::new(
            5, 5_000, 60_000,
        ));
    }

    /// [`Self::spawn_catalog_retry`] with an injectable backoff (fast in tests).
    fn spawn_catalog_retry_with_backoff(&self, backoff: crate::tools::retry::BackoffConfig) {
        if !crate::util::config::resolve_remote_fetch_enabled() {
            return;
        }
        if self
            .inner
            .retry_in_flight
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tracing::debug!("model catalog retry already in flight, skipping");
            return;
        }

        let mgr = self.clone();
        tokio::task::spawn(async move {
            let _retry_guard = RetryInFlightGuard(mgr.inner.clone());
            let result = crate::tools::retry::execute_with_backoff(
                &backoff,
                || {
                    let mgr = mgr.clone();
                    async move {
                        if mgr.inner.catalog.read().has_fetched_real_catalog {
                            return Ok(());
                        }

                        mgr.fetch_and_apply().await;

                        if mgr.inner.catalog.read().has_fetched_real_catalog {
                            Ok(())
                        } else {
                            Err("model catalog fetch returned no models")
                        }
                    }
                },
                |attempt, max_retries, delay| async move {
                    xai_grok_telemetry::unified_log::warn(
                        "model catalog: retry scheduled",
                        None,
                        Some(serde_json::json!({
                            "attempt": attempt,
                            "max_retries": max_retries,
                            "delay_ms": delay.as_millis() as u64,
                        })),
                    );
                },
            )
            .await;

            match result {
                Ok(()) => {
                    let count = mgr.available().len();
                    xai_grok_telemetry::unified_log::info(
                        "model catalog: retry succeeded",
                        None,
                        Some(serde_json::json!({ "model_count": count })),
                    );
                    mgr.notify_models_updated();
                }
                Err(e) => {
                    xai_grok_telemetry::unified_log::warn(
                        "model catalog: all retries exhausted",
                        None,
                        Some(serde_json::json!({ "error": e })),
                    );
                }
            }
        });
    }

    /// One-shot background catalog refresh after readiness; no-op when a fresh disk cache already loaded a real catalog.
    pub fn spawn_background_refresh(&self) {
        if self.inner.catalog.read().has_fetched_real_catalog {
            tracing::debug!(
                "skipping startup background model refresh: fresh cache already loaded"
            );
            return;
        }
        self.spawn_catalog_retry();
    }

    /// Refresh the model catalog on every auth token refresh.
    pub fn start_auth_refresh_watcher(&self, notify: Arc<tokio::sync::Notify>) {
        let mut first = Box::pin(notify.clone().notified_owned());
        first.as_mut().enable();
        self.start_auth_refresh_watcher_with_first(notify, first);
    }

    /// Start the watcher with a waiter that may have been armed before model
    /// construction. The next waiter is created before each reconciliation so
    /// a refresh that lands during the async fetch is handled next iteration.
    pub(crate) fn start_auth_refresh_watcher_with_first(
        &self,
        notify: Arc<tokio::sync::Notify>,
        first: Pin<Box<tokio::sync::futures::OwnedNotified>>,
    ) {
        let mgr = self.clone();
        let had_catalog_at_start = self.inner.catalog.read().has_fetched_real_catalog;
        xai_grok_telemetry::unified_log::info(
            "model catalog: auth refresh watcher started",
            None,
            Some(serde_json::json!({
                "had_real_catalog": had_catalog_at_start,
                "model_count": self.available().len(),
            })),
        );
        tokio::spawn(async move {
            let mut next = first;
            loop {
                next.as_mut().await;
                next.set(notify.clone().notified_owned());
                next.as_mut().enable();
                let had_catalog = mgr.inner.catalog.read().has_fetched_real_catalog;
                let old_count = mgr.available().len();
                xai_grok_telemetry::unified_log::info(
                    "model catalog: auth refresh watcher triggered",
                    None,
                    Some(serde_json::json!({
                        "had_real_catalog": had_catalog,
                        "model_count_before": old_count,
                    })),
                );
                // A refresh can change the catalog transport authority even
                // when the selected model stays the same. Recompute it before
                // fetching; unlike `on_auth_changed`, this path must retain the
                // historical unauthenticated fetch behavior used by custom
                // endpoints and tests.
                let config = mgr.inner.cfg.read().clone();
                let has_session = mgr.inner.auth_manager.current_or_expired().is_some();
                *mgr.inner.fetch_auth.write() =
                    ModelFetchAuth::resolve(&config.endpoints, has_session);
                mgr.fetch_and_apply().await;
                let has_catalog = mgr.inner.catalog.read().has_fetched_real_catalog;
                let new_count = mgr.available().len();
                if has_catalog {
                    if !had_catalog || new_count != old_count {
                        xai_grok_telemetry::unified_log::info(
                            "model catalog: auth refresh watcher updated catalog",
                            None,
                            Some(serde_json::json!({
                                "model_count_before": old_count,
                                "model_count_after": new_count,
                                "was_recovery": !had_catalog,
                            })),
                        );
                    }
                    mgr.notify_models_updated();
                } else {
                    xai_grok_telemetry::unified_log::warn(
                        "model catalog: auth refresh watcher fetch failed",
                        None,
                        Some(serde_json::json!({
                            "model_count": old_count,
                        })),
                    );
                }
            }
        });
    }

    /// Wipe in-memory state so a previous identity's catalog doesn't leak.
    fn clear(&self) {
        #[cfg(test)]
        self.clear_inner(|| {});
        #[cfg(not(test))]
        self.clear_inner();
    }

    fn clear_inner(&self, #[cfg(test)] after_catalog_lock: impl FnOnce()) {
        // Match the catalog publication lock order. Authority readers can see
        // either the previous catalog/selection pair or the fully-cleared
        // state, never an empty catalog with the previous explicit-pick flag.
        let mut catalog = self.inner.catalog.write();
        #[cfg(test)]
        after_catalog_lock();
        let selection_commit = self.inner.selection_commit.write();
        let current = self.inner.current_model_id.write();
        *catalog = CatalogState::default();
        self.inner
            .user_selected_model
            .store(false, Ordering::Relaxed);
        *self.inner.current_reasoning_effort.write() = None;
        *self.inner.substituted_preference.write() = None;
        self.bump_catalog_generation();
        drop(current);
        drop(selection_commit);
        drop(catalog);
    }

    #[cfg(test)]
    fn clear_with_after_catalog_lock(&self, after_catalog_lock: impl FnOnce()) {
        self.clear_inner(after_catalog_lock);
    }

    /// Build a `SamplingConfig` from the current model + auth state.
    pub fn sampling_config(&self) -> SamplingConfig {
        let config = self.inner.cfg.read().clone();
        let auth_manager = self.inner.auth_manager.as_ref();
        let is_session_auth = self.is_session_auth();
        let current_model_id = self.current_model_id();
        let all_models = self.models();
        let mut fallback;
        let current_model = match all_models
            .get(current_model_id.0.as_ref())
            .filter(|entry| {
                entry.info.user_selectable && entry.info.visible_for_auth(is_session_auth)
            })
            .or_else(|| {
                all_models.values().find(|entry| {
                    entry.info.user_selectable && entry.info.visible_for_auth(is_session_auth)
                })
            }) {
            Some(m) => m,
            None if !all_models.is_empty() => {
                tracing::error!(
                    "catalog has no model selectable for the current auth mode; withholding the sampling endpoint"
                );
                fallback = ModelEntry::fallback("", &config.endpoints);
                fallback
                    .config_validation_errors
                    .push("no model is selectable for the current authentication mode".to_owned());
                &fallback
            }
            None => {
                tracing::warn!("no models available in catalog; defaulting to bundled model");
                let default_id = crate::models::default_model().to_string();
                fallback = ModelEntry::fallback(&default_id, &config.endpoints);
                &fallback
            }
        };

        let session_auth = auth_manager.current_or_expired();
        let credentials =
            resolve_credentials(current_model, session_auth.as_ref().map(|a| a.key.as_str()));

        let mut sampling = sampling_config_for_model(
            current_model,
            credentials,
            config.endpoints.alpha_test_key.clone(),
            config.client_version.clone(),
            crate::managed_config::resolve_deployment_id(
                config.endpoints.deployment_key.as_deref(),
            ),
            None,
            &crate::agent::trusted_origins::TrustedXaiOrigins::load(),
        );
        // #110 / #131: this is not only a startup snapshot. When the session
        // path cannot resolve a model id it clones this config verbatim
        // (`resolve_sampling_config_for_model`), and the readiness latch skips
        // entries it cannot find — so an unready model here would carry the
        // user's first prompt to its endpoint with the credential stripped
        // but the destination intact. Withhold the destination too: a config
        // with no endpoint fails locally instead of reaching a stranger.
        // Catalog-unavailable / unknown ids are handled upstream by keeping
        // the configured id and deferring validation; this path always has a
        // concrete catalog entry, so `!ready` here means Unusable.
        if let (false, reason) = crate::agent::config::model_readiness(current_model) {
            let reason = reason.unwrap_or_else(|| "model is not ready".to_owned());
            tracing::error!(
                model = current_model.info().model.as_str(),
                %reason,
                "construction-time sampling config is not ready; withholding its endpoint"
            );
            sampling.base_url = String::new();
        }
        sampling
    }

    /// Disk-cache origin key for this manager's current endpoints/auth shape
    fn cache_origin(&self) -> String {
        let endpoints = self.inner.cfg.read().endpoints.clone();
        let fetch_auth = *self.inner.fetch_auth.read();
        crate::remote::models_list_url(&endpoints, fetch_auth)
    }

    fn try_load_cache(&self) -> bool {
        let fetch_auth = *self.inner.fetch_auth.read();
        let Some(cached) = self
            .inner
            .cache
            .load_fresh(&fetch_auth.cache_auth_method(), &self.cache_origin())
        else {
            return false;
        };
        let cfg = self.inner.cfg.read().clone();
        self.apply_catalog(&cfg, cached.models, cached.etag);
        true
    }

    /// A catalog-fetch session refresh bounded by `STARTUP_AUTH_REFRESH_TIMEOUT`.
    /// A hung IdP on a cold cache degrades to a session-less fetch (the
    /// bundled/cache catalog stays and the next refresh retries) instead of
    /// stalling boot, mirroring the readiness path's no-mint auth bound.
    async fn bounded_startup_auth(auth_manager: &Arc<AuthManager>) -> Option<GrokAuth> {
        Self::bounded_auth_refresh(async { auth_manager.auth().await.ok() }).await
    }

    /// Bounds an auth-refresh future to `STARTUP_AUTH_REFRESH_TIMEOUT`, yielding
    /// `None` on timeout. Split out so the timeout contract is unit-testable
    /// without a live IdP.
    async fn bounded_auth_refresh<F>(fut: F) -> Option<GrokAuth>
    where
        F: std::future::Future<Output = Option<GrokAuth>>,
    {
        match tokio::time::timeout(crate::http::STARTUP_AUTH_REFRESH_TIMEOUT, fut).await {
            Ok(auth) => auth,
            Err(_) => {
                tracing::warn!(
                    timeout_secs = crate::http::STARTUP_AUTH_REFRESH_TIMEOUT.as_secs(),
                    "model catalog: auth refresh timed out; fetching without a fresh session"
                );
                None
            }
        }
    }

    fn spawn_fetch(&self, new_etag: Option<String>) {
        self.spawn_fetch_inner(
            new_etag,
            crate::util::config::resolve_remote_fetch_enabled(),
        );
    }

    /// `remote_fetch_enabled` is a parameter so tests can drive the gate without touching on-disk config.
    fn spawn_fetch_inner(&self, new_etag: Option<String>, remote_fetch_enabled: bool) {
        if !remote_fetch_enabled {
            tracing::info!("model catalog refresh skipped: remote_fetch disabled");
            return;
        }
        if self
            .inner
            .refresh_in_flight
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tracing::debug!("model catalog refresh already in flight, skipping");
            return;
        }
        let cfg = self.inner.cfg.read().clone();
        let endpoints = cfg.endpoints.clone();
        let fetch_auth = *self.inner.fetch_auth.read();
        let auth_manager = self.inner.auth_manager.clone();
        let endpoint = self.inner.endpoint.clone();
        let mgr = self.clone();

        tokio::task::spawn(async move {
            let _refresh_guard = RefreshInFlightGuard(mgr.inner.clone());
            let auth = Self::bounded_startup_auth(&auth_manager).await;
            let new_prefetched = match tokio::time::timeout(
                crate::http::STARTUP_FETCH_TIMEOUT,
                endpoint.fetch_models(endpoints, auth, fetch_auth),
            )
            .await
            {
                Ok(models) => models,
                Err(_) => {
                    tracing::warn!("etag-triggered model refresh timed out");
                    None
                }
            };
            if !mgr.apply_refresh_result(&cfg, new_prefetched, new_etag) {
                return;
            }
            tracing::info!("models manager refreshed");
            mgr.notify_models_updated();
        });
    }

    /// Resolve the model list: tries cache first, then fetches from the network.
    pub async fn list_models(&self, strategy: RefreshStrategy) {
        match strategy {
            RefreshStrategy::Offline => {
                self.try_load_cache();
            }
            RefreshStrategy::OnlineIfUncached => {
                if self.try_load_cache() {
                    return;
                }
                self.fetch_and_apply().await;
            }
            RefreshStrategy::Online => {
                self.fetch_and_apply().await;
            }
        }
    }

    async fn fetch_and_apply(&self) {
        self.fetch_and_apply_inner(crate::util::config::resolve_remote_fetch_enabled())
            .await
    }

    /// `remote_fetch_enabled` is a parameter so tests can drive the gate
    async fn fetch_and_apply_inner(&self, remote_fetch_enabled: bool) {
        if !remote_fetch_enabled {
            tracing::info!("model catalog refresh skipped: remote_fetch disabled");
            return;
        }
        let auth = Self::bounded_startup_auth(&self.inner.auth_manager).await;
        let has_auth = auth.is_some();
        let fetch_auth = *self.inner.fetch_auth.read();
        let cfg = self.inner.cfg.read().clone();
        xai_grok_telemetry::unified_log::info(
            "model catalog: fetching",
            None,
            Some(serde_json::json!({
                "has_auth": has_auth,
                "fetch_auth": format!("{fetch_auth:?}"),
            })),
        );
        let endpoint = self.inner.endpoint.clone();
        let new_prefetched = match tokio::time::timeout(
            crate::http::STARTUP_FETCH_TIMEOUT,
            endpoint.fetch_models(cfg.endpoints.clone(), auth, fetch_auth),
        )
        .await
        {
            Ok(res) => res,
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = crate::http::STARTUP_FETCH_TIMEOUT.as_secs(),
                    "model catalog fetch timed out"
                );
                None
            }
        };
        let success = self.apply_refresh_result(&cfg, new_prefetched, None);
        if success {
            xai_grok_telemetry::unified_log::info(
                "model catalog: fetch succeeded",
                None,
                Some(serde_json::json!({
                    "model_count": self.available().len(),
                })),
            );
        }
    }

    /// Publish a resolved catalog under one atomic write, then reselect the model (default on first real catalog, else keep current if present).
    fn apply_catalog(
        &self,
        cfg: &config::Config,
        models: IndexMap<String, ModelEntry>,
        new_etag: Option<String>,
    ) {
        #[cfg(test)]
        self.apply_catalog_inner(cfg, models, new_etag, || {});
        #[cfg(not(test))]
        self.apply_catalog_inner(cfg, models, new_etag);
    }

    fn apply_catalog_inner(
        &self,
        cfg: &config::Config,
        models: IndexMap<String, ModelEntry>,
        new_etag: Option<String>,
        #[cfg(test)] mut before_selection_commit: impl FnMut(),
    ) {
        // Keep the catalog write guard through the paired selection commit.
        // Session authority readers use catalog -> selection -> current, so
        // this identical order publishes one coherent authority state.
        let mut cat = self.inner.catalog.write();
        let first_real_catalog = !cat.has_fetched_real_catalog;
        cat.has_fetched_real_catalog = true;
        cat.prefetched = Some(models);
        cat.models = resolve_model_catalog(cfg, cat.prefetched.clone());
        cat.etag = new_etag;
        cat.allowlist_excludes_all = allowlist_matches_nothing(cfg, &cat.models);
        let excludes_all = cat.allowlist_excludes_all;

        let (source, old, new_id, changed, honour_explicit_preference, needs_reselection) = loop {
            let auth = self.inner.auth_manager.selection_snapshot();
            let usable_xai = Self::usable_ambient_xai_from_snapshot(cfg, auth);
            let models = &cat.models;
            let (key, _, source, _) = resolve_default_model_for_catalog_with_usable_xai(
                cfg,
                models,
                auth.is_session_auth,
                cat.has_fetched_real_catalog,
                usable_xai,
            );
            let selection_commit = self.inner.selection_commit.write();
            let mut current = self.inner.current_model_id.write();
            #[cfg(test)]
            before_selection_commit();

            let old = current.clone();
            let user_picked = self.inner.user_selected_model.load(Ordering::Relaxed);
            let needs_reselection = match models.get(old.0.as_ref()) {
                None => true,
                Some(entry) => {
                    let not_selectable = !entry.info.user_selectable
                        || !entry.info.visible_for_auth(auth.is_session_auth);
                    let ready_codex_exists = models.values().any(|entry| {
                        resolution::is_ready_selectable_openai_codex_entry(
                            entry,
                            auth.is_session_auth,
                        )
                    });
                    let stranded_on_ambient_grok = Self::should_reseat_stranded_ambient_grok(
                        user_picked,
                        usable_xai,
                        entry,
                        ready_codex_exists,
                    );
                    not_selectable || stranded_on_ambient_grok
                }
            };
            let had_substitution = self.substituted_preference().is_some();
            let honour_explicit_preference = !first_real_catalog
                && had_substitution
                && !user_picked
                && key.as_str() != old.0.as_ref()
                && resolution::is_explicit_preference(source);
            let select_default = (first_real_catalog && !user_picked)
                || needs_reselection
                || honour_explicit_preference;
            let new_id = if select_default {
                acp::ModelId::new(Arc::from(key))
            } else {
                old.clone()
            };

            if !self
                .inner
                .auth_manager
                .selection_generation_is_current(auth.generation)
            {
                drop(current);
                drop(selection_commit);
                std::thread::yield_now();
                continue;
            }
            let changed = old != new_id;
            *current = new_id.clone();
            if !self
                .inner
                .auth_manager
                .selection_generation_is_current(auth.generation)
            {
                *current = old;
                drop(current);
                drop(selection_commit);
                std::thread::yield_now();
                continue;
            }
            self.revalidate_reasoning_effort_locked(models, &new_id);
            *self.inner.substituted_preference.write() =
                resolution::substituted_preference(cfg, source);
            drop(current);
            drop(selection_commit);
            break (
                source,
                old,
                new_id,
                changed,
                honour_explicit_preference,
                needs_reselection,
            );
        };

        // Every publish advances the generation so session `model_auth_memo`
        // entries that depended on the prior snapshot are not reused after a
        // transient miss (etag refresh that drops then restores a model).
        // Background fetch/retry paths never pass through the agent, so a
        // generation key is required — `invalidate_model_auth_memo_all_sessions`
        // only runs on config-watcher ext methods (#159 F1).
        self.bump_catalog_generation();
        drop(cat);

        if excludes_all {
            tracing::error!("allowed_models excludes all fetched models; prompts will be blocked");
        }
        if changed {
            if honour_explicit_preference && !needs_reselection {
                tracing::info!(
                    old = %old.0, new = %new_id.0, source = %source,
                    "configured preference now honourable, reseating"
                );
            } else {
                tracing::info!(
                    old = %old.0, new = %new_id.0, source = %source,
                    "catalog publication reselected current model"
                );
            }
            self.publish_model_switch();
        }
    }

    #[cfg(test)]
    fn apply_catalog_with_before_selection_commit_for_test(
        &self,
        cfg: &config::Config,
        models: IndexMap<String, ModelEntry>,
        before_selection_commit: impl FnOnce(),
    ) {
        let mut before_selection_commit = Some(before_selection_commit);
        self.apply_catalog_inner(cfg, models, None, || {
            if let Some(before_selection_commit) = before_selection_commit.take() {
                before_selection_commit();
            }
        });
    }

    fn apply_refresh_result(
        &self,
        config: &config::Config,
        new_prefetched: Option<IndexMap<String, ModelEntry>>,
        new_etag: Option<String>,
    ) -> bool {
        let Some(new_prefetched) = new_prefetched else {
            tracing::warn!("model refresh failed, leaving existing models unchanged");
            xai_grok_telemetry::unified_log::warn(
                "model catalog refresh failed",
                None,
                Some(serde_json::json!({
                    "had_real_catalog": self.inner.catalog.read().has_fetched_real_catalog,
                })),
            );
            return false;
        };
        self.apply_catalog(config, new_prefetched, new_etag);
        true
    }

    pub fn allowlist_excludes_all(&self) -> bool {
        self.inner.catalog.read().allowlist_excludes_all
    }

    /// Re-pick the default if `current_model_id` is gone from the catalog,
    /// no longer user-selectable, or hidden for the current auth mode. Always
    /// refreshes the #131 verdict: the
    /// early-return path is exactly when a stale `Some` taken against an
    /// emptier catalog would otherwise survive.
    ///
    /// Warm-cache path: a prefetched disk catalog already set
    /// `has_fetched_real_catalog`, so later remote catalogs take this method
    /// rather than [`Self::reselect_default_model`]. When a prior substitution
    /// verdict exists and resolve now honours an explicit preference that is
    /// not the seated id (and the user has not `/model`-picked), reseat —
    /// otherwise clearing the verdict would retract the accusation while the
    /// substitute stayed seated. Gating on a prior verdict also keeps
    /// campaign-only preferred flips from yanking a live session.
    fn reselect_current_model_if_missing(&self, config: &config::Config) {
        #[cfg(test)]
        self.reselect_current_model_if_missing_inner(config, || {});
        #[cfg(not(test))]
        self.reselect_current_model_if_missing_inner(config);
    }

    fn reselect_current_model_if_missing_inner(
        &self,
        config: &config::Config,
        #[cfg(test)] mut before_commit: impl FnMut(),
    ) {
        let mut retries = 0usize;
        loop {
            let auth = self.inner.auth_manager.selection_snapshot();
            let usable_xai = Self::usable_ambient_xai_from_snapshot(config, auth);
            let cat = self.inner.catalog.read();
            let models = &cat.models;
            let selection_commit = self.inner.selection_commit.write();
            let mut current = self.inner.current_model_id.write();
            let old = current.clone();
            let needs_reselection = match models.get(old.0.as_ref()) {
                None => true,
                Some(entry) => {
                    let not_selectable = !entry.info.user_selectable
                        || !entry.info.visible_for_auth(auth.is_session_auth);
                    // #303: only yank first-party ambient Grok/sentinel when a
                    // ready OpenAI Codex *account* route exists — never BYOK /
                    // auth_scheme=none / third-party CodexResponses shims.
                    let stranded_on_ambient_grok = !usable_xai
                        && resolution::is_first_party_ambient_xai_entry(entry)
                        && models.values().any(|entry| {
                            resolution::is_ready_selectable_openai_codex_entry(
                                entry,
                                auth.is_session_auth,
                            )
                        });
                    not_selectable || stranded_on_ambient_grok
                }
            };
            let (key, _, source, _) = resolve_default_model_for_catalog_with_usable_xai(
                config,
                models,
                auth.is_session_auth,
                cat.has_fetched_real_catalog,
                usable_xai,
            );
            #[cfg(test)]
            before_commit();
            let user_picked = self.inner.user_selected_model.load(Ordering::Relaxed);
            // Only reseat when we previously recorded a substitution: that is
            // the warm-cache "preference was missing, now honourable" path.
            let had_substitution = self.substituted_preference().is_some();
            let honour_explicit_preference = had_substitution
                && !user_picked
                && key.as_str() != old.0.as_ref()
                && resolution::is_explicit_preference(source);
            let should_change = needs_reselection || honour_explicit_preference;
            if user_picked && !honour_explicit_preference {
                let still_missing = models.get(old.0.as_ref()).is_none_or(|entry| {
                    !entry.info.user_selectable
                        || !entry.info.visible_for_auth(auth.is_session_auth)
                });
                if !still_missing {
                    if self
                        .inner
                        .auth_manager
                        .selection_generation_is_current(auth.generation)
                    {
                        self.revalidate_reasoning_effort_locked(models, &current);
                        *self.inner.substituted_preference.write() =
                            resolution::substituted_preference(config, source);
                        drop(current);
                        drop(selection_commit);
                        drop(cat);
                        return;
                    }
                    retries += 1;
                    if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                        std::thread::yield_now();
                    }
                    continue;
                }
            }
            if !self
                .inner
                .auth_manager
                .selection_generation_is_current(auth.generation)
            {
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }
            if !should_change {
                self.revalidate_reasoning_effort_locked(models, &current);
                *self.inner.substituted_preference.write() =
                    resolution::substituted_preference(config, source);
                drop(current);
                drop(selection_commit);
                drop(cat);
                return;
            }
            let new_id = acp::ModelId::new(Arc::from(key));
            let changed = *current != new_id;
            *current = new_id.clone();
            if !self
                .inner
                .auth_manager
                .selection_generation_is_current(auth.generation)
            {
                *current = old;
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }
            self.revalidate_reasoning_effort_locked(models, &new_id);
            *self.inner.substituted_preference.write() =
                resolution::substituted_preference(config, source);
            drop(current);
            drop(selection_commit);
            drop(cat);
            if changed {
                if honour_explicit_preference && !needs_reselection {
                    tracing::info!(
                        old = %old.0, new = %new_id.0, source = %source,
                        "configured preference now honourable, reseating"
                    );
                } else {
                    tracing::info!(
                        old = %old.0, new = %new_id.0, source = %source,
                        "current model not in new catalog, reselecting default"
                    );
                }
                self.publish_model_switch();
            }
            return;
        }
    }

    #[cfg(test)]
    fn reselect_current_model_if_missing_with_before_commit(
        &self,
        config: &config::Config,
        before_commit: impl FnOnce(),
    ) {
        let mut before_commit = Some(before_commit);
        self.reselect_current_model_if_missing_inner(config, || {
            if let Some(before_commit) = before_commit.take() {
                before_commit();
            }
        });
    }

    /// Re-resolve the default model against the current catalog.
    fn reselect_default_model(&self, config: &config::Config) {
        #[cfg(test)]
        self.reselect_default_model_inner(config, || {});
        #[cfg(not(test))]
        self.reselect_default_model_inner(config);
    }

    fn reselect_default_model_inner(
        &self,
        config: &config::Config,
        #[cfg(test)] mut before_commit: impl FnMut(),
    ) {
        let mut retries = 0usize;
        loop {
            let auth = self.inner.auth_manager.selection_snapshot();
            let usable_xai = Self::usable_ambient_xai_from_snapshot(config, auth);
            let cat = self.inner.catalog.read();
            let models = &cat.models;
            let (key, _, source, unready_reason) =
                resolve_default_model_for_catalog_with_usable_xai(
                    config,
                    models,
                    auth.is_session_auth,
                    cat.has_fetched_real_catalog,
                    usable_xai,
                );
            #[cfg(test)]
            before_commit();
            let new_id = acp::ModelId::new(Arc::from(key));
            let selection_commit = self.inner.selection_commit.write();
            let mut current = self.inner.current_model_id.write();
            if !self
                .inner
                .auth_manager
                .selection_generation_is_current(auth.generation)
            {
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }
            let old = current.clone();
            let changed = old != new_id;
            *current = new_id.clone();
            if !self
                .inner
                .auth_manager
                .selection_generation_is_current(auth.generation)
            {
                *current = old;
                retries += 1;
                if retries >= IMPLICIT_SELECTION_RETRY_YIELD_AFTER {
                    std::thread::yield_now();
                }
                continue;
            }
            self.revalidate_reasoning_effort_locked(models, &new_id);
            *self.inner.substituted_preference.write() =
                resolution::substituted_preference(config, source);
            drop(current);
            drop(selection_commit);
            drop(cat);
            if changed {
                if let Some(reason) = &unready_reason {
                    tracing::error!(
                        old = %old.0, new = %new_id.0, source = %source, %reason,
                        "re-resolved default model to an unusable catalog entry"
                    );
                } else {
                    tracing::info!(
                        old = %old.0, new = %new_id.0, source = %source,
                        "re-resolved default model after catalog populated"
                    );
                }
                self.publish_model_switch();
            }
            return;
        }
    }

    #[cfg(test)]
    fn reselect_default_model_with_before_commit(
        &self,
        config: &config::Config,
        before_commit: impl FnOnce(),
    ) {
        let mut before_commit = Some(before_commit);
        self.reselect_default_model_inner(config, || {
            if let Some(before_commit) = before_commit.take() {
                before_commit();
            }
        });
    }
}

// ── Refresh strategy ────────────────────────────────────────────────────────

/// How to resolve the model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Always fetch from network, ignore cache.
    Online,
    /// Only use cached data, never fetch.
    Offline,
    /// Use cache if fresh, otherwise fetch.
    OnlineIfUncached,
}

mod cache;
mod endpoint;
mod fetch;
mod resolution;

pub(crate) use cache::*;
pub(crate) use endpoint::*;
pub(crate) use fetch::*;
pub use fetch::{
    EarlyPrefetchHandle, EarlyPrefetchResult, start_early_prefetch,
    start_early_prefetch_settings_only, start_early_prefetch_with_auth,
};
pub(crate) use resolution::*;

#[cfg(test)]
mod selection_atomicity_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn ready_xai_entry(slug: &str) -> ModelEntry {
        let mut info = config::ModelInfo::fallback(slug);
        info.base_url = "https://api.x.ai/v1".to_string();
        ModelEntry {
            info,
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
            config_validation_errors: Vec::new(),
        }
    }

    #[test]
    fn session_authority_never_observes_mixed_explicit_selection() {
        let tmp = tempfile::tempdir().expect("temp model-selection home");
        let mut catalog = IndexMap::new();
        catalog.insert("grok-old".to_string(), ready_xai_entry("grok-old"));
        catalog.insert("grok-new".to_string(), ready_xai_entry("grok-new"));
        let manager = ModelsManager::new(
            None,
            catalog,
            acp::ModelId::new("grok-old"),
            Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default())),
            config::Config::default(),
        );

        let (selection_midpoint_tx, selection_midpoint_rx) = mpsc::channel();
        let (release_selection_tx, release_selection_rx) = mpsc::channel();
        let writer = {
            let manager = manager.clone();
            std::thread::spawn(move || {
                manager.set_current_model_id_with_before_id_publish(
                    acp::ModelId::new("grok-new"),
                    || {
                        selection_midpoint_tx
                            .send(())
                            .expect("announce explicit-selection midpoint");
                        release_selection_rx
                            .recv_timeout(Duration::from_secs(5))
                            .expect("release explicit-selection publication");
                    },
                );
            })
        };
        selection_midpoint_rx
            .recv()
            .expect("writer must reach the flag/id midpoint");

        // The writer hook exposes exactly one invalid raw pair: explicit=true
        // with the previous id. Readers must take selection_commit, whose
        // non-blocking acquisition deterministically proves that pair is not
        // observable through the authority API; no timeout/scheduling oracle
        // is involved.
        assert!(manager.inner.user_selected_model.load(Ordering::Relaxed));
        assert_eq!(manager.inner.current_model_id.read().0.as_ref(), "grok-old");
        assert!(
            manager.inner.selection_commit.try_read().is_none(),
            "authority readers cannot acquire the transaction at the torn midpoint"
        );

        let (reader_started_tx, reader_started_rx) = mpsc::channel();
        let (reader_reached_selection_tx, reader_reached_selection_rx) = mpsc::channel();
        let reader = {
            let manager = manager.clone();
            std::thread::spawn(move || {
                reader_started_tx
                    .send(())
                    .expect("announce authority reader start");
                manager.session_model_authority_with_before_selection_snapshot(|| {
                    reader_reached_selection_tx
                        .send(())
                        .expect("announce authority selection boundary");
                })
            })
        };
        reader_started_rx
            .recv()
            .expect("authority reader must start while writer is paused");
        reader_reached_selection_rx
            .recv()
            .expect("authority reader must reach the protected selection boundary");
        assert!(
            manager.inner.selection_commit.try_read().is_none(),
            "the actual authority reader remains excluded at the torn midpoint"
        );

        release_selection_tx
            .send(())
            .expect("release explicit selection");
        writer.join().expect("selection writer must finish");
        let snapshot = reader.join().expect("authority reader must finish");
        assert_eq!(snapshot.fallback_model_id.0.as_ref(), "grok-new");
    }

    #[test]
    fn authority_reader_never_sees_new_catalog_with_old_selection() {
        let tmp = tempfile::tempdir().expect("temp first-catalog selection home");
        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-default".to_string());
        let manager = ModelsManager::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("grok-old"),
            Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default())),
            cfg.clone(),
        );
        let mut catalog = IndexMap::new();
        catalog.insert("grok-old".to_string(), ready_xai_entry("grok-old"));
        catalog.insert("grok-default".to_string(), ready_xai_entry("grok-default"));

        let (catalog_midpoint_tx, catalog_midpoint_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let catalog_writer = {
            let manager = manager.clone();
            std::thread::spawn(move || {
                manager.apply_catalog_with_before_selection_commit_for_test(&cfg, catalog, || {
                    catalog_midpoint_tx
                        .send(())
                        .expect("announce catalog/selection commit midpoint");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release catalog/selection commit");
                });
            })
        };
        catalog_midpoint_rx
            .recv()
            .expect("catalog writer must reach the selection commit midpoint");

        let (reader_started_tx, reader_started_rx) = mpsc::channel();
        let reader = {
            let manager = manager.clone();
            std::thread::spawn(move || {
                reader_started_tx
                    .send(())
                    .expect("announce catalog authority reader start");
                manager.session_model_authority_with_before_selection_snapshot(|| {})
            })
        };
        reader_started_rx
            .recv()
            .expect("authority reader must start during catalog publication");
        assert!(
            manager.inner.catalog.try_read().is_none(),
            "new catalog must remain unpublished until its selection commits"
        );
        assert!(
            manager.inner.selection_commit.try_read().is_none(),
            "catalog publication must retain the selection transaction until current is committed"
        );
        assert!(
            manager.inner.current_model_id.try_read().is_none(),
            "catalog publication must retain current-model commit ownership"
        );

        release_tx.send(()).expect("release catalog publication");
        catalog_writer.join().expect("catalog writer must finish");
        let snapshot = reader.join().expect("authority reader must finish");
        assert_eq!(snapshot.fallback_model_id.0.as_ref(), "grok-default");
        assert!(snapshot.catalog.contains_key("grok-default"));
    }

    #[test]
    fn reasoning_revalidation_acquires_catalog_before_selection_state() {
        let tmp = tempfile::tempdir().expect("temp reasoning lock-order home");
        let mut catalog = IndexMap::new();
        catalog.insert("grok-current".to_string(), ready_xai_entry("grok-current"));
        let manager = ModelsManager::new(
            None,
            catalog,
            acp::ModelId::new("grok-current"),
            Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default())),
            config::Config::default(),
        );

        let (catalog_locked_tx, catalog_locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let validator = {
            let manager = manager.clone();
            std::thread::spawn(move || {
                manager.revalidate_current_reasoning_effort_with_after_catalog_lock(|| {
                    catalog_locked_tx
                        .send(())
                        .expect("announce reasoning catalog lock");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release reasoning lock-order probe");
                });
            })
        };
        catalog_locked_rx
            .recv()
            .expect("reasoning validator must acquire catalog first");

        assert!(manager.inner.catalog.try_write().is_none());
        assert!(manager.inner.selection_commit.try_write().is_some());
        assert!(manager.inner.current_model_id.try_write().is_some());
        assert!(manager.inner.current_reasoning_effort.try_write().is_some());
        release_tx.send(()).expect("release reasoning revalidation");
        validator.join().expect("reasoning validator must finish");
    }

    #[test]
    fn explicit_ambient_grok_survives_catalog_refresh() {
        let tmp = tempfile::tempdir().expect("temp explicit refresh home");
        let mut cfg = config::Config::default();
        cfg.models.default = Some("grok-default".to_string());
        let manager = ModelsManager::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("grok-default"),
            Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default())),
            cfg,
        );
        manager.set_current_model_id(acp::ModelId::new("grok-explicit"));
        let mut catalog = IndexMap::new();
        let explicit = ready_xai_entry("grok-explicit");
        catalog.insert("grok-explicit".to_string(), explicit.clone());
        catalog.insert("grok-default".to_string(), ready_xai_entry("grok-default"));

        assert!(
            !ModelsManager::should_reseat_stranded_ambient_grok(true, false, &explicit, true),
            "a ready Codex route must not override a present explicit /model Grok pick"
        );
        manager.apply_catalog_for_test(catalog);
        assert_eq!(manager.current_model_id().0.as_ref(), "grok-explicit");
    }

    #[test]
    fn clear_linearizes_after_in_flight_reselection_secondary_state() {
        let tmp = tempfile::tempdir().expect("temp clear/reselection home");
        let mut cfg = config::Config::default();
        cfg.models.default = Some("missing-preference".to_string());
        let mut catalog = IndexMap::new();
        catalog.insert(
            "grok-fallback".to_string(),
            ready_xai_entry("grok-fallback"),
        );
        let manager = ModelsManager::new(
            None,
            catalog,
            acp::ModelId::new("grok-fallback"),
            Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default())),
            cfg.clone(),
        );

        let (reselection_ready_tx, reselection_ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let reselection = {
            let manager = manager.clone();
            std::thread::spawn(move || {
                manager.reselect_default_model_with_before_commit(&cfg, || {
                    reselection_ready_tx
                        .send(())
                        .expect("announce reselection commit boundary");
                    release_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("release reselection commit");
                });
            })
        };
        reselection_ready_rx
            .recv()
            .expect("reselection must hold the catalog snapshot");

        let (clear_started_tx, clear_started_rx) = mpsc::channel();
        let clear = {
            let manager = manager.clone();
            std::thread::spawn(move || {
                clear_started_tx.send(()).expect("announce clear attempt");
                manager.clear();
            })
        };
        clear_started_rx.recv().expect("clear must start");
        assert!(manager.inner.catalog.try_write().is_none());

        release_tx.send(()).expect("release reselection");
        reselection.join().expect("reselection must finish");
        clear.join().expect("clear must finish after reselection");
        assert!(manager.models().is_empty());
        assert!(manager.substituted_preference().is_none());
        assert_eq!(manager.current_reasoning_effort(), None);
    }

    #[test]
    fn clear_cannot_split_atomic_user_model_and_effort_selection() {
        let tmp = tempfile::tempdir().expect("temp clear/user-selection home");
        let mut catalog = IndexMap::new();
        catalog.insert("grok-old".to_string(), ready_xai_entry("grok-old"));
        catalog.insert("grok-new".to_string(), ready_xai_entry("grok-new"));
        let manager = ModelsManager::new(
            None,
            catalog,
            acp::ModelId::new("grok-old"),
            Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default())),
            config::Config::default(),
        );

        let (selection_ready_tx, selection_ready_rx) = mpsc::channel();
        let (release_selection_tx, release_selection_rx) = mpsc::channel();
        let selection = {
            let manager = manager.clone();
            std::thread::spawn(move || {
                manager.set_current_model_and_reasoning_effort_with_before_id_publish(
                    acp::ModelId::new("grok-new"),
                    Some(ReasoningEffort::High),
                    || {
                        selection_ready_tx
                            .send(())
                            .expect("announce atomic user-selection midpoint");
                        release_selection_rx
                            .recv_timeout(Duration::from_secs(5))
                            .expect("release atomic user selection");
                    },
                );
            })
        };
        selection_ready_rx
            .recv()
            .expect("user selection must hold selection_commit");

        let (clear_has_catalog_tx, clear_has_catalog_rx) = mpsc::channel();
        let clear = {
            let manager = manager.clone();
            std::thread::spawn(move || {
                manager.clear_with_after_catalog_lock(|| {
                    clear_has_catalog_tx
                        .send(())
                        .expect("announce clear catalog lock");
                });
            })
        };
        clear_has_catalog_rx
            .recv()
            .expect("clear must own catalog before waiting on selection");
        assert!(manager.inner.catalog.try_read().is_none());
        assert!(manager.inner.selection_commit.try_read().is_none());

        release_selection_tx
            .send(())
            .expect("release user selection");
        selection.join().expect("user selection must finish");
        clear.join().expect("clear must finish");
        assert!(!manager.inner.user_selected_model.load(Ordering::Relaxed));
        assert_eq!(manager.current_reasoning_effort(), None);
        assert!(manager.models().is_empty());
    }
}

#[cfg(test)]
mod tests;
