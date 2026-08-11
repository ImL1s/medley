use super::*;

fn test_manager() -> ModelsManager {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let tmp = std::env::temp_dir().join("grok-test-models-manager");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .cache(test_cache_manager(&tmp))
    .build()
}

/// #110: with an empty catalog, `sampling_config` synthesises a fallback
/// entry against `models_base_url`. Pointed anywhere non-first-party that
/// entry is credential-less and unready, and the choke point strips it — but
/// stripping is not enough here. When the session path cannot resolve a model
/// id it clones this construction-time config verbatim
/// (`resolve_sampling_config_for_model`), and the readiness latch skips
/// entries it cannot find, so the user's first prompt would be sent to that
/// origin with no authentication. Unready has to mean unusable at this seam.
#[test]
fn construction_fallback_is_unusable_when_the_catalog_endpoint_is_external() {
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = std::env::temp_dir().join("grok-test-fallback-origin");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mut cfg = config::Config::default();
    cfg.endpoints.models_base_url = Some("https://third-party.example/v1".to_string());
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        cfg,
    )
    .cache(test_cache_manager(&tmp))
    .build();

    let sampling = mgr.sampling_config();
    assert!(
        sampling.base_url.is_empty(),
        "an unready construction fallback must not carry a usable endpoint, got {}",
        sampling.base_url
    );
    assert_eq!(sampling.api_key, None, "and no credential with it");
}

/// The same seam on a first-party endpoint is the normal startup path and
/// must keep working.
#[test]
fn construction_fallback_is_usable_on_a_first_party_endpoint() {
    let tmp = std::env::temp_dir().join("grok-test-fallback-first-party");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .cache(test_cache_manager(&tmp))
    .build();

    assert!(
        !mgr.sampling_config().base_url.is_empty(),
        "the first-party default is the normal startup path"
    );
}

#[tokio::test]
async fn catalog_retry_recovers_after_endpoint_returns() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecoveringEndpoint {
        calls: Arc<AtomicUsize>,
        catalog: IndexMap<String, ModelEntry>,
    }
    impl ModelsEndpoint for RecoveringEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let out = if n == 0 {
                None
            } else {
                Some(self.catalog.clone())
            };
            Box::pin(async move { out })
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = std::env::temp_dir().join("grok-test-catalog-retry");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(RecoveringEndpoint {
        calls: calls.clone(),
        catalog: make_prefetched(&["grok-4"]),
    }))
    .build();
    assert!(!mgr.has_fetched_real_catalog());

    mgr.spawn_catalog_retry_with_backoff(crate::tools::retry::BackoffConfig::new(5, 1, 10));

    let mut recovered = false;
    for _ in 0..200 {
        if mgr.has_fetched_real_catalog() {
            recovered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        recovered,
        "catalog retry did not recover after the endpoint returned"
    );
    assert!(mgr.models().contains_key("grok-4"));
    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "expected a failed attempt then a success",
    );
}

#[tokio::test]
async fn offline_strategy_serves_cache_without_fetching() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingEndpoint {
        calls: Arc<AtomicUsize>,
    }
    impl ModelsEndpoint for CountingEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { None })
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config_from_toml("[models]\ndefault = \"grok-4.5\""),
    )
    .endpoint(Arc::new(CountingEndpoint {
        calls: calls.clone(),
    }))
    .cache(test_cache_manager(tmp.path()))
    .build();

    let seeder = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    seeder.persist(
        &make_prefetched(&["grok-4.5"]),
        Some("etag-x"),
        auth_method,
        &mgr.cache_origin(),
    );

    mgr.list_models(RefreshStrategy::Offline).await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "Offline must serve the disk cache, never hit the transport",
    );
    assert!(mgr.models().contains_key("grok-4.5"));
    assert!(mgr.has_fetched_real_catalog());
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4.5",
        "first real catalog from the disk cache must resolve the configured default",
    );
}

#[tokio::test]
async fn auth_refresh_watcher_refetches_on_notify() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NotifyEndpoint {
        calls: Arc<AtomicUsize>,
        catalog: IndexMap<String, ModelEntry>,
    }
    impl ModelsEndpoint for NotifyEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let catalog = self.catalog.clone();
            Box::pin(async move { Some(catalog) })
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = std::env::temp_dir().join("grok-test-auth-refresh-watcher");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(NotifyEndpoint {
        calls: calls.clone(),
        catalog: make_prefetched(&["grok-4"]),
    }))
    .build();
    assert!(!mgr.has_fetched_real_catalog());

    let notify = Arc::new(tokio::sync::Notify::new());
    mgr.start_auth_refresh_watcher(notify.clone());
    notify.notify_one();

    let mut updated = false;
    for _ in 0..200 {
        if mgr.has_fetched_real_catalog() {
            updated = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(updated, "watcher did not re-fetch the catalog on notify");
    assert!(mgr.models().contains_key("grok-4"));
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test(start_paused = true)]
async fn hanging_fetch_does_not_block_refresh() {
    struct HangingEndpoint;
    impl ModelsEndpoint for HangingEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            Box::pin(std::future::pending())
        }
    }

    let tmp = std::env::temp_dir().join("grok-test-hanging-fetch");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(HangingEndpoint))
    .build();

    tokio::time::timeout(
        crate::http::STARTUP_FETCH_TIMEOUT * 10,
        mgr.fetch_and_apply_inner(true),
    )
    .await
    .expect("fetch_and_apply_inner must return despite a hanging endpoint");

    assert!(
        !mgr.has_fetched_real_catalog(),
        "a timed-out fetch must not mark a real catalog",
    );
}

#[tokio::test(start_paused = true)]
async fn slow_fetch_within_timeout_still_applies() {
    // "Slow but succeeds": a fetch that returns just under STARTUP_FETCH_TIMEOUT
    // must still be applied, not degraded to offline.
    struct SlowEndpoint {
        catalog: IndexMap<String, ModelEntry>,
        delay: std::time::Duration,
    }
    impl ModelsEndpoint for SlowEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            let catalog = self.catalog.clone();
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Some(catalog)
            })
        }
    }

    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(SlowEndpoint {
        catalog: make_prefetched(&["grok-4"]),
        delay: crate::http::STARTUP_FETCH_TIMEOUT / 2,
    }))
    .build();

    mgr.fetch_and_apply_inner(true).await;
    assert!(
        mgr.has_fetched_real_catalog(),
        "a fetch within the timeout must apply, not degrade",
    );
    assert!(mgr.models().contains_key("grok-4"));
}

#[tokio::test(start_paused = true)]
async fn etag_refresh_is_bounded_and_single_flighted() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingHangEndpoint {
        calls: Arc<AtomicUsize>,
    }
    impl ModelsEndpoint for CountingHangEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(CountingHangEndpoint {
        calls: calls.clone(),
    }))
    .build();

    // First etag change spawns a bounded fetch; let the task register in-flight.
    mgr.spawn_fetch_inner(Some("etag-1".into()), true);
    tokio::task::yield_now().await;
    // Single-flight: a second spawn while one is in flight must not fetch again.
    mgr.spawn_fetch_inner(Some("etag-2".into()), true);
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "single-flight: only one etag fetch in flight at a time",
    );

    // Advance past the bound so the hung fetch is abandoned and the guard clears.
    tokio::time::sleep(crate::http::STARTUP_FETCH_TIMEOUT * 2).await;
    tokio::task::yield_now().await;

    // Guard released → a later etag change fetches again.
    mgr.spawn_fetch_inner(Some("etag-3".into()), true);
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "after the timeout cleared the in-flight guard, a new etag fetch proceeds",
    );

    // remote_fetch disabled is a no-op: no additional fetch.
    mgr.spawn_fetch_inner(Some("etag-4".into()), false);
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "disabled gate must not fetch"
    );
}

fn config_from_toml(toml: &str) -> config::Config {
    config::Config::new_from_toml_cfg(&toml::from_str(toml).unwrap()).unwrap()
}

#[test]
fn model_show_model_fingerprint_reads_catalog_flag() {
    let mgr = test_manager();

    let mut flagged = ModelEntry {
        info: config::ModelInfo::fallback("fp-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    flagged.info.show_model_fingerprint = true;
    mgr.insert_test_entry("fp-model", flagged);

    mgr.insert_test_entry(
        "plain-model",
        ModelEntry {
            info: config::ModelInfo::fallback("plain-model"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
            config_validation_errors: Vec::new(),
        },
    );

    let mut custom = ModelEntry {
        info: config::ModelInfo::fallback("enterprise-slug"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    custom.info.show_model_fingerprint = true;
    mgr.insert_test_entry("enterprise-key", custom);

    assert!(mgr.model_show_model_fingerprint("fp-model"));
    assert!(!mgr.model_show_model_fingerprint("plain-model"));
    assert!(!mgr.model_show_model_fingerprint("missing-model"));
    assert!(
        mgr.model_show_model_fingerprint("enterprise-slug"),
        "slug lookup must resolve to the catalog key and read the flag",
    );
    assert!(mgr.model_show_model_fingerprint("enterprise-key"));
}

#[test]
fn default_model_honors_allowlist_when_no_default_set() {
    let cfg = config_from_toml(
        r#"
            [models]
            allowed_models = ["keep-*"]
            [model.zzz-first]
            model = "zzz-first"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.keep-one]
            model = "keep-one"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
    );
    let catalog = resolve_model_catalog(&cfg, None);
    let (_key, entry, _src, _) = resolve_default_model(&cfg, &catalog, true);
    assert!(
        entry.info.user_selectable,
        "picked non-selectable {}",
        entry.model
    );
}

#[test]
fn validate_selectable_rejects_bad_allowlists() {
    let excluded = config_from_toml(
        r#"
            [models]
            default = "grok-3"
            allowed_models = ["grok-4*"]
            [model.grok-3]
            model = "grok-3"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
    );
    let catalog = resolve_model_catalog(&excluded, None);
    assert!(
        validate_selectable(&excluded, &catalog)
            .unwrap_err()
            .contains("grok-3")
    );

    let zero = config_from_toml(
        r#"
            [models]
            allowed_models = ["nomatch-*"]
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
    );
    let catalog = resolve_model_catalog(&zero, None);
    assert!(validate_selectable(&zero, &catalog).is_err());
}

#[tokio::test]
async fn refresh_if_new_etag_skips_when_same() {
    let mgr = test_manager();
    mgr.inner.catalog.write().etag = Some("\"abc123\"".to_string());

    mgr.refresh_if_new_etag("\"abc123\"".to_string()).await;
    assert_eq!(
        mgr.inner.catalog.read().etag.as_deref(),
        Some("\"abc123\""),
        "etag should remain unchanged when same"
    );
}

#[tokio::test]
async fn set_current_model_id_change_fires_watch_to_all_subscribers() {
    let mgr = test_manager();
    let mut rx_a = mgr.subscribe_model_switch();
    let mut rx_b = mgr.subscribe_model_switch();
    let initial_a = *rx_a.borrow_and_update();
    let initial_b = *rx_b.borrow_and_update();
    assert_eq!(initial_a, initial_b);

    mgr.set_current_model_id(acp::ModelId::new("default"));
    let same_id_ticked = tokio::time::timeout(std::time::Duration::from_millis(25), rx_a.changed())
        .await
        .is_ok();
    assert!(
        !same_id_ticked,
        "set_current_model_id(same id) must NOT bump the watch generation",
    );

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    tokio::time::timeout(std::time::Duration::from_millis(100), rx_a.changed())
        .await
        .expect("rx_a saw the switch")
        .expect("watch channel still open");
    tokio::time::timeout(std::time::Duration::from_millis(100), rx_b.changed())
        .await
        .expect("rx_b saw the switch")
        .expect("watch channel still open");
    assert_ne!(*rx_a.borrow(), initial_a);
    assert_eq!(*rx_a.borrow(), *rx_b.borrow());
    assert!(mgr.model_switch_generation() > initial_a);
}

#[tokio::test]
async fn model_switch_generation_snapshot_reflects_current_state() {
    let mgr = test_manager();
    let start = mgr.model_switch_generation();
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    assert_eq!(mgr.model_switch_generation(), start + 1);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    assert_eq!(mgr.model_switch_generation(), start + 1);
    mgr.set_current_model_id(acp::ModelId::new("grok-3"));
    assert_eq!(mgr.model_switch_generation(), start + 2);
}

#[test]
fn first_catalog_reselect_bumps_model_switch_watch() {
    let mgr = test_manager();
    let start = mgr.model_switch_generation();
    let cfg = config_from_toml("[models]\ndefault = \"grok-4.5\"");
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4.5", "grok-4"])), None);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4.5");
    assert!(
        mgr.model_switch_generation() > start,
        "background reselection must fire the model-switch watch",
    );
}

#[test]
fn reselect_missing_current_model_bumps_watch() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4", "grok-3"])), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    let start = mgr.model_switch_generation();
    // A later catalog drops the current model → reselect_current_model_if_missing.
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-3"])), None);
    assert_ne!(mgr.current_model_id().0.as_ref(), "grok-4");
    assert!(
        mgr.model_switch_generation() > start,
        "reselecting away from a removed current model must fire the watch",
    );
}

/// #296: once a non-empty runtime catalog is authoritative, the manager must
/// never retain or synthesize a current id that is absent from that catalog.
/// Even an all-unready catalog has more truthful identities than the bundled
/// pre-catalog sentinel: seating a real entry lets the UI surface its concrete
/// readiness reason and keeps the session catalog id aligned with its route.
#[test]
fn authoritative_all_unready_catalog_seats_a_present_model_id() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let mut entry = make_model_entry("unready-runtime-model");
    entry
        .config_validation_errors
        .push("fixture intentionally unready".to_string());
    let catalog = IndexMap::from([("unready-runtime-model".to_string(), entry)]);

    mgr.apply_refresh_result(&cfg, Some(catalog), None);

    let current = mgr.current_model_id();
    assert_eq!(current.0.as_ref(), "unready-runtime-model");
    assert!(
        mgr.models().contains_key(current.0.as_ref()),
        "an authoritative catalog must never publish an absent current id"
    );
    let listed = mgr
        .available()
        .get(&current)
        .cloned()
        .expect("the seated runtime model must remain visible to the client");
    let meta = listed
        .meta
        .expect("an unready model must expose readiness meta");
    assert_eq!(meta.get("ready"), Some(&serde_json::json!(false)));
    assert_eq!(
        meta.get("readinessReason"),
        Some(&serde_json::json!("fixture intentionally unready")),
        "the TUI must receive the concrete reason instead of rendering an unknown model"
    );
}

/// A non-empty catalog can still have no entry the current credential is
/// allowed to select (for example, an API-key session receiving only
/// OAuth-only entries). That state must not seat or sample an auth-hidden
/// model merely to keep the internal id inside the raw catalog.
#[test]
fn authoritative_catalog_without_auth_visible_model_fails_closed() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let mut oauth_only = make_model_entry("oauth-only");
    oauth_only.info.supported_in_api = false;

    mgr.apply_refresh_result(
        &cfg,
        Some(IndexMap::from([("oauth-only".to_string(), oauth_only)])),
        None,
    );

    assert!(mgr.available().is_empty());
    assert!(
        mgr.current_model_id().0.is_empty(),
        "an auth-hidden catalog entry must not become the current model"
    );
    let sampling = mgr.sampling_config();
    assert!(
        sampling.base_url.is_empty(),
        "an auth-hidden catalog must fail before any provider request"
    );
    assert_eq!(sampling.api_key, None);
}

#[test]
fn authoritative_catalog_without_user_selectable_model_fails_closed() {
    let mgr = test_manager();
    let cfg = config_from_toml("[models]\nallowed_models = [\"allowed-*\"]");
    let hidden_from_picker = make_model_entry("not-selectable");

    mgr.apply_refresh_result(
        &cfg,
        Some(IndexMap::from([(
            "not-selectable".to_string(),
            hidden_from_picker,
        )])),
        None,
    );

    assert!(mgr.available().is_empty());
    assert!(mgr.current_model_id().0.is_empty());
    let sampling = mgr.sampling_config();
    assert!(sampling.base_url.is_empty());
    assert_eq!(sampling.api_key, None);
}

#[test]
fn rebuild_updates_models_and_available() {
    let mgr = test_manager();
    assert!(mgr.models().is_empty());
    assert!(mgr.available().is_empty());

    let cfg = config::Config::default();
    let mut prefetched = IndexMap::new();
    prefetched.insert(
        "test-model".to_string(),
        ModelEntry {
            info: config::ModelInfo::fallback("test-model"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
            config_validation_errors: Vec::new(),
        },
    );

    let gen_before = mgr.catalog_generation();
    mgr.rebuild(&cfg, Some(prefetched));

    assert!(
        !mgr.models().is_empty(),
        "models should be populated after rebuild"
    );
    assert_ne!(
        mgr.catalog_generation(),
        gen_before,
        "rebuild must bump catalog_generation so auth memos cannot outlive the swap (#159 F1)"
    );
}

#[test]
fn current_reasoning_effort_round_trip() {
    let mgr = test_manager();
    assert_eq!(mgr.current_reasoning_effort(), None);

    mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));
    assert_eq!(mgr.current_reasoning_effort(), Some(ReasoningEffort::High));

    mgr.set_current_reasoning_effort(None);
    assert_eq!(mgr.current_reasoning_effort(), None);
}

#[test]
fn current_reasoning_effort_seeded_from_config() {
    let tmp = std::env::temp_dir().join("grok-test-models-manager-seed");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mut cfg = config::Config::default();
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::Xhigh);
    let mut entry = ModelEntry {
        info: config::ModelInfo::fallback("default"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "xhigh".into(),
        value: ReasoningEffort::Xhigh,
        label: "Extra high".into(),
        description: None,
        default: true,
    }];
    let mgr = ModelsManager::new(
        None,
        IndexMap::from([("default".to_string(), entry)]),
        acp::ModelId::new("default"),
        auth_manager,
        cfg,
    );
    assert_eq!(mgr.current_reasoning_effort(), Some(ReasoningEffort::Xhigh),);
}

#[test]
fn current_reasoning_effort_rejects_persisted_value_outside_model_menu() {
    let tmp = std::env::temp_dir().join("grok-test-models-manager-invalid-seed");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mut cfg = config::Config::default();
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::Xhigh);
    let mut entry = ModelEntry {
        info: config::ModelInfo::fallback("default"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "low".into(),
        value: ReasoningEffort::Low,
        label: "Low".into(),
        description: None,
        default: true,
    }];
    let mgr = ModelsManager::new(
        None,
        IndexMap::from([("default".to_string(), entry)]),
        acp::ModelId::new("default"),
        auth_manager,
        cfg,
    );
    assert_eq!(
        mgr.current_reasoning_effort(),
        None,
        "a persisted tier removed from the current model's menu must not seed the session",
    );
}

fn reasoning_entry_with_menu(model: &str, effort: ReasoningEffort) -> (String, ModelEntry) {
    let mut entry = ModelEntry {
        info: config::ModelInfo::fallback(model),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: effort.to_string(),
        value: effort,
        label: effort.to_string(),
        description: None,
        default: true,
    }];
    entry.info.reasoning_effort = Some(effort);
    (model.to_owned(), entry)
}

#[test]
fn catalog_refresh_clears_current_effort_removed_from_model_menu() {
    let mgr = test_manager();
    mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));
    mgr.apply_catalog_for_test(IndexMap::from([reasoning_entry_with_menu(
        "default",
        ReasoningEffort::Low,
    )]));
    assert_eq!(mgr.current_reasoning_effort(), None);
}

#[test]
fn stale_effort_revalidation_does_not_clear_newer_selection() {
    let mgr = test_manager();
    let validated_model = mgr.current_model_id();
    mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));

    // Deterministic interleaving: validation captured A/high, then a switch
    // committed B/high before the invalid result attempted its conditional
    // clear. Comparing only the effort would incorrectly clear B's selection.
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));
    mgr.clear_reasoning_effort_if_selection_unchanged(&validated_model, ReasoningEffort::High);

    assert_eq!(
        mgr.current_reasoning_effort(),
        Some(ReasoningEffort::High),
        "a refresh validating A/high must not clobber a newer B/high selection"
    );
}

#[test]
fn config_refresh_clears_current_effort_removed_from_model_menu() {
    let tmp = std::env::temp_dir().join("grok-test-models-manager-config-effort-refresh");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mgr = ModelsManager::new(
        None,
        IndexMap::from([reasoning_entry_with_menu("default", ReasoningEffort::High)]),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    );
    mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));

    let mut new_config = config::Config::default();
    new_config.config_models.insert(
        "default".to_owned(),
        config::ConfigModelOverride {
            supports_reasoning_effort: Some(true),
            reasoning_effort: Some(ReasoningEffort::Low),
            reasoning_efforts: vec![ReasoningEffortOption {
                id: "low".into(),
                value: ReasoningEffort::Low,
                label: "Low".into(),
                description: None,
                default: true,
            }],
            ..Default::default()
        },
    );
    mgr.apply_config(new_config);
    assert_eq!(mgr.current_reasoning_effort(), None);
}

#[test]
fn unconditional_default_reselection_revalidates_current_effort() {
    let tmp = std::env::temp_dir().join("grok-test-models-manager-default-effort-reselect");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mut cfg = config::Config::default();
    cfg.models.default = Some("model-b".to_owned());
    for (model, effort) in [
        ("model-a", ReasoningEffort::High),
        ("model-b", ReasoningEffort::Low),
    ] {
        cfg.config_models.insert(
            model.to_owned(),
            config::ConfigModelOverride {
                model: Some(model.to_owned()),
                supports_reasoning_effort: Some(true),
                reasoning_effort: Some(effort),
                reasoning_efforts: vec![ReasoningEffortOption {
                    id: effort.to_string(),
                    value: effort,
                    label: effort.to_string(),
                    description: None,
                    default: true,
                }],
                ..Default::default()
            },
        );
    }
    let initial_models = resolve_model_catalog(&cfg, None);
    let mgr = ModelsManager::new(
        None,
        initial_models,
        acp::ModelId::new("model-a"),
        auth_manager,
        cfg.clone(),
    );
    mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));

    mgr.apply_config_reselecting_default(cfg);

    assert_eq!(mgr.current_model_id(), acp::ModelId::new("model-b"));
    assert_eq!(mgr.current_reasoning_effort(), None);
}

#[test]
fn session_model_effort_validation_uses_requested_model_menu() {
    let tmp = std::env::temp_dir().join("grok-test-models-manager-session-effort");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let models = IndexMap::from([
        reasoning_entry_with_menu("model-a", ReasoningEffort::Xhigh),
        reasoning_entry_with_menu("model-b", ReasoningEffort::Low),
    ]);
    let mgr = ModelsManager::new(
        None,
        models,
        acp::ModelId::new("model-a"),
        auth_manager,
        config::Config::default(),
    );

    assert!(mgr.model_offers_reasoning_effort("model-a", ReasoningEffort::Xhigh));
    assert!(
        !mgr.model_offers_reasoning_effort("model-b", ReasoningEffort::Xhigh),
        "a custom session model must not inherit an effort offered only by the manager's current model"
    );
}

#[test]
fn menu_less_reasoning_model_accepts_legacy_minimal_effort() {
    let tmp = std::env::temp_dir().join("grok-test-models-manager-minimal-effort");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mut entry = ModelEntry {
        info: config::ModelInfo::fallback("legacy-reasoning"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_efforts.clear();
    let mut cfg = config::Config::default();
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::Minimal);
    let mgr = ModelsManager::new(
        None,
        IndexMap::from([("legacy-reasoning".to_string(), entry)]),
        acp::ModelId::new("legacy-reasoning"),
        auth_manager,
        cfg,
    );

    assert!(mgr.model_offers_reasoning_effort("legacy-reasoning", ReasoningEffort::Minimal));
    assert_eq!(
        mgr.current_reasoning_effort(),
        Some(ReasoningEffort::Minimal)
    );
}

#[test]
fn rebuild_revalidates_current_reasoning_effort() {
    let mut cfg = config::Config::default();
    cfg.config_models.insert(
        "default".to_string(),
        config::ConfigModelOverride {
            model: Some("default".to_string()),
            supports_reasoning_effort: Some(true),
            reasoning_efforts: vec![ReasoningEffortOption {
                id: "low".to_string(),
                value: ReasoningEffort::Low,
                label: "Low".to_string(),
                description: None,
                default: true,
            }],
            ..Default::default()
        },
    );
    let mgr = test_manager();
    mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));

    mgr.rebuild(&cfg, None);

    assert_eq!(mgr.current_reasoning_effort(), None);
}

#[test]
fn default_reasoning_effort_only_stamps_supporting_model() {
    use indexmap::IndexMap;

    let mut cfg = config::Config::default();
    cfg.models.default = Some("reasoning-model".to_string());
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);

    let mut prefetched = IndexMap::new();
    let mut reasoning_entry = ModelEntry {
        info: config::ModelInfo::fallback("reasoning-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    reasoning_entry.info.supports_reasoning_effort = true;
    prefetched.insert("reasoning-model".to_string(), reasoning_entry);

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    assert_eq!(
        catalog["reasoning-model"].info.reasoning_effort,
        Some(ReasoningEffort::High),
        "reasoning-supporting default model should be stamped",
    );

    let mut cfg = config::Config::default();
    cfg.models.default = Some("plain-model".to_string());
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);

    let mut prefetched = IndexMap::new();
    let plain_entry = ModelEntry {
        info: config::ModelInfo::fallback("plain-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    prefetched.insert("plain-model".to_string(), plain_entry);

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    assert_eq!(
        catalog["plain-model"].info.reasoning_effort, None,
        "non-reasoning default model must NOT be stamped with persisted effort",
    );

    let mut cfg = config::Config::default();
    cfg.models.default = Some("limited-model".to_string());
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);
    let mut limited_entry = ModelEntry {
        info: config::ModelInfo::fallback("limited-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    limited_entry.info.supports_reasoning_effort = true;
    limited_entry.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "low".into(),
        value: ReasoningEffort::Low,
        label: "Low".into(),
        description: None,
        default: true,
    }];
    limited_entry.info.reasoning_effort = Some(ReasoningEffort::Low);
    let catalog = resolve_model_catalog(
        &cfg,
        Some(IndexMap::from([(
            "limited-model".to_string(),
            limited_entry,
        )])),
    );
    assert_eq!(
        catalog["limited-model"].info.reasoning_effort,
        Some(ReasoningEffort::Low),
        "a persisted tier outside the advertised menu must not replace the catalog default",
    );
}

#[test]
fn reasoning_effort_override_skips_models_that_do_not_offer_level() {
    use indexmap::IndexMap;
    use xai_grok_sampling_types::ReasoningEffortOption;

    let cfg = config::Config {
        reasoning_effort_override: Some(ReasoningEffort::None),
        ..Default::default()
    };

    let mut prefetched = IndexMap::new();
    let mut no_none = ModelEntry {
        info: config::ModelInfo::fallback("grok-4.5"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    no_none.info.supports_reasoning_effort = true;
    no_none.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "high".into(),
        value: ReasoningEffort::High,
        label: "High".into(),
        description: None,
        default: true,
    }];
    no_none.info.reasoning_effort = Some(ReasoningEffort::High);
    prefetched.insert("grok-4.5".to_string(), no_none);

    let mut with_none = ModelEntry {
        info: config::ModelInfo::fallback("legacy-none"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    with_none.info.supports_reasoning_effort = true;
    with_none.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "none".into(),
        value: ReasoningEffort::None,
        label: "None".into(),
        description: None,
        default: true,
    }];
    prefetched.insert("legacy-none".to_string(), with_none);

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    assert_eq!(
        catalog["grok-4.5"].info.reasoning_effort,
        Some(ReasoningEffort::High),
        "--effort none must not stamp onto models that do not offer none"
    );
    assert_eq!(
        catalog["legacy-none"].info.reasoning_effort,
        Some(ReasoningEffort::None),
        "models that list none should still accept the override"
    );
}

#[test]
fn config_menu_only_model_derives_support_and_default() {
    let mut cfg = config::Config::default();
    cfg.config_models.insert(
        "menu-only".to_string(),
        config::ConfigModelOverride {
            reasoning_efforts: vec![
                ReasoningEffortOption {
                    id: "balanced".to_string(),
                    value: ReasoningEffort::Medium,
                    label: "Balanced".to_string(),
                    description: None,
                    default: false,
                },
                ReasoningEffortOption {
                    id: "deep".to_string(),
                    value: ReasoningEffort::Xhigh,
                    label: "Deep".to_string(),
                    description: None,
                    default: true,
                },
            ],
            ..Default::default()
        },
    );
    cfg.config_models
        .insert("plain".to_string(), config::ConfigModelOverride::default());

    let catalog = resolve_model_catalog(&cfg, None);
    let info = &catalog["menu-only"].info;
    assert!(
        info.supports_reasoning_effort,
        "menu-only model must derive support"
    );
    assert_eq!(
        info.reasoning_effort,
        Some(ReasoningEffort::Xhigh),
        "derived default = marked-default option value"
    );
    assert!(!catalog["plain"].info.supports_reasoning_effort);
    assert_eq!(catalog["plain"].info.reasoning_effort, None);

    let tmp = std::env::temp_dir().join("grok-test-models-manager-menu-only");
    let auth_manager = Arc::new(AuthManager::new(&tmp, GrokComConfig::default()));
    let mgr = ModelsManager::new(
        None,
        catalog,
        acp::ModelId::new("menu-only"),
        auth_manager,
        cfg,
    );
    assert!(mgr.model_supports_reasoning_effort("menu-only"));
    assert_eq!(
        mgr.model_default_reasoning_effort("menu-only"),
        Some(ReasoningEffort::Xhigh)
    );
    assert_eq!(mgr.model_reasoning_efforts("menu-only").len(), 2);
    assert!(!mgr.model_supports_reasoning_effort("plain"));
    assert_eq!(mgr.model_default_reasoning_effort("plain"), None);
}

#[test]
fn cli_reasoning_effort_override_only_stamps_supporting_models() {
    use indexmap::IndexMap;

    let cfg = config::Config {
        reasoning_effort_override: Some(ReasoningEffort::High),
        ..config::Config::default()
    };

    let mut prefetched = IndexMap::new();
    let mut reasoning_entry = ModelEntry {
        info: config::ModelInfo::fallback("reasoning-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    reasoning_entry.info.supports_reasoning_effort = true;
    prefetched.insert("reasoning-model".to_string(), reasoning_entry);

    let plain_entry = ModelEntry {
        info: config::ModelInfo::fallback("plain-model"),
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    prefetched.insert("plain-model".to_string(), plain_entry);

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    assert_eq!(
        catalog["reasoning-model"].info.reasoning_effort,
        Some(ReasoningEffort::High),
        "reasoning-supporting model should be stamped",
    );
    assert_eq!(
        catalog["plain-model"].info.reasoning_effort, None,
        "non-reasoning model must NOT be stamped",
    );
}

#[test]
fn apply_refresh_result_only_updates_etag_on_success() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    mgr.inner.catalog.write().etag = Some("\"old\"".to_string());

    assert!(
        !mgr.apply_refresh_result(&cfg, None, Some("\"new\"".to_string())),
        "failed refresh should report no update"
    );
    assert_eq!(
        mgr.inner.catalog.read().etag.as_deref(),
        Some("\"old\""),
        "etag should remain unchanged when refresh fails"
    );
    assert!(
        mgr.prefetched().is_none(),
        "prefetched models should stay unchanged"
    );
}

/// The production constructor rather than a hand-rolled copy of it.
///
/// The copy left `base_url` empty, which no catalog entry ever is in
/// production -- `ModelEntry::fallback` fills it from the endpoints. That was
/// invisible until readiness began consulting the endpoint (#110), at which
/// point every fixture here became a credential-less non-first-party model
/// and stopped being selectable.
fn make_model_entry(model_id: &str) -> ModelEntry {
    ModelEntry::fallback(model_id, &config::EndpointsConfig::default())
}

fn make_prefetched(ids: &[&str]) -> IndexMap<String, ModelEntry> {
    ids.iter()
        .map(|id| (id.to_string(), make_model_entry(id)))
        .collect()
}

// ── startup background refresh ─────────────────────────────────────

#[test]
fn spawn_background_refresh_is_noop_when_real_catalog_present() {
    let mgr = test_manager();
    mgr.inner.catalog.write().has_fetched_real_catalog = true;
    mgr.spawn_background_refresh(); // must not panic (no tokio::spawn taken)
    assert!(mgr.has_fetched_real_catalog());
}

#[test]
fn from_config_without_prefetch_produces_usable_catalog() {
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let cfg = config::Config::default();

    let mgr = ModelsManager::from_config(&cfg, None, auth_manager).unwrap();

    let cat = mgr.inner.catalog.read();
    let catalog = &cat.models;
    assert!(
        !catalog.is_empty(),
        "zero-network boot must produce at least one model in the internal catalog"
    );
    let default = mgr.current_model_id();
    assert!(
        catalog.contains_key(default.0.as_ref()),
        "default model {:?} not in internal catalog: {:?}",
        default,
        catalog.keys().collect::<Vec<_>>()
    );
    drop(cat);
    assert!(
        !mgr.has_fetched_real_catalog(),
        "cold-cache boot must not claim a real catalog"
    );
}

// ── auth-change refresh: has_fetched_real_catalog flag ─────────────

#[test]
fn first_apply_refresh_reselects_default_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    assert!(!mgr.has_fetched_real_catalog());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    assert!(mgr.has_fetched_real_catalog());
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-3");
}

#[test]
fn subsequent_apply_refresh_preserves_user_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    mgr.inner.catalog.write().prefetched = None;
    mgr.inner.catalog.write().etag = None;

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "user's model selection must survive auth-change refresh"
    );
}

#[test]
fn subsequent_refresh_reselects_when_model_removed() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let prefetched = make_prefetched(&["grok-3", "grok-4.5"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-3",
        "should fall back to config default when current is removed"
    );
}

#[test]
fn failed_refresh_does_not_set_has_fetched_real_catalog() {
    let mgr = test_manager();
    let cfg = config::Config::default();

    mgr.apply_refresh_result(&cfg, None, None);

    assert!(
        !mgr.has_fetched_real_catalog(),
        "failed refresh must not flip has_fetched_real_catalog"
    );
}

// ── apply_config: honor changed preferred model from config ────────

#[test]
fn apply_config_honors_new_preferred_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let mut stale_cfg = config::Config::default();
    stale_cfg.models.default = None;
    *mgr.inner.cfg.write() = stale_cfg;

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("grok-3".to_string());
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-3",
        "apply_config must honor updated preferred model from config"
    );
}

#[test]
fn apply_config_preserves_current_when_preferred_unchanged() {
    let mgr = test_manager();
    let cfg = config::Config::default();

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let new_cfg = config::Config::default();
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "apply_config must not reset model when preferred hasn't changed"
    );
}

#[test]
fn apply_config_falls_back_when_preferred_not_in_catalog() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("grok-nonexistent".to_string());
    mgr.apply_config(new_cfg);

    let current = mgr.current_model_id();
    let first_available = mgr.available().keys().next().unwrap().clone();
    assert_eq!(
        current.0.as_ref(),
        first_available.0.as_ref(),
        "should fall back to first visible model when preferred not in catalog"
    );
}

#[test]
fn apply_config_both_none_preferred_preserves_current() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    let new_cfg = config::Config::default();
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "both-None preferred must preserve user's runtime model"
    );
}

#[test]
fn apply_config_old_some_new_none_preserves_current() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-3");

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let new_cfg = config::Config::default();
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "old=Some new=None must not reset model (is_some guard)"
    );
}

// ── end-to-end: auth refresh + config reload compose correctly ───

#[test]
fn auth_refresh_then_config_reload_preserves_user_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    mgr.inner.catalog.write().prefetched = None;
    mgr.inner.catalog.write().etag = None;

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("grok-4".to_string());
    mgr.apply_config(new_cfg);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");
}

// ── disk-cache hot-reload (external models_cache.json writes) ────

fn test_cache_manager(dir: &std::path::Path) -> ModelsCacheManager {
    ModelsCacheManager {
        path: dir.join(MODELS_CACHE_FILE),
        ttl: CACHE_TTL,
    }
}

#[test]
fn reload_from_disk_cache_applies_external_catalog() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());

    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    cache.persist(
        &make_prefetched(&["grok-4.5", "grok-4.3"]),
        Some("etag-ext"),
        auth_method,
        &mgr.cache_origin(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(mgr.has_fetched_real_catalog());
    assert!(mgr.models().contains_key("grok-4.5"));
    assert!(mgr.models().contains_key("grok-4.3"));
    assert_eq!(mgr.inner.catalog.read().etag.as_deref(), Some("etag-ext"));
}

#[test]
fn reload_from_disk_cache_recomputes_allowlist_excludes_all() {
    let mgr = test_manager();
    let cfg = config_from_toml("[models]\nallowed_models = [\"keep-*\"]");

    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["other-1"])), None);
    assert!(
        mgr.allowlist_excludes_all(),
        "setup: allowlist should exclude the entire catalog"
    );
    *mgr.inner.cfg.write() = cfg.clone();

    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    cache.persist(
        &make_prefetched(&["keep-1"]),
        Some("etag-keep"),
        auth_method,
        &mgr.cache_origin(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(mgr.models().contains_key("keep-1"));
    assert!(
        !mgr.allowlist_excludes_all(),
        "corrective external cache write must unlatch the prompt block"
    );
}

#[test]
fn reload_from_disk_cache_resolves_default_on_first_catalog() {
    let mgr = test_manager();
    assert!(!mgr.has_fetched_real_catalog());
    let cfg = config_from_toml("[models]\ndefault = \"keep-1\"");
    *mgr.inner.cfg.write() = cfg.clone();

    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    cache.persist(
        &make_prefetched(&["keep-1", "other-1"]),
        Some("etag-first"),
        auth_method,
        &mgr.cache_origin(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(mgr.has_fetched_real_catalog());
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "keep-1",
        "first real catalog must resolve the configured default"
    );
}

#[test]
fn reload_from_disk_cache_skips_identical_catalog_and_adopts_etag() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched.clone()), Some("etag-a".into()));
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    cache.persist(
        &prefetched,
        Some("etag-b"),
        auth_method,
        &mgr.cache_origin(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "identical catalog must not disturb the user's model"
    );
    assert_eq!(
        mgr.inner.catalog.read().etag.as_deref(),
        Some("etag-b"),
        "etag should be adopted so refresh_if_new_etag stays accurate"
    );
}

#[test]
fn reload_from_disk_cache_ignores_stale_cache() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    let stale = ModelsCache {
        fetched_at: Utc::now() - ChronoDuration::seconds(3600),
        grok_version: Some(xai_grok_version::VERSION.to_string()),
        auth_method: Some(auth_method),
        origin: Some(mgr.cache_origin()),
        etag: Some("etag-stale".into()),
        models: make_prefetched(&["grok-stale"]),
    };
    cache.atomic_write(&stale);

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-stale"));
    assert!(mgr.inner.catalog.read().etag.is_none());
}

#[test]
fn reload_from_disk_cache_ignores_auth_method_mismatch() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let current = mgr.inner.fetch_auth.read().cache_auth_method();
    let other = if current == CacheAuthMethod::Session {
        CacheAuthMethod::ApiKey
    } else {
        CacheAuthMethod::Session
    };
    cache.persist(
        &make_prefetched(&["grok-other-auth"]),
        Some("etag-x"),
        other,
        &mgr.cache_origin(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-other-auth"));
}

#[test]
fn reload_from_disk_cache_ignores_origin_mismatch() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    cache.persist(
        &make_prefetched(&["grok-other-origin"]),
        Some("etag-y"),
        auth_method,
        "http://127.0.0.1:49953/v1/models",
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-other-origin"));
    assert!(mgr.inner.catalog.read().etag.is_none());
}

#[test]
fn reload_from_disk_cache_ignores_legacy_cache_without_origin() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
    let legacy = ModelsCache {
        fetched_at: Utc::now(),
        grok_version: Some(xai_grok_version::VERSION.to_string()),
        auth_method: Some(auth_method),
        origin: None,
        etag: Some("etag-legacy".into()),
        models: make_prefetched(&["grok-legacy"]),
    };
    cache.atomic_write(&legacy);

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-legacy"));
}

// ── clear() resets has_fetched_real_catalog ──────────────────────

#[test]
fn clear_resets_has_fetched_real_catalog() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    assert!(mgr.has_fetched_real_catalog());
    mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));

    mgr.clear();
    assert!(!mgr.has_fetched_real_catalog());
    assert_eq!(mgr.current_reasoning_effort(), None);

    let prefetched = make_prefetched(&["grok-4.5", "grok-4.3"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    let first_available = mgr.available().keys().next().unwrap().clone();
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        first_available.0.as_ref()
    );
}

#[test]
fn is_campaign_only_flip_detects_campaign_driven_changes() {
    let camp: std::collections::HashSet<String> = ["beta".into()].into_iter().collect();
    assert!(is_campaign_only_flip(
        &Some("alpha".into()),
        &Some("beta".into()),
        &camp
    ));
    assert!(is_campaign_only_flip(
        &Some("beta".into()),
        &Some("alpha".into()),
        &camp
    ));
    assert!(!is_campaign_only_flip(
        &Some("alpha".into()),
        &Some("gamma".into()),
        &camp
    ));
    assert!(!is_campaign_only_flip(
        &Some("beta".into()),
        &Some("beta".into()),
        &camp
    ));
    assert!(!is_campaign_only_flip(&Some("beta".into()), &None, &camp));
    assert!(!is_campaign_only_flip(
        &Some("alpha".into()),
        &Some("beta".into()),
        &std::collections::HashSet::new()
    ));
}

#[test]
fn campaign_only_flip_does_not_reselect_live_session() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("alpha".to_string());
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["alpha", "beta"])), None);
    *mgr.inner.cfg.write() = cfg.clone(); // old_preferred = "alpha"
    assert_eq!(mgr.current_model_id().0.as_ref(), "alpha");

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("beta".to_string());
    new_cfg.models.default_is_campaign_driven = true; // campaign overriding
    mgr.apply_config(new_cfg);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "alpha",
        "campaign-only flip must not yank a still-selectable live session"
    );

    let mgr2 = test_manager();
    let mut cfg2 = config::Config::default();
    cfg2.models.default = Some("alpha".to_string());
    mgr2.apply_refresh_result(&cfg2, Some(make_prefetched(&["alpha", "beta"])), None);
    *mgr2.inner.cfg.write() = cfg2.clone();
    let mut new_cfg2 = config::Config::default();
    new_cfg2.models.default = Some("beta".to_string());
    mgr2.apply_config(new_cfg2);
    assert_eq!(
        mgr2.current_model_id().0.as_ref(),
        "beta",
        "a non-campaign preferred change must reselect"
    );
}

#[test]
fn unavailable_campaign_default_falls_back_to_config_default() {
    let catalog = make_prefetched(&["real-model", "other-model"]);

    let mut cfg = config::Config::default();
    cfg.models.default = Some("missing-model".to_string());
    cfg.models.default_is_campaign_driven = true;
    cfg.models.pre_campaign_default = Some("real-model".to_string());
    let (key, _, _, _) = resolve_default_model(&cfg, &catalog, true);
    assert_eq!(
        key, "real-model",
        "must fall back to the pre-campaign default"
    );

    let mut cfg2 = config::Config::default();
    cfg2.models.default = Some("missing-model".to_string());
    cfg2.models.default_is_campaign_driven = true;
    cfg2.models.pre_campaign_default = Some("also-missing".to_string());
    let (key2, _, _, _) = resolve_default_model(&cfg2, &catalog, true);
    assert_eq!(&key2, catalog.keys().next().unwrap());

    let mut cfg3 = config::Config::default();
    cfg3.models.default = Some("missing-model".to_string());
    cfg3.models.pre_campaign_default = Some("real-model".to_string());
    let (key3, _, _, _) = resolve_default_model(&cfg3, &catalog, true);
    assert_eq!(
        &key3,
        catalog.keys().next().unwrap(),
        "non-campaign catalog miss must not recover via campaign state"
    );

    let mut cfg4 = config::Config {
        default_model_override: Some("missing-cli-model".to_string()),
        ..Default::default()
    };
    cfg4.models.default = Some("campaign-model".to_string());
    cfg4.models.default_is_campaign_driven = true;
    cfg4.models.pre_campaign_default = Some("real-model".to_string());
    let (key4, _, _, _) = resolve_default_model(&cfg4, &catalog, true);
    assert_eq!(
        &key4,
        catalog.keys().next().unwrap(),
        "a CLI pref miss must not detour through pre_campaign_default"
    );
}

/// A campaign-driven default that is *present* in the catalog but unready must
/// still recover the pre-campaign default.
///
/// #131 keeps an explicit unready preference selected instead of silently
/// swapping it, which is right for a choice the user made. A campaign default
/// lands in the same `ConfigSource::Config` slot without the user choosing
/// anything, so counting it as explicit turns one bad remote push into a
/// cohort that cannot complete a turn until somebody hand-edits config --
/// which is the exact failure `pre_campaign_default` exists to undo.
///
/// Every case in `unavailable_campaign_default_falls_back_to_config_default`
/// uses a preference *absent* from the catalog. That reaches the recovery
/// through the catalog-miss branch and so never exercised this one.
#[test]
fn an_unready_campaign_default_recovers_the_pre_campaign_default() {
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    let mut pushed = make_model_entry("pushed-model");
    pushed
        .config_validation_errors
        .push("invalid auth_scheme `not-a-scheme`".into());
    catalog.insert("pushed-model".to_string(), pushed);
    catalog.insert("real-model".to_string(), make_model_entry("real-model"));

    let mut cfg = config::Config::default();
    cfg.models.default = Some("pushed-model".to_string());
    cfg.models.default_is_campaign_driven = true;
    cfg.models.pre_campaign_default = Some("real-model".to_string());

    let (key, _, _, reason) = resolve_default_model(&cfg, &catalog, true);
    assert_eq!(
        key, "real-model",
        "a pushed default that cannot authenticate must not strand the cohort"
    );
    assert!(
        reason.is_none(),
        "recovering is not a failure to report to the user: {reason:?}"
    );

    // The same broken entry, chosen by the user instead of pushed, is still
    // kept selected and reported. That is #131, and this fix must not undo it.
    let mut chosen = config::Config::default();
    chosen.models.default = Some("pushed-model".to_string());
    chosen.models.pre_campaign_default = Some("real-model".to_string());
    let (key, _, _, reason) = resolve_default_model(&chosen, &catalog, true);
    assert_eq!(
        key, "pushed-model",
        "an explicit user choice that is broken stays selected"
    );
    assert!(reason.is_some(), "and the user is told why");
}

// ── ModelFetchAuth::resolve priority tests ──────────────────────

use serial_test::serial;
use xai_grok_test_support::EnvGuard;

#[test]
#[serial]
fn resolve_custom_endpoint_always_wins() {
    let _key = EnvGuard::set("XAI_API_KEY", "test-key");
    let endpoints = config::EndpointsConfig {
        models_base_url: Some("https://custom.example.com".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, true),
        ModelFetchAuth::CustomEndpoint,
    );
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::CustomEndpoint,
    );
}

#[test]
#[serial]
fn from_config_surfaces_missing_custom_catalog_key() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mut cfg = config::Config::default();
    cfg.endpoints.models_list_url = Some("https://custom.example.com/v1/models".to_owned());

    let result = ModelsManager::from_config_with_remote_fetch(&cfg, None, auth_manager, true);
    let Err(message) = result else {
        panic!("missing custom catalog key must be a configuration error");
    };
    assert_eq!(
        message,
        "Custom model catalog requires XAI_API_KEY (or GROK_CODE_XAI_API_KEY)."
    );
}

#[test]
#[serial]
fn from_config_accepts_explicit_custom_catalog_key() {
    let _key = EnvGuard::set("XAI_API_KEY", "catalog-key");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mut cfg = config::Config::default();
    cfg.endpoints.models_base_url = Some("https://custom.example.com/v1".to_owned());

    assert!(ModelsManager::from_config_with_remote_fetch(&cfg, None, auth_manager, true).is_ok());
}

#[test]
#[serial]
fn from_config_allows_offline_custom_catalog_without_key() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mut cfg = config::Config::default();
    cfg.endpoints.models_base_url = Some("https://custom.example.com/v1".to_owned());

    assert!(ModelsManager::from_config_with_remote_fetch(&cfg, None, auth_manager, false).is_ok());
}

#[test]
#[serial]
fn from_config_surfaces_missing_key_for_untrusted_proxy_catalog() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mut cfg = config::Config::default();
    cfg.endpoints.cli_chat_proxy_base_url = Some("https://proxy.example.com/v1".to_owned());

    let result = ModelsManager::from_config_with_remote_fetch(&cfg, None, auth_manager, true);
    let Err(message) = result else {
        panic!("untrusted proxy catalog without an explicit key must fail closed");
    };
    assert_eq!(
        message,
        "Custom model catalog requires XAI_API_KEY (or GROK_CODE_XAI_API_KEY)."
    );
}

#[test]
#[serial]
fn resolve_cached_session_wins_over_api_key() {
    let _key = EnvGuard::set("XAI_API_KEY", "test-key");
    let endpoints = config::EndpointsConfig::default();
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, true),
        ModelFetchAuth::Session,
        "cached session should take priority over API key",
    );
}

#[test]
#[serial]
fn resolve_api_key_used_when_no_session() {
    let _key = EnvGuard::set("XAI_API_KEY", "test-key");
    let endpoints = config::EndpointsConfig::default();
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::ApiKey,
        "API key should be used when no cached session exists",
    );
}

#[test]
#[serial]
fn resolve_falls_back_to_session_when_nothing_set() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig::default();
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::Session,
        "should fall back to Session when nothing else is configured",
    );
}

#[test]
#[serial]
fn resolve_deployment_key_when_no_session_or_api_key() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::Deployment,
    );
}

#[test]
#[serial]
fn resolve_deployment_key_outranks_ambient_api_key() {
    let _key = EnvGuard::set("XAI_API_KEY", "stray-env-key");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::Deployment,
        "managed deployment_key should outrank an ambient XAI_API_KEY",
    );
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, true),
        ModelFetchAuth::Session,
        "an active session should still win over a managed deployment",
    );
}

// ── remote_fetch gate: resolve_prefetch_env_from_parts ───────────

#[test]
#[serial]
fn prefetch_env_none_when_remote_fetch_disabled_despite_credentials() {
    let _key = EnvGuard::set("XAI_API_KEY", "stray-env-key");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        models_base_url: Some("https://custom.example.com".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert!(
        resolve_prefetch_env_from_parts(Some(GrokAuth::test_default()), endpoints.clone(), false,)
            .is_none(),
        "session auth must not re-arm the prefetch when remote_fetch is off",
    );
    assert!(
        resolve_prefetch_env_from_parts(None, endpoints, false).is_none(),
        "API key / deployment key / custom endpoint must not re-arm it either",
    );
}

#[test]
#[serial]
fn prefetch_env_resolves_when_remote_fetch_enabled() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert!(resolve_prefetch_env_from_parts(None, endpoints, true).is_some());
    assert!(
        resolve_prefetch_env_from_parts(None, config::EndpointsConfig::default(), true).is_none(),
        "no credentials and no custom endpoint must stay a no-prefetch launch",
    );
}

#[tokio::test]
async fn fetch_and_apply_degrades_offline_when_remote_fetch_disabled() {
    let mgr = test_manager();
    mgr.insert_test_entry(
        "static-one",
        ModelEntry {
            info: config::ModelInfo::fallback("static-one"),
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
            config_validation_errors: Vec::new(),
        },
    );

    mgr.fetch_and_apply_inner(false).await;

    assert!(
        !mgr.has_fetched_real_catalog(),
        "no catalog fetch may be recorded when remote_fetch is disabled",
    );
    assert!(
        mgr.models().contains_key("static-one"),
        "the static catalog must keep resolving",
    );
}

// ── supported_in_api tests ──────────────────────────────────────

#[test]
fn default_model_skips_oauth_only_for_api_key_users() {
    let cfg = config::Config::default();
    let mut catalog = IndexMap::new();

    let mut oauth_only = make_model_entry("oauth-only");
    oauth_only.info.supported_in_api = false;
    catalog.insert("oauth-only".to_string(), oauth_only);

    catalog.insert("public-model".to_string(), make_model_entry("public-model"));

    let (key, _, _, _) = resolve_default_model(&cfg, &catalog, false);
    assert_ne!(
        key, "oauth-only",
        "API-key default must not be an OAuth-only model"
    );
    assert_eq!(key, "public-model");

    let (key, _, _, _) = resolve_default_model(&cfg, &catalog, true);
    assert!(
        key == "oauth-only" || key == "public-model",
        "OAuth user should be able to use either model as default"
    );
}

#[test]
fn visible_for_auth_logic() {
    let mut info = config::ModelInfo::fallback("test");

    assert!(info.visible_for_auth(true));
    assert!(info.visible_for_auth(false));

    info.hidden = true;
    assert!(!info.visible_for_auth(true));
    assert!(!info.visible_for_auth(false));

    info.hidden = false;
    info.supported_in_api = false;
    assert!(info.visible_for_auth(true));
    assert!(!info.visible_for_auth(false));
}

// ── duplicate model slug re-keying (A/B experiment "auto" alias) ──

fn make_entry_config(model: &str, name: Option<&str>) -> config::ModelEntryConfig {
    make_entry_config_with_id(None, model, name)
}

fn make_entry_config_with_id(
    id: Option<&str>,
    model: &str,
    name: Option<&str>,
) -> config::ModelEntryConfig {
    config::ModelEntryConfig {
        id: id.map(|s| s.to_owned()),
        model: model.to_owned(),
        base_url: "https://test.api/v1".to_owned(),
        name: name.map(|n| n.to_owned()),
        description: None,
        max_completion_tokens: None,
        temperature: None,
        top_p: None,
        api_key: None,
        env_key: None,
        api_backend: Default::default(),
        context_window: std::num::NonZeroU64::new(200_000).unwrap(),
        auto_compact_threshold_percent: None,
        system_prompt_label: None,
        extra_headers: IndexMap::new(),
        api_base_url: None,
        use_concise: false,
        agent_type: config::default_agent_type(),
        inference_idle_timeout_secs: None,
        max_retries: None,
        hidden: false,
        supported_in_api: true,
        auth_scheme: None,
        reasoning_effort: None,
        supports_reasoning_effort: false,
        reasoning_efforts: Vec::new(),
        supports_backend_search: false,
        compactions_remaining: None,
        compaction_at_tokens: None,
        show_model_fingerprint: false,
        stream_tool_calls: None,
        laziness_detector: config::LazinessDetectorPerModelConfig::default(),
    }
}

#[test]
fn build_prefetched_map_distinct_ids_same_slug() {
    let entries = vec![
        make_entry_config_with_id(Some("auto"), "grok-build", Some("Auto")),
        make_entry_config_with_id(Some("grok-build"), "grok-build", Some("Grok Build")),
        make_entry_config_with_id(
            Some("experimental-fast"),
            "experimental-fast",
            Some("Grok Fast"),
        ),
    ];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 3, "all three entries should survive");
    assert!(map.contains_key("auto"));
    assert!(map.contains_key("grok-build"));
    assert!(map.contains_key("experimental-fast"));
    assert_eq!(
        map["auto"].info.model, "grok-build",
        "auto entry should still route to grok-build"
    );
    assert_eq!(map["grok-build"].info.model, "grok-build");
}

#[test]
fn build_prefetched_map_no_id_falls_back_to_slug() {
    let entries = vec![
        make_entry_config("model-a", Some("Model A")),
        make_entry_config("model-b", Some("Model B")),
    ];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 2);
    assert!(map.contains_key("model-a"));
    assert!(map.contains_key("model-b"));
}

#[test]
fn build_prefetched_map_duplicate_id_overwrites() {
    let entries = vec![
        make_entry_config_with_id(Some("grok-build"), "grok-build", Some("First")),
        make_entry_config_with_id(Some("grok-build"), "grok-build", Some("Second")),
    ];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 1, "duplicate id: second overwrites first");
    assert_eq!(map["grok-build"].info.name.as_deref(), Some("Second"));
}

#[test]
fn resolve_default_model_prefers_id_over_model_slug() {
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert(
        "auto-grok-build".to_string(),
        make_model_entry("grok-build"),
    );
    catalog.insert("grok-build".to_string(), make_model_entry("grok-build"));

    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-build".to_string());

    let (key, _, _, _) = resolve_default_model(&cfg, &catalog, true);
    assert_eq!(key, "grok-build", "must match id, not first slug hit");
}

/// #131: an explicit configured default that is catalogued-but-unusable must
/// be kept (no silent substitute), with the readiness reason returned.
#[test]
fn resolve_default_model_keeps_explicit_unusable_preference() {
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    let mut custom = make_model_entry("custom");
    custom
        .config_validation_errors
        .push("invalid auth_scheme `not-a-scheme`".into());
    catalog.insert("custom".to_string(), custom);

    let mut cfg = config::Config::default();
    cfg.models.default = Some("custom".to_string());

    let (key, entry, _, reason) = resolve_default_model(&cfg, &catalog, true);
    assert_eq!(key, "custom", "must keep the explicit unusable preference");
    assert!(
        !crate::agent::config::model_readiness(&entry).0,
        "kept entry must still be unusable"
    );
    let reason = reason.expect("must surface the readiness reason");
    assert!(
        reason.contains("invalid auth_scheme"),
        "unexpected reason: {reason}"
    );
}

/// Keeping an unusable explicit preference must not bypass the gates that say
/// whether the user may select it at all.
///
/// `allowed_models` / `hidden_models` / `supported_in_api` answer a different
/// question from "does it work", and an earlier version returned the unready
/// preference *before* consulting them. `validate_selectable` guards
/// `models.default` but not `GROK_DEFAULT_MODEL`, and `reselect_default_model`
/// never calls it, so this is the only gate on that path — without it the
/// session's current model can be one `available()` does not list, and
/// `allowed_models` stops being a gate.
#[test]
fn an_unusable_preference_the_user_may_not_select_is_not_seated() {
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();

    let mut hidden = make_model_entry("hidden-custom");
    hidden
        .config_validation_errors
        .push("invalid auth_scheme `not-a-scheme`".into());
    hidden.info.user_selectable = false;
    catalog.insert("hidden-custom".to_string(), hidden);

    let usable = make_model_entry("usable");
    catalog.insert("usable".to_string(), usable);

    let mut cfg = config::Config::default();
    cfg.models.default = Some("hidden-custom".to_string());

    let (key, _, _, _) = resolve_default_model(&cfg, &catalog, true);
    assert_ne!(
        key, "hidden-custom",
        "a model the user may not select must not be seated just because it was named"
    );
}

/// When no preference is set and every selectable entry is unready, fall back
/// to the bundled default sentinel rather than returning an unusable entry.
#[test]
fn resolve_default_model_falls_back_when_all_selectable_unready() {
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    let mut custom = make_model_entry("custom");
    custom
        .config_validation_errors
        .push("invalid auth_scheme `not-a-scheme`".into());
    catalog.insert("custom".to_string(), custom);

    let cfg = config::Config::default(); // no explicit preference

    let (key, entry, _, reason) = resolve_default_model(&cfg, &catalog, true);
    assert!(reason.is_none());
    assert_ne!(
        key, "custom",
        "must not pick an unready selectable model as the implicit default"
    );
    assert_eq!(key, crate::models::default_model());
    assert!(
        entry.config_validation_errors.is_empty(),
        "bundled sentinel must be validation-clean"
    );
    assert!(crate::agent::config::model_readiness(&entry).0);
}

#[test]
fn build_prefetched_map_none_id_falls_back_to_slug() {
    let entries = vec![make_entry_config_with_id(
        None,
        "grok-build",
        Some("Grok Build"),
    )];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 1);
    assert!(map.contains_key("grok-build"));
}

// ── persisted model id → catalog key (session resume) ─────────────

#[test]
fn resolve_catalog_key_maps_routing_slug_to_config_key() {
    let mut models = IndexMap::new();
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-4.5"),
    );
    models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

    let persisted = acp::ModelId::new("grok-4.5");
    let key = resolve_catalog_key(&models, &persisted).expect("slug must resolve");
    assert_eq!(key.0.as_ref(), "enterprise-grok-build");
}

#[test]
fn resolve_catalog_key_prefers_exact_key_match() {
    let mut models = IndexMap::new();
    models.insert("grok-4.5".to_string(), make_model_entry("grok-4.5"));

    let persisted = acp::ModelId::new("grok-4.5");
    let key = resolve_catalog_key(&models, &persisted).expect("exact key must resolve");
    assert_eq!(key.0.as_ref(), "grok-4.5");
}

#[test]
fn resolve_catalog_key_none_when_slug_is_ambiguous() {
    let mut models = IndexMap::new();
    models.insert(
        "default-grok-build".to_string(),
        make_model_entry("grok-4.5"),
    );
    models.insert("user-grok-build".to_string(), make_model_entry("grok-4.5"));

    let persisted = acp::ModelId::new("grok-4.5");
    assert!(
        resolve_catalog_key(&models, &persisted).is_none(),
        "ambiguous routing slugs must not silently pick one catalog key"
    );
}

#[test]
fn selectable_catalog_key_for_persisted_none_when_resolved_not_available() {
    let mut models = IndexMap::new();
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-4.5"),
    );

    let available: IndexMap<_, _> = IndexMap::new();
    let persisted = acp::ModelId::new("grok-4.5");
    assert!(selectable_catalog_key_for_persisted(&models, &available, &persisted).is_none());
}

#[test]
fn selectable_prefers_available_identity_over_non_selectable_exact_key() {
    let mut models = IndexMap::new();
    models.insert("grok-build".to_string(), make_model_entry("grok-build"));
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-build"),
    );
    models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

    let available = test_available_keys(&["enterprise-grok-build", "grok-4.3"]);

    let persisted = acp::ModelId::new("grok-build");
    assert_eq!(
        resolve_catalog_key(&models, &persisted)
            .expect("exact key exists")
            .0
            .as_ref(),
        "grok-build"
    );
    let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
        .expect("must resolve to selectable section");
    assert_eq!(key.0.as_ref(), "enterprise-grok-build");
}

#[test]
fn selectable_matches_routing_slug_when_no_exact_key() {
    let mut models = IndexMap::new();
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-build"),
    );
    models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

    let available = test_available_keys(&["enterprise-grok-build", "grok-4.3"]);

    let persisted = acp::ModelId::new("grok-build");
    let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
        .expect("slug must resolve to selectable key");
    assert_eq!(key.0.as_ref(), "enterprise-grok-build");
}

#[test]
fn selectable_prefers_exact_key_over_later_slug_match() {
    let mut models = IndexMap::new();
    models.insert("grok-build".to_string(), make_model_entry("grok-4.5"));
    models.insert("other".to_string(), make_model_entry("grok-build"));

    let available = test_available_keys(&["grok-build", "other"]);

    let persisted = acp::ModelId::new("grok-build");
    let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
        .expect("exact selectable key must win");
    assert_eq!(key.0.as_ref(), "grok-build");
}

#[test]
fn selectable_catalog_resolution_reports_ambiguous_slug() {
    let mut models = IndexMap::new();
    models.insert("local-fast".to_string(), make_model_entry("qwen"));
    models.insert("remote-accurate".to_string(), make_model_entry("qwen"));
    let available = test_available_keys(&["local-fast", "remote-accurate"]);
    let persisted = acp::ModelId::new("qwen");

    let resolution = selectable_catalog_resolution_for_persisted(&models, &available, &persisted);
    assert_eq!(
        resolution,
        PersistedCatalogKeyResolution::AmbiguousSlug {
            slug: acp::ModelId::new("qwen"),
            matches: vec![
                acp::ModelId::new("local-fast"),
                acp::ModelId::new("remote-accurate")
            ],
        }
    );
    assert!(
        selectable_catalog_key_for_persisted(&models, &available, &persisted).is_none(),
        "legacy slug-only restores must require an explicit catalog key when ambiguous"
    );
}

fn test_available_keys(keys: &[&str]) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    keys.iter()
        .map(|k| {
            let id = acp::ModelId::new(*k);
            (id.clone(), acp::ModelInfo::new(id, (*k).to_string()))
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn bounded_auth_refresh_times_out_to_none() {
    // A hung IdP (never-ready auth future) must degrade to None within the
    // bound so a cold-cache boot fetch can't stall on it.
    let started = tokio::time::Instant::now();
    let result =
        ModelsManager::bounded_auth_refresh(std::future::pending::<Option<GrokAuth>>()).await;
    assert!(result.is_none(), "a hung auth refresh must yield None");
    assert!(
        started.elapsed() >= crate::http::STARTUP_AUTH_REFRESH_TIMEOUT,
        "must wait the full bound before giving up",
    );
}

#[tokio::test]
async fn bounded_auth_refresh_passes_through_ready_value() {
    let result =
        ModelsManager::bounded_auth_refresh(async { Some(GrokAuth::test_default()) }).await;
    assert!(
        result.is_some(),
        "a ready session must pass through unchanged"
    );
}

#[tokio::test]
async fn explicit_model_pick_survives_first_real_catalog() {
    // Non-blocking boot lets the user pick a model before the first real
    // catalog lands; that pick must not be clobbered by default reselection.
    let mgr = test_manager();
    let cfg = config_from_toml("[models]\ndefault = \"grok-4.5\"");
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4.5", "grok-4"])), None);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "an explicit /model pick must survive the first real catalog",
    );
}

#[tokio::test]
async fn identity_switch_clears_user_pick_latch() {
    // After an identity change (`clear()`), the new identity's first catalog must
    // reselect its own default rather than inherit the prior user's pick.
    let mgr = test_manager();
    let cfg = config_from_toml("[models]\ndefault = \"grok-4.5\"");
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    mgr.clear();
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4.5", "grok-4"])), None);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4.5",
        "a new identity's first catalog must reselect the default after clear()",
    );
}

/// A Codex entry the picker cannot offer must not suppress the xAI login
/// screen: `allowed_models` / `hidden_models` filter the catalog by clearing
/// `user_selectable` / setting `hidden`, and a session started on that basis
/// would strand the user on a default xAI model with no credential.
#[test]
fn filtered_out_codex_model_does_not_count_as_selectable() {
    let tmp = tempfile::tempdir().expect("temp home");
    let auth_home = tmp.path();
    let auth = crate::auth::GrokAuth {
        key: "live-codex-token".to_owned(),
        auth_mode: crate::auth::AuthMode::OpenAiCodex,
        refresh_token: Some("refresh".to_owned()),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        oidc_issuer: Some(crate::auth::openai_codex::ISSUER.to_owned()),
        oidc_client_id: Some(crate::auth::openai_codex::CLIENT_ID.to_owned()),
        account_id: Some("account".to_owned()),
        ..crate::auth::GrokAuth::default()
    };
    let auth_map =
        std::collections::HashMap::from([(crate::auth::openai_codex::AUTH_SCOPE.to_owned(), auth)]);
    std::fs::write(
        auth_home.join("auth.json"),
        serde_json::to_vec(&auth_map).unwrap(),
    )
    .unwrap();

    let toml_cfg: toml::Value = toml::from_str("").unwrap();
    let cfg = config::Config::new_from_toml_cfg(&toml_cfg).expect("config should parse");
    let mut catalog = config::resolve_model_list(&cfg, None);
    let preset_key = crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID;
    let preset = catalog.get_mut(preset_key).expect("preset in catalog");
    preset.auth_provider = Some(crate::auth::AuthProviderRef::openai_codex(
        crate::auth::openai_codex::manager(auth_home),
    ));

    let manager_with = |models: IndexMap<String, ModelEntry>| {
        let xai_home = tmp.path().join("xai");
        ModelsManagerBuilder::new(
            None,
            models,
            acp::ModelId::new("default"),
            Arc::new(AuthManager::new(&xai_home, GrokComConfig::default())),
            config::Config::default(),
        )
        .cache(test_cache_manager(tmp.path()))
        .build()
    };

    assert!(
        manager_with(catalog.clone()).has_selectable_openai_codex_model(),
        "a ready, unfiltered preset must count — otherwise this test is vacuous"
    );

    let mut filtered = catalog.clone();
    filtered.get_mut(preset_key).unwrap().info.user_selectable = false;
    assert!(
        !manager_with(filtered).has_selectable_openai_codex_model(),
        "an allowed_models-filtered Codex model must not suppress the login screen"
    );

    let mut hidden = catalog;
    hidden.get_mut(preset_key).unwrap().info.hidden = true;
    assert!(
        !manager_with(hidden).has_selectable_openai_codex_model(),
        "a hidden_models-hidden Codex model must not suppress the login screen"
    );
}

/// #131 helpers: a ready entry (first-party origin needs no declared
/// credential) and a manager built over a fixed catalog.
fn ready_entry(slug: &str) -> ModelEntry {
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

fn manager_over(cfg: &config::Config, catalog: IndexMap<String, ModelEntry>) -> ModelsManager {
    let tmp = tempfile::tempdir().expect("temp home for #131 models tests");
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    // Persist the directory for AuthManager's lifetime; unique per call so
    // parallel tests do not race on a shared `grok-test-models-131` path.
    let _path = tmp.keep();
    ModelsManager::from_config(cfg, Some(catalog), auth_manager)
        .expect("manager construction should succeed")
}

/// #131: a configured default **not seated** (absent from the catalog, or
/// present but not user-selectable) is still substituted, and that substitution
/// is now reported — the configured id and the configuration that supplied it.
///
/// This is the case no client can reconstruct: the substitute occupies
/// `currentModelId`, and the configured model is not in the selectable
/// `availableModels` listing to be looked up.
///
/// The selection itself is asserted unchanged. This reports the decision; it
/// does not remake it.
#[test]
fn absent_configured_default_is_substituted_and_reported() {
    let mut cfg = config::Config::default();
    cfg.models.default = Some("typo-provider".to_string());

    let mut catalog = IndexMap::new();
    catalog.insert("grok-4".to_string(), ready_entry("grok-4"));

    let mgr = manager_over(&cfg, catalog);

    // Unchanged: a substitute is still seated, exactly as before.
    assert_ne!(
        mgr.current_model_id().0.as_ref(),
        "typo-provider",
        "an absent preference is still substituted — this change reports, it does not select"
    );

    let reported = mgr
        .substituted_preference()
        .expect("an absent configured default must be reported");
    assert_eq!(reported.configured, "typo-provider");
    assert_eq!(
        reported.source_wire(),
        "config",
        "a `[models] default` preference is reported as coming from config"
    );
}

/// #131: present in the catalog but `user_selectable = false` is the other
/// half of "not seated". Resolve falls through to `Default` the same way as
/// absence; the preference must still be reported.
///
/// Asserted at the resolve layer: `from_config` rebuilds selectable flags from
/// `allowed_models` (and rejects a config default the allowlist excludes), so
/// a hand-flipped `user_selectable` does not survive construction. The docs
/// claim is about [`resolve_default_model`]'s fall-through.
#[test]
fn present_but_not_selectable_configured_default_is_substituted_and_reported() {
    let mut cfg = config::Config::default();
    cfg.models.default = Some("hidden-byo".to_string());

    let mut catalog = IndexMap::new();
    let mut blocked = ready_entry("hidden-byo");
    blocked.info.user_selectable = false;
    catalog.insert("hidden-byo".to_string(), blocked);
    catalog.insert("grok-4".to_string(), ready_entry("grok-4"));

    let (key, _, source, _) = resolve_default_model(&cfg, &catalog, false);
    assert_ne!(
        key.as_str(),
        "hidden-byo",
        "a not-user-selectable preference must not be seated"
    );
    let reported = substituted_preference(&cfg, source)
        .expect("present-but-not-selectable must be reported as substituted");
    assert_eq!(reported.configured, "hidden-byo");
    assert_eq!(reported.source_wire(), "config");
}

/// #131 counterweight: a configured default that is **honoured** must produce
/// no substitution field — including the kept-but-unready case, which #145
/// already covers through `readinessReason`. A field that appeared for a
/// preference the user did get would describe a rejection that never happened.
#[test]
fn honoured_configured_default_reports_no_substitution() {
    // Ready and present.
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-4".to_string());
    let mut catalog = IndexMap::new();
    catalog.insert("grok-4".to_string(), ready_entry("grok-4"));
    let mgr = manager_over(&cfg, catalog);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");
    assert!(
        mgr.substituted_preference().is_none(),
        "a honoured preference is not a substitution"
    );

    // Present but unready: #145 keeps it selected, so it was not substituted
    // either — and its reason already travels per-model.
    let mut cfg = config::Config::default();
    cfg.models.default = Some("byo-provider".to_string());
    let mut unready = config::ModelInfo::fallback("byo-provider");
    unready.base_url = "https://api.third-party.example/v1".to_string();
    let mut catalog = IndexMap::new();
    catalog.insert(
        "byo-provider".to_string(),
        ModelEntry {
            info: unready,
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
            config_validation_errors: Vec::new(),
        },
    );
    catalog.insert("grok-4".to_string(), ready_entry("grok-4"));
    let mgr = manager_over(&cfg, catalog);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "byo-provider",
        "#145 keeps an unready explicit preference selected"
    );
    assert!(
        mgr.substituted_preference().is_none(),
        "kept-but-unready is not a substitution; its reason travels as readinessReason"
    );
}

/// #131: a campaign-driven default that goes missing must not be reported as
/// the user's configuration being rejected. They never wrote it, and naming it
/// would send them to edit a line that is not theirs.
#[test]
fn campaign_driven_default_is_not_reported_as_the_users_choice() {
    let mut cfg = config::Config::default();
    cfg.models.default = Some("pushed-model".to_string());
    cfg.models.default_is_campaign_driven = true;

    let mut catalog = IndexMap::new();
    catalog.insert("grok-4".to_string(), ready_entry("grok-4"));

    let mgr = manager_over(&cfg, catalog);
    assert!(
        mgr.substituted_preference().is_none(),
        "a pushed default is not the user's configuration"
    );
}

/// #131: for a configured default that is present in the catalog but
/// **unready**, the readiness reason already reaches the client — through
/// per-model `readinessReason` in `modelState.availableModels`, not through
/// `unready_default_reason`, which is log-only.
///
/// This is why the kept case needs no new wire field. The issue predates #145,
/// and #145 keeping the model instead of swapping it is exactly what makes
/// `currentModelId` *be* the configured model, which is what makes the
/// per-model path sufficient. What #131 still owes the user is the case this
/// chain cannot reach: a configured default **absent** from the catalog, which
/// no client can look up because it is not there to look up.
///
/// Asserted link by link on purpose: if this breaks, the failure names which
/// link moved rather than only "the reason stopped arriving".
#[test]
fn configured_unready_default_already_publishes_its_reason_per_model() {
    use crate::agent::config::model_readiness;
    use crate::agent::models::resolution::available_models;

    let mut cfg = config::Config::default();
    cfg.models.default = Some("byo-provider".to_string());

    // Credential-less against a non-first-party origin: unready with no
    // dependence on ambient environment, so the assertions below are about the
    // chain rather than about the machine running them.
    let mut info = config::ModelInfo::fallback("byo-provider");
    info.base_url = "https://api.third-party.example/v1".to_string();
    let mut catalog = IndexMap::new();
    catalog.insert(
        "byo-provider".to_string(),
        ModelEntry {
            info,
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: None,
            config_validation_errors: Vec::new(),
        },
    );

    // Link 0 — the model really is unready, and readiness produced a reason.
    let (ready, reason) = model_readiness(&catalog["byo-provider"]);
    assert!(
        !ready,
        "precondition: credential-less external model is unready"
    );
    let reason = reason.expect("an unready model must carry an actionable reason");

    // Link 1 (#145) — an explicit configured preference is KEPT, not swapped,
    // so the selected model *is* the one the user configured.
    let (key, _entry, source, unready) = resolve_default_model(&cfg, &catalog, true);
    assert_eq!(
        key, "byo-provider",
        "#145: an unready explicit preference is kept, not substituted"
    );
    assert!(
        matches!(source, config::ConfigSource::Config),
        "a `[models] default` preference reports source Config, got {source:?}"
    );
    assert_eq!(
        unready.as_deref(),
        Some(reason.as_str()),
        "the same reason is handed back to the caller"
    );

    // Link 2 — readiness is NOT a filter on the ACP listing, so the model the
    // user configured is still there for a client to look up.
    let available = available_models(&catalog, true);
    let listed = available
        .get(&acp::ModelId::new(key.as_str()))
        .expect("unready entries stay in availableModels (#133)");

    // Link 3 — and it carries the reason, in the field the pager and headless
    // already read via `unready_reason_from_model_meta`.
    let meta = listed.meta.as_ref().expect("listed model carries meta");
    assert_eq!(
        meta.get("ready").and_then(serde_json::Value::as_bool),
        Some(false),
        "the listing marks it unready"
    );
    assert_eq!(
        meta.get("readinessReason")
            .and_then(serde_json::Value::as_str),
        Some(reason.as_str()),
        "the reason the client renders is the reason readiness computed"
    );
}

/// #131 B2: when the current model is still present and selectable,
/// `reselect_current_model_if_missing` must still recompute the substitution
/// verdict. A stale `Some` taken against an emptier catalog would otherwise
/// survive the early return forever.
#[test]
fn reselect_if_missing_clears_stale_substitution_on_early_return() {
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-4".to_string());
    let mut catalog = IndexMap::new();
    catalog.insert("grok-4".to_string(), ready_entry("grok-4"));
    let mgr = manager_over(&cfg, catalog);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");
    assert!(
        mgr.substituted_preference().is_none(),
        "precondition: preference is honoured"
    );

    *mgr.inner.substituted_preference.write() = Some(SubstitutedPreference {
        configured: "grok-4".to_string(),
        source: config::ConfigSource::Config,
    });
    assert!(
        mgr.substituted_preference().is_some(),
        "precondition: inject a stale accusation"
    );

    mgr.reselect_current_model_if_missing(&cfg);
    assert!(
        mgr.substituted_preference().is_none(),
        "early-return must still clear a stale substitution verdict"
    );
}

/// #131 B2: `clear()` must wipe the substitution verdict so a new identity
/// does not inherit the previous identity's accusation.
#[test]
fn clear_wipes_substituted_preference() {
    let mut cfg = config::Config::default();
    cfg.models.default = Some("typo-provider".to_string());
    let mut catalog = IndexMap::new();
    catalog.insert("grok-4".to_string(), ready_entry("grok-4"));
    let mgr = manager_over(&cfg, catalog);
    assert!(
        mgr.substituted_preference().is_some(),
        "precondition: absent preference is reported"
    );

    mgr.clear();
    assert!(
        mgr.substituted_preference().is_none(),
        "clear() must wipe the substitution verdict with the rest of identity state"
    );
}

/// #131 B1+B4+B5: after a false accusation against a thinner (warm-cache)
/// catalog, landing the real catalog must (1) *reseat* the configured
/// preference, (2) clear the in-memory verdict, and (3) publish JSON `null` on
/// a real `x.ai/models/update` ExtNotification — not via a hand-rolled
/// `write_substituted_default_model_meta` call that bypasses the wire.
///
/// Deleting the `model_state.meta(...)` block in `notify_models_updated` must
/// fail this test; asserting only on `substituted_preference()` would not
/// (same bar as initialize B3). Asserting seating stops the warm-cache path
/// from retracting while the substitute stays current.
#[test]
fn models_update_meta_clears_substitution_after_catalog_lands() {
    let mut cfg = config::Config::default();
    cfg.models.default = Some("my-byo".to_string());

    let mut thin = IndexMap::new();
    thin.insert("grok-4".to_string(), ready_entry("grok-4"));
    let mgr = manager_over(&cfg, thin);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "precondition: thin catalog seats the substitute"
    );
    assert!(
        mgr.substituted_preference().is_some(),
        "precondition: thin catalog substitutes the preference"
    );

    let (gateway, mut rx) = crate::test_support::lsp_runtime::test_gateway_with_receiver();
    mgr.set_gateway(gateway);

    // Accusation while it stands — through the notify wire, not a hand write.
    mgr.notify_models_updated();
    let accused = recv_models_update_meta(&mut rx);
    assert!(
        accused
            .get(SUBSTITUTED_DEFAULT_MODEL_META_KEY)
            .is_some_and(|v| v.is_object()),
        "x.ai/models/update SessionModelState._meta must carry the accusation while it stands"
    );

    let mut full = IndexMap::new();
    full.insert("my-byo".to_string(), ready_entry("my-byo"));
    full.insert("grok-4".to_string(), ready_entry("grok-4"));
    // Prefetch already set has_fetched_real_catalog — this is the warm-cache
    // path that takes reselect_current_model_if_missing, not reselect_default.
    assert!(
        mgr.inner.catalog.read().has_fetched_real_catalog,
        "precondition: warm-cache path (prefetch marked the catalog real)"
    );
    mgr.apply_refresh_result(&cfg, Some(full), None);
    mgr.notify_models_updated();

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "my-byo",
        "warm-cache refresh must reseat the now-honourable preference — \
         retracting while the substitute stays seated is the B4 lie"
    );
    assert!(
        mgr.substituted_preference().is_none(),
        "in-memory verdict must clear once the preference is seated"
    );

    let cleared = recv_models_update_meta(&mut rx);
    assert!(
        cleared
            .get(SUBSTITUTED_DEFAULT_MODEL_META_KEY)
            .is_some_and(|v| v.is_null()),
        "x.ai/models/update SessionModelState._meta must publish JSON null so clients can retract"
    );
    assert_eq!(
        cleared.get("currentModelId").and_then(|v| v.as_str()),
        Some("my-byo"),
        "wire currentModelId must name the reseated preference"
    );
}

/// Drain one `x.ai/models/update` ExtNotification and return its params as JSON
/// (the `SessionModelState` body, including `_meta`).
fn recv_models_update_meta(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) -> serde_json::Map<String, serde_json::Value> {
    let msg = rx
        .try_recv()
        .expect("expected an x.ai/models/update ExtNotification");
    let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
        panic!("expected ExtNotification, got another message kind");
    };
    assert_eq!(
        args.request.method.as_ref(),
        "x.ai/models/update",
        "notify_models_updated must publish x.ai/models/update"
    );
    let params: serde_json::Value =
        serde_json::from_str(args.request.params.get()).expect("models/update params are JSON");
    let obj = params
        .as_object()
        .cloned()
        .expect("SessionModelState serializes as a JSON object");
    // Flatten: assertions look at both top-level currentModelId and _meta keys.
    let mut flat = obj.clone();
    if let Some(serde_json::Value::Object(meta)) = obj.get("_meta") {
        for (k, v) in meta {
            flat.insert(k.clone(), v.clone());
        }
    }
    flat
}

/// #131 B4 counterweight: an explicit `/model` pick must not be clobbered when
/// a previously missing configured preference later appears in the catalog.
#[test]
fn warm_cache_refresh_does_not_reseat_over_user_model_pick() {
    let mut cfg = config::Config::default();
    cfg.models.default = Some("my-byo".to_string());

    let mut thin = IndexMap::new();
    thin.insert("grok-4".to_string(), ready_entry("grok-4"));
    let mgr = manager_over(&cfg, thin);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");
    // User explicitly keeps the substitute.
    mgr.set_current_model_id(acp::ModelId::new(std::sync::Arc::from("grok-4")));

    let mut full = IndexMap::new();
    full.insert("my-byo".to_string(), ready_entry("my-byo"));
    full.insert("grok-4".to_string(), ready_entry("grok-4"));
    mgr.apply_refresh_result(&cfg, Some(full), None);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "a /model pick must survive the preference becoming available"
    );
}

#[test]
fn test_catalog_auth_schemes_and_override() {
    use crate::remote::client::fetch_models_blocking;
    use xai_grok_test_support::EnvGuard;

    let target_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let last_auth_header = Arc::new(std::sync::Mutex::new(None));
    let last_custom_header = Arc::new(std::sync::Mutex::new(None));
    let last_extra_header = Arc::new(std::sync::Mutex::new(None));

    let target_reqs_clone = target_requests.clone();
    let auth_header_clone = last_auth_header.clone();
    let custom_header_clone = last_custom_header.clone();
    let extra_header_clone = last_extra_header.clone();

    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(move |headers: axum::http::HeaderMap| async move {
            target_reqs_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(auth) = headers.get("authorization")
                && let Ok(s) = auth.to_str()
            {
                *auth_header_clone.lock().unwrap() = Some(s.to_string());
            }
            if let Some(custom) = headers.get("x-api-key")
                && let Ok(s) = custom.to_str()
            {
                *custom_header_clone.lock().unwrap() = Some(s.to_string());
            }
            if let Some(extra) = headers.get("X-Organization")
                && let Ok(s) = extra.to_str()
            {
                *extra_header_clone.lock().unwrap() = Some(s.to_string());
            }
            axum::Json(serde_json::json!({
                "data": [
                    {
                        "id": "my-mock-model",
                        "model": "my-mock-model",
                        "contextWindow": 4096,
                        "baseUrl": "https://api.example.com/v1"
                    }
                ]
            }))
        }),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let listener =
        rt.block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap() });
    let addr = listener.local_addr().unwrap();
    let mock_endpoint = format!("http://{}/v1/models", addr);

    let server_task = rt.spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Test cases:
    // 1. None auth scheme (anonymous)
    {
        let mut cfg = config::Config::default();
        cfg.models.endpoint = Some(mock_endpoint.clone());
        cfg.models.catalog_auth_scheme = Some("none".to_string());

        let catalog_auth = cfg.models.catalog_auth_config().unwrap();
        cfg.endpoints.catalog_auth = catalog_auth;

        *last_auth_header.lock().unwrap() = None;
        *last_custom_header.lock().unwrap() = None;

        let result = fetch_models_blocking(
            &cfg.endpoints,
            None,
            crate::agent::models::ModelFetchAuth::CustomEndpoint,
        );
        assert!(result.is_ok());
        assert_eq!(*last_auth_header.lock().unwrap(), None);
        assert_eq!(*last_custom_header.lock().unwrap(), None);
    }

    // 2. Bearer auth scheme
    {
        let _g = EnvGuard::set("CATALOG_KEY", "dummy-bearer-token");
        let mut cfg = config::Config::default();
        cfg.models.endpoint = Some(mock_endpoint.clone());
        cfg.models.catalog_auth_scheme = Some("bearer".to_string());
        cfg.models.catalog_env_key = Some(config::EnvKeys::One("CATALOG_KEY".to_string()));

        let catalog_auth = cfg.models.catalog_auth_config().unwrap();
        cfg.endpoints.catalog_auth = catalog_auth;

        *last_auth_header.lock().unwrap() = None;

        let result = fetch_models_blocking(
            &cfg.endpoints,
            None,
            crate::agent::models::ModelFetchAuth::CustomEndpoint,
        );
        assert!(result.is_ok());
        assert_eq!(
            *last_auth_header.lock().unwrap(),
            Some("Bearer dummy-bearer-token".to_string())
        );
    }

    // 3. X-API-KEY auth scheme
    {
        let _g = EnvGuard::set("CATALOG_KEY", "dummy-x-api-key");
        let mut cfg = config::Config::default();
        cfg.models.endpoint = Some(mock_endpoint.clone());
        cfg.models.catalog_auth_scheme = Some("x_api_key".to_string());
        cfg.models.catalog_env_key = Some(config::EnvKeys::One("CATALOG_KEY".to_string()));

        let catalog_auth = cfg.models.catalog_auth_config().unwrap();
        cfg.endpoints.catalog_auth = catalog_auth;

        *last_custom_header.lock().unwrap() = None;

        let result = fetch_models_blocking(
            &cfg.endpoints,
            None,
            crate::agent::models::ModelFetchAuth::CustomEndpoint,
        );
        assert!(result.is_ok());
        assert_eq!(
            *last_custom_header.lock().unwrap(),
            Some("dummy-x-api-key".to_string())
        );
    }

    // 4. Extra headers
    {
        let mut cfg = config::Config::default();
        cfg.models.endpoint = Some(mock_endpoint.clone());
        cfg.models.catalog_auth_scheme = Some("none".to_string());
        let mut headers = IndexMap::new();
        headers.insert("X-Organization".to_string(), "Anthropic".to_string());
        headers.insert("Host".to_string(), "bad-host.com".to_string());
        cfg.models.catalog_headers = headers;

        let catalog_auth = cfg.models.catalog_auth_config().unwrap();
        cfg.endpoints.catalog_auth = catalog_auth;

        *last_extra_header.lock().unwrap() = None;

        // Validation fails because Host is a protected header
        let validate_result = crate::remote::validate_models_catalog_auth(
            &cfg.endpoints,
            crate::agent::models::ModelFetchAuth::CustomEndpoint,
            true,
        );
        assert!(validate_result.is_err());
        assert!(
            validate_result
                .unwrap_err()
                .contains("protected and cannot be overridden")
        );
    }

    // 5. Invalid validation (empty env var or missing env var name)
    {
        let mut cfg = config::Config::default();
        cfg.models.endpoint = Some(mock_endpoint.clone());
        cfg.models.catalog_auth_scheme = Some("bearer".to_string());
        let catalog_auth = cfg.models.catalog_auth_config().unwrap();
        cfg.endpoints.catalog_auth = catalog_auth;
        let validate_result = crate::remote::validate_models_catalog_auth(
            &cfg.endpoints,
            crate::agent::models::ModelFetchAuth::CustomEndpoint,
            true,
        );
        assert!(validate_result.is_err());
        assert!(
            validate_result
                .unwrap_err()
                .contains("catalog_env_key is missing")
        );

        let _g = EnvGuard::set("EMPTY_KEY", "");
        let mut cfg2 = config::Config::default();
        cfg2.models.endpoint = Some(mock_endpoint.clone());
        cfg2.models.catalog_auth_scheme = Some("bearer".to_string());
        cfg2.models.catalog_env_key = Some(config::EnvKeys::One("EMPTY_KEY".to_string()));
        cfg2.endpoints.catalog_auth = cfg2.models.catalog_auth_config().unwrap();
        let validate_result = crate::remote::validate_models_catalog_auth(
            &cfg2.endpoints,
            crate::agent::models::ModelFetchAuth::CustomEndpoint,
            true,
        );
        assert!(validate_result.is_err());
    }

    server_task.abort();
}

// ── #303 Codex-only implicit default ────────────────────────────────

/// Serialize #303 fixtures: they mutate process env (GROK_AUTH_PATH / XAI keys)
/// and must not interleave.
static CODEX_ONLY_DEFAULT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Build a ready Codex preset entry backed by a live-looking scoped credential.
///
/// Returns an [`EnvGuard`] that pins `GROK_AUTH_PATH` to this fixture's auth
/// file for the lifetime of the test — required because
/// `AuthManager::new_openai_codex` prefers that env over `grok_home`.
fn ready_codex_entry(auth_home: &std::path::Path) -> (ModelEntry, xai_grok_test_support::EnvGuard) {
    use xai_grok_test_support::EnvGuard;
    let auth_path = auth_home.join("auth.json");
    let auth = crate::auth::GrokAuth {
        key: "live-codex-token".to_owned(),
        auth_mode: crate::auth::AuthMode::OpenAiCodex,
        refresh_token: Some("refresh".to_owned()),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        oidc_issuer: Some(crate::auth::openai_codex::ISSUER.to_owned()),
        oidc_client_id: Some(crate::auth::openai_codex::CLIENT_ID.to_owned()),
        account_id: Some("account".to_owned()),
        ..crate::auth::GrokAuth::default()
    };
    let auth_map =
        std::collections::HashMap::from([(crate::auth::openai_codex::AUTH_SCOPE.to_owned(), auth)]);
    std::fs::write(&auth_path, serde_json::to_vec(&auth_map).unwrap()).unwrap();

    // Pin after write so status reads this file even if peer tests thrash env.
    let auth_path_guard = EnvGuard::set(
        "GROK_AUTH_PATH",
        auth_path.to_str().expect("utf-8 temp path"),
    );

    let slug = crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID;
    let mut entry = ModelEntry::fallback(slug, &config::EndpointsConfig::default());
    entry.info.model = slug.to_string();
    entry.info.api_backend = crate::sampling::ApiBackend::CodexResponses;
    entry.info.base_url = crate::auth::openai_codex::CODEX_API_BASE_URL.to_string();
    entry.info.user_selectable = true;
    entry.auth_provider = Some(crate::auth::AuthProviderRef::openai_codex(
        crate::auth::openai_codex::manager(auth_home),
    ));
    let (ready, reason) = crate::agent::config::model_readiness(&entry);
    assert!(
        ready,
        "fixture Codex entry must be ready, got reason={reason:?}"
    );
    (entry, auth_path_guard)
}

#[test]
fn codex_only_cold_start_defaults_to_ready_codex() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let codex_key = crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string();

    // Grok first (bundled order), then ready Codex — the historical failure mode.
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(codex_key.clone(), codex);

    let cfg = config::Config::default(); // no explicit preference
    let (key, entry, source, reason) = resolve_default_model(&cfg, &catalog, false);
    assert!(
        reason.is_none(),
        "ready Codex default must not be unready: {reason:?}"
    );
    assert!(
        matches!(source, config::ConfigSource::Default),
        "implicit path reports Default, got {source:?}"
    );
    assert_eq!(
        key, codex_key,
        "#303: Codex-only cold start must seat ready Codex, not ambient-ready Grok"
    );
    assert_eq!(
        entry.info.api_backend,
        crate::sampling::ApiBackend::CodexResponses
    );
}

#[test]
fn codex_only_default_not_bundled_grok_when_codex_ready() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );

    let (key, _, _, _) = resolve_default_model(&config::Config::default(), &catalog, false);
    assert_ne!(
        key,
        crate::models::default_model(),
        "must not keep the bundled default when a ready Codex route exists"
    );
}

#[test]
fn xai_ambient_still_prefers_first_party_grok() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::XAI_API_KEY_ENV_VAR;
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, "test-xai-key-not-real");

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );

    let (key, _, _, _) = resolve_default_model(&config::Config::default(), &catalog, false);
    assert_eq!(
        key, "grok-4.5",
        "with ambient XAI_API_KEY, first ready first-party Grok remains eligible"
    );
}

#[test]
fn codex_ready_reseats_ambient_grok_without_xai_auth() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let codex_key = crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string();

    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(codex_key.clone(), codex);

    let xai_home = tmp.path().join("xai");
    std::fs::create_dir_all(&xai_home).unwrap();
    let mgr = ModelsManagerBuilder::new(
        None,
        catalog,
        acp::ModelId::new("grok-4.5"), // stranded default
        Arc::new(AuthManager::new(&xai_home, GrokComConfig::default())),
        config::Config::default(),
    )
    .cache(test_cache_manager(tmp.path()))
    .build();

    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4.5");
    mgr.reselect_current_model_if_missing(&config::Config::default());
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        codex_key.as_str(),
        "#303: manager reseat must leave ambient Grok for ready Codex when no usable xAI auth"
    );
}

#[test]
#[serial_test::serial]
fn invalid_env_probe_reseats_only_after_unusable_verdict() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, "invalid-xai-key");
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
    let _default = EnvGuard::unset("GROK_DEFAULT_MODEL");

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let codex_key = crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string();
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(codex_key.clone(), codex);
    let prefetched = IndexMap::from([(
        "grok-4.5".to_string(),
        catalog.get("grok-4.5").expect("ambient Grok entry").clone(),
    )]);
    let empty = toml::Value::Table(toml::map::Map::new());
    let production_cfg = config::Config::new_from_toml_cfg(&empty)
        .expect("production config with canonical Codex preset");

    let xai_home = tmp.path().join("xai-probe-verdict");
    std::fs::create_dir_all(&xai_home).unwrap();
    let auth_manager = Arc::new(AuthManager::new(&xai_home, GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        Some(prefetched.clone()),
        catalog,
        acp::ModelId::new("grok-4.5"),
        auth_manager.clone(),
        production_cfg.clone(),
    )
    .cache(test_cache_manager(tmp.path()))
    .build();

    mgr.apply_first_party_env_api_key_probe_result(true);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4.5");
    assert!(auth_manager.first_party_env_api_key_ok());

    mgr.apply_first_party_env_api_key_probe_result(false);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        codex_key.as_str(),
        "a failed probe must invalidate presence-only Grok precedence"
    );
    assert!(!auth_manager.first_party_env_api_key_ok());

    mgr.apply_catalog_for_test(prefetched);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        codex_key.as_str(),
        "the first real catalog must retain the failed env-probe verdict"
    );

    let default_reapply = production_cfg.clone();
    assert!(
        !mgr.usable_ambient_xai(&default_reapply),
        "the stored failed verdict must suppress env-only ambient xAI"
    );
    mgr.apply_config_reselecting_default(default_reapply);
    assert!(
        !mgr.usable_ambient_xai(&production_cfg),
        "config reapply must not clear the stored failed verdict"
    );
    assert!(mgr.models().values().any(|entry| {
        resolution::is_ready_selectable_openai_codex_entry(entry, mgr.is_session_auth())
    }));
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        codex_key.as_str(),
        "a later default re-resolution must retain the failed env-probe verdict"
    );

    let mut campaign = production_cfg.clone();
    campaign.models.default = Some("grok-4.5".to_string());
    campaign.models.default_is_campaign_driven = true;
    mgr.apply_config_reselecting_default(campaign);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        codex_key.as_str(),
        "a campaign-driven Grok default must not revive a proven-invalid ambient env route"
    );

    mgr.apply_first_party_env_api_key_probe_result(true);
    let mut valid_campaign = production_cfg;
    valid_campaign.models.default = Some("grok-4.5".to_string());
    valid_campaign.models.default_is_campaign_driven = true;
    mgr.apply_config_reselecting_default(valid_campaign);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4.5",
        "a successful env probe must keep campaign-driven Grok eligible"
    );
}

#[test]
#[serial_test::serial]
fn invalid_env_probe_does_not_overwrite_user_pick_at_commit_boundary() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, "invalid-xai-key");
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
    let _default = EnvGuard::unset("GROK_DEFAULT_MODEL");

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut user_model = ready_entry("local-user-model");
    user_model.info.base_url = "http://127.0.0.1:8080/v1".to_string();
    user_model.info.auth_scheme = xai_grok_sampler::AuthScheme::None;
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );
    catalog.insert("local-user-model".to_string(), user_model);

    let xai_home = tmp.path().join("xai-probe-user-race");
    std::fs::create_dir_all(&xai_home).unwrap();
    let auth_manager = Arc::new(AuthManager::new(&xai_home, GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        catalog,
        acp::ModelId::new("grok-4.5"),
        auth_manager.clone(),
        config::Config::default(),
    )
    .cache(test_cache_manager(tmp.path()))
    .build();

    mgr.apply_first_party_env_api_key_probe_result_with_before_commit(false, || {
        assert!(
            mgr.inner.cfg.try_write().is_none(),
            "the config snapshot must stay read-locked through current-model commit"
        );
        assert!(
            mgr.inner.catalog.try_write().is_none(),
            "the catalog snapshot must stay read-locked through current-model commit"
        );
        mgr.set_current_model_id(acp::ModelId::new("local-user-model"));
    });

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "local-user-model",
        "a user pick after target resolution but before commit must win"
    );
    assert!(mgr.inner.user_selected_model.load(Ordering::Relaxed));
    assert!(
        !auth_manager.first_party_env_api_key_ok(),
        "the race-safe abort must still publish the failed probe verdict"
    );
}

#[test]
#[serial_test::serial]
fn invalid_env_probe_preserves_explicit_and_user_picked_grok() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, "invalid-xai-key");
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
    let _default = EnvGuard::unset("GROK_DEFAULT_MODEL");

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );
    let xai_home = tmp.path().join("xai-explicit-probe");
    std::fs::create_dir_all(&xai_home).unwrap();
    let auth_manager = Arc::new(AuthManager::new(&xai_home, GrokComConfig::default()));
    let manager = |cfg: config::Config| {
        ModelsManagerBuilder::new(
            Some(catalog.clone()),
            catalog.clone(),
            acp::ModelId::new("grok-4.5"),
            auth_manager.clone(),
            cfg,
        )
        .cache(test_cache_manager(tmp.path()))
        .build()
    };

    let mut cli = config::Config::default();
    cli.default_model_override = Some("grok-4.5".to_string());
    let cli_mgr = manager(cli);
    cli_mgr.apply_first_party_env_api_key_probe_result(false);
    assert_eq!(cli_mgr.current_model_id().0.as_ref(), "grok-4.5");

    {
        let _env_default = EnvGuard::set("GROK_DEFAULT_MODEL", "grok-4.5");
        let env_mgr = manager(config::Config::default());
        env_mgr.apply_first_party_env_api_key_probe_result(false);
        assert_eq!(env_mgr.current_model_id().0.as_ref(), "grok-4.5");
    }

    let mut configured = config::Config::default();
    configured.models.default = Some("grok-4.5".to_string());
    let configured_mgr = manager(configured);
    configured_mgr.apply_first_party_env_api_key_probe_result(false);
    assert_eq!(configured_mgr.current_model_id().0.as_ref(), "grok-4.5");
    let mut configured_reapply = config::Config::default();
    configured_reapply.models.default = Some("grok-4.5".to_string());
    configured_mgr.apply_config_reselecting_default(configured_reapply);
    assert_eq!(
        configured_mgr.current_model_id().0.as_ref(),
        "grok-4.5",
        "a genuine user config preference remains authoritative after a failed env probe"
    );

    let mut missing = config::Config::default();
    missing.default_model_override = Some("missing-explicit-model".to_string());
    let missing_mgr = manager(missing);
    missing_mgr.apply_first_party_env_api_key_probe_result(false);
    assert_eq!(
        missing_mgr.current_model_id().0.as_ref(),
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID,
        "a missing explicit preference must not protect its implicit Grok substitute"
    );

    let picked_mgr = manager(config::Config::default());
    picked_mgr.set_current_model_id(acp::ModelId::new("grok-4.5"));
    picked_mgr.apply_first_party_env_api_key_probe_result(false);
    assert_eq!(picked_mgr.current_model_id().0.as_ref(), "grok-4.5");
}

#[test]
#[serial_test::serial]
fn invalid_env_probe_preserves_other_usable_xai_routes() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use crate::auth::{
        AuthMode, FirstPartySessionEligibility, GrokAuth, PreferredAuthMethod, XAI_OAUTH2_ISSUER,
    };
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, "invalid-xai-key");
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
    let _default = EnvGuard::unset("GROK_DEFAULT_MODEL");

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );
    let auth_manager = |suffix: &str| {
        let home = tmp.path().join(suffix);
        std::fs::create_dir_all(&home).unwrap();
        Arc::new(AuthManager::new(&home, GrokComConfig::default()))
    };
    let manager = |cfg: config::Config, auth: Arc<AuthManager>| {
        ModelsManagerBuilder::new(
            Some(catalog.clone()),
            catalog.clone(),
            acp::ModelId::new("grok-4.5"),
            auth,
            cfg,
        )
        .cache(test_cache_manager(tmp.path()))
        .build()
    };
    let assert_preserved = |mgr: ModelsManager, label: &str| {
        mgr.apply_first_party_env_api_key_probe_result(false);
        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "grok-4.5",
            "{label} must preserve Grok after the failed env probe"
        );
        assert!(
            !mgr.inner.auth_manager.first_party_env_api_key_ok(),
            "{label} must still publish the failed env-key verdict"
        );
        let cfg = mgr.inner.cfg.read().clone();
        mgr.apply_config_reselecting_default(cfg);
        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "grok-4.5",
            "{label} must preserve Grok across later default re-resolution"
        );
    };

    let mut deployment = config::Config::default();
    deployment.endpoints.deployment_key = Some("deployment-key".to_string());
    assert_preserved(
        manager(deployment, auth_manager("xai-deployment-probe")),
        "nonblank deployment key",
    );

    let mut api_key_pin = config::Config::default();
    api_key_pin.grok_com_config.preferred_method = Some(PreferredAuthMethod::ApiKey);
    assert_preserved(
        manager(api_key_pin, auth_manager("xai-api-key-pin-probe")),
        "preferred_method=api_key",
    );

    let mut oidc_pin = config::Config::default();
    oidc_pin.grok_com_config.preferred_method = Some(PreferredAuthMethod::Oidc);
    assert_preserved(
        manager(oidc_pin, auth_manager("xai-oidc-pin-probe")),
        "preferred_method=oidc",
    );

    let wire_usable = auth_manager("xai-wire-usable-probe");
    wire_usable.hot_swap(GrokAuth {
        key: "live-access".into(),
        auth_mode: AuthMode::Oidc,
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        refresh_token: Some("live-refresh".into()),
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_owned()),
        oidc_client_id: Some("client".into()),
        user_id: "u".into(),
        ..GrokAuth::test_default()
    });
    assert_eq!(
        wire_usable.first_party_session_eligibility(),
        FirstPartySessionEligibility::WireUsable
    );
    assert_preserved(
        manager(config::Config::default(), wire_usable),
        "wire-usable xAI session",
    );

    let refreshable = auth_manager("xai-refreshable-probe");
    refreshable.hot_swap(GrokAuth {
        key: "expired-access".into(),
        auth_mode: AuthMode::Oidc,
        create_time: chrono::Utc::now() - chrono::Duration::hours(2),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        refresh_token: Some("refresh-complete".into()),
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_owned()),
        oidc_client_id: Some("client".into()),
        user_id: "u".into(),
        ..GrokAuth::test_default()
    });
    assert_eq!(
        refreshable.first_party_session_eligibility(),
        FirstPartySessionEligibility::Refreshable
    );
    assert_preserved(
        manager(config::Config::default(), refreshable),
        "complete-refreshable expired OIDC session",
    );
}

#[test]
fn byok_unchanged_when_codex_ready() {
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());

    // BYOK-style entry: own api_key, non-first-party origin.
    let mut byok = make_model_entry("my-byok");
    byok.info.base_url = "https://third-party.example/v1".to_string();
    byok.api_key = Some("sk-test".to_string());
    assert!(byok.has_own_credentials());
    assert!(!resolution::is_first_party_ambient_xai_entry(&byok));

    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("my-byok".to_string(), byok);
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );

    let mut cfg = config::Config::default();
    cfg.models.default = Some("my-byok".to_string());
    let (key, _, _, _) = resolve_default_model(&cfg, &catalog, false);
    assert_eq!(
        key, "my-byok",
        "explicit BYOK preference must not be swapped for Codex under #303"
    );

    // Manager reseat must not yank BYOK current when ready Codex exists.
    let xai_home = tmp.path().join("xai");
    std::fs::create_dir_all(&xai_home).unwrap();
    let mgr = ModelsManagerBuilder::new(
        None,
        catalog,
        acp::ModelId::new("my-byok"),
        Arc::new(AuthManager::new(&xai_home, GrokComConfig::default())),
        config::Config::default(),
    )
    .cache(test_cache_manager(tmp.path()))
    .build();
    mgr.reselect_current_model_if_missing(&config::Config::default());
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "my-byok",
        "BYOK current must not be stranded-reseat to Codex"
    );
}

/// Keyless `auth_scheme = none` is not ambient first-party xAI and must not be
/// reseated to Codex when the user already seated it (Pro P1 coverage split).
#[test]
fn auth_scheme_none_unchanged_when_codex_ready() {
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());

    let mut keyless = make_model_entry("local-none");
    keyless.info.base_url = "http://127.0.0.1:8080/v1".to_string();
    keyless.info.auth_scheme = xai_grok_sampler::AuthScheme::None;
    keyless.api_key = None;
    keyless.env_key = None;
    assert!(!keyless.has_own_credentials());
    assert!(!resolution::is_first_party_ambient_xai_entry(&keyless));

    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("local-none".to_string(), keyless);
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );

    let mut cfg = config::Config::default();
    cfg.models.default = Some("local-none".to_string());
    let (key, _, _, _) = resolve_default_model(&cfg, &catalog, false);
    assert_eq!(
        key, "local-none",
        "explicit auth_scheme=none preference must not be swapped for Codex"
    );

    let xai_home = tmp.path().join("xai-none");
    std::fs::create_dir_all(&xai_home).unwrap();
    let mgr = ModelsManagerBuilder::new(
        None,
        catalog,
        acp::ModelId::new("local-none"),
        Arc::new(AuthManager::new(&xai_home, GrokComConfig::default())),
        config::Config::default(),
    )
    .cache(test_cache_manager(tmp.path()))
    .build();
    mgr.reselect_current_model_if_missing(&config::Config::default());
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "local-none",
        "auth_scheme=none current must not be stranded-reseat to Codex"
    );
}

/// Deployment key is ambient usable xAI — keep Grok over ready Codex.
#[test]
fn deployment_key_keeps_first_party_when_codex_ready() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );

    let mut cfg = config::Config::default();
    cfg.endpoints.deployment_key = Some("deploy-key-not-blank".to_string());
    assert!(
        resolution::usable_ambient_xai_auth(&cfg, false),
        "non-empty deployment_key is ambient usable xAI"
    );
    let (key, _, _, _) = resolve_default_model(&cfg, &catalog, false);
    assert_eq!(key, "grok-4.5");
}

/// Third-party CodexResponses shim must not steal #303 default over ambient Grok
/// (Pro P1: only official OpenAI Codex account routes qualify).
#[test]
fn third_party_codex_responses_does_not_steal_default() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let mut shim = make_model_entry("third-party-codex");
    shim.info.base_url = "https://proxy.example/codex".to_string();
    shim.info.api_backend = crate::sampling::ApiBackend::CodexResponses;
    shim.info.user_selectable = true;
    // Ready-ish without openai-codex provider / official base URL.
    assert!(
        !resolution::is_openai_codex_account_route(&shim),
        "third-party CodexResponses is not an OpenAI Codex account route"
    );
    assert!(!resolution::is_ready_selectable_openai_codex_entry(
        &shim, false
    ));

    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert("third-party-codex".to_string(), shim);

    // No usable xAI → without the taxonomy fix this would seat the shim first.
    let (key, entry, _, _) = resolve_default_model(&config::Config::default(), &catalog, false);
    assert_eq!(
        key, "grok-4.5",
        "third-party CodexResponses must not displace bundled Grok as #303 default"
    );
    assert_ne!(
        entry.info.base_url,
        crate::auth::openai_codex::CODEX_API_BASE_URL
    );
}

/// Pro P0: blank primary + valid legacy still counts as ambient usable → Grok.
#[test]
fn blank_primary_valid_legacy_keeps_grok_when_codex_ready() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, "");
    let _l = EnvGuard::set(LEGACY_XAI_API_KEY_ENV_VAR, "legacy-live-key");

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );

    assert!(
        resolution::usable_ambient_xai_auth(&config::Config::default(), false),
        "blank primary + valid legacy must count as ambient xAI"
    );
    let (key, _, _, _) = resolve_default_model(&config::Config::default(), &catalog, false);
    assert_eq!(
        key, "grok-4.5",
        "legacy fallthrough must keep first-party Grok over Codex"
    );
}

/// Pro P0: blank / whitespace `XAI_API_KEY` must not keep Grok over ready Codex.
#[test]
fn blank_xai_env_does_not_pin_grok_when_codex_ready() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, "");
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let codex_key = crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string();
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(codex_key.clone(), codex);

    assert!(
        !resolution::usable_ambient_xai_auth(&config::Config::default(), false),
        "blank XAI_API_KEY must not count as usable ambient xAI"
    );
    let (key, _, _, _) = resolve_default_model(&config::Config::default(), &catalog, false);
    assert_eq!(
        key, codex_key,
        "blank env must seat ready Codex, not ambient Grok"
    );
}

/// Pro P0: whitespace-only primary is not ambient usable.
#[test]
fn whitespace_xai_env_does_not_pin_grok_when_codex_ready() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::set(XAI_API_KEY_ENV_VAR, "  \t ");
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let codex_key = crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string();
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(codex_key.clone(), codex);

    let (key, _, _, _) = resolve_default_model(&config::Config::default(), &catalog, false);
    assert_eq!(key, codex_key);
}

/// Pro P0: `[auth] preferred_method = api_key` keeps Grok even with ready Codex
/// and no live credential (avoids model/auth-surface contradiction).
#[test]
fn preferred_method_api_key_pin_keeps_first_party_when_codex_ready() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use crate::auth::PreferredAuthMethod;
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );

    let mut cfg = config::Config::default();
    cfg.grok_com_config.preferred_method = Some(PreferredAuthMethod::ApiKey);
    assert!(
        resolution::usable_ambient_xai_auth(&cfg, false),
        "api_key pin must preserve ambient xAI / Grok precedence"
    );
    let (key, _, _, _) = resolve_default_model(&cfg, &catalog, false);
    assert_eq!(
        key, "grok-4.5",
        "preferred_method=api_key must not seat Codex over first-party Grok"
    );
}

/// Pro P0: `[auth] preferred_method = oidc` same pin semantics as api_key.
#[test]
fn preferred_method_oidc_pin_keeps_first_party_when_codex_ready() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use crate::auth::PreferredAuthMethod;
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );

    let mut cfg = config::Config::default();
    cfg.grok_com_config.preferred_method = Some(PreferredAuthMethod::Oidc);
    let (key, _, _, _) = resolve_default_model(&cfg, &catalog, false);
    assert_eq!(key, "grok-4.5");
}

/// Pro P1: hard-expired OIDC with complete refresh surface keeps Grok over Codex.
#[test]
fn refreshable_expired_oidc_session_keeps_first_party_when_codex_ready() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use crate::auth::{AuthMode, FirstPartySessionEligibility, GrokAuth, XAI_OAUTH2_ISSUER};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );

    let xai_home = tmp.path().join("xai-refreshable");
    std::fs::create_dir_all(&xai_home).unwrap();
    let am = Arc::new(AuthManager::new(&xai_home, GrokComConfig::default()));
    let expired = GrokAuth {
        key: "expired-access".into(),
        auth_mode: AuthMode::Oidc,
        create_time: chrono::Utc::now() - chrono::Duration::hours(2),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        refresh_token: Some("rt-complete".into()),
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_owned()),
        oidc_client_id: Some("client".into()),
        user_id: "u".into(),
        ..GrokAuth::test_default()
    };
    // Use hot_swap for memory classification.
    am.hot_swap(expired);
    assert_eq!(
        am.first_party_session_eligibility(),
        FirstPartySessionEligibility::Refreshable
    );
    let cfg = config::Config::default();
    assert_eq!(
        resolution::classify_ambient_xai_auth(
            &cfg,
            am.first_party_session_eligibility(),
            am.first_party_env_api_key_ok(),
        ),
        resolution::AmbientXaiEligibility::RefreshableSession
    );

    let mgr = ModelsManagerBuilder::new(
        None,
        catalog,
        acp::ModelId::new("grok-4.5"),
        am,
        cfg.clone(),
    )
    .cache(test_cache_manager(tmp.path()))
    .build();
    // Warm path: stranded ambient Grok must stay when refreshable session exists.
    mgr.reselect_current_model_if_missing(&cfg);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4.5",
        "refreshable hard-expired OIDC must not reseat to Codex"
    );
}

/// A first-party External session refreshes by re-running the configured
/// provider command, not through OIDC discovery.  Its cached credential
/// therefore does not need an OIDC client_id or refresh_token to keep ambient
/// Grok precedence while the command can deterministically self-heal it.
#[test]
fn refreshable_expired_external_session_keeps_first_party_when_codex_ready() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use crate::auth::{AuthMode, FirstPartySessionEligibility, GrokAuth, XAI_OAUTH2_ISSUER};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string(),
        codex,
    );

    let xai_home = tmp.path().join("xai-external-refreshable");
    std::fs::create_dir_all(&xai_home).unwrap();
    let cfg = GrokComConfig {
        auth_provider_command: Some(
            "printf '%s' '{\"access_token\":\"fresh-external\",\"issuer\":\"https://auth.x.ai\"}'"
                .to_owned(),
        ),
        ..GrokComConfig::default()
    };
    let am = Arc::new(AuthManager::new(&xai_home, cfg));
    am.hot_swap(GrokAuth {
        key: "expired-external".into(),
        auth_mode: AuthMode::External,
        create_time: chrono::Utc::now() - chrono::Duration::hours(2),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        refresh_token: None,
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_owned()),
        oidc_client_id: None,
        user_id: "u".into(),
        ..GrokAuth::test_default()
    });
    assert_eq!(
        am.first_party_session_eligibility(),
        FirstPartySessionEligibility::Refreshable,
        "configured external provider command is the complete refresh authority"
    );

    let model_cfg = config::Config::default();
    let mgr = ModelsManagerBuilder::new(
        None,
        catalog,
        acp::ModelId::new("grok-4.5"),
        am,
        model_cfg.clone(),
    )
    .cache(test_cache_manager(tmp.path()))
    .build();
    mgr.reselect_current_model_if_missing(&model_cfg);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4.5",
        "refreshable hard-expired External session must not reseat to Codex"
    );
}

/// Pro P1 stack: `ModelsManager::from_config` (no CLI `--model`) seats ready
/// OpenAI Codex and produces a CodexResponses sampling config against the
/// official Codex base URL — not ambient Grok.
///
/// Uses production config presets (`merge_openai_codex_presets`) so the Codex
/// entry is a **canonical** account route; hand-stitched
/// `AuthProviderRef::openai_codex` on prefetched entries is fail-closed by
/// `resolve_model_list` outside that profile.
#[test]
fn codex_only_from_config_seats_codex_and_sampling_stack() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    // Live Codex credential file + GROK_AUTH_PATH pin (used by attached
    // openai-codex manager during resolve_model_list).
    let (_codex_fixture, _auth_path_pin) = ready_codex_entry(tmp.path());
    let codex_key = crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string();

    // Prefetch only ambient Grok first — Codex arrives from config presets so
    // it is marked canonical and keeps a working auth_provider attachment.
    let mut prefetched: IndexMap<String, ModelEntry> = IndexMap::new();
    prefetched.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));

    // Empty first-party xAI home — no ambient session on the models AuthManager.
    // Unset GROK_AUTH_PATH only for this AuthManager construction would steal
    // the Codex pin; instead use a dedicated path that has no xAI entry while
    // GROK_AUTH_PATH remains the Codex fixture for the provider attach step.
    let xai_home = tmp.path().join("xai-empty");
    std::fs::create_dir_all(&xai_home).unwrap();
    // Build xAI manager against xai_home *without* reading GROK_AUTH_PATH:
    // temporarily clear, construct, restore via drop order after pin lives.
    let auth = {
        let _clear_path = EnvGuard::unset("GROK_AUTH_PATH");
        Arc::new(AuthManager::new(&xai_home, GrokComConfig::default()))
    };
    // Re-pin Codex auth path for resolve_model_list provider attach.
    let _re_pin = EnvGuard::set(
        "GROK_AUTH_PATH",
        tmp.path()
            .join("auth.json")
            .to_str()
            .expect("utf-8 temp path"),
    );
    assert!(
        !auth.has_ambient_first_party_session(),
        "precondition: no ambient first-party session"
    );

    // Production config path includes openai-codex presets (not Config::default).
    let empty = toml::Value::Table(toml::map::Map::new());
    let cfg = config::Config::new_from_toml_cfg(&empty).expect("empty toml config");
    assert!(
        cfg.config_models.contains_key(codex_key.as_str())
            || cfg
                .config_models
                .values()
                .any(|m| m.model_provider.as_deref()
                    == Some(crate::agent::model_providers::OPENAI_CODEX_PROVIDER_ID)),
        "precondition: config must carry openai-codex preset after merge"
    );

    let mgr = ModelsManager::from_config_with_remote_fetch(&cfg, Some(prefetched), auth, false)
        .expect("from_config with prefetched catalog must succeed offline");

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        codex_key.as_str(),
        "from_config cold start without --model must seat ready Codex, not Grok"
    );

    let models = mgr.models();
    let current = models
        .get(codex_key.as_str())
        .expect("seated Codex remains in catalog");
    assert!(
        resolution::is_ready_selectable_openai_codex_entry(current, false),
        "seated entry must be the shared OpenAI Codex account predicate"
    );
    assert!(
        current.auth_provider.is_some(),
        "Codex account route carries provider-scoped bearer, not XAI_API_KEY"
    );

    let sampling = mgr.sampling_config();
    assert_eq!(
        sampling.api_backend,
        crate::sampling::ApiBackend::CodexResponses,
        "sampling stack must use CodexResponses backend"
    );
    assert_eq!(
        sampling.base_url.trim_end_matches('/'),
        crate::auth::openai_codex::CODEX_API_BASE_URL.trim_end_matches('/'),
        "sampling destination must be official Codex API base"
    );
    assert_eq!(
        sampling.model.as_str(),
        crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID,
        "sampling model id must match seated catalog wire model"
    );
    assert!(
        !sampling.base_url.is_empty(),
        "ready Codex sampling config must keep its endpoint"
    );
    assert!(
        sampling.api_key.is_none(),
        "production Codex construction must not freeze raw credential bytes in api_key"
    );
    assert_eq!(
        sampling.credential_source,
        Some(crate::sampling::CredentialSource::AuthProvider {
            name: crate::agent::model_providers::OPENAI_CODEX_PROVIDER_ID.to_owned(),
        }),
        "production Codex construction must retain provider-scoped auth provenance"
    );
    let resolver = sampling
        .bearer_resolver
        .as_ref()
        .expect("production Codex construction carries a live provider resolver");
    let credential = resolver
        .current_credential()
        .expect("provider resolver exposes the structured Codex credential");
    assert_eq!(credential.access_token, "live-codex-token");
    assert_eq!(
        credential.account_id, None,
        "persisted workspace identity must be normalized from trusted JWT claims"
    );

    for name in sampling.extra_headers.keys() {
        let name = name.to_ascii_lowercase();
        assert!(
            name != "authorization"
                && name != "chatgpt-account-id"
                && name != "x-api-key"
                && !name.starts_with("x-xai-")
                && !name.starts_with("x-grok-"),
            "production Codex construction carried identity header {name} outside its resolver"
        );
    }
    for name in [
        "x-api-key",
        "x-xai-token-auth",
        "x-grok-conv-id",
        "x-grok-req-id",
        "x-grok-session-id",
        "x-grok-agent-id",
        "x-grok-turn-idx",
        "x-grok-deployment-id",
        "x-grok-user-id",
        "x-grok-client-version",
        "x-grok-doom-loop-check",
        "x-compactions-remaining",
    ] {
        assert!(
            !sampling.extra_headers.contains_key(name),
            "production Codex construction retained xAI-only header {name}"
        );
    }
    crate::sampling::Client::new(sampling.clone())
        .expect("production Codex sampling config must construct only for the official endpoint");
}

/// Pro P1: hard-expired session without complete refresh surface seats Codex.
#[test]
fn hard_expired_nonrefreshable_session_seats_codex() {
    let _serial = CODEX_ONLY_DEFAULT_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use crate::auth::{AuthMode, FirstPartySessionEligibility, GrokAuth};
    use xai_grok_test_support::EnvGuard;
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let tmp = tempfile::tempdir().expect("temp home");
    let (codex, _auth_path_pin) = ready_codex_entry(tmp.path());
    let codex_key = crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID.to_string();
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert("grok-4.5".to_string(), ready_entry("grok-4.5"));
    catalog.insert(codex_key.clone(), codex);

    let xai_home = tmp.path().join("xai-dead");
    std::fs::create_dir_all(&xai_home).unwrap();
    let am = Arc::new(AuthManager::new(&xai_home, GrokComConfig::default()));
    // RT present but no issuer/client — malformed for ambient self-heal.
    let dead = GrokAuth {
        key: "expired-access".into(),
        auth_mode: AuthMode::Oidc,
        create_time: chrono::Utc::now() - chrono::Duration::hours(2),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        refresh_token: Some("rt-incomplete".into()),
        oidc_issuer: None,
        oidc_client_id: None,
        user_id: "u".into(),
        ..GrokAuth::test_default()
    };
    am.hot_swap(dead);
    assert_eq!(
        am.first_party_session_eligibility(),
        FirstPartySessionEligibility::None
    );

    let cfg = config::Config::default();
    assert_eq!(
        resolution::classify_ambient_xai_auth(
            &cfg,
            am.first_party_session_eligibility(),
            am.first_party_env_api_key_ok(),
        ),
        resolution::AmbientXaiEligibility::Unavailable
    );

    let mgr = ModelsManagerBuilder::new(
        None,
        catalog,
        acp::ModelId::new("grok-4.5"),
        am,
        cfg.clone(),
    )
    .cache(test_cache_manager(tmp.path()))
    .build();
    mgr.reselect_current_model_if_missing(&cfg);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        codex_key.as_str(),
        "hard-expired non-refreshable session must reseat ambient Grok to Codex"
    );
}
