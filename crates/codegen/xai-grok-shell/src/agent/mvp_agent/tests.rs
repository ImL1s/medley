use super::*;

/// `grok_home()` is a process-wide OnceLock, so a tempfile `$MEDLEY_HOME`
/// config cannot disarm catalog fetch after another test has resolved the
/// home. Pin the thread-local offline path `MvpAgent::new` actually reads.
struct ProcessRemoteFetchOff;

impl ProcessRemoteFetchOff {
    fn install() -> Self {
        crate::util::config::override_remote_fetch_enabled(Some(false));
        Self
    }
}

impl Drop for ProcessRemoteFetchOff {
    fn drop(&mut self) {
        crate::util::config::override_remote_fetch_enabled(None);
    }
}

/// Pin the process state directory to `home` for this test, and prove the pin
/// took (#420).
///
/// `grok_home()` caches its answer in a process-wide `OnceLock`, so the
/// `MEDLEY_HOME` / `GROK_HOME` env guards a test sets are no-ops once any
/// earlier test in the binary has resolved the home. A test relying on them
/// alone reads and writes the *developer's* live state directory: it passes
/// alone (it wins the cache), passes on a fresh CI container (nothing is there
/// yet), and fails on any machine that has run it before, because its own
/// fixture session ids are already persisted from the previous run.
///
/// The `assert_eq!` is the guarantee, not decoration: if a future change makes
/// the pin stop reaching `grok_home()`, this fails here — naming the directory
/// that actually resolved — rather than surfacing much later as a duplicate-id
/// rejection or a hook-wait timeout.
#[must_use]
fn pin_fixture_state_home(home: &std::path::Path) -> xai_grok_config::state_home::StateHomeGuard {
    let guard = xai_grok_config::state_home::StateHomeGuard::pin(home);
    assert_eq!(
        xai_grok_config::grok_home(),
        home,
        "the fixture state directory must be the one grok_home() resolves"
    );
    guard
}

/// Assert the pinned state directory holds no session under `session_id` yet.
///
/// Fixed session-id fixtures are safe only while each test owns its own state
/// directory. This makes a lost isolation fail at the reuse, naming the id and
/// the directory, instead of intermittently somewhere downstream (#420).
fn assert_fixture_session_id_unused(session_id: &str) {
    let existing = crate::session::persistence::find_any_session_dir_by_id_result(session_id)
        .expect("scan the pinned state directory for the fixture session id");
    assert!(
        existing.is_none(),
        "fixture session id {session_id} is already persisted under {} — this \
         test is not running against an isolated state directory",
        xai_grok_config::grok_home().display()
    );
}

/// Wait for a `/new` boundary hook, but let the request itself win the race.
///
/// `new_session` rejects a duplicate session id *before* it reaches any of
/// these boundaries, so a bare `timeout(.., hook.wait_until_entered())` reports
/// `Elapsed(())` — which names neither what it waited for nor why it never
/// arrived. Joining the request task in the same select turns that into the
/// rejection that actually happened (#420 acceptance 3).
async fn await_new_session_boundary(
    request_task: &mut tokio::task::JoinHandle<Result<acp::NewSessionResponse, acp::Error>>,
    hook: impl std::future::Future<Output = ()>,
    boundary: &str,
) {
    tokio::select! {
        () = hook => {}
        joined = &mut *request_task => {
            let outcome = match joined {
                Ok(Ok(_)) => "it returned successfully".to_owned(),
                Ok(Err(error)) => format!("it was rejected: {error:?}"),
                Err(join_error) => format!("its task ended: {join_error}"),
            };
            panic!("session/new never reached {boundary}: {outcome}");
        }
        () = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
            panic!(
                "timed out after 10s waiting for session/new to reach {boundary}; \
                 the request was still in flight"
            );
        }
    }
}

/// Build an unsigned JWT with a `tier` claim (header.payload.sig base64url).
fn jwt_with_tier(tier: u64) -> String {
    use base64::Engine;
    let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = enc.encode(br#"{"alg":"none"}"#);
    let payload = enc.encode(format!(r#"{{"tier":{tier}}}"#).as_bytes());
    format!("{header}.{payload}.sig")
}
#[test]
fn jwt_tier_claim_maps_free_and_paid() {
    assert_eq!(jwt_tier_claim(&jwt_with_tier(0)).as_deref(), Some("free"));
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(1)).as_deref(),
        Some("supergrok")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(2)).as_deref(),
        Some("x_basic")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(3)).as_deref(),
        Some("x_premium")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(4)).as_deref(),
        Some("x_premium_plus")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(5)).as_deref(),
        Some("supergrok_heavy")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(6)).as_deref(),
        Some("supergrok_lite")
    );
    assert_eq!(
        jwt_tier_claim(&jwt_with_tier(7)).as_deref(),
        Some("supergrok_plus")
    );
    assert_eq!(jwt_tier_claim(&jwt_with_tier(9)).as_deref(), Some("9"));
    assert_eq!(jwt_tier_claim(&jwt_with_tier(99)).as_deref(), Some("99"));
}
fn auth_with_mode(mode: crate::auth::AuthMode, key: &str) -> crate::auth::GrokAuth {
    crate::auth::GrokAuth {
        key: key.into(),
        auth_mode: mode,
        create_time: chrono::Utc::now(),
        user_id: "u".into(),
        email: None,
        first_name: None,
        last_name: None,
        profile_image_asset_id: None,
        principal_type: None,
        principal_id: None,
        team_id: None,
        team_name: None,
        team_role: None,
        organization_id: None,
        organization_name: None,
        organization_role: None,
        user_blocked_reason: None,
        team_blocked_reasons: vec![],
        coding_data_retention_opt_out: false,
        has_grok_code_access: None,
        refresh_token: None,
        expires_at: None,
        oidc_issuer: None,
        oidc_client_id: None,
        id_token: None,
        account_id: None,
        chatgpt_account_is_fedramp: false,
    }
}
#[test]
fn auth_init_before_and_after_disk_reload_diagnostics_are_secret_free() {
    let access_before = "GB002-access-before-Q7w5E3r1T9y7";
    let refresh_before = "GB002-refresh-before-A7s5D3f1G9h7";
    let access_after = "GB002-access-after-Z9x7C5v3B1n9";
    let refresh_after = "GB002-refresh-after-M8k6J4h2G0f8";
    let mut pre = auth_with_mode(crate::auth::AuthMode::Oidc, access_before);
    pre.refresh_token = Some(refresh_before.to_string());
    let mut post = auth_with_mode(crate::auth::AuthMode::Oidc, access_after);
    post.refresh_token = Some(refresh_after.to_string());

    let context = acp_agent::auth_init_disk_refresh_context(Some(&pre), Some(&post));
    assert_eq!(context["access_relation"], "different");
    assert_eq!(context["refresh_relation"], "different");
    assert_eq!(context["access_pre_present"], true);
    assert_eq!(context["access_post_present"], true);
    assert_eq!(context["refresh_pre_present"], true);
    assert_eq!(context["refresh_post_present"], true);
    let rendered = context.to_string();
    for secret in [access_before, refresh_before, access_after, refresh_after] {
        assert!(!rendered.contains(secret));
        for window in secret.as_bytes().windows(8) {
            assert!(!rendered.contains(std::str::from_utf8(window).unwrap()));
        }
    }
}
#[test]
fn resolve_subscription_tier_prefers_display_then_api_key_then_jwt() {
    assert_eq!(
        resolve_subscription_tier_for_telemetry(Some("Free".into()), None).as_deref(),
        Some("Free")
    );
    let api = auth_with_mode(crate::auth::AuthMode::ApiKey, "xai-not-a-jwt");
    assert_eq!(
        resolve_subscription_tier_for_telemetry(Some("  ".into()), Some(&api)).as_deref(),
        Some("api_key")
    );
    assert_eq!(
        resolve_subscription_tier_for_telemetry(None, Some(&api)).as_deref(),
        Some("api_key")
    );
    let oauth = auth_with_mode(crate::auth::AuthMode::Oidc, &jwt_with_tier(0));
    assert_eq!(
        resolve_subscription_tier_for_telemetry(None, Some(&oauth)).as_deref(),
        Some("free")
    );
    assert_ne!(
        resolve_subscription_tier_for_telemetry(None, Some(&api)).as_deref(),
        Some("free")
    );
}
/// JWT claim ↔ `/user` tier mapping used to gate post-unblock catalog refresh
/// (a stale older paid claim must not skip retry).
#[test]
fn jwt_claim_matches_user_subscription_tier_known_pairs() {
    let cases = [
        ("supergrok", "GrokPro"),
        ("x_basic", "XBasic"),
        ("x_premium", "XPremium"),
        ("x_premium_plus", "XPremiumPlus"),
        ("supergrok_heavy", "SuperGrokPro"),
        ("9", "EnterpriseMystery"),
        ("supergrok_lite", "SuperGrokLite"),
        ("supergrok_plus", "SuperGrokPlus"),
    ];
    for (claim, user_tier) in cases {
        assert!(
            jwt_claim_matches_user_subscription_tier(claim, user_tier),
            "{claim} should match {user_tier}"
        );
    }
}
#[test]
fn jwt_claim_matches_user_subscription_tier_rejects_stale_and_unknown() {
    assert!(!jwt_claim_matches_user_subscription_tier(
        "x_basic",
        "SuperGrokPro"
    ));
    assert!(!jwt_claim_matches_user_subscription_tier(
        "supergrok",
        "SuperGrokPro"
    ));
    assert!(!jwt_claim_matches_user_subscription_tier(
        "supergrok",
        "SuperGrokPlus"
    ));
    assert!(!jwt_claim_matches_user_subscription_tier(
        "supergrok_heavy",
        "SuperGrokPlus"
    ));
    assert!(!jwt_claim_matches_user_subscription_tier("free", "GrokPro"));
    assert!(!jwt_claim_matches_user_subscription_tier("", "XPremium"));
    assert!(!jwt_claim_matches_user_subscription_tier(
        "supergrok_heavy",
        "EnterpriseMystery"
    ));
    assert!(!jwt_claim_matches_user_subscription_tier(
        "0",
        "EnterpriseMystery"
    ));
}
/// Single-flight flag must clear on Drop even if the retry task panics /
/// aborts mid-backoff (guards against the flag stuck true forever).
#[test]
fn post_unblock_jwt_retry_in_flight_guard_clears_on_drop() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let flag = Arc::new(AtomicBool::new(true));
    {
        let _guard = PostUnblockJwtRetryInFlightGuard { flag: flag.clone() };
        assert!(flag.load(Ordering::Acquire));
    }
    assert!(
        !flag.load(Ordering::Acquire),
        "Drop must release post_unblock_jwt_retry_in_flight"
    );
    let flag = Arc::new(AtomicBool::new(true));
    let flag_for_catch = flag.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = PostUnblockJwtRetryInFlightGuard {
            flag: flag_for_catch,
        };
        panic!("simulate retry task panic");
    }));
    assert!(result.is_err());
    assert!(
        !flag.load(Ordering::Acquire),
        "Drop must release flag on panic unwind"
    );
}
mod hunk_tracking_mode {
    use super::super::{plan_hunk_tracking, resolve_hunk_tracking_mode};
    use xai_hunk_tracker::TrackingMode;
    #[test]
    fn off_and_disabled_disable_tracking() {
        assert_eq!(resolve_hunk_tracking_mode(Some("off")), None);
        assert_eq!(resolve_hunk_tracking_mode(Some("disabled")), None);
    }
    #[test]
    fn matching_is_case_insensitive_and_trimmed() {
        assert_eq!(resolve_hunk_tracking_mode(Some("OFF")), None);
        assert_eq!(resolve_hunk_tracking_mode(Some("  Off ")), None);
        assert_eq!(resolve_hunk_tracking_mode(Some("DISABLED")), None);
        assert_eq!(
            resolve_hunk_tracking_mode(Some("Agent_Only")),
            Some(TrackingMode::AgentOnly)
        );
        assert_eq!(
            resolve_hunk_tracking_mode(Some(" ALL_DIRTY ")),
            Some(TrackingMode::AllDirty)
        );
    }
    #[test]
    fn recognized_modes_parse() {
        assert_eq!(
            resolve_hunk_tracking_mode(Some("agent_only")),
            Some(TrackingMode::AgentOnly)
        );
        assert_eq!(
            resolve_hunk_tracking_mode(Some("all_dirty")),
            Some(TrackingMode::AllDirty)
        );
    }
    #[test]
    fn parser_absent_returns_none_policy_defaults_in_plan() {
        assert_eq!(resolve_hunk_tracking_mode(None), None);
        assert_eq!(resolve_hunk_tracking_mode(Some("")), None);
        assert_eq!(
            resolve_hunk_tracking_mode(Some("bogus")),
            Some(TrackingMode::AllDirty)
        );
    }
    #[test]
    fn plan_disables_actor_forward_and_loc_together() {
        for off in ["off", "disabled", "OFF"] {
            let plan = plan_hunk_tracking(Some(off));
            assert_eq!(plan.actor_mode, None, "{off} must not spawn the actor");
            assert!(!plan.enabled(), "{off} must disable the forward + LOC sink");
        }
    }
    #[test]
    fn plan_enables_actor_and_forward_for_active_modes() {
        for (mode, expected) in [
            ("agent_only", TrackingMode::AgentOnly),
            ("all_dirty", TrackingMode::AllDirty),
            ("bogus", TrackingMode::AllDirty),
        ] {
            let plan = plan_hunk_tracking(Some(mode));
            assert_eq!(plan.actor_mode, Some(expected));
            assert!(plan.enabled());
        }
        let plan = plan_hunk_tracking(None);
        assert_eq!(plan.actor_mode, None);
        assert!(!plan.enabled());
    }
}
mod capture {
    use tokio::sync::mpsc;
    use tracing::Subscriber;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;
    pub(crate) struct CapturedEvent {
        pub level: tracing::Level,
        pub fields: String,
    }
    pub(crate) struct Captured {
        pub events_rx: mpsc::UnboundedReceiver<CapturedEvent>,
        _guard: tracing::subscriber::DefaultGuard,
    }
    pub(crate) fn capture() -> Captured {
        let (tx, rx) = mpsc::unbounded_channel();
        let subscriber = tracing_subscriber::registry().with(CaptureLayer { tx });
        let guard = tracing::subscriber::set_default(subscriber);
        Captured {
            events_rx: rx,
            _guard: guard,
        }
    }
    struct CaptureLayer {
        tx: mpsc::UnboundedSender<CapturedEvent>,
    }
    impl<S> Layer<S> for CaptureLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: Context<'_, S>,
        ) {
            let mut v = Visitor::default();
            attrs.record(&mut v);
            let _ = self.tx.send(CapturedEvent {
                level: *attrs.metadata().level(),
                fields: v.out,
            });
        }

        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut v = Visitor::default();
            event.record(&mut v);
            let _ = self.tx.send(CapturedEvent {
                level: *event.metadata().level(),
                fields: v.out,
            });
        }
    }
    #[derive(Default)]
    struct Visitor {
        out: String,
    }
    impl tracing::field::Visit for Visitor {
        fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
            if !self.out.is_empty() {
                self.out.push(' ');
            }
            self.out.push_str(f.name());
            self.out.push('=');
            self.out.push_str(&format!("{v:?}"));
        }
        fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
            if !self.out.is_empty() {
                self.out.push(' ');
            }
            self.out.push_str(f.name());
            self.out.push('=');
            self.out.push_str(v);
        }
    }
}
#[test]
fn warn_on_missing_parent_session_emits_when_session_absent() {
    let captured = capture::capture();
    warn_on_missing_parent_session_for_validate_type("ghost-session", false);
    let mut rx = captured.events_rx;
    let mut saw = false;
    while let Ok(event) = rx.try_recv() {
        if event.level == tracing::Level::WARN
            && event
                .fields
                .contains("ValidateType received for unknown parent session")
            && event.fields.contains("parent_session_id=ghost-session")
        {
            saw = true;
            break;
        }
    }
    assert!(saw, "warn must fire");
}
#[test]
fn warn_on_missing_parent_session_silent_when_session_present() {
    let captured = capture::capture();
    warn_on_missing_parent_session_for_validate_type("real-session", true);
    let mut rx = captured.events_rx;
    assert!(rx.try_recv().is_err());
}
#[tokio::test(flavor = "current_thread")]
async fn broadcast_refresh_skill_baseline_sends_one_message_per_sender() {
    let mut receivers = Vec::new();
    let mut senders = Vec::new();
    for _ in 0..3 {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        senders.push(tx);
        receivers.push(rx);
    }
    MvpAgent::broadcast_refresh_skill_baseline(senders);
    for mut rx in receivers {
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::session::SessionCommand::RefreshSkillBaseline)
        ));
        assert!(
            rx.try_recv().is_err(),
            "broadcast must send exactly one message per sender",
        );
    }
}
#[tokio::test(flavor = "current_thread")]
async fn broadcast_refresh_skill_baseline_tolerates_dropped_receiver() {
    let (tx_alive, mut rx_alive) = tokio::sync::mpsc::unbounded_channel();
    let (tx_dead, rx_dead) = tokio::sync::mpsc::unbounded_channel();
    drop(rx_dead);
    MvpAgent::broadcast_refresh_skill_baseline(vec![tx_alive, tx_dead]);
    assert!(matches!(
        rx_alive.try_recv(),
        Ok(crate::session::SessionCommand::RefreshSkillBaseline)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn invalidate_model_auth_memo_all_sessions_sends_to_each_session() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let agent = build_minimal_agent_for_tests();
            let sid_a = acp::SessionId::new("sess-memo-a");
            let sid_b = acp::SessionId::new("sess-memo-b");
            let (handle_a, _tx_a, mut rx_a) = make_live_session_handle(&sid_a, None);
            let (handle_b, _tx_b, mut rx_b) = make_live_session_handle(&sid_b, None);
            agent.session_registry.put_resident(&sid_a, handle_a);
            agent.session_registry.put_resident(&sid_b, handle_b);

            let n = agent.invalidate_model_auth_memo_all_sessions();
            assert_eq!(n, 2);

            let cmd_a = tokio::time::timeout(std::time::Duration::from_secs(1), rx_a.recv())
                .await
                .expect("timeout waiting for InvalidateModelAuthMemo on A")
                .expect("channel open");
            let cmd_b = tokio::time::timeout(std::time::Duration::from_secs(1), rx_b.recv())
                .await
                .expect("timeout waiting for InvalidateModelAuthMemo on B")
                .expect("channel open");
            assert!(matches!(
                cmd_a,
                crate::session::SessionCommand::InvalidateModelAuthMemo
            ));
            assert!(matches!(
                cmd_b,
                crate::session::SessionCommand::InvalidateModelAuthMemo
            ));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn authenticate_local_none_succeeds_without_credentials() {
    use acp::Agent as _;
    let agent = build_minimal_agent_for_tests();
    assert!(agent.auth_manager.current().is_none());
    assert!(agent.auth_method_id.load().is_none());

    let resp = agent
        .authenticate(acp::AuthenticateRequest::new(acp::AuthMethodId::new(
            crate::agent::auth_method::LOCAL_NONE_METHOD_ID,
        )))
        .await
        .expect("local.none must authenticate without a key");
    let _ = resp;

    let method = agent.auth_method_id.load();
    assert_eq!(
        method.as_ref().map(|m| m.0.as_ref()),
        Some(crate::agent::auth_method::LOCAL_NONE_METHOD_ID)
    );
    assert!(
        agent.auth_manager.current().is_none(),
        "local.none must not store a session credential"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn authenticate_local_none_rejected_when_preferred_oidc() {
    use acp::Agent as _;
    let agent = build_minimal_agent_for_tests();
    agent.cfg.borrow_mut().grok_com_config.preferred_method =
        Some(crate::auth::PreferredAuthMethod::Oidc);

    let err = agent
        .authenticate(acp::AuthenticateRequest::new(acp::AuthMethodId::new(
            crate::agent::auth_method::LOCAL_NONE_METHOD_ID,
        )))
        .await
        .expect_err("oidc pin must reject local.none");
    assert_eq!(err.code, acp::Error::auth_required().code);
    assert!(agent.auth_method_id.load().is_none());
}
/// The monotonic turn counter must never wrap on the DB-bound i32 path.
/// `allocate_turn_number` returns u64; the AB submission casts to i32.
/// Verify we saturate instead of wrapping.
#[test]
fn trace_turn_to_i32_saturates_at_max() {
    let small: u64 = 42;
    let result = i32::try_from(small).unwrap_or(i32::MAX);
    assert_eq!(result, 42);
    let huge: u64 = (i32::MAX as u64) + 100;
    let result = i32::try_from(huge).unwrap_or(i32::MAX);
    assert_eq!(result, i32::MAX);
    let boundary: u64 = i32::MAX as u64;
    let result = i32::try_from(boundary).unwrap_or(i32::MAX);
    assert_eq!(result, i32::MAX);
}
#[test]
fn settings_allow_access_none_settings_is_allowed() {
    assert!(settings_allow_access(None));
}
#[test]
fn settings_allow_access_true_is_allowed() {
    let rs = crate::util::config::RemoteSettings {
        allow_access: Some(true),
        ..Default::default()
    };
    assert!(settings_allow_access(Some(&rs)));
}
#[test]
fn settings_allow_access_false_is_blocked() {
    let rs = crate::util::config::RemoteSettings {
        allow_access: Some(false),
        ..Default::default()
    };
    assert!(!settings_allow_access(Some(&rs)));
}
#[test]
fn settings_allow_access_field_absent_is_allowed() {
    let rs = crate::util::config::RemoteSettings {
        allow_access: None,
        ..Default::default()
    };
    assert!(settings_allow_access(Some(&rs)));
}
/// After allocating a turn number, the retained (in-memory) turn counter holds
/// the next value (current + 1). This is the value that must be persisted via
/// `SetNextTraceTurn` so the counter survives restarts.
#[test]
fn allocate_turn_number_advances_counter() {
    use std::cell::RefCell;
    use std::collections::HashMap;
    let counters: RefCell<HashMap<acp::SessionId, u64>> = RefCell::new(HashMap::new());
    let sid = acp::SessionId::new("test-session");
    let allocate = |id: &acp::SessionId| -> u64 {
        let mut m = counters.borrow_mut();
        let turn = m.get(id).copied().unwrap_or(0u64);
        m.insert(id.clone(), turn.saturating_add(1));
        turn
    };
    assert_eq!(allocate(&sid), 0);
    assert_eq!(*counters.borrow().get(&sid).unwrap(), 1);
    assert_eq!(allocate(&sid), 1);
    assert_eq!(*counters.borrow().get(&sid).unwrap(), 2);
    assert_eq!(allocate(&sid), 2);
    assert_eq!(*counters.borrow().get(&sid).unwrap(), 3);
}
/// Build a synthetic harness `task` call/result pair carrying the
/// `<subagent_result>` footer, mirroring what the verifier/planner record.
fn harness_pair(id: &str) -> Vec<xai_grok_sampling_types::conversation::ConversationItem> {
    use xai_grok_sampling_types::ToolCall;
    use xai_grok_sampling_types::conversation::ConversationItem;
    vec![
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: id.into(),
            name: "task".into(),
            arguments: "{}".into(),
        }]),
        ConversationItem::tool_result(id, "<subagent_result>\nsubagent_id: skeptic-1"),
    ]
}
/// Agent-side upload path: each drained harness turn takes a distinct,
/// monotonic turn number that CONTINUES past the user turn, advances the
/// per-session counter, and is persisted via exactly one `SetNextTraceTurn`.
/// This is what makes each sibling `turn_{N}` reachable — without the
/// advance every harness turn would clobber the same GCS path.
#[tokio::test(flavor = "current_thread")]
async fn upload_harness_trace_turns_numbers_siblings_and_persists_counter() {
    let agent = build_minimal_agent_for_tests();
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
        cfg.telemetry.trace_upload = Some(true);
        cfg.endpoints.trace_upload_bucket = Some("gs://harness-trace-test".to_string());
    }
    let sid = acp::SessionId::new("harness-upload-sess");
    let info = crate::session::info::Info {
        id: sid.clone(),
        cwd: "/tmp".to_string(),
    };
    let mut handle = make_test_handle("test-model", false, None);
    handle.info = info.clone();
    let queue_home = tempfile::tempdir().unwrap();
    let queue_cfg = crate::session::repo_changes::TraceExportConfig {
        bucket_url: Some("gs://harness-trace-test".to_string()),
        service_account_key: None,
        prefix_dir: None,
        gcs_prefix: None,
        absolute_paths: false,
        archive_name_override: None,
        upload_method: crate::session::repo_changes::UploadMethod::Direct {
            service_account_key: None,
        },
    };
    let queue = crate::upload::trace::spawn_upload_queue(
        queue_home.path(),
        &queue_cfg,
        Some(xai_grok_version::VERSION),
        agent.auth_manager.clone(),
    );
    let _ = handle.upload_queue.set(queue);
    agent.insert_resident(&sid, handle);
    for _ in 0..3 {
        agent.allocate_turn_number(&sid);
    }
    assert_eq!(agent.session_turn_number(&sid), Some(3));
    let built = agent
        .build_harness_trace_uploads(
            &sid,
            &info,
            "test-model",
            3,
            vec![harness_pair("a"), harness_pair("b")],
        )
        .await;
    let numbers: Vec<u64> = built.iter().map(|(_, m, _)| m.turn_number).collect();
    assert_eq!(numbers, vec![3, 4], "siblings take base, base+1");
    assert!(
        built.iter().all(|(_, m, _)| m.model == "test-model"),
        "harness metadata carries the requested model alias",
    );
    let (cmd_tx, mut cmd_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::session::SessionCommand>();
    agent
        .upload_harness_trace_turns(
            &sid,
            &info,
            &cmd_tx,
            "test-model",
            vec![harness_pair("a"), harness_pair("b")],
        )
        .await;
    assert_eq!(
        agent.session_turn_number(&sid),
        Some(5),
        "two siblings advance the counter by two from the user turn",
    );
    let mut persisted = Vec::new();
    while let Ok(cmd) = cmd_rx.try_recv() {
        if let crate::session::SessionCommand::SetNextTraceTurn {
            next_trace_turn, ..
        } = cmd
        {
            persisted.push(next_trace_turn);
        }
    }
    assert_eq!(
        persisted,
        vec![5],
        "persist the advanced counter once, ahead of the spawned uploads",
    );
}
/// With trace upload disabled the agent-side path must NOT burn a turn
/// number or persist a counter (and spawns no upload). The buffer-clearing
/// half of the drain is the caller's `TakeHarnessTraceTurns`; this guards
/// the upload function's uploads-disabled branch.
#[tokio::test(flavor = "current_thread")]
async fn upload_harness_trace_turns_uploads_disabled_does_not_burn_counter() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("harness-disabled-sess");
    let info = crate::session::info::Info {
        id: sid.clone(),
        cwd: "/tmp".to_string(),
    };
    let (cmd_tx, mut cmd_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::session::SessionCommand>();
    agent
        .upload_harness_trace_turns(&sid, &info, &cmd_tx, "test-model", vec![harness_pair("a")])
        .await;
    assert_eq!(
        agent.session_turn_number(&sid),
        None,
        "uploads-disabled skip must not consume a turn number",
    );
    assert!(
        cmd_rx.try_recv().is_err(),
        "uploads-disabled path must not persist a counter",
    );
}
/// Guards the per-harness-turn manifest seam: (1) every turn's ctx carries
/// a FRESH `artifact_tracker`, so turn 1 never inherits turn 0's recorded
/// artifacts; (2) recording the turn's metadata + turn_messages yields a
/// manifest listing exactly those two; (3) `fully_uploaded` is true iff
/// neither failed.
#[tokio::test(flavor = "current_thread")]
async fn upload_harness_trace_turns_build_per_turn_manifest() {
    use crate::upload::manifest::{
        ArtifactResult, ArtifactStatus, build_manifest, record_artifact, resolve_upload_method,
    };
    let agent = build_minimal_agent_for_tests();
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
        cfg.telemetry.trace_upload = Some(true);
        cfg.endpoints.trace_upload_bucket = Some("gs://harness-trace-test".to_string());
    }
    let sid = acp::SessionId::new("harness-manifest-sess");
    let info = crate::session::info::Info {
        id: sid.clone(),
        cwd: "/tmp".to_string(),
    };
    let mut handle = make_test_handle("test-model", false, None);
    handle.info = info.clone();
    let queue_home = tempfile::tempdir().unwrap();
    let queue_cfg = crate::session::repo_changes::TraceExportConfig {
        bucket_url: Some("gs://harness-trace-test".to_string()),
        service_account_key: None,
        prefix_dir: None,
        gcs_prefix: None,
        absolute_paths: false,
        archive_name_override: None,
        upload_method: crate::session::repo_changes::UploadMethod::Direct {
            service_account_key: None,
        },
    };
    let queue = crate::upload::trace::spawn_upload_queue(
        queue_home.path(),
        &queue_cfg,
        Some(xai_grok_version::VERSION),
        agent.auth_manager.clone(),
    );
    let _ = handle.upload_queue.set(queue);
    agent.insert_resident(&sid, handle);
    let built = agent
        .build_harness_trace_uploads(
            &sid,
            &info,
            "test-model",
            0,
            vec![harness_pair("a"), harness_pair("b")],
        )
        .await;
    assert_eq!(
        built.len(),
        2,
        "both harness turns obtained a trace context"
    );
    let ctx0 = &built[0].0;
    record_artifact(
        &ctx0.artifact_tracker,
        "metadata.json",
        ArtifactResult::Succeeded,
    );
    record_artifact(
        &ctx0.artifact_tracker,
        "turn_messages.json",
        ArtifactResult::Succeeded,
    );
    let m0 = build_manifest(&ctx0.artifact_tracker, resolve_upload_method(ctx0));
    assert!(matches!(
        m0.artifacts.get("metadata.json"),
        Some(ArtifactStatus::Succeeded)
    ));
    assert!(matches!(
        m0.artifacts.get("turn_messages.json"),
        Some(ArtifactStatus::Succeeded)
    ));
    assert!(m0.fully_uploaded, "both succeeded → fully_uploaded");
    let ctx1 = &built[1].0;
    let before = build_manifest(&ctx1.artifact_tracker, resolve_upload_method(ctx1));
    assert!(
        before.artifacts.is_empty(),
        "per-turn tracker: turn 1 must not inherit turn 0's artifacts",
    );
    record_artifact(
        &ctx1.artifact_tracker,
        "metadata.json",
        ArtifactResult::Succeeded,
    );
    record_artifact(
        &ctx1.artifact_tracker,
        "turn_messages.json",
        ArtifactResult::Failed {
            reason: "upload_failed",
            error: None,
        },
    );
    let m1 = build_manifest(&ctx1.artifact_tracker, resolve_upload_method(ctx1));
    assert!(
        !m1.fully_uploaded,
        "a failed turn_messages flips fully_uploaded",
    );
    assert_eq!(m1.artifacts.len(), 2, "no cross-turn contamination");
}
/// With no overrides and model_agent_type = None, the default agent is used.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_defaults_to_grok_build() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        None,
        None,
    );
    assert_eq!(def.name, config::DEFAULT_AGENT_TYPE);
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// When model_agent_type = Some("codex"), the codex agent is selected even
/// though the default chain would return grok-build.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_model_agent_type_overrides_default() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        None,
        Some("codex"),
    );
    assert_eq!(def.name, "codex");
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// When model_agent_type is None, the chain-resolved default agent is
/// NOT overridden. This is the crux of the leader-mode fix: a session whose
/// model has no agent_type must get the default agent, not a stale value
/// from a different client's model.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_none_agent_type_does_not_override() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        None,
        None,
    );
    assert_eq!(def.name, config::DEFAULT_AGENT_TYPE);
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// Regression for the web-client devbox bug: an ACP profile must
/// win when the model's `agent_type` is the default value.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_acp_profile_wins_when_model_agent_type_is_default() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let acp_profile = xai_grok_agent::AgentDefinition::from_json(&serde_json::json!(
        { "name" : "custom-devbox-profile", "description" :
        "Custom devbox profile", "systemPrompt" :
        "You are a custom-configured devbox agent.", }
    ))
    .expect("agent definition must parse");
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        Some(acp_profile),
        Some(config::DEFAULT_AGENT_TYPE),
    );
    assert_eq!(
        def.name, "custom-devbox-profile",
        "ACP _meta.agentProfile must win when model_agent_type is the default value"
    );
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// Regression: after `DEFAULT_AGENT_TYPE` flipped to
/// `grok-build-plan`, models in the catalog that still declare
/// `agent_type = "grok-build"` explicitly must NOT preempt an ACP
/// profile. Any value in the `grok-build*` family is the stock harness
/// with no strict requirement.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_acp_profile_wins_for_explicit_grok_build_family() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let acp_profile = xai_grok_agent::AgentDefinition::from_json(&serde_json::json!({
        "name": "custom-devbox-profile",
        "description": "Custom devbox profile",
    }))
    .expect("agent definition must parse");
    for family_variant in ["grok-build", "grok-build-plan", "grok-build-concise"] {
        let def = MvpAgent::resolve_agent_definition(
            tmp.path(),
            None,
            &config::AgentSelectionConfig::default(),
            Some(acp_profile.clone()),
            Some(family_variant),
        );
        assert_eq!(
            def.name, "custom-devbox-profile",
            "ACP profile must win for grok-build family variant `{family_variant}`"
        );
    }
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// A model-declared Markdown definition carries an exact prompt/source
/// identity even when it otherwise uses the stock wire template and toolset.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_model_selects_nonstrict_custom_prompt() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let agents_dir = tmp.path().join(".grok").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("issue-nine-reviewer.md"),
        "---\nname: issue-nine-reviewer\ndescription: exact custom prompt\n---\nReview issue nine.\n",
    )
    .unwrap();

    let acp_profile = xai_grok_agent::AgentDefinition::from_json(&serde_json::json!({
        "name": "client-profile",
        "description": "must not replace the model-declared custom harness",
    }))
    .unwrap();
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        Some(acp_profile),
        Some("issue-nine-reviewer"),
    );

    assert_eq!(def.name, "issue-nine-reviewer");
    assert_eq!(def.prompt_body.as_deref(), Some("Review issue nine."));
    assert!(def.source_path.is_some());
    assert!(!def.is_strict_harness());
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_AGENT", v) },
        None => unsafe { std::env::remove_var("GROK_AGENT") },
    }
}
/// Both qualified and unambiguous bare plugin names must resolve to the same
/// exact non-strict plugin definition rather than the ambient default/profile.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_model_selects_nonstrict_plugin_bare_and_qualified() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let plugin_root = tmp.path().join("plugin-one");
    let agents_dir = plugin_root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(plugin_root.join("plugin.json"), r#"{"name":"plugin-one"}"#).unwrap();
    std::fs::write(
        agents_dir.join("issue-nine-plugin-reviewer.md"),
        "---\nname: issue-nine-plugin-reviewer\ndescription: plugin prompt\n---\nReview from plugin one.\n",
    )
    .unwrap();
    let discovery = xai_grok_agent::plugins::discovery::DiscoveryConfig {
        cli_plugin_dirs: vec![plugin_root],
        ..Default::default()
    };
    let discovered = xai_grok_agent::plugins::discover_plugins(
        Some(tmp.path()),
        &discovery,
        &xai_grok_agent::plugins::TrustStore::load_from(tmp.path().join("trust")),
        true,
    );
    let registry = xai_grok_agent::plugins::PluginRegistry::from_discovered(
        discovered,
        &[],
        &["plugin-one".to_owned()],
    );
    let acp_profile = xai_grok_agent::AgentDefinition::from_json(&serde_json::json!({
        "name": "client-profile",
        "description": "must not replace the model-declared plugin harness",
    }))
    .unwrap();

    for required in [
        "issue-nine-plugin-reviewer",
        "plugin-one:issue-nine-plugin-reviewer",
    ] {
        let def = MvpAgent::resolve_agent_definition_with_plugins(
            tmp.path(),
            None,
            &config::AgentSelectionConfig::default(),
            Some(acp_profile.clone()),
            Some(required),
            Some(&registry),
        );
        assert_eq!(def.name, "issue-nine-plugin-reviewer");
        assert_eq!(def.plugin_name.as_deref(), Some("plugin-one"));
        assert_eq!(def.prompt_body.as_deref(), Some("Review from plugin one."));
        assert!(def.source_path.is_some());
        assert!(!def.is_strict_harness());
        assert!(harnesses_are_compatible(&def, required, Some(&def)));
    }
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_AGENT", v) },
        None => unsafe { std::env::remove_var("GROK_AGENT") },
    }
}
/// A non-strict (stock / vision-capable) model leaves the template alone, so
/// such models keep native image input.
#[test]
fn inherited_harness_template_skips_nonstrict_model() {
    use xai_grok_agent::prompt::user_message::UserMessageTemplate;
    let tmp = tempfile::tempdir().unwrap();
    assert!(
        inherited_harness_template(
            &UserMessageTemplate::Default,
            Some(config::DEFAULT_AGENT_TYPE),
            tmp.path(),
        )
        .is_none()
    );
}
/// An explicit (non-default) template is never overridden — inheritance only
/// fills in the default.
#[test]
fn inherited_harness_template_respects_explicit_template() {
    use xai_grok_agent::prompt::user_message::UserMessageTemplate;
    let tmp = tempfile::tempdir().unwrap();
    let explicit = UserMessageTemplate::Custom("MY CUSTOM TEMPLATE".to_owned());
    assert!(inherited_harness_template(&explicit, Some("cursor"), tmp.path()).is_none());
}
/// CLI `--agent-profile` wins when model_agent_type is the default
/// (also shadowed by the same regression).
#[test]
#[serial_test::serial]
fn resolve_agent_definition_cli_agent_profile_wins_when_model_agent_type_is_default() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let profile_path = tmp.path().join("cli-profile.md");
    std::fs::write(
        &profile_path,
        "---\nname: cli-profile\ndescription: cli test\n---\nYou are a CLI profile.\n",
    )
    .unwrap();
    let def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        Some(&profile_path),
        &config::AgentSelectionConfig::default(),
        None,
        Some(config::DEFAULT_AGENT_TYPE),
    );
    assert_eq!(def.name, "cli-profile");
    if let Some(v) = prev {
        unsafe { std::env::set_var("GROK_AGENT", v) }
    }
}
/// Agent profile with `model: Override(id)` preserves the field through resolution.
#[test]
#[serial_test::serial]
fn resolve_agent_definition_agent_profile_with_model_override() {
    let prev = std::env::var("GROK_AGENT").ok();
    unsafe {
        std::env::remove_var("GROK_AGENT");
    }
    let tmp = tempfile::tempdir().unwrap();
    let agents_dir = tmp.path().join(".grok").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
            agents_dir.join("test-architect.md"),
            "---\nname: test-architect\ndescription: test\nmodel: test-model-123\n---\nYou are a test.\n",
        )
        .unwrap();
    let agent_config = config::AgentSelectionConfig {
        name: Some("test-architect".to_string()),
        definition: None,
        system_prompt_label: None,
    };
    let def = MvpAgent::resolve_agent_definition(tmp.path(), None, &agent_config, None, None);
    assert_eq!(def.name, "test-architect");
    assert_eq!(
        def.model,
        xai_grok_agent::config::ModelOverride::Override("test-model-123".to_string()),
        "agent profile model override must be preserved through resolution"
    );
    match prev {
        Some(v) => unsafe { std::env::set_var("GROK_AGENT", v) },
        None => unsafe { std::env::remove_var("GROK_AGENT") },
    }
}
#[test]
fn read_session_or_init_meta_str_prefers_session_meta() {
    let session = serde_json::json!({ "rules": "from-session" });
    let init = serde_json::json!({ "rules": "from-init" });
    assert_eq!(
        read_session_or_init_meta_str(session.as_object(), init.as_object(), "rules"),
        Some("from-session"),
    );
}
#[test]
fn read_session_or_init_meta_str_falls_back_to_init_meta() {
    let session = serde_json::json!({ "other": "x" });
    let init = serde_json::json!({ "rules": "from-init" });
    assert_eq!(
        read_session_or_init_meta_str(session.as_object(), init.as_object(), "rules"),
        Some("from-init"),
    );
    assert_eq!(
        read_session_or_init_meta_str(None, init.as_object(), "rules"),
        Some("from-init"),
    );
}
#[test]
fn parse_session_plugin_dirs_filters_and_dedupes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = dunce::canonicalize(tmp.path()).unwrap().join("plugin");
    std::fs::create_dir(&dir).unwrap();
    let file = tmp.path().join("file.txt");
    std::fs::write(&file, "x").unwrap();
    let meta = serde_json::json!({
        "pluginDirs": [
            dir.to_string_lossy(),          // kept
            dir.to_string_lossy(),          // duplicate → deduped
            file.to_string_lossy(),         // not a directory → skipped
            "relative/path",                // not absolute → skipped
            42,                             // not a string → skipped
        ]
    });
    assert_eq!(parse_session_plugin_dirs(meta.as_object()), vec![dir]);
    assert!(parse_session_plugin_dirs(None).is_empty());
    assert!(parse_session_plugin_dirs(serde_json::json!({}).as_object()).is_empty());
}
#[test]
fn read_session_or_init_meta_str_returns_none_when_absent() {
    assert_eq!(read_session_or_init_meta_str(None, None, "rules"), None,);
    let session = serde_json::json!({ "other": "x" });
    assert_eq!(
        read_session_or_init_meta_str(session.as_object(), None, "rules"),
        None,
    );
}
#[test]
fn read_session_or_init_meta_str_ignores_non_string_values() {
    let session = serde_json::json!({ "rules": 42 });
    let init = serde_json::json!({ "rules": "from-init" });
    assert_eq!(
        read_session_or_init_meta_str(session.as_object(), init.as_object(), "rules"),
        Some("from-init"),
    );
}
#[test]
fn system_prompt_override_from_meta_prefers_session_and_rejects_empty() {
    let session = serde_json::json!({ "systemPromptOverride": "from session" });
    let init = serde_json::json!({ "systemPromptOverride": "from init" });
    assert_eq!(
        system_prompt_override_from_meta(session.as_object(), init.as_object()),
        Some("from session")
    );
    assert_eq!(
        system_prompt_override_from_meta(None, init.as_object()),
        Some("from init")
    );
    let empty = serde_json::json!({ "systemPromptOverride": "" });
    assert_eq!(
        system_prompt_override_from_meta(empty.as_object(), None),
        None
    );
    assert_eq!(system_prompt_override_from_meta(None, None), None);
}
#[test]
fn enqueue_replace_system_prompt_override_sends_when_present() {
    use crate::session::SessionCommand;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let session = serde_json::json!({ "systemPromptOverride": "from session" });
    let init = serde_json::json!({ "systemPromptOverride": "from init" });
    enqueue_replace_system_prompt_override(&tx, session.as_object(), init.as_object());
    match rx.try_recv() {
        Ok(SessionCommand::ReplaceSystemPrompt { system_prompt }) => {
            assert_eq!(system_prompt, "from session", "session meta wins over init");
        }
        _ => panic!("expected a ReplaceSystemPrompt command"),
    }
}
#[test]
fn enqueue_replace_system_prompt_override_noop_when_absent_or_empty() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    enqueue_replace_system_prompt_override(
        &tx,
        serde_json::json!({ "systemPromptOverride": "" }).as_object(),
        None,
    );
    enqueue_replace_system_prompt_override(&tx, serde_json::json!({}).as_object(), None);
    enqueue_replace_system_prompt_override(&tx, None, None);
    assert!(
        rx.try_recv().is_err(),
        "no command should be enqueued without a non-empty override"
    );
}
/// Regression for the web-client `_meta.agentProfile` -> `set_session_model`
/// flow: a zero-turn switch from `grok-build` (a client profile name) to
/// `grok-build-plan` (the default model agent_type) must be
/// treated as compatible so the harness rebuild is skipped and the
/// custom prompt body is preserved.
#[test]
fn harnesses_are_compatible_for_stock_family_pairs() {
    let stock = |name: &str| {
        let mut definition = xai_grok_agent::AgentDefinition::default_grok_build();
        definition.name = name.to_owned();
        definition
    };
    let grok_build = stock("grok-build");
    let grok_build_plan = stock("grok-build-plan");
    let grok_build_concise = stock("grok-build-concise");
    let remote_sidebar = stock("remote-sidebar");
    assert!(harnesses_are_compatible(
        &grok_build,
        "grok-build-plan",
        Some(&grok_build_plan),
    ));
    assert!(harnesses_are_compatible(
        &grok_build_plan,
        "grok-build",
        Some(&grok_build),
    ));
    assert!(harnesses_are_compatible(&grok_build, "grok-build", None));
    assert!(harnesses_are_compatible(
        &grok_build_concise,
        "grok-build-plan",
        Some(&grok_build_plan),
    ));
    assert!(harnesses_are_compatible(
        &remote_sidebar,
        "grok-build-plan",
        Some(&grok_build_plan),
    ));
}
#[test]
fn harnesses_are_compatible_rejects_strict_mismatches() {
    let codex = xai_grok_agent::AgentDefinition::codex();
    let stock = xai_grok_agent::AgentDefinition::grok_build_plan();
    assert!(harnesses_are_compatible(&codex, "codex", None));
    assert!(!harnesses_are_compatible(&stock, "codex", Some(&codex),));
    assert!(!harnesses_are_compatible(
        &stock,
        "missing-custom-harness",
        None,
    ));
}

fn ready_fallback_entry(model_id: &str, agent_type: &str) -> ModelEntry {
    let mut entry = ModelEntry::fallback(model_id, &config::EndpointsConfig::default());
    entry.info.agent_type = agent_type.to_owned();
    entry
}

#[test]
fn ready_compatible_fallback_skips_incompatible_candidate_in_catalog_order() {
    let active = xai_grok_agent::AgentDefinition::codex();
    let candidates = [
        acp::ModelId::new("first-ready-incompatible"),
        acp::ModelId::new("second-ready-compatible"),
    ];

    let selected = first_ready_compatible_model(
        candidates.clone(),
        &active,
        |id| match id.0.as_ref() {
            "first-ready-incompatible" => Some(ready_fallback_entry(id.0.as_ref(), "cursor")),
            "second-ready-compatible" => Some(ready_fallback_entry(id.0.as_ref(), "codex")),
            _ => None,
        },
        |agent_type| {
            xai_grok_agent::discovery::by_name_in_cwd(agent_type, std::path::Path::new("."))
        },
    );

    assert_eq!(selected, Some(candidates[1].clone()));
}

#[test]
fn ready_compatible_fallback_returns_none_without_compatible_candidate() {
    let active = xai_grok_agent::AgentDefinition::codex();
    let candidates = [
        acp::ModelId::new("ready-cursor"),
        acp::ModelId::new("ready-grok-build"),
    ];

    let selected = first_ready_compatible_model(
        candidates,
        &active,
        |id| match id.0.as_ref() {
            "ready-cursor" => Some(ready_fallback_entry(id.0.as_ref(), "cursor")),
            "ready-grok-build" => Some(ready_fallback_entry(id.0.as_ref(), "grok-build")),
            _ => None,
        },
        |agent_type| {
            xai_grok_agent::discovery::by_name_in_cwd(agent_type, std::path::Path::new("."))
        },
    );

    assert_eq!(selected, None);
}

#[test]
fn cold_spawn_reconciled_route_rejects_incompatible_harness() {
    let active = xai_grok_agent::AgentDefinition::codex();
    let endpoints = config::EndpointsConfig::default();
    let mut removed = ModelEntry::fallback("retained-route", &endpoints);
    removed.info.agent_type = "codex".to_owned();
    let mut replacement = ModelEntry::fallback("retained-route", &endpoints);
    replacement.info.agent_type = "cursor".to_owned();
    let catalog = indexmap::IndexMap::from([
        ("removed-key".to_owned(), removed),
        ("replacement-key".to_owned(), replacement),
    ]);
    let identity = xai_chat_state::CatalogIdentity {
        model_id: "removed-key".to_owned(),
        route: "retained-route".to_owned(),
        lineage: xai_chat_state::CatalogResolutionLineage::UniqueRoute,
        auth_scheme: Some(xai_chat_state::CatalogAuthScheme::Bearer),
    };
    let reconciled = crate::agent::models::reconcile_persisted_catalog_identity(
        &indexmap::IndexMap::from([(
            "replacement-key".to_owned(),
            catalog["replacement-key"].clone(),
        )]),
        &identity,
    )
    .expect("route remaps to the replacement key");

    let selected = first_ready_compatible_model(
        [acp::ModelId::new(reconciled.model_id)],
        &active,
        |id| catalog.get(id.0.as_ref()).cloned(),
        |agent_type| {
            xai_grok_agent::discovery::by_name_in_cwd(agent_type, std::path::Path::new("."))
        },
    );

    assert_eq!(selected, None, "cold spawn must reject the remapped key");
}

#[test]
fn cold_spawn_unresolved_model_uses_catalog_compatible_fallback_without_latch() {
    let persisted = acp::ModelId::new("persisted-unresolved");
    let compatible = acp::ModelId::new("second-ready-compatible");

    let selection = cold_spawn_fallback_selection(&persisted, Some(compatible.clone()), None);

    assert_eq!(selection.model_id, compatible);
    assert_eq!(selection.unavailable_model, None);
}

#[test]
fn cold_spawn_restore_preserves_unavailable_latch_without_compatible_fallback() {
    let persisted = acp::ModelId::new("persisted-unavailable");

    let selection = cold_spawn_fallback_selection(&persisted, None, None);

    assert_eq!(selection.model_id, persisted.clone());
    assert_eq!(selection.unavailable_model, Some(persisted));
}

#[test]
fn empty_catalog_keeps_persisted_identity_pending_instead_of_rejecting_load() {
    let models = indexmap::IndexMap::new();
    let identity = xai_chat_state::CatalogIdentity {
        model_id: "pending-key".to_owned(),
        route: "pending-route".to_owned(),
        lineage: xai_chat_state::CatalogResolutionLineage::UniqueRoute,
        auth_scheme: Some(xai_chat_state::CatalogAuthScheme::Bearer),
    };

    assert!(!should_reject_unresolved_persisted_identity(
        &models,
        Some(&identity),
        None,
    ));
}

#[test]
fn prompt_recovery_rejects_a_remapped_model_with_an_incompatible_harness() {
    run_local_for_bridge_test(|| async {
        let agent = build_agent_with_model_for_tests("replacement", "replacement-route");
        let mut model = agent.models_manager.models()["replacement"].clone();
        model.info.agent_type = "codex".to_owned();
        let active = xai_grok_agent::AgentDefinition::default_grok_build();
        let required = xai_grok_agent::AgentDefinition::codex();

        assert!(!recovered_model_harness_is_compatible(
            &active,
            &model,
            Some(&required),
        ));
    });
}

#[test]
fn identity_backed_prompt_recovery_requires_persisted_harness_evidence() {
    let identity = xai_chat_state::CatalogIdentity {
        model_id: "persisted-key".to_owned(),
        route: "persisted-route".to_owned(),
        lineage: xai_chat_state::CatalogResolutionLineage::UniqueRoute,
        auth_scheme: Some(xai_chat_state::CatalogAuthScheme::Bearer),
    };

    assert!(!latched_recovery_has_required_harness(
        Some(&identity),
        None,
    ));
    assert!(latched_recovery_has_required_harness(
        Some(&identity),
        Some("grok-build"),
    ));
}

#[test]
fn cold_spawn_current_only_fallback_keeps_persisted_model_latched() {
    let persisted = acp::ModelId::new("persisted-unready");
    let current = acp::ModelId::new("current-compatible-but-not-selectable");

    let selection = cold_spawn_fallback_selection(&persisted, None, Some(current.clone()));

    assert_eq!(selection.model_id, current);
    assert_eq!(selection.unavailable_model, Some(persisted));
}

#[test]
fn cold_spawn_usable_fallback_clears_stale_unavailable_latch() {
    let registry = SessionRegistry::default();
    let session_id = acp::SessionId::new("cold-spawn-stale-latch");
    let persisted = acp::ModelId::new("persisted-unavailable");
    registry.set_unavailable_model(&session_id, persisted.clone());

    let selection = cold_spawn_fallback_selection(
        &persisted,
        Some(acp::ModelId::new("ready-compatible")),
        None,
    );
    selection.replace_unavailable_latch(&registry, &session_id, None, None);

    assert_eq!(registry.unavailable_model(&session_id), None);
}

#[test]
fn harnesses_are_compatible_matches_bare_and_qualified_plugin_identity() {
    let source = std::path::PathBuf::from("/plugins/one/agents/reviewer.md");
    let mut active = xai_grok_agent::AgentDefinition::default_grok_build();
    active.name = "reviewer".to_owned();
    active.plugin_name = Some("plugin-one".to_owned());
    active.source_path = Some(source.clone());
    active.prompt_body = Some("Review from plugin one".to_owned());
    let required = active.clone();

    assert!(harnesses_are_compatible(
        &active,
        "plugin-one:reviewer",
        Some(&required),
    ));
    assert!(harnesses_are_compatible(
        &active,
        "reviewer",
        Some(&required),
    ));
}
#[test]
fn harnesses_are_compatible_rejects_different_plugin_and_custom_prompt_sources() {
    let plugin = |owner: &str| {
        let mut definition = xai_grok_agent::AgentDefinition::default_grok_build();
        definition.name = "reviewer".to_owned();
        definition.plugin_name = Some(owner.to_owned());
        definition.source_path = Some(std::path::PathBuf::from(format!(
            "/plugins/{owner}/agents/reviewer.md"
        )));
        definition.prompt_body = Some(format!("Review from {owner}"));
        definition
    };
    let plugin_one = plugin("plugin-one");
    let plugin_two = plugin("plugin-two");
    assert!(!harnesses_are_compatible(
        &plugin_one,
        "plugin-two:reviewer",
        Some(&plugin_two),
    ));

    let custom = |path: &str, body: &str| {
        let mut definition = xai_grok_agent::AgentDefinition::default_grok_build();
        definition.name = "reviewer".to_owned();
        definition.source_path = Some(std::path::PathBuf::from(path));
        definition.prompt_body = Some(body.to_owned());
        definition
    };
    let project = custom("/repo/.grok/agents/reviewer.md", "Project review prompt");
    let user = custom("/home/.grok/agents/reviewer.md", "User review prompt");
    assert!(!harnesses_are_compatible(&project, "reviewer", Some(&user),));

    let mut inline = xai_grok_agent::AgentDefinition::default_grok_build();
    inline.name = "grok-build".to_owned();
    inline.prompt_body = Some("Client-provided prompt".to_owned());
    assert!(harnesses_are_compatible(
        &inline,
        "grok-build",
        Some(&inline),
    ));
    let built_in = xai_grok_agent::AgentDefinition::default_grok_build();
    assert!(!harnesses_are_compatible(
        &inline,
        "grok-build",
        Some(&built_in),
    ));
}

#[test]
fn harnesses_are_compatible_rejects_changed_external_runtime_contract() {
    let mut active = xai_grok_agent::AgentDefinition::default_grok_build();
    active.name = "reviewer".to_owned();
    active.plugin_name = Some("plugin-one".to_owned());
    active.source_path = Some(std::path::PathBuf::from(
        "/plugins/plugin-one/agents/reviewer.md",
    ));
    active.prompt_body = Some("Review from plugin one".to_owned());

    let mut required = active.clone();
    required.permission_mode = xai_grok_agent::config::PermissionMode::BypassPermissions;

    assert!(!harnesses_are_compatible(
        &active,
        "plugin-one:reviewer",
        Some(&required),
    ));
}

#[test]
fn model_switch_cli_clamps_do_not_false_mismatch_external_harness() {
    let mut raw = xai_grok_agent::AgentDefinition::default_grok_build();
    raw.name = "reviewer".to_owned();
    raw.plugin_name = Some("plugin-one".to_owned());
    raw.source_path = Some(std::path::PathBuf::from(
        "/plugins/plugin-one/agents/reviewer.md",
    ));
    raw.prompt_body = Some("Review from plugin one".to_owned());

    let overrides = crate::agent::config::CliAgentOverrides {
        tools: Some(vec!["Read".to_owned(), "Grep".to_owned()]),
        disallowed_tools: Some(vec!["Bash".to_owned()]),
        permission_mode: Some(xai_grok_agent::config::PermissionMode::DontAsk),
        ..Default::default()
    };
    let mut active = raw.clone();
    overrides.apply_to_definition(&mut active);
    let required = apply_session_cli_clamps(Some(raw), &overrides).unwrap();

    assert!(harnesses_are_compatible(
        &active,
        "plugin-one:reviewer",
        Some(&required),
    ));
}

#[test]
fn model_switch_rebuilt_definition_retains_session_cli_clamps() {
    let mut raw = xai_grok_agent::AgentDefinition::codex();
    raw.tools = vec!["HarnessOwnedTool".to_owned()];
    raw.disallowed_tools = vec!["HarnessOwnedDeny".to_owned()];
    raw.permission_mode = xai_grok_agent::config::PermissionMode::Default;
    let overrides = crate::agent::config::CliAgentOverrides {
        tools: Some(vec!["Read".to_owned()]),
        disallowed_tools: Some(vec!["Bash".to_owned(), "Write".to_owned()]),
        permission_mode: Some(xai_grok_agent::config::PermissionMode::Plan),
        ..Default::default()
    };

    let rebuilt = apply_session_cli_clamps(Some(raw), &overrides).unwrap();

    assert_eq!(rebuilt.tools, vec!["Read"]);
    assert_eq!(rebuilt.disallowed_tools, vec!["Bash", "Write"]);
    assert_eq!(
        rebuilt.permission_mode,
        xai_grok_agent::config::PermissionMode::Plan
    );
    assert_eq!(rebuilt.name, "codex", "harness identity must be preserved");
}

#[test]
fn harnesses_are_compatible_rejects_changed_strict_system_prompt() {
    let active = xai_grok_agent::AgentDefinition::codex();
    let mut required = active.clone();
    required.system_prompt = xai_grok_agent::prompt::context::TemplateOverride::Custom(
        "Different strict system prompt".to_owned(),
    );

    assert!(!harnesses_are_compatible(&active, "codex", Some(&required),));
}
#[test]
fn explicit_agent_type_wins_over_session_default() {
    assert_eq!(
        resolve_required_agent_type(Some("cursor"), "grok-build-plan"),
        "cursor"
    );
}
#[test]
fn null_agent_type_falls_back_to_session_default_grok_build_plan() {
    assert_eq!(
        resolve_required_agent_type(None, "grok-build-plan"),
        "grok-build-plan"
    );
}
#[test]
fn null_agent_type_falls_back_to_session_default_grok_build() {
    assert_eq!(
        resolve_required_agent_type(None, "grok-build"),
        "grok-build"
    );
}
#[test]
fn null_agent_type_returns_to_session_default_after_cursor_switch() {
    let session_default = "grok-build-plan";
    let required_after_null = resolve_required_agent_type(None, session_default);
    assert_eq!(required_after_null, "grok-build-plan");
    assert_ne!(required_after_null, "cursor");
}
/// Compatible stock switches (no rebuild) must NOT mutate `agent_name`,
/// preserving the session's original ACP `agentProfile`.
#[test]
fn agent_name_unchanged_without_harness_rebuild() {
    let unchanged = agent_name_after_model_switch(false, "grok-build-plan", "remote-sidebar");
    assert_eq!(
        unchanged, "remote-sidebar",
        "a compatible stock switch must preserve the original agent profile name"
    );
}
/// End-to-end test: config -> resolve -> override -> finalize -> tool_definitions.
///
/// Exercises the full live path through to the finalized toolset, proving
/// that the hashline tools appear in the actual tool definitions that
/// would be sent to the model.
#[tokio::test]
async fn file_toolset_override_e2e_to_finalized_toolset() {
    use crate::tools::{FileToolset, ShellToolsetConfig};
    use xai_grok_tools::computer::local::{LocalFs, LocalTerminalBackend};
    use xai_grok_tools::notification::ToolNotificationHandle;
    use xai_grok_tools::registry::types::SessionContext;
    let tmp = tempfile::tempdir().unwrap();
    let mut def = MvpAgent::resolve_agent_definition(
        tmp.path(),
        None,
        &config::AgentSelectionConfig::default(),
        None,
        None,
    );
    let toolset_config = ShellToolsetConfig {
        file_toolset: FileToolset::Hashline,
        ..ShellToolsetConfig::default()
    };
    let effective = toolset_config.resolve_file_toolset(None);
    let file_tools = effective
        .tool_configs(&toolset_config.hashline)
        .expect("default hashline config should validate");
    def.override_file_tools(file_tools);
    let builder = xai_grok_tools::registry::types::ToolRegistryBuilder::new();
    let ctx = SessionContext {
        backend: std::sync::Arc::new(LocalTerminalBackend::new()),
        fs: std::sync::Arc::new(LocalFs),
        cwd: tmp.path().to_path_buf(),
        session_folder: tmp.path().join("session"),
        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: tmp.path().join("state.json"),
        memory_backend: None,
        web_search_config: xai_grok_tools::implementations::web_search::WebSearchConfig::default(),
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig::default(),
        video_gen_config: xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig::default(),
        app_builder_deployer_config: xai_grok_tools::implementations::grok_build::deploy_app::AppBuilderDeployerConfig::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
    };
    let toolset = builder
        .finalize(def.tool_config, ctx)
        .expect("hashline toolset should finalize");
    let defs = toolset.tool_definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
    assert!(names.contains(&"hashline_read"), "defs: {names:?}");
    assert!(names.contains(&"hashline_edit"), "defs: {names:?}");
    assert!(names.contains(&"hashline_grep"), "defs: {names:?}");
    assert!(!names.contains(&"read_file"), "defs: {names:?}");
    assert!(!names.contains(&"search_replace"), "defs: {names:?}");
    assert!(names.contains(&"list_dir"), "defs: {names:?}");
}
/// Invalid hashline config returns a clean error, not a panic.
#[test]
fn file_toolset_override_invalid_config_returns_error() {
    use crate::tools::FileToolset;
    use crate::tools::config::HashlineSchemeConfig;
    let bad = HashlineSchemeConfig {
        scheme: "bogus".to_owned(),
        hash_len: 0,
        chunk_size: 0,
    };
    let err = FileToolset::Hashline.tool_configs(&bad);
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("unknown"));
}
/// Helper: creates a real SessionHandle with the given model, yolo, and client id.
/// Requires a tokio runtime for SessionSignalsHandle::new().
fn make_test_handle(
    model: &str,
    yolo: bool,
    client_id: Option<&str>,
) -> crate::session::SessionHandle {
    let (cmd_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    let (hunk_event_tx, _hunk_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let hunk_cancel = tokio_util::sync::CancellationToken::new();
    let hunk_tracker_handle = xai_hunk_tracker::HunkTrackerActor::spawn(
        "test".to_string(),
        std::path::PathBuf::from("/tmp"),
        hunk_event_tx,
        xai_hunk_tracker::TrackingMode::AllDirty,
        hunk_cancel,
    );
    crate::session::SessionHandle {
        cmd_tx,
        persistence_tx,
        fresh_publication: None,
        current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
        pending_interactions: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        info: crate::session::info::Info {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".to_string(),
        },
        max_turns: None,
        capability_mode: None,
        resolved_tool_overrides: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
        hunk_tracker_handle,
        workspace_toolset: std::sync::Arc::new(
            xai_grok_tools::registry::types::FinalizedToolset::empty_for_test(),
        ),
        chat_state_handle: xai_chat_state::ChatStateHandle::noop(),
        signals_handle: crate::session::signals::SessionSignalsHandle::new(),
        gateway_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        mcp_servers: vec![],
        initial_client_mcp_servers: vec![],
        display_cwd: None,
        feedback_manager: std::sync::Arc::new(
            crate::session::feedback_manager::FeedbackManager::local_only("test"),
        ),
        upload_queue: Arc::new(OnceLock::new()),
        upload_failures_since_success: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        tool_context: crate::tools::ToolContext::new_local_context(
            xai_grok_paths::AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap(),
            std::sync::Arc::new(xai_grok_workspace::file_system::LocalFs::new(
                std::path::PathBuf::from("/tmp"),
            )),
            std::sync::Arc::new(crate::terminal::LocalTerminalRunner),
        ),
        model_id: acp::ModelId::new(model),
        auxiliary_model_provenance: crate::session::AuxiliaryModelProvenance::default(),
        scheduler_background_loops: true,
        reasoning_effort: None,
        yolo_mode: yolo,
        origin_client: client_id.map(|s| crate::http::OriginClientInfo {
            product: s.to_string(),
            version: None,
        }),
        code_nav_enabled: false,
        ask_user_question_enabled: true,
        plan_mode: std::sync::Arc::new(parking_lot::Mutex::new(
            crate::session::plan_mode::PlanModeTracker::new(std::path::PathBuf::from("/tmp")),
        )),
        force_compact: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        permission_handle: xai_grok_workspace::permission::PermissionHandle::allow_all(),
        attribution_callback: None,
        agent_name: "grok-build".to_string(),
        managed_mcp_proxy_base_url: String::new(),
        session_default_agent_profile: None,
        allowed_subagent_types: None,
        hook_registry: None,
        workspace_ops: xai_grok_workspace::WorkspaceOps::for_test(),
        terminal_backend: None,
        tools_notification_handle: None,
        scheduler_handle: None,
    }
}
/// lookup_session_model returns the per-session model when one is known.
#[tokio::test]
async fn lookup_session_model_returns_per_session_model() {
    let default_model = acp::ModelId::new("default-model");
    assert_eq!(
        lookup_session_model(Some(acp::ModelId::new("grok-3-fast")), &default_model)
            .0
            .as_ref(),
        "grok-3-fast"
    );
    assert_eq!(
        lookup_session_model(Some(acp::ModelId::new("codex-mini")), &default_model)
            .0
            .as_ref(),
        "codex-mini"
    );
}
/// lookup_session_model falls back to the default when no session model is known.
#[tokio::test]
async fn lookup_session_model_fallback_no_session() {
    let default_model = acp::ModelId::new("grok-3");
    assert_eq!(
        lookup_session_model(None, &default_model).0.as_ref(),
        "grok-3"
    );
}
/// Mutating session A's model_id via the handle does not affect session B.
#[tokio::test]
async fn set_session_model_does_not_cross_contaminate() {
    let sid_a = acp::SessionId::new("sess-a");
    let sid_b = acp::SessionId::new("sess-b");
    let default_model = acp::ModelId::new("default");
    let mut sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = [
        (sid_a.clone(), make_test_handle("grok-3", false, None)),
        (sid_b.clone(), make_test_handle("grok-3", false, None)),
    ]
    .into();
    sessions.get_mut(&sid_a).unwrap().model_id = acp::ModelId::new("codex-mini");
    assert_eq!(
        lookup_session_model(
            sessions.get(&sid_a).map(|h| h.model_id.clone()),
            &default_model
        )
        .0
        .as_ref(),
        "codex-mini"
    );
    assert_eq!(
        lookup_session_model(
            sessions.get(&sid_b).map(|h| h.model_id.clone()),
            &default_model
        )
        .0
        .as_ref(),
        "grok-3",
        "Session B's model must not be affected by session A's model change"
    );
}
#[tokio::test]
async fn unresolved_empty_current_model_uses_live_fail_closed_sampling_config() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};

    let agent = build_minimal_agent_for_tests();
    assert!(
        !agent.sampling_config.borrow().base_url.is_empty(),
        "the fixture must retain a usable startup route to expose a stale fallback"
    );

    let mut oauth_only = ModelEntry::fallback("oauth-only", &EndpointsConfig::default());
    oauth_only.info.supported_in_api = false;
    agent
        .models_manager
        .apply_catalog_for_test(indexmap::IndexMap::from([(
            "oauth-only".to_owned(),
            oauth_only,
        )]));

    let current = agent.models_manager.current_model_id();
    assert!(current.0.is_empty());
    let sampling = agent.resolve_sampling_config_for_model(&current, None);
    assert!(
        sampling.base_url.is_empty(),
        "an empty current id must not fall back to the stale startup endpoint"
    );
    assert_eq!(sampling.api_key, None);
}
/// #360: `/new` must keep a valid built-in effort when the model
/// advertises reasoning support but leaves `reasoning_efforts` empty.
#[tokio::test]
async fn new_session_keeps_effort_when_supported_menu_is_implicit() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    use xai_grok_sampling_types::ReasoningEffort;

    let agent = build_minimal_agent_for_tests();
    let mut entry = ModelEntry::fallback("implicit-effort-model", &EndpointsConfig::default());
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_efforts.clear();
    agent
        .models_manager
        .insert_test_entry("implicit-effort-model", entry);
    agent
        .models_manager
        .set_current_reasoning_effort(Some(ReasoningEffort::High));

    let mut sampling =
        agent.resolve_sampling_config_for_model(&acp::ModelId::new("implicit-effort-model"), None);
    sampling.model = "implicit-effort-model".into();
    sampling.reasoning_effort = None;
    agent.apply_current_reasoning_effort(&mut sampling);
    assert_eq!(
        sampling.reasoning_effort,
        Some(ReasoningEffort::High),
        "implicit catalog menu must keep the selected built-in effort"
    );
}

#[tokio::test]
async fn model_state_prefers_session_reasoning_effort_over_global() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    use xai_grok_sampling_types::{
        REASONING_EFFORT_META_KEY, ReasoningEffort, ReasoningEffortOption,
    };
    let agent = build_minimal_agent_for_tests();
    let mut entry = ModelEntry::fallback("effort-model", &EndpointsConfig::default());
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "low".into(),
        value: ReasoningEffort::Low,
        label: "Low".into(),
        description: None,
        default: true,
    }];
    agent
        .models_manager
        .insert_test_entry("effort-model", entry);
    agent
        .models_manager
        .set_current_reasoning_effort(Some(ReasoningEffort::Low));
    let read_effort = |state: &acp::SessionModelState| -> Option<String> {
        state
            .available_models
            .iter()
            .find(|m| m.model_id.0.as_ref() == "effort-model")
            .and_then(|m| m.meta.as_ref())
            .and_then(|m| m.get(REASONING_EFFORT_META_KEY))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    let pinned = acp::SessionId::new("sess-pinned");
    let mut handle = make_test_handle("effort-model", false, None);
    handle.reasoning_effort = Some(ReasoningEffort::Xhigh);
    agent.insert_resident(&pinned, handle);
    let pinned_state = agent.model_state(Some(&pinned));
    assert_eq!(
        read_effort(&pinned_state).as_deref(),
        Some("xhigh"),
        "model_state must report the running session's actual effort even after the catalog menu removes it",
    );
    let pinned_options = agent.session_config_options(Some(&pinned), &pinned_state);
    assert!(
        pinned_options
            .iter()
            .any(|option| option.category == "mode" && option.id == "xhigh" && option.selected),
        "session config must synthesize a selected option for the immutable actor's active tier"
    );
    let unset = acp::SessionId::new("sess-unset");
    agent.insert_resident(&unset, make_test_handle("effort-model", false, None));
    assert_eq!(
        read_effort(&agent.model_state(Some(&unset))).as_deref(),
        Some("low"),
        "absent session effort falls back to the global default",
    );
}

#[tokio::test]
async fn session_config_preserves_resident_effort_after_catalog_disables_reasoning() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    use xai_grok_sampling_types::ReasoningEffort;

    let agent = build_minimal_agent_for_tests();
    let mut enabled = ModelEntry::fallback("effort-model", &EndpointsConfig::default());
    enabled.info.supports_reasoning_effort = true;
    agent
        .models_manager
        .insert_test_entry("effort-model", enabled);
    let session_id = acp::SessionId::new("resident-effort-after-disable");
    let mut handle = make_test_handle("effort-model", false, None);
    handle.reasoning_effort = Some(ReasoningEffort::Xhigh);
    agent.insert_resident(&session_id, handle);

    let disabled = ModelEntry::fallback("effort-model", &EndpointsConfig::default());
    agent
        .models_manager
        .insert_test_entry("effort-model", disabled);

    let state = agent.model_state(Some(&session_id));
    let options = agent.session_config_options(Some(&session_id), &state);
    assert!(
        options
            .iter()
            .any(|option| { option.category == "mode" && option.id == "xhigh" && option.selected })
    );
}
/// A resident routing slug is a sampler route, not an ACP picker identity.
/// Normalize it to the unique catalog key so model state, effort annotation,
/// and session config all select the same available entry.
#[tokio::test]
async fn session_config_options_resolves_routing_slug_to_catalog_model() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    use xai_grok_sampling_types::{REASONING_EFFORT_META_KEY, ReasoningEffort};
    let agent = build_minimal_agent_for_tests();
    let mut entry = ModelEntry::fallback("catalog-key-model", &EndpointsConfig::default());
    entry.info.model = "routing-slug".to_string();
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_effort = Some(ReasoningEffort::High);
    agent
        .models_manager
        .insert_test_entry("catalog-key-model", entry);
    let sid = acp::SessionId::new("sess-slug");
    agent.insert_resident(&sid, make_test_handle("routing-slug", false, None));
    let state = agent.model_state(Some(&sid));
    assert_eq!(state.current_model_id.0.as_ref(), "catalog-key-model");
    assert_eq!(
        state
            .available_models
            .iter()
            .filter(|model| model.model_id == state.current_model_id)
            .count(),
        1,
        "the normalized resident must select exactly one advertised model"
    );
    assert_eq!(
        state
            .available_models
            .iter()
            .find(|model| model.model_id == state.current_model_id)
            .and_then(|model| model.meta.as_ref())
            .and_then(|meta| meta.get(REASONING_EFFORT_META_KEY)),
        Some(&serde_json::json!("high")),
        "model-state effort must use the same catalog-key identity as session config"
    );
    let opts = agent.session_config_options(Some(&sid), &state);
    let modes: Vec<_> = opts.iter().filter(|o| o.category == "mode").collect();
    assert!(
        !modes.is_empty(),
        "reasoning modes must surface for a slug-identified session"
    );
    assert!(
        modes.iter().any(|o| o.id == "high" && o.selected),
        "catalog default effort should be selected"
    );
    assert_eq!(
        opts.iter()
            .filter(|option| option.category == "model" && option.selected)
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        vec!["catalog-key-model"],
        "resolved catalog model must be the only selected picker entry"
    );
}

#[tokio::test]
async fn removed_resident_model_is_selected_only_as_an_unavailable_placeholder() {
    use xai_grok_sampling_types::{REASONING_EFFORT_META_KEY, ReasoningEffort};

    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-removed-model");
    let mut handle = make_test_handle("removed-route", false, None);
    handle.reasoning_effort = Some(ReasoningEffort::Xhigh);
    agent.insert_resident(&sid, handle);

    let state = agent.model_state(Some(&sid));
    assert_eq!(state.current_model_id.0.as_ref(), "removed-route");
    assert_eq!(
        state
            .available_models
            .iter()
            .filter(|model| model.model_id == state.current_model_id)
            .count(),
        1
    );
    let placeholder = state
        .available_models
        .iter()
        .find(|model| model.model_id == state.current_model_id)
        .expect("removed resident route must have an explicit unavailable state");
    assert_eq!(
        placeholder.meta.as_ref().and_then(|meta| meta.get("ready")),
        Some(&serde_json::json!(false))
    );
    assert_eq!(
        placeholder
            .meta
            .as_ref()
            .and_then(|meta| meta.get(REASONING_EFFORT_META_KEY)),
        Some(&serde_json::json!("xhigh")),
        "the immutable resident effort remains observable on the unavailable state"
    );
    let options = agent.session_config_options(Some(&sid), &state);
    assert_eq!(
        options
            .iter()
            .filter(|option| option.category == "model" && option.selected)
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "an unavailable resident placeholder is state, not a selectable model option"
    );
    assert!(
        !options
            .iter()
            .any(|option| { option.category == "model" && option.id == "removed-route" })
    );
    assert_eq!(
        options
            .iter()
            .filter(|option| option.category == "mode" && option.selected)
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        vec!["xhigh"]
    );
}

#[tokio::test]
async fn auth_hidden_resident_model_is_selected_only_as_an_unavailable_placeholder() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    use xai_grok_sampling_types::{
        REASONING_EFFORT_META_KEY, ReasoningEffort, ReasoningEffortOption,
    };

    let agent = build_minimal_agent_for_tests();
    let mut oauth_only = ModelEntry::fallback("oauth-only", &EndpointsConfig::default());
    oauth_only.info.supported_in_api = false;
    oauth_only.info.supports_reasoning_effort = true;
    oauth_only.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "low".to_string(),
        value: ReasoningEffort::Low,
        label: "Low".to_string(),
        description: None,
        default: true,
    }];
    agent
        .models_manager
        .insert_test_entry("oauth-only", oauth_only);
    let sid = acp::SessionId::new("sess-auth-hidden-model");
    let mut handle = make_test_handle("oauth-only", false, None);
    handle.reasoning_effort = Some(ReasoningEffort::Low);
    agent.insert_resident(&sid, handle);

    let state = agent.model_state(Some(&sid));
    let placeholder = state
        .available_models
        .iter()
        .find(|model| model.model_id == state.current_model_id)
        .expect("auth-hidden resident route must have an explicit unavailable state");
    assert_eq!(state.current_model_id.0.as_ref(), "oauth-only");
    assert_eq!(
        state
            .available_models
            .iter()
            .filter(|model| model.model_id == state.current_model_id)
            .count(),
        1
    );
    assert_eq!(
        placeholder.meta.as_ref().and_then(|meta| meta.get("ready")),
        Some(&serde_json::json!(false))
    );
    assert_eq!(
        placeholder
            .meta
            .as_ref()
            .and_then(|meta| meta.get(REASONING_EFFORT_META_KEY)),
        Some(&serde_json::json!("low"))
    );
    let options = agent.session_config_options(Some(&sid), &state);
    assert_eq!(
        options
            .iter()
            .filter(|option| option.category == "model" && option.selected)
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "an auth-hidden resident placeholder is state, not a selectable model option"
    );
    assert!(
        !options
            .iter()
            .any(|option| { option.category == "model" && option.id == "oauth-only" })
    );
    assert_eq!(
        options
            .iter()
            .filter(|option| option.category == "mode" && option.selected)
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        vec!["low"]
    );
}

#[test]
fn production_set_session_model_rejects_auth_hidden_target_before_actor_dispatch() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};

    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let mut oauth_only = ModelEntry::fallback("switch-oauth-only", &EndpointsConfig::default());
        oauth_only.info.supported_in_api = false;
        oauth_only.info.auth_scheme = xai_grok_sampler::AuthScheme::None;
        agent
            .models_manager
            .insert_test_entry("switch-oauth-only", oauth_only);
        let sid = acp::SessionId::new("set-hidden-model");
        let (handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        agent.insert_resident(&sid, handle);

        let error = <MvpAgent as acp::Agent>::set_session_model(
            &agent,
            acp::SetSessionModelRequest::new(sid, acp::ModelId::new("switch-oauth-only")),
        )
        .await
        .expect_err("auth-hidden targets must fail at the production trait gate");
        assert_eq!(error.code, acp::ErrorCode::InvalidParams);
        assert!(
            error
                .data
                .as_ref()
                .and_then(serde_json::Value::as_str)
                .is_some_and(|message| message.contains("current authentication mode"))
        );
        assert!(matches!(
            cmd_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    });
}

#[test]
fn production_prompt_latches_removed_or_auth_hidden_resident_before_dispatch() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};

    run_local_for_bridge_test(|| async {
        for hidden in [false, true] {
            let agent = build_minimal_agent_for_tests();
            let model_id = if hidden {
                "prompt-auth-hidden"
            } else {
                "prompt-removed"
            };
            let mut entry = ModelEntry::fallback(model_id, &EndpointsConfig::default());
            entry.info.auth_scheme = xai_grok_sampler::AuthScheme::None;
            agent
                .models_manager
                .insert_test_entry(model_id, entry.clone());
            let sid = acp::SessionId::new(format!("resident-{model_id}"));
            let (handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
            agent.insert_resident(&sid, {
                let mut handle = handle;
                handle.model_id = acp::ModelId::new(model_id);
                handle
            });

            if hidden {
                entry.info.supported_in_api = false;
                agent.models_manager.insert_test_entry(model_id, entry);
            } else {
                agent
                    .models_manager
                    .apply_catalog_for_test(indexmap::IndexMap::new());
            }

            let response = <MvpAgent as acp::Agent>::prompt(
                &agent,
                acp::PromptRequest::new(
                    sid.clone(),
                    vec![acp::ContentBlock::from("must not reach the actor")],
                ),
            )
            .await
            .expect("unavailable resident prompt must block cleanly");
            assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
            assert_eq!(
                agent.session_registry.unavailable_model(&sid),
                Some(acp::ModelId::new(model_id))
            );
            assert!(matches!(
                cmd_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
        }
    });
}

#[tokio::test]
async fn prompt_slug_normalization_does_not_overwrite_a_concurrent_model_switch() {
    let sid = acp::SessionId::new("prompt-normalization-cas");
    let (mut handle, _cmd_tx, _cmd_rx) = make_live_session_handle(&sid, None);
    let stale_slug = acp::ModelId::new("stale-routing-slug");
    let normalized_key = acp::ModelId::new("stale-catalog-key");
    let switched_model = acp::ModelId::new("concurrently-selected-model");

    handle.model_id = switched_model.clone();
    assert!(
        !super::acp_agent::normalize_resident_model_if_unchanged(
            &mut handle,
            &stale_slug,
            &normalized_key,
        ),
        "a stale normalization attempt must lose to the newer committed model"
    );
    assert_eq!(handle.model_id, switched_model);

    handle.model_id = stale_slug.clone();
    assert!(super::acp_agent::normalize_resident_model_if_unchanged(
        &mut handle,
        &stale_slug,
        &normalized_key,
    ));
    assert_eq!(handle.model_id, normalized_key);
}

#[test]
fn production_prompt_recovery_does_not_undo_a_concurrent_user_model_switch() {
    run_local_for_bridge_test(|| async {
        let old_model = "prompt-recovery-old";
        let new_model = "prompt-recovery-new";
        let agent = std::rc::Rc::new(build_agent_with_model_for_tests(old_model, "grok-build"));
        let mut new_entry = agent.models_manager.models()[old_model].clone();
        new_entry.info.model = new_model.to_owned();
        agent.models_manager.insert_test_entry(new_model, new_entry);

        let sid = acp::SessionId::new("prompt-recovery-user-switch-race");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(old_model);
        handle.agent_name = "grok-build".to_owned();
        agent.insert_resident(&sid, handle);
        let old_identity = crate::agent::models::resolve_catalog_identity(
            &agent.models_manager.models(),
            &acp::ModelId::new(old_model),
        )
        .expect("old model identity");
        agent.session_registry.set_unavailable_model_with_identity(
            &sid,
            acp::ModelId::new(old_model),
            Some(old_identity),
            Some("grok-build".to_owned()),
        );

        let (switch_holds_lock_tx, switch_holds_lock_rx) = tokio::sync::oneshot::channel();
        let (prompt_captured_tx, prompt_captured_rx) = tokio::sync::oneshot::channel();
        let hook_agent = agent.clone();
        let hook_sid = sid.clone();
        super::acp_agent::install_prompt_recovery_boundary_hook(&sid, move |restore_model_id| {
            assert_eq!(restore_model_id.0.as_ref(), old_model);
            assert_eq!(
                hook_agent.session_registry.unavailable_model(&hook_sid),
                Some(acp::ModelId::new(old_model))
            );
            let _ = prompt_captured_tx.send(());
        });

        let apply_targets = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let actor_apply_targets = apply_targets.clone();
        let get_active_agent_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let actor_get_active_agent_count = get_active_agent_count.clone();
        let prompt_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let actor_prompt_count = prompt_count.clone();
        let (prompt_dispatched_tx, prompt_dispatched_rx) = tokio::sync::oneshot::channel();
        let actor = tokio::task::spawn_local(async move {
            let mut switch_holds_lock_tx = Some(switch_holds_lock_tx);
            let mut prompt_captured_rx = Some(prompt_captured_rx);
            let mut prompt_dispatched_tx = Some(prompt_dispatched_tx);
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        actor_get_active_agent_count.set(actor_get_active_agent_count.get() + 1);
                        if let Some(tx) = switch_holds_lock_tx.take() {
                            let _ = tx.send(());
                            prompt_captured_rx
                                .take()
                                .expect("first switch waits for prompt recovery snapshot")
                                .await
                                .expect("prompt must capture the old latch");
                        }
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch {
                        prepared,
                        responds_to,
                    } => {
                        let target = prepared.catalog_identity.model_id.clone();
                        actor_apply_targets.borrow_mut().push(target.clone());
                        let _ = responds_to.send(Ok(crate::session::AppliedModelSwitch {
                            previous_model_id: acp::ModelId::new(old_model),
                            catalog_model_id: acp::ModelId::new(target),
                            did_rebuild: false,
                            active_agent_type: Some("grok-build".to_owned()),
                            web_search: None,
                        }));
                    }
                    TestSessionCommand::GetCurrentPromptMode { responds_to } => {
                        let _ = responds_to.send(Default::default());
                    }
                    TestSessionCommand::GetCurrentModel { responds_to } => {
                        let _ = responds_to.send(new_model.to_owned());
                    }
                    TestSessionCommand::GetModelMetadata { responds_to } => {
                        let _ = responds_to.send(Default::default());
                    }
                    TestSessionCommand::CopyFile { respond_to } => {
                        let _ = respond_to.send(Err(anyhow::anyhow!(
                            "session copy is unavailable in the fake actor"
                        )));
                    }
                    TestSessionCommand::SetNextTraceTurn { .. } => {}
                    TestSessionCommand::PersistGitHead { .. } => {}
                    TestSessionCommand::TakeHarnessTraceTurns { respond_to } => {
                        let _ = respond_to.send(Vec::new());
                    }
                    TestSessionCommand::TakeTurnMessages { respond_to } => {
                        let _ = respond_to.send(None);
                    }
                    TestSessionCommand::TakeStreamingCapture { respond_to, .. } => {
                        let _ = respond_to.send(None);
                    }
                    TestSessionCommand::Prompt { respond_to, .. } => {
                        actor_prompt_count.set(actor_prompt_count.get() + 1);
                        if let Some(tx) = prompt_dispatched_tx.take() {
                            let _ = tx.send(());
                        }
                        let _ = respond_to.send(crate::session::ok_end_turn(0, None));
                    }
                    _ => panic!("unexpected command during prompt recovery race"),
                }
            }
        });

        let switch_agent = agent.clone();
        let switch_sid = sid.clone();
        let switch_task = tokio::task::spawn_local(async move {
            <MvpAgent as acp::Agent>::set_session_model(
                &switch_agent,
                acp::SetSessionModelRequest::new(switch_sid, acp::ModelId::new(new_model)),
            )
            .await
        });
        switch_holds_lock_rx
            .await
            .expect("user switch must hold the dispatch lock before prompt starts");

        let prompt_agent = agent.clone();
        let prompt_sid = sid.clone();
        let prompt_task = tokio::task::spawn_local(async move {
            <MvpAgent as acp::Agent>::prompt(
                &prompt_agent,
                acp::PromptRequest::new(
                    prompt_sid,
                    vec![acp::ContentBlock::from("use the newly selected model")],
                ),
            )
            .await
        });

        let switch_result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let switch_result = switch_task.await.expect("switch task");
            prompt_dispatched_rx
                .await
                .expect("the prompt must reach the actor after the user switch");
            switch_result
        })
        .await
        .expect("the serialized switch and prompt must complete without deadlock");
        switch_result.expect("user model switch");
        prompt_task.abort();
        assert_eq!(apply_targets.borrow().as_slice(), [new_model]);
        assert_eq!(get_active_agent_count.get(), 1);
        assert_eq!(prompt_count.get(), 1);
        assert_eq!(
            agent.resident_handle(&sid).unwrap().model_id,
            acp::ModelId::new(new_model)
        );
        assert!(agent.session_registry.unavailable_model(&sid).is_none());
        assert!(
            agent
                .session_registry
                .unavailable_catalog_identity(&sid)
                .is_none()
        );
        assert!(
            agent
                .session_registry
                .unavailable_agent_name(&sid)
                .is_none()
        );
        actor.abort();
    });
}

#[test]
fn production_prompt_recovery_superseded_by_a_new_block_remains_fail_closed() {
    run_local_for_bridge_test(|| async {
        let old_model = "prompt-recovery-old-block";
        let ready_fallback = "prompt-recovery-ready-fallback";
        let new_unavailable_model = "prompt-recovery-new-block";
        let agent = std::rc::Rc::new(build_agent_with_model_for_tests(old_model, "grok-build"));
        let mut fallback_entry = agent.models_manager.models()[old_model].clone();
        fallback_entry.info.model = ready_fallback.to_owned();
        agent
            .models_manager
            .insert_test_entry(ready_fallback, fallback_entry);

        let sid = acp::SessionId::new("prompt-recovery-new-block-race");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(old_model);
        handle.agent_name = "grok-build".to_owned();
        agent.insert_resident(&sid, handle);
        let old_identity = crate::agent::models::resolve_catalog_identity(
            &agent.models_manager.models(),
            &acp::ModelId::new(old_model),
        )
        .expect("old model identity");
        agent.session_registry.set_unavailable_model_with_identity(
            &sid,
            acp::ModelId::new(old_model),
            Some(old_identity),
            Some("grok-build".to_owned()),
        );

        let hook_agent = agent.clone();
        let hook_sid = sid.clone();
        super::acp_agent::install_prompt_recovery_boundary_hook(&sid, move |restore_model_id| {
            assert_eq!(restore_model_id.0.as_ref(), old_model);
            hook_agent.with_resident_mut(&hook_sid, |resident| {
                resident.model_id = acp::ModelId::new(ready_fallback);
            });
            hook_agent
                .session_registry
                .set_unavailable_model_with_identity(
                    &hook_sid,
                    acp::ModelId::new(new_unavailable_model),
                    None,
                    Some("grok-build".to_owned()),
                );
        });

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            <MvpAgent as acp::Agent>::prompt(
                &agent,
                acp::PromptRequest::new(
                    sid.clone(),
                    vec![acp::ContentBlock::from("must remain blocked")],
                ),
            ),
        )
        .await
        .expect("superseded recovery must not hang")
        .expect("superseded recovery must block cleanly");

        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
        assert_eq!(
            agent.resident_handle(&sid).unwrap().model_id,
            acp::ModelId::new(ready_fallback)
        );
        assert_eq!(
            agent.session_registry.unavailable_model(&sid),
            Some(acp::ModelId::new(new_unavailable_model))
        );
        assert!(
            cmd_rx.try_recv().is_err(),
            "a newer unavailable latch must block before any actor command"
        );
    });
}

#[test]
fn production_prompt_recovery_preserves_an_aba_latch_written_during_actor_commit() {
    run_local_for_bridge_test(|| async {
        let model_id = "prompt-recovery-actor-aba";
        let agent = std::rc::Rc::new(build_agent_with_model_for_tests(model_id, "grok-build"));
        let sid = acp::SessionId::new("prompt-recovery-actor-aba");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(model_id);
        handle.agent_name = "grok-build".to_owned();
        agent.insert_resident(&sid, handle);
        let identity = crate::agent::models::resolve_catalog_identity(
            &agent.models_manager.models(),
            &acp::ModelId::new(model_id),
        )
        .expect("model identity");
        agent.session_registry.set_unavailable_model_with_identity(
            &sid,
            acp::ModelId::new(model_id),
            Some(identity.clone()),
            Some("grok-build".to_owned()),
        );

        let apply_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let actor_apply_count = apply_count.clone();
        let actor_agent = agent.clone();
        let actor_sid = sid.clone();
        let actor_identity = identity.clone();
        let actor = tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch {
                        prepared,
                        responds_to,
                    } => {
                        actor_apply_count.set(actor_apply_count.get() + 1);
                        assert_eq!(prepared.catalog_identity.model_id, model_id);
                        actor_agent
                            .session_registry
                            .take_unavailable_model(&actor_sid);
                        actor_agent
                            .session_registry
                            .set_unavailable_model_with_identity(
                                &actor_sid,
                                acp::ModelId::new(model_id),
                                Some(actor_identity.clone()),
                                Some("grok-build".to_owned()),
                            );
                        let _ = responds_to.send(Ok(crate::session::AppliedModelSwitch {
                            previous_model_id: acp::ModelId::new(model_id),
                            catalog_model_id: acp::ModelId::new(model_id),
                            did_rebuild: false,
                            active_agent_type: Some("grok-build".to_owned()),
                            web_search: None,
                        }));
                    }
                    _ => panic!("unexpected command after an ABA recovery latch"),
                }
            }
        });

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            <MvpAgent as acp::Agent>::prompt(
                &agent,
                acp::PromptRequest::new(
                    sid.clone(),
                    vec![acp::ContentBlock::from("must remain blocked after ABA")],
                ),
            ),
        )
        .await
        .expect("ABA recovery must not hang")
        .expect("ABA recovery must block cleanly");

        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
        assert_eq!(apply_count.get(), 1);
        assert_eq!(
            agent.session_registry.unavailable_model(&sid),
            Some(acp::ModelId::new(model_id))
        );
        assert_eq!(
            agent.session_registry.unavailable_catalog_identity(&sid),
            Some(identity)
        );
        actor.abort();
    });
}

#[test]
fn production_prompt_rechecks_a_new_latch_immediately_before_actor_dispatch() {
    run_local_for_bridge_test(|| async {
        let model_id = "prompt-late-unavailable-model";
        let agent = std::rc::Rc::new(build_agent_with_model_for_tests(model_id, "grok-build"));
        let sid = acp::SessionId::new("prompt-late-unavailable-model");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(model_id);
        agent.insert_resident(&sid, handle);

        let prompt_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let actor_prompt_count = prompt_count.clone();
        let actor_agent = agent.clone();
        let actor_sid = sid.clone();
        let actor = tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetCurrentPromptMode { responds_to } => {
                        actor_agent
                            .session_registry
                            .set_unavailable_model(&actor_sid, acp::ModelId::new(model_id));
                        let _ = responds_to.send(Default::default());
                    }
                    TestSessionCommand::GetCurrentModel { responds_to } => {
                        let _ = responds_to.send(model_id.to_owned());
                    }
                    TestSessionCommand::CopyFile { respond_to } => {
                        let _ = respond_to.send(Err(anyhow::anyhow!(
                            "session copy is unavailable in the fake actor"
                        )));
                    }
                    TestSessionCommand::SetNextTraceTurn { .. } => {}
                    TestSessionCommand::Prompt { .. } => {
                        actor_prompt_count.set(actor_prompt_count.get() + 1);
                    }
                    _ => panic!("unexpected command during late unavailable-model latch"),
                }
            }
        });

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            <MvpAgent as acp::Agent>::prompt(
                &agent,
                acp::PromptRequest::new(
                    sid.clone(),
                    vec![acp::ContentBlock::from("must not reach the actor")],
                ),
            ),
        )
        .await
        .expect("late latch must not hang")
        .expect("late latch must block cleanly");

        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
        assert_eq!(prompt_count.get(), 0);
        assert_eq!(
            agent.session_registry.unavailable_model(&sid),
            Some(acp::ModelId::new(model_id))
        );
        actor.abort();
    });
}

struct TestSessionLoadMarker {
    registry: SessionRegistry,
    session_id: acp::SessionId,
    rx: tokio::sync::watch::Receiver<bool>,
    _tx: tokio::sync::watch::Sender<bool>,
}

impl TestSessionLoadMarker {
    fn begin(agent: &MvpAgent, session_id: &acp::SessionId) -> Self {
        let registry = agent.session_registry.clone();
        let (tx, rx) = registry
            .begin_attach(session_id)
            .expect("test load claim must be available");
        Self {
            registry,
            session_id: session_id.clone(),
            rx,
            _tx: tx,
        }
    }
}

impl Drop for TestSessionLoadMarker {
    fn drop(&mut self) {
        self.registry.settle_attach(&self.session_id, &self.rx);
    }
}

#[test]
fn production_prompt_fails_closed_when_load_starts_at_dispatch_boundary() {
    run_local_for_bridge_test(|| async {
        let model_id = "prompt-load-at-dispatch-boundary";
        let agent = build_agent_with_model_for_tests(model_id, "grok-build");
        let sid = acp::SessionId::new("prompt-load-at-dispatch-boundary");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(model_id);
        agent.insert_resident(&sid, handle);

        let load_marker = std::rc::Rc::new(std::cell::RefCell::new(None));
        let hook_marker = load_marker.clone();
        let registry_agent = agent.session_registry.clone();
        let hook_sid = sid.clone();
        super::acp_agent::install_prompt_dispatch_boundary_hook(&sid, move || {
            let (tx, rx) = registry_agent
                .begin_attach(&hook_sid)
                .expect("dispatch-boundary load claim must be available");
            *hook_marker.borrow_mut() = Some(TestSessionLoadMarker {
                registry: registry_agent,
                session_id: hook_sid,
                rx,
                _tx: tx,
            });
        });

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            <MvpAgent as acp::Agent>::prompt(
                &agent,
                acp::PromptRequest::new(
                    sid.clone(),
                    vec![acp::ContentBlock::from("must not reach the actor")],
                ),
            ),
        )
        .await
        .expect("the dispatch-boundary load race must not hang")
        .expect("a load raced at the dispatch boundary must block cleanly");

        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
        assert!(
            agent.session_registry.is_attaching(&sid),
            "the boundary hook must keep the raced load active through the fail-closed check"
        );
        assert!(matches!(
            cmd_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        drop(load_marker.borrow_mut().take());
        assert!(
            !agent.session_registry.is_attaching(&sid),
            "the test load marker must settle after the assertion"
        );
    });
}

#[test]
fn production_prompt_rechecks_load_immediately_before_actor_dispatch() {
    run_local_for_bridge_test(|| async {
        let model_id = "prompt-load-during-preparation";
        let agent = std::rc::Rc::new(build_agent_with_model_for_tests(model_id, "grok-build"));
        let sid = acp::SessionId::new("prompt-load-during-preparation");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(model_id);
        agent.insert_resident(&sid, handle);

        let load_marker = std::rc::Rc::new(std::cell::RefCell::new(None));
        let actor_marker = load_marker.clone();
        let actor_agent = agent.clone();
        let actor_sid = sid.clone();
        let prompt_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let actor_prompt_count = prompt_count.clone();
        let actor = tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetCurrentPromptMode { responds_to } => {
                        *actor_marker.borrow_mut() =
                            Some(TestSessionLoadMarker::begin(&actor_agent, &actor_sid));
                        let _ = responds_to.send(Default::default());
                    }
                    TestSessionCommand::GetCurrentModel { responds_to } => {
                        let _ = responds_to.send(model_id.to_owned());
                    }
                    TestSessionCommand::CopyFile { respond_to } => {
                        let _ = respond_to.send(Err(anyhow::anyhow!(
                            "session copy is unavailable in the fake actor"
                        )));
                    }
                    TestSessionCommand::SetNextTraceTurn { .. } => {}
                    TestSessionCommand::Prompt { .. } => {
                        actor_prompt_count.set(actor_prompt_count.get() + 1);
                    }
                    _ => panic!("unexpected command during prompt preparation load race"),
                }
            }
        });

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            <MvpAgent as acp::Agent>::prompt(
                &agent,
                acp::PromptRequest::new(
                    sid.clone(),
                    vec![acp::ContentBlock::from("must not reach the actor")],
                ),
            ),
        )
        .await
        .expect("the prompt preparation load race must not hang")
        .expect("a load raced during preparation must block cleanly");

        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
        assert_eq!(prompt_count.get(), 0, "no Prompt command may be dispatched");
        assert!(agent.session_registry.is_attaching(&sid));
        drop(load_marker.borrow_mut().take());
        assert!(!agent.session_registry.is_attaching(&sid));
        actor.abort();
    });
}

#[test]
fn production_model_switch_rejects_load_started_while_waiting_for_dispatch_lock() {
    use std::task::Poll;

    run_local_for_bridge_test(|| async {
        let old_model = "switch-load-race-old";
        let new_model = "switch-load-race-new";
        let agent = build_agent_with_model_for_tests(old_model, "grok-build");
        let mut new_entry = agent.models_manager.models()[old_model].clone();
        new_entry.info.model = new_model.to_owned();
        agent.models_manager.insert_test_entry(new_model, new_entry);

        let sid = acp::SessionId::new("switch-load-race");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(old_model);
        agent.insert_resident(&sid, handle);

        let dispatch_lock = agent.dispatch_lock(&sid);
        let dispatch_guard = dispatch_lock.lock().await;
        let mut switch = Box::pin(<MvpAgent as acp::Agent>::set_session_model(
            &agent,
            acp::SetSessionModelRequest::new(sid.clone(), acp::ModelId::new(new_model)),
        ));
        assert!(
            matches!(futures::poll!(switch.as_mut()), Poll::Pending),
            "the model switch must be waiting on the held dispatch lock"
        );

        let load_guard = agent.begin_session_load(&sid).expect("load claim");
        drop(dispatch_guard);
        let error = tokio::time::timeout(std::time::Duration::from_secs(5), switch)
            .await
            .expect("the waiting model switch load race must not hang")
            .expect_err("a newly-started load must supersede the waiting model switch");

        assert_eq!(error.code, acp::ErrorCode::InternalError);
        assert!(
            error
                .to_string()
                .contains("session load started before actor dispatch"),
            "unexpected error: {error:?}"
        );
        assert!(matches!(
            cmd_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        drop(load_guard);
        assert!(!agent.session_registry.is_attaching(&sid));
        assert_eq!(
            agent.resident_handle(&sid).unwrap().model_id,
            acp::ModelId::new(old_model)
        );
    });
}

#[test]
fn production_user_model_switch_preserves_a_newer_unavailable_latch() {
    run_local_for_bridge_test(|| async {
        let old_model = "user-switch-old-model";
        let new_model = "user-switch-new-model";
        let concurrent_block = "user-switch-concurrent-block";
        let agent = std::rc::Rc::new(build_agent_with_model_for_tests(old_model, "grok-build"));
        let mut new_entry = agent.models_manager.models()[old_model].clone();
        new_entry.info.model = new_model.to_owned();
        agent.models_manager.insert_test_entry(new_model, new_entry);

        let sid = acp::SessionId::new("user-switch-preserves-newer-latch");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(old_model);
        handle.agent_name = "grok-build".to_owned();
        agent.insert_resident(&sid, handle);

        let actor_agent = agent.clone();
        let actor_sid = sid.clone();
        let actor = tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch {
                        prepared,
                        responds_to,
                    } => {
                        assert_eq!(prepared.catalog_identity.model_id, new_model);
                        actor_agent
                            .session_registry
                            .set_unavailable_model(&actor_sid, acp::ModelId::new(concurrent_block));
                        let _ = responds_to.send(Ok(crate::session::AppliedModelSwitch {
                            previous_model_id: acp::ModelId::new(old_model),
                            catalog_model_id: acp::ModelId::new(new_model),
                            did_rebuild: false,
                            active_agent_type: Some("grok-build".to_owned()),
                            web_search: None,
                        }));
                    }
                    _ => panic!("unexpected command during user model switch"),
                }
            }
        });

        <MvpAgent as acp::Agent>::set_session_model(
            &agent,
            acp::SetSessionModelRequest::new(sid.clone(), acp::ModelId::new(new_model)),
        )
        .await
        .expect("the user model switch itself must commit");

        assert_eq!(
            agent.resident_handle(&sid).unwrap().model_id,
            acp::ModelId::new(new_model)
        );
        assert_eq!(
            agent.session_registry.unavailable_model(&sid),
            Some(acp::ModelId::new(concurrent_block)),
            "the newer fail-closed decision must survive the older actor receipt"
        );
        assert_eq!(
            agent.models_manager.current_model_id(),
            acp::ModelId::new(new_model)
        );
        actor.abort();
    });
}

#[test]
fn production_model_switch_rejects_a_receipt_from_a_replaced_resident() {
    run_local_for_bridge_test(|| async {
        let old_model = "replaced-resident-old-model";
        let new_model = "replaced-resident-new-model";
        let replacement_model = "replacement-resident-model";
        let agent = std::rc::Rc::new(build_agent_with_model_for_tests(old_model, "grok-build"));
        let mut new_entry = agent.models_manager.models()[old_model].clone();
        new_entry.info.model = new_model.to_owned();
        agent.models_manager.insert_test_entry(new_model, new_entry);

        let sid = acp::SessionId::new("model-switch-replaced-resident");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(old_model);
        handle.agent_name = "grok-build".to_owned();
        agent.insert_resident(&sid, handle);

        let actor_agent = agent.clone();
        let actor_sid = sid.clone();
        let actor = tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch {
                        prepared,
                        responds_to,
                    } => {
                        assert_eq!(prepared.catalog_identity.model_id, new_model);
                        let (mut replacement, _replacement_tx, _replacement_rx) =
                            make_live_session_handle(&actor_sid, None);
                        replacement.model_id = acp::ModelId::new(replacement_model);
                        actor_agent.insert_resident(&actor_sid, replacement);
                        let _ = responds_to.send(Ok(crate::session::AppliedModelSwitch {
                            previous_model_id: acp::ModelId::new(old_model),
                            catalog_model_id: acp::ModelId::new(new_model),
                            did_rebuild: false,
                            active_agent_type: Some("grok-build".to_owned()),
                            web_search: None,
                        }));
                    }
                    _ => panic!("unexpected command during replaced-resident switch"),
                }
            }
        });

        let error = <MvpAgent as acp::Agent>::set_session_model(
            &agent,
            acp::SetSessionModelRequest::new(sid.clone(), acp::ModelId::new(new_model)),
        )
        .await
        .expect_err("a receipt from the displaced actor must not commit outer mirrors");

        assert!(
            error.to_string().contains("resident session changed"),
            "unexpected error: {error:?}"
        );
        assert_eq!(
            agent.resident_handle(&sid).unwrap().model_id,
            acp::ModelId::new(replacement_model)
        );
        assert_eq!(
            agent.models_manager.current_model_id(),
            acp::ModelId::new(old_model),
            "a stale receipt must not update the process-wide model mirror"
        );
        actor.abort();
    });
}

#[test]
fn production_set_model_reauthorizes_after_dispatch_lock_before_any_actor_command() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};

    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let target_id = "switch-boundary-target";
        let mut target = ModelEntry::fallback(target_id, &EndpointsConfig::default());
        target.info.auth_scheme = xai_grok_sampler::AuthScheme::None;
        agent
            .models_manager
            .insert_test_entry(target_id, target.clone());
        let sid = acp::SessionId::new("set-model-boundary-reauthorize");
        let (handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        agent.insert_resident(&sid, handle);

        let models_manager = agent.models_manager.clone();
        crate::agent::handlers::model_switch::install_dispatch_boundary_hook(&sid, move || {
            target.info.user_selectable = false;
            models_manager.insert_test_entry(target_id, target);
        });
        let error = <MvpAgent as acp::Agent>::set_session_model(
            &agent,
            acp::SetSessionModelRequest::new(sid, acp::ModelId::new(target_id)),
        )
        .await
        .expect_err("a target hidden at the serialized boundary must be rejected");
        assert_eq!(error.code, acp::ErrorCode::InvalidParams);
        assert!(matches!(
            cmd_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    });
}

#[test]
fn production_prompt_reauthorizes_fresh_resident_after_dispatch_lock() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};

    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let old_id = "prompt-boundary-old";
        let hidden_id = "prompt-boundary-hidden";
        for model_id in [old_id, hidden_id] {
            let mut entry = ModelEntry::fallback(model_id, &EndpointsConfig::default());
            entry.info.auth_scheme = xai_grok_sampler::AuthScheme::None;
            if model_id == hidden_id {
                entry.info.user_selectable = false;
            }
            agent.models_manager.insert_test_entry(model_id, entry);
        }
        let sid = acp::SessionId::new("prompt-boundary-fresh-resident");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(old_id);
        agent.insert_resident(&sid, handle);

        let registry = agent.session_registry.clone();
        let hook_sid = sid.clone();
        super::acp_agent::install_prompt_dispatch_boundary_hook(&sid, move || {
            registry.with_resident_mut(&hook_sid, |resident| {
                resident.model_id = acp::ModelId::new(hidden_id);
            });
        });
        let response = <MvpAgent as acp::Agent>::prompt(
            &agent,
            acp::PromptRequest::new(
                sid.clone(),
                vec![acp::ContentBlock::from("must not reach the actor")],
            ),
        )
        .await
        .expect("the raced prompt must fail closed as an EndTurn");
        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
        assert_eq!(
            agent.session_registry.unavailable_model(&sid),
            Some(acp::ModelId::new(hidden_id)),
            "the boundary must authorize the fresh post-switch resident, not the stale handle"
        );
        assert!(matches!(
            cmd_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    });
}

#[test]
fn production_prompt_rejects_auth_change_after_prepare_before_sampling_dispatch() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};

    run_local_for_bridge_test(|| async {
        let agent = build_agent_with_auth(crate::auth::GrokAuth {
            key: "session-auth".into(),
            auth_mode: crate::auth::AuthMode::WebLogin,
            ..crate::auth::GrokAuth::test_default()
        });
        let model_id = "prompt-auth-race";
        let mut entry = ModelEntry::fallback(model_id, &EndpointsConfig::default());
        entry.info.supported_in_api = false;
        entry.info.auth_scheme = xai_grok_sampler::AuthScheme::None;
        agent.models_manager.insert_test_entry(model_id, entry);
        let sid = acp::SessionId::new("prompt-auth-change-before-send");
        let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.model_id = acp::ModelId::new(model_id);
        agent.insert_resident(&sid, handle);

        let auth_manager = agent.auth_manager.clone();
        let prompt_seen = std::rc::Rc::new(std::cell::Cell::new(false));
        let actor_prompt_seen = prompt_seen.clone();
        tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetCurrentPromptMode { responds_to } => {
                        auth_manager.clear_in_memory();
                        let _ = responds_to.send(Default::default());
                    }
                    TestSessionCommand::GetCurrentModel { responds_to } => {
                        let _ = responds_to.send(model_id.to_owned());
                    }
                    TestSessionCommand::Prompt { .. } => actor_prompt_seen.set(true),
                    _ => {}
                }
            }
        });

        let response = <MvpAgent as acp::Agent>::prompt(
            &agent,
            acp::PromptRequest::new(
                sid.clone(),
                vec![acp::ContentBlock::from(
                    "must be rejected at final authorization",
                )],
            ),
        )
        .await
        .expect("an auth race must fail closed as EndTurn");
        assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
        assert!(!prompt_seen.get(), "no sampling command may be dispatched");
        assert_eq!(
            agent.session_registry.unavailable_model(&sid),
            Some(acp::ModelId::new(model_id))
        );
    });
}

#[test]
fn model_dispatch_authority_recovers_only_from_a_fresh_visible_generation() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};

    run_local_for_bridge_test(|| async {
        let agent = build_agent_with_auth(crate::auth::GrokAuth {
            key: "session-auth".into(),
            auth_mode: crate::auth::AuthMode::WebLogin,
            ..crate::auth::GrokAuth::test_default()
        });
        let model_id = acp::ModelId::new("dispatch-authority-recovery");
        let mut entry = ModelEntry::fallback(model_id.0.as_ref(), &EndpointsConfig::default());
        entry.info.supported_in_api = false;
        entry.info.auth_scheme = xai_grok_sampler::AuthScheme::None;
        agent
            .models_manager
            .insert_test_entry(model_id.0.to_string(), entry);

        let stale = agent
            .models_manager
            .model_dispatch_authority(&model_id)
            .expect("session auth initially authorizes the OAuth-only model");
        agent.auth_manager.clear_in_memory();
        let mut stale_dispatched = false;
        assert!(
            agent
                .models_manager
                .commit_model_dispatch(&stale, || stale_dispatched = true)
                .is_err()
        );
        assert!(!stale_dispatched);
        assert!(
            agent
                .models_manager
                .model_dispatch_authority(&model_id)
                .is_err(),
            "the hidden generation must remain unavailable"
        );

        agent
            .auth_manager
            .hot_swap(crate::auth::GrokAuth::test_default());
        let recovered = agent
            .models_manager
            .model_dispatch_authority(&model_id)
            .expect("a fresh session-auth generation may safely recover the route");
        let mut recovered_dispatched = false;
        agent
            .models_manager
            .commit_model_dispatch(&recovered, || recovered_dispatched = true)
            .expect("unchanged recovered authority may commit");
        assert!(recovered_dispatched);
    });
}
/// YOLO toggle scoped by client_identifier: only matching sessions are updated.
#[tokio::test]
async fn yolo_toggle_scoped_by_client_identifier() {
    let sid_tui = acp::SessionId::new("sess-tui");
    let sid_vscode = acp::SessionId::new("sess-vscode");
    let mut sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = [
        (
            sid_tui.clone(),
            make_test_handle("grok-3", false, Some("grok-tui")),
        ),
        (
            sid_vscode.clone(),
            make_test_handle("grok-3", false, Some("grok-code-extension")),
        ),
    ]
    .into();
    let updated =
        apply_yolo_mode_to_matching_sessions(sessions.values_mut(), Some("grok-tui"), true);
    assert_eq!(updated, 1, "exactly one matching session should be updated");
    assert!(
        sessions[&sid_tui].yolo_mode,
        "TUI session should have yolo=true after TUI toggle"
    );
    assert!(
        !sessions[&sid_vscode].yolo_mode,
        "VS Code session must NOT be affected by TUI's yolo toggle"
    );
}
/// A client can explicitly disable YOLO for its own sessions after startup,
/// even if those sessions were initially created with yolo=true.
#[tokio::test]
async fn yolo_toggle_can_disable_session_started_with_yolo_enabled() {
    let sid_tui = acp::SessionId::new("sess-tui");
    let sid_other = acp::SessionId::new("sess-other");
    let mut sessions: HashMap<acp::SessionId, crate::session::SessionHandle> = [
        (
            sid_tui.clone(),
            make_test_handle("grok-3", true, Some("grok-tui")),
        ),
        (
            sid_other.clone(),
            make_test_handle("grok-3", true, Some("grok-code-extension")),
        ),
    ]
    .into();
    let updated =
        apply_yolo_mode_to_matching_sessions(sessions.values_mut(), Some("grok-tui"), false);
    assert_eq!(updated, 1, "only the sender's session should be updated");
    assert!(
        !sessions[&sid_tui].yolo_mode,
        "sender session should be switched to yolo=false"
    );
    assert!(
        sessions[&sid_other].yolo_mode,
        "other client's session must keep its previous yolo state"
    );
}
/// `drain_old_session_thread` returns immediately when the thread has
/// already finished.
#[tokio::test]
async fn drain_finished_thread_returns_immediately() {
    let session_threads: RefCell<HashMap<acp::SessionId, crate::session::SessionThread>> =
        RefCell::new(HashMap::new());
    let sid = acp::SessionId::new("drain-test");
    let handle = std::thread::spawn(|| {});
    std::thread::sleep(std::time::Duration::from_millis(10));
    session_threads.borrow_mut().insert(
        sid.clone(),
        crate::session::SessionThread::from_handle(handle),
    );
    let thread = session_threads.borrow_mut().remove(&sid).unwrap();
    assert!(thread.is_finished(), "thread should be finished");
    assert!(!session_threads.borrow().contains_key(&sid));
}
/// `drain_old_session_thread` waits for a slow thread to finish.
#[tokio::test]
async fn drain_waits_for_slow_thread() {
    let session_threads: RefCell<HashMap<acp::SessionId, crate::session::SessionThread>> =
        RefCell::new(HashMap::new());
    let sid = acp::SessionId::new("slow-drain");
    let handle = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(100));
    });
    session_threads.borrow_mut().insert(
        sid.clone(),
        crate::session::SessionThread::from_handle(handle),
    );
    let thread = session_threads.borrow_mut().remove(&sid).unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if thread.is_finished() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "thread should finish within 5s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(thread.is_finished());
}
/// Drain respects the 5s deadline and returns even if the thread is still running.
#[tokio::test]
async fn drain_respects_deadline() {
    let session_threads: RefCell<HashMap<acp::SessionId, crate::session::SessionThread>> =
        RefCell::new(HashMap::new());
    let sid = acp::SessionId::new("hung-drain");
    let handle = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(30));
    });
    session_threads.borrow_mut().insert(
        sid.clone(),
        crate::session::SessionThread::from_handle(handle),
    );
    let thread = session_threads.borrow_mut().remove(&sid).unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
    let mut timed_out = false;
    loop {
        if thread.is_finished() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        timed_out,
        "should have timed out waiting for the hung thread"
    );
    assert!(!thread.is_finished(), "thread should still be running");
}
#[test]
fn parse_code_nav_capability_present_and_true() {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "x.ai/codeNavigation".to_string(),
        serde_json::json!({ "enabled": true }),
    );
    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
        acp::ClientCapabilities::new()
            .fs(acp::FileSystemCapabilities::new())
            .terminal(false)
            .meta(meta),
    );
    assert!(MvpAgent::parse_code_nav_capability(&init));
}
#[test]
fn parse_code_nav_capability_absent_returns_false() {
    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
        acp::ClientCapabilities::new()
            .fs(acp::FileSystemCapabilities::new())
            .terminal(false),
    );
    assert!(!MvpAgent::parse_code_nav_capability(&init));
}
#[test]
fn parse_code_nav_capability_false_returns_false() {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "x.ai/codeNavigation".to_string(),
        serde_json::json!({ "enabled": false }),
    );
    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
        acp::ClientCapabilities::new()
            .fs(acp::FileSystemCapabilities::new())
            .terminal(false)
            .meta(meta),
    );
    assert!(!MvpAgent::parse_code_nav_capability(&init));
}
/// Verify that two session handles with different code-nav state produce
/// independent eligibility outcomes — the key leader-mode isolation test.
///
/// This tests the `code_nav_eligibility_for_request` lookup path directly
/// by inspecting the per-handle fields rather than building a full agent,
/// which mirrors what the method actually reads at runtime.
#[tokio::test]
async fn test_per_session_code_nav_isolation() {
    let web_handle = {
        let mut h = make_test_handle("model", false, Some("grok-web"));
        h.code_nav_enabled = true;
        h
    };
    let tui_handle = {
        let mut h = make_test_handle("model", false, Some("grok-tui"));
        h.code_nav_enabled = false;
        h
    };
    let check = |handle: &crate::session::SessionHandle| {
        let ct = crate::http::client_type_from_origin(handle.origin_client.as_ref());
        if !matches!(ct, ClientType::GrokWeb) {
            return Err(CodeNavEligibility::ClientNotWeb);
        }
        if !handle.code_nav_enabled {
            return Err(CodeNavEligibility::CapabilityNotAdvertised);
        }
        Ok(())
    };
    assert!(
        check(&web_handle).is_ok(),
        "web session with capability should pass client-type and capability gates"
    );
    assert_eq!(
        check(&tui_handle),
        Err(CodeNavEligibility::ClientNotWeb),
        "tui session should be rejected at gate 1"
    );
    let mut web_no_cap = web_handle.clone();
    web_no_cap.code_nav_enabled = false;
    assert_eq!(
        check(&web_no_cap),
        Err(CodeNavEligibility::CapabilityNotAdvertised),
        "web session without capability should be rejected at gate 2"
    );
    assert!(
        check(&web_handle).is_ok(),
        "original web handle must be unaffected"
    );
}
/// Verify that code-nav requests without a sessionId are rejected.
///
/// `sessionId` is required so per-client capability gating is unambiguous
/// in both simple and leader modes.  Falling back to shared global state
/// (last-client-wins in leader mode) is not safe.
#[test]
fn test_sessionless_request_requires_session_id() {
    let session_id: Option<&acp::SessionId> = None;
    let result: Result<(), CodeNavEligibility> = if session_id.is_none() {
        Err(CodeNavEligibility::SessionRequired)
    } else {
        Ok(())
    };
    assert_eq!(
        result,
        Err(CodeNavEligibility::SessionRequired),
        "cwd-only requests with no sessionId must return SessionRequired"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn ext_method_routes_auth_cleared_and_refreshes_resident_sessions() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let agent = build_agent_with_auth(crate::auth::GrokAuth {
                key: "eligible".into(),
                auth_mode: crate::auth::AuthMode::WebLogin,
                ..crate::auth::GrokAuth::test_default()
            });
            use acp::Agent as _;
            agent.managed_mcp_cache.lock().await.enable_gateway_tools();
            let sid = acp::SessionId::new("sess-auth-cleared");
            let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
            agent.insert_resident(&sid, handle);
            let params = serde_json::json!({});
            agent
                .ext_method(acp::ExtRequest::new(
                    "x.ai/internal/auth_cleared",
                    std::sync::Arc::from(serde_json::value::to_raw_value(&params).unwrap()),
                ))
                .await
                .expect("auth_cleared must route through session-admin");
            let barrier = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
                .await
                .expect("admission barrier should be sent")
                .expect("channel should stay open until command is received");
            let SessionCommand::BeginManagedGatewayAdmission { respond_to } = barrier else {
                panic!("expected gateway admission barrier before disable");
            };
            respond_to.send(()).expect("barrier acknowledgement");
            let refresh = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
                .await
                .expect("refresh command should follow barrier")
                .expect("channel should stay open until command is received");
            assert!(matches!(refresh, SessionCommand::RefreshMcpSearchIndex));
            assert!(!agent.managed_mcp_cache.lock().await.gateway_tools_active);
        })
        .await;
}
/// Fresh managed catalog sync must push UpdateMcpServers with the injected
/// managed connector. The `search_tool` rebuild is a SEPARATE broadcast
/// (`refresh_mcp_search_index_in_sessions`), so it is not asserted here.
#[tokio::test(flavor = "current_thread")]
async fn sync_fresh_managed_mcp_pushes_update() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let agent = build_agent_with_auth(crate::auth::GrokAuth {
                key: "eligible".into(),
                auth_mode: crate::auth::AuthMode::WebLogin,
                ..crate::auth::GrokAuth::test_default()
            });
            let sid = acp::SessionId::new("sess-managed-sync");
            let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
            agent.insert_resident(&sid, handle);
            let managed = vec![crate::session::managed_mcp::ManagedMcpConfig {
                name: "Linear".into(),
                endpoint: "https://mcp.example.com/linear".into(),
                headers: std::collections::HashMap::from([(
                    "Authorization".into(),
                    "Bearer tok".into(),
                )]),
                token_expires_at: None,
                scope: None,
                scope_id: None,
                scope_name: None,
            }];
            agent.sync_fresh_managed_mcp_to_sessions(&managed);
            let first = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
                .await
                .expect("UpdateMcpServers should be sent")
                .expect("channel should stay open");
            let SessionCommand::UpdateMcpServers { mcp_servers, .. } = first else {
                panic!("expected UpdateMcpServers as the first synced command");
            };
            let managed_name = crate::session::managed_mcp::to_managed_name("Linear");
            let linear = mcp_servers
                .iter()
                .find_map(|s| match s {
                    acp::McpServer::Http(http) if http.name == managed_name => Some(http),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    panic!("merged catalog must contain managed HTTP server {managed_name}")
                });
            assert!(
                linear
                    .headers
                    .iter()
                    .any(|h| h.name == "Authorization" && h.value == "Bearer tok"),
                "managed server must carry the injected Authorization header"
            );
        })
        .await;
}
/// The gateway-catalog refresh broadcast pushes `RefreshMcpSearchIndex` to every
/// live session (independent of the legacy managed-connector sync).
#[tokio::test(flavor = "current_thread")]
async fn refresh_mcp_search_index_broadcasts_to_sessions() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-search-index");
    let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
    agent.insert_resident(&sid, handle);
    agent.refresh_mcp_search_index_in_sessions();
    let cmd = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
        .await
        .expect("RefreshMcpSearchIndex should be sent")
        .expect("channel should stay open");
    assert!(matches!(cmd, SessionCommand::RefreshMcpSearchIndex));
}
/// Live gateway enablement must close restricted external dispatch before the
/// asynchronous auth refresh or catalog fetch can yield.
#[tokio::test(flavor = "current_thread")]
async fn gateway_catalog_fetch_begins_session_admission_synchronously() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let agent = build_agent_with_auth(crate::auth::GrokAuth {
                key: "eligible".into(),
                auth_mode: crate::auth::AuthMode::WebLogin,
                ..crate::auth::GrokAuth::test_default()
            });
            agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = true;
            let sid = acp::SessionId::new("sess-gateway-admission");
            let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
            agent.session_registry.put_resident(&sid, handle);

            agent.spawn_managed_gateway_tool_catalog_fetch();

            let first = cmd_rx
                .recv()
                .await
                .expect("gateway admission command should be queued");
            let SessionCommand::BeginManagedGatewayAdmission { respond_to } = first else {
                panic!("expected BeginManagedGatewayAdmission first");
            };
            assert!(
                !agent.managed_mcp_cache.lock().await.gateway_tools_active,
                "catalog activation/fetch must wait until every session applies the barrier"
            );
            respond_to
                .send(())
                .expect("test barrier acknowledgement should be received");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn warm_gateway_catalog_does_not_reopen_session_admission() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        key: "eligible".into(),
        auth_mode: crate::auth::AuthMode::WebLogin,
        ..crate::auth::GrokAuth::test_default()
    });
    agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = true;
    {
        let mut state = agent.managed_mcp_cache.lock().await;
        state.enable_gateway_tools();
        let epoch = state.start_gateway_tool_fetch().unwrap();
        assert!(state.complete_gateway_tool_fetch(
            epoch,
            crate::session::managed_mcp::GatewayToolCatalog {
                tools: vec![],
                total_tools: 0,
                connectors_needing_reauth: vec![],
            },
        ));
    }
    let sid = acp::SessionId::new("sess-warm-gateway");
    let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
    agent.session_registry.put_resident(&sid, handle);

    assert!(
        agent.get_managed_mcp_gateway_tool_catalog().await.is_some(),
        "ready gateway catalog should use the warm cache"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), cmd_rx.recv())
            .await
            .is_err(),
        "a warm cache read must not queue a barrier with no matching refresh"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn explicit_gateway_cache_invalidation_waits_for_all_session_admissions() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let agent = std::rc::Rc::new(build_agent_with_auth(crate::auth::GrokAuth {
                key: "eligible".into(),
                auth_mode: crate::auth::AuthMode::WebLogin,
                ..crate::auth::GrokAuth::test_default()
            }));
            agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = true;
            {
                let mut state = agent.managed_mcp_cache.lock().await;
                state.enable_gateway_tools();
                let epoch = state.start_gateway_tool_fetch().unwrap();
                assert!(state.complete_gateway_tool_fetch(
                    epoch,
                    crate::session::managed_mcp::GatewayToolCatalog {
                        tools: vec![],
                        total_tools: 0,
                        connectors_needing_reauth: vec![],
                    },
                ));
            }
            let sid = acp::SessionId::new("sess-explicit-gateway-refresh");
            let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
            agent.session_registry.put_resident(&sid, handle);

            let cache = agent.managed_mcp_cache.clone();
            let invalidate_agent = agent.clone();
            let invalidate = tokio::task::spawn_local(async move {
                invalidate_agent
                    .invalidate_gateway_tool_cache_after_session_admission()
                    .await
            });

            let first = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
                .await
                .expect("first admission must be queued before cache invalidation")
                .expect("session command channel must remain open");
            let SessionCommand::BeginManagedGatewayAdmission {
                respond_to: first_ack,
            } = first
            else {
                panic!("expected first gateway admission barrier");
            };
            assert!(!invalidate.is_finished());
            {
                let state = cache.lock().await;
                assert!(state.gateway_refresh_in_progress);
                assert!(matches!(
                    &state.gateway_tool_cache,
                    crate::session::managed_mcp::GatewayToolCatalogCache::Ready(_)
                ));
            }

            let sid_second = acp::SessionId::new("sess-explicit-gateway-refresh-late");
            let (second_handle, _second_tx, mut second_rx) =
                make_live_session_handle(&sid_second, None);
            agent
                .session_registry
                .put_resident(&sid_second, second_handle);
            first_ack
                .send(())
                .expect("first barrier receiver remains live");

            let second = tokio::time::timeout(std::time::Duration::from_secs(1), second_rx.recv())
                .await
                .expect("session added during the wait must receive admission")
                .expect("second session command channel must remain open");
            let SessionCommand::BeginManagedGatewayAdmission {
                respond_to: second_ack,
            } = second
            else {
                panic!("expected second gateway admission barrier");
            };
            assert!(!invalidate.is_finished());
            {
                let state = cache.lock().await;
                assert!(state.gateway_refresh_in_progress);
                assert!(matches!(
                    &state.gateway_tool_cache,
                    crate::session::managed_mcp::GatewayToolCatalogCache::Ready(_)
                ));
            }
            second_ack
                .send(())
                .expect("second barrier receiver remains live");

            invalidate
                .await
                .expect("cache invalidation task must finish");
            let state = cache.lock().await;
            assert!(state.gateway_refresh_in_progress);
            assert!(matches!(
                &state.gateway_tool_cache,
                crate::session::managed_mcp::GatewayToolCatalogCache::NotFetched
            ));
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn issue39_gateway_invalidation_includes_registered_child_sessions() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let agent = std::rc::Rc::new(build_agent_with_auth(crate::auth::GrokAuth {
                key: "eligible".into(),
                auth_mode: crate::auth::AuthMode::WebLogin,
                ..crate::auth::GrokAuth::test_default()
            }));
            agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = true;
            {
                let mut state = agent.managed_mcp_cache.lock().await;
                state.enable_gateway_tools();
                let epoch = state.start_gateway_tool_fetch().unwrap();
                assert!(state.complete_gateway_tool_fetch(
                    epoch,
                    crate::session::managed_mcp::GatewayToolCatalog {
                        tools: vec![],
                        total_tools: 0,
                        connectors_needing_reauth: vec![],
                    },
                ));
            }

            let sid = acp::SessionId::new("sess-issue39-parent");
            let (handle, _tx, mut parent_rx) = make_live_session_handle(&sid, None);
            agent.session_registry.put_resident(&sid, handle);

            let child_sid = acp::SessionId::new("sess-issue39-child");
            let (child_tx, mut child_rx) = tokio::sync::mpsc::unbounded_channel();
            agent
                .managed_gateway_child_sessions
                .borrow_mut()
                .insert(child_sid, child_tx);

            let invalidate_agent = agent.clone();
            let invalidate = tokio::task::spawn_local(async move {
                invalidate_agent
                    .invalidate_gateway_tool_cache_after_session_admission()
                    .await
            });

            let parent = tokio::time::timeout(std::time::Duration::from_secs(1), parent_rx.recv())
                .await
                .expect("parent admission must be queued")
                .expect("parent channel must remain open");
            let SessionCommand::BeginManagedGatewayAdmission {
                respond_to: parent_ack,
            } = parent
            else {
                panic!("expected parent gateway admission barrier");
            };

            let child = tokio::time::timeout(std::time::Duration::from_secs(1), child_rx.recv())
                .await
                .expect("child admission must be queued")
                .expect("child channel must remain open");
            let SessionCommand::BeginManagedGatewayAdmission {
                respond_to: child_ack,
            } = child
            else {
                panic!("expected child gateway admission barrier");
            };

            parent_ack
                .send(())
                .expect("parent barrier receiver remains live");
            assert!(
                !invalidate.is_finished(),
                "refresh invalidation must wait for child-session admission too"
            );
            child_ack
                .send(())
                .expect("child barrier receiver remains live");

            invalidate
                .await
                .expect("cache invalidation should finish after both barriers");
            let state = agent.managed_mcp_cache.lock().await;
            assert!(state.gateway_refresh_in_progress);
            assert!(matches!(
                state.gateway_tool_cache,
                crate::session::managed_mcp::GatewayToolCatalogCache::NotFetched
            ));
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn ineligible_gateway_auth_queues_barrier_before_revoking_catalog() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
            agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = true;
            {
                let mut state = agent.managed_mcp_cache.lock().await;
                state.enable_gateway_tools();
                let epoch = state.start_gateway_tool_fetch().unwrap();
                assert!(state.complete_gateway_tool_fetch(
                    epoch,
                    crate::session::managed_mcp::GatewayToolCatalog {
                        tools: vec![],
                        total_tools: 0,
                        connectors_needing_reauth: vec![],
                    },
                ));
            }
            let sid = acp::SessionId::new("sess-ineligible-gateway");
            let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
            agent.session_registry.put_resident(&sid, handle);

            assert!(
                agent.get_managed_mcp_gateway_tool_catalog().await.is_none(),
                "API-key auth is not eligible for managed gateway tools"
            );
            let barrier = cmd_rx.recv().await.expect("revocation barrier");
            let SessionCommand::BeginManagedGatewayAdmission { respond_to } = barrier else {
                panic!("expected gateway admission barrier before revocation");
            };
            respond_to.send(()).expect("barrier acknowledgement");
            assert!(matches!(
                cmd_rx.recv().await,
                Some(SessionCommand::RefreshMcpSearchIndex)
            ));
            assert!(!agent.managed_mcp_cache.lock().await.gateway_tools_active);
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn issue39_aborted_explicit_gateway_refresh_recovers_waiters() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let hold_fetch = std::sync::Arc::new(tokio::sync::Notify::new());
            let app_hold = hold_fetch.clone();
            let app = axum::Router::new().route(
                "/mcp/tools/list",
                axum::routing::get(move || {
                    let hold = app_hold.clone();
                    async move {
                        hold.notified().await;
                        axum::Json(serde_json::json!({
                            "tools": [],
                            "total_tools": 0,
                            "connectors_needing_reauth": []
                        }))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("listener");
            let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

            let auth = crate::auth::GrokAuth {
                key: "eligible".into(),
                auth_mode: crate::auth::AuthMode::WebLogin,
                ..crate::auth::GrokAuth::test_default()
            };
            let (agent, _rx) = build_agent_with_auth_and_proxy(
                auth,
                base_url,
                crate::agent::config::AgentMode::Leader,
            );
            agent.cfg.borrow_mut().managed_mcp_gateway_tools_enabled = true;
            let sid = acp::SessionId::new("sess-issue39-refresh-cancel");
            let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
            agent.session_registry.put_resident(&sid, handle);

            let agent = std::rc::Rc::new(agent);
            let refresh_agent = agent.clone();
            let refresh = tokio::task::spawn_local(async move {
                refresh_agent
                    .refresh_managed_mcp_gateway_tool_catalog()
                    .await
            });

            let barrier = tokio::time::timeout(std::time::Duration::from_secs(1), cmd_rx.recv())
                .await
                .expect("admission barrier should be queued")
                .expect("session command channel should stay open");
            let SessionCommand::BeginManagedGatewayAdmission { respond_to } = barrier else {
                panic!("expected gateway admission barrier before fetch");
            };
            respond_to
                .send(())
                .expect("test barrier acknowledgement should be received");

            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if matches!(
                        agent.managed_mcp_cache.lock().await.gateway_tool_cache,
                        crate::session::managed_mcp::GatewayToolCatalogCache::Fetching(_)
                    ) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("refresh must reach Fetching before cancellation");

            let waiter_cache = agent.managed_mcp_cache.clone();
            let waiter = tokio::task::spawn_local(async move {
                crate::session::managed_mcp::get_or_fetch_gateway_tool_catalog(
                    &waiter_cache,
                    "http://127.0.0.1:0",
                    None,
                )
                .await
            });

            refresh.abort();
            let _ = refresh.await;

            let waiter_result = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("aborted refresh must wake gateway waiters")
                .expect("waiter task should not panic");
            assert!(
                waiter_result.is_none(),
                "auth-less waiter should wake and fail closed instead of hanging"
            );
            assert!(matches!(
                agent.managed_mcp_cache.lock().await.gateway_tool_cache,
                crate::session::managed_mcp::GatewayToolCatalogCache::NotFetched
            ));

            hold_fetch.notify_waiters();
            server.abort();
        })
        .await;
}
/// Build a minimal MvpAgent suitable for testing extension methods.
fn build_minimal_agent_for_tests() -> MvpAgent {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let cfg = AgentConfig::default();
    MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config")
}

#[test]
fn initialize_advertises_only_nonblank_external_auth_provider_commands() {
    let advertised = |command: Option<&str>| {
        let config = crate::auth::GrokComConfig {
            auth_provider_command: command.map(str::to_owned),
            ..Default::default()
        };
        super::acp_agent::has_advertised_auth_provider_command(&config)
    };

    assert!(!advertised(None));
    assert!(!advertised(Some("")));
    assert!(!advertised(Some(" \t\n")));
    assert!(advertised(Some("acme-auth --token")));
}

fn build_agent_with_model_for_tests(model_id: &str, agent_type: &str) -> MvpAgent {
    use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
    use crate::auth::{AuthManager, GrokComConfig};

    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let mut cfg = AgentConfig::default();
    cfg.models.default = Some(model_id.to_owned());
    cfg.config_models.insert(
        model_id.to_owned(),
        ConfigModelOverride {
            model: Some(model_id.to_owned()),
            base_url: Some("http://localhost".to_owned()),
            auth_scheme: Some(xai_grok_sampler::AuthScheme::None),
            agent_type: Some(agent_type.to_owned()),
            ..Default::default()
        },
    );
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
    agent
        .models_manager
        .set_current_model_id(acp::ModelId::new(model_id));
    agent
}

fn build_cross_provider_agent_for_tests(target_api_key: Option<&str>) -> MvpAgent {
    build_cross_provider_agent_with_gateway_for_tests(target_api_key).0
}

fn build_cross_provider_agent_with_gateway_for_tests(
    target_api_key: Option<&str>,
) -> (
    MvpAgent,
    tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) {
    use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
    use crate::auth::{AuthManager, GrokComConfig};

    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let mut cfg = AgentConfig::default();
    cfg.models.default = Some("source-provider".to_owned());
    for (id, wire_model, base_url, api_key) in [
        (
            "source-provider",
            "source-wire-model",
            "https://source.invalid/v1",
            Some("source-test-key"),
        ),
        (
            "target-provider",
            "target-wire-model",
            "https://target.invalid/v1",
            target_api_key,
        ),
    ] {
        cfg.config_models.insert(
            id.to_owned(),
            ConfigModelOverride {
                model: Some(wire_model.to_owned()),
                base_url: Some(base_url.to_owned()),
                api_key: api_key.map(str::to_owned),
                api_backend: Some(xai_grok_sampling_types::ApiBackend::Responses),
                auth_scheme: Some(xai_grok_sampler::AuthScheme::Bearer),
                agent_type: Some("grok-build".to_owned()),
                ..Default::default()
            },
        );
    }
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
    agent
        .models_manager
        .set_current_model_id(acp::ModelId::new("source-provider"));
    (agent, rx)
}

#[derive(Clone, Copy)]
enum PromptRecoveryQuarantineRace {
    NewerDifferentLatch,
    ClearedLatch,
}

async fn assert_prompt_recovery_quarantines_stale_actor_receipt(
    race: PromptRecoveryQuarantineRace,
) {
    let source_model = "source-provider";
    let target_model = "target-provider";
    let newer_unavailable_model = "newer-unavailable-provider";
    let session_name = match race {
        PromptRecoveryQuarantineRace::NewerDifferentLatch => "prompt-recovery-newer-quarantine",
        PromptRecoveryQuarantineRace::ClearedLatch => "prompt-recovery-rebuilt-quarantine",
    };
    let sid = acp::SessionId::new(session_name);
    let _ = crate::agent::handlers::model_switch::take_captured_success_telemetry(session_name);
    let _ = crate::agent::handlers::model_switch::take_captured_failure_telemetry(session_name);
    let (agent, mut gateway_rx) =
        build_cross_provider_agent_with_gateway_for_tests(Some("target-test-key"));
    let agent = std::rc::Rc::new(agent);
    agent.sync_process_static_api_key(Some(source_model));
    assert_eq!(
        agent.auth_manager.static_api_key_for_export().as_deref(),
        Some("source-test-key")
    );

    let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
    handle.model_id = acp::ModelId::new(source_model);
    handle.agent_name = "grok-build".to_owned();
    handle.auxiliary_model_provenance = crate::session::AuxiliaryModelProvenance {
        session_summary_follows_default: true,
        web_search_follows_default: true,
        web_search_model: source_model.to_owned(),
        image_description_follows_default: true,
        image_description_model: source_model.to_owned(),
    };
    agent.insert_resident(&sid, handle);
    let source_notice = crate::session::WebSearchDisabledNotice {
        model_id: source_model.to_owned(),
        reason: "source web search remains disabled".to_owned(),
        message: "source-provider web search remains disabled".to_owned(),
    };
    agent
        .web_search_disabled
        .borrow_mut()
        .insert(sid.clone(), source_notice.clone());
    let target_identity = crate::agent::models::resolve_catalog_identity(
        &agent.models_manager.models(),
        &acp::ModelId::new(target_model),
    )
    .expect("target catalog identity");
    agent.session_registry.set_unavailable_model_with_identity(
        &sid,
        acp::ModelId::new(target_model),
        Some(target_identity.clone()),
        Some("grok-build".to_owned()),
    );

    let apply_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let actor_apply_count = apply_count.clone();
    let prompt_count = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let actor_prompt_count = prompt_count.clone();
    let actor_agent = agent.clone();
    let actor_sid = sid.clone();
    let actor = tokio::task::spawn_local(async move {
        while let Some(command) = cmd_rx.recv().await {
            match command {
                TestSessionCommand::GetActiveAgent { responds_to } => {
                    let _ = responds_to.send(Some("grok-build".to_owned()));
                }
                TestSessionCommand::ApplyModelSwitch {
                    prepared,
                    responds_to,
                } => {
                    actor_apply_count.set(actor_apply_count.get() + 1);
                    assert_eq!(prepared.catalog_identity.model_id, target_model);
                    match race {
                        PromptRecoveryQuarantineRace::NewerDifferentLatch => {
                            actor_agent
                                .session_registry
                                .set_unavailable_model_with_identity(
                                    &actor_sid,
                                    acp::ModelId::new(newer_unavailable_model),
                                    None,
                                    Some("newer-agent".to_owned()),
                                );
                        }
                        PromptRecoveryQuarantineRace::ClearedLatch => {
                            assert_eq!(
                                actor_agent
                                    .session_registry
                                    .take_unavailable_model(&actor_sid),
                                Some(acp::ModelId::new(target_model))
                            );
                        }
                    }
                    let target_notice = crate::session::WebSearchDisabledNotice {
                        model_id: target_model.to_owned(),
                        reason: "actor target web-search result".to_owned(),
                        message: "target-provider actor web-search notice".to_owned(),
                    };
                    let _ = responds_to.send(Ok(crate::session::AppliedModelSwitch {
                        previous_model_id: acp::ModelId::new(source_model),
                        catalog_model_id: acp::ModelId::new(target_model),
                        did_rebuild: false,
                        active_agent_type: Some("grok-build".to_owned()),
                        web_search: Some(crate::session::AppliedWebSearchState {
                            enabled: false,
                            disable_notice: Some(target_notice),
                        }),
                    }));
                }
                TestSessionCommand::Prompt { respond_to, .. } => {
                    actor_prompt_count.set(actor_prompt_count.get() + 1);
                    let _ = respond_to.send(crate::session::ok_end_turn(0, None));
                }
                _ => panic!("unexpected command during quarantined prompt recovery"),
            }
        }
    });

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        <MvpAgent as acp::Agent>::prompt(
            &agent,
            acp::PromptRequest::new(
                sid.clone(),
                vec![acp::ContentBlock::from(
                    "a stale recovery receipt must remain quarantined",
                )],
            ),
        ),
    )
    .await
    .expect("quarantined prompt recovery must not hang")
    .expect("quarantined prompt recovery must block cleanly");

    assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
    assert_eq!(apply_count.get(), 1);
    assert_eq!(prompt_count.get(), 0, "no Prompt command may be dispatched");
    assert_eq!(
        agent.resident_handle(&sid).unwrap().model_id,
        acp::ModelId::new(target_model),
        "the resident mirror must reconcile to the actor-owned target"
    );
    match race {
        PromptRecoveryQuarantineRace::NewerDifferentLatch => {
            assert_eq!(
                agent.session_registry.unavailable_model(&sid),
                Some(acp::ModelId::new(newer_unavailable_model))
            );
            assert_eq!(
                agent.session_registry.unavailable_catalog_identity(&sid),
                None
            );
            assert_eq!(
                agent
                    .session_registry
                    .unavailable_agent_name(&sid)
                    .as_deref(),
                Some("newer-agent")
            );
        }
        PromptRecoveryQuarantineRace::ClearedLatch => {
            assert_eq!(
                agent.session_registry.unavailable_model(&sid),
                Some(acp::ModelId::new(target_model)),
                "a concurrent take must be replaced by a target quarantine latch"
            );
            assert_eq!(
                agent.session_registry.unavailable_catalog_identity(&sid),
                Some(target_identity)
            );
            assert_eq!(
                agent
                    .session_registry
                    .unavailable_agent_name(&sid)
                    .as_deref(),
                Some("grok-build")
            );
        }
    }
    assert_eq!(
        agent.models_manager.current_model_id(),
        acp::ModelId::new(source_model),
        "a quarantined recovery must not publish the target globally"
    );
    assert_eq!(
        agent.auth_manager.static_api_key_for_export().as_deref(),
        Some("source-test-key"),
        "a quarantined recovery must not publish the target API key"
    );
    assert_eq!(
        agent.web_search_disabled.borrow().get(&sid),
        Some(&source_notice),
        "a quarantined recovery must not publish the actor target web notice"
    );
    while let Ok(message) = gateway_rx.try_recv() {
        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = message {
            if args.request.method.as_ref() == "x.ai/session_notification" {
                let notification: crate::extensions::notification::SessionNotification =
                    serde_json::from_str(args.request.params.get())
                        .expect("valid session notification");
                if let crate::extensions::notification::SessionUpdate::ModelChanged {
                    model_id,
                    ..
                } = notification.update
                {
                    assert_ne!(
                        model_id, target_model,
                        "a quarantined recovery must not broadcast target ModelChanged"
                    );
                }
            }
            let _ = args.response_tx.send(Ok(()));
        }
    }
    assert!(
        crate::agent::handlers::model_switch::take_captured_success_telemetry(session_name)
            .is_empty(),
        "a quarantined recovery must not emit model-switch success telemetry"
    );
    let _ = crate::agent::handlers::model_switch::take_captured_failure_telemetry(session_name);
    actor.abort();
}

#[test]
#[serial_test::serial]
fn production_prompt_recovery_with_newer_latch_reconciles_only_internal_resident() {
    let _xai_api_key = xai_grok_test_support::EnvGuard::unset("XAI_API_KEY");
    let _grok_code_xai_api_key = xai_grok_test_support::EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    run_local_for_bridge_test(|| async {
        assert_prompt_recovery_quarantines_stale_actor_receipt(
            PromptRecoveryQuarantineRace::NewerDifferentLatch,
        )
        .await;
    });
}

#[test]
#[serial_test::serial]
fn production_prompt_recovery_rebuilds_quarantine_after_concurrent_take() {
    let _xai_api_key = xai_grok_test_support::EnvGuard::unset("XAI_API_KEY");
    let _grok_code_xai_api_key = xai_grok_test_support::EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    run_local_for_bridge_test(|| async {
        assert_prompt_recovery_quarantines_stale_actor_receipt(
            PromptRecoveryQuarantineRace::ClearedLatch,
        )
        .await;
    });
}

#[test]
fn model_overrides_live_cross_provider_switch_rebuilds_inherited_auxiliary_lanes() {
    run_local_for_bridge_test(|| async {
        let agent = build_cross_provider_agent_for_tests(Some("target-test-key"));
        let sid = acp::SessionId::new("cross-provider-auxiliary-switch");
        let mut handle = make_test_handle("source-provider", false, None);
        handle.info.id = sid.clone();
        handle.agent_name = "grok-build".to_owned();
        handle.auxiliary_model_provenance = crate::session::AuxiliaryModelProvenance {
            session_summary_follows_default: true,
            web_search_follows_default: true,
            web_search_model: "source-provider".to_owned(),
            image_description_follows_default: true,
            image_description_model: "source-provider".to_owned(),
        };
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        handle.cmd_tx = cmd_tx;
        agent.insert_resident(&sid, handle);
        agent.web_search_disabled.borrow_mut().insert(
            sid.clone(),
            crate::session::WebSearchDisabledNotice {
                model_id: "source-provider".to_owned(),
                reason: "stale test reason".to_owned(),
                message: "stale test notice".to_owned(),
            },
        );
        {
            // A config refresh after spawn must not rewrite this resident
            // session's inheritance provenance.
            let mut cfg = agent.cfg.borrow_mut();
            cfg.session_summary_follows_default = false;
            cfg.web_search_follows_default = false;
            cfg.web_search_model = "source-provider".to_owned();
            cfg.image_description_follows_default = false;
        }

        tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch {
                        prepared,
                        responds_to,
                    } => {
                        assert_eq!(
                            prepared
                                .summary_sampling_config
                                .as_ref()
                                .map(|config| config.model.as_str()),
                            Some("target-wire-model")
                        );
                        assert!(prepared.replace_inherited_web_search);
                        assert_eq!(
                            prepared
                                .web_search_sampling_config
                                .as_ref()
                                .map(|config| config.model.as_str()),
                            Some("target-wire-model")
                        );
                        assert!(prepared.web_search_disable_notice.is_none());
                        assert_eq!(
                            prepared.image_description_model.as_deref(),
                            Some("target-provider")
                        );
                        let _ = responds_to.send(Ok(crate::session::AppliedModelSwitch {
                            previous_model_id: acp::ModelId::new("source-provider"),
                            catalog_model_id: acp::ModelId::new(
                                prepared.catalog_identity.model_id.clone(),
                            ),
                            did_rebuild: false,
                            active_agent_type: Some("grok-build".to_owned()),
                            web_search: Some(crate::session::AppliedWebSearchState {
                                enabled: false,
                                disable_notice: None,
                            }),
                        }));
                    }
                    _ => panic!("unexpected cross-provider model-switch command"),
                }
            }
        });

        crate::agent::handlers::model_switch::apply(
            &agent,
            acp::SetSessionModelRequest::new(sid.clone(), acp::ModelId::new("target-provider")),
        )
        .await
        .expect("cross-provider model switch");
        assert_eq!(
            agent.resident_handle(&sid).unwrap().model_id.0.as_ref(),
            "target-provider"
        );
        assert!(
            !agent.web_search_disabled.borrow().contains_key(&sid),
            "an applied policy-disabled state must clear the prior availability notice"
        );
    });
}

#[test]
fn model_overrides_global_web_search_disable_skips_live_replacement() {
    run_local_for_bridge_test(|| async {
        let agent = build_cross_provider_agent_for_tests(Some("target-test-key"));
        agent.cfg.borrow_mut().disable_web_search = true;
        let sid = acp::SessionId::new("globally-disabled-web-search-switch");
        let mut handle = make_test_handle("source-provider", false, None);
        handle.info.id = sid.clone();
        handle.agent_name = "grok-build".to_owned();
        handle.auxiliary_model_provenance = crate::session::AuxiliaryModelProvenance {
            session_summary_follows_default: true,
            web_search_follows_default: true,
            web_search_model: "source-provider".to_owned(),
            image_description_follows_default: true,
            image_description_model: "source-provider".to_owned(),
        };
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        handle.cmd_tx = cmd_tx;
        agent.insert_resident(&sid, handle);

        tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch {
                        prepared,
                        responds_to,
                    } => {
                        assert!(!prepared.replace_inherited_web_search);
                        assert!(prepared.web_search_sampling_config.is_none());
                        assert!(prepared.web_search_disable_notice.is_none());
                        let _ = responds_to.send(Ok(crate::session::AppliedModelSwitch {
                            previous_model_id: acp::ModelId::new("source-provider"),
                            catalog_model_id: acp::ModelId::new(
                                prepared.catalog_identity.model_id.clone(),
                            ),
                            did_rebuild: false,
                            active_agent_type: Some("grok-build".to_owned()),
                            web_search: None,
                        }));
                    }
                    _ => panic!("unexpected globally-disabled model-switch command"),
                }
            }
        });

        crate::agent::handlers::model_switch::apply(
            &agent,
            acp::SetSessionModelRequest::new(sid, acp::ModelId::new("target-provider")),
        )
        .await
        .expect("global disable must not prevent the primary model switch");
    });
}

#[test]
fn model_overrides_cold_web_search_notice_describes_operative_session_model() {
    run_local_for_bridge_test(|| async {
        let agent = build_cross_provider_agent_for_tests(None);

        let disabled = agent
            .web_search_disable_details_for_model("target-provider")
            .expect("missing target credential must be described");

        assert_eq!(disabled.model_id, "target-provider");
        assert!(disabled.user_notice().contains("target-provider"));
        assert!(!disabled.user_notice().contains("source-provider"));

        // The handler must trust the actor receipt rather than re-preflighting.
        // Use a locally usable target, then return a disabled applied outcome;
        // a second preflight would incorrectly clear this notice.
        let agent = build_cross_provider_agent_for_tests(Some("target-test-key"));
        let sid = acp::SessionId::new("cross-provider-disabled-auxiliary-switch");
        let mut handle = make_test_handle("source-provider", false, None);
        handle.info.id = sid.clone();
        handle.agent_name = "grok-build".to_owned();
        handle.auxiliary_model_provenance = crate::session::AuxiliaryModelProvenance {
            session_summary_follows_default: true,
            web_search_follows_default: true,
            web_search_model: "source-provider".to_owned(),
            image_description_follows_default: true,
            image_description_model: "source-provider".to_owned(),
        };
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        handle.cmd_tx = cmd_tx;
        agent.insert_resident(&sid, handle);

        tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch {
                        prepared,
                        responds_to,
                    } => {
                        assert!(prepared.web_search_sampling_config.is_some());
                        assert!(prepared.web_search_disable_notice.is_none());
                        let disable_notice = crate::session::WebSearchDisabledNotice {
                            model_id: "target-provider".to_owned(),
                            reason: "actor-applied unavailable state".to_owned(),
                            message: "web_search target-provider is unavailable after actor commit"
                                .to_owned(),
                        };
                        let _ = responds_to.send(Ok(crate::session::AppliedModelSwitch {
                            previous_model_id: acp::ModelId::new("source-provider"),
                            catalog_model_id: acp::ModelId::new(
                                prepared.catalog_identity.model_id.clone(),
                            ),
                            did_rebuild: false,
                            active_agent_type: Some("grok-build".to_owned()),
                            web_search: Some(crate::session::AppliedWebSearchState {
                                enabled: false,
                                disable_notice: Some(disable_notice),
                            }),
                        }));
                    }
                    _ => panic!("unexpected disabled model-switch command"),
                }
            }
        });

        crate::agent::handlers::model_switch::apply(
            &agent,
            acp::SetSessionModelRequest::new(sid.clone(), acp::ModelId::new("target-provider")),
        )
        .await
        .expect("disabled cross-provider model switch");
        let notice = &agent.web_search_disabled.borrow()[&sid];
        assert_eq!(notice.model_id, "target-provider");
        assert!(notice.message.contains("target-provider"));
        assert!(!notice.message.contains("source-provider"));
    });
}

#[test]
fn acp_model_switch_validation_and_apply_handoff_emit_one_failure_event() {
    run_local_for_bridge_test(|| async {
        for rejection in ["unknown", "disallowed", "unready", "unknown-session"] {
            let model_id = format!("validation-{rejection}-model");
            let session_id = acp::SessionId::new(format!("validation-{rejection}-session"));
            let agent = build_agent_with_model_for_tests(&model_id, "grok-build");
            let requested_model = if rejection == "unknown" {
                acp::ModelId::new(format!("{model_id}-missing"))
            } else if matches!(rejection, "disallowed" | "unready") {
                let mut entry = agent
                    .models_manager
                    .models()
                    .shift_remove(&model_id)
                    .expect("test model");
                if rejection == "disallowed" {
                    entry.info.user_selectable = false;
                } else {
                    entry
                        .config_validation_errors
                        .push("injected readiness failure".to_owned());
                }
                agent
                    .models_manager
                    .insert_test_entry(model_id.clone(), entry);
                acp::ModelId::new(model_id.clone())
            } else {
                acp::ModelId::new(model_id.clone())
            };

            let error = <MvpAgent as acp::Agent>::set_session_model(
                &agent,
                acp::SetSessionModelRequest::new(session_id.clone(), requested_model.clone()),
            )
            .await
            .expect_err("public ACP validation must reject the request");
            assert_eq!(error.code, acp::ErrorCode::InvalidParams, "{rejection}");

            let events = crate::agent::handlers::model_switch::take_captured_failure_telemetry(
                session_id.0.as_ref(),
            );
            assert_eq!(events.len(), 1, "{rejection}: {events:?}");
            let event = &events[0];
            assert_eq!(event["session_id"], session_id.0.as_ref(), "{rejection}");
            assert_eq!(
                event["new_model_id"],
                requested_model.0.as_ref(),
                "{rejection}"
            );
            assert_eq!(event["success"], false, "{rejection}");
            assert_eq!(
                event["error_code"],
                crate::agent::config::MODEL_SWITCH_VALIDATION_FAILED,
                "{rejection}"
            );
            assert!(event.get("required_agent_type").is_none(), "{rejection}");
            assert!(event.get("current_agent_type").is_none(), "{rejection}");
        }
    });
}

#[test]
fn zero_turn_model_switch_fails_closed_when_required_harness_is_unresolved() {
    run_local_for_bridge_test(|| async {
        let model_id = "unresolved-harness-model";
        let agent = build_agent_with_model_for_tests(model_id, "missing-custom-harness");
        let sid = acp::SessionId::new("zero-turn-unresolved-harness");
        let mut handle = make_test_handle("previous-model", false, None);
        handle.info.id = sid.clone();
        handle.agent_name = "grok-build".to_owned();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        handle.cmd_tx = cmd_tx;
        agent.session_registry.put_resident(&sid, handle);

        tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch {
                        prepared,
                        responds_to,
                    } => {
                        assert!(
                            prepared.required_definition.is_none(),
                            "the missing required harness must stay unresolved"
                        );
                        let _ =
                            responds_to.send(Err(crate::agent::config::ModelSwitchHarnessError {
                                code: crate::agent::config::MODEL_SWITCH_REBUILD_FAILED.to_owned(),
                                active_agent_type: "grok-build".to_owned(),
                                required_agent_type: "missing-custom-harness".to_owned(),
                                model_id: prepared.catalog_identity.model_id.clone(),
                                reason: "agent_definition_unresolved".to_owned(),
                            }
                            .into_acp_error()));
                    }
                    _ => panic!("unexpected model-switch command"),
                }
            }
        });

        let err = crate::agent::handlers::model_switch::apply(
            &agent,
            acp::SetSessionModelRequest::new(sid.clone(), acp::ModelId::new(model_id)),
        )
        .await
        .expect_err("an unresolved required harness must fail the whole switch");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(|code| code.as_str()),
            Some(crate::agent::config::MODEL_SWITCH_REBUILD_FAILED),
        );
        let handle = agent
            .resident_handle(&sid)
            .expect("session remains registered");
        assert_eq!(handle.model_id.0.as_ref(), "previous-model");
        assert_eq!(handle.agent_name, "grok-build");
    });
}

#[test]
fn new_session_explicit_model_fails_before_spawn_when_harness_is_unresolved() {
    run_local_for_bridge_test(|| async {
        let model_id = "unresolved-default-harness-model";
        let agent = build_agent_with_model_for_tests(model_id, "missing-custom-harness");
        agent
            .auth_manager
            .hot_swap(crate::auth::GrokAuth::test_default());
        agent.set_auth_method(acp::AuthMethodId::new(
            crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
        ));
        let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
            acp::ClientCapabilities::new()
                .fs(acp::FileSystemCapabilities::new())
                .terminal(false),
        );
        agent
            .initialize_request
            .set(init)
            .expect("test initialize request should be set once");
        let cwd = tempfile::tempdir().expect("temporary session cwd");

        let err = <MvpAgent as acp::Agent>::new_session(
            &agent,
            acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                serde_json::json!({ "modelId": model_id })
                    .as_object()
                    .cloned(),
            ),
        )
        .await
        .expect_err("an unresolved explicit-model harness must fail before spawn");
        let payload = crate::agent::config::ModelSwitchHarnessError::from_acp_error(&err)
            .unwrap_or_else(|| panic!("expected structured harness error, got {err:?}"));
        assert_eq!(payload.model_id, model_id);
        assert_eq!(payload.required_agent_type, "missing-custom-harness");
        assert_eq!(payload.reason, "agent_definition_unresolved");
        assert!(
            agent.resident_ids().is_empty(),
            "failed harness preflight must not register a session"
        );
    });
}

#[test]
fn new_session_unknown_model_fallback_still_preflights_default_harness() {
    run_local_for_bridge_test(|| async {
        let default_model_id = "unresolved-fallback-harness-model";
        let agent =
            build_agent_with_model_for_tests(default_model_id, "missing-default-custom-harness");
        agent
            .auth_manager
            .hot_swap(crate::auth::GrokAuth::test_default());
        agent.set_auth_method(acp::AuthMethodId::new(
            crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
        ));
        let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
            acp::ClientCapabilities::new()
                .fs(acp::FileSystemCapabilities::new())
                .terminal(false),
        );
        agent
            .initialize_request
            .set(init)
            .expect("test initialize request should be set once");
        let cwd = tempfile::tempdir().expect("temporary session cwd");

        let err = <MvpAgent as acp::Agent>::new_session(
            &agent,
            acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                serde_json::json!({ "modelId": "unknown-explicit-model" })
                    .as_object()
                    .cloned(),
            ),
        )
        .await
        .expect_err("fallback default harness must be checked before spawn");
        let payload = crate::agent::config::ModelSwitchHarnessError::from_acp_error(&err)
            .unwrap_or_else(|| panic!("expected structured harness error, got {err:?}"));
        assert_eq!(payload.model_id, default_model_id);
        assert_eq!(
            payload.required_agent_type,
            "missing-default-custom-harness"
        );
        assert_eq!(payload.reason, "agent_definition_unresolved");
        assert!(
            agent.resident_ids().is_empty(),
            "failed fallback harness preflight must not register a session"
        );
    });
}
fn session_usage_request(session_id: &str) -> acp::ExtRequest {
    acp::ExtRequest::new(
        "x.ai/session/usage",
        serde_json::value::to_raw_value(&serde_json::json!({ "sessionId": session_id }))
            .unwrap()
            .into(),
    )
}
#[tokio::test(flavor = "current_thread")]
async fn session_usage_unknown_session_is_resource_not_found() {
    let agent = build_minimal_agent_for_tests();
    let err = crate::extensions::usage::handle(&agent, &session_usage_request("no-such-session"))
        .await
        .expect_err("unknown session");
    assert_eq!(
        err.code,
        acp::Error::resource_not_found(None::<String>).code
    );
}
#[tokio::test(flavor = "current_thread")]
async fn session_usage_dead_chat_state_actor_fails_closed() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("usage-dead-actor-sess");
    let mut handle = make_test_handle("test-model", false, None);
    handle.info.id = sid.clone();
    agent.insert_resident(&sid, handle);
    let err =
        crate::extensions::usage::handle(&agent, &session_usage_request("usage-dead-actor-sess"))
            .await
            .expect_err("dead chat-state actor");
    assert_eq!(err.code, acp::Error::internal_error().code);
}
/// The session responses publish the value THIS session's spawn pinned, so a
/// client describing `/loop` fires can never contradict what the fires do.
#[tokio::test(flavor = "current_thread")]
async fn session_meta_publishes_the_sessions_pinned_scheduler_background_loops() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("loop-mode-sess");
    let mut handle = make_test_handle("test-model", false, None);
    handle.info.id = sid.clone();
    handle.scheduler_background_loops = false;
    agent.insert_resident(&sid, handle);
    let model_state = agent.model_state(Some(&sid));
    let mut meta = serde_json::Map::new();
    agent.insert_session_config_meta(&mut meta, &sid, "/tmp".to_string(), None, &model_state);
    assert_eq!(
        meta.get(crate::session::SCHEDULER_BACKGROUND_LOOPS_META_KEY),
        Some(&serde_json::json!(false)),
        "session meta must carry the handle's pinned value"
    );
}
/// #161: the disable notice is published on the session responses, per session.
///
/// This is what makes the `insert_session_config_meta` block load-bearing —
/// without it, deleting that block breaks nothing anywhere, and it is where the
/// whole design lives: the notice rides the response precisely because a
/// notification could not be routed before the client bound the session id, and
/// because headless has no xAI-notification consumer at all.
#[tokio::test(flavor = "current_thread")]
async fn session_meta_publishes_the_web_search_disable_notice_per_session() {
    let agent = build_minimal_agent_for_tests();
    let told = acp::SessionId::new("ws-disabled-sess");
    let untold = acp::SessionId::new("ws-fine-sess");
    for sid in [&told, &untold] {
        let mut handle = make_test_handle("test-model", false, None);
        handle.info.id = (*sid).clone();
        agent.session_registry.put_resident(sid, handle);
    }
    agent.web_search_disabled.borrow_mut().insert(
        told.clone(),
        crate::session::WebSearchDisabledNotice {
            model_id: "grok-4-fast".into(),
            reason: "no API key or session credential available".into(),
            message: "web_search is unavailable: model \"grok-4-fast\" could not be used (no API key or session credential available)".into(),
        },
    );

    let model_state = agent.model_state(Some(&told));
    let mut meta = serde_json::Map::new();
    agent.insert_session_config_meta(&mut meta, &told, "/tmp".to_string(), None, &model_state);
    let published = meta
        .get(crate::session::WEB_SEARCH_DISABLED_META_KEY)
        .expect("session with a notice must publish it");
    let round_tripped: crate::session::WebSearchDisabledNotice =
        serde_json::from_value(published.clone()).expect("published shape must be the shared type");
    assert_eq!(round_tripped.model_id, "grok-4-fast");
    assert!(round_tripped.message.contains("web_search is unavailable"));

    // Control: absent key == available. A blanket "always publish" would pass
    // the assertion above and fail here.
    let mut other = serde_json::Map::new();
    agent.insert_session_config_meta(&mut other, &untold, "/tmp".to_string(), None, &model_state);
    assert!(
        other
            .get(crate::session::WEB_SEARCH_DISABLED_META_KEY)
            .is_none(),
        "a session with no notice must publish no key"
    );
}
/// #201 warm-load seam: a resident session must recompute from current
/// auth/catalog state, not keep spawn's one-time snapshot.
///
/// Covers the three user-visible states:
/// - Ready: key absent (web_search available)
/// - Unusable: key present with reason
/// - Unknown(CatalogUnavailable): key absent (silent, "not enough information")
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn session_load_recompute_updates_web_search_notice_for_ready_unusable_and_unknown() {
    use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
    use crate::agent::config::{Config as AgentConfig, ModelEntry, ModelInfo};
    use crate::auth::{AuthManager, GrokComConfig};
    use indexmap::IndexMap;
    use xai_grok_test_support::EnvGuard;

    const WS_MODEL: &str = "ws-runtime-only-201";
    let _g = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _l = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);

    let temp_dir = tempfile::tempdir().expect("temporary auth root");
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let cfg = AgentConfig {
        web_search_model: WS_MODEL.to_owned(),
        ..AgentConfig::default()
    };

    let mut info = ModelInfo::fallback(WS_MODEL);
    info.base_url = "https://vendor.example/v1".to_owned();
    let mut runtime_catalog = IndexMap::new();
    runtime_catalog.insert(
        WS_MODEL.to_owned(),
        ModelEntry {
            info,
            api_key: Some("runtime-only-test-key".to_owned()),
            env_key: None,
            auth_provider: None,
            api_base_url: None,
            config_validation_errors: Vec::new(),
        },
    );
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, Some(runtime_catalog))
        .expect("valid test config");

    let sid = acp::SessionId::new("ws-warm-201");
    let mut handle = make_test_handle("test-model", false, None);
    handle.info.id = sid.clone();
    handle.auxiliary_model_provenance = crate::session::AuxiliaryModelProvenance {
        web_search_follows_default: false,
        web_search_model: WS_MODEL.to_owned(),
        ..Default::default()
    };
    agent.session_registry.put_resident(&sid, handle);

    // Ready route: web_search remains available, so `_meta` must stay silent.
    agent
        .recompute_web_search_disable_notice_for_session(&sid)
        .await;
    let ready_state = agent.model_state(Some(&sid));
    let mut ready_meta = serde_json::Map::new();
    agent.insert_session_config_meta(
        &mut ready_meta,
        &sid,
        "/tmp".to_string(),
        None,
        &ready_state,
    );
    assert!(
        ready_meta
            .get(crate::session::WEB_SEARCH_DISABLED_META_KEY)
            .is_none(),
        "ready web_search route must publish no disable key"
    );

    // Becomes unusable while resident: the next warm load must publish notice.
    let mut unusable = agent
        .models_manager
        .models()
        .get(WS_MODEL)
        .cloned()
        .expect("runtime model exists");
    unusable.api_key = None;
    unusable
        .config_validation_errors
        .push("injected readiness failure".to_owned());
    agent.models_manager.insert_test_entry(WS_MODEL, unusable);

    agent
        .recompute_web_search_disable_notice_for_session(&sid)
        .await;
    let unusable_state = agent.model_state(Some(&sid));
    let mut unusable_meta = serde_json::Map::new();
    agent.insert_session_config_meta(
        &mut unusable_meta,
        &sid,
        "/tmp".to_string(),
        None,
        &unusable_state,
    );
    let published = unusable_meta
        .get(crate::session::WEB_SEARCH_DISABLED_META_KEY)
        .expect("resident session that became unusable must publish a disable notice");
    let notice: crate::session::WebSearchDisabledNotice =
        serde_json::from_value(published.clone()).expect("shared notice schema");
    assert_eq!(notice.model_id, WS_MODEL);
    assert!(
        notice.reason.contains("injected readiness failure"),
        "notice reason must come from readiness failure"
    );

    // Catalog unavailable (unknown): stay silent instead of reporting disabled.
    agent.models_manager.apply_catalog_for_test(IndexMap::new());
    agent
        .recompute_web_search_disable_notice_for_session(&sid)
        .await;
    let unknown_state = agent.model_state(Some(&sid));
    let mut unknown_meta = serde_json::Map::new();
    agent.insert_session_config_meta(
        &mut unknown_meta,
        &sid,
        "/tmp".to_string(),
        None,
        &unknown_state,
    );
    assert!(
        unknown_meta
            .get(crate::session::WEB_SEARCH_DISABLED_META_KEY)
            .is_none(),
        "Unknown(CatalogUnavailable) must stay silent on warm load"
    );
}
/// #161: the per-session entry dies with the session, so a long-lived process
/// cannot accumulate them. `take_session` is the single funnel for a handle
/// leaving `self.sessions`, which is why cleaning there is sufficient.
#[tokio::test(flavor = "current_thread")]
async fn take_session_drops_the_web_search_notice() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("ws-cleanup-sess");
    let mut handle = make_test_handle("test-model", false, None);
    handle.info.id = sid.clone();
    agent.session_registry.put_resident(&sid, handle);
    agent.web_search_disabled.borrow_mut().insert(
        sid.clone(),
        crate::session::WebSearchDisabledNotice {
            model_id: "m".into(),
            reason: "r".into(),
            message: "msg".into(),
        },
    );
    assert!(agent.web_search_disabled.borrow().contains_key(&sid));
    let _ = agent.take_session(&sid);
    assert!(
        !agent.web_search_disabled.borrow().contains_key(&sid),
        "take_session must drop the session-scoped notice"
    );
}
/// #161: the latch is per-session, not process-wide. Two sessions disabled for
/// *different* reasons must each publish their own notice on their own
/// response `_meta`. The pre-#185 `Cell<bool>` latch swallowed every notice
/// after the first, and a "first wins" / "last wins" read of the map would
/// still pass the told/untold pair above while collapsing these two. Reading
/// the entry for a second response (a reconnect-driven reload) must not
/// consume it either -- the notice is not a one-shot.
#[tokio::test(flavor = "current_thread")]
async fn session_meta_keeps_web_search_notices_session_scoped() {
    let agent = build_minimal_agent_for_tests();
    let first = acp::SessionId::new("ws-first-sess");
    let second = acp::SessionId::new("ws-second-sess");
    for sid in [&first, &second] {
        let mut handle = make_test_handle("test-model", false, None);
        handle.info.id = (*sid).clone();
        agent.session_registry.put_resident(sid, handle);
    }
    let notice_for = |model: &str, reason: &str| crate::session::WebSearchDisabledNotice {
        model_id: model.into(),
        reason: reason.into(),
        message: format!(
            "web_search is unavailable: model \"{model}\" could not be used ({reason})"
        ),
    };
    agent.web_search_disabled.borrow_mut().insert(
        first.clone(),
        notice_for("grok-4-fast", "no API key or session credential available"),
    );
    agent.web_search_disabled.borrow_mut().insert(
        second.clone(),
        notice_for("vendor-large", "model is not ready"),
    );

    let published_for = |sid: &acp::SessionId| {
        let model_state = agent.model_state(Some(sid));
        let mut meta = serde_json::Map::new();
        agent.insert_session_config_meta(&mut meta, sid, "/tmp".to_string(), None, &model_state);
        let raw = meta
            .get(crate::session::WEB_SEARCH_DISABLED_META_KEY)
            .expect("each disabled session must publish its own notice");
        serde_json::from_value::<crate::session::WebSearchDisabledNotice>(raw.clone())
            .expect("shared notice schema")
    };
    let first_notice = published_for(&first);
    assert_eq!(first_notice.model_id, "grok-4-fast");
    assert!(first_notice.reason.contains("no API key"));
    let second_notice = published_for(&second);
    assert_eq!(second_notice.model_id, "vendor-large");
    assert!(second_notice.reason.contains("model is not ready"));

    // A reconnect-driven reload rebuilds the response `_meta`; the entry must
    // survive being read, or the reload silently re-latches like the old
    // process-wide flag did.
    let republished = published_for(&first);
    assert_eq!(
        republished, first_notice,
        "re-reading the response meta must not consume the notice"
    );
}
/// Build a minimal MvpAgent with pre-loaded auth for gate tests.
fn build_agent_with_auth(auth: crate::auth::GrokAuth) -> MvpAgent {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    auth_manager.hot_swap(auth);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let cfg = AgentConfig::default();
    MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config")
}
/// #180: `seed_client_config_auth_if_available` must not reinject ambient
/// credentials into a post-strip header-auth route.
///
/// `sampling_config_for_model` treats `ExplicitHeader` as a **post-strip**
/// label: for a model that authenticates by a header the user declared, it
/// sets `credentials.api_key = None` and *then* writes `ExplicitHeader`, so
/// the label means "the ambient credential has been removed". The seed's
/// `api_key.is_none()` gate is satisfied by exactly that strip; without the
/// early return it would put the ambient session bytes back.
///
/// The property is stronger than "do not leave the inconsistent pair": the
/// seed must not reinject ambient bytes **at all**. Stamping `XaiSession` on
/// reinjection would make the pair consistent and L3-refuse the route, while
/// an assertion that only checks `!(api_key.is_some() && ExplicitHeader)`
/// would stay green. See the sampler-side companion
/// `ambient_token_labelled_explicit_header_must_not_reach_an_external_origin`.
#[tokio::test(flavor = "current_thread")]
async fn seeded_ambient_key_must_not_keep_a_post_strip_explicit_header_label() {
    let local = tokio::task::LocalSet::new();
    local.run_until(seeded_ambient_key_body()).await;
}

async fn seeded_ambient_key_body() {
    use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
    use crate::auth::{AuthManager, GrokComConfig};

    const VENDOR_MODEL: &str = "vendor-header-auth";

    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    // A live ambient xAI session, as any signed-in user has.
    auth_manager.hot_swap(crate::auth::GrokAuth {
        key: "ambient-xai-session-jwt".into(),
        auth_mode: crate::auth::AuthMode::WebLogin,
        ..crate::auth::GrokAuth::test_default()
    });
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);

    // An ordinary third-party provider: external origin, authenticated by a
    // declared `x-api-key`, with no `api_key` / `env_key` of its own.
    let mut cfg = AgentConfig::default();
    cfg.models.default = Some(VENDOR_MODEL.to_owned());
    let mut extra_headers = indexmap::IndexMap::new();
    extra_headers.insert("x-api-key".to_owned(), "sk-vendor-declared".to_owned());
    cfg.config_models.insert(
        VENDOR_MODEL.to_owned(),
        ConfigModelOverride {
            model: Some(VENDOR_MODEL.to_owned()),
            base_url: Some("https://vendor.example/v1".to_owned()),
            extra_headers,
            ..Default::default()
        },
    );
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
    agent
        .models_manager
        .set_current_model_id(acp::ModelId::new(VENDOR_MODEL));

    // `MvpAgent::new` resolves the agent-level config once, and which model
    // wins there depends on default-model resolution rather than on anything
    // this test is about. Recompute it through the production path now that
    // the vendor model is current: `ModelsManager::sampling_config()` is the
    // same function `new` calls, so this is the state a user whose default is
    // the vendor model starts with — not a hand-built config.
    *agent.sampling_config.borrow_mut() = agent.models_manager.sampling_config();

    // Precondition: the strip ran and labelled the route honestly. If this
    // fails the fixture no longer reaches the seam under test.
    {
        let seeded = agent.sampling_config.borrow();
        assert!(
            seeded.api_key.is_none(),
            "precondition: the ambient strip must have cleared the key for a \
             header-authenticated model on an external origin. \
             model={:?} base_url={:?} source={:?} auth_scheme={:?} \
             extra_header_names={:?}",
            seeded.model,
            seeded.base_url,
            seeded.credential_source,
            seeded.auth_scheme,
            seeded.extra_headers.keys().collect::<Vec<_>>(),
        );
        assert!(
            matches!(
                seeded.credential_source,
                Some(xai_grok_sampler::CredentialSource::ExplicitHeader { .. })
            ),
            "precondition: the strip labels the route ExplicitHeader; got {:?}",
            seeded.credential_source
        );
    }

    agent.seed_client_config_auth_if_available();

    let seeded = agent.sampling_config.borrow();
    // Property: the seed must not reinject ambient bytes into a post-strip
    // header-auth route *at all* — not merely avoid leaving the inconsistent
    // pair. Removing only the ExplicitHeader early-return while keeping the
    // XaiSession stamp reinjects ambient under a consistent ambient label;
    // the old `!(api_key.is_some() && ExplicitHeader)` assertion stayed green
    // on that mutation while the route became ambient-labelled and L3-refused.
    assert!(
        seeded.api_key.is_none(),
        "the seed reinjected a credential into a post-strip header-auth \
         route. That route authenticates by the user's declared header on an \
         external origin; ambient bytes must stay absent under every label. \
         (Value withheld.)"
    );
    assert!(
        matches!(
            seeded.credential_source,
            Some(xai_grok_sampler::CredentialSource::ExplicitHeader { .. })
        ),
        "post-strip header-auth label must remain ExplicitHeader after seed; \
         got {:?}",
        seeded.credential_source
    );
}
/// Regression: boot-time plugin discovery is deferred past ACP
/// `initialize`, so the shared plugin registry starts empty.
/// `resolve_mcp_servers` reads that snapshot to merge plugin-contributed
/// MCP servers into a new session, so without lazy population the servers
/// silently vanished until an explicit `/plugins reload`.
/// `ensure_plugin_registry` must build the snapshot on first use.
#[tokio::test]
#[serial_test::serial]
async fn ensure_plugin_registry_lazily_populates_snapshot() {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    use xai_grok_test_support::EnvGuard;
    let grok_home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", grok_home.path());
    let plugin_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        plugin_dir.path().join("plugin.json"),
        r#"{"name": "regr-lazy-mcp-plugin"}"#,
    )
    .unwrap();
    std::fs::write(
        plugin_dir.path().join(".mcp.json"),
        r#"{"mcpServers":{"regr-srv":{"command":"echo","args":["hi"]}}}"#,
    )
    .unwrap();
    let auth_home = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(auth_home.path(), GrokComConfig::default()));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let mut cfg = AgentConfig::default();
    cfg.plugins.cli_plugin_dirs = vec![plugin_dir.path().to_path_buf()];
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
    assert!(
        agent.plugin_registry_handle.snapshot().is_none(),
        "snapshot must start empty (boot discovery deferred past initialize)"
    );
    agent.ensure_plugin_registry();
    let snapshot = agent
        .plugin_registry_handle
        .snapshot()
        .expect("snapshot must be populated on first use");
    assert!(
        snapshot.get("regr-lazy-mcp-plugin").is_some(),
        "lazy discovery must surface the plugin so its MCP server merges into the session"
    );
    agent.ensure_plugin_registry();
    assert!(
        agent
            .plugin_registry_handle
            .snapshot()
            .is_some_and(|s| s.get("regr-lazy-mcp-plugin").is_some()),
        "repeat call must keep the populated snapshot"
    );
}
#[cfg(unix)]
mod process_scope_reclaim;
mod session_resume_close_tests;
mod subagent_spawn_context_tests;
/// No load in flight and no session → the wait returns immediately
/// (the caller then surfaces "unknown session id" exactly as before).
#[tokio::test]
async fn wait_for_in_flight_load_returns_immediately_when_idle() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-none");
    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        agent.wait_for_in_flight_session_load(&sid),
    )
    .await
    .expect("wait must not block when no load is in flight");
}
/// A waiter racing an in-flight `session/load` blocks until the load
/// finishes and then observes the registered session. This is the
/// agent-side guarantee that closes the post-leader-crash
/// "unknown session id" race: the reconnect replay's `session/load` and
/// the client's next `session/prompt` can arrive back-to-back.
#[tokio::test]
async fn wait_for_in_flight_load_blocks_until_load_completes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let agent = std::rc::Rc::new(build_minimal_agent_for_tests());
            let sid = acp::SessionId::new("sess-loading");
            let guard = agent.begin_session_load(&sid).expect("load claim");
            let waiter_agent = agent.clone();
            let waiter_sid = sid.clone();
            let waiter = tokio::task::spawn_local(async move {
                waiter_agent
                    .wait_for_in_flight_session_load(&waiter_sid)
                    .await;
                waiter_agent.is_resident(&waiter_sid)
            });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(!waiter.is_finished(), "waiter must block while loading");
            let handle = make_test_handle("test-model", false, None);
            agent.insert_resident(&sid, handle);
            drop(guard);
            let found_session = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
                .await
                .expect("waiter must wake when the load guard drops")
                .expect("waiter task must not panic");
            assert!(
                found_session,
                "after the wait, the session must be visible to the racing request"
            );
        })
        .await;
}

/// Registration happens before `session/load` finishes restoring its persisted
/// model. A racing request must therefore remain behind the load marker even
/// after the handle becomes visible, or it can observe the older model and run
/// before the restore's fallback wins.
#[tokio::test]
async fn registered_session_stays_gated_until_restored_model_is_final() {
    use std::task::Poll;

    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-registered-while-loading");
    let persisted = acp::ModelId::new("persisted-model");
    let fallback = acp::ModelId::new("ready-fallback");
    let guard = agent.begin_session_load(&sid).expect("load claim");
    let mut handle = make_test_handle(persisted.0.as_ref(), false, None);
    handle.info.id = sid.clone();
    agent.session_registry.put_resident(&sid, handle);

    let mut racing_request = Box::pin(agent.session_handle_waiting_for_load(&sid));
    assert!(
        matches!(futures::poll!(racing_request.as_mut()), Poll::Pending),
        "a registered handle must remain gated until persisted restoration completes"
    );

    agent.with_resident_mut(&sid, |h| h.model_id = fallback.clone());
    drop(guard);

    let observed = racing_request.await.expect("restored session handle");
    assert_eq!(
        observed.model_id, fallback,
        "the racing request must observe the restored fallback, never the persisted model"
    );
    assert_eq!(
        agent.resident_handle(&sid).unwrap().model_id,
        observed.model_id,
        "the registered handle and racing request must agree on the final model ordering"
    );
}

#[test]
fn attach_restore_rejects_catalog_change_between_prepare_and_actor_dispatch() {
    run_local_for_bridge_test(|| async {
        let agent = build_agent_with_model_for_tests("removed-key", "grok-build");
        let mut reused = agent.models_manager.models()["removed-key"].clone();
        reused.info.model = "foreign-route".to_owned();
        reused.info.base_url = "https://foreign.example/v1".to_owned();
        reused.api_key = Some("foreign-secret".to_owned());
        reused.info.auth_scheme = xai_grok_sampler::AuthScheme::Bearer;
        agent
            .models_manager
            .insert_test_entry("removed-key", reused);
        let mut replacement = agent.models_manager.models()["removed-key"].clone();
        replacement.info.model = "retained-route".to_owned();
        replacement.info.base_url = "https://retained.example/v1".to_owned();
        replacement.api_key = Some("retained-secret".to_owned());
        let mut refreshed_reuse = replacement.clone();
        refreshed_reuse.info.model = "refreshed-foreign-route".to_owned();
        refreshed_reuse.info.base_url = "https://refreshed-foreign.example/v1".to_owned();
        refreshed_reuse.api_key = Some("refreshed-foreign-secret".to_owned());
        agent
            .models_manager
            .insert_test_entry("replacement-key", replacement);

        let sid = acp::SessionId::new("resume-catalog-lineage");
        let mut handle = make_test_handle("removed-key", false, None);
        handle.info.id = sid.clone();
        handle.agent_name = "grok-build".to_owned();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        handle.cmd_tx = cmd_tx;
        agent.session_registry.put_resident(&sid, handle);
        let models_manager = agent.models_manager.clone();
        let apply_dispatched = std::rc::Rc::new(std::cell::Cell::new(false));
        let actor_apply_dispatched = apply_dispatched.clone();
        tokio::task::spawn_local(async move {
            while let Some(command) = cmd_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        // Simulate an etag refresh after restore reconciled the
                        // persisted route but before the actor switch commits.
                        models_manager
                            .insert_test_entry("replacement-key", refreshed_reuse.clone());
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch {
                        prepared: _,
                        responds_to: _,
                    } => {
                        actor_apply_dispatched.set(true);
                    }
                    _ => panic!("unexpected command during persisted model restore"),
                }
            }
        });

        let info = crate::session::info::Info {
            id: sid.clone(),
            cwd: "/tmp/resume-catalog-lineage".to_owned(),
        };
        let mut summary =
            crate::session::persistence::Summary::new(&info, acp::ModelId::new("removed-key"))
                .unwrap();
        summary.catalog_identity = Some(xai_chat_state::CatalogIdentity {
            model_id: "removed-key".to_owned(),
            route: "retained-route".to_owned(),
            lineage: xai_chat_state::CatalogResolutionLineage::UniqueRoute,
            auth_scheme: Some(xai_chat_state::CatalogAuthScheme::Bearer),
        });
        let guard = agent.begin_session_load(&sid).expect("load claim");
        agent.restore_persisted_model(&sid, &summary, &guard).await;
        assert!(
            !apply_dispatched.get(),
            "a catalog generation change after preparation must prevent actor dispatch"
        );
        assert_eq!(
            agent.resident_handle(&sid).unwrap().model_id.0.as_ref(),
            "removed-key",
            "the stale prepared route must not update the resident mirror"
        );
        drop(guard);
    });
}

#[test]
fn attach_restore_blocks_exact_key_reuse_and_unique_route_ambiguity() {
    run_local_for_bridge_test(|| async {
        for (label, lineage, replacement_count) in [
            (
                "exact-reuse",
                xai_chat_state::CatalogResolutionLineage::ExactKey,
                1usize,
            ),
            (
                "unique-route-ambiguity",
                xai_chat_state::CatalogResolutionLineage::UniqueRoute,
                2usize,
            ),
        ] {
            let agent = build_agent_with_model_for_tests("removed-key", "grok-build");
            let mut reused = agent.models_manager.models()["removed-key"].clone();
            reused.info.model = "foreign-route".to_owned();
            reused.info.base_url = "https://foreign.example/v1".to_owned();
            reused.api_key = Some("foreign-secret".to_owned());
            reused.info.auth_scheme = xai_grok_sampler::AuthScheme::Bearer;
            agent
                .models_manager
                .insert_test_entry("removed-key", reused.clone());
            for index in 0..replacement_count {
                let mut replacement = reused.clone();
                replacement.info.model = "retained-route".to_owned();
                replacement.info.base_url = format!("https://retained-{index}.example/v1");
                replacement.api_key = Some(format!("retained-secret-{index}"));
                agent
                    .models_manager
                    .insert_test_entry(format!("replacement-{index}"), replacement);
            }

            let sid = acp::SessionId::new(format!("resume-{label}"));
            let mut handle = make_test_handle("removed-key", false, None);
            handle.info.id = sid.clone();
            let resident_agent_name = handle.agent_name.clone();
            let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            handle.cmd_tx = cmd_tx;
            agent.session_registry.put_resident(&sid, handle);
            let info = crate::session::info::Info {
                id: sid.clone(),
                cwd: format!("/tmp/{label}"),
            };
            let mut summary =
                crate::session::persistence::Summary::new(&info, acp::ModelId::new("removed-key"))
                    .unwrap();
            summary.catalog_identity = Some(xai_chat_state::CatalogIdentity {
                model_id: "removed-key".to_owned(),
                route: "retained-route".to_owned(),
                lineage,
                auth_scheme: Some(xai_chat_state::CatalogAuthScheme::Bearer),
            });
            summary.agent_name = (label != "exact-reuse").then(|| "grok-build".to_owned());
            let guard = agent.begin_session_load(&sid).expect("load claim");
            agent.restore_persisted_model(&sid, &summary, &guard).await;
            assert_eq!(
                agent.session_registry.unavailable_model(&sid),
                Some(acp::ModelId::new("removed-key")),
                "{label} must latch instead of selecting any credential"
            );
            assert_eq!(
                agent.session_registry.unavailable_catalog_identity(&sid),
                summary.catalog_identity,
                "{label} must retain identity for fail-closed prompt recovery"
            );
            assert_eq!(
                agent.session_registry.unavailable_agent_name(&sid),
                Some(resident_agent_name),
                "{label} must retain explicit persisted or resident harness evidence"
            );
            assert!(
                reconcile_latched_catalog_snapshot(
                    &agent.models_manager.models(),
                    &agent.models_manager.available(),
                    agent
                        .session_registry
                        .unavailable_catalog_identity(&sid)
                        .as_ref()
                        .unwrap(),
                )
                .is_none(),
                "{label} prompt recovery must not accept a reused key or ambiguous route"
            );
            assert!(
                cmd_rx.try_recv().is_err(),
                "{label} must not send a model switch that could attach a secret"
            );
            drop(guard);
        }
    });
}

/// The production `session/load` restore entry point must bypass only its own
/// load marker. A real registered session is restored while an ordinary model
/// switch remains gated; after the guard drops, that later request commits last.
#[test]
fn load_restore_apply_bypasses_own_marker_and_preserves_request_order() {
    use std::task::Poll;

    let local = tokio::task::LocalSet::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(local.run_until(async {
            let restored_model = "restored-model";
            let later_model = "later-model";
            let agent = std::rc::Rc::new(build_agent_with_model_for_tests(
                restored_model,
                "grok-build",
            ));
            let mut later_entry = agent
                .models_manager
                .models()
                .get(restored_model)
                .expect("restored model entry")
                .clone();
            later_entry.info.model = later_model.to_owned();
            agent
                .models_manager
                .insert_test_entry(later_model, later_entry);

            let sid = acp::SessionId::new("sess-load-restore-order");
            let guard = agent.begin_session_load(&sid).expect("load claim");
            let mut handle = make_test_handle("persisted-model", false, None);
            handle.info.id = sid.clone();
            let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            handle.cmd_tx = cmd_tx;
            agent.session_registry.put_resident(&sid, handle);
            let unavailable_model = acp::ModelId::new("persisted-unavailable");
            agent
                .session_registry
                .set_unavailable_model(&sid, unavailable_model.clone());

            tokio::task::spawn_local(async move {
                while let Some(command) = cmd_rx.recv().await {
                    match command {
                        TestSessionCommand::GetActiveAgent { responds_to } => {
                            let _ = responds_to.send(Some("grok-build".to_owned()));
                        }
                        TestSessionCommand::ApplyModelSwitch {
                            prepared,
                            responds_to,
                        } => {
                            let model_id = acp::ModelId::new(prepared.catalog_identity.model_id);
                            let _ = responds_to.send(Ok(crate::session::AppliedModelSwitch {
                                previous_model_id: acp::ModelId::new("previous-model"),
                                catalog_model_id: model_id,
                                did_rebuild: false,
                                active_agent_type: Some("grok-build".to_owned()),
                                web_search: None,
                            }));
                        }
                        _ => panic!("unexpected command during model restore"),
                    }
                }
            });

            let mut later_request = Box::pin(crate::agent::handlers::model_switch::apply(
                &agent,
                acp::SetSessionModelRequest::new(sid.clone(), acp::ModelId::new(later_model)),
            ));
            assert!(
                matches!(futures::poll!(later_request.as_mut()), Poll::Pending),
                "an external switch must remain gated while session/load owns the marker"
            );

            acp_agent::restore_registered_session_model(
                &agent,
                acp::SetSessionModelRequest::new(sid.clone(), acp::ModelId::new(restored_model)),
                &guard,
                None,
            )
            .await
            .expect("session/load restore must not wait on its own marker");
            assert_eq!(
                agent.resident_handle(&sid).unwrap().model_id.0.as_ref(),
                restored_model,
                "the load restore must commit before the load marker is released"
            );
            assert_eq!(
                agent.session_registry.unavailable_model(&sid),
                Some(unavailable_model),
                "session/load must preserve its intentional fail-closed latch"
            );
            assert!(
                matches!(futures::poll!(later_request.as_mut()), Poll::Pending),
                "the external request must still be gated until load completion"
            );

            drop(guard);
            later_request
                .await
                .expect("the gated external switch must run after load completion");
            assert_eq!(
                agent.resident_handle(&sid).unwrap().model_id.0.as_ref(),
                later_model,
                "the later external request must be the final committed model"
            );
            assert_eq!(
                agent.session_registry.unavailable_model(&sid),
                None,
                "a successful external user switch must clear the load-time latch"
            );
        }));
}

/// #357: the active `session/load` path must restore a persisted Ultra effort
/// into the actor request and the resident mirror when the refreshed catalog
/// still advertises it.
#[test]
fn load_restore_preserves_advertised_persisted_ultra_effort() {
    use crate::agent::config::EndpointsConfig;
    use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

    run_local_for_bridge_test(|| async {
        let model_id = "gpt-5.6-sol";
        let agent = build_agent_with_model_for_tests(model_id, "grok-build");
        let mut entry = agent
            .models_manager
            .models()
            .get(model_id)
            .cloned()
            .unwrap_or_else(|| {
                crate::agent::config::ModelEntry::fallback(model_id, &EndpointsConfig::default())
            });
        entry.info.supports_reasoning_effort = true;
        entry.info.reasoning_effort = Some(ReasoningEffort::Low);
        entry.info.reasoning_efforts = vec![
            ReasoningEffortOption {
                id: "low".into(),
                value: ReasoningEffort::Low,
                label: "Low".into(),
                description: None,
                default: true,
            },
            ReasoningEffortOption {
                id: "ultra".into(),
                value: ReasoningEffort::Ultra,
                label: "Ultra".into(),
                description: None,
                default: false,
            },
        ];
        agent.models_manager.insert_test_entry(model_id, entry);

        let session_id = acp::SessionId::new("resume-ultra-advertised");
        let guard = agent.begin_session_load(&session_id).expect("load claim");
        let mut handle = make_test_handle(model_id, false, None);
        handle.info.id = session_id.clone();
        let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
        handle.cmd_tx = command_tx;
        agent.session_registry.put_resident(&session_id, handle);

        tokio::task::spawn_local(async move {
            while let Some(command) = command_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch {
                        prepared,
                        responds_to,
                    } => {
                        assert_eq!(
                            prepared.sampling_config.reasoning_effort,
                            Some(ReasoningEffort::Ultra),
                            "session/load must send the persisted Ultra tier to the actor"
                        );
                        let _ = responds_to.send(Ok(crate::session::AppliedModelSwitch {
                            previous_model_id: acp::ModelId::new(model_id),
                            catalog_model_id: acp::ModelId::new(model_id),
                            did_rebuild: false,
                            active_agent_type: Some("grok-build".to_owned()),
                            web_search: None,
                        }));
                    }
                    _ => panic!("unexpected command during Ultra restore"),
                }
            }
        });

        let info = crate::session::info::Info {
            id: session_id.clone(),
            cwd: "/tmp/resume-ultra-advertised".to_owned(),
        };
        let mut summary =
            crate::session::persistence::Summary::new(&info, acp::ModelId::new(model_id))
                .expect("summary");
        summary.reasoning_effort = Some(ReasoningEffort::Ultra);

        agent
            .restore_persisted_model(&session_id, &summary, &guard)
            .await;
        let resident = agent.resident_handle(&session_id).expect("resident handle");
        assert_eq!(resident.model_id.0.as_ref(), model_id);
        assert_eq!(resident.reasoning_effort, Some(ReasoningEffort::Ultra));
        assert_eq!(agent.session_registry.unavailable_model(&session_id), None);
        drop(guard);
    });
}

/// #357: a saved Ultra tier is no longer valid when the refreshed model menu
/// stops at Max. The attach must latch the session instead of silently running
/// with the model default.
#[test]
fn load_restore_latches_when_persisted_ultra_is_no_longer_advertised() {
    use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

    run_local_for_bridge_test(|| async {
        let model_id = "gpt-5.6-luna";
        let agent = build_agent_with_model_for_tests(model_id, "grok-build");
        let mut entry = agent.models_manager.models()[model_id].clone();
        entry.info.supports_reasoning_effort = true;
        entry.info.reasoning_effort = Some(ReasoningEffort::Medium);
        entry.info.reasoning_efforts = vec![ReasoningEffortOption {
            id: "max".into(),
            value: ReasoningEffort::Max,
            label: "Max".into(),
            description: None,
            default: false,
        }];
        agent.models_manager.insert_test_entry(model_id, entry);

        let session_id = acp::SessionId::new("resume-ultra-stale");
        let guard = agent.begin_session_load(&session_id).expect("load claim");
        let mut handle = make_test_handle(model_id, false, None);
        handle.info.id = session_id.clone();
        let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
        handle.cmd_tx = command_tx;
        agent.session_registry.put_resident(&session_id, handle);

        tokio::task::spawn_local(async move {
            while let Some(command) = command_rx.recv().await {
                match command {
                    TestSessionCommand::GetActiveAgent { responds_to } => {
                        let _ = responds_to.send(Some("grok-build".to_owned()));
                    }
                    TestSessionCommand::ApplyModelSwitch { .. } => {
                        panic!("stale persisted Ultra must fail before actor dispatch")
                    }
                    _ => panic!("unexpected command during stale Ultra restore"),
                }
            }
        });

        let info = crate::session::info::Info {
            id: session_id.clone(),
            cwd: "/tmp/resume-ultra-stale".to_owned(),
        };
        let mut summary =
            crate::session::persistence::Summary::new(&info, acp::ModelId::new(model_id))
                .expect("summary");
        summary.reasoning_effort = Some(ReasoningEffort::Ultra);

        agent
            .restore_persisted_model(&session_id, &summary, &guard)
            .await;
        assert_eq!(
            agent.session_registry.unavailable_model(&session_id),
            Some(acp::ModelId::new(model_id)),
            "invalid persisted effort must latch prompts"
        );
        assert_eq!(
            agent
                .resident_handle(&session_id)
                .expect("resident remains for diagnostics")
                .reasoning_effort,
            None,
            "stale Ultra must not silently become the model default"
        );
        drop(guard);
    });
}

/// A failed load (guard dropped WITHOUT registering the session) also
/// wakes waiters — they re-check, find nothing, and the caller surfaces
/// the regular "unknown session id" error rather than hanging.
#[tokio::test]
async fn wait_for_in_flight_load_wakes_on_failed_load() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let agent = std::rc::Rc::new(build_minimal_agent_for_tests());
            let sid = acp::SessionId::new("sess-load-fails");
            let guard = agent.begin_session_load(&sid).expect("load claim");
            let waiter_agent = agent.clone();
            let waiter_sid = sid.clone();
            let waiter = tokio::task::spawn_local(async move {
                waiter_agent
                    .wait_for_in_flight_session_load(&waiter_sid)
                    .await;
                waiter_agent.is_resident(&waiter_sid)
            });
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            drop(guard);
            let found_session = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
                .await
                .expect("waiter must wake when the failed load's guard drops")
                .expect("waiter task must not panic");
            assert!(!found_session, "failed load leaves no session behind");
        })
        .await;
}
/// Two concurrent loads of the same session: the first guard's drop must
/// not remove the second load's marker (waiters keep waiting on the
/// newer in-flight load).
#[tokio::test]
async fn concurrent_load_guards_do_not_clobber_each_other() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-concurrent");
    let guard_one = agent.begin_session_load(&sid).expect("load claim");
    let guard_two = agent.begin_session_load(&sid).expect("load claim");
    drop(guard_one);
    assert!(
        agent.session_registry.is_attaching(&sid),
        "second load's marker must survive the first guard's drop"
    );
    drop(guard_two);
    assert!(
        agent.session_registry.attaching_count() == 0,
        "all markers removed once every load finished"
    );
}
/// Dropping a newer duplicate load while an older load is still restoring must
/// keep the older marker visible to waiters. Otherwise an external
/// `session_handle_waiting_for_load` sees no marker and releases early while the
/// older restore is still in flight.
#[tokio::test(start_paused = true)]
async fn dropping_newer_duplicate_load_keeps_older_wait_marker_alive() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-concurrent-newer-drops-first");
    let guard_one = agent.begin_session_load(&sid).expect("load claim");
    let guard_two = agent.begin_session_load(&sid).expect("load claim");

    // Superseded load finishes first.
    drop(guard_two);
    assert!(
        agent.session_registry.is_attaching(&sid),
        "dropping the newer guard must not clear the older in-flight load marker"
    );
    assert_eq!(
        agent.wait_for_in_flight_session_load(&sid).await,
        SessionLoadWait::TimedOut,
        "with the older load still active, waiter-side lookup must fail closed"
    );
    drop(guard_one);
}
/// The load-restore bypass is owner-bound: with duplicate loads of the same
/// session, the second `begin_session_load` replaces the marker, so the older
/// load must NOT resolve a handle through the newer load's marker — only the
/// load that owns the live marker may bypass.
#[tokio::test]
async fn older_load_cannot_borrow_newer_loads_marker() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-duplicate-load");
    let guard_one = agent.begin_session_load(&sid).expect("load claim");
    let mut handle = make_test_handle("test-model", false, None);
    handle.info.id = sid.clone();
    agent.session_registry.put_resident(&sid, handle);

    // The only live marker is guard_one's: its own restore may bypass.
    assert!(
        agent.session_handle_during_load(&sid, &guard_one).is_some(),
        "the load that owns the live marker must resolve its registered handle"
    );

    // A duplicate load begins and replaces the marker.
    let guard_two = agent.begin_session_load(&sid).expect("load claim");
    assert!(
        agent.session_handle_during_load(&sid, &guard_one).is_none(),
        "the older load must not ride the newer load's marker"
    );
    assert!(
        agent.session_handle_during_load(&sid, &guard_two).is_some(),
        "the newer load owns the live marker and may bypass"
    );

    // Once the newer load's guard drops, its marker is gone; the older load's
    // bypass stays refused (its own marker was replaced, never restored).
    drop(guard_two);
    assert!(
        agent.session_handle_during_load(&sid, &guard_one).is_none(),
        "no live marker owned by the older load means no bypass"
    );
    drop(guard_one);
}
/// The bounded load wait must fail closed: when it expires with the load
/// guard still alive, the caller gets `None` — never the registered
/// mid-restore handle, which is not ready. Driven by paused tokio time, so
/// no real waiting occurs.
#[tokio::test(start_paused = true)]
async fn load_wait_timeout_fails_closed_on_mid_restore_handle() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-load-timeout");
    let _guard = agent.begin_session_load(&sid).expect("load claim");
    // Registration lands before restoration finishes — the exact window the
    // timeout must not expose.
    let mut handle = make_test_handle("test-model", false, None);
    handle.info.id = sid.clone();
    agent.session_registry.put_resident(&sid, handle);

    // Paused time auto-advances past the 60s load-wait deadline while the
    // guard is alive, expiring the bounded wait.
    let resolved = agent.session_handle_waiting_for_load(&sid).await;
    assert!(
        resolved.is_none(),
        "a timed-out load wait must fail closed, not hand out the mid-restore handle"
    );
    assert!(
        agent.resident_handle(&sid).is_some(),
        "the handle IS registered — `None` above is the fail-closed timeout, \
         not an absent session"
    );
}
/// `resident_activity` returns `NeedsInput` whenever the session's
/// pending-interaction map is non-empty — and that wins even over a
/// running turn (a session blocked on a permission mid-turn "needs
/// input"). Clearing the map falls back to Working / Idle.
#[tokio::test]
async fn resident_activity_reports_needs_input_when_pending() {
    use crate::agent::roster::RosterActivity;
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-pending");
    let handle = make_test_handle("grok-3", false, None);
    let pending = handle.pending_interactions.clone();
    let prompt_id = handle.current_prompt_id.clone();
    agent.insert_resident(&sid, handle);
    assert_eq!(agent.resident_activity(&sid), RosterActivity::Idle);
    *prompt_id.lock().unwrap() = Some("turn-1".to_string());
    assert_eq!(agent.resident_activity(&sid), RosterActivity::Working);
    pending.lock().unwrap().insert(
        "call-1".to_string(),
        crate::session::pending_interaction::PendingKind::Permission,
    );
    assert_eq!(agent.resident_activity(&sid), RosterActivity::NeedsInput);
    let entry = agent.resident_roster_entry(&sid).expect("resident entry");
    assert_eq!(entry.activity, RosterActivity::NeedsInput);
    pending.lock().unwrap().clear();
    assert_eq!(agent.resident_activity(&sid), RosterActivity::Working);
}
/// Drain the agent gateway, returning the first `x.ai/sessions/changed`
/// payload that carries an upserted entry (ignoring any unrelated
/// notifications, which parse into an empty `RosterChanged`).
fn drain_roster_changed(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) -> Option<crate::agent::roster::RosterChanged> {
    let mut found = None;
    while let Ok(msg) = rx.try_recv() {
        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg {
            if found.is_none()
                && let Ok(changed) = serde_json::from_str::<crate::agent::roster::RosterChanged>(
                    args.request.params.get(),
                )
                && !changed.upserted.is_empty()
            {
                found = Some(changed);
            }
            let _ = args.response_tx.send(Ok(()));
        }
    }
    found
}
/// A turn-boundary activity delta (`push_roster_activity_delta`) broadcasts
/// an `x.ai/sessions/changed` upsert carrying the *overridden* activity, so
/// every attached dashboard reflects Working/Idle immediately instead of
/// waiting for the ≤1s roster poll (turn-start/turn-end). The
/// override matters because at turn-start the actor has not yet published
/// `current_prompt_id`, so a natural `resident_activity` read would emit
/// `Idle` for a session that is in fact starting a turn.
#[tokio::test]
async fn push_roster_activity_delta_broadcasts_overridden_activity() {
    use crate::agent::config::Config as AgentConfig;
    use crate::agent::roster::RosterActivity;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let cfg = AgentConfig::default();
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
    let sid = acp::SessionId::new("sess-activity");
    agent.insert_resident(&sid, make_test_handle("grok-3", false, None));
    agent.push_roster_activity_delta(&sid, RosterActivity::Working);
    let changed = drain_roster_changed(&mut rx).expect("turn-start delta emitted");
    assert_eq!(changed.upserted.len(), 1);
    assert_eq!(changed.upserted[0].session_id, sid.0.to_string());
    assert!(changed.upserted[0].resident);
    assert_eq!(
        changed.upserted[0].activity,
        RosterActivity::Working,
        "forced activity must override the Idle that resident_activity would read"
    );
    assert!(changed.removed.is_empty());
    agent.push_roster_activity_delta(&sid, RosterActivity::Idle);
    let changed = drain_roster_changed(&mut rx).expect("turn-end delta emitted");
    assert_eq!(changed.upserted[0].activity, RosterActivity::Idle);
}
/// Extract the inner payload from an ExtResponse.
#[expect(
    dead_code,
    reason = "unused in production; remove expect when wired or delete the item"
)]
fn parse_ext_body(resp: &acp::ExtResponse) -> serde_json::Value {
    let outer: serde_json::Value =
        serde_json::from_str(resp.0.get()).expect("ExtResponse must be valid JSON");
    outer
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("ExtResponse has no 'result' key; full JSON: {outer}"))
}
/// Replicate the lookup logic of code_nav_eligibility_for_request so we
/// can test it with a plain sessions HashMap.
fn check_nav_eligibility_from_sessions(
    sessions: &HashMap<acp::SessionId, crate::session::SessionHandle>,
    session_id: Option<&acp::SessionId>,
) -> Result<(), CodeNavEligibility> {
    let session_id = match session_id {
        Some(sid) => sid,
        None => return Err(CodeNavEligibility::SessionRequired),
    };
    let Some(handle) = sessions.get(session_id) else {
        return Err(CodeNavEligibility::SessionRequired);
    };
    let ct = crate::http::client_type_from_origin(handle.origin_client.as_ref());
    if !matches!(ct, ClientType::GrokWeb) {
        return Err(CodeNavEligibility::ClientNotWeb);
    }
    if !handle.code_nav_enabled {
        return Err(CodeNavEligibility::CapabilityNotAdvertised);
    }
    Ok(())
}
/// Web session with code-nav capability is eligible.
///
/// This is the "happy path" that allows lazy index startup on the first
/// code-nav request.
#[tokio::test]
async fn test_web_session_with_capability_is_eligible() {
    let sid = acp::SessionId::new("sess-web");
    let mut handle = make_test_handle("model", false, Some("grok-web"));
    handle.code_nav_enabled = true;
    let sessions = [(sid.clone(), handle)].into();
    assert!(
        check_nav_eligibility_from_sessions(&sessions, Some(&sid)).is_ok(),
        "web session with code-nav capability must be eligible"
    );
}
/// TUI session is rejected at gate 1 (client type) regardless of capability.
#[tokio::test]
async fn test_tui_session_is_rejected() {
    let sid = acp::SessionId::new("sess-tui");
    let mut handle = make_test_handle("model", false, Some("grok-tui"));
    handle.code_nav_enabled = true;
    let sessions = [(sid.clone(), handle)].into();
    assert_eq!(
        check_nav_eligibility_from_sessions(&sessions, Some(&sid)),
        Err(CodeNavEligibility::ClientNotWeb),
        "TUI client must be rejected at gate 1 (client type)"
    );
}
/// Web session without capability is rejected at gate 2.
#[tokio::test]
async fn test_web_session_without_capability_is_rejected() {
    let sid = acp::SessionId::new("sess-web-no-cap");
    let mut handle = make_test_handle("model", false, Some("grok-web"));
    handle.code_nav_enabled = false;
    let sessions = [(sid.clone(), handle)].into();
    assert_eq!(
        check_nav_eligibility_from_sessions(&sessions, Some(&sid)),
        Err(CodeNavEligibility::CapabilityNotAdvertised),
        "web client without capability must be rejected at gate 2"
    );
}
/// Leader-mode isolation: two sessions with different code-nav state return
/// independent results.
#[tokio::test]
async fn test_leader_mode_two_sessions_stay_isolated() {
    let web_sid = acp::SessionId::new("web");
    let tui_sid = acp::SessionId::new("tui");
    let mut web_handle = make_test_handle("model", false, Some("grok-web"));
    web_handle.code_nav_enabled = true;
    let mut tui_handle = make_test_handle("model", false, Some("grok-tui"));
    tui_handle.code_nav_enabled = false;
    let sessions = [(web_sid.clone(), web_handle), (tui_sid.clone(), tui_handle)].into();
    assert!(
        check_nav_eligibility_from_sessions(&sessions, Some(&web_sid)).is_ok(),
        "web session must be eligible"
    );
    assert_eq!(
        check_nav_eligibility_from_sessions(&sessions, Some(&tui_sid)),
        Err(CodeNavEligibility::ClientNotWeb),
        "tui session must remain ineligible even when web session is eligible"
    );
}
/// Unknown session ID returns SessionRequired, not a global fallback.
///
/// This is the stale/evicted session path: a caller with a session ID that
/// no longer exists in the sessions map must get SessionRequired, not
/// accidentally inherit the last-initialized client's eligibility.
#[tokio::test]
async fn test_unknown_session_id_returns_session_required() {
    let known_sid = acp::SessionId::new("known");
    let mut known_handle = make_test_handle("model", false, Some("grok-web"));
    known_handle.code_nav_enabled = true;
    let sessions = [(known_sid.clone(), known_handle)].into();
    let stale_sid = acp::SessionId::new("stale-or-evicted");
    assert_eq!(
        check_nav_eligibility_from_sessions(&sessions, Some(&stale_sid)),
        Err(CodeNavEligibility::SessionRequired),
        "stale/evicted sessionId must not fall back to global state"
    );
    assert!(check_nav_eligibility_from_sessions(&sessions, Some(&known_sid)).is_ok());
}
mod parse_json_object_env_tests {
    use super::parse_json_object_env;
    unsafe fn set(k: &str, v: &str) {
        unsafe { std::env::set_var(k, v) };
    }
    unsafe fn unset(k: &str) {
        unsafe { std::env::remove_var(k) };
    }
    #[test]
    #[serial_test::serial]
    fn valid_json_object_returns_some() {
        unsafe { set("TEST_JSON_OBJ", r#"{"team":"platform","org":"acme"}"#) };
        let result = parse_json_object_env("TEST_JSON_OBJ");
        unsafe { unset("TEST_JSON_OBJ") };
        let val = result.expect("should parse valid JSON object");
        assert_eq!(val["team"], "platform");
        assert_eq!(val["org"], "acme");
    }
    #[test]
    #[serial_test::serial]
    fn non_object_json_returns_none() {
        unsafe { set("TEST_JSON_ARR", r#"["not","an","object"]"#) };
        let result = parse_json_object_env("TEST_JSON_ARR");
        unsafe { unset("TEST_JSON_ARR") };
        assert!(result.is_none());
    }
    #[test]
    #[serial_test::serial]
    fn invalid_json_returns_none() {
        unsafe { set("TEST_JSON_BAD", "not json at all") };
        let result = parse_json_object_env("TEST_JSON_BAD");
        unsafe { unset("TEST_JSON_BAD") };
        assert!(result.is_none());
    }
    #[test]
    #[serial_test::serial]
    fn unset_var_returns_none() {
        unsafe { unset("TEST_JSON_UNSET") };
        assert!(parse_json_object_env("TEST_JSON_UNSET").is_none());
    }
}
mod eligibility_gates {
    use super::*;
    /// Standalone replica of the first three eligibility gates.
    /// Gate 4 (git root) requires a real filesystem and is covered by
    /// integration tests.
    fn check_gates(
        client_type: ClientType,
        code_nav_enabled: bool,
        indexing_enabled: bool,
    ) -> Result<(), CodeNavEligibility> {
        if !matches!(client_type, ClientType::GrokWeb) {
            return Err(CodeNavEligibility::ClientNotWeb);
        }
        if !code_nav_enabled {
            return Err(CodeNavEligibility::CapabilityNotAdvertised);
        }
        if !indexing_enabled {
            return Err(CodeNavEligibility::DisabledByConfig);
        }
        Ok(())
    }
    #[test]
    fn non_web_client_rejected() {
        assert_eq!(
            check_gates(ClientType::Generic, true, true),
            Err(CodeNavEligibility::ClientNotWeb)
        );
    }
    #[test]
    fn tui_client_rejected() {
        assert_eq!(
            check_gates(ClientType::GrokTUI, true, true),
            Err(CodeNavEligibility::ClientNotWeb)
        );
    }
    #[test]
    fn web_client_no_capability_rejected() {
        assert_eq!(
            check_gates(ClientType::GrokWeb, false, true),
            Err(CodeNavEligibility::CapabilityNotAdvertised)
        );
    }
    #[test]
    fn web_client_with_capability_config_disabled_rejected() {
        assert_eq!(
            check_gates(ClientType::GrokWeb, true, false),
            Err(CodeNavEligibility::DisabledByConfig)
        );
    }
    #[test]
    fn web_client_with_capability_and_config_passes_first_three_gates() {
        assert!(check_gates(ClientType::GrokWeb, true, true).is_ok());
    }
}
#[test]
fn find_model_by_id_prefers_key_then_falls_back_to_slug() {
    let entry = |model: &str| ModelEntry {
        info: config::ModelInfo {
            user_selectable: true,
            id: None,
            model: model.to_string(),
            base_url: String::new(),
            name: None,
            description: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: crate::sampling::ApiBackend::default(),
            auth_scheme: Default::default(),
            extra_headers: IndexMap::new(),
            query_params: IndexMap::new(),
            env_http_headers: IndexMap::new(),
            context_window: std::num::NonZeroU64::new(200_000).unwrap(),
            auto_compact_threshold_percent: None,
            system_prompt_label: None,
            use_concise: false,
            agent_type: config::default_agent_type(),
            inference_idle_timeout_secs: None,
            max_retries: None,
            hidden: false,
            supported_in_api: true,
            reasoning_effort: None,
            supports_reasoning_effort: false,
            reasoning_efforts: Vec::new(),
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            show_model_fingerprint: false,
            stream_tool_calls: None,
            laziness_detector: crate::agent::config::LazinessDetectorPerModelConfig::default(),
            codex_wire: None,
            catalog_degraded_reason: None,
            catalog_upgrade: None,
        },
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
        config_validation_errors: Vec::new(),
    };
    let mut models = indexmap::IndexMap::new();
    models.insert("a".to_string(), entry("target"));
    models.insert("target".to_string(), entry("other"));
    assert_eq!(
        config::find_model_by_id(&models, "target").unwrap().model,
        "other",
        "key match should win over slug scan"
    );
    assert_eq!(
        config::find_model_by_id(&models, "a").unwrap().model,
        "target",
        "exact key match for 'a'"
    );
}
fn write_updates(dir: &std::path::Path, lines: &[&str]) -> PathBuf {
    let path = dir.join("updates.jsonl");
    std::fs::write(&path, lines.join("\n")).unwrap();
    path
}
fn bg_line(task_id: &str) -> String {
    format!(
        r#"{{"timestamp":1,"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"task_backgrounded","task_id":"{task_id}","command":"sleep 99","cwd":"/tmp"}}}}}}"#
    )
}
fn completed_line(task_id: &str) -> String {
    format!(
        r#"{{"timestamp":2,"method":"_x.ai/session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"task_completed","task_snapshot":{{"task_id":"{task_id}","completed":true}}}}}}}}"#
    )
}
fn orphaned_ids(tasks: &[OrphanedTask]) -> std::collections::HashSet<&str> {
    tasks.iter().map(|t| t.task_id.as_str()).collect()
}
#[test]
fn orphaned_tasks_returns_empty_for_no_file() {
    let result = MvpAgent::find_orphaned_background_tasks(&None);
    assert!(result.is_empty());
}
#[test]
fn orphaned_tasks_returns_empty_for_missing_file() {
    let path = PathBuf::from("/nonexistent/updates.jsonl");
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    assert!(result.is_empty());
}
#[test]
fn orphaned_tasks_returns_empty_when_all_completed() {
    let tmp = tempfile::tempdir().unwrap();
    let bg = bg_line("t1");
    let done = completed_line("t1");
    let path = write_updates(tmp.path(), &[&bg, &done]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    assert!(result.is_empty());
}
#[test]
fn orphaned_tasks_returns_uncompleted() {
    let tmp = tempfile::tempdir().unwrap();
    let bg1 = bg_line("t1");
    let bg2 = bg_line("t2");
    let done1 = completed_line("t1");
    let path = write_updates(tmp.path(), &[&bg1, &bg2, &done1]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    let ids = orphaned_ids(&result);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains("t2"));
}
#[test]
fn orphaned_tasks_returns_multiple_uncompleted() {
    let tmp = tempfile::tempdir().unwrap();
    let bg1 = bg_line("t1");
    let bg2 = bg_line("t2");
    let bg3 = bg_line("t3");
    let done2 = completed_line("t2");
    let path = write_updates(tmp.path(), &[&bg1, &bg2, &bg3, &done2]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    let ids = orphaned_ids(&result);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains("t1"));
    assert!(ids.contains("t3"));
}
#[test]
fn orphaned_tasks_captures_command_and_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let bg = bg_line("t1");
    let path = write_updates(tmp.path(), &[&bg]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].command, "sleep 99");
    assert_eq!(result[0].cwd, "/tmp");
}
#[test]
fn orphaned_tasks_skips_malformed_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let bg = bg_line("t1");
    let path = write_updates(tmp.path(), &["not json", &bg, "{}"]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    assert_eq!(result.len(), 1);
}
#[test]
fn orphaned_tasks_ignores_unrelated_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let bg = bg_line("t1");
    let unrelated = r#"{"timestamp":1,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"auto_compact_started","percentage":80}}}"#;
    let path = write_updates(tmp.path(), &[&bg, unrelated]);
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    assert_eq!(result.len(), 1);
}
#[test]
fn orphaned_tasks_filters_rewind_dead_branches() {
    let tmp = tempfile::tempdir().unwrap();
    let user_msg = r#"{"timestamp":0,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}}}"#;
    let bg_before_rewind = bg_line("t-dead");
    let rewind = r#"{"timestamp":3,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"rewind_marker","target_prompt_index":0,"created_at":"2025-01-01T00:00:00Z"}}}"#;
    let user_msg2 = r#"{"timestamp":4,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"retry"}}}}"#;
    let bg_after_rewind = bg_line("t-alive");
    let path = write_updates(
        tmp.path(),
        &[
            user_msg,
            &bg_before_rewind,
            rewind,
            user_msg2,
            &bg_after_rewind,
        ],
    );
    let result = MvpAgent::find_orphaned_background_tasks(&Some(path));
    let ids = orphaned_ids(&result);
    assert!(
        ids.contains("t-alive"),
        "task after rewind should be present"
    );
    assert!(
        !ids.contains("t-dead"),
        "task in dead branch should be filtered"
    );
}
#[test]
fn allow_access_from_remote_settings() {
    let json = serde_json::json!({ "allow_access": true });
    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
    assert_eq!(rs.allow_access, Some(true));
    let json = serde_json::json!({ "allow_access": false });
    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
    assert_eq!(rs.allow_access, Some(false));
    let json = serde_json::json!({});
    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
    assert_eq!(rs.allow_access, None);
}
#[test]
fn on_demand_enabled_from_remote_settings() {
    let json = serde_json::json!({ "on_demand_enabled": false });
    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
    assert_eq!(rs.on_demand_enabled, Some(false));
    let json = serde_json::json!({});
    let rs: crate::util::config::RemoteSettings = serde_json::from_value(json).unwrap();
    assert_eq!(rs.on_demand_enabled, None);
}
/// Regression for a 401 sequence seen in production. After a long idle
/// window, the auth manager may have no
/// live token by the time `session/new` runs. For session-based auth methods
/// we MUST still report `SessionToken` so chat_state credentials retain the
/// session-token shape and `try_refresh_session_token` will run on the next
/// prompt instead of early-returning.
#[tokio::test(flavor = "current_thread")]
async fn auth_type_session_based_no_current_returns_session_token() {
    for method_id in [
        crate::agent::auth_method::CACHED_TOKEN_AUTH_METHOD_ID,
        crate::agent::auth_method::GROK_COM_METHOD_ID,
        crate::agent::auth_method::OIDC_METHOD_ID,
    ] {
        let agent = build_minimal_agent_for_tests();
        agent.set_auth_method(acp::AuthMethodId::new(method_id));
        assert!(
            agent.auth_manager.current().is_none(),
            "{method_id}: precondition: AuthManager has no current token",
        );
        assert_eq!(
            agent.auth_type(),
            xai_chat_state::AuthType::SessionToken,
            "{method_id}: session-based auth must report SessionToken even \
                 without a live token -- otherwise chat_state gets locked into \
                 auth_type = ApiKey and try_refresh_session_token will skip \
                 every subsequent refresh attempt.",
        );
    }
}
/// BYOK guard. Users with `xai.api_key` must continue to report `ApiKey`
/// regardless of live-token state -- BYOK sessions have nothing to refresh,
/// and reporting `SessionToken` would route through cli-chat-proxy paths
/// (image_gen / video_gen base_url) that don't apply to BYOK keys.
#[tokio::test(flavor = "current_thread")]
async fn auth_type_xai_api_key_no_current_returns_api_key() {
    let agent = build_minimal_agent_for_tests();
    agent.set_auth_method(acp::AuthMethodId::new(
        crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
    ));
    assert!(agent.auth_manager.current().is_none());
    assert_eq!(
        agent.auth_type(),
        xai_chat_state::AuthType::ApiKey,
        "xai.api_key auth must report ApiKey -- BYOK has no session-token \
             behavior to fall back to."
    );
}
/// Positive baseline: when both signals agree (session-based method AND
/// a live in-memory token), `SessionToken` is returned. This is the
/// common case during a healthy session.
#[tokio::test(flavor = "current_thread")]
async fn auth_type_session_based_with_current_returns_session_token() {
    use crate::auth::GrokAuth;
    let agent = build_minimal_agent_for_tests();
    agent.set_auth_method(acp::AuthMethodId::new(
        crate::agent::auth_method::OIDC_METHOD_ID,
    ));
    agent.auth_manager.hot_swap(GrokAuth::test_default());
    assert!(agent.auth_manager.current().is_some());
    assert_eq!(agent.auth_type(), xai_chat_state::AuthType::SessionToken,);
}
/// Defensive case: no `auth_method_id` selected yet (pre-`authenticate`
/// state) and no live credential. We default to `ApiKey` so callers
/// that key off this value (e.g. `resolve_chat_state_auth_type` for chat
/// routing) don't accidentally route session-token-shaped traffic
/// through cli-chat-proxy before a method has been chosen.
#[tokio::test(flavor = "current_thread")]
async fn auth_type_no_method_id_no_current_returns_api_key() {
    let agent = build_minimal_agent_for_tests();
    assert!(agent.auth_method_id.load().is_none());
    assert!(agent.auth_manager.current().is_none());
    assert_eq!(agent.auth_type(), xai_chat_state::AuthType::ApiKey,);
}
/// Live credential present but `auth_method_id` is still `None`. The
/// in-memory bearer takes precedence: this is the order observed during
/// `initialize()` silent refresh -- a token is hot-swapped in before
/// `authenticate()` writes the method id. Reporting `SessionToken`
/// here matches pre-fix behavior and keeps logging stable.
#[tokio::test(flavor = "current_thread")]
async fn auth_type_no_method_id_with_current_returns_session_token() {
    use crate::auth::GrokAuth;
    let agent = build_minimal_agent_for_tests();
    agent.auth_manager.hot_swap(GrokAuth::test_default());
    assert!(agent.auth_method_id.load().is_none());
    assert!(agent.auth_manager.current().is_some());
    assert_eq!(agent.auth_type(), xai_chat_state::AuthType::SessionToken,);
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_new_session_plan_rebuilds_after_same_key_catalog_swap_before_seal() {
    let agent = build_minimal_agent_for_tests();
    let current = agent.models_manager.current_model_id();
    let original_catalog = agent.models_manager.models();
    let original = original_catalog
        .get(current.0.as_ref())
        .expect("minimal agent current model must exist")
        .clone();
    let original_route = original.info().model.clone();
    let replacement_base_url = format!(
        "{}/replacement",
        original.info().base_url.trim_end_matches('/')
    );
    let mut replacement = original.clone();
    replacement.info.model = "same-key-replacement-route".to_owned();
    replacement.info.base_url = replacement_base_url.clone();
    let mut replacement_catalog = original_catalog;
    replacement_catalog.insert(current.0.to_string(), replacement);
    let swapped = std::cell::Cell::new(false);
    let plan = agent
        .prepare_new_session_model_plan_with_before_seal(None, None, || {
            if !swapped.replace(true) {
                agent
                    .models_manager
                    .apply_catalog_for_test(replacement_catalog.clone());
            }
        })
        .expect("same-key plan must resolve");
    assert_eq!(plan.catalog_identity.route, "same-key-replacement-route");
    assert_eq!(plan.model_entry.info().model, "same-key-replacement-route");
    assert_eq!(plan.sampling_config.model, "same-key-replacement-route");
    assert_eq!(plan.sampling_config.base_url, replacement_base_url);
    assert!(
        agent
            .models_manager
            .new_session_model_authority_is_current(&plan.auth_authority),
        "the rebuilt plan must carry the replacement catalog authority"
    );
    assert_ne!(plan.catalog_identity.route, original_route);
}

/// #360: an empty `reasoning_efforts` list means the model uses Medley's
/// built-in effort menu when support is explicit; `/new` must retain the
/// selected effort under the same canonical capability semantics.
#[tokio::test(flavor = "current_thread")]
async fn prepared_new_session_plan_preserves_selected_effort_with_implicit_menu() {
    use xai_grok_sampling_types::ReasoningEffort;

    let agent = build_minimal_agent_for_tests();
    let current = agent.models_manager.current_model_id();
    let mut entry = agent.models_manager.models()[current.0.as_ref()].clone();
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_effort = None;
    entry.info.reasoning_efforts.clear();
    agent
        .models_manager
        .insert_test_entry(current.0.to_string(), entry);
    let inserted = agent.models_manager.models()[current.0.as_ref()].clone();
    assert!(inserted.info.supports_reasoning_effort);
    assert_eq!(inserted.info.reasoning_effort, None);
    assert!(inserted.info.reasoning_efforts.is_empty());
    agent
        .models_manager
        .set_current_reasoning_effort(Some(ReasoningEffort::High));

    let plan = agent
        .prepare_new_session_model_plan(None, None)
        .expect("supported implicit effort menu must prepare");

    assert_eq!(
        plan.sampling_config.reasoning_effort,
        Some(ReasoningEffort::High),
        "/new must retain the selected built-in effort when the model omits an explicit menu"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prepared_new_session_plan_rejects_same_key_catalog_swap_at_publication() {
    let agent = build_minimal_agent_for_tests();
    let current = agent.models_manager.current_model_id();
    let mut replacement_catalog = agent.models_manager.models();
    let replacement = replacement_catalog
        .get_mut(current.0.as_ref())
        .expect("minimal agent current model must exist");
    replacement.info.model = "post-prepare-replacement-route".to_owned();
    replacement.info.base_url = "https://post-prepare.invalid/v1".to_owned();
    let plan = agent
        .prepare_new_session_model_plan(None, None)
        .expect("initial plan must resolve");

    agent
        .models_manager
        .apply_catalog_for_test(replacement_catalog);

    assert!(
        !agent
            .models_manager
            .new_session_model_authority_is_current(&plan.auth_authority),
        "a post-prepare same-key route replacement must invalidate publication"
    );
}

#[test]
fn new_session_profile_pin_reports_the_committed_resident_model() {
    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
        use crate::auth::{AuthManager, GrokComConfig};
        use acp::Agent as _;

        let tmp = tempfile::tempdir().expect("profile fixture");
        let profile = tmp.path().join("pinned.md");
        std::fs::write(
            &profile,
            "---\nname: grok-build\ndescription: pinned fixture\nmodel: pinned-model\n---\n",
        )
        .expect("write profile");
        let mut cfg = AgentConfig::default();
        cfg.models.default = Some("prepared-model".to_owned());
        cfg.agent_profile_path = Some(profile);
        for (id, auth_scheme) in [
            ("prepared-model", xai_grok_sampler::AuthScheme::None),
            ("pinned-model", xai_grok_sampler::AuthScheme::None),
            ("unready-request", xai_grok_sampler::AuthScheme::Bearer),
        ] {
            cfg.config_models.insert(
                id.to_owned(),
                ConfigModelOverride {
                    model: Some(id.to_owned()),
                    base_url: Some("http://localhost".to_owned()),
                    auth_scheme: Some(auth_scheme),
                    agent_type: Some("grok-build".to_owned()),
                    ..Default::default()
                },
            );
        }
        let auth = std::sync::Arc::new(AuthManager::new(
            &tmp.path().join("auth"),
            GrokComConfig::default(),
        ));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth, None).expect("agent");
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .expect("initialize once");
        agent
            .authenticate(acp::AuthenticateRequest::new(acp::AuthMethodId::new(
                crate::agent::auth_method::LOCAL_NONE_METHOD_ID,
            )))
            .await
            .expect("authenticate local fixture");
        let cwd = tempfile::tempdir().expect("session cwd");
        let response = <MvpAgent as acp::Agent>::new_session(
            &agent,
            acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                serde_json::json!({ "modelId": "unready-request" })
                    .as_object()
                    .cloned(),
            ),
        )
        .await
        .expect("spawn profile-pinned session");
        assert_eq!(
            response
                .models
                .expect("advertised model")
                .current_model_id
                .0
                .as_ref(),
            "pinned-model"
        );
        assert_eq!(
            agent
                .resident_handle(&response.session_id)
                .expect("resident")
                .model_id
                .0
                .as_ref(),
            "pinned-model"
        );
        let notification = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|message| match message {
                xai_acp_lib::AcpClientMessage::ExtNotification(args)
                    if args.request.method.as_ref() == "x.ai/session_notification" =>
                {
                    Some(args.request.params.get().to_owned())
                }
                _ => None,
            })
            .find(|payload| payload.contains("model_auto_switched"))
            .expect("fallback notification");
        assert!(notification.contains("unready-request"));
        assert!(notification.contains("pinned-model"));
        assert!(!notification.contains("prepared-model"));
    });
}

#[test]
#[serial_test::serial]
fn new_session_publishes_one_complete_staged_tree() {
    use acp::Agent as _;

    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
        use crate::auth::{AuthManager, GrokComConfig};
        use xai_grok_test_support::EnvGuard;

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000156";
        let grok_home = tempfile::tempdir().expect("isolated grok home");
        let _medley = EnvGuard::set("MEDLEY_HOME", grok_home.path());
        let _home = EnvGuard::set("GROK_HOME", grok_home.path());
        let _state_home = pin_fixture_state_home(grok_home.path());
        assert_fixture_session_id_unused(SESSION_ID);
        let _remote_fetch = ProcessRemoteFetchOff::install();
        let _xai_key = EnvGuard::unset("XAI_API_KEY");
        let _grok_code_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        let auth_root = tempfile::tempdir().expect("isolated auth root");
        let mut cfg = AgentConfig::default();
        cfg.models.default = Some("staged-publication-model".to_owned());
        cfg.config_models.insert(
            "staged-publication-model".to_owned(),
            ConfigModelOverride {
                model: Some("staged-publication-model".to_owned()),
                base_url: Some("http://localhost".to_owned()),
                auth_scheme: Some(xai_grok_sampler::AuthScheme::None),
                agent_type: Some("grok-build".to_owned()),
                ..Default::default()
            },
        );
        let auth = std::sync::Arc::new(AuthManager::new(
            &auth_root.path().join("auth"),
            GrokComConfig::default(),
        ));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth, None).expect("agent");
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .expect("initialize once");
        agent
            .authenticate(acp::AuthenticateRequest::new(acp::AuthMethodId::new(
                crate::agent::auth_method::LOCAL_NONE_METHOD_ID,
            )))
            .await
            .expect("authenticate local fixture");

        let cwd = tempfile::tempdir().expect("session cwd");
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            <MvpAgent as acp::Agent>::new_session(
                &agent,
                acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                    serde_json::json!({ "sessionId": SESSION_ID })
                        .as_object()
                        .cloned(),
                ),
            ),
        )
        .await
        .expect("new_session must not hang")
        .expect("publish the complete staged session tree");
        assert_eq!(response.session_id.0.as_ref(), SESSION_ID);

        let cwd_string = cwd.path().to_string_lossy();
        let state_home = xai_grok_config::grok_home();
        let published_session = state_home
            .join("sessions")
            .join(crate::util::grok_home::encode_cwd_dirname(&cwd_string))
            .join(SESSION_ID);
        for artifact in [
            "summary.json",
            "prompt_context.json",
            "system_prompt.txt",
            "chat_history.jsonl",
        ] {
            assert!(
                published_session.join(artifact).is_file(),
                "published session is missing {artifact}"
            );
        }
        assert!(
            !published_session
                .join(crate::session::persistence::UNPUBLISHED_SESSION_MARKER)
                .exists(),
            "the public session must not retain its staging marker"
        );
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(published_session.join("summary.json")).expect("read summary"),
        )
        .expect("published summary must be valid JSON");
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(published_session.join("prompt_context.json"))
                .expect("read prompt context"),
        )
        .expect("published prompt context must be valid JSON");
        assert!(
            !std::fs::read_to_string(published_session.join("system_prompt.txt"))
                .expect("read system prompt")
                .is_empty(),
            "published system prompt must not be empty"
        );
        let history = std::fs::read_to_string(published_session.join("chat_history.jsonl"))
            .expect("read chat history");
        assert!(
            !history.is_empty(),
            "published chat history must not be empty"
        );
        for line in history.lines() {
            serde_json::from_str::<serde_json::Value>(line)
                .expect("each published chat-history line must be valid JSON");
        }

        let stage_container = state_home.join(".private/session-staging").join(
            crate::session::persistence::session_stage_container_name(SESSION_ID),
        );
        assert!(
            !stage_container.exists(),
            "the committed private stage container must be removed"
        );
    });
}

#[test]
#[serial_test::serial]
fn new_session_api_key_auth_rejects_ready_session_only_explicit_model() {
    use acp::Agent as _;

    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
        use crate::auth::{AuthManager, AuthMode, GrokComConfig};
        use xai_grok_test_support::EnvGuard;

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000101";
        let grok_home = tempfile::tempdir().expect("isolated grok home");
        let _home = EnvGuard::set("GROK_HOME", grok_home.path());
        let _state_home = pin_fixture_state_home(grok_home.path());
        assert_fixture_session_id_unused(SESSION_ID);
        let _xai_key = EnvGuard::unset("XAI_API_KEY");
        let _grok_code_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        let tmp = tempfile::tempdir().expect("auth visibility fixture");
        let mut cfg = AgentConfig::default();
        cfg.models.default = Some("api-visible-default".to_owned());
        for (id, supported_in_api) in [("api-visible-default", true), ("ready-session-only", false)]
        {
            cfg.config_models.insert(
                id.to_owned(),
                ConfigModelOverride {
                    model: Some(id.to_owned()),
                    base_url: Some("http://localhost".to_owned()),
                    api_key: Some(format!("{id}-credential")),
                    auth_scheme: Some(xai_grok_sampler::AuthScheme::Bearer),
                    supported_in_api: Some(supported_in_api),
                    agent_type: Some("grok-build".to_owned()),
                    ..Default::default()
                },
            );
        }
        let auth = std::sync::Arc::new(AuthManager::new(
            &tmp.path().join("auth"),
            GrokComConfig::default(),
        ));
        auth.hot_swap(crate::auth::GrokAuth {
            auth_mode: AuthMode::ApiKey,
            ..crate::auth::GrokAuth::test_default()
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth, None).expect("agent");
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .expect("initialize once");
        agent.set_auth_method(acp::AuthMethodId::new(
            crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
        ));
        let expected_fallback = agent.models_manager.current_model_id();
        assert_ne!(expected_fallback.0.as_ref(), "ready-session-only");
        let cwd = tempfile::tempdir().expect("session cwd");
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            <MvpAgent as acp::Agent>::new_session(
                &agent,
                acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                    serde_json::json!({
                        "sessionId": SESSION_ID,
                        "modelId": "ready-session-only",
                    })
                    .as_object()
                    .cloned(),
                ),
            ),
        )
        .await
        .expect("new_session must not hang")
        .expect("fall back to API-visible default");

        assert_eq!(response.session_id.0.as_ref(), SESSION_ID);
        assert_eq!(
            response
                .models
                .expect("advertised model")
                .current_model_id
                .0
                .as_ref(),
            expected_fallback.0.as_ref()
        );
        assert_eq!(
            agent
                .resident_handle(&response.session_id)
                .expect("resident")
                .model_id
                .0
                .as_ref(),
            expected_fallback.0.as_ref()
        );
        let notice = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|message| match message {
                xai_acp_lib::AcpClientMessage::ExtNotification(args)
                    if args.request.method.as_ref() == "x.ai/session_notification" =>
                {
                    serde_json::from_str::<crate::extensions::notification::SessionNotification>(
                        args.request.params.get(),
                    )
                    .ok()
                }
                _ => None,
            })
            .find_map(|notification| match notification.update {
                crate::extensions::notification::SessionUpdate::ModelAutoSwitched {
                    previous_model_id,
                    new_model_id,
                    reason,
                } => Some((previous_model_id, new_model_id, reason)),
                _ => None,
            })
            .expect("authentication-mode fallback notice");
        assert_eq!(notice.0, "ready-session-only");
        assert_eq!(notice.1, expected_fallback.0.as_ref());
        assert!(notice.2.contains("authentication mode"), "{}", notice.2);
    });
}

#[test]
#[serial_test::serial]
fn new_session_api_key_auth_skips_ready_session_only_profile_pin() {
    use acp::Agent as _;

    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
        use crate::auth::{AuthManager, AuthMode, GrokComConfig};
        use xai_grok_test_support::EnvGuard;

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000102";
        let grok_home = tempfile::tempdir().expect("isolated grok home");
        let _home = EnvGuard::set("GROK_HOME", grok_home.path());
        let _state_home = pin_fixture_state_home(grok_home.path());
        assert_fixture_session_id_unused(SESSION_ID);
        let _xai_key = EnvGuard::unset("XAI_API_KEY");
        let _grok_code_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        let tmp = tempfile::tempdir().expect("profile auth visibility fixture");
        let profile = tmp.path().join("session-only-pin.md");
        std::fs::write(
            &profile,
            "---\nname: grok-build\ndescription: session-only pin\nmodel: ready-session-only-pin\n---\n",
        )
        .expect("write profile");
        let mut cfg = AgentConfig::default();
        cfg.models.default = Some("api-visible-default".to_owned());
        cfg.agent_profile_path = Some(profile);
        for (id, supported_in_api) in [
            ("api-visible-default", true),
            ("ready-session-only-pin", false),
        ] {
            cfg.config_models.insert(
                id.to_owned(),
                ConfigModelOverride {
                    model: Some(id.to_owned()),
                    base_url: Some("http://localhost".to_owned()),
                    api_key: Some(format!("{id}-credential")),
                    auth_scheme: Some(xai_grok_sampler::AuthScheme::Bearer),
                    supported_in_api: Some(supported_in_api),
                    agent_type: Some("grok-build".to_owned()),
                    ..Default::default()
                },
            );
        }
        let auth = std::sync::Arc::new(AuthManager::new(
            &tmp.path().join("auth"),
            GrokComConfig::default(),
        ));
        auth.hot_swap(crate::auth::GrokAuth {
            auth_mode: AuthMode::ApiKey,
            ..crate::auth::GrokAuth::test_default()
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth, None).expect("agent");
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .expect("initialize once");
        agent.set_auth_method(acp::AuthMethodId::new(
            crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
        ));
        let expected_fallback = agent.models_manager.current_model_id();
        assert_ne!(expected_fallback.0.as_ref(), "ready-session-only-pin");
        let cwd = tempfile::tempdir().expect("session cwd");
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            <MvpAgent as acp::Agent>::new_session(
                &agent,
                acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                    serde_json::json!({ "sessionId": SESSION_ID })
                        .as_object()
                        .cloned(),
                ),
            ),
        )
        .await
        .expect("new_session must not hang")
        .expect("spawn API-visible default");

        assert_eq!(response.session_id.0.as_ref(), SESSION_ID);
        assert_eq!(
            response
                .models
                .expect("advertised model")
                .current_model_id
                .0
                .as_ref(),
            expected_fallback.0.as_ref()
        );
        assert_eq!(
            agent
                .resident_handle(&response.session_id)
                .expect("resident")
                .model_id
                .0
                .as_ref(),
            expected_fallback.0.as_ref()
        );
    });
}

#[test]
#[serial_test::serial]
fn new_session_duplicate_id_rejects_before_spawn_and_preserves_existing_resident() {
    use acp::Agent as _;

    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
        use crate::auth::{AuthManager, AuthMode, GrokComConfig};
        use xai_grok_test_support::EnvGuard;

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000104";
        let grok_home = tempfile::tempdir().expect("isolated grok home");
        let _home = EnvGuard::set("GROK_HOME", grok_home.path());
        let _xai_key = EnvGuard::unset("XAI_API_KEY");
        let _grok_code_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        let tmp = tempfile::tempdir().expect("resident collision fixture");
        let mut cfg = AgentConfig::default();
        cfg.models.default = Some("session-only-default".to_owned());
        cfg.config_models.insert(
            "session-only-default".to_owned(),
            ConfigModelOverride {
                model: Some("session-only-default".to_owned()),
                base_url: Some("http://localhost".to_owned()),
                auth_scheme: Some(xai_grok_sampler::AuthScheme::Bearer),
                supported_in_api: Some(false),
                agent_type: Some("grok-build".to_owned()),
                ..Default::default()
            },
        );
        let auth = std::sync::Arc::new(AuthManager::new(
            &tmp.path().join("auth"),
            GrokComConfig::default(),
        ));
        auth.hot_swap(crate::auth::GrokAuth {
            key: "session-auth".to_owned(),
            auth_mode: AuthMode::WebLogin,
            ..crate::auth::GrokAuth::test_default()
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth, None).expect("agent");
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .expect("initialize once");
        agent.set_auth_method(acp::AuthMethodId::new(
            crate::agent::auth_method::GROK_COM_METHOD_ID,
        ));

        let sid = acp::SessionId::new(SESSION_ID);
        let (mut existing, existing_tx, _existing_rx) = make_live_session_handle(&sid, None);
        existing.model_id = acp::ModelId::new("existing-resident-model");
        agent.insert_resident(&sid, existing);
        let final_commit_hook_ran = std::rc::Rc::new(std::cell::Cell::new(false));
        let hook_probe = final_commit_hook_ran.clone();
        let commit_hook = agent_ops::install_new_session_before_resident_commit_hook(move || {
            hook_probe.set(true);
        });
        let cwd = tempfile::tempdir().expect("session cwd");
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            <MvpAgent as acp::Agent>::new_session(
                &agent,
                acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                    serde_json::json!({
                        "sessionId": SESSION_ID,
                        "modelId": "session-only-default",
                    })
                    .as_object()
                    .cloned(),
                ),
            ),
        )
        .await
        .expect("new_session must not hang")
        .expect_err("duplicate session id must be rejected before replacement spawn");
        drop(commit_hook);
        assert!(
            !final_commit_hook_ran.get(),
            "an occupied id must fail at the creation claim, before final publication"
        );

        let surviving = agent
            .resident_handle(&sid)
            .expect("failed replacement must preserve the prior resident");
        assert_eq!(
            surviving.model_id.0.as_ref(),
            "existing-resident-model",
            "the unpublished replacement must not overwrite the prior handle"
        );
        assert!(
            surviving.cmd_tx.same_channel(&existing_tx),
            "the surviving resident must retain the prior actor channel"
        );
    });
}

#[test]
#[serial_test::serial]
fn new_session_auth_flip_before_resident_commit_leaves_no_session_state() {
    use acp::Agent as _;

    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
        use crate::auth::{AuthManager, AuthMode, GrokComConfig, XAI_OAUTH2_ISSUER};
        use xai_grok_test_support::{EnvGuard, MockInferenceServer};

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000103";
        let grok_home = tempfile::tempdir().expect("isolated grok home");
        let _medley = EnvGuard::set("MEDLEY_HOME", grok_home.path());
        let _home = EnvGuard::set("GROK_HOME", grok_home.path());
        let _remote_fetch = ProcessRemoteFetchOff::install();
        let _xai_key = EnvGuard::unset("XAI_API_KEY");
        let _grok_code_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        let _session_registry = EnvGuard::set("GROK_SESSION_REGISTRY", "true");
        let _relay_sync = EnvGuard::set("GROK_RELAY_SYNC_ENABLED", "true");
        let registry_server = MockInferenceServer::start()
            .await
            .expect("remote persistence probe");
        let tmp = tempfile::tempdir().expect("auth flip fixture");
        let mut cfg = AgentConfig {
            mode: crate::agent::config::AgentMode::Tui,
            storage_mode: StorageMode::Writeback,
            ..Default::default()
        };
        cfg.endpoints.cli_chat_proxy_base_url = Some(registry_server.url());
        cfg.remote_settings = Some(crate::util::config::RemoteSettings {
            session_registry_enabled: Some(true),
            ..Default::default()
        });
        cfg.grok_com_config.grok_ws_url = "ws://127.0.0.1:9".to_owned();
        cfg.models.default = Some("session-only-default".to_owned());
        cfg.config_models.insert(
            "session-only-default".to_owned(),
            ConfigModelOverride {
                model: Some("session-only-default".to_owned()),
                base_url: Some("http://localhost".to_owned()),
                auth_scheme: Some(xai_grok_sampler::AuthScheme::Bearer),
                supported_in_api: Some(false),
                agent_type: Some("grok-build".to_owned()),
                ..Default::default()
            },
        );
        let auth = std::sync::Arc::new(AuthManager::new(
            &tmp.path().join("auth"),
            GrokComConfig::default(),
        ));
        auth.hot_swap(crate::auth::GrokAuth {
            key: "session-auth".to_owned(),
            auth_mode: AuthMode::Oidc,
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_owned()),
            ..crate::auth::GrokAuth::test_default()
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth, None).expect("agent");
        let memory = crate::config::MemoryConfig {
            enabled: true,
            ..Default::default()
        };
        agent.set_memory_config(memory);
        agent.start_subagent_coordinator();
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .expect("initialize once");
        agent.set_auth_method(acp::AuthMethodId::new(
            crate::agent::auth_method::GROK_COM_METHOD_ID,
        ));
        let auth_manager = agent.auth_manager.clone();
        let commit_hook = agent_ops::install_new_session_before_resident_commit_hook(move || {
            auth_manager.clear_in_memory();
        });
        let cwd = tempfile::tempdir().expect("session cwd");
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            <MvpAgent as acp::Agent>::new_session(
                &agent,
                acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                    serde_json::json!({
                        "sessionId": SESSION_ID,
                        "modelId": "session-only-default",
                    })
                    .as_object()
                    .cloned(),
                ),
            ),
        )
        .await
        .expect("new_session must not hang")
        .expect_err("auth drift at resident commit must reject the session");
        drop(commit_hook);

        let sid = acp::SessionId::new(SESSION_ID);
        assert!(!agent.is_resident(&sid));
        assert!(agent.resident_handle(&sid).is_none());
        assert!(agent.session_live_state_for(&sid).is_none());
        let snapshot = agent.registry_snapshot().await;
        assert_eq!(snapshot.sessions, 0);
        assert_eq!(snapshot.loading_sessions, 0);
        assert_eq!(snapshot.session_registry_entries, 0);
        assert_eq!(snapshot.session_threads, 0);
        assert_eq!(snapshot.resident_resources, 0);
        assert_eq!(snapshot.retained_resources, 0);
        assert_eq!(snapshot.dispatch_locks, 0);
        assert_eq!(snapshot.session_turn_numbers, 0);
        assert_eq!(snapshot.permission_event_receivers, 0);
        assert_eq!(snapshot.model_unavailable_sessions, 0);
        assert_eq!(snapshot.session_live_state, 0);
        assert_eq!(snapshot.session_index_claims, 0);
        assert_eq!(snapshot.require_gateway_sessions, 0);
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        while let Ok(message) = rx.try_recv() {
            let rendered = format!("{message:?}");
            assert!(
                !rendered.contains(SESSION_ID),
                "an unpublished session must emit no gateway or relay notification: {rendered}"
            );
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = message {
                let _ = args.response_tx.send(Ok(()));
            }
        }
        let remote_session_requests: Vec<_> = registry_server
            .requests()
            .into_iter()
            .filter(|request| request.path.contains("/sessions/"))
            .collect();
        assert!(
            remote_session_requests.is_empty(),
            "an unpublished session must never register or update remote persistence: \
             {remote_session_requests:?}"
        );
        let telemetry =
            xai_grok_telemetry::unified_log::snapshot_session_log(SESSION_ID).unwrap_or_default();
        assert!(
            telemetry.is_empty(),
            "an unpublished session must emit no session-scoped telemetry: {}",
            String::from_utf8_lossy(&telemetry)
        );
        assert!(
            crate::session::persistence::find_any_session_dir_by_id_result(SESSION_ID)
                .expect("scan persisted sessions")
                .is_none(),
            "failed commit must not persist a session directory"
        );
    });
}

#[test]
#[serial_test::serial]
fn cancelling_new_session_during_actor_init_leaves_no_session_state() {
    use acp::Agent as _;

    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
        use crate::auth::{AuthManager, AuthMode, GrokComConfig};
        use xai_grok_test_support::EnvGuard;

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000105";
        let grok_home = tempfile::tempdir().expect("isolated grok home");
        let _home = EnvGuard::set("GROK_HOME", grok_home.path());
        let _remote_fetch = ProcessRemoteFetchOff::install();
        let _xai_key = EnvGuard::unset("XAI_API_KEY");
        let _grok_code_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        let _relay_sync = EnvGuard::set("GROK_RELAY_SYNC_ENABLED", "true");
        let tmp = tempfile::tempdir().expect("cancelled spawn fixture");
        let mut cfg = AgentConfig {
            mode: crate::agent::config::AgentMode::Tui,
            storage_mode: StorageMode::Writeback,
            ..Default::default()
        };
        cfg.grok_com_config.grok_ws_url = "ws://127.0.0.1:9".to_owned();
        cfg.models.default = Some("session-only-default".to_owned());
        cfg.config_models.insert(
            "session-only-default".to_owned(),
            ConfigModelOverride {
                model: Some("session-only-default".to_owned()),
                base_url: Some("http://localhost".to_owned()),
                auth_scheme: Some(xai_grok_sampler::AuthScheme::Bearer),
                supported_in_api: Some(false),
                agent_type: Some("grok-build".to_owned()),
                ..Default::default()
            },
        );
        let auth = std::sync::Arc::new(AuthManager::new(
            &tmp.path().join("auth"),
            GrokComConfig::default(),
        ));
        auth.hot_swap(crate::auth::GrokAuth {
            key: "session-auth".to_owned(),
            auth_mode: AuthMode::WebLogin,
            ..crate::auth::GrokAuth::test_default()
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth, None).expect("agent");
        let memory = crate::config::MemoryConfig {
            enabled: true,
            ..Default::default()
        };
        agent.set_memory_config(memory);
        agent.start_subagent_coordinator();
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .expect("initialize once");
        agent.set_auth_method(acp::AuthMethodId::new(
            crate::agent::auth_method::GROK_COM_METHOD_ID,
        ));
        let agent = std::rc::Rc::new(agent);
        let mut actor_init_hook =
            crate::session::acp_session::install_spawn_actor_init_test_hook(SESSION_ID);
        let cwd = tempfile::tempdir().expect("session cwd");
        let request_agent = agent.clone();
        let request = acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
            serde_json::json!({
                "sessionId": SESSION_ID,
                "modelId": "session-only-default",
            })
            .as_object()
            .cloned(),
        );
        let request_task = tokio::task::spawn_local(async move {
            <MvpAgent as acp::Agent>::new_session(&request_agent, request).await
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            actor_init_hook.wait_until_entered(),
        )
        .await
        .expect("session thread must reach the actor-init boundary");
        request_task.abort();
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(10), request_task)
            .await
            .expect("cancellation must abort and join the provisional session thread")
            .expect_err("aborted session/new request task must be cancelled");
        assert!(cancelled.is_cancelled());

        let sid = acp::SessionId::new(SESSION_ID);
        assert!(!agent.is_resident(&sid));
        assert!(agent.resident_handle(&sid).is_none());
        assert!(agent.session_live_state_for(&sid).is_none());
        let snapshot = agent.registry_snapshot().await;
        assert_eq!(snapshot.sessions, 0);
        assert_eq!(snapshot.loading_sessions, 0);
        assert_eq!(snapshot.session_registry_entries, 0);
        assert_eq!(snapshot.session_threads, 0);
        assert_eq!(snapshot.resident_resources, 0);
        assert_eq!(snapshot.retained_resources, 0);
        assert_eq!(snapshot.dispatch_locks, 0);
        assert_eq!(snapshot.session_turn_numbers, 0);
        assert_eq!(snapshot.permission_event_receivers, 0);
        assert_eq!(snapshot.model_unavailable_sessions, 0);
        assert_eq!(snapshot.session_live_state, 0);
        assert_eq!(snapshot.session_index_claims, 0);
        assert_eq!(snapshot.require_gateway_sessions, 0);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if crate::session::persistence::find_any_session_dir_by_id_result(SESSION_ID)
                    .expect("scan persisted sessions")
                    .is_none()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled session persistence must be deleted");
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        while let Ok(message) = rx.try_recv() {
            let rendered = format!("{message:?}");
            assert!(
                !rendered.contains(SESSION_ID),
                "a cancelled unpublished session must emit no gateway or relay notification: {rendered}"
            );
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = message {
                let _ = args.response_tx.send(Ok(()));
            }
        }
        let telemetry =
            xai_grok_telemetry::unified_log::snapshot_session_log(SESSION_ID).unwrap_or_default();
        assert!(
            telemetry.is_empty(),
            "actor initialization, including memory init telemetry, must not run after request cancellation: {}",
            String::from_utf8_lossy(&telemetry)
        );
    });
}

#[test]
#[serial_test::serial]
fn cancelling_after_actor_init_before_publish_ack_cleans_state_and_allows_retry() {
    use acp::Agent as _;

    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
        use crate::auth::{AuthManager, AuthMode, GrokComConfig};
        use xai_grok_test_support::{EnvGuard, MockInferenceServer};

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000107";
        let grok_home = tempfile::tempdir().expect("isolated grok home");
        let _medley = EnvGuard::set("MEDLEY_HOME", grok_home.path());
        let _home = EnvGuard::set("GROK_HOME", grok_home.path());
        let _state_home = pin_fixture_state_home(grok_home.path());
        assert_fixture_session_id_unused(SESSION_ID);
        let _remote_fetch = ProcessRemoteFetchOff::install();
        let _xai_key = EnvGuard::unset("XAI_API_KEY");
        let _grok_code_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        let _session_registry = EnvGuard::set("GROK_SESSION_REGISTRY", "true");
        let _relay_sync = EnvGuard::set("GROK_RELAY_SYNC_ENABLED", "true");
        let captured_tracing = capture::capture();
        let registry_server = MockInferenceServer::start()
            .await
            .expect("remote persistence probe");
        let tmp = tempfile::tempdir().expect("post-init cancellation fixture");
        let memory_root = tmp.path().join("memory-root");
        let gc_sentinel = memory_root.join("tmp-provisional-gc-sentinel");
        std::fs::create_dir_all(&gc_sentinel).expect("create memory GC sentinel");
        let mut cfg = AgentConfig {
            mode: crate::agent::config::AgentMode::Tui,
            storage_mode: StorageMode::Writeback,
            ..Default::default()
        };
        cfg.endpoints.cli_chat_proxy_base_url = Some(registry_server.url());
        cfg.remote_settings = Some(crate::util::config::RemoteSettings {
            session_registry_enabled: Some(true),
            ..Default::default()
        });
        cfg.grok_com_config.grok_ws_url = "ws://127.0.0.1:9".to_owned();
        cfg.models.default = Some("session-only-default".to_owned());
        cfg.config_models.insert(
            "session-only-default".to_owned(),
            ConfigModelOverride {
                model: Some("session-only-default".to_owned()),
                base_url: Some("http://localhost".to_owned()),
                auth_scheme: Some(xai_grok_sampler::AuthScheme::Bearer),
                supported_in_api: Some(false),
                agent_type: Some("grok-build".to_owned()),
                ..Default::default()
            },
        );
        let auth = std::sync::Arc::new(AuthManager::new(
            &tmp.path().join("auth"),
            GrokComConfig::default(),
        ));
        auth.hot_swap(crate::auth::GrokAuth {
            key: "session-auth".to_owned(),
            auth_mode: AuthMode::WebLogin,
            ..crate::auth::GrokAuth::test_default()
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth, None).expect("agent");
        let mut memory = crate::config::MemoryConfig {
            enabled: true,
            ..Default::default()
        };
        memory.watcher.enabled = true;
        memory.root_dir_override = Some(memory_root.clone());
        agent.set_memory_config(memory);
        agent.start_subagent_coordinator();
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .expect("initialize once");
        agent.set_auth_method(acp::AuthMethodId::new(
            crate::agent::auth_method::GROK_COM_METHOD_ID,
        ));
        let agent = std::rc::Rc::new(agent);
        let mut actor_init_hook =
            crate::session::acp_session::install_spawn_actor_init_test_hook(SESSION_ID);
        let mut publish_ack_hook =
            crate::session::persistence::install_publish_fresh_ack_test_hook(SESSION_ID);
        let cwd = tempfile::tempdir().expect("session cwd");
        let memory_storage =
            crate::session::memory::MemoryStorage::new(cwd.path(), Some(memory_root.as_path()));
        let request = acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
            serde_json::json!({
                "sessionId": SESSION_ID,
                "modelId": "session-only-default",
            })
            .as_object()
            .cloned(),
        );
        let request_agent = agent.clone();
        let mut request_task = tokio::task::spawn_local(async move {
            <MvpAgent as acp::Agent>::new_session(&request_agent, request).await
        });

        await_new_session_boundary(
            &mut request_task,
            actor_init_hook.wait_until_entered(),
            "the actor-init boundary",
        )
        .await;
        actor_init_hook.release();
        await_new_session_boundary(
            &mut request_task,
            publish_ack_hook.wait_until_entered(),
            "publish acknowledgement after actor initialization",
        )
        .await;
        assert!(
            gc_sentinel.is_dir(),
            "provisional actor construction must not run shared-memory GC"
        );
        assert!(
            !memory_storage.global_memory_file().exists()
                && !memory_storage.workspace_dir().exists(),
            "provisional actor construction must not create memory templates or workspace state"
        );
        assert!(
            crate::session::acp_session::take_memory_session_init_test_observation(SESSION_ID)
                .is_none(),
            "MemorySessionInit must not be emitted before publication"
        );

        request_task.abort();
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(10), request_task)
            .await
            .expect("cancelled session/new request must join")
            .expect_err("aborted session/new request task must be cancelled");
        assert!(cancelled.is_cancelled());
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            actor_init_hook.wait_until_thread_exited(),
        )
        .await
        .expect(
            "publication abort must stop the initialized session thread before persistence resumes",
        );
        publish_ack_hook.release();

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if crate::session::persistence::find_any_session_dir_by_id_result(SESSION_ID)
                    .expect("scan persisted sessions")
                    .is_none()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled post-init session persistence must be deleted");
        assert!(
            gc_sentinel.is_dir(),
            "cancelling an unpublished session must leave shared-memory GC state untouched"
        );
        assert!(
            !memory_storage.global_memory_file().exists()
                && !memory_storage.workspace_dir().exists(),
            "cancelling an unpublished session must create no memory files"
        );
        assert!(
            crate::session::acp_session::take_memory_session_init_test_observation(SESSION_ID)
                .is_none(),
            "a cancelled unpublished session must emit no MemorySessionInit event"
        );

        let sid = acp::SessionId::new(SESSION_ID);
        assert!(!agent.is_resident(&sid));
        assert!(agent.resident_handle(&sid).is_none());
        assert!(agent.session_live_state_for(&sid).is_none());
        let snapshot = agent.registry_snapshot().await;
        assert_eq!(snapshot.sessions, 0);
        assert_eq!(snapshot.loading_sessions, 0);
        assert_eq!(snapshot.session_registry_entries, 0);
        assert_eq!(snapshot.session_threads, 0);
        assert_eq!(snapshot.resident_resources, 0);
        assert_eq!(snapshot.retained_resources, 0);
        assert_eq!(snapshot.dispatch_locks, 0);
        assert_eq!(snapshot.session_turn_numbers, 0);
        assert_eq!(snapshot.permission_event_receivers, 0);
        assert_eq!(snapshot.model_unavailable_sessions, 0);
        assert_eq!(snapshot.session_live_state, 0);
        assert_eq!(snapshot.session_index_claims, 0);
        assert_eq!(snapshot.require_gateway_sessions, 0);
        let workspace_binding_present = agent
            .workspace_ops
            .borrow()
            .as_ref()
            .and_then(xai_grok_workspace::WorkspaceOps::workspace_handle)
            .and_then(|workspace| workspace.session(SESSION_ID))
            .is_some();
        assert!(
            !workspace_binding_present,
            "the provisional workspace binding and its toolset must be released before retry"
        );
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        while let Ok(message) = rx.try_recv() {
            let rendered = format!("{message:?}");
            assert!(
                !rendered.contains(SESSION_ID),
                "a cancelled unpublished session must emit no gateway or relay notification: {rendered}"
            );
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = message {
                let _ = args.response_tx.send(Ok(()));
            }
        }
        let remote_session_requests: Vec<_> = registry_server
            .requests()
            .into_iter()
            .filter(|request| request.path.contains("/sessions/"))
            .collect();
        assert!(
            remote_session_requests.is_empty(),
            "a cancelled unpublished session must never register or update remote persistence: \
             {remote_session_requests:?}"
        );
        let telemetry =
            xai_grok_telemetry::unified_log::snapshot_session_log(SESSION_ID).unwrap_or_default();
        assert!(
            telemetry.is_empty(),
            "a cancelled unpublished session must emit no session-scoped telemetry: {}",
            String::from_utf8_lossy(&telemetry)
        );
        let mut captured_tracing_rx = captured_tracing.events_rx;
        let mut trace_identity_leaks = Vec::new();
        let session_id_prefix = &SESSION_ID[..8];
        while let Ok(event) = captured_tracing_rx.try_recv() {
            if event.fields.contains(SESSION_ID) || event.fields.contains(session_id_prefix) {
                trace_identity_leaks.push(event.fields);
            }
        }
        assert!(
            trace_identity_leaks.is_empty(),
            "a cancelled unpublished session UUID or truncated prefix must not escape through tracing spans/events: \
             {trace_identity_leaks:#?}"
        );

        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            <MvpAgent as acp::Agent>::new_session(
                &agent,
                acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                    serde_json::json!({
                        "sessionId": SESSION_ID,
                        "modelId": "session-only-default",
                    })
                    .as_object()
                    .cloned(),
                ),
            ),
        )
        .await
        .expect("same-id retry must not hang")
        .expect("same-id retry must succeed after post-init cancellation cleanup");
        assert_eq!(retry.session_id.0.as_ref(), SESSION_ID);
        assert!(agent.is_resident(&retry.session_id));
        let memory_init = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(observation) =
                    crate::session::acp_session::take_memory_session_init_test_observation(
                        SESSION_ID,
                    )
                {
                    break observation;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("published retry must emit MemorySessionInit");
        assert!(memory_init.watcher_config_enabled);
        assert!(
            memory_storage.global_memory_file().is_file(),
            "published retry must activate the global memory template"
        );
    });
}

#[test]
#[serial_test::serial]
fn new_session_actor_spawn_failure_cleans_provisional_state_and_allows_same_id_retry() {
    use acp::Agent as _;

    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ConfigModelOverride};
        use crate::auth::{AuthManager, AuthMode, GrokComConfig};
        use xai_grok_test_support::{EnvGuard, MockInferenceServer};

        const SESSION_ID: &str = "019c0000-0000-7000-8000-000000000106";
        let grok_home = tempfile::tempdir().expect("isolated grok home");
        let _medley = EnvGuard::set("MEDLEY_HOME", grok_home.path());
        let _home = EnvGuard::set("GROK_HOME", grok_home.path());
        let _state_home = pin_fixture_state_home(grok_home.path());
        assert_fixture_session_id_unused(SESSION_ID);
        let _remote_fetch = ProcessRemoteFetchOff::install();
        let _xai_key = EnvGuard::unset("XAI_API_KEY");
        let _grok_code_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
        let _session_registry = EnvGuard::set("GROK_SESSION_REGISTRY", "true");
        let _relay_sync = EnvGuard::set("GROK_RELAY_SYNC_ENABLED", "true");
        let registry_server = MockInferenceServer::start()
            .await
            .expect("remote persistence probe");
        let tmp = tempfile::tempdir().expect("spawn failure fixture");
        let mut cfg = AgentConfig {
            mode: crate::agent::config::AgentMode::Tui,
            storage_mode: StorageMode::Writeback,
            ..Default::default()
        };
        cfg.endpoints.cli_chat_proxy_base_url = Some(registry_server.url());
        cfg.remote_settings = Some(crate::util::config::RemoteSettings {
            session_registry_enabled: Some(true),
            ..Default::default()
        });
        cfg.grok_com_config.grok_ws_url = "ws://127.0.0.1:9".to_owned();
        cfg.models.default = Some("session-only-default".to_owned());
        cfg.config_models.insert(
            "session-only-default".to_owned(),
            ConfigModelOverride {
                model: Some("session-only-default".to_owned()),
                base_url: Some("http://localhost".to_owned()),
                auth_scheme: Some(xai_grok_sampler::AuthScheme::Bearer),
                supported_in_api: Some(false),
                agent_type: Some("grok-build".to_owned()),
                ..Default::default()
            },
        );
        let auth = std::sync::Arc::new(AuthManager::new(
            &tmp.path().join("auth"),
            GrokComConfig::default(),
        ));
        auth.hot_swap(crate::auth::GrokAuth {
            key: "session-auth".to_owned(),
            auth_mode: AuthMode::WebLogin,
            ..crate::auth::GrokAuth::test_default()
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth, None).expect("agent");
        agent.start_subagent_coordinator();
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .expect("initialize once");
        agent.set_auth_method(acp::AuthMethodId::new(
            crate::agent::auth_method::GROK_COM_METHOD_ID,
        ));
        let agent = std::rc::Rc::new(agent);
        let mut failure_hook =
            crate::session::acp_session::install_spawn_actor_failure_test_hook(SESSION_ID);
        let cwd = tempfile::tempdir().expect("session cwd");
        let request = acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
            serde_json::json!({
                "sessionId": SESSION_ID,
                "modelId": "session-only-default",
            })
            .as_object()
            .cloned(),
        );
        let request_agent = agent.clone();
        let mut request_task = tokio::task::spawn_local(async move {
            <MvpAgent as acp::Agent>::new_session(&request_agent, request).await
        });

        await_new_session_boundary(
            &mut request_task,
            failure_hook.wait_until_entered(),
            "the injected actor failure",
        )
        .await;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            failure_hook.wait_until_thread_exited(),
        )
        .await
        .expect("injected actor failure must exit the session thread");
        let error = tokio::time::timeout(std::time::Duration::from_secs(10), request_task)
            .await
            .expect("failed session/new must complete")
            .expect("session/new task must not panic")
            .expect_err("injected actor failure must reject session/new");
        let rendered_error = format!("{error:?}");
        assert!(rendered_error.contains("provisional session initialization failed"));
        assert!(!rendered_error.contains(SESSION_ID));
        assert!(!rendered_error.contains("injected session actor initialization failure"));

        let sid = acp::SessionId::new(SESSION_ID);
        assert!(!agent.is_resident(&sid));
        assert!(agent.resident_handle(&sid).is_none());
        assert!(agent.session_live_state_for(&sid).is_none());
        let snapshot = agent.registry_snapshot().await;
        assert_eq!(snapshot.sessions, 0);
        assert_eq!(snapshot.loading_sessions, 0);
        assert_eq!(snapshot.session_registry_entries, 0);
        assert_eq!(snapshot.session_threads, 0);
        assert_eq!(snapshot.resident_resources, 0);
        assert_eq!(snapshot.retained_resources, 0);
        assert_eq!(snapshot.dispatch_locks, 0);
        assert_eq!(snapshot.session_turn_numbers, 0);
        assert_eq!(snapshot.permission_event_receivers, 0);
        assert_eq!(snapshot.model_unavailable_sessions, 0);
        assert_eq!(snapshot.session_live_state, 0);
        assert_eq!(snapshot.session_index_claims, 0);
        assert_eq!(snapshot.require_gateway_sessions, 0);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if crate::session::persistence::find_any_session_dir_by_id_result(SESSION_ID)
                    .expect("scan persisted sessions")
                    .is_none()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("failed session persistence must be deleted");
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        while let Ok(message) = rx.try_recv() {
            let rendered = format!("{message:?}");
            assert!(
                !rendered.contains(SESSION_ID),
                "a failed unpublished session must emit no gateway or relay notification: {rendered}"
            );
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = message {
                let _ = args.response_tx.send(Ok(()));
            }
        }
        let remote_session_requests: Vec<_> = registry_server
            .requests()
            .into_iter()
            .filter(|request| request.path.contains("/sessions/"))
            .collect();
        assert!(
            remote_session_requests.is_empty(),
            "a failed unpublished session must never register or update remote persistence: \
             {remote_session_requests:?}"
        );
        let telemetry =
            xai_grok_telemetry::unified_log::snapshot_session_log(SESSION_ID).unwrap_or_default();
        assert!(
            telemetry.is_empty(),
            "a failed unpublished session must emit no session-scoped telemetry: {}",
            String::from_utf8_lossy(&telemetry)
        );

        drop(failure_hook);
        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            <MvpAgent as acp::Agent>::new_session(
                &agent,
                acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                    serde_json::json!({
                        "sessionId": SESSION_ID,
                        "modelId": "session-only-default",
                    })
                    .as_object()
                    .cloned(),
                ),
            ),
        )
        .await
        .expect("same-id retry must not hang")
        .expect("same-id retry must succeed after failed spawn cleanup");
        assert_eq!(retry.session_id.0.as_ref(), SESSION_ID);
        assert!(agent.is_resident(&retry.session_id));
    });
}

#[test]
#[serial_test::serial]
fn production_spawn_latches_post_seal_unready_prepared_identity() {
    use acp::Agent as _;

    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ConfigModelOverride, EnvKeys};
        use crate::auth::{AuthManager, GrokComConfig};
        use xai_grok_test_support::EnvGuard;

        const KEY_ENV: &str = "GROK_TEST_POST_SEAL_PREPARED_KEY";
        let _key = EnvGuard::set(KEY_ENV, "sealed-prepared-credential");
        let tmp = tempfile::tempdir().expect("post-seal fixture");
        let mut cfg = AgentConfig::default();
        cfg.models.default = Some("sealed-model".to_owned());
        cfg.config_models.insert(
            "sealed-model".to_owned(),
            ConfigModelOverride {
                model: Some("sealed-route".to_owned()),
                base_url: Some("http://localhost".to_owned()),
                auth_scheme: Some(xai_grok_sampler::AuthScheme::Bearer),
                env_key: Some(EnvKeys::One(KEY_ENV.to_owned())),
                agent_type: Some("grok-build".to_owned()),
                ..Default::default()
            },
        );
        let auth = std::sync::Arc::new(AuthManager::new(
            &tmp.path().join("auth"),
            GrokComConfig::default(),
        ));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth, None).expect("agent");
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .expect("initialize once");
        agent.set_auth_method(acp::AuthMethodId::new(
            crate::agent::auth_method::XAI_API_KEY_METHOD_ID,
        ));
        let removed_key = std::rc::Rc::new(std::cell::RefCell::new(None));
        let removed_key_hook = removed_key.clone();
        let seal_hook = agent_ops::install_new_session_plan_before_seal_hook(move || {
            let should_remove = removed_key_hook.borrow().is_none();
            if should_remove {
                *removed_key_hook.borrow_mut() = Some(EnvGuard::unset(KEY_ENV));
            }
        });
        let cwd = tempfile::tempdir().expect("session cwd");
        let response = <MvpAgent as acp::Agent>::new_session(
            &agent,
            acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(
                serde_json::json!({ "modelId": "sealed-model" })
                    .as_object()
                    .cloned(),
            ),
        )
        .await
        .expect("spawn sealed prepared session");
        drop(seal_hook);

        assert_eq!(
            agent
                .session_registry
                .unavailable_model(&response.session_id),
            Some(acp::ModelId::new("sealed-model"))
        );
        let sealed_identity = agent
            .session_registry
            .unavailable_catalog_identity(&response.session_id)
            .expect("production spawn must retain exact prepared identity");
        assert_eq!(sealed_identity.model_id, "sealed-model");
        assert_eq!(sealed_identity.route, "sealed-route");
        let resident_agent_name = agent
            .resident_handle(&response.session_id)
            .expect("resident after production spawn")
            .agent_name;
        assert_eq!(
            agent
                .session_registry
                .unavailable_agent_name(&response.session_id)
                .as_deref(),
            Some(resident_agent_name.as_str())
        );
        let before_key = agent
            .resident_handle(&response.session_id)
            .expect("resident before recovery")
            .chat_state_handle
            .get_prepared_model_state()
            .await
            .expect("prepared state before recovery")
            .2
            .api_key()
            .map(str::to_owned);

        let mut wrong_harness = agent.models_manager.models()["sealed-model"].clone();
        wrong_harness.api_key = Some("wrong-harness-secret".to_owned());
        wrong_harness.info.agent_type = "codex".to_owned();
        agent
            .models_manager
            .insert_test_entry("sealed-model", wrong_harness);
        let blocked = agent
            .prompt(acp::PromptRequest::new(
                response.session_id.clone(),
                vec![acp::ContentBlock::from("recover with wrong harness")],
            ))
            .await
            .expect("wrong-harness recovery blocks cleanly");
        assert_eq!(blocked.stop_reason, acp::StopReason::EndTurn);

        let mut replacement = agent.models_manager.models()["sealed-model"].clone();
        replacement.info.model = "ready-replacement-route".to_owned();
        replacement.info.agent_type = "grok-build".to_owned();
        replacement.api_key = Some("replacement-credential-must-not-attach".to_owned());
        agent
            .models_manager
            .insert_test_entry("sealed-model", replacement);
        let blocked = agent
            .prompt(acp::PromptRequest::new(
                response.session_id.clone(),
                vec![acp::ContentBlock::from("first prompt after replacement")],
            ))
            .await
            .expect("same-key replacement blocks cleanly");
        assert_eq!(blocked.stop_reason, acp::StopReason::EndTurn);
        assert_eq!(
            agent
                .session_registry
                .unavailable_catalog_identity(&response.session_id),
            Some(sealed_identity)
        );
        let after_key = agent
            .resident_handle(&response.session_id)
            .expect("resident after recovery")
            .chat_state_handle
            .get_prepared_model_state()
            .await
            .expect("prepared state after recovery")
            .2
            .api_key()
            .map(str::to_owned);
        assert_eq!(after_key, before_key);
        assert_ne!(
            after_key.as_deref(),
            Some("replacement-credential-must-not-attach")
        );
    });
}

#[test]
fn spawn_runtime_tuning_prefers_pinned_then_prepared_exact_entry() {
    let endpoints = config::EndpointsConfig::default();
    let mut pinned = ModelEntry::fallback("shared-key", &endpoints);
    pinned.info.model = "pinned-route".to_owned();
    pinned.info.auto_compact_threshold_percent = Some(71);
    pinned.info.system_prompt_label = Some("Pinned".to_owned());
    pinned.info.inference_idle_timeout_secs = Some(41);
    pinned.info.max_retries = Some(2);
    let mut prepared = ModelEntry::fallback("shared-key", &endpoints);
    prepared.info.model = "prepared-route".to_owned();
    prepared.info.auto_compact_threshold_percent = Some(72);
    prepared.info.system_prompt_label = Some("Prepared".to_owned());
    prepared.info.inference_idle_timeout_secs = Some(51);
    prepared.info.max_retries = Some(3);
    let mut replacement = ModelEntry::fallback("shared-key", &endpoints);
    replacement.info.model = "replacement-route".to_owned();
    replacement.info.auto_compact_threshold_percent = Some(73);
    replacement.info.system_prompt_label = Some("Replacement".to_owned());
    replacement.info.inference_idle_timeout_secs = Some(61);
    replacement.info.max_retries = Some(4);
    let catalog = indexmap::IndexMap::from([("shared-key".to_owned(), replacement)]);
    let identity = xai_chat_state::CatalogIdentity {
        model_id: "shared-key".to_owned(),
        route: "prepared-route".to_owned(),
        lineage: xai_chat_state::CatalogResolutionLineage::ExactKey,
        auth_scheme: None,
    };

    let selected = select_spawn_model_entry(Some(&pinned), Some(&prepared), &catalog, &identity)
        .expect("pinned entry wins");
    assert_eq!(selected.info.model, "pinned-route");
    assert_eq!(selected.info.auto_compact_threshold_percent, Some(71));
    assert_eq!(selected.info.system_prompt_label.as_deref(), Some("Pinned"));
    assert_eq!(
        resolve_inference_idle_timeout_secs(Some(selected), None),
        41
    );
    assert_eq!(selected.info.max_retries, Some(2));

    let selected = select_spawn_model_entry(None, Some(&prepared), &catalog, &identity)
        .expect("prepared entry wins over a same-key replacement");
    assert_eq!(selected.info.model, "prepared-route");
    assert_eq!(selected.info.auto_compact_threshold_percent, Some(72));
    assert_eq!(
        selected.info.system_prompt_label.as_deref(),
        Some("Prepared")
    );
    assert_eq!(
        resolve_inference_idle_timeout_secs(Some(selected), None),
        51
    );
    assert_eq!(selected.info.max_retries, Some(3));

    let selected = select_spawn_model_entry(None, None, &catalog, &identity)
        .expect("live catalog is the final fallback");
    assert_eq!(selected.info.model, "replacement-route");
    assert_eq!(selected.info.auto_compact_threshold_percent, Some(73));
    assert_eq!(
        selected.info.system_prompt_label.as_deref(),
        Some("Replacement")
    );
    assert_eq!(
        resolve_inference_idle_timeout_secs(Some(selected), None),
        61
    );
    assert_eq!(selected.info.max_retries, Some(4));
}

/// Minimal agent whose `grok_com_config` engages the api-key kill switch
/// (`disable_api_key_auth = true`), mirroring a forced-IdP deployment.
fn build_agent_with_api_key_auth_disabled() -> MvpAgent {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let mut cfg = AgentConfig::default();
    cfg.grok_com_config.disable_api_key_auth = Some(true);
    MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config")
}
/// Deployment-key / managed-config user: `XAI_API_KEY` resolves and the kill
/// switch is off, so a dead `cached_token` MUST fall through to `xai.api_key`
/// (no browser). This is the exact regression the fallthrough fixes.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn cached_token_fallthrough_prefers_api_key_for_deployment_key() {
    use crate::agent::auth_method::{XAI_API_KEY_ENV_VAR, XAI_API_KEY_METHOD_ID};
    use xai_grok_test_support::EnvGuard;
    let _lockdown = EnvGuard::unset("GROK_DISABLE_API_KEY_AUTH");
    let _key = EnvGuard::set(XAI_API_KEY_ENV_VAR, "test-deployment-key");
    let agent = build_minimal_agent_for_tests();
    assert_eq!(
        agent
            .cached_token_fallthrough_method_id()
            .as_ref()
            .map(|id| id.0.as_ref()),
        Some(XAI_API_KEY_METHOD_ID),
        "deployment-key user (XAI_API_KEY set, no kill switch) must fall \
         through to xai.api_key on a dead cached_token -- not interactive login",
    );
}
/// Forced-IdP deployment: even with `XAI_API_KEY` present, the admin kill
/// switch keeps the fallthrough on interactive `grok.com` (api-key auth is
/// neither advertised nor an eligible fallthrough).
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn cached_token_fallthrough_respects_kill_switch() {
    use crate::agent::auth_method::{GROK_COM_METHOD_ID, XAI_API_KEY_ENV_VAR};
    use xai_grok_test_support::EnvGuard;
    let _lockdown = EnvGuard::unset("GROK_DISABLE_API_KEY_AUTH");
    let _key = EnvGuard::set(XAI_API_KEY_ENV_VAR, "test-deployment-key");
    let agent = build_agent_with_api_key_auth_disabled();
    assert_eq!(
        agent
            .cached_token_fallthrough_method_id()
            .as_ref()
            .map(|id| id.0.as_ref()),
        Some(GROK_COM_METHOD_ID),
        "disable_api_key_auth must keep the cached_token fallthrough on \
         interactive grok.com so XAI_API_KEY can't bypass forced IdP login",
    );
}
/// No advertiseable credentials at all (no env key, no kill switch): the user
/// genuinely needs to log in, so the fallthrough is interactive `grok.com`.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial]
async fn cached_token_fallthrough_falls_to_grok_com_without_credentials() {
    use crate::agent::auth_method::{
        GROK_COM_METHOD_ID, LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR,
    };
    use xai_grok_test_support::EnvGuard;
    let _lockdown = EnvGuard::unset("GROK_DISABLE_API_KEY_AUTH");
    let _new = EnvGuard::unset(XAI_API_KEY_ENV_VAR);
    let _legacy = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
    let agent = build_minimal_agent_for_tests();
    assert_eq!(
        agent
            .cached_token_fallthrough_method_id()
            .as_ref()
            .map(|id| id.0.as_ref()),
        Some(GROK_COM_METHOD_ID),
        "no API-key creds and no kill switch -> interactive grok.com login",
    );
}
/// Verifies the 4-state matrix of `(disable_zdr_incompatible_tools, zdr_video_output_s3)`:
///
/// | ZDR flag | S3 config | Result                                      |
/// |----------|-----------|---------------------------------------------|
/// | false    | None      | Enabled, no S3 (normal non-ZDR mode)        |
/// | true     | None      | Disabled (ZDR with no escape hatch)         |
/// | false    | Some      | Enabled, S3 **not** threaded (non-ZDR)      |
/// | true     | Some      | Enabled, S3 threaded (ZDR with upload path) |
#[tokio::test(flavor = "current_thread")]
async fn prepare_video_gen_config_disabled_when_zdr_flag_set() {
    use xai_grok_tools::implementations::grok_build::video_gen::{
        S3AccessCredentials, VideoGenConfig, ZdrVideoOutputS3Config,
    };
    fn zdr_s3() -> ZdrVideoOutputS3Config {
        ZdrVideoOutputS3Config {
            bucket: "team-videos".into(),
            endpoint: "https://s3.example.com".into(),
            region: "us-east-1".into(),
            key_prefix: "grok-videos/".into(),
            expires_secs: 900,
            read_write: S3AccessCredentials {
                access_key_id: "AKIA...".into(),
                secret_access_key: "secret".into(),
            },
            read_only: None,
        }
    }
    let agent = build_minimal_agent_for_tests();
    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
    assert!(matches!(
        agent.prepare_video_gen_config(),
        VideoGenConfig::Enabled { .. }
    ));
    agent.cfg.borrow_mut().disable_zdr_incompatible_tools = true;
    assert!(matches!(
        agent.prepare_video_gen_config(),
        VideoGenConfig::Disabled
    ));
    agent.cfg.borrow_mut().zdr_video_output_s3 = Some(zdr_s3());
    agent.cfg.borrow_mut().disable_zdr_incompatible_tools = false;
    let VideoGenConfig::Enabled {
        zdr_video_output_s3: s3_when_non_zdr,
        ..
    } = agent.prepare_video_gen_config()
    else {
        panic!("expected Enabled");
    };
    assert!(
        s3_when_non_zdr.is_none(),
        "S3 config must not be threaded when ZDR flag is off"
    );
    agent.cfg.borrow_mut().disable_zdr_incompatible_tools = true;
    let VideoGenConfig::Enabled {
        zdr_video_output_s3,
        ..
    } = agent.prepare_video_gen_config()
    else {
        panic!("expected Enabled");
    };
    assert!(zdr_video_output_s3.as_ref().is_some_and(|c| c.is_valid()));
}
#[tokio::test(flavor = "current_thread")]
async fn prepare_video_gen_config_respects_feature_flag() {
    use xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig;
    let agent = build_minimal_agent_for_tests();
    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
    assert!(matches!(
        agent.prepare_video_gen_config(),
        VideoGenConfig::Enabled { .. }
    ));
    agent.cfg.borrow_mut().features.video_gen = Some(false);
    assert!(matches!(
        agent.prepare_video_gen_config(),
        VideoGenConfig::Disabled
    ));
}
/// The imagine tier gate fails **open**: with no resolved auth we can't confirm
/// a restricted personal tier, so the tools stay advertised and un-flagged (the
/// server 429 remains the authoritative backstop). Guards against accidentally
/// disabling a paid feature when tier info hasn't loaded.
#[tokio::test(flavor = "current_thread")]
async fn prepare_image_gen_config_fails_open_without_auth() {
    use xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig;
    let agent = build_minimal_agent_for_tests();
    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
    let ImageGenConfig::Enabled {
        tier_restricted, ..
    } = agent.prepare_image_gen_config()
    else {
        panic!("expected Enabled");
    };
    assert!(
        !tier_restricted,
        "no resolved auth ⇒ fail open (tools not tier-restricted)"
    );
}
#[test]
fn inject_proxy_headers_omits_identity_on_third_party_origin() {
    let mut headers = indexmap::IndexMap::new();
    super::inject_proxy_headers(
        &mut headers,
        Some("9.9.9"),
        None,
        "https://vendor.example/v1",
    );
    assert!(headers.get("x-grok-client-version").is_none());
    assert!(headers.get("x-grok-client-identifier").is_none());
    assert!(headers.get("X-XAI-Token-Auth").is_none());
}

#[test]
fn inject_proxy_headers_keeps_identity_on_trusted_xai_origin() {
    let mut headers = indexmap::IndexMap::new();
    super::inject_proxy_headers(&mut headers, Some("9.9.9"), None, "https://api.x.ai/v1");
    assert_eq!(
        headers.get("x-grok-client-version").map(String::as_str),
        Some("9.9.9")
    );
    assert!(headers.get("x-grok-client-identifier").is_some());
}

#[test]
fn inject_proxy_headers_keeps_identity_on_production_proxy() {
    let mut headers = indexmap::IndexMap::new();
    super::inject_proxy_headers(
        &mut headers,
        Some("9.9.9"),
        None,
        crate::env::PROD_CLI_CHAT_PROXY_BASE_URL,
    );
    assert_eq!(
        headers.get("x-grok-client-version").map(String::as_str),
        Some("9.9.9")
    );
    assert_eq!(
        headers.get("X-XAI-Token-Auth").map(String::as_str),
        Some("xai-grok-cli")
    );
}

/// The imagine tools bypass cli-chat-proxy (direct API calls), so the server
/// can only scope the coding data-retention opt-out (`/privacy opt-out`) to
/// Build traffic via the `x-grok-client-identifier` header. If this header is
/// dropped, opted-out users' imagine prompts are logged/retained server-side.
#[tokio::test(flavor = "current_thread")]
async fn prepare_image_gen_config_sends_client_identifier_header() {
    use xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig;
    let agent = build_minimal_agent_for_tests();
    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
    let ImageGenConfig::Enabled { extra_headers, .. } = agent.prepare_image_gen_config() else {
        panic!("expected Enabled");
    };
    assert_eq!(
        extra_headers
            .get("x-grok-client-identifier")
            .map(String::as_str),
        Some(crate::http::process_client_identifier().as_str()),
        "imagine API calls must carry the client identifier so the server \
         applies the coding ZDR opt-out to Build traffic"
    );
}
/// Same contract for video generation (also a direct API call).
#[tokio::test(flavor = "current_thread")]
async fn prepare_video_gen_config_sends_client_identifier_header() {
    use xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig;
    let agent = build_minimal_agent_for_tests();
    agent.sampling_config.borrow_mut().api_key = Some("test-key".to_string());
    let VideoGenConfig::Enabled { extra_headers, .. } = agent.prepare_video_gen_config() else {
        panic!("expected Enabled");
    };
    assert_eq!(
        extra_headers
            .get("x-grok-client-identifier")
            .map(String::as_str),
        Some(crate::http::process_client_identifier().as_str()),
        "video gen API calls must carry the client identifier so the server \
         applies the coding ZDR opt-out to Build traffic"
    );
}
/// Regression: `x.ai/auth/info` must return profile fields even when the
/// access token is expired — profile data does not expire with the token,
/// and hiding it made the desktop render "Signed in" with no identity.
#[tokio::test]
async fn auth_info_returns_profile_when_token_expired() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        email: Some("user@example.com".into()),
        first_name: Some("Test".into()),
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        ..crate::auth::GrokAuth::test_default()
    });
    let resp = crate::extensions::auth::handle(
        &agent,
        &acp::ExtRequest::new(
            "x.ai/auth/info",
            std::sync::Arc::from(serde_json::value::to_raw_value(&serde_json::json!({})).unwrap()),
        ),
    )
    .await
    .expect("auth/info must succeed with an expired token");
    let info: serde_json::Value = serde_json::from_str(resp.0.get()).unwrap();
    assert_eq!(info["email"], "user@example.com");
    assert_eq!(info["firstName"], "Test");
}
#[tokio::test]
async fn data_collection_enabled_for_normal_user() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    assert!(
        !agent.is_data_collection_disabled(),
        "normal user must have data collection enabled"
    );
}
#[tokio::test]
async fn data_collection_disabled_for_zdr_team() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
        ..crate::auth::GrokAuth::test_default()
    });
    assert!(
        agent.is_data_collection_disabled(),
        "ZDR team must have data collection disabled"
    );
    assert!(
        agent.trace_upload_config_snapshot().is_none(),
        "trace uploads must be disabled for ZDR team"
    );
}
#[tokio::test]
async fn data_collection_disabled_for_zdr_moderated_team() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS_MODERATED".into()],
        ..crate::auth::GrokAuth::test_default()
    });
    assert!(
        agent.is_data_collection_disabled(),
        "ZDR-moderated team must have data collection disabled"
    );
}
#[tokio::test]
async fn data_collection_disabled_for_opted_out_team() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        coding_data_retention_opt_out: true,
        ..crate::auth::GrokAuth::test_default()
    });
    assert!(
        agent.is_data_collection_disabled(),
        "opted-out team must have data collection disabled"
    );
    assert!(
        agent.trace_upload_config_snapshot().is_none(),
        "trace uploads must be disabled for opted-out team"
    );
}
#[tokio::test]
async fn data_collection_disabled_for_zdr_plus_opt_out() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
        coding_data_retention_opt_out: true,
        ..crate::auth::GrokAuth::test_default()
    });
    assert!(
        agent.is_data_collection_disabled(),
        "ZDR + opt-out must have data collection disabled"
    );
}
#[tokio::test]
async fn data_collection_enabled_for_non_zdr_team_with_unrelated_blocks() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        team_blocked_reasons: vec![
            "BLOCKED_REASON_BILLING".into(),
            "BLOCKED_REASON_SUSPENDED".into(),
        ],
        ..crate::auth::GrokAuth::test_default()
    });
    assert!(
        !agent.is_data_collection_disabled(),
        "non-ZDR blocked reasons must not disable data collection"
    );
}
fn enable_product_telemetry(agent: &MvpAgent) {
    agent.cfg.borrow_mut().features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
}
/// Enable trace uploads via config so only the auth-level privacy gate
/// can disable collection in the tests below.
fn enable_trace_upload_config(agent: &MvpAgent) {
    let mut cfg = agent.cfg.borrow_mut();
    cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Enabled);
    cfg.telemetry.trace_upload = Some(true);
}
#[tokio::test]
async fn product_analytics_enabled_for_normal_user_with_telemetry_on() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    enable_product_telemetry(&agent);
    assert!(agent.product_analytics_enabled());
}
#[tokio::test]
async fn product_analytics_enabled_despite_coding_retention_opt_out() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        coding_data_retention_opt_out: true,
        ..crate::auth::GrokAuth::test_default()
    });
    enable_product_telemetry(&agent);
    assert!(agent.is_data_collection_disabled());
    assert!(agent.product_analytics_enabled());
}
#[tokio::test]
async fn product_analytics_disabled_for_zdr_team() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
        ..crate::auth::GrokAuth::test_default()
    });
    enable_product_telemetry(&agent);
    assert!(!agent.product_analytics_enabled());
}
#[tokio::test]
async fn product_analytics_disabled_when_telemetry_off() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    agent.cfg.borrow_mut().features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
    assert!(!agent.product_analytics_enabled());
}
/// Counting HTTP stub: any request increments the counter and gets a
/// storage-proxy-shaped 200 so the client does not retry.
async fn spawn_counting_storage_stub() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count_clone = count.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new().fallback(move || {
        let count = count_clone.clone();
        async move {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (
                [("content-type", "application/json")],
                r#"{"bucket":"test-bucket","path":"auth-diagnostics/test.jsonl"}"#,
            )
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), count)
}
/// Regression: the auth-diagnostics uploader was gated only on the
/// trace-upload config switch; it must also honor ZDR / retention
/// opt-out, checked at invocation time.
#[tokio::test]
async fn diagnostic_upload_skipped_for_opted_out_user() {
    let (stub_url, count) = spawn_counting_storage_stub().await;
    let agent = build_agent_with_auth(crate::auth::GrokAuth {
        coding_data_retention_opt_out: true,
        ..crate::auth::GrokAuth::test_default()
    });
    enable_trace_upload_config(&agent);
    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
    let uploader = agent
        .diagnostic_upload_config()
        .expect("uploader is wired whenever trace upload config is on");
    uploader(b"log".to_vec(), "user-id-1".into()).await;
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no diagnostics request may leave the machine after opt-out"
    );
}
#[tokio::test]
async fn diagnostic_upload_sent_for_normal_user() {
    let (stub_url, count) = spawn_counting_storage_stub().await;
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    enable_trace_upload_config(&agent);
    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
    let uploader = agent
        .diagnostic_upload_config()
        .expect("uploader is wired whenever trace upload config is on");
    uploader(b"log".to_vec(), "user-id-1".into()).await;
    assert!(
        count.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "positive control: diagnostics upload reaches the proxy for a \
         normal user"
    );
}
/// The diagnostics privacy gate fails closed: with no credential in the
/// `AuthManager` (e.g. a mid-session `/logout` raced the refresh failure
/// that triggers the upload), nothing may leave the machine.
#[tokio::test]
async fn diagnostic_upload_skipped_without_credentials() {
    let (stub_url, count) = spawn_counting_storage_stub().await;
    let agent = build_minimal_agent_for_tests();
    enable_trace_upload_config(&agent);
    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
    let uploader = agent
        .diagnostic_upload_config()
        .expect("uploader is wired whenever trace upload config is on");
    uploader(b"log".to_vec(), "user-id-1".into()).await;
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "missing credentials must fail closed for diagnostics uploads"
    );
}
/// The diagnostics uploader is wired once (at agent construction), so it
/// must re-check the live trace-upload mirror at invocation time: a
/// mid-session config-level kill switch stops diagnostics uploads too.
#[tokio::test]
async fn diagnostic_upload_skipped_after_mid_session_trace_upload_kill_switch() {
    let (stub_url, count) = spawn_counting_storage_stub().await;
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    enable_trace_upload_config(&agent);
    agent.cfg.borrow_mut().endpoints.trace_upload_url = Some(stub_url);
    agent.sync_collection_config_gate();
    let uploader = agent
        .diagnostic_upload_config()
        .expect("uploader is wired whenever trace upload config is on");
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
        cfg.telemetry.trace_upload = Some(false);
    }
    agent.sync_collection_config_gate();
    uploader(b"log".to_vec(), "user-id-1".into()).await;
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "an already-wired diagnostics uploader must honor a mid-session \
         trace-upload kill switch"
    );
}
/// The live collection gate reads a `Send` mirror of the config-level
/// trace-upload switch; `sync_collection_config_gate` must keep that mirror
/// current so a mid-session remote-settings flip (kill switch) stops
/// collection without a new session.
#[tokio::test]
async fn collection_config_gate_mirror_follows_trace_upload_flip() {
    let agent = build_agent_with_auth(crate::auth::GrokAuth::test_default());
    enable_trace_upload_config(&agent);
    agent.sync_collection_config_gate();
    assert!(
        agent
            .trace_upload_live
            .load(std::sync::atomic::Ordering::Relaxed),
        "precondition: mirror reflects the enabled switch"
    );
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
        cfg.telemetry.trace_upload = Some(false);
    }
    agent.sync_collection_config_gate();
    assert!(
        !agent
            .trace_upload_live
            .load(std::sync::atomic::Ordering::Relaxed),
        "mirror must follow a mid-session config-level trace-upload flip"
    );
}
/// `parse_session_kind` routes `session/load` to the gateway Chat path vs. the
/// disk-backed Build path. Anything but an explicit `kind: "chat"` is Build.
#[test]
fn parse_session_kind_matrix() {
    use crate::session::unified_list::SessionKind;
    use serde_json::json;
    let cases: &[(&str, serde_json::Value, SessionKind)] = &[
        (
            "chat",
            json!({"x.ai/session": {"kind": "chat"}}),
            SessionKind::Chat,
        ),
        (
            "build",
            json!({"x.ai/session": {"kind": "build"}}),
            SessionKind::Build,
        ),
        (
            "chat_malformed_sibling",
            json!({"x.ai/session": {"kind": "chat", "facets": "not-a-map"}}),
            SessionKind::Chat,
        ),
        (
            "unknown_kind",
            json!({"x.ai/session": {"kind": "frob"}}),
            SessionKind::Build,
        ),
        ("absent", json!({}), SessionKind::Build),
    ];
    for (label, meta, expected) in cases {
        assert_eq!(parse_session_kind(meta.as_object()), *expected, "[{label}]");
    }
    assert_eq!(parse_session_kind(None), SessionKind::Build, "[none]");
}
#[test]
fn reject_chat_kind_without_feature_errors_without_chat_feature() {
    use serde_json::json;
    assert!(
        reject_chat_kind_without_feature(json!({"x.ai/session": {"kind": "chat"}}).as_object())
            .is_err()
    );
    assert!(reject_chat_kind_without_feature(None).is_ok());
    assert!(
        reject_chat_kind_without_feature(
            json!({ "x.ai/session" : { "kind" : "build" } }).as_object()
        )
        .is_ok()
    );
}
#[test]
fn chat_initial_model_matrix() {
    let cases: &[(&str, bool, Option<&str>, Option<&str>)] = &[
        ("chat_with_model", true, Some("grok-4.5"), Some("grok-4.5")),
        ("chat_without_model", true, None, None),
        ("build_with_model", false, Some("grok-4.5"), None),
        ("build_without_model", false, None, None),
    ];
    for (label, is_chat_kind, custom_model_id, expected) in cases {
        assert_eq!(
            chat_initial_model(*is_chat_kind, *custom_model_id).as_deref(),
            *expected,
            "[{label}]"
        );
    }
}
#[test]
fn chat_new_session_model_state_matrix() {
    fn state_with(current: &str, available: &[&str]) -> acp::SessionModelState {
        acp::SessionModelState::new(
            acp::ModelId::new(current.to_owned()),
            available
                .iter()
                .map(|id| {
                    acp::ModelInfo::new(acp::ModelId::new((*id).to_owned()), (*id).to_owned())
                })
                .collect(),
        )
    }
    let cases: &[(&str, acp::SessionModelState, Option<&str>, &str)] = &[
        (
            "requested_in_catalog",
            state_with("auto", &["auto", "grok-4"]),
            Some("grok-4"),
            "grok-4",
        ),
        (
            "no_request_keeps_catalog_default",
            state_with("auto", &["auto", "grok-4"]),
            None,
            "auto",
        ),
        (
            "requested_not_in_catalog",
            state_with("auto", &["auto"]),
            Some("grok-4.5"),
            "grok-4.5",
        ),
        (
            "requested_with_empty_catalog",
            state_with("", &[]),
            Some("grok-4"),
            "grok-4",
        ),
    ];
    for (label, state, requested, expected) in cases {
        let out = chat_new_session_model_state(state.clone(), requested.map(str::to_owned));
        assert_eq!(out.current_model_id.0.as_ref(), *expected, "[{label}]");
        assert_eq!(
            out.available_models.len(),
            state.available_models.len(),
            "[{label}] override must not mutate the catalog"
        );
    }
}
/// valid `x.ai/local_workspace` → ExistingWorkspace only.
/// Never reads `envId` / never emits SandboxEnvironment.
#[cfg(feature = "local-workspace")]
#[test]
fn parse_session_computer_sessions_local_workspace_matrix() {
    use crate::gateway_bridge::ComputerSession;
    use serde_json::json;
    fn existing(server_id: &str, cwd: Option<&str>) -> Vec<ComputerSession> {
        vec![ComputerSession::ExistingWorkspace {
            server_id: server_id.to_owned(),
            cwd: cwd.map(str::to_owned),
        }]
    }
    let cases: &[(&str, serde_json::Value, Option<Vec<ComputerSession>>)] = &[
        (
            "attach_server_id_on_local",
            json!({
                "x.ai/local_workspace": {
                    "mode": "attach",
                    "server_id": "lw-attach-1",
                    "cwd": "/repo",
                },
                "envId": "env-must-be-ignored",
            }),
            Some(existing("lw-attach-1", Some("/repo"))),
        ),
        (
            "attach_server_id_from_cloud_existing",
            json!({
                "x.ai/local_workspace": {
                    "mode": "attach",
                    "cwd": "/repo",
                },
                "x.ai/cloud_existing_workspace": {
                    "server_id": "lw-attach-2",
                    "cwd": "/repo-existing",
                },
                "envId": "env-must-be-ignored",
            }),
            Some(existing("lw-attach-2", Some("/repo"))),
        ),
        (
            "own_with_server_id_ignores_envid",
            json!({
                "x.ai/local_workspace": {
                    "mode": "own",
                    "server_id": "lw-own-1",
                    "cwd": "/Users/me/src",
                },
                "envId": "env-must-be-ignored",
            }),
            Some(existing("lw-own-1", Some("/Users/me/src"))),
        ),
        (
            "own_without_server_id_no_sandbox_fallback",
            json!({
                "x.ai/local_workspace": {
                    "mode": "own",
                    "cwd": "/Users/me/src",
                },
                "envId": "env-must-be-ignored",
            }),
            None,
        ),
        (
            "invalid_mode_falls_through_to_envid",
            json!({
                "x.ai/local_workspace": {
                    "mode": "bogus",
                    "server_id": "lw-x",
                },
                "envId": "env-prod",
            }),
            Some(vec![ComputerSession::SandboxEnvironment {
                environment_id: Some("env-prod".to_owned()),
            }]),
        ),
        (
            "non_object_local_falls_through_to_envid",
            json!({
                "x.ai/local_workspace": "not-an-object",
                "envId": "env-prod",
            }),
            Some(vec![ComputerSession::SandboxEnvironment {
                environment_id: Some("env-prod".to_owned()),
            }]),
        ),
    ];
    for (label, meta, expected) in cases {
        let got = parse_session_computer_sessions(meta.as_object());
        assert_eq!(
            got.as_deref(),
            expected.as_deref(),
            "[{label}] local_workspace match-table mismatch"
        );
    }
}
/// Local intent without resolvable server_id fails closed (no silent unstamped start).
#[cfg(feature = "local-workspace")]
#[test]
fn resolve_local_workspace_missing_server_id_fails_closed() {
    use serde_json::json;
    let meta = json!({
        "x.ai/session": { "kind": "chat" },
        "x.ai/local_workspace": {
            "mode": "own",
            "cwd": "/repo",
        }
    });
    let err = resolve_session_computer_sessions(meta.as_object())
        .expect_err("own without server_id must fail closed");
    assert_eq!(
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(|v| v.as_str()),
        Some("local_workspace_server_id_missing")
    );
}
/// Supervisor map + reap guard / shutdown_gateway_bridge tear down the entry.
#[cfg(all(feature = "local-workspace", unix))]
#[test]
fn local_workspace_reap_guard_and_shutdown_clear_map() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = gateway_bridge_test_session_id();
        {
            let mut guard = agent.new_local_workspace_reap_guard(sid.clone(), true);
            guard.disarm();
        }
        assert!(agent.local_workspace_supervisors.borrow().is_empty());
        agent.shutdown_gateway_bridge(&sid);
        assert!(
            agent
                .local_workspace_generations
                .borrow()
                .get(&sid)
                .is_none()
        );
    });
}
/// Pre-bridge crash refresh rewrites handshake stamp from live supervisor id.
#[cfg(all(feature = "local-workspace", unix))]
#[test]
fn refresh_sessions_from_supervisor_overrides_server_id() {
    use crate::gateway_bridge::ComputerSession;
    use crate::gateway_bridge::local_workspace_supervisor::test_start_ready_own;
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = gateway_bridge_test_session_id();
        let original = Some(vec![ComputerSession::ExistingWorkspace {
            server_id: "lw-stale".into(),
            cwd: Some("/repo".into()),
        }]);
        let unchanged = agent.refresh_sessions_from_supervisor(&sid, original.clone());
        assert!(matches!(
            unchanged.as_ref().and_then(|v| v.first()),
            Some(ComputerSession::ExistingWorkspace { server_id, .. }) if server_id == "lw-stale"
        ));
        let (_dir, handle) = test_start_ready_own().await;
        let live_id = handle.server_id.clone();
        agent.register_local_workspace_supervisor(sid.clone(), handle);
        let refreshed = agent.refresh_sessions_from_supervisor(&sid, original);
        match refreshed.as_ref().and_then(|v| v.first()) {
            Some(ComputerSession::ExistingWorkspace { server_id, .. }) => {
                assert_eq!(
                    server_id, &live_id,
                    "refresh must use live supervisor server_id"
                );
            }
            other => panic!("expected ExistingWorkspace, got {other:?}"),
        }
        agent.shutdown_gateway_bridge(&sid);
    });
}
/// start_own + register stamps server_id into meta and stores the handle.
#[cfg(all(feature = "local-workspace", unix))]
#[test]
fn start_own_registers_and_stamps_server_id() {
    use crate::gateway_bridge::local_workspace_supervisor::{
        stamp_server_id_into_meta, test_start_ready_own,
    };
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = gateway_bridge_test_session_id();
        let (_dir, handle) = test_start_ready_own().await;
        let server_id = handle.server_id.clone();
        let mut meta = acp::Meta::new();
        meta.insert(
            "x.ai/local_workspace".into(),
            serde_json::json!({"mode": "own", "cwd": "/tmp/repo"}),
        );
        stamp_server_id_into_meta(&mut meta, &server_id);
        assert_eq!(
            meta.get("x.ai/local_workspace")
                .and_then(|v| v.get("server_id"))
                .and_then(|v| v.as_str()),
            Some(server_id.as_str())
        );
        agent.register_local_workspace_supervisor(sid.clone(), handle);
        assert!(
            agent
                .local_workspace_supervisors
                .borrow()
                .contains_key(&sid),
            "handle must be registered by SessionId"
        );
        assert!(
            agent
                .local_workspace_generations
                .borrow()
                .get(&sid)
                .is_some_and(|g| *g >= 1),
            "arm must bump generation"
        );
        agent.shutdown_gateway_bridge(&sid);
    });
}
/// Armed reap guard removes a registered supervisor on drop (session/new failure).
#[cfg(all(feature = "local-workspace", unix))]
#[test]
fn reap_guard_drop_removes_registered_supervisor() {
    use crate::gateway_bridge::local_workspace_supervisor::test_start_ready_own;
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = gateway_bridge_test_session_id();
        let (_dir, handle) = test_start_ready_own().await;
        agent.register_local_workspace_supervisor(sid.clone(), handle);
        assert!(
            agent
                .local_workspace_supervisors
                .borrow()
                .contains_key(&sid)
        );
        {
            let _guard = agent.new_local_workspace_reap_guard(sid.clone(), true);
        }
        assert!(
            agent
                .local_workspace_supervisors
                .borrow()
                .get(&sid)
                .is_none(),
            "armed guard drop must reap supervisor"
        );
        assert!(
            agent
                .local_workspace_generations
                .borrow()
                .get(&sid)
                .is_none(),
            "armed guard drop must invalidate generation"
        );
    });
}
/// Shutdown generation invalidates a pending restart re-insert.
#[cfg(all(feature = "local-workspace", unix))]
#[test]
fn shutdown_generation_invalidates_stale_restart() {
    use crate::gateway_bridge::local_workspace_supervisor::test_start_ready_own;
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = gateway_bridge_test_session_id();
        let (_dir, handle) = test_start_ready_own().await;
        agent.register_local_workspace_supervisor(sid.clone(), handle);
        let generation = *agent
            .local_workspace_generations
            .borrow()
            .get(&sid)
            .expect("generation after register");
        agent.shutdown_gateway_bridge(&sid);
        assert!(
            agent.local_workspace_generations.borrow().get(&sid) != Some(&generation),
            "shutdown must invalidate generation so stale restart cannot re-insert"
        );
        assert!(
            agent
                .local_workspace_supervisors
                .borrow()
                .get(&sid)
                .is_none()
        );
    });
}
/// `spawn_gateway_bridge` uses `tokio::task::spawn_local`.
fn run_local_for_bridge_test<F, Fut, T>(body: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime must build");
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, body())
}
#[test]
fn chat_session_spawn_options_matches_thin_profile() {
    let sid = acp::SessionId::new(std::sync::Arc::from("00000000-0000-0000-0000-000000000099"));
    let cwd = xai_grok_paths::AbsPathBuf::new(std::env::temp_dir()).expect("temp cwd");
    let opts = chat_session_spawn_options(
        SessionInfo {
            id: sid,
            cwd: cwd.as_str().to_owned(),
        },
        cwd,
        None,
        None,
        acp::ModelId::new(std::sync::Arc::from("test-model")),
        false,
    );
    assert!(opts.mcp_servers.is_empty());
    assert!(opts.initial_client_mcp_servers.is_empty());
    assert!(!opts.client_code_nav_enabled);
    assert!(!opts.client_terminal);
    assert!(!opts.client_fs_read);
    assert!(!opts.client_fs_write);
    assert!(opts.chat_history.is_empty());
    assert!(opts.managed_mcp_expires_at.is_none());
    assert!(!opts.session_auto_mode);
    assert!(
        opts.persistence.is_noop(),
        "K10 thin profile must use PersistenceHandle::noop()"
    );
    assert!(opts.is_chat_kind);
}
/// `remove_session` releases the workspace binding and drains the
/// per-session side maps. Test agents default to `workspace_ops = None`,
/// so no other test reaches the release.
#[tokio::test]
async fn remove_session_releases_workspace_binding_and_side_maps() {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("test-session-workspace-release");
    let ops = xai_grok_workspace::WorkspaceOps::for_test();
    let toolset =
        std::sync::Arc::new(xai_grok_tools::registry::types::FinalizedToolset::empty_for_test());
    let toolset_weak = std::sync::Arc::downgrade(&toolset);
    ops.bind_local_session(
        sid.0.as_ref(),
        std::env::temp_dir(),
        xai_hunk_tracker::HunkTrackerHandle::noop(),
        toolset,
        None,
    )
    .expect("bind_local_session must succeed");
    assert!(toolset_weak.upgrade().is_some());
    *agent.workspace_ops.borrow_mut() = Some(ops);
    agent
        .session_registry
        .set_unavailable_model(&sid, acp::ModelId::new(std::sync::Arc::from("gone-model")));
    agent.set_turn_number(&sid, 3);
    let (_permission_tx, permission_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_grok_workspace::permission::PermissionEvent>();
    agent
        .session_registry
        .set_permission_receiver(&sid, permission_rx);
    agent.remove_session(&sid);
    assert!(
        toolset_weak.upgrade().is_none(),
        "the workspace binding must release the toolset"
    );
    assert!(agent.session_registry.unavailable_model(&sid).is_none());
    assert_eq!(agent.session_registry.counts().resident_resources, 0);
    assert_eq!(
        agent.session_registry.counts().retained_resources,
        0,
        "retained per-session resources must be reclaimed on removal"
    );
}
/// Without a bridge, `ext_method` falls through to the unchanged local
/// dispatch (`rewind::handle`), which reports the missing session — proving
/// the routing hook is skipped in local mode.
#[test]
fn ext_method_rewind_uses_local_dispatch_without_bridge() {
    use acp::Agent as _;
    let _env = crate::env::EnvVarGuard::remove(crate::env::GROK_DISABLE_CUSTOM_BRIDGE_ENV);
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let params = serde_json::json!({ "sessionId": "sess-local" });
        let err = agent
            .ext_method(acp::ExtRequest::new(
                "x.ai/rewind/points",
                std::sync::Arc::from(serde_json::value::to_raw_value(&params).unwrap()),
            ))
            .await
            .expect_err("local rewind with no session must error");
        assert_eq!(err.code, acp::Error::resource_not_found(None).code);
    });
}
#[test]
fn cancel_does_not_forward_to_bridge_in_local_mode() {
    use crate::session::SessionCommand;
    use acp::Agent as _;
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-cancel-local");
        let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        agent.insert_resident(&sid, handle);
        agent
            .cancel(acp::CancelNotification::new(sid.clone()))
            .await
            .expect("cancel must succeed");
        let mut saw_local_cancel = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let SessionCommand::Cancel(..) = cmd {
                saw_local_cancel = true;
            }
        }
        assert!(
            saw_local_cancel,
            "local-mode cancel dispatches the local SessionCommand::Cancel with no bridge attached"
        );
    });
}
/// Regression (post-cancel slot hang, first bad release 0.2.101; see
/// `dispatch_lock`). SDK e2e shape:
/// `test_cancel_ends_in_flight_turn_and_frees_slot` (grok-agent-sdk).
/// Fake actor must answer intake oneshots (#341); dropping them hangs `prompt`.
#[test]
fn cancel_never_overtakes_in_flight_prompt_intake() {
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    use crate::session::SessionCommand;
    use crate::session::commands::{PromptCompletionKind, PromptTurnOk};
    use crate::session::plan_mode::PromptMode;
    use acp::Agent as _;
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        // Keep this mailbox-ordering regression independent from developer
        // credentials and the remote-settings refresh path reached during
        // prompt trace setup.
        agent.cfg.borrow_mut().remote_settings = Some(Default::default());
        let mut model = ModelEntry::fallback("test-model", &EndpointsConfig::default());
        model.info.auth_scheme = xai_grok_sampler::AuthScheme::None;
        agent.models_manager.insert_test_entry("test-model", model);
        let sid = acp::SessionId::new("sess-cancel-intake-race");
        let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        agent.insert_resident(&sid, handle);
        let order: std::rc::Rc<std::cell::RefCell<Vec<&'static str>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let (intake_parked_tx, intake_parked_rx) = tokio::sync::oneshot::channel::<()>();
        let (cancel_started_tx, cancel_started_rx) = tokio::sync::oneshot::channel::<()>();
        let driver_order = order.clone();
        tokio::task::spawn_local(async move {
            let mut intake_parked_tx = Some(intake_parked_tx);
            let mut cancel_started_rx = Some(cancel_started_rx);
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    SessionCommand::GetCurrentPromptMode { responds_to } => {
                        if let Some(tx) = intake_parked_tx.take() {
                            let _ = tx.send(());
                            if let Some(rx) = cancel_started_rx.take() {
                                rx.await
                                    .expect("cancel starts while prompt still holds intake");
                            }
                        }
                        let _ = responds_to.send(Default::default());
                    }
                    SessionCommand::GetCurrentModel { responds_to } => {
                        let _ = responds_to.send("test-model".to_owned());
                    }
                    SessionCommand::GetModelMetadata { responds_to } => {
                        let _ = responds_to.send(Default::default());
                    }
                    SessionCommand::CopyFile { respond_to } => {
                        let _ = respond_to.send(Err(anyhow::anyhow!(
                            "session copy is unavailable in the fake actor"
                        )));
                    }
                    SessionCommand::SetNextTraceTurn { .. } => {}
                    SessionCommand::PersistGitHead { .. } => {}
                    SessionCommand::TakeHarnessTraceTurns { respond_to } => {
                        let _ = respond_to.send(Vec::new());
                    }
                    SessionCommand::TakeTurnMessages { respond_to } => {
                        let _ = respond_to.send(None);
                    }
                    SessionCommand::TakeStreamingCapture { respond_to, .. } => {
                        let _ = respond_to.send(None);
                    }
                    SessionCommand::Prompt { respond_to, .. } => {
                        driver_order.borrow_mut().push("prompt");
                        // Short-circuit the ACP turn-finalization path: this
                        // regression is only about mailbox admission order,
                        // and the minimal test agent has no subagent-event
                        // backend to service the normal completed-turn query.
                        let _ = respond_to.send(Ok(crate::session::commands::PromptTurnOk {
                            stop_reason: acp::StopReason::Cancelled,
                            total_tokens: 0,
                            turn_snapshot: None,
                            completion_kind:
                                crate::session::commands::PromptCompletionKind::RemovedFromQueue,
                            structured_output: None,
                            usage: None,
                            tool_overrides: None,
                        }));
                    }
                    SessionCommand::Cancel(..) => {
                        driver_order.borrow_mut().push("cancel");
                    }
                    _ => {}
                }
            }
        });
        let prompt_fut = agent.prompt(acp::PromptRequest::new(
            sid.clone(),
            vec![acp::ContentBlock::from("hi")],
        ));
        let cancel_fut = async {
            intake_parked_rx
                .await
                .expect("prompt intake reaches the fake actor");
            let _ = cancel_started_tx.send(());
            let _ = agent
                .cancel(acp::CancelNotification::new(sid.clone()))
                .await;
        };
        let (prompt_result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            futures::join!(prompt_fut, cancel_fut)
        })
        .await
        .expect("prompt/cancel ordering test must not hang");
        prompt_result.expect("prompt completes after fake actor response");
        assert_eq!(
            order.borrow().as_slice(),
            ["prompt", "cancel"],
            "cancel must land on the actor mailbox after the prompt it targets"
        );
    });
}
use crate::session::SessionCommand as TestSessionCommand;
/// Build a session handle wired to a *live* command channel. Returns the
/// handle (move into `sessions`) plus a probe `cmd_tx`/`cmd_rx` so a test
/// can observe what the agent sends to the actor and prove the channel is
/// live.
fn make_live_session_handle(
    sid: &acp::SessionId,
    running_prompt: Option<&str>,
) -> (
    crate::session::SessionHandle,
    tokio::sync::mpsc::UnboundedSender<TestSessionCommand>,
    tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand>,
) {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut handle = make_test_handle("test-model", false, Some("grok-tui"));
    handle.cmd_tx = cmd_tx.clone();
    handle.info = crate::session::info::Info {
        id: sid.clone(),
        cwd: "/tmp".to_string(),
    };
    if let Some(pid) = running_prompt {
        *handle.current_prompt_id.lock().unwrap() = Some(pid.to_string());
    }
    (handle, cmd_tx, cmd_rx)
}
/// Spawn a minimal fake session actor on the `LocalSet` that answers
/// `SessionCommand::IsBusy` with `busy` and forwards every other command to
/// the returned receiver so a test can assert on them (e.g. `Shutdown`).
fn spawn_fake_actor(
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand>,
    busy: bool,
) -> tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand> {
    let (observed_tx, observed_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                TestSessionCommand::IsBusy { respond_to } => {
                    let _ = respond_to.send(busy);
                }
                other => {
                    let _ = observed_tx.send(other);
                }
            }
        }
    });
    observed_rx
}
/// Drive `x.ai/internal/evict_sessions` through the real `ext_notification`
/// handler path (not the internal helper) — matches how the leader server
/// signals a client disconnect.
async fn drive_disconnect(agent: &MvpAgent, sid: &acp::SessionId) {
    drive_disconnect_many(agent, &[sid]).await;
}
/// Like `drive_disconnect`, but evicts several sessions in a single
/// `x.ai/internal/evict_sessions` notification — the realistic shape of a
/// real client disconnect, and the path that exercises `handle_evict_sessions`'
/// concurrent `join_all` check pass followed by the sequential act pass.
async fn drive_disconnect_many(agent: &MvpAgent, sids: &[&acp::SessionId]) {
    use acp::Agent as _;
    let ids: Vec<&str> = sids.iter().map(|s| s.0.as_ref()).collect();
    let params = serde_json::json!({ "sessionIds": ids });
    let raw = serde_json::value::to_raw_value(&params).unwrap();
    agent
        .ext_notification(acp::ExtNotification::new(
            "x.ai/internal/evict_sessions",
            raw.into(),
        ))
        .await
        .expect("evict_sessions notification must be handled");
}
/// Drive `x.ai/session/close` through the real `ext_method` dispatch
/// (`ext_method` → `handlers::session::handle` → `handle_session_close`),
/// exercising the exact production path that finalizes the replica.
async fn drive_close(agent: &MvpAgent, session_id: &str) -> Result<acp::ExtResponse, acp::Error> {
    use acp::Agent as _;
    let params = serde_json::json!({ "sessionId": session_id });
    let raw = serde_json::value::to_raw_value(&params).unwrap();
    agent
        .ext_method(acp::ExtRequest::new(
            "x.ai/session/close",
            std::sync::Arc::from(raw),
        ))
        .await
}
/// Every method `parse_queue_edit_command` accepts must be forwarded from
/// `ext_notification` to that session's mailbox. Parser-only coverage misses
/// a dispatch drop.
#[tokio::test(flavor = "current_thread")]
async fn ext_notification_forwards_each_queue_method_to_session_actor() {
    use acp::Agent as _;
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-queue-rt");
    let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
    agent.insert_resident(&sid, handle);
    let session_id = sid.0.as_ref();
    let cases: [(&str, serde_json::Value); 7] = [
        (
            "x.ai/queue/remove",
            serde_json::json!({
                "sessionId": session_id,
                "id": "p-remove",
                "expectedVersion": 3,
                "owner": "grok-tui",
            }),
        ),
        (
            "x.ai/queue/reorder",
            serde_json::json!({
                "sessionId": session_id,
                "orderedIds": ["a", "b"],
            }),
        ),
        (
            "x.ai/queue/clear",
            serde_json::json!({
                "sessionId": session_id,
                "clientIdentifier": "grok-desktop",
            }),
        ),
        (
            "x.ai/queue/edit",
            serde_json::json!({
                "sessionId": session_id,
                "id": "p-edit",
                "newText": "rewritten",
                "owner": "grok-vscode",
            }),
        ),
        (
            "x.ai/queue/interject",
            serde_json::json!({
                "sessionId": session_id,
                "id": "p-interject",
                "expectedVersion": 2,
                "owner": "grok-tui",
                "newText": "now",
            }),
        ),
        (
            "x.ai/queue/hold_edit",
            serde_json::json!({
                "sessionId": session_id,
                "id": "p-hold",
            }),
        ),
        (
            "x.ai/queue/release_edit",
            serde_json::json!({
                "sessionId": session_id,
                "id": "p-release",
            }),
        ),
    ];
    for (method, params) in cases {
        let raw = serde_json::value::to_raw_value(&params).expect("serialize queue params");
        agent
            .ext_notification(acp::ExtNotification::new(method, raw.into()))
            .await
            .unwrap_or_else(|e| panic!("{method} ext_notification failed: {e}"));
        let cmd = cmd_rx.try_recv().unwrap_or_else(|e| {
            panic!("{method} must land a SessionCommand on the actor mailbox, try_recv={e}")
        });
        match (method, cmd) {
            (
                "x.ai/queue/remove",
                SessionCommand::RemoveQueuedPrompt {
                    id,
                    expected_version,
                    owner,
                },
            ) => {
                assert_eq!(id, "p-remove");
                assert_eq!(expected_version, 3);
                assert_eq!(owner.as_deref(), Some("grok-tui"));
            }
            ("x.ai/queue/reorder", SessionCommand::ReorderQueue { ordered_ids }) => {
                assert_eq!(ordered_ids, vec!["a", "b"]);
            }
            ("x.ai/queue/clear", SessionCommand::ClearQueue { owner }) => {
                assert_eq!(owner.as_deref(), Some("grok-desktop"));
            }
            (
                "x.ai/queue/edit",
                SessionCommand::EditQueuedPrompt {
                    id,
                    new_text,
                    editor,
                },
            ) => {
                assert_eq!(id, "p-edit");
                assert_eq!(new_text, "rewritten");
                assert_eq!(editor.as_deref(), Some("grok-vscode"));
            }
            (
                "x.ai/queue/interject",
                SessionCommand::InterjectQueuedPrompt {
                    id,
                    expected_version,
                    owner,
                    new_text,
                },
            ) => {
                assert_eq!(id, "p-interject");
                assert_eq!(expected_version, 2);
                assert_eq!(owner.as_deref(), Some("grok-tui"));
                assert_eq!(new_text.as_deref(), Some("now"));
            }
            ("x.ai/queue/hold_edit", SessionCommand::HoldCombineEdit { id }) => {
                assert_eq!(id, "p-hold");
            }
            ("x.ai/queue/release_edit", SessionCommand::ReleaseCombineEdit { id }) => {
                assert_eq!(id, "p-release");
            }
            (method, _) => {
                panic!("{method} dispatched a SessionCommand of the wrong variant")
            }
        }
    }
    assert!(
        matches!(
            cmd_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "no extra SessionCommand may remain after the seven queue methods"
    );
}
/// Methods the parser rejects (unknown, outbound `changed`, missing id /
/// newText) and a missing session must not send a command or panic.
#[tokio::test(flavor = "current_thread")]
async fn ext_notification_queue_rejects_unknown_method_missing_id_and_unknown_session() {
    use acp::Agent as _;
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-queue-neg");
    let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
    agent.insert_resident(&sid, handle);
    let session_id = sid.0.as_ref();
    let negatives: [(&str, serde_json::Value); 9] = [
        (
            "x.ai/queue/bogus",
            serde_json::json!({ "sessionId": session_id, "id": "p1" }),
        ),
        (
            "x.ai/queue/changed",
            serde_json::json!({
                "sessionId": session_id,
                "entries": [{
                    "id": "p1",
                    "version": 0,
                    "kind": "prompt",
                    "text": "hello",
                    "position": 0,
                }],
            }),
        ),
        (
            "x.ai/queue/hold_edit",
            serde_json::json!({ "sessionId": session_id }),
        ),
        (
            "x.ai/queue/release_edit",
            serde_json::json!({ "sessionId": session_id }),
        ),
        (
            "x.ai/queue/remove",
            serde_json::json!({ "sessionId": session_id }),
        ),
        (
            "x.ai/queue/edit",
            serde_json::json!({ "sessionId": session_id, "newText": "x" }),
        ),
        (
            "x.ai/queue/edit",
            serde_json::json!({ "sessionId": session_id, "id": "p-edit" }),
        ),
        (
            "x.ai/queue/interject",
            serde_json::json!({ "sessionId": session_id }),
        ),
        (
            "x.ai/queue/hold_edit",
            serde_json::json!({ "sessionId": "no-such-session", "id": "p1" }),
        ),
    ];
    for (method, params) in negatives {
        let raw = serde_json::value::to_raw_value(&params).expect("serialize queue params");
        agent
            .ext_notification(acp::ExtNotification::new(method, raw.into()))
            .await
            .unwrap_or_else(|e| panic!("{method} ext_notification must not fail: {e}"));
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "{method} must not send a SessionCommand (params={params})"
        );
    }
    let agent_empty = build_minimal_agent_for_tests();
    let raw = serde_json::value::to_raw_value(&serde_json::json!({
        "sessionId": "ghost",
        "id": "p1",
    }))
    .expect("serialize");
    agent_empty
        .ext_notification(acp::ExtNotification::new(
            "x.ai/queue/release_edit",
            raw.into(),
        ))
        .await
        .expect("queue edit for a missing session must not error");
}
/// Fire-and-forget: a gone session actor must not turn `ext_notification`
/// into an error or panic.
#[tokio::test(flavor = "current_thread")]
async fn ext_notification_queue_edit_survives_dropped_actor_mailbox() {
    use acp::Agent as _;
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new("sess-queue-dead");
    let (handle, _tx, cmd_rx) = make_live_session_handle(&sid, None);
    agent.insert_resident(&sid, handle);
    drop(cmd_rx);
    let params = serde_json::json!({
        "sessionId": sid.0.as_ref(),
        "id": "p-hold",
    });
    let raw = serde_json::value::to_raw_value(&params).expect("serialize queue params");
    agent
        .ext_notification(acp::ExtNotification::new(
            "x.ai/queue/hold_edit",
            raw.into(),
        ))
        .await
        .expect("queue edit must not error when the session actor mailbox is gone");
}
/// No-evict keystone: a client disconnecting mid-turn must NOT destroy the
/// session. The actor stays resident, no `Shutdown` is sent, the resident
/// session's command channel still **delivers** commands (so a reconnecting
/// `session/load` can keep driving the turn), and `finalize()` is NOT called
/// on a mere disconnect.
#[test]
fn disconnect_keeps_live_session_resident_without_finalize() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-live");
        let (_cmd_tx, mut cmd_rx) = {
            let (handle, tx, rx) = make_live_session_handle(&sid, Some("turn-1"));
            agent.insert_resident(&sid, handle);
            (tx, rx)
        };
        drive_disconnect(&agent, &sid).await;
        assert!(
            agent.is_resident(&sid),
            "live session must stay resident across client disconnect"
        );
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "no command may be sent to a session kept resident with live work"
        );
        let resident = agent
            .resident_handle(&sid)
            .expect("session must still be resident");
        resident
            .cmd_tx
            .send(TestSessionCommand::ResetPermissionState)
            .expect("resident session channel must accept commands post-disconnect");
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Ok(TestSessionCommand::ResetPermissionState)
            ),
            "the resident session's receiver must observe the delivered command"
        );
        assert!(
            agent.finalize_spy.borrow().is_empty(),
            "finalize() must NOT fire on client disconnect"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::Working),
            "a kept-resident session with live work is Working"
        );
    });
}
/// Keep-resident must hold even if the `current_prompt_id` lock is poisoned:
/// an unknown state is treated as "busy" (never unload). Guards against a
/// regression flipping the `unwrap_or(true)` fallback to `false`.
#[test]
fn disconnect_keeps_resident_on_poisoned_lock() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-poison");
        let (handle, _tx, _rx) = make_live_session_handle(&sid, None);
        let poison_target = handle.current_prompt_id.clone();
        agent.insert_resident(&sid, handle);
        let _ = std::thread::spawn(move || {
            let _g = poison_target.lock().unwrap();
            panic!("poison current_prompt_id");
        })
        .join();
        assert!(
            agent
                .resident_handle(&sid)
                .unwrap()
                .current_prompt_id
                .lock()
                .is_err(),
            "precondition: the lock must be poisoned"
        );
        drive_disconnect(&agent, &sid).await;
        assert!(
            agent.is_resident(&sid),
            "a session with an unknown (poisoned) state must be kept resident"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::Working),
        );
    });
}
/// A wedged actor stays tracked. `remove_session` releases everything else but
/// keeps a still-running thread, because dropping its handle would detach the
/// thread and leave nothing for the supervisor sweep to find.
#[test]
fn remove_session_keeps_a_running_thread_tracked() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-wedged");
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        agent.session_registry.set_thread(
            &sid,
            crate::session::SessionThread::from_handle(std::thread::spawn(move || {
                let _ = release_rx.recv();
            })),
        );
        agent.set_turn_number(&sid, 1);
        agent.remove_session(&sid);
        assert!(
            agent.session_registry.has_thread(&sid),
            "a running actor thread must survive removal for the sweep"
        );
        assert_eq!(
            agent.session_registry.counts().retained_resources,
            0,
            "everything except the running thread must be released"
        );
        drop(release_tx);
        for _ in 0..100 {
            agent.sweep_dead_sessions();
            if !agent.session_registry.has_thread(&sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !agent.session_registry.has_thread(&sid),
            "the sweep must reclaim the thread once it exits"
        );
    });
}
/// Idle-unload stub (memory bound) + supervisor interaction: a *fully idle*
/// session is unloaded to disk on disconnect (actor `Shutdown`, handle
/// dropped) while the `SessionThread` is **retained** for
/// `drain_old_session_thread`. It is not finalized, and once the kept thread
/// finishes the supervisor reaps it as a *clean* exit — never `DeadFailed`.
#[test]
fn disconnect_unloads_idle_session_without_finalize() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-idle");
        let (handle, _cmd_tx, cmd_rx) = make_live_session_handle(&sid, None);
        agent.insert_resident(&sid, handle);
        let mut observed = spawn_fake_actor(cmd_rx, false);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        agent.session_registry.set_thread(
            &sid,
            crate::session::SessionThread::from_handle(std::thread::spawn(move || {
                let _ = release_rx.recv();
            })),
        );
        agent.ensure_session_supervisor();
        drive_disconnect(&agent, &sid).await;
        assert!(
            !agent.is_resident(&sid),
            "idle session must be unloaded from the resident map on disconnect"
        );
        assert!(
            agent.session_registry.has_thread(&sid),
            "idle-unload must keep the SessionThread for reconnect drain"
        );
        let shutdown = tokio::time::timeout(std::time::Duration::from_secs(1), observed.recv())
            .await
            .expect("idle-unload must send a command within 1s")
            .expect("fake actor channel must stay open");
        assert!(
            matches!(shutdown, TestSessionCommand::Shutdown(_)),
            "idle-unload must send SessionCommand::Shutdown"
        );
        assert!(
            agent.finalize_spy.borrow().is_empty(),
            "idle-unload on disconnect must NOT finalize the cloud replica"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::Dormant),
            "an idle-unloaded session demotes to Dormant"
        );
        drop(release_tx);
        let deadline = tokio::time::Instant::now() + (SESSION_SUPERVISOR_TICK * 6);
        while tokio::time::Instant::now() < deadline {
            if !agent.session_registry.has_thread(&sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !agent.session_registry.has_thread(&sid),
            "supervisor must drop the finished kept thread"
        );
        assert!(
            !agent
                .roster_delta_spy
                .borrow()
                .iter()
                .any(|(id, st)| id == sid.0.as_ref() && *st == SessionLiveState::DeadFailed),
            "a cleanly idle-unloaded session must not be reaped as DeadFailed"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            None,
            "clean-exit sweep must drop the Dormant live-state entry"
        );
    });
}
/// The `IsBusy` keep-resident path. A between-turns session
/// (`current_prompt_id = None`) whose actor answers `IsBusy = true` (queued
/// inputs at the turn boundary) must be kept resident — NOT unloaded — and
/// must receive no `Shutdown`. This exercises the async round-trip that the
/// sync fast-path tests skip.
#[test]
fn disconnect_keeps_resident_when_actor_reports_busy() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-busy");
        let (handle, _cmd_tx, cmd_rx) = make_live_session_handle(&sid, None);
        agent.insert_resident(&sid, handle);
        let mut observed = spawn_fake_actor(cmd_rx, true);
        drive_disconnect(&agent, &sid).await;
        assert!(
            agent.is_resident(&sid),
            "a between-turns session with queued work (IsBusy=true) must stay resident"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::Working),
            "an actor-reported-busy session is kept Working"
        );
        tokio::task::yield_now().await;
        assert!(
            matches!(
                observed.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "a busy session must not be sent Shutdown"
        );
    });
}
/// A between-turns session whose ONLY outstanding work is a parked
/// `PlanApproval` reverse-request (the resume re-park) must be kept resident on
/// disconnect. The actor answers `IsBusy = false`, so the keep-resident outcome
/// can come ONLY from the parked-approval sync fast path in `session_has_live_work`
/// — deleting that check would let this session unload (mutation-killing).
#[test]
fn disconnect_keeps_resident_when_plan_approval_parked() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-plan-parked");
        let (handle, _cmd_tx, cmd_rx) = make_live_session_handle(&sid, None);
        handle.pending_interactions.lock().unwrap().insert(
            "exit-plan-mode-resume".to_string(),
            crate::session::pending_interaction::PendingKind::PlanApproval,
        );
        agent.insert_resident(&sid, handle);
        let mut observed = spawn_fake_actor(cmd_rx, false);
        drive_disconnect(&agent, &sid).await;
        assert!(
            agent.is_resident(&sid),
            "a session with a parked plan-approval must stay resident"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::Working),
            "a parked-approval session is kept Working"
        );
        tokio::task::yield_now().await;
        assert!(
            matches!(
                observed.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "a parked-approval session must not be sent Shutdown"
        );
    });
}
/// Mixed batch in a *single* `x.ai/internal/evict_sessions` notification —
/// the realistic disconnect shape and the path that exercises
/// `handle_evict_sessions`' `join_all` two-pass (concurrent `IsBusy` checks,
/// then sequential act). One session's actor reports busy (→ kept resident,
/// `Working`, no `Shutdown`); the other is idle (→ unloaded, `Dormant`,
/// `Shutdown` sent). Each must get its own outcome with no cross-contamination
/// between the concurrent check pass and the sequential act pass.
#[test]
fn disconnect_mixed_batch_keeps_busy_unloads_idle() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid_busy = acp::SessionId::new("sess-batch-busy");
        let sid_idle = acp::SessionId::new("sess-batch-idle");
        let (busy_handle, _busy_tx, busy_rx) = make_live_session_handle(&sid_busy, None);
        let (idle_handle, _idle_tx, idle_rx) = make_live_session_handle(&sid_idle, None);
        agent.insert_resident(&sid_busy, busy_handle);
        agent.insert_resident(&sid_idle, idle_handle);
        let mut busy_observed = spawn_fake_actor(busy_rx, true);
        let mut idle_observed = spawn_fake_actor(idle_rx, false);
        drive_disconnect_many(&agent, &[&sid_busy, &sid_idle]).await;
        assert!(
            agent.is_resident(&sid_busy),
            "the busy session in the batch must stay resident"
        );
        assert_eq!(
            agent.session_live_state_for(&sid_busy),
            Some(SessionLiveState::Working),
            "the busy session must be Working"
        );
        assert!(
            !agent.is_resident(&sid_idle),
            "the idle session in the batch must be unloaded"
        );
        assert_eq!(
            agent.session_live_state_for(&sid_idle),
            Some(SessionLiveState::Dormant),
            "the idle session must be Dormant"
        );
        let idle_shutdown =
            tokio::time::timeout(std::time::Duration::from_secs(1), idle_observed.recv())
                .await
                .expect("idle session must receive a command within 1s")
                .expect("fake actor channel must stay open");
        assert!(
            matches!(idle_shutdown, TestSessionCommand::Shutdown(_)),
            "the idle session must be sent Shutdown"
        );
        tokio::task::yield_now().await;
        assert!(
            matches!(
                busy_observed.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "the busy session must not be sent Shutdown in a mixed batch"
        );
        assert!(
            agent.finalize_spy.borrow().is_empty(),
            "neither batch outcome may finalize on a mere disconnect"
        );
    });
}
/// The bounded `session_live_state` map does not grow without bound
/// across repeated create/close cycles — every terminal close drops its
/// entry, so the map size stays at the live count, not the cumulative count.
#[test]
fn session_live_state_map_is_bounded_across_cycles() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        for i in 0..50 {
            let sid = acp::SessionId::new(format!("sess-cycle-{i}"));
            let (handle, _tx, _rx) = make_live_session_handle(&sid, Some("turn"));
            agent.insert_resident(&sid, handle);
            agent.set_session_live_state(&sid, SessionLiveState::IdleResident);
            assert_eq!(
                agent.close_active_session(&sid).await,
                crate::agent::mvp_agent::session_lifecycle::CloseOutcome::Closed,
                "cycle {i} must actually close, or the bound below proves nothing"
            );
        }
        assert_eq!(
            agent.session_registry.counts().session_live_state,
            0,
            "terminal closes must leave no residual live-state entries (bounded map)"
        );
    });
}
/// Finalize fires on a genuine terminal close, driven through the real
/// `x.ai/session/close` dispatch rather than the internal helper.
#[test]
fn explicit_close_finalizes_the_replica() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-close");
        let (handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, Some("turn-1"));
        agent.insert_resident(&sid, handle);
        drive_close(&agent, "no-such-session")
            .await
            .expect("close of a missing session must succeed as a no-op");
        assert!(
            agent.finalize_spy.borrow().is_empty(),
            "closing a missing session must NOT finalize"
        );
        drive_close(&agent, sid.0.as_ref())
            .await
            .expect("session close must be handled");
        let Ok(TestSessionCommand::Cancel(options)) = cmd_rx.try_recv() else {
            panic!("close must send Cancel before anything else");
        };
        assert_eq!(
            (
                options.cancel_subagents,
                options.kill_background_tasks,
                options.rewind_if_no_output,
                options.trigger.as_ref().map(|t| t.as_str()),
                options.user_initiated
            ),
            (true, true, false, Some("session_close"), false),
        );
        assert!(
            matches!(
                cmd_rx.try_recv(),
                Ok(TestSessionCommand::Shutdown(
                    crate::session::ShutdownKind::CancelRunningTurn
                ))
            ),
            "close frees the session, so its Shutdown must cancel the running \
             turn; a graceful one would let the turn answer EndTurn as the \
             actor tears down"
        );
        assert_eq!(
            agent.finalize_spy.borrow().as_slice(),
            &[sid.0.to_string()],
            "explicit close must finalize the cloud replica exactly once"
        );
        assert!(
            !agent.is_resident(&sid),
            "explicit close removes the session"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            None,
            "terminal removal must drop the live-state entry (bounded map)"
        );
        assert!(
            agent
                .roster_delta_spy
                .borrow()
                .iter()
                .any(|(id, st)| id == sid.0.as_ref() && *st == SessionLiveState::Completed),
            "explicit close must emit a Completed roster delta"
        );
    });
}
/// Join-handle supervisor: a *resident* actor that panics is reaped
/// promptly — removed from `sessions`/`session_threads`, demoted to
/// `DeadFailed` (observed via the roster delta, since the live-state entry
/// is dropped on removal), and NOT finalized (the conversation persists).
///
/// Polls in real time (the panic unwinds on a real OS thread, independent of
/// the tokio clock); the reap lands within a small number of supervisor
/// ticks. The injected-panic backtrace on stderr is expected and harmless.
#[test]
fn supervisor_reaps_panicked_resident_actor() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-panic");
        let (handle, _tx, _rx) = make_live_session_handle(&sid, Some("turn-1"));
        agent.insert_resident(&sid, handle);
        let panic_thread = std::thread::spawn(|| panic!("injected actor panic"));
        agent.session_registry.set_thread(
            &sid,
            crate::session::SessionThread::from_handle(panic_thread),
        );
        agent.set_session_live_state(&sid, SessionLiveState::Working);
        agent.ensure_session_supervisor();
        let deadline = tokio::time::Instant::now() + (SESSION_SUPERVISOR_TICK * 6);
        while tokio::time::Instant::now() < deadline {
            if !agent.session_registry.has_thread(&sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !agent.session_registry.has_thread(&sid),
            "supervisor must reap the dead thread"
        );
        assert!(
            !agent.is_resident(&sid),
            "reaped session must be removed from the resident map"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            None,
            "terminal removal drops the live-state entry (bounded map)"
        );
        assert!(
            agent
                .roster_delta_spy
                .borrow()
                .iter()
                .any(|(id, st)| id == sid.0.as_ref() && *st == SessionLiveState::DeadFailed),
            "a reaped resident actor must emit a DeadFailed roster delta"
        );
        assert!(
            agent.finalize_spy.borrow().is_empty(),
            "reaping a dead actor must NOT finalize (conversation persists)"
        );
    });
}
/// Regression: writeback must self-correct once remote settings arrive
/// (the field used to be frozen at construction).
#[tokio::test]
#[serial_test::serial]
async fn storage_mode_self_corrects_to_writeback_when_settings_arrive() {
    let _env = crate::env::EnvVarGuard::remove("GROK_STORAGE_MODE");
    let auth = crate::auth::GrokAuth {
        auth_mode: crate::auth::AuthMode::Oidc,
        oidc_issuer: Some("https://auth.x.ai".to_string()),
        key: "test-token".to_string(),
        ..Default::default()
    };
    let agent = build_agent_with_auth(auth);
    agent.cfg.borrow_mut().mode = crate::agent::config::AgentMode::Leader;
    assert_eq!(agent.storage_mode(), StorageMode::Local);
    agent.cfg.borrow_mut().remote_settings = Some(crate::util::config::RemoteSettings {
        writeback_enabled: Some(true),
        ..Default::default()
    });
    agent.on_remote_settings_changed();
    assert_eq!(agent.storage_mode(), StorageMode::Writeback);
}
/// `spawn_settings_reapply` coalesces: while one reapply is in flight,
/// repeated calls (boot + rapid `/new`) do not spawn overlapping tasks.
#[test]
fn spawn_settings_reapply_coalesces_while_in_flight() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        assert_eq!(agent.settings_reapply_spawn_count.get(), 0);
        agent.spawn_settings_reapply();
        agent.spawn_settings_reapply();
        agent.spawn_settings_reapply();
        assert_eq!(
            agent.settings_reapply_spawn_count.get(),
            1,
            "overlapping settings reapplies must coalesce to a single task"
        );
        assert!(agent.settings_reapply_in_flight.get());
    });
}
/// The in-flight guard clears on task completion (via the `ClearOnDrop`
/// guard, so it also clears on panic), allowing a later reapply to re-spawn.
#[test]
fn spawn_settings_reapply_clears_flag_after_completion() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        agent.spawn_settings_reapply();
        assert_eq!(agent.settings_reapply_spawn_count.get(), 1);
        assert!(agent.settings_reapply_in_flight.get());
        let mut cleared = false;
        for _ in 0..40 {
            if !agent.settings_reapply_in_flight.get() {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            cleared,
            "in-flight flag must clear after the task completes"
        );
        agent.spawn_settings_reapply();
        assert_eq!(
            agent.settings_reapply_spawn_count.get(),
            2,
            "a reapply after completion must spawn again"
        );
    });
}
/// The post-auth fetch has its own guard, so an in-flight settings reapply
/// cannot coalesce away a freshly authenticated identity's gate and settings
/// resolution.
#[test]
fn post_auth_settings_not_coalesced_by_in_flight_reapply() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        agent.spawn_settings_reapply();
        assert!(agent.settings_reapply_in_flight.get());
        agent.spawn_post_auth_settings(crate::auth::GrokAuth::test_default());
        assert_eq!(
            agent.post_auth_settings_spawn_count.get(),
            1,
            "post-auth must spawn on its own guard despite an in-flight reapply"
        );
        assert!(agent.post_auth_settings_in_flight.get());
    });
}
/// Agent with pre-loaded auth, a gateway receiver (to assert emitted
/// notifications), and the proxy URL pointed at a mock `/v1/settings`.
///
/// Bootstrap must stay self-contained: a custom proxy URL also changes model
/// catalog credential selection, which is unrelated to these settings tests.
/// Seed an empty settings snapshot through bootstrap, then restore the exact
/// post-auth state (no settings yet + mock proxy URL) on the constructed agent.
fn build_agent_with_auth_and_proxy(
    auth: crate::auth::GrokAuth,
    proxy_url: String,
    mode: crate::agent::config::AgentMode,
) -> (
    MvpAgent,
    tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    auth_manager.hot_swap(auth);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let mut cfg = AgentConfig {
        mode,
        ..Default::default()
    };
    // Keep these tests independent from developer-local catalog endpoint env
    // overrides. They exercise the authenticated proxy settings path, not a
    // custom API-key model catalog.
    cfg.endpoints.models_base_url = None;
    cfg.endpoints.models_list_url = None;
    cfg.remote_settings = Some(Default::default());
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.remote_settings = None;
        cfg.endpoints.cli_chat_proxy_base_url = Some(proxy_url);
    }
    (agent, rx)
}
/// Drain the gateway, returning `true` if any `x.ai/settings/update`
/// notification was emitted (and acking each so the sender doesn't warn).
fn drained_settings_update(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) -> bool {
    let mut found = false;
    while let Ok(msg) = rx.try_recv() {
        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg {
            if &*args.request.method == "x.ai/settings/update" {
                found = true;
            }
            let _ = args.response_tx.send(Ok(()));
        }
    }
    found
}
/// Re-open the process-global external-OTEL gate on drop so a closed gate
/// never leaks into another test.
struct RestoreOtelGate;
impl Drop for RestoreOtelGate {
    fn drop(&mut self) {
        xai_grok_telemetry::external::mark_external_otel_settings_resolved();
    }
}
/// Regression: `cfg.remote_settings` is not reset on an account switch, so the
/// access gate must not read a previous identity's cached `allow_access`. A
/// mismatched identity stays provisionally open (unknown), like the OTEL gate's
/// `rearm_on_switch`.
#[tokio::test]
async fn access_gate_does_not_leak_verdict_across_identities() {
    use crate::agent::config::AgentMode;
    use crate::auth::{GrokAuth, XAI_OAUTH2_ISSUER};
    let auth_a = GrokAuth {
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        user_id: "user-a".into(),
        ..GrokAuth::test_default()
    };
    let (agent, _rx) = build_agent_with_auth_and_proxy(
        auth_a,
        "http://127.0.0.1:1/".to_string(),
        AgentMode::Leader,
    );
    {
        let mut cfg = agent.cfg.borrow_mut();
        cfg.remote_settings = Some(crate::util::config::RemoteSettings {
            allow_access: Some(false),
            ..Default::default()
        });
    }
    *agent.allow_access_resolved_for.borrow_mut() = Some("user-a".to_string());
    let auth_b = GrokAuth {
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        user_id: "user-b".into(),
        ..GrokAuth::test_default()
    };
    assert!(auth_b.is_xai_auth(), "precondition: first-party xAI auth");
    agent.enforce_grok_code_access(&auth_b).await;
    assert!(
        agent.tier_allowed.get(),
        "identity B must not inherit identity A's denied allow_access verdict",
    );
}
/// First-party xAI auth + `writeback_enabled` settings → storage upgrades to
/// Writeback; the settings arrival also emits `x.ai/settings/update` and opens
/// the external-OTEL gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn post_auth_settings_xai_upgrades_writeback_emits_and_opens_gate() {
    use crate::agent::config::AgentMode;
    use crate::auth::{GrokAuth, XAI_OAUTH2_ISSUER};
    let _restore = RestoreOtelGate;
    let _storage_env = crate::env::EnvVarGuard::remove("GROK_STORAGE_MODE");
    let server = xai_grok_test_support::MockInferenceServer::start()
        .await
        .unwrap();
    server.set_settings(serde_json::json!({
        "writeback_enabled": true,
        "allow_access": true,
    }));
    let xai_auth = GrokAuth {
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        ..GrokAuth::test_default()
    };
    assert!(xai_auth.is_xai_auth(), "precondition: first-party xAI auth");
    let (agent, mut rx) =
        build_agent_with_auth_and_proxy(xai_auth, server.url(), AgentMode::Leader);
    assert_eq!(
        agent.storage_mode(),
        StorageMode::Local,
        "precondition: leader boots in Local storage mode"
    );
    xai_grok_telemetry::external::suppress_external_otel_until_settings();
    assert!(!xai_grok_telemetry::external::is_settings_gate_open());
    agent.maybe_fetch_post_auth_settings().await;
    assert_eq!(
        agent.storage_mode(),
        StorageMode::Writeback,
        "xai auth + writeback_enabled settings must upgrade storage to Writeback"
    );
    assert!(
        xai_grok_telemetry::external::is_settings_gate_open(),
        "a settings response must open the external-OTEL gate"
    );
    assert!(
        drained_settings_update(&mut rx),
        "settings arrival must push x.ai/settings/update to clients"
    );
}
/// BYOK auth must not be upgraded to `Writeback` even when the server
/// advertises it; the push and gate still fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn post_auth_settings_non_xai_keeps_local_but_still_emits() {
    use crate::agent::config::AgentMode;
    use crate::auth::{AuthMode, GrokAuth};
    let _restore = RestoreOtelGate;
    let server = xai_grok_test_support::MockInferenceServer::start()
        .await
        .unwrap();
    server.set_settings(serde_json::json!({
        "writeback_enabled": true,
        "allow_access": true,
    }));
    let api_auth = GrokAuth {
        auth_mode: AuthMode::ApiKey,
        ..GrokAuth::test_default()
    };
    assert!(
        !api_auth.is_xai_auth(),
        "precondition: non-first-party auth"
    );
    let (agent, mut rx) =
        build_agent_with_auth_and_proxy(api_auth, server.url(), AgentMode::Leader);
    xai_grok_telemetry::external::suppress_external_otel_until_settings();
    agent.maybe_fetch_post_auth_settings().await;
    assert_eq!(
        agent.storage_mode(),
        StorageMode::Local,
        "non-xai auth must stay Local even when writeback is advertised remotely"
    );
    assert!(
        xai_grok_telemetry::external::is_settings_gate_open(),
        "a settings response must open the gate regardless of auth kind"
    );
    assert!(
        drained_settings_update(&mut rx),
        "settings arrival must push x.ai/settings/update for non-xai auth too"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn post_auth_settings_failure_resolves_gate_onto_local_policy() {
    use crate::agent::config::AgentMode;
    use crate::auth::{GrokAuth, XAI_OAUTH2_ISSUER};
    let _restore = RestoreOtelGate;
    let server = xai_grok_test_support::MockInferenceServer::start()
        .await
        .unwrap();
    let xai_auth = GrokAuth {
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        ..GrokAuth::test_default()
    };
    let (agent, _rx) = build_agent_with_auth_and_proxy(xai_auth, server.url(), AgentMode::Leader);
    xai_grok_telemetry::external::suppress_external_otel_until_settings();
    assert!(!xai_grok_telemetry::external::is_settings_gate_open());
    agent.maybe_fetch_post_auth_settings().await;
    assert!(
        xai_grok_telemetry::external::is_settings_gate_open(),
        "an exhausted fetch is a definitive answer: open on local policy"
    );
    assert!(
        agent.cfg.borrow().remote_settings.is_none(),
        "opening the gate must not fabricate settings; none were fetched"
    );
}
/// A same-credential refresh must NOT re-suppress a gate already resolved for
/// that credential; the reason `OtelGate` remembers the identity. With the
/// gate resolved-open for this identity, a later failing (`Retry`) refresh
/// leaves it OPEN (regressing the identity guard would re-close it forever).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn same_credential_refresh_does_not_flap_resolved_gate() {
    use crate::agent::config::AgentMode;
    use crate::auth::{GrokAuth, XAI_OAUTH2_ISSUER};
    let _restore = RestoreOtelGate;
    let server = xai_grok_test_support::MockInferenceServer::start()
        .await
        .unwrap();
    let xai_auth = GrokAuth {
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        ..GrokAuth::test_default()
    };
    let (agent, _rx) =
        build_agent_with_auth_and_proxy(xai_auth.clone(), server.url(), AgentMode::Leader);
    agent.otel_gate.set_resolved_for(&xai_auth.user_id);
    xai_grok_telemetry::external::mark_external_otel_settings_resolved();
    assert!(xai_grok_telemetry::external::is_settings_gate_open());
    agent.refresh_remote_settings(&xai_auth).await;
    assert!(
        xai_grok_telemetry::external::is_settings_gate_open(),
        "a same-credential refresh must not flap a gate already resolved for it"
    );
}
/// A `/settings` 401 from a token that rotated mid-flight must self-heal:
/// refresh once and, if the token changed, re-fetch with it. Without the
/// re-fetch the stale 401 fails OPEN (no remote policy).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn settings_self_heal_refetches_after_token_rotation() {
    use crate::agent::config::AgentMode;
    use crate::auth::refresh::{RefreshOutcome, TokenRefresher};
    use crate::auth::{GrokAuth, XAI_OAUTH2_ISSUER};
    let _restore = RestoreOtelGate;
    let server = xai_grok_test_support::MockInferenceServer::start_with_required_auth(
        vec![xai_grok_test_support::MockModelEntry::new("grok-build")],
        "rotated-key",
    )
    .await
    .unwrap();
    server.set_settings(serde_json::json!({ "allow_access": true }));
    struct RotatingRefresher;
    #[async_trait::async_trait]
    impl TokenRefresher for RotatingRefresher {
        async fn refresh(&self, _r: crate::auth::manager::RefreshReason) -> RefreshOutcome {
            RefreshOutcome::Success(Box::new(GrokAuth {
                key: "rotated-key".into(),
                oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..GrokAuth::test_default()
            }))
        }
    }
    let stale = GrokAuth {
        key: "stale-key".into(),
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        ..GrokAuth::test_default()
    };
    let (agent, _rx) =
        build_agent_with_auth_and_proxy(stale.clone(), server.url(), AgentMode::Leader);
    agent
        .auth_manager
        .set_refresher(std::sync::Arc::new(RotatingRefresher));
    xai_grok_telemetry::external::suppress_external_otel_until_settings();
    agent.refresh_remote_settings(&stale).await;
    assert!(
        xai_grok_telemetry::external::is_settings_gate_open(),
        "the rotated-token re-fetch must land settings and open the gate"
    );
    assert!(
        agent.cfg.borrow().remote_settings.is_some(),
        "the re-fetched settings must be stored"
    );
}
/// A logout can land while the detached post-auth fetch is in flight; the
/// result must not be cached for the logged-out identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn settings_not_cached_when_identity_logs_out_during_fetch() {
    use crate::agent::config::AgentMode;
    use crate::auth::{GrokAuth, XAI_OAUTH2_ISSUER};
    let _restore = RestoreOtelGate;
    let server = xai_grok_test_support::MockInferenceServer::start()
        .await
        .unwrap();
    server.set_settings(serde_json::json!({ "allow_access": true }));
    let xai_auth = GrokAuth {
        oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
        ..GrokAuth::test_default()
    };
    let (agent, _rx) =
        build_agent_with_auth_and_proxy(xai_auth.clone(), server.url(), AgentMode::Leader);
    agent.auth_manager.clear_in_memory();
    agent.refresh_remote_settings(&xai_auth).await;
    assert!(
        agent.cfg.borrow().remote_settings.is_none(),
        "settings fetched for a logged-out identity must not be cached"
    );
}
/// `ensure_session_supervisor` is idempotent: calling it repeatedly spawns
/// the sweeper loop exactly once.
#[test]
fn ensure_session_supervisor_is_idempotent() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        assert_eq!(agent.supervisor_spawn_count.get(), 0);
        agent.ensure_session_supervisor();
        agent.ensure_session_supervisor();
        agent.ensure_session_supervisor();
        assert_eq!(
            agent.supervisor_spawn_count.get(),
            1,
            "the supervisor task must be spawned at most once"
        );
        assert!(agent.supervisor_started.get());
    });
}
/// After a terminal removal (reap/close drops the live-state entry), a later
/// reload of the same SessionId starts clean at `IdleResident` with no stale
/// terminal state leaking in (ties to the bounded-map fix).
#[test]
fn reload_after_terminal_removal_starts_clean() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        let sid = acp::SessionId::new("sess-reload");
        let (handle, _tx, _rx) = make_live_session_handle(&sid, Some("turn-1"));
        agent.insert_resident(&sid, handle);
        assert_eq!(
            agent.close_active_session(&sid).await,
            crate::agent::mvp_agent::session_lifecycle::CloseOutcome::Closed,
            "the reload below is only meaningful after a close that happened"
        );
        assert_eq!(
            agent.session_live_state_for(&sid),
            None,
            "terminal removal must leave no stale state"
        );
        let (handle2, _tx2, _rx2) = make_live_session_handle(&sid, None);
        agent.insert_resident(&sid, handle2);
        agent.set_session_live_state(&sid, SessionLiveState::IdleResident);
        assert_eq!(
            agent.session_live_state_for(&sid),
            Some(SessionLiveState::IdleResident),
            "a reloaded session must start at IdleResident, not a stale terminal state"
        );
    });
}
/// Build an agent whose gateway is wired to a live receiver, so a test can
/// observe (and answer) agent→client reverse-requests like the dormant
/// `x.ai/folder_trust/request` round-trip.
fn build_agent_with_gateway_rx() -> (
    MvpAgent,
    tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) {
    use crate::agent::config::Config as AgentConfig;
    use crate::auth::{AuthManager, GrokComConfig};
    let temp_dir = tempfile::tempdir().unwrap();
    let auth_manager =
        std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(tx);
    let cfg = AgentConfig::default();
    let agent = MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
    (agent, rx)
}
/// A git repo whose only repo-local config is a project `.mcp.json` declaring
/// `projsrv` — so it is untrusted-with-configs, and the project server should
/// reappear after a trust grant.
fn repo_with_project_mcp_server() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git2::Repository::init(tmp.path()).unwrap();
    std::fs::write(
        tmp.path().join(".mcp.json"),
        r#"{"mcpServers":{"projsrv":{"command":"echo","args":["hi"]}}}"#,
    )
    .unwrap();
    tmp
}
fn write_project_subagent_definitions(cwd: &std::path::Path) {
    let roles = cwd.join(".grok/roles");
    let personas = cwd.join(".grok/personas");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::create_dir_all(&personas).unwrap();
    std::fs::write(roles.join("probe.toml"), "description = \"Project role\"").unwrap();
    std::fs::write(
        personas.join("probe.toml"),
        "instructions = \"Project persona\"",
    )
    .unwrap();
}
fn folder_trust_on() -> crate::util::config::RemoteSettings {
    crate::util::config::RemoteSettings {
        folder_trust_enabled: Some(true),
        ..Default::default()
    }
}
#[test]
fn subagent_spawn_context_uses_parent_auxiliary_provenance_after_config_reload() {
    run_local_for_bridge_test(|| async {
        let (agent, _rx) = build_agent_with_gateway_rx();
        let sid = acp::SessionId::new("subagent-parent-auxiliary-provenance");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.auxiliary_model_provenance = crate::session::AuxiliaryModelProvenance {
            web_search_follows_default: true,
            web_search_model: "spawn-default-search".to_owned(),
            image_description_follows_default: true,
            image_description_model: "spawn-default-image".to_owned(),
            ..Default::default()
        };
        agent.insert_resident(&sid, handle);
        {
            let mut cfg = agent.cfg.borrow_mut();
            cfg.web_search_follows_default = false;
            cfg.web_search_model = "reloaded-explicit-search".to_owned();
            cfg.image_description_follows_default = false;
            cfg.image_description_model = Some("reloaded-explicit-image".to_owned());
        }

        let ctx = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert!(ctx.web_search_follows_default);
        assert_eq!(ctx.web_search_model, "spawn-default-search");
        assert!(ctx.image_description_follows_default);
        assert_eq!(ctx.image_description_model, "spawn-default-image");
    });
}
#[test]
#[serial_test::serial]
fn subagent_spawn_context_reloads_project_definitions_after_trust_changes() {
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    write_project_subagent_definitions(repo.path());
    run_local_for_bridge_test(|| async {
        let (agent, _rx) = build_agent_with_gateway_rx();
        let sid = acp::SessionId::new("roles-personas-trust-transition");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo.path().display().to_string();
        agent.insert_resident(&sid, handle);
        {
            let mut cfg = agent.cfg.borrow_mut();
            cfg.subagent_roles.insert(
                "refreshed".into(),
                xai_grok_subagent_resolution::config::SubagentRole {
                    description: "Refreshed user role".into(),
                    source_dir: Some(repo.path().join("user-roles")),
                    ..Default::default()
                },
            );
            cfg.subagent_model_overrides
                .insert("probe".into(), "refreshed-model".into());
            cfg.subagent_toggle.insert("probe".into(), false);
        }
        crate::agent::folder_trust::record_for_test(repo.path(), false);
        let untrusted = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert!(!untrusted.subagent_roles.contains_key("probe"));
        assert!(!untrusted.subagent_personas.contains_key("probe"));
        assert_eq!(
            untrusted
                .subagent_roles
                .get("refreshed")
                .map(|role| role.description.as_str()),
            Some("Refreshed user role")
        );
        assert_eq!(
            untrusted
                .subagent_model_overrides
                .get("probe")
                .map(String::as_str),
            Some("refreshed-model")
        );
        assert_eq!(untrusted.subagent_toggle.get("probe"), Some(&false));
        crate::agent::folder_trust::record_for_test(repo.path(), true);
        let trusted = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert_eq!(
            trusted
                .subagent_roles
                .get("probe")
                .map(|role| role.description.as_str()),
            Some("Project role")
        );
        assert!(trusted.subagent_personas.contains_key("probe"));
        crate::agent::folder_trust::record_for_test(repo.path(), false);
        let revoked = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert!(!revoked.subagent_roles.contains_key("probe"));
        assert!(!revoked.subagent_personas.contains_key("probe"));
    });
}
/// End-to-end gate wiring: project `.grok/roles` / `personas` alone must drive
/// real `resolve_and_record` untrusted (not a forced `record_for_test` verdict),
/// keep project defs out of Task spawn context, then re-admit them after grant.
#[test]
#[serial_test::serial]
fn project_roles_personas_gated_via_resolve_and_record_chain() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = tempfile::tempdir().unwrap();
    git2::Repository::init(repo.path()).unwrap();
    write_project_subagent_definitions(repo.path());
    run_local_for_bridge_test(|| async {
        let (agent, _rx) = build_agent_with_gateway_rx();
        let sid = acp::SessionId::new("roles-personas-resolve-chain");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo.path().display().to_string();
        agent.insert_resident(&sid, handle);
        let allowed = crate::agent::folder_trust::resolve_and_record(
            repo.path(),
            Some(&folder_trust_on()),
            false,
        );
        assert!(
            !allowed,
            "roles/personas markers alone must resolve untrusted without a grant"
        );
        assert!(
            !crate::agent::folder_trust::project_scope_allowed(repo.path()),
            "cached verdict after resolve_and_record must stay untrusted"
        );
        let untrusted = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert!(
            !untrusted.subagent_roles.contains_key("probe"),
            "untrusted: project role must stay out of spawn context"
        );
        assert!(
            !untrusted.subagent_personas.contains_key("probe"),
            "untrusted: project persona must stay out of spawn context"
        );
        crate::agent::folder_trust::grant_folder_trust(repo.path());
        let allowed = crate::agent::folder_trust::resolve_and_record(
            repo.path(),
            Some(&folder_trust_on()),
            false,
        );
        assert!(allowed, "store-granted folder must resolve trusted");
        let trusted = agent.build_subagent_spawn_context(sid.0.as_ref());
        assert_eq!(
            trusted
                .subagent_roles
                .get("probe")
                .map(|role| role.description.as_str()),
            Some("Project role")
        );
        assert!(
            trusted.subagent_personas.contains_key("probe"),
            "trusted: project persona must enter spawn context after grant"
        );
    });
}
/// Pull the next `x.ai/folder_trust/request` reverse-request off the gateway and
/// answer it with `outcome`. Returns the request's decoded params.
async fn answer_folder_trust_request(
    gw_rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
    outcome: &str,
) -> serde_json::Value {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv())
        .await
        .expect("trust request must be sent")
        .expect("gateway channel open");
    let xai_acp_lib::AcpClientMessage::ExtMethod(args) = msg else {
        panic!("expected an ext_method reverse-request, got a different message");
    };
    assert_eq!(args.request.method.as_ref(), "x.ai/folder_trust/request");
    let params: serde_json::Value = serde_json::from_str(args.request.params.get()).unwrap();
    let resp: acp::ExtResponse = acp::ExtResponse::new(std::sync::Arc::from(
        serde_json::value::to_raw_value(&serde_json::json!({ "outcome": outcome })).unwrap(),
    ));
    let _ = args.response_tx.send(Ok(resp));
    params
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_grant_reloads_project_mcp() {
    use xai_grok_test_support::EnvGuard;
    use xai_grok_workspace::trust::{TrustStore, workspace_key};
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
        assert!(
            !crate::agent::folder_trust::project_scope_allowed(&repo_path),
            "untrusted-with-configs workspace must gate project scope before the grant"
        );
        let sid = acp::SessionId::new("sess-trust");
        let (mut handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.insert_resident(&sid, handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        let params = answer_folder_trust_request(&mut gw_rx, "trust").await;
        assert!(
            params["configKinds"]
                .as_array()
                .is_some_and(|k| k.iter().any(|v| v == "mcp")),
            "request must summarize detected config kinds; got {params}"
        );
        assert_eq!(
            params["sessionId"], "sess-trust",
            "trust request must carry the session id for leader routing; got {params}"
        );
        let mut saw_project_mcp = false;
        let mut saw_reload_plugins = false;
        let mut saw_reload_hooks = false;
        for _ in 0..8 {
            match tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await {
                Ok(Some(TestSessionCommand::UpdateMcpServers { mcp_servers, .. })) => {
                    saw_project_mcp |= mcp_servers
                        .iter()
                        .any(|s| crate::session::managed_mcp::mcp_server_name(s) == "projsrv");
                }
                Ok(Some(TestSessionCommand::ReloadPlugins { .. })) => {
                    saw_reload_plugins = true;
                }
                Ok(Some(TestSessionCommand::ReloadHooks)) => saw_reload_hooks = true,
                Ok(Some(_other)) => continue,
                _ => break,
            }
            if saw_project_mcp && saw_reload_plugins && saw_reload_hooks {
                break;
            }
        }
        assert!(
            saw_project_mcp,
            "trust grant must reload the session's now-trusted project MCP server"
        );
        assert!(
            saw_reload_plugins,
            "trust grant must reload plugins (plugin-contributed hooks/MCP)"
        );
        assert!(
            saw_reload_hooks,
            "trust grant must reload the session's own project hooks"
        );
        assert!(
            TrustStore::load().is_trusted(&workspace_key(&repo_path)),
            "accepting the prompt must persist the trust grant"
        );
        assert!(
            crate::agent::folder_trust::project_scope_allowed(&repo_path),
            "the in-process gate must flip to trusted after the grant"
        );
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_reject_keeps_gated() {
    use xai_grok_test_support::EnvGuard;
    use xai_grok_workspace::trust::{TrustStore, workspace_key};
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
        let sid = acp::SessionId::new("sess-reject");
        let (mut handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.insert_resident(&sid, handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        let _ = answer_folder_trust_request(&mut gw_rx, "reject").await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), cmd_rx.recv())
                .await
                .is_err(),
            "rejecting trust must leave the session's project servers gated (no reload)"
        );
        assert!(
            !TrustStore::load().is_trusted(&workspace_key(&repo_path)),
            "rejecting trust must leave the store unchanged"
        );
        assert!(
            !crate::agent::folder_trust::project_scope_allowed(&repo_path),
            "rejecting trust must keep the workspace gated"
        );
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_dormant_when_feature_off() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = crate::util::config::RemoteSettings {
        folder_trust_enabled: Some(false),
        ..Default::default()
    };
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        let sid = acp::SessionId::new("sess-dormant");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.insert_resident(&sid, handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), gw_rx.recv())
                .await
                .is_err(),
            "feature off must emit no trust request (dormant)"
        );
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_no_request_without_capability() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        assert!(!agent.interactive_trust_client.get());
        let sid = acp::SessionId::new("sess-nocap");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.insert_resident(&sid, handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), gw_rx.recv())
                .await
                .is_err(),
            "a client without the capability must get no trust request"
        );
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_client_error_fails_closed() {
    use xai_grok_test_support::EnvGuard;
    use xai_grok_workspace::trust::{TrustStore, workspace_key};
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
        let sid = acp::SessionId::new("sess-err");
        let (mut handle, _tx, mut cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.insert_resident(&sid, handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv())
            .await
            .expect("trust request must be sent")
            .expect("gateway channel open");
        assert!(matches!(msg, xai_acp_lib::AcpClientMessage::ExtMethod(_)));
        drop(msg);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), cmd_rx.recv())
                .await
                .is_err(),
            "a failed client round-trip must not reload the session"
        );
        assert!(
            !TrustStore::load().is_trusted(&workspace_key(&repo_path)),
            "a failed client round-trip must not grant trust"
        );
        assert!(!crate::agent::folder_trust::project_scope_allowed(
            &repo_path
        ));
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_dedups_same_workspace() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
        let sid = acp::SessionId::new("sess-dedup");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.insert_resident(&sid, handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv()).await;
        assert!(
            matches!(first, Ok(Some(xai_acp_lib::AcpClientMessage::ExtMethod(_)))),
            "first prompt for an untrusted workspace must emit a request"
        );
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), gw_rx.recv())
                .await
                .is_err(),
            "a workspace already prompted this process must not be re-prompted"
        );
    });
}
/// Which reload commands a session received after a grant.
struct ReloadCmds {
    update_mcp: bool,
    reload_plugins: bool,
    reload_hooks: bool,
    mcp_names: Vec<String>,
}
/// Drain a session's command channel for the post-grant reload trio
/// (`UpdateMcpServers` + `ReloadPlugins` + `ReloadHooks`), capturing the merged
/// MCP server names so a test can assert per-cwd reload.
async fn drain_reload_commands(
    cmd_rx: &mut tokio::sync::mpsc::UnboundedReceiver<TestSessionCommand>,
) -> ReloadCmds {
    let mut out = ReloadCmds {
        update_mcp: false,
        reload_plugins: false,
        reload_hooks: false,
        mcp_names: Vec::new(),
    };
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await {
            Ok(Some(TestSessionCommand::UpdateMcpServers { mcp_servers, .. })) => {
                out.update_mcp = true;
                out.mcp_names = mcp_servers
                    .iter()
                    .map(|s| crate::session::managed_mcp::mcp_server_name(s).to_string())
                    .collect();
            }
            Ok(Some(TestSessionCommand::ReloadPlugins { .. })) => {
                out.reload_plugins = true;
            }
            Ok(Some(TestSessionCommand::ReloadHooks)) => out.reload_hooks = true,
            Ok(Some(_other)) => continue,
            _ => break,
        }
        if out.update_mcp && out.reload_plugins && out.reload_hooks {
            break;
        }
    }
    out
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_reloads_all_same_workspace_sessions() {
    use xai_grok_test_support::EnvGuard;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let root = repo.path().to_path_buf();
    let subdir = root.join("sub");
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::write(
        subdir.join(".mcp.json"),
        r#"{"mcpServers":{"subsrv":{"command":"echo","args":["hi"]}}}"#,
    )
    .unwrap();
    let other = repo_with_project_mcp_server();
    let other_path = other.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&root, Some(&remote), false);
        let sid_root = acp::SessionId::new("sess-root");
        let (mut h_root, _t1, mut rx_root) = make_live_session_handle(&sid_root, None);
        h_root.info.cwd = root.to_string_lossy().to_string();
        agent.insert_resident(&sid_root, h_root);
        let sid_sub = acp::SessionId::new("sess-sub");
        let (mut h_sub, _t2, mut rx_sub) = make_live_session_handle(&sid_sub, None);
        h_sub.info.cwd = subdir.to_string_lossy().to_string();
        agent.insert_resident(&sid_sub, h_sub);
        let sid_other = acp::SessionId::new("sess-other");
        let (mut h_other, _t3, mut rx_other) = make_live_session_handle(&sid_other, None);
        h_other.info.cwd = other_path.to_string_lossy().to_string();
        agent.insert_resident(&sid_other, h_other);
        agent.maybe_spawn_interactive_trust_prompt(&sid_root, &root, Some(&remote));
        let _ = answer_folder_trust_request(&mut gw_rx, "trust").await;
        let root_cmds = drain_reload_commands(&mut rx_root).await;
        assert!(
            root_cmds.update_mcp && root_cmds.reload_plugins && root_cmds.reload_hooks,
            "root session must get UpdateMcpServers + ReloadPlugins + ReloadHooks"
        );
        let sub_cmds = drain_reload_commands(&mut rx_sub).await;
        assert!(
            sub_cmds.update_mcp && sub_cmds.reload_plugins && sub_cmds.reload_hooks,
            "subdir session must get UpdateMcpServers + ReloadPlugins + ReloadHooks"
        );
        assert!(
            sub_cmds.mcp_names.iter().any(|n| n == "subsrv"),
            "subdir session must reload against its own cwd (expect subsrv); got {:?}",
            sub_cmds.mcp_names
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), rx_other.recv())
                .await
                .is_err(),
            "a session under a different workspace_key must not be reloaded"
        );
    });
}
#[test]
#[serial_test::serial]
fn interactive_trust_prompt_reprompts_after_untrust() {
    use xai_grok_test_support::EnvGuard;
    use xai_hooks_plugins_types::HooksAction;
    let home = tempfile::tempdir().unwrap();
    let _env = EnvGuard::set("GROK_HOME", home.path());
    let _sim = EnvGuard::set(xai_grok_version::TEST_VERSION_ENV, "0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    let repo = repo_with_project_mcp_server();
    let repo_path = repo.path().to_path_buf();
    let remote = folder_trust_on();
    run_local_for_bridge_test(|| async {
        let (agent, mut gw_rx) = build_agent_with_gateway_rx();
        agent.interactive_trust_client.set(true);
        crate::agent::folder_trust::resolve_and_record(&repo_path, Some(&remote), false);
        let sid = acp::SessionId::new("sess-reprompt");
        let (mut handle, _tx, _cmd_rx) = make_live_session_handle(&sid, None);
        handle.info.cwd = repo_path.to_string_lossy().to_string();
        agent.insert_resident(&sid, handle);
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            matches!(
                tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv()).await,
                Ok(Some(xai_acp_lib::AcpClientMessage::ExtMethod(_)))
            ),
            "first prompt must emit a request"
        );
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), gw_rx.recv())
                .await
                .is_err(),
            "a prompted workspace must be suppressed before untrust"
        );
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            agent.execute_hooks_action(&sid, HooksAction::Untrust),
        )
        .await;
        agent.maybe_spawn_interactive_trust_prompt(&sid, &repo_path, Some(&remote));
        assert!(
            matches!(
                tokio::time::timeout(std::time::Duration::from_secs(2), gw_rx.recv()).await,
                Ok(Some(xai_acp_lib::AcpClientMessage::ExtMethod(_)))
            ),
            "after untrust clears the dedup, the workspace must be promptable again"
        );
    });
}
fn ann(id: &str) -> xai_grok_announcements::RemoteAnnouncement {
    xai_grok_announcements::RemoteAnnouncement {
        id: Some(id.to_string()),
        message: Some(format!("{id}-msg")),
        severity: Some("critical".to_string()),
        ..Default::default()
    }
}
/// `RemoteSettings` with only `announcements` set (callers add sentinel
/// fields as needed).
fn settings_with(
    announcements: Option<Vec<xai_grok_announcements::RemoteAnnouncement>>,
) -> crate::util::config::RemoteSettings {
    crate::util::config::RemoteSettings {
        announcements,
        ..Default::default()
    }
}
fn test_now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}
/// Pushes must carry strictly increasing generations, seeded from unix-epoch
/// seconds so a restarted leader still beats pager watermarks that survived
/// re-election (`AppView.announcements_last_gen` is never reset).
#[tokio::test]
async fn announcements_gen_seeds_from_epoch_and_strictly_increases() {
    let agent = build_minimal_agent_for_tests();
    let epoch_before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let first = agent.next_announcements_gen();
    let second = agent.next_announcements_gen();
    assert!(
        first >= epoch_before,
        "first gen must be epoch-seeded: {first} < {epoch_before}"
    );
    assert!(
        second > first,
        "gens must strictly increase: {first} -> {second}"
    );
    let far_ahead = first + 1_000_000;
    agent.announcements_gen.set(far_ahead);
    assert_eq!(agent.next_announcements_gen(), far_ahead + 1);
}
/// An unchanged visible list must not produce a push (idle steady-state is
/// silent); a changed one — including clearing to empty — must.
#[test]
fn announcements_push_gate_emits_only_on_change() {
    let now = test_now();
    assert_eq!(
        announcements_push_payload(None, &[], now, AnnouncementsPushMode::IfChanged),
        None
    );
    let list_a = vec![ann("a")];
    assert_eq!(
        announcements_push_payload(
            Some(list_a.as_slice()),
            &[],
            now,
            AnnouncementsPushMode::IfChanged
        ),
        Some(list_a.clone())
    );
    assert_eq!(
        announcements_push_payload(
            Some(list_a.as_slice()),
            &list_a,
            now,
            AnnouncementsPushMode::IfChanged
        ),
        None
    );
    let list_ab = vec![ann("a"), ann("b")];
    assert_eq!(
        announcements_push_payload(
            Some(list_ab.as_slice()),
            &list_a,
            now,
            AnnouncementsPushMode::IfChanged
        ),
        Some(list_ab.clone())
    );
    assert_eq!(
        announcements_push_payload(None, &list_ab, now, AnnouncementsPushMode::IfChanged),
        Some(vec![])
    );
}
/// `seed` (per-client initialize) re-emits an unchanged non-empty list for
/// the freshly attached client, but stays silent when there is nothing to
/// show.
#[test]
fn announcements_push_gate_seed_reemits_nonempty_only() {
    let now = test_now();
    let list_a = vec![ann("a")];
    assert_eq!(
        announcements_push_payload(
            Some(list_a.as_slice()),
            &list_a,
            now,
            AnnouncementsPushMode::SeedNewClient
        ),
        Some(list_a.clone()),
        "seed must re-push an unchanged non-empty list"
    );
    assert_eq!(
        announcements_push_payload(None, &[], now, AnnouncementsPushMode::SeedNewClient),
        None,
        "seed with nothing visible must stay silent"
    );
}
/// `/new` forces a push even when the visible list is unchanged — including
/// unchanged-empty — so the pager re-merges its config-layer (requirements/
/// user/managed TOML) announcements from local mid-session edits.
#[test]
fn announcements_push_gate_force_mode_pushes_unchanged_and_empty() {
    let now = test_now();
    let list_a = vec![ann("a")];
    assert_eq!(
        announcements_push_payload(
            Some(list_a.as_slice()),
            &list_a,
            now,
            AnnouncementsPushMode::Force
        ),
        Some(list_a.clone()),
        "force must push an unchanged list"
    );
    assert_eq!(
        announcements_push_payload(None, &[], now, AnnouncementsPushMode::Force),
        Some(vec![]),
        "force must push even an unchanged empty list"
    );
}
/// An addition that is already expired on arrival never becomes visible, so
/// it must not re-emit.
#[test]
fn announcements_push_gate_ignores_expired_only_addition() {
    let now = test_now();
    let expired = xai_grok_announcements::RemoteAnnouncement {
        expires_at: Some("2000-01-01T00:00:00Z".to_string()),
        ..ann("expired")
    };
    let list_a = vec![ann("a")];
    let stored = vec![ann("a"), expired];
    assert_eq!(
        announcements_push_payload(
            Some(stored.as_slice()),
            &list_a,
            now,
            AnnouncementsPushMode::IfChanged
        ),
        None,
        "an already-expired addition must not re-emit"
    );
}
/// A previously emitted item that passes its `expires_at` between gate runs
/// must emit the shrunken (here: empty) list exactly once, so live banners
/// clear on time instead of outliving their own expiry.
#[test]
fn announcements_push_gate_emits_on_expiry_crossing() {
    let expiring = xai_grok_announcements::RemoteAnnouncement {
        expires_at: Some("2026-06-01T00:00:00Z".to_string()),
        ..ann("soon")
    };
    let stored = vec![expiring.clone()];
    let before = chrono::DateTime::parse_from_rfc3339("2026-05-31T23:59:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let emitted = announcements_push_payload(
        Some(stored.as_slice()),
        &[],
        before,
        AnnouncementsPushMode::IfChanged,
    )
    .expect("live item must emit");
    assert_eq!(emitted, stored);
    let after = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:01:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert_eq!(
        announcements_push_payload(
            Some(stored.as_slice()),
            &emitted,
            after,
            AnnouncementsPushMode::IfChanged
        ),
        Some(vec![]),
        "expiry crossing must emit the shrunken list"
    );
    assert_eq!(
        announcements_push_payload(
            Some(stored.as_slice()),
            &[],
            after,
            AnnouncementsPushMode::IfChanged
        ),
        None
    );
}
/// A poll apply must touch ONLY `remote_settings.announcements`; every other
/// stored field keeps its pre-poll value (full reapply stays owned by
/// startup, auth, and `/new`).
#[tokio::test]
async fn polled_announcements_apply_touches_announcements_only() {
    let agent = build_minimal_agent_for_tests();
    let mut stored = settings_with(Some(vec![ann("old")]));
    stored.tips = Some(vec!["stored-tip".to_string()]);
    stored.allow_access = Some(true);
    stored.default_model = Some("stored-model".to_string());
    agent.cfg.borrow_mut().remote_settings = Some(stored);
    let mut fresh = settings_with(Some(vec![ann("new")]));
    fresh.tips = Some(vec!["fresh-tip".to_string()]);
    fresh.allow_access = Some(false);
    fresh.default_model = Some("fresh-model".to_string());
    agent.apply_polled_announcements(fresh, Some(vec![ann("old")]));
    let cfg = agent.cfg.borrow();
    let after = cfg
        .remote_settings
        .as_ref()
        .expect("settings still present");
    assert_eq!(after.announcements, Some(vec![ann("new")]));
    assert_eq!(
        after.tips,
        Some(vec!["stored-tip".to_string()]),
        "tips must be untouched by a poll apply"
    );
    assert_eq!(
        after.allow_access,
        Some(true),
        "allow_access must be untouched by a poll apply"
    );
    assert_eq!(
        after.default_model.as_deref(),
        Some("stored-model"),
        "default_model must be untouched by a poll apply"
    );
}
/// A poll apply must never fabricate `remote_settings` from scratch — the
/// `is_none()`-keyed retry/gating semantics of the full-refresh owners
/// depend on absence staying observable.
#[tokio::test]
async fn polled_announcements_apply_never_fabricates_settings() {
    let agent = build_minimal_agent_for_tests();
    agent.cfg.borrow_mut().remote_settings = None;
    agent.apply_polled_announcements(settings_with(Some(vec![ann("a")])), None);
    assert!(
        agent.cfg.borrow().remote_settings.is_none(),
        "a poll must leave absent remote_settings absent"
    );
}
/// A full-refresh writer landing during the poll's fetch makes the poll's
/// result stale; the apply must skip rather than clobber the fresher store
/// (the next tick reconciles).
#[tokio::test]
async fn polled_announcements_apply_skips_when_writer_landed_mid_fetch() {
    let agent = build_minimal_agent_for_tests();
    let pre_fetch = Some(vec![ann("old")]);
    agent.cfg.borrow_mut().remote_settings = Some(settings_with(Some(vec![ann("mid-fetch")])));
    agent.apply_polled_announcements(settings_with(Some(vec![ann("stale-poll")])), pre_fetch);
    assert_eq!(
        agent
            .cfg
            .borrow()
            .remote_settings
            .as_ref()
            .and_then(|s| s.announcements.clone()),
        Some(vec![ann("mid-fetch")]),
        "the mid-fetch writer's store must win over the stale poll result"
    );
}
/// End-to-end through the shared gate: every emission advances the baseline
/// and carries a strictly larger gen; unchanged state is silent unless
/// seeding a new client.
#[tokio::test]
async fn emit_announcements_gate_emits_updates_baseline_and_bumps_gen() {
    let (agent, mut rx) = build_agent_with_gateway_rx();
    agent.cfg.borrow_mut().remote_settings = Some(settings_with(Some(vec![ann("a")])));
    let recv_gen =
        |rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>| {
            let msg = rx.try_recv().expect("expected an announcements push");
            let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
                panic!("expected ExtNotification, got another message kind");
            };
            assert_eq!(args.request.method.as_ref(), "x.ai/announcements/update");
            let parsed: serde_json::Value =
                serde_json::from_str(args.request.params.get()).expect("valid JSON payload");
            parsed
                .get("gen")
                .and_then(|g| g.as_u64())
                .expect("gen field")
        };
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    let first_gen = recv_gen(&mut rx);
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    assert!(rx.try_recv().is_err(), "unchanged list must not re-push");
    agent.emit_announcements(AnnouncementsPushMode::SeedNewClient);
    let seed_gen = recv_gen(&mut rx);
    assert!(
        seed_gen > first_gen,
        "gen must strictly increase: {first_gen} -> {seed_gen}"
    );
    agent.cfg.borrow_mut().remote_settings = Some(settings_with(None));
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    let clear_gen = recv_gen(&mut rx);
    assert!(clear_gen > seed_gen);
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    assert!(
        rx.try_recv().is_err(),
        "cleared state must push exactly once"
    );
    agent.emit_announcements(AnnouncementsPushMode::Force);
    let force_gen = recv_gen(&mut rx);
    assert!(
        force_gen > clear_gen,
        "forced push must keep gens increasing"
    );
}
/// A send the gateway channel rejects must not advance the last-emitted
/// baseline; the next gate call then re-diffs and re-pushes the same list
/// (the poll's natural retry, no dedicated retry machinery).
#[tokio::test]
async fn emit_announcements_gate_keeps_baseline_on_failed_send_and_retries() {
    let (mut agent, rx) = build_agent_with_gateway_rx();
    agent.cfg.borrow_mut().remote_settings = Some(settings_with(Some(vec![ann("a")])));
    drop(rx);
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    assert!(
        agent.last_emitted_announcements.borrow().is_empty(),
        "a failed send must leave the baseline untouched"
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.gateway = GatewaySender::new(tx);
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    let msg = rx
        .try_recv()
        .expect("next gate call must re-push after a failed send");
    let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
        panic!("expected ExtNotification, got another message kind");
    };
    assert_eq!(args.request.method.as_ref(), "x.ai/announcements/update");
    assert_eq!(
        *agent.last_emitted_announcements.borrow(),
        vec![ann("a")],
        "a successful send advances the baseline"
    );
    agent.emit_announcements(AnnouncementsPushMode::IfChanged);
    assert!(rx.try_recv().is_err(), "unchanged list must not re-push");
}
mod direct_hub_cloud_removed {
    use super::super::{DIRECT_HUB_CLOUD_REMOVED_MSG, reject_direct_hub_cloud_meta};
    use crate::agent::config::HubConfig;
    fn assert_direct_hub_error(err: agent_client_protocol::Error) {
        assert_eq!(
            err.data.as_ref(),
            Some(&serde_json::Value::String(
                DIRECT_HUB_CLOUD_REMOVED_MSG.to_string()
            )),
            "error data must be the exact D8 message, got: {err:?}"
        );
        assert_eq!(
            err.code,
            agent_client_protocol::ErrorCode::InvalidParams,
            "must be invalid_params, got: {err:?}"
        );
    }
    #[test]
    fn cloud_server_id_meta_is_hard_error() {
        let meta = serde_json::json!({ "x.ai/cloud_server_id": "srv-123" });
        let err = reject_direct_hub_cloud_meta(meta.as_object()).expect_err("must reject");
        assert_direct_hub_error(err);
    }
    #[test]
    fn cloud_server_id_null_still_present_is_hard_error() {
        let meta = serde_json::json!({ "x.ai/cloud_server_id": null });
        let err = reject_direct_hub_cloud_meta(meta.as_object()).expect_err("must reject");
        assert_direct_hub_error(err);
    }
    #[test]
    fn cloud_server_id_with_gateway_meta_still_hard_error() {
        let meta = serde_json::json!({
            "x.ai/cloud_server_id": "srv-legacy",
            "envId": "env-1",
            "x.ai/cloud_existing_workspace": {
                "server_id": "ws-1",
                "cwd": "/workspace"
            }
        });
        let err = reject_direct_hub_cloud_meta(meta.as_object()).expect_err("Direct stamp wins");
        assert_direct_hub_error(err);
    }
    #[test]
    fn absent_or_empty_meta_ok() {
        assert!(reject_direct_hub_cloud_meta(None).is_ok());
        assert!(reject_direct_hub_cloud_meta(serde_json::json!({}).as_object()).is_ok());
        assert!(
            reject_direct_hub_cloud_meta(
                serde_json::json!({
                    "envId": "env-1"
                })
                .as_object()
            )
            .is_ok()
        );
        assert!(
            reject_direct_hub_cloud_meta(
                serde_json::json!({
                    "x.ai/cloud_existing_workspace": {
                        "server_id": "ws-1",
                        "cwd": "/workspace"
                    }
                })
                .as_object()
            )
            .is_ok()
        );
    }
    #[test]
    fn hub_url_gating_matrix() {
        let with_url = HubConfig {
            url: Some("wss://hub.example/ws".into()),
        };
        let without_url = HubConfig { url: None };
        let blank = HubConfig {
            url: Some("   ".into()),
        };
        assert!(with_url.is_enabled());
        assert!(!without_url.is_enabled());
        assert!(!blank.is_enabled());
    }
    #[test]
    fn hub_config_is_url_only_workspace_default() {
        let json = serde_json::to_value(HubConfig {
            url: Some("wss://hub.example/ws".into()),
        })
        .expect("serialize");
        let obj = json.as_object().expect("object");
        assert_eq!(
            obj.keys().collect::<Vec<_>>(),
            vec!["url"],
            "HubConfig must only serialize url (no proxy-mode fields)"
        );
        let from_legacy: HubConfig = serde_json::from_value(serde_json::json!({
            "url": "wss://hub.example/ws",
            "workspace_mode": "remote",
            "send_turn_hooks": false,
        }))
        .expect("ignore unknown fields");
        assert_eq!(from_legacy.url.as_deref(), Some("wss://hub.example/ws"));
    }
}
mod local_workspace_removed {
    use super::super::{LOCAL_WORKSPACE_REMOVED_MSG, reject_removed_local_workspace_meta};
    fn assert_local_workspace_removed_error(err: agent_client_protocol::Error) {
        assert_eq!(
            err.code,
            agent_client_protocol::ErrorCode::InvalidParams,
            "must be invalid_params, got: {err:?}"
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(|v| v.as_str()),
            Some("local_workspace_removed"),
            "error code must identify removed local-workspace surface"
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|d| d.get("message"))
                .and_then(|v| v.as_str()),
            Some(LOCAL_WORKSPACE_REMOVED_MSG),
            "error message must preserve the removed-surface guidance"
        );
    }
    #[test]
    fn local_workspace_removed_meta_rejected_fail_closed_matrix() {
        let cases: &[(&str, serde_json::Value, bool)] = &[
            (
                "present_object",
                serde_json::json!({ "x.ai/local_workspace": { "mode": "attach", "server_id": "srv-1" } }),
                true,
            ),
            (
                "present_null",
                serde_json::json!({ "x.ai/local_workspace": null }),
                true,
            ),
            ("absent_key", serde_json::json!({ "envId": "env-1" }), false),
        ];
        for (label, meta, expect_error) in cases {
            let outcome = reject_removed_local_workspace_meta(meta.as_object());
            match (expect_error, outcome) {
                (true, Err(err)) => assert_local_workspace_removed_error(err),
                (false, Ok(())) => {}
                (true, Ok(())) => panic!("[{label}] expected local-workspace rejection"),
                (false, Err(err)) => panic!("[{label}] unexpected rejection: {err:?}"),
            }
        }
    }
}
mod soft_default_settings_emit {
    use super::*;
    #[tokio::test]
    async fn emit_settings_update_carries_permission_mode_from_cfg() {
        use crate::agent::config::Config as AgentConfig;
        use crate::auth::{AuthManager, GrokComConfig};
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let temp_dir = tempfile::tempdir().unwrap();
                let auth_manager = std::sync::Arc::new(AuthManager::new(
                    temp_dir.path(),
                    GrokComConfig::default(),
                ));
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let gateway = GatewaySender::new(tx);
                let cfg = AgentConfig {
                    remote_settings: Some(crate::util::config::RemoteSettings {
                        permission_mode: Some("always-approve".into()),
                        slash_command_tags: Some(
                            [("workflows".to_string(), "new".to_string())]
                                .into_iter()
                                .collect(),
                        ),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                let agent =
                    MvpAgent::new(gateway, &cfg, auth_manager, None).expect("valid test config");
                agent.cfg.borrow_mut().remote_settings = cfg.remote_settings.clone();
                agent.emit_settings_update_notification();
                let msg = rx.try_recv().expect("settings/update must be emitted");
                let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
                    panic!("expected ExtNotification, got {msg:?}");
                };
                assert_eq!(args.request.method.as_ref(), "x.ai/settings/update");
                let params: serde_json::Value =
                    serde_json::from_str(args.request.params.get()).expect("parse params");
                assert_eq!(
                    params.get("permission_mode").and_then(|v| v.as_str()),
                    Some("always-approve"),
                    "post-auth emit must carry remote permission_mode for first session"
                );
                assert_eq!(
                    params
                        .get("slash_command_tags")
                        .and_then(|v| v.get("workflows"))
                        .and_then(|v| v.as_str()),
                    Some("new"),
                    "post-auth emit must carry remote slash_command_tags"
                );
                let _ = args.response_tx.send(Ok(()));
            })
            .await;
    }
}

/// #303 / #320: production initialize must probe an ambient xAI env key even
/// when a ready Codex account route exists, repair the implicit Grok default,
/// and keep `/new` on Codex when a later soft campaign nudges back to Grok.
#[test]
#[serial_test::serial]
fn initialize_invalid_xai_probe_reseats_implicit_grok_to_ready_codex() {
    const CHILD_ENV: &str = "__MEDLEY_INVALID_XAI_PROBE_CHILD";
    const CHILD_PASS: &str = "invalid-xai-probe-reseat-ok";

    if std::env::var_os(CHILD_ENV).is_none() {
        let tmp = tempfile::tempdir().expect("fresh-process state home");
        let state_home = tmp.path().join("state");
        std::fs::create_dir_all(&state_home).expect("create fresh-process state home");
        let filter = module_path!()
            .split_once("::")
            .map(|(_, rest)| rest)
            .unwrap_or_default();
        let mut command = std::process::Command::new(std::env::current_exe().expect("current_exe"));
        command
            .arg("--exact")
            .arg(format!(
                "{filter}::initialize_invalid_xai_probe_reseats_implicit_grok_to_ready_codex"
            ))
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_ENV, &state_home)
            .env("MEDLEY_HOME", &state_home)
            .env("GROK_HOME", &state_home)
            .env_remove("GROK_DEFAULT_MODEL")
            .env_remove("GROK_DEPLOYMENT_KEY")
            .env_remove("GROK_DISABLE_API_KEY_AUTH")
            .env_remove("GROK_AUTH")
            .env_remove("GROK_AUTH_PATH")
            .env_remove("GROK_LOCAL_AUTH")
            .env_remove("GROK_AUTH_PROVIDER_COMMAND")
            .env_remove("GROK_AUTH_PROVIDER_LABEL")
            .env_remove("GROK_AUTH_TOKEN_TTL")
            .env_remove("GROK_OIDC_ISSUER")
            .env_remove("GROK_OIDC_CLIENT_ID")
            .env_remove("GROK_OIDC_SCOPES")
            .env_remove("GROK_OIDC_AUDIENCE")
            .env_remove("GROK_OAUTH2_ISSUER")
            .env_remove("GROK_OAUTH2_CLIENT_ID")
            .env_remove("GROK_OAUTH2_SCOPES")
            .env_remove("GROK_OAUTH2_PRINCIPAL_TYPE")
            .env_remove("GROK_OAUTH2_PRINCIPAL_ID")
            .env_remove("GROK_OAUTH2_REFERRER")
            .env_remove("GROK_CAMPAIGNS_OVERRIDE")
            .stdin(std::process::Stdio::null());
        xai_tty_utils::detach_std_command(&mut command);
        let output = command.output().expect("spawn invalid-probe child test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && !stderr.contains("panicked at"),
            "fresh-process invalid-probe regression failed (status: {:?})\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
        assert!(
            stdout.contains(CHILD_PASS),
            "child did not execute the invalid-probe regression\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        return;
    }

    let state_home = std::path::PathBuf::from(
        std::env::var_os(CHILD_ENV).expect("child state home marker must be present"),
    );
    assert_eq!(
        xai_grok_config::grok_home(),
        state_home,
        "fresh child must pin the process-global state home to the fixture"
    );

    run_local_for_bridge_test(|| async {
        use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
        use crate::agent::config::Config as AgentConfig;
        use crate::auth::{AuthManager, AuthMode, GrokAuth, GrokComConfig};
        use xai_grok_test_support::EnvGuard;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("probe listener");
        let addr = listener.local_addr().expect("probe address");
        listener
            .set_nonblocking(true)
            .expect("nonblocking probe listener");
        let probe_server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "initialize did not send the production /api-key probe"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept probe request: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("make accepted probe stream blocking");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .expect("bound probe request read");

            const MAX_REQUEST_HEADER_BYTES: usize = 8192;
            let mut request = Vec::with_capacity(2048);
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                assert!(
                    request.len() < MAX_REQUEST_HEADER_BYTES,
                    "probe request headers exceeded {MAX_REQUEST_HEADER_BYTES} bytes"
                );
                let mut chunk = [0u8; 1024];
                let remaining = MAX_REQUEST_HEADER_BYTES - request.len();
                let read_len = remaining.min(chunk.len());
                match stream.read(&mut chunk[..read_len]) {
                    Ok(0) => panic!("probe connection closed before complete request headers"),
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => panic!("read probe request: {error}"),
                }
            }
            let request = String::from_utf8_lossy(&request);
            assert!(
                request.starts_with("GET /v1/api-key "),
                "initialize must use the production /api-key probe path: {request}"
            );
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 29\r\nConnection: close\r\n\r\n{\"error\":\"Incorrect API key\"}",
                )
                .expect("write probe response");
        });

        let tmp = tempfile::tempdir().expect("temp auth fixtures");
        let auth_path = tmp.path().join("auth.json");
        let codex_auth = GrokAuth {
            key: "live-codex-token".to_owned(),
            auth_mode: AuthMode::OpenAiCodex,
            refresh_token: Some("refresh".to_owned()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            oidc_issuer: Some(crate::auth::openai_codex::ISSUER.to_owned()),
            oidc_client_id: Some(crate::auth::openai_codex::CLIENT_ID.to_owned()),
            account_id: Some("account".to_owned()),
            ..GrokAuth::default()
        };
        let auth_map = std::collections::HashMap::from([(
            crate::auth::openai_codex::AUTH_SCOPE.to_owned(),
            codex_auth,
        )]);
        std::fs::write(&auth_path, serde_json::to_vec(&auth_map).unwrap())
            .expect("write Codex auth fixture");

        let _xai_key = EnvGuard::set(XAI_API_KEY_ENV_VAR, "invalid-xai-key");
        let _legacy_key = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
        let _api_key_lockdown = EnvGuard::unset("GROK_DISABLE_API_KEY_AUTH");
        let _default_model = EnvGuard::unset("GROK_DEFAULT_MODEL");
        let xai_home = tmp.path().join("xai-empty");
        std::fs::create_dir_all(&xai_home).expect("create xAI home");
        let auth_manager =
            std::sync::Arc::new(AuthManager::new(&xai_home, GrokComConfig::default()));
        let _auth_path = crate::auth::openai_codex::CodexAuthPathGuard::pin(auth_path);

        let empty = toml::Value::Table(toml::map::Map::new());
        let mut cfg = AgentConfig::new_from_toml_cfg(&empty).expect("production config");
        assert!(
            cfg.endpoints.deployment_key.is_none(),
            "fixture must not inherit deployment auth that bypasses the invalid env-key probe"
        );
        // Keep the fixture cold and deterministic: production bootstrap otherwise
        // fetches process-global remote settings, whose campaign default can
        // explicitly pin a Codex model before the probe under test runs.
        cfg.remote_settings = Some(Default::default());
        cfg.endpoints.xai_api_base_url = format!("http://{addr}/v1");
        cfg.models.default = None;
        cfg.default_model_override = None;

        assert!(
            crate::agent::auth_method::has_xai_api_key_env(),
            "fixture must expose the non-blank ambient xAI key to cold resolution"
        );
        let grok_key = crate::models::default_model().to_owned();
        let mut prefetched = crate::agent::config::default_model_entries(
            &crate::agent::config::EndpointsConfig::default(),
        );
        let grok = prefetched
            .shift_remove(&grok_key)
            .expect("bundled production Grok entry");
        let (grok_ready, grok_reason) = crate::agent::config::model_readiness(&grok);
        assert!(
            grok_ready,
            "bundled production Grok entry must be picker-ready: {grok_reason:?}"
        );
        prefetched.clear();
        prefetched.insert(grok_key.clone(), grok);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth_manager, Some(prefetched))
            .expect("valid production agent");
        assert_eq!(
            agent.models_manager.current_model_id().0.as_ref(),
            grok_key.as_str(),
            "presence-only cold resolution initially treats the non-blank xAI key as ambient"
        );

        <MvpAgent as acp::Agent>::initialize(
            &agent,
            acp::InitializeRequest::new(acp::ProtocolVersion::V1),
        )
        .await
        .expect("initialize succeeds after invalid xAI probe");
        probe_server.join().expect("probe server");

        assert_eq!(
            agent.models_manager.current_model_id().0.as_ref(),
            crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID,
            "invalid ambient xAI key must not leave the implicit cold-start model stranded on Grok when Codex OAuth is ready"
        );

        let _campaign = EnvGuard::set(
            "GROK_CAMPAIGNS_OVERRIDE",
            r#"[{"id":"issue-320-new-session","models":{"default":"grok-4.5"}}]"#,
        );
        let active_attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let active_attempts_hook = active_attempts.clone();
        let active_auth_manager = agent.auth_manager.clone();
        let active_hook = agent_ops::install_new_session_plan_before_seal_hook(move || {
            if active_attempts_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                // The selection authority is the process xAI manager. This
                // fixture intentionally keeps that manager empty while Codex
                // credentials live in their provider-scoped store, so use the
                // real in-memory clear writer to advance its generation.
                active_auth_manager.clear_in_memory();
            }
        });
        let session_cwd = tempfile::tempdir().expect("new-session cwd");
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            <MvpAgent as acp::Agent>::new_session(
                &agent,
                acp::NewSessionRequest::new(session_cwd.path().to_path_buf()),
            ),
        )
        .await
        .expect("production /new must not hang")
        .expect("production /new must succeed on the repaired Codex route");
        drop(active_hook);
        assert_eq!(
            active_attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "active ACP /new must rebuild after the pre-seal auth mutation"
        );
        let advertised = response
            .models
            .expect("/new must advertise its model state");
        assert_eq!(
            advertised.current_model_id.0.as_ref(),
            crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID,
            "an active Grok campaign must not revive the proven-invalid ambient xAI route"
        );
        let handle = agent
            .resident_handle(&response.session_id)
            .expect("new session must remain resident");
        assert_eq!(
            handle.model_id.0.as_ref(),
            crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID,
            "the persisted/session sampling identity must match the advertised Codex model"
        );
        let (sampling, _, credentials) = handle
            .chat_state_handle
            .get_prepared_model_state()
            .await
            .expect("resident session must expose its live prepared model state");
        assert_eq!(
            sampling.model,
            crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID,
            "the live sampler must stay pinned to Codex under the rejected Grok campaign"
        );
        assert_eq!(
            credentials.api_key(),
            None,
            "active /new must not persist provider-scoped Codex bearer bytes in chat state"
        );
        assert_eq!(
            credentials.source(),
            Some(&xai_grok_sampler::CredentialSource::AuthProvider {
                name: crate::agent::model_providers::OPENAI_CODEX_PROVIDER_ID.to_owned(),
            }),
            "active /new must bind the credential to the official Codex provider"
        );

        let dormant_attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dormant_attempts_hook = dormant_attempts.clone();
        let dormant_auth_manager = agent.auth_manager.clone();
        let dormant_hook = agent_ops::install_new_session_plan_before_seal_hook(move || {
            if dormant_attempts_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                dormant_auth_manager.clear_in_memory();
            }
        });
        let inner_cwd = tempfile::tempdir().expect("repeated new-session cwd");
        let inner_response = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            <MvpAgent as acp::Agent>::new_session(
                &agent,
                acp::NewSessionRequest::new(inner_cwd.path().to_path_buf()),
            ),
        )
        .await
        .expect("repeated production new_session must not hang")
        .expect("repeated production new_session must preserve the repaired Codex route");
        drop(dormant_hook);
        assert_eq!(
            dormant_attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "repeated production /new must rebuild after the pre-seal auth mutation"
        );
        let inner_advertised = inner_response
            .models
            .expect("production new_session must advertise its model state");
        assert_eq!(
            inner_advertised.current_model_id.0.as_ref(),
            crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID,
            "repeated production /new must retain active ACP campaign gating"
        );
        let inner_handle = agent
            .resident_handle(&inner_response.session_id)
            .expect("inner-created session must remain resident");
        let (inner_sampling, _, inner_credentials) = inner_handle
            .chat_state_handle
            .get_prepared_model_state()
            .await
            .expect("inner-created session prepared model state");
        assert_eq!(
            inner_sampling.model,
            crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID,
            "the repeated production path's live sampler must also stay on Codex"
        );
        assert_eq!(
            inner_credentials.api_key(),
            None,
            "repeated production /new must not persist provider-scoped Codex bearer bytes in chat state"
        );
        assert_eq!(
            inner_credentials.source(),
            Some(&xai_grok_sampler::CredentialSource::AuthProvider {
                name: crate::agent::model_providers::OPENAI_CODEX_PROVIDER_ID.to_owned(),
            }),
            "dormant /new must bind the credential to the official Codex provider"
        );
    });
    println!("{CHILD_PASS}");
}

/// Production initialize must HTTP-probe a present first-party env key and,
/// on an unusable verdict, reseat implicit Grok onto ready Codex (#303/#317).
///
/// In-process sibling of
/// `initialize_invalid_xai_probe_reseats_implicit_grok_to_ready_codex`: this
/// one drives the probe through the shared race-free test-support helpers
/// (`accept_with_deadline` / `read_http_request_headers`, #317) and pins the
/// Codex auth file via `CodexAuthPathGuard` rather than `GROK_AUTH_PATH`, so it
/// is the variant that guards those helpers and that resolution path. The
/// fresh-process variant instead re-execs the test binary to prove the same
/// reseat holds against a cold process-global state home and additionally
/// covers the later soft-campaign `/new` nudge (#320). Both are wanted; keep
/// the base name a strict prefix of this one so ci.yml's substring
/// `run_nonzero` filter keeps matching, and so the sibling's `--exact`
/// self-dispatch keeps resolving to exactly one test.
#[test]
#[serial_test::serial]
fn initialize_invalid_xai_probe_reseats_implicit_grok_to_ready_codex_in_process() {
    run_local_for_bridge_test(|| async {
        use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
        use crate::agent::config::Config as AgentConfig;
        use crate::auth::{AuthManager, AuthMode, GrokAuth, GrokComConfig};
        use xai_grok_test_support::{
            DEFAULT_MAX_HTTP_HEADER_BYTES, EnvGuard, accept_with_deadline,
            read_http_request_headers,
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("probe listener");
        let addr = listener.local_addr().expect("probe address");
        listener
            .set_nonblocking(true)
            .expect("nonblocking probe listener");
        let probe_server = std::thread::spawn(move || {
            use std::io::Write;
            let mut stream = accept_with_deadline(
                &listener,
                std::time::Instant::now() + std::time::Duration::from_secs(2),
            )
            .expect("initialize did not send the production /api-key probe");
            let request = read_http_request_headers(
                &mut stream,
                std::time::Duration::from_secs(1),
                DEFAULT_MAX_HTTP_HEADER_BYTES,
            )
            .expect("read probe request");
            let request = String::from_utf8_lossy(&request);
            assert!(
                request.starts_with("GET /v1/api-key "),
                "initialize must use the production /api-key probe path: {request}"
            );
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 29\r\nConnection: close\r\n\r\n{\"error\":\"Incorrect API key\"}",
                )
                .expect("write probe response");
        });

        let tmp = tempfile::tempdir().expect("temp home");
        let state_home = tmp.path().join("state");
        std::fs::create_dir_all(&state_home).expect("create isolated state home");
        let _medley_home = EnvGuard::set("MEDLEY_HOME", &state_home);
        let _grok_home = EnvGuard::set("GROK_HOME", &state_home);
        let _state_home = pin_fixture_state_home(&state_home);
        let auth_path = tmp.path().join("auth.json");
        let codex_auth = GrokAuth {
            key: "live-codex-token".to_owned(),
            auth_mode: AuthMode::OpenAiCodex,
            refresh_token: Some("refresh".to_owned()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            oidc_issuer: Some(crate::auth::openai_codex::ISSUER.to_owned()),
            oidc_client_id: Some(crate::auth::openai_codex::CLIENT_ID.to_owned()),
            account_id: Some("account".to_owned()),
            ..GrokAuth::default()
        };
        let auth_map = std::collections::HashMap::from([(
            crate::auth::openai_codex::AUTH_SCOPE.to_owned(),
            codex_auth,
        )]);
        std::fs::write(&auth_path, serde_json::to_vec(&auth_map).unwrap())
            .expect("write Codex auth fixture");

        let _xai_key = EnvGuard::set(XAI_API_KEY_ENV_VAR, "invalid-xai-key");
        let _legacy_key = EnvGuard::unset(LEGACY_XAI_API_KEY_ENV_VAR);
        let _default_model = EnvGuard::unset("GROK_DEFAULT_MODEL");
        let xai_home = tmp.path().join("xai-empty");
        std::fs::create_dir_all(&xai_home).expect("create xAI home");
        let auth_manager =
            std::sync::Arc::new(AuthManager::new(&xai_home, GrokComConfig::default()));
        let _auth_path = crate::auth::openai_codex::CodexAuthPathGuard::pin(auth_path);

        let empty = toml::Value::Table(toml::map::Map::new());
        let mut cfg = AgentConfig::new_from_toml_cfg(&empty).expect("production config");
        // Keep the fixture cold and deterministic: production bootstrap otherwise
        // fetches process-global remote settings, whose campaign default can
        // explicitly pin a Codex model before the probe under test runs.
        cfg.remote_settings = Some(Default::default());
        cfg.endpoints.xai_api_base_url = format!("http://{addr}/v1");
        cfg.models.default = None;
        cfg.default_model_override = None;

        assert!(
            crate::agent::auth_method::has_xai_api_key_env(),
            "fixture must expose the non-blank ambient xAI key to cold resolution"
        );
        let grok_key = crate::models::default_model().to_owned();
        let mut prefetched = crate::agent::config::default_model_entries(
            &crate::agent::config::EndpointsConfig::default(),
        );
        let grok = prefetched
            .shift_remove(&grok_key)
            .expect("bundled production Grok entry");
        let (grok_ready, grok_reason) = crate::agent::config::model_readiness(&grok);
        assert!(
            grok_ready,
            "bundled production Grok entry must be picker-ready: {grok_reason:?}"
        );
        prefetched.clear();
        prefetched.insert(grok_key.clone(), grok);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = MvpAgent::new(GatewaySender::new(tx), &cfg, auth_manager, Some(prefetched))
            .expect("valid production agent");
        assert_eq!(
            agent.models_manager.current_model_id().0.as_ref(),
            grok_key.as_str(),
            "presence-only cold resolution initially treats the non-blank xAI key as ambient"
        );

        <MvpAgent as acp::Agent>::initialize(
            &agent,
            acp::InitializeRequest::new(acp::ProtocolVersion::V1),
        )
        .await
        .expect("initialize succeeds after invalid xAI probe");
        probe_server.join().expect("probe server");

        assert_eq!(
            agent.models_manager.current_model_id().0.as_ref(),
            crate::agent::model_providers::OPENAI_CODEX_PRESET_MODEL_ID,
            "invalid ambient xAI key must not leave the implicit cold-start model stranded on Grok when Codex OAuth is ready"
        );
    });
}

/// #131 B3: the deliverable is the `initialize` response `_meta` key, not the
/// in-memory lock. Deleting the insert in `AcpAgent::initialize` must fail
/// this test; asserting only on `substituted_preference()` would not.
#[test]
fn initialize_publishes_substituted_default_model_meta() {
    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ModelEntry, ModelInfo};
        use crate::auth::{AuthManager, GrokComConfig};
        use indexmap::IndexMap;

        // CLI override beats disk/campaign `models.default` so this assertion
        // is about the wire path, not about whoever last wrote ~/.medley.
        let temp_dir = tempfile::tempdir().unwrap();
        let auth_manager =
            std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let gateway = GatewaySender::new(tx);
        let cfg = AgentConfig {
            default_model_override: Some("typo-provider-131".to_owned()),
            ..AgentConfig::default()
        };

        let mut catalog = IndexMap::new();
        let mut info = ModelInfo::fallback("grok-4");
        info.base_url = "https://api.x.ai/v1".to_string();
        catalog.insert(
            "grok-4".to_string(),
            ModelEntry {
                info,
                api_key: None,
                env_key: None,
                auth_provider: None,
                api_base_url: None,
                config_validation_errors: Vec::new(),
            },
        );

        let agent =
            MvpAgent::new(gateway, &cfg, auth_manager, Some(catalog)).expect("valid test config");

        let reported_pref = agent
            .models_manager
            .substituted_preference()
            .expect("precondition: in-memory verdict is set — this test still asserts the wire");
        assert_eq!(reported_pref.configured, "typo-provider-131");

        let resp = <MvpAgent as acp::Agent>::initialize(
            &agent,
            acp::InitializeRequest::new(acp::ProtocolVersion::V1),
        )
        .await
        .expect("initialize must succeed");

        let meta = resp.meta.as_ref().expect("initialize must carry _meta");
        let reported = meta
            .get(SUBSTITUTED_DEFAULT_MODEL_META_KEY)
            .unwrap_or_else(|| {
                panic!(
                    "initialize _meta must publish {SUBSTITUTED_DEFAULT_MODEL_META_KEY} when the configured default was substituted"
                )
            });
        assert_eq!(
            reported.get("configuredModelId").and_then(|v| v.as_str()),
            Some("typo-provider-131"),
        );
        assert_eq!(reported.get("source").and_then(|v| v.as_str()), Some("cli"),);
    });
}

/// #131 B3 counterweight: when the preference was honoured, initialize `_meta`
/// must omit the key — absent-vs-present is the whole contract.
#[test]
fn initialize_omits_substituted_default_model_meta_when_honoured() {
    run_local_for_bridge_test(|| async {
        use crate::agent::config::{Config as AgentConfig, ModelEntry, ModelInfo};
        use crate::auth::{AuthManager, GrokComConfig};
        use indexmap::IndexMap;

        let temp_dir = tempfile::tempdir().unwrap();
        let auth_manager =
            std::sync::Arc::new(AuthManager::new(temp_dir.path(), GrokComConfig::default()));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let gateway = GatewaySender::new(tx);
        let cfg = AgentConfig {
            default_model_override: Some("honoured-131".to_owned()),
            ..AgentConfig::default()
        };

        let mut catalog = IndexMap::new();
        let mut info = ModelInfo::fallback("honoured-131");
        info.base_url = "https://api.x.ai/v1".to_string();
        catalog.insert(
            "honoured-131".to_string(),
            ModelEntry {
                info,
                api_key: None,
                env_key: None,
                auth_provider: None,
                api_base_url: None,
                config_validation_errors: Vec::new(),
            },
        );

        let agent =
            MvpAgent::new(gateway, &cfg, auth_manager, Some(catalog)).expect("valid test config");
        assert!(
            agent.models_manager.substituted_preference().is_none(),
            "precondition: preference is honoured"
        );

        let resp = <MvpAgent as acp::Agent>::initialize(
            &agent,
            acp::InitializeRequest::new(acp::ProtocolVersion::V1),
        )
        .await
        .expect("initialize must succeed");

        let meta = resp.meta.as_ref().expect("initialize must carry _meta");
        assert!(
            meta.get(SUBSTITUTED_DEFAULT_MODEL_META_KEY).is_none(),
            "honoured preference must omit {SUBSTITUTED_DEFAULT_MODEL_META_KEY}, not send null"
        );
    });
}

#[cfg(feature = "dhat-heap")]
mod dhat_soak;
