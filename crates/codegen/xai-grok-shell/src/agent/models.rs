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
    if config::find_model_by_id(available, requested).is_some_and(&is_available) {
        return None;
    }

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
    Some(format!("Unknown Task.model slug '{requested}'. {guidance}"))
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
    pub auth_scheme: xai_grok_sampler::AuthScheme,
    pub supports_backend_search: bool,
    pub compactions_remaining: Option<xai_grok_sampling_types::CompactionsRemaining>,
    pub compaction_at_tokens: Option<xai_grok_sampling_types::CompactionAtTokens>,
    pub codex_wire: Option<xai_grok_sampling_types::CodexWireCapabilities>,
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
        let has_session = self.auth_manager.current_or_expired().is_some();
        let fetch_auth = ModelFetchAuth::resolve(&self.cfg.endpoints, has_session);
        let current_reasoning_effort = self.cfg.models.default_reasoning_effort;
        ModelsManager {
            inner: Arc::new(Inner {
                catalog: RwLock::new(CatalogState {
                    prefetched: self.prefetched,
                    models: self.models,
                    ..Default::default()
                }),
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

impl ModelsManager {
    /// Original routing model and alias lineage for one exact catalog key.
    /// Opaque sampling-config model overrides must not replace this identity.
    pub(crate) fn catalog_route_identity(&self, catalog_model_id: &str) -> Option<(String, bool)> {
        self.inner
            .catalog
            .read()
            .models
            .get(catalog_model_id)
            .map(|entry| {
                let route = entry.info().model.clone();
                let allows_route_remap = catalog_model_id != route;
                (route, allows_route_remap)
            })
    }

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
        let models = &catalog.models;
        if preferred_id_must_exist && preferred_id.is_some_and(|id| !models.contains_key(id)) {
            return None;
        }
        let preferred = preferred_id
            .and_then(|id| resolve_catalog_key(models, &acp::ModelId::new(id)))
            .and_then(|key| {
                models
                    .get(key.0.as_ref())
                    .filter(|entry| {
                        entry.info().model == routing_model
                            || alternate_preferred_route == Some(entry.info().model.as_str())
                    })
                    .map(|entry| (key.0.to_string(), entry))
            });
        let (model_id, entry) = if let Some(preferred) = preferred {
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
            auth_scheme: info.auth_scheme,
            supports_backend_search: info.supports_backend_search,
            compactions_remaining: info.compactions_remaining,
            compaction_at_tokens: info.compaction_at_tokens,
            codex_wire: info.codex_wire.clone(),
        })
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
        let has_session = auth_manager.current_or_expired().is_some();
        let is_session_auth = auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_session_auth());
        let fetch_auth = ModelFetchAuth::resolve(&cfg.endpoints, has_session);
        crate::remote::validate_models_catalog_auth(
            &cfg.endpoints,
            fetch_auth,
            remote_fetch_enabled,
        )?;
        let prefetched_models = prefetched_models.or_else(|| {
            let cache = ModelsCacheManager::new();
            cache
                .load_fresh(
                    &fetch_auth.cache_auth_method(),
                    &crate::remote::models_list_url(&cfg.endpoints, fetch_auth),
                )
                .map(|c| c.models)
        });
        let has_prefetched = prefetched_models.is_some();
        let catalog = resolve_model_catalog(cfg, prefetched_models.clone());

        if has_prefetched {
            validate_selectable(cfg, &catalog)?;
        }

        let (current_model_key, current_model, model_source, unready_default_reason) =
            resolve_default_model(cfg, &catalog, is_session_auth);

        if let Some(reason) = &unready_default_reason {
            tracing::error!(
                model_id = %current_model.model,
                source = %model_source,
                %reason,
                "default model resolved to an unusable configured preference"
            );
        } else {
            tracing::info!(
                model_id = %current_model.model,
                source = %model_source,
                "default model resolved"
            );
        }

        let current_model_id = acp::ModelId::new(Arc::from(current_model_key));

        let mgr = Self::new(
            prefetched_models,
            catalog,
            current_model_id,
            auth_manager,
            cfg.clone(),
        );
        mgr.record_substituted_preference(cfg, model_source);
        if has_prefetched {
            mgr.inner.catalog.write().has_fetched_real_catalog = true;
        }
        Ok(mgr)
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
            models
                .get(cur.0.as_ref())
                .is_some_and(|e| e.info.user_selectable)
        };
        if preferred_changed && !(campaign_only_flip && current_still_ok) {
            self.reselect_default_model(&new_config);
        } else {
            self.reselect_current_model_if_missing(&new_config);
        }

        self.notify_models_updated();
    }

    /// [`Self::apply_config`] plus an unconditional default re-resolve, for remote-settings arrival while no session exists.
    pub(crate) fn apply_config_reselecting_default(&self, new_config: config::Config) {
        self.apply_config(new_config.clone());
        self.reselect_default_model(&new_config);
        self.notify_models_updated();
    }

    // ── Accessors ───────────────────────────────────────────────────

    pub fn models(&self) -> IndexMap<String, ModelEntry> {
        self.inner.catalog.read().models.clone()
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
        self.models().values().any(|entry| {
            entry.is_openai_codex_profile()
                && entry.info.user_selectable
                && entry.info.visible_for_auth(is_session_auth)
                && config::model_readiness(entry).0
        })
    }

    /// ACP-visible (non-hidden) projection of the catalog.
    pub fn available(&self) -> IndexMap<acp::ModelId, acp::ModelInfo> {
        let snapshot = {
            let cat = self.inner.catalog.read();
            let models = &cat.models;
            models.clone()
        };

        let selectable: IndexMap<_, _> = snapshot
            .into_iter()
            .filter(|(_, e)| e.info.user_selectable)
            .collect();

        available_models(&selectable, self.is_session_auth())
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
        self.inner
            .user_selected_model
            .store(true, Ordering::Relaxed);
        self.set_current_model_id_internal(id);
    }

    fn set_current_model_id_internal(&self, id: acp::ModelId) {
        let changed = {
            let mut cur = self.inner.current_model_id.write();
            let changed = *cur != id;
            *cur = id;
            changed
        };
        if changed {
            self.inner
                .model_switch_watch
                .send_modify(|generation| *generation += 1);
        }
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
        *self.inner.current_reasoning_effort.write() = effort;
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
            loop {
                notify.notified().await;
                if !crate::util::config::resolve_remote_fetch_enabled() {
                    tracing::debug!(
                        "model catalog: auth refresh watcher skipped (remote_fetch disabled)"
                    );
                    continue;
                }
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
        *self.inner.catalog.write() = CatalogState::default();
        // A new identity starts fresh: drop the prior user's pick so its
        // first catalog reselects that identity's default.
        self.inner
            .user_selected_model
            .store(false, Ordering::Relaxed);
        // Same invariant for #131: a previous identity's substitution verdict
        // must not accuse the new one of rejecting a preference it never held.
        *self.inner.substituted_preference.write() = None;
        // The catalog just became empty, which is a content change like any
        // other -- without this a memo taken under the previous identity stays
        // valid at the same generation and is served after the wipe (#159).
        self.bump_catalog_generation();
    }

    /// Build a `SamplingConfig` from the current model + auth state.
    pub fn sampling_config(&self) -> SamplingConfig {
        let config = self.inner.cfg.read().clone();
        let auth_manager = self.inner.auth_manager.as_ref();
        let current_model_id = self.current_model_id();
        let all_models = self.models();
        let fallback;
        let current_model = match all_models
            .get(current_model_id.0.as_ref())
            .or_else(|| all_models.values().next())
        {
            Some(m) => m,
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
        let (first_real_catalog, excludes_all) = {
            let mut cat = self.inner.catalog.write();
            let first_real_catalog = !cat.has_fetched_real_catalog;
            cat.has_fetched_real_catalog = true;
            cat.prefetched = Some(models);
            cat.models = resolve_model_catalog(cfg, cat.prefetched.clone());
            cat.etag = new_etag;
            cat.allowlist_excludes_all = allowlist_matches_nothing(cfg, &cat.models);
            (first_real_catalog, cat.allowlist_excludes_all)
        };
        // Every publish advances the generation so session `model_auth_memo`
        // entries that depended on the prior snapshot are not reused after a
        // transient miss (etag refresh that drops then restores a model).
        // Background fetch/retry paths never pass through the agent, so a
        // generation key is required — `invalidate_model_auth_memo_all_sessions`
        // only runs on config-watcher ext methods (#159 F1).
        self.bump_catalog_generation();
        if excludes_all {
            tracing::error!("allowed_models excludes all fetched models; prompts will be blocked");
        }

        // Respect an explicit pre-catalog `/model` pick: auto-select the
        // default on the first catalog only when the user hasn't chosen.
        // Either way a now-invalid selection is replaced.
        if first_real_catalog && !self.inner.user_selected_model.load(Ordering::Relaxed) {
            self.reselect_default_model(cfg);
        } else {
            self.reselect_current_model_if_missing(cfg);
        }
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

    /// Re-pick the default if `current_model_id` is gone from the catalog *or*
    /// no longer user-selectable. Always refreshes the #131 verdict: the
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
        let current = self.inner.current_model_id.read().clone();
        let needs_reselection = {
            let cat = self.inner.catalog.read();
            let models = &cat.models;
            match models.get(current.0.as_ref()) {
                None => true,
                Some(entry) => !entry.info.user_selectable,
            }
        };
        let (key, _, source, _) = {
            let cat = self.inner.catalog.read();
            let models = &cat.models;
            resolve_default_model(config, models, self.is_session_auth())
        };
        let user_picked = self.inner.user_selected_model.load(Ordering::Relaxed);
        // Only reseat when we previously recorded a substitution: that is the
        // warm-cache "preference was missing, now honourable" path. Without
        // this gate, a campaign-only preferred flip (which deliberately takes
        // this method rather than reselect_default_model) would yank a live
        // session onto the pushed default.
        let had_substitution = self.substituted_preference().is_some();
        let honour_explicit_preference = had_substitution
            && !user_picked
            && key.as_str() != current.0.as_ref()
            && resolution::is_explicit_preference(source);
        self.record_substituted_preference(config, source);
        if !needs_reselection && !honour_explicit_preference {
            return;
        }
        let new_id = acp::ModelId::new(Arc::from(key));
        if honour_explicit_preference && !needs_reselection {
            tracing::info!(
                old = %current.0, new = %new_id.0, source = %source,
                "configured preference now honourable, reseating"
            );
        } else {
            tracing::info!(
                old = %current.0, new = %new_id.0, source = %source,
                "current model not in new catalog, reselecting default"
            );
        }
        self.set_current_model_id_internal(new_id);
    }

    /// Re-resolve the default model against the current catalog.
    fn reselect_default_model(&self, config: &config::Config) {
        let (key, _, source, unready_reason) = {
            let cat = self.inner.catalog.read();
            let models = &cat.models;
            resolve_default_model(config, models, self.is_session_auth())
        };
        let new_id = acp::ModelId::new(Arc::from(key));
        // Recorded whether or not the selection changes: the catalog landing is
        // exactly when an "absent" verdict taken against an emptier catalog
        // becomes wrong, and that correction is independent of whether the
        // resolved key moved.
        self.record_substituted_preference(config, source);
        let current = self.inner.current_model_id.read().clone();
        if current.0.as_ref() != new_id.0.as_ref() {
            if let Some(reason) = &unready_reason {
                tracing::error!(
                    old = %current.0, new = %new_id.0, source = %source, %reason,
                    "re-resolved default model to an unusable configured preference"
                );
            } else {
                tracing::info!(
                    old = %current.0, new = %new_id.0, source = %source,
                    "re-resolved default model after catalog populated"
                );
            }
            self.set_current_model_id_internal(new_id);
        }
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
mod tests;
