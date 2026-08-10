use super::*;

/// Model-id restore resolution outcome for persisted session identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PersistedCatalogKeyResolution {
    /// Resolved to one authoritative catalog key.
    Resolved(acp::ModelId),
    /// The persisted value is a routing slug shared by multiple selectable
    /// catalog entries. Caller must require an explicit user choice.
    AmbiguousSlug {
        slug: acp::ModelId,
        matches: Vec<acp::ModelId>,
    },
    /// No selectable catalog entry maps to this persisted identity.
    Missing,
}

fn catalog_keys_for_slug(models: &IndexMap<String, ModelEntry>, slug: &str) -> Vec<acp::ModelId> {
    models
        .iter()
        .filter(|(_, entry)| entry.info.model == slug)
        .map(|(key, _)| acp::ModelId::new(key.clone()))
        .collect()
}

/// Map a model id (catalog key or routing slug) to its catalog key.
///
/// Slug matches must be unique. Ambiguous slugs (multiple catalog keys sharing
/// the same wire model) return `None` so callers cannot silently pick one.
pub(crate) fn resolve_catalog_key(
    models: &IndexMap<String, ModelEntry>,
    id: &acp::ModelId,
) -> Option<acp::ModelId> {
    let id_str = id.0.as_ref();
    if models.contains_key(id_str) {
        return Some(id.clone());
    }
    let mut matches = catalog_keys_for_slug(models, id_str);
    if matches.len() == 1 {
        return matches.pop();
    }
    None
}

/// Resolve a requested id and capture the resolver lineage at the same catalog
/// read that selected the entry. Callers must carry this identity with the
/// prepared sampler instead of reconstructing it after a catalog refresh.
pub(crate) fn resolve_catalog_identity(
    models: &IndexMap<String, ModelEntry>,
    id: &acp::ModelId,
) -> Option<xai_chat_state::CatalogIdentity> {
    let key = resolve_catalog_key(models, id)?;
    let entry = models
        .get(key.0.as_ref())
        .expect("resolve_catalog_key returns a present key");
    Some(xai_chat_state::CatalogIdentity {
        model_id: key.0.to_string(),
        route: entry.info().model.clone(),
        lineage: if key == *id {
            xai_chat_state::CatalogResolutionLineage::ExactKey
        } else {
            xai_chat_state::CatalogResolutionLineage::UniqueRoute
        },
        auth_scheme: Some(match entry.info().auth_scheme {
            xai_grok_sampler::AuthScheme::Bearer => xai_chat_state::CatalogAuthScheme::Bearer,
            xai_grok_sampler::AuthScheme::XApiKey => xai_chat_state::CatalogAuthScheme::XApiKey,
            xai_grok_sampler::AuthScheme::None => xai_chat_state::CatalogAuthScheme::None,
        }),
    })
}

/// Reconcile a persisted catalog identity with one current catalog snapshot.
///
/// Exact-key lineage never follows a reused key to a different route. A
/// unique-route identity may move to the one current entry that still carries
/// its committed route, but ambiguity fails closed.
pub(crate) fn reconcile_persisted_catalog_identity(
    models: &IndexMap<String, ModelEntry>,
    persisted: &xai_chat_state::CatalogIdentity,
) -> Option<xai_chat_state::CatalogIdentity> {
    let matching_key = models
        .get(persisted.model_id.as_str())
        .filter(|entry| entry.info().model == persisted.route)
        .map(|entry| (persisted.model_id.clone(), entry));
    let (model_id, entry) = match persisted.lineage {
        xai_chat_state::CatalogResolutionLineage::ExactKey => matching_key?,
        xai_chat_state::CatalogResolutionLineage::UniqueRoute => {
            if let Some(matching_key) = matching_key {
                matching_key
            } else {
                let mut matches = models
                    .iter()
                    .filter(|(_, entry)| entry.info().model == persisted.route);
                let first = matches.next()?;
                if matches.next().is_some() {
                    return None;
                }
                (first.0.clone(), first.1)
            }
        }
    };
    Some(xai_chat_state::CatalogIdentity {
        model_id,
        route: persisted.route.clone(),
        lineage: persisted.lineage,
        auth_scheme: Some(match entry.info().auth_scheme {
            xai_grok_sampler::AuthScheme::Bearer => xai_chat_state::CatalogAuthScheme::Bearer,
            xai_grok_sampler::AuthScheme::XApiKey => xai_chat_state::CatalogAuthScheme::XApiKey,
            xai_grok_sampler::AuthScheme::None => xai_chat_state::CatalogAuthScheme::None,
        }),
    })
}

/// Persisted-model resolver constrained to selectable (`available`) entries.
///
/// Unlike `resolve_catalog_key`, this reports ambiguity explicitly so restore
/// paths can block and ask the user to choose an exact catalog key.
pub(crate) fn selectable_catalog_resolution_for_persisted(
    models: &IndexMap<String, ModelEntry>,
    available: &IndexMap<acp::ModelId, acp::ModelInfo>,
    id: &acp::ModelId,
) -> PersistedCatalogKeyResolution {
    if available.contains_key(id) {
        return PersistedCatalogKeyResolution::Resolved(id.clone());
    }
    let id_str = id.0.as_ref();
    let matches: Vec<acp::ModelId> = models
        .iter()
        .filter(|(key, entry)| {
            available.contains_key(&acp::ModelId::new((*key).clone())) && entry.info.model == id_str
        })
        .map(|(key, _)| acp::ModelId::new(key.clone()))
        .collect();
    match matches.len() {
        0 => PersistedCatalogKeyResolution::Missing,
        1 => PersistedCatalogKeyResolution::Resolved(matches[0].clone()),
        _ => PersistedCatalogKeyResolution::AmbiguousSlug {
            slug: id.clone(),
            matches,
        },
    }
}

/// Catalog key for a persisted session model id, restricted to **selectable**
pub(crate) fn selectable_catalog_key_for_persisted(
    models: &IndexMap<String, ModelEntry>,
    available: &IndexMap<acp::ModelId, acp::ModelInfo>,
    id: &acp::ModelId,
) -> Option<acp::ModelId> {
    match selectable_catalog_resolution_for_persisted(models, available, id) {
        PersistedCatalogKeyResolution::Resolved(key) => Some(key),
        PersistedCatalogKeyResolution::AmbiguousSlug { .. }
        | PersistedCatalogKeyResolution::Missing => None,
    }
}

/// A "campaign-only" preferred flip: the default changed and either side's value
pub(crate) fn is_campaign_only_flip(
    old_preferred: &Option<String>,
    new_preferred: &Option<String>,
    campaign_defaults: &std::collections::HashSet<String>,
) -> bool {
    if new_preferred == old_preferred || new_preferred.is_none() {
        return false;
    }
    new_preferred
        .as_ref()
        .is_some_and(|p| campaign_defaults.contains(p))
        || old_preferred
            .as_ref()
            .is_some_and(|p| campaign_defaults.contains(p))
}

/// Pick the default model: CLI > env > config > remote-settings hint, falling
/// back when the preference is missing from the catalog.
///
/// The fourth return value is `Some(reason)` when an *explicit* configured
/// preference is present in the catalog but unusable (#131): the id is kept
/// (no silent substitute) and callers surface the readiness reason.
/// Whether ambient first-party xAI auth can actually sample right now.
///
/// Used only for **implicit** default selection (#303). Does not change
/// [`crate::agent::config::model_readiness`] (first-party entries stay
/// picker-ready without a live credential for login UX).
///
/// `has_usable_xai_session` must mean a **non-expired, sample-ready** first-party
/// xAI session (not merely `current_or_expired` visibility). Also counts ambient
/// `XAI_API_KEY` and a non-empty deployment key. Does **not** treat BYOK / Codex
/// as ambient xAI.
///
/// An explicit `[auth] preferred_method` pin (`api_key` / `oidc`) preserves
/// first-party Grok precedence even when no live credential is present, so the
/// model chooser cannot seat Codex while startup auth remains pinned to an
/// unavailable xAI family (Pro P0 / #303).
pub(crate) fn usable_ambient_xai_auth(cfg: &config::Config, has_usable_xai_session: bool) -> bool {
    use crate::auth::PreferredAuthMethod;
    if matches!(
        cfg.grok_com_config.preferred_method,
        Some(PreferredAuthMethod::ApiKey) | Some(PreferredAuthMethod::Oidc)
    ) {
        return true;
    }
    if has_usable_xai_session {
        return true;
    }
    if crate::agent::auth_method::has_xai_api_key_env() {
        return true;
    }
    cfg.endpoints
        .deployment_key
        .as_ref()
        .is_some_and(|k| !k.trim().is_empty())
}

/// True when this catalog entry is a ready OpenAI Codex Responses route.
fn is_ready_codex_entry(entry: &ModelEntry) -> bool {
    entry.info.api_backend == crate::sampling::ApiBackend::CodexResponses
        && crate::agent::config::model_readiness(entry).0
}

/// Bundled / ambient first-party xAI entry (no own credential, first-party origin).
///
/// Used to narrow #303 reseat so BYOK / `auth_scheme=none` / third-party routes
/// are not yanked when a ready Codex catalog appears.
pub(crate) fn is_first_party_ambient_xai_entry(entry: &ModelEntry) -> bool {
    if entry.info.api_backend == crate::sampling::ApiBackend::CodexResponses {
        return false;
    }
    if entry.has_own_credentials() {
        return false;
    }
    if entry.info.auth_scheme == xai_grok_sampler::AuthScheme::None {
        return false;
    }
    crate::util::is_xai_api_bearer_url(&entry.info.base_url)
}

pub(crate) fn resolve_default_model(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> (String, ModelEntry, config::ConfigSource, Option<String>) {
    // Test / simple callers: treat OAuth-visible session as usable. Production
    // `ModelsManager` paths use [`resolve_default_model_with_usable_xai`] with a
    // non-expired usable-token probe so hard-expired sessions do not pin Grok.
    let usable_xai = usable_ambient_xai_auth(cfg, is_session_auth);
    resolve_default_model_with_usable_xai(cfg, catalog, is_session_auth, usable_xai)
}

pub(crate) fn resolve_default_model_with_usable_xai(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
    usable_xai: bool,
) -> (String, ModelEntry, config::ConfigSource, Option<String>) {
    // Visible ≠ ready (#133/#131): auth-gated listing must not collapse onto
    // the readiness bool. Ready entries are a separate filter used only when
    // picking a fallback substitute. ACP listing (`available_models` /
    // `to_acp_model_info`) keeps unready entries labelled via meta.
    let ready_visible: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| {
            e.info.visible_for_auth(is_session_auth)
                && e.info.user_selectable
                && crate::agent::config::model_readiness(e).0
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let model_pref = configured_preference(cfg);

    let first_or_fallback = || -> (String, ModelEntry) {
        // #303: when there is no usable ambient xAI credential, do not seat the
        // bundled first-party Grok entry as the implicit default solely because
        // it sorts first and is picker-ready. Prefer a ready Codex account entry.
        if !usable_xai
            && let Some((key, entry)) = ready_visible.iter().find(|(_, e)| is_ready_codex_entry(e))
        {
            tracing::info!(
                model_id = %entry.model,
                "no usable ambient xAI auth; seating first ready Codex default"
            );
            return (key.clone(), entry.clone());
        }
        if let Some((key, first)) = ready_visible.first() {
            return (key.clone(), first.clone());
        }
        if let Some((key, entry)) = catalog
            .iter()
            .find(|(_, e)| e.info.user_selectable && crate::agent::config::model_readiness(e).0)
        {
            tracing::warn!(
                "no auth-visible selectable ready model; using first ready selectable entry"
            );
            return (key.clone(), entry.clone());
        }
        // The bundled fallback has neither a custom base URL nor validation
        // errors, so it is the safe sentinel when the entire catalog is unready.
        tracing::warn!("no ready selectable models; falling back to bundled default (pre-catalog)");
        let default_id = crate::models::default_model().to_string();
        let mut entry = ModelEntry::fallback(&default_id, &cfg.endpoints);
        entry.info.user_selectable = match ModelGlobSet::compile(cfg.models.allowed_models.as_ref())
        {
            Ok(None) => true,
            Ok(Some(set)) => set.matches(&default_id, &default_id),
            Err(_) => false,
        };
        (default_id, entry)
    };

    match &model_pref {
        None => {
            let (key, first) = first_or_fallback();
            (key, first, config::ConfigSource::Default, None)
        }
        Some(pref) => {
            let is_explicit = is_explicit_preference(pref.source);
            // A campaign-driven default arrives over the wire into the same
            // `Config` slot a user's own choice occupies, so `pref.source`
            // alone cannot tell them apart. Keeping an unready *user* choice
            // selected is #131's whole point; keeping an unready *pushed* one
            // strands the cohort on a model it cannot authenticate until
            // someone hand-edits config -- which is the exact failure
            // `pre_campaign_default` exists to undo.
            let campaign_driven = is_campaign_driven_preference(cfg, pref.source);
            // Honour the configured id against the full catalog first, before
            // the ready-only filter. An explicit preference that is present
            // but unusable must not be silently replaced (#131).
            //
            // Slug scans must stay deterministic and aligned with resume paths:
            // duplicate routing slugs are first-class and require an explicit
            // catalog key whenever lookup would be ambiguous.
            if let Some((key, entry)) = catalog
                .get_key_value(&pref.value)
                .or_else(|| catalog.iter().rev().find(|(_, m)| m.model == pref.value))
            {
                let (ready, reason) = crate::agent::config::model_readiness(entry);
                // The visibility gates apply whether or not the model is ready.
                // `allowed_models` / `hidden_models` / `supported_in_api` are
                // about whether the user may select it at all, which is a
                // different question from whether it works -- and an earlier
                // version of this returned unready explicit prefs *before*
                // checking them, so an env-var default could seat a model that
                // `available()` does not even list. `validate_selectable` does
                // not cover the env var, so this is the only gate on that path.
                let selectable =
                    entry.info.visible_for_auth(is_session_auth) && entry.info.user_selectable;
                if !ready {
                    if is_explicit && selectable && !campaign_driven {
                        let reason = reason.unwrap_or_else(|| "model is not ready".to_owned());
                        tracing::error!(
                            model_id = %pref.value,
                            source = %pref.source,
                            %reason,
                            "configured default model is not ready; keeping it selected (not swapping)"
                        );
                        return (key.clone(), entry.clone(), pref.source, Some(reason));
                    }
                    // Remote / campaign-driven / non-explicit preference, or
                    // one the user may not select: skip, so the campaign
                    // recovery below reaches `pre_campaign_default` rather
                    // than stranding a cohort on a model it cannot
                    // authenticate. An unready model is absent from
                    // `ready_visible`, so falling through here lands in the
                    // `found == None` branch where that recovery lives.
                } else if selectable {
                    return (key.clone(), entry.clone(), pref.source, None);
                }
            }

            let found = ready_visible
                .get_key_value(&pref.value)
                .or_else(|| ready_visible.iter().find(|(_, m)| m.model == pref.value));

            if let Some((key, entry)) = found {
                (key.clone(), entry.clone(), pref.source, None)
            } else {
                if is_explicit {
                    tracing::warn!(
                        model_id = %pref.value, source = %pref.source,
                        "preferred model not in available models, falling back"
                    );
                } else {
                    tracing::debug!(
                        model_id = %pref.value, source = %pref.source,
                        "remote default_model not in available models, skipping"
                    );
                }
                let campaign_pref_missing = is_campaign_driven_preference(cfg, pref.source);
                if campaign_pref_missing
                    && let Some(prev) = cfg
                        .models
                        .pre_campaign_default
                        .as_deref()
                        .filter(|s| !s.is_empty())
                    && let Some((key, entry)) = ready_visible
                        .get_key_value(prev)
                        .or_else(|| ready_visible.iter().find(|(_, m)| m.model == prev))
                {
                    tracing::info!(
                        unavailable = %pref.value, fallback = %prev,
                        "campaign-driven default unavailable in catalog; recovering the pre-campaign default"
                    );
                    return (
                        key.clone(),
                        entry.clone(),
                        config::ConfigSource::Config,
                        None,
                    );
                }
                let (key, first) = first_or_fallback();
                (key, first, config::ConfigSource::Default, None)
            }
        }
    }
}

/// Resolve the default while preserving the authoritative-catalog identity
/// invariant.
///
/// Before a real catalog arrives, [`resolve_default_model`] may synthesize the
/// bundled default as a safe startup sentinel when every locally known entry
/// is unready. Once a non-empty runtime catalog is authoritative, that same
/// sentinel would be absent from `catalog`: session setup could then persist
/// the synthetic id while sampling independently falls back to a real route.
/// Keep the pre-catalog safety behavior, but seat an actual catalog entry once
/// the caller says the snapshot is authoritative and at least one entry is
/// selectable for the current auth mode (#296). An authoritative catalog with
/// no auth-visible entry publishes the established empty-current state rather
/// than seating a model the picker deliberately hides.
pub(crate) fn resolve_default_model_for_catalog(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
    authoritative: bool,
) -> (String, ModelEntry, config::ConfigSource, Option<String>) {
    let usable_xai = usable_ambient_xai_auth(cfg, is_session_auth);
    resolve_default_model_for_catalog_with_usable_xai(
        cfg,
        catalog,
        is_session_auth,
        authoritative,
        usable_xai,
    )
}

pub(crate) fn resolve_default_model_for_catalog_with_usable_xai(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
    authoritative: bool,
    usable_xai: bool,
) -> (String, ModelEntry, config::ConfigSource, Option<String>) {
    let resolved = resolve_default_model_with_usable_xai(cfg, catalog, is_session_auth, usable_xai);
    if !authoritative || catalog.is_empty() {
        return resolved;
    }

    if catalog.get(&resolved.0).is_some_and(|entry| {
        entry.info.user_selectable && entry.info.visible_for_auth(is_session_auth)
    }) {
        return resolved;
    }

    let Some((key, entry)) = catalog.iter().find(|(_, entry)| {
        entry.info.user_selectable && entry.info.visible_for_auth(is_session_auth)
    }) else {
        let reason = "no model is selectable for the current authentication mode".to_owned();
        let mut sentinel = ModelEntry::fallback("", &cfg.endpoints);
        sentinel.info.user_selectable = false;
        sentinel.config_validation_errors.push(reason.clone());
        tracing::error!(
            synthetic_model_id = %resolved.0,
            "authoritative catalog has no auth-visible selectable model; publishing an empty current model"
        );
        return (
            String::new(),
            sentinel,
            config::ConfigSource::Default,
            Some(reason),
        );
    };
    let (ready, reason) = crate::agent::config::model_readiness(entry);
    let reason = (!ready).then(|| reason.unwrap_or_else(|| "model is not ready".to_owned()));
    tracing::warn!(
        synthetic_model_id = %resolved.0,
        selected_model_id = %key,
        "authoritative catalog has no ready default; seating a present model so identity and readiness stay observable"
    );
    (
        key.clone(),
        entry.clone(),
        config::ConfigSource::Default,
        reason,
    )
}

/// The default-model preference the user configured, in precedence order
/// (`--model` override, `GROK_DEFAULT_MODEL`, `[models] default`, remote).
///
/// Shared by [`resolve_default_model`] and [`substituted_preference`] so the
/// two can never disagree about *what* was configured.
pub(crate) fn configured_preference(cfg: &config::Config) -> Option<config::Resolved<String>> {
    config::resolve_string_flag(
        cfg.default_model_override.as_deref(),
        "GROK_DEFAULT_MODEL",
        cfg.models.default.as_deref(),
        cfg.remote_settings
            .as_ref()
            .and_then(|rs| rs.default_model.as_deref()),
    )
}

/// `true` when the preference is the user's own choice rather than a remote
/// default. One definition, used by every decision that turns on it —
/// including the warm-cache reseat in `reselect_current_model_if_missing`.
pub(crate) fn is_explicit_preference(source: config::ConfigSource) -> bool {
    matches!(
        source,
        config::ConfigSource::Cli | config::ConfigSource::Env | config::ConfigSource::Config
    )
}

/// `true` when a `Config`-sourced preference was pushed by a campaign rather
/// than written by the user. It arrives over the wire into the same slot a
/// user's own choice occupies, so `source` alone cannot tell them apart.
fn is_campaign_driven_preference(cfg: &config::Config, source: config::ConfigSource) -> bool {
    cfg.models.default_is_campaign_driven && matches!(source, config::ConfigSource::Config)
}

/// `initialize` / `x.ai/models/update` `_meta` key naming a configured default
/// that resolve fell through on, so a substitute was seated instead (#131).
///
/// Shape: `{"configuredModelId": "<id>", "source": "cli" | "env" | "config"}`.
///
/// Derived from [`resolve_default_model`]'s output source, not from a second
/// seating check: an explicit preference that was seated comes back carrying
/// `pref.source`; the only way it yields [`config::ConfigSource::Default`] is
/// the not-found fall-through (catalog absence *or* present-but-not-user-
/// selectable). Present-but-unready is different — #145 keeps that model
/// selected, so this key stays omitted.
///
/// That proxy matches seating truth when every resolve site either seats what
/// resolve returned or leaves a still-missing preference substituted. The
/// warm-cache refresh path reseats when a previously missing preference
/// becomes honourable (unless the user `/model`-picked), so a cleared verdict
/// cannot mean "would honour if reseated" while the substitute stays current.
///
/// On `initialize`, omitted (not null) when the preference was honoured — and
/// written on the *response* top-level `_meta`. On `x.ai/models/update`,
/// present as the object or as JSON `null` on `SessionModelState._meta` (a
/// different JSON path; same key name) so a prior accusation can be retracted
/// when the catalog self-corrects and the preference is seated.
pub(crate) const SUBSTITUTED_DEFAULT_MODEL_META_KEY: &str = "x.ai/substitutedDefaultModel";

/// A configured default that resolve fell through on, so a substitute was seated.
///
/// This is the one rejection a client cannot reconstruct from
/// `currentModelId` + `availableModels` alone. When the configured model is
/// present but unready, #145 keeps it selected, so `currentModelId` names it
/// and `availableModels` carries its `readinessReason`. When it is absent from
/// the catalog, or present but not user-selectable, there is nothing useful to
/// look up — and the substitute occupies every field that would otherwise have
/// named the preference (#131).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubstitutedPreference {
    /// The model id the user configured, verbatim.
    pub configured: String,
    /// Which configuration supplied it. A stale `GROK_DEFAULT_MODEL` in a shell
    /// profile and a line in `config.toml` need different remedies, and nothing
    /// else on the wire distinguishes them.
    pub source: config::ConfigSource,
}

impl SubstitutedPreference {
    /// Stable wire spelling of the configuration that supplied the preference.
    ///
    /// Exhaustive over [`config::ConfigSource`] so a new variant fails to
    /// compile rather than being silently labelled `"config"`. Only the three
    /// explicit arms are reachable: [`substituted_preference`] filters to
    /// those before constructing this type.
    pub(crate) fn source_wire(&self) -> &'static str {
        match self.source {
            config::ConfigSource::Cli => "cli",
            config::ConfigSource::Env => "env",
            config::ConfigSource::Config => "config",
            config::ConfigSource::Requirement
            | config::ConfigSource::SystemManagedConfig
            | config::ConfigSource::ManagedConfig
            | config::ConfigSource::UserConfig
            | config::ConfigSource::Remote
            | config::ConfigSource::Default => {
                unreachable!(
                    "substituted_preference only constructs Cli|Env|Config; got {}",
                    self.source
                )
            }
        }
    }

    /// Wire object for [`SUBSTITUTED_DEFAULT_MODEL_META_KEY`].
    pub(crate) fn to_meta_value(&self) -> serde_json::Value {
        serde_json::json!({
            "configuredModelId": self.configured,
            "source": self.source_wire(),
        })
    }
}

/// Did [`resolve_default_model`] substitute an explicit configured preference?
///
/// Derived from that function's **own output** rather than re-deciding it. An
/// explicit preference that was honoured — ready, or kept-unready under #145 —
/// comes back carrying `pref.source`; the only way an explicit preference
/// yields [`config::ConfigSource::Default`] is the not-found fall-back.
/// Re-implementing the readiness and visibility rules here would be a second
/// classifier that can drift from the first, which is exactly the failure this
/// module already guards against elsewhere.
///
/// Returns `None` for a campaign-driven default: the user did not write it, and
/// telling them their configuration was rejected would name the wrong culprit.
pub(crate) fn substituted_preference(
    cfg: &config::Config,
    resolved_source: config::ConfigSource,
) -> Option<SubstitutedPreference> {
    if !matches!(resolved_source, config::ConfigSource::Default) {
        return None;
    }
    let pref = configured_preference(cfg)?;
    if !is_explicit_preference(pref.source) || is_campaign_driven_preference(cfg, pref.source) {
        return None;
    }
    Some(SubstitutedPreference {
        configured: pref.value,
        source: pref.source,
    })
}

/// Filter hidden and auth-gated entries out of `catalog` and convert to ACP wire format.
pub(crate) fn available_models(
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    let visible: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| e.info.visible_for_auth(is_session_auth))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    config::to_acp_model_info(&visible)
}

/// Compiled glob matcher shared by `allowed_models`, `disabled_models`, and `hidden_models` (matched against catalog key or model id).
pub(crate) struct ModelGlobSet(GlobSet);

impl ModelGlobSet {
    /// Compile a filter list (`Ok(None)` for `None`/empty). Fails **closed**: an invalid pattern returns `Err` listing every bad one.
    pub(crate) fn compile(patterns: Option<&Vec<String>>) -> Result<Option<Self>, Vec<String>> {
        let patterns = match patterns {
            Some(p) if !p.is_empty() => p,
            _ => return Ok(None),
        };
        let mut builder = GlobSetBuilder::new();
        let mut invalid = Vec::new();
        for pat in patterns {
            match Glob::new(pat) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(_) => invalid.push(pat.clone()),
            }
        }
        if !invalid.is_empty() {
            return Err(invalid);
        }
        builder
            .build()
            .map(|set| Some(Self(set)))
            .map_err(|e| vec![e.to_string()])
    }

    fn matches(&self, key: &str, model: &str) -> bool {
        self.0.is_match(key) || self.0.is_match(model)
    }
}

/// Single source of truth for the catalog. Applies, in order: `disabled_models`
pub(crate) fn resolve_model_catalog(
    cfg: &config::Config,
    prefetched: Option<IndexMap<String, ModelEntry>>,
) -> IndexMap<String, ModelEntry> {
    let mut catalog: IndexMap<String, ModelEntry> = config::resolve_model_list(cfg, prefetched);

    if let Ok(Some(disabled)) = ModelGlobSet::compile(cfg.models.disabled_models.as_ref()) {
        let before = catalog.len();
        catalog.retain(|key, entry| !disabled.matches(key, &entry.model));
        let removed = before - catalog.len();
        if removed > 0 {
            tracing::info!(count = removed, "disabled_models: removed from catalog");
        }
    }

    match ModelGlobSet::compile(cfg.models.allowed_models.as_ref()) {
        Ok(None) => {
            for entry in catalog.values_mut() {
                entry.info.user_selectable = true;
            }
        }
        Ok(Some(allowed)) => {
            for (key, entry) in catalog.iter_mut() {
                entry.info.user_selectable = allowed.matches(key, &entry.model);
            }
        }
        Err(bad) => {
            tracing::error!(patterns = ?bad, "allowed_models: invalid glob(s); marking nothing selectable");
            for entry in catalog.values_mut() {
                entry.info.user_selectable = false;
            }
        }
    }

    if let Ok(Some(hidden)) = ModelGlobSet::compile(cfg.models.hidden_models.as_ref()) {
        for (key, entry) in catalog.iter_mut() {
            if hidden.matches(key, &entry.model) {
                entry.info.hidden = true;
            }
        }
    }

    if let Some(effort) = cfg.models.default_reasoning_effort
        && let Some(default_id) = cfg.models.default.as_deref()
        && let Some(entry) = catalog.get_mut(default_id)
        && model_offers_reasoning_effort(&entry.info, effort)
    {
        entry.info.reasoning_effort = Some(effort);
    }

    if let Some(effort) = cfg.reasoning_effort_override {
        for entry in catalog.values_mut() {
            if model_offers_reasoning_effort(&entry.info, effort) {
                entry.info.reasoning_effort = Some(effort);
            }
        }
    }

    catalog
}

/// Whether `effort` is a value this model will accept on the wire.
pub(crate) fn model_offers_reasoning_effort(
    info: &config::ModelInfo,
    effort: ReasoningEffort,
) -> bool {
    reasoning_effort_is_offered(
        info.supports_reasoning_effort,
        &info.reasoning_efforts,
        effort,
    )
}

pub(crate) fn reasoning_effort_is_offered(
    supports_reasoning_effort: bool,
    reasoning_efforts: &[ReasoningEffortOption],
    effort: ReasoningEffort,
) -> bool {
    if !supports_reasoning_effort {
        return false;
    }
    if reasoning_efforts.is_empty() {
        crate::agent::session_config::SELECTABLE_REASONING_EFFORTS.contains(&effort)
    } else {
        reasoning_efforts.iter().any(|opt| opt.value == effort)
    }
}

/// True when an active `allowed_models` allowlist leaves no selectable model.
pub(crate) fn allowlist_matches_nothing(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
) -> bool {
    cfg.models
        .allowed_models
        .as_ref()
        .is_some_and(|a| !a.is_empty())
        && !catalog.values().any(|e| e.info.user_selectable)
}

/// Reject an `allowed_models` allowlist that leaves no selectable model, or excludes an explicitly configured default; run only against a real catalog.
pub(crate) fn validate_selectable(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
) -> Result<(), String> {
    let Some(allowed) = cfg.models.allowed_models.as_ref().filter(|a| !a.is_empty()) else {
        return Ok(());
    };
    let patterns = allowed.join(", ");
    if !catalog.values().any(|e| e.info.user_selectable) {
        return Err(format!(
            "None of your available models match allowed_models ({patterns}). \
             Broaden the patterns or remove allowed_models, then try again."
        ));
    }
    for (src, id) in [
        ("default", cfg.models.default.as_deref()),
        ("-m flag", cfg.default_model_override.as_deref()),
    ] {
        if let Some(id) = id
            && let Some(entry) = catalog
                .get(id)
                .or_else(|| catalog.values().find(|e| e.model == id))
            && !entry.info.user_selectable
        {
            return Err(format!(
                "\"{id}\" (your {src}) isn't allowed by allowed_models ({patterns}). \
                 Add it to allowed_models, or set a different model."
            ));
        }
    }
    Ok(())
}
