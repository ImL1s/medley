use super::support::*;
use super::*;
use crate::auth::{AuthManager, AuthMode, GrokAuth, GrokComConfig};
use agent_client_protocol as acp;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::mpsc;

/// Test refresher that returns a fresh token and records that it
/// was invoked. Used to drive the auth-arm success path.
struct AlwaysSucceedRefresher {
    called: Arc<AtomicBool>,
}

struct CountingCodexRefresher {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::auth::refresh::TokenRefresher for CountingCodexRefresher {
    async fn refresh(
        &self,
        reason: crate::auth::refresh::RefreshReason,
    ) -> crate::auth::refresh::RefreshOutcome {
        assert_eq!(reason, crate::auth::refresh::RefreshReason::ServerRejected);
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        crate::auth::refresh::RefreshOutcome::Success(Box::new(GrokAuth {
            key: format!("refreshed-codex-token-{call}"),
            auth_mode: AuthMode::OpenAiCodex,
            refresh_token: Some(format!("codex-rt-{call}")),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            account_id: Some("acct-codex".to_owned()),
            ..GrokAuth::test_default()
        }))
    }
}
#[async_trait::async_trait]
impl crate::auth::refresh::TokenRefresher for AlwaysSucceedRefresher {
    async fn refresh(
        &self,
        _reason: crate::auth::refresh::RefreshReason,
    ) -> crate::auth::refresh::RefreshOutcome {
        self.called.store(true, Ordering::SeqCst);
        crate::auth::refresh::RefreshOutcome::Success(Box::new(GrokAuth {
            key: "refreshed-test-token".to_string(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-new".into()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..GrokAuth::test_default()
        }))
    }
}

/// `(tempdir, manager)` with an expired OIDC token loaded so
/// `unauthorized_recovery()` actually dispatches to the refresher.
/// Tempdir must outlive the manager (auth.json path).
fn auth_manager_with_refresher(
    refresher: Arc<dyn crate::auth::refresh::TokenRefresher>,
) -> (tempfile::TempDir, Arc<AuthManager>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
    am.hot_swap(GrokAuth {
        key: "initial-test-key".into(),
        auth_mode: AuthMode::Oidc,
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        ..GrokAuth::test_default()
    });
    am.set_refresher(refresher);
    (dir, am)
}

/// Build a `SamplingErrorInfo` of kind Auth - the same shape the
/// inner `OaiCompatClient` emit surfaces after recording its own
/// attribution.
fn auth_error() -> xai_grok_sampler::SamplingErrorInfo {
    xai_grok_sampler::SamplingErrorInfo {
        kind: xai_grok_sampler::SamplingErrorKind::Auth,
        message: "Unauthorized (401)".to_string(),
        status_code: Some(401),
        is_retryable: false,
        retry_after_secs: None,
        should_retry: None,
        model_metadata: None,
        empty_response_context: None,
        doom_loop_triggers: None,
        doom_loop_aborted_at_chunk: None,
        credential: xai_grok_sampling_types::SentCredential::Unknown,
    }
}

/// Construct a test actor with the supplied `auth_manager` and
/// session-token credentials wired in. Wraps the actor in `Arc`
/// ready for `handle_sampling_failure`.
async fn make_actor_with_auth_manager(
    auth_manager: Option<Arc<AuthManager>>,
) -> (Arc<SessionActor>, mpsc::UnboundedReceiver<PersistenceMsg>) {
    make_actor_with_auth_and_credentials(
        auth_manager,
        xai_chat_state::AuthType::SessionToken,
        "initial-test-key".to_string(),
    )
    .await
}

/// Variant that pins the credential `auth_type`; the `auth_method_id` is
/// derived from it. Use [`make_actor_with_method_and_credentials`] to pin the
/// two independently.
async fn make_actor_with_auth_and_credentials(
    auth_manager: Option<Arc<AuthManager>>,
    auth_type: xai_chat_state::AuthType,
    api_key: String,
) -> (Arc<SessionActor>, mpsc::UnboundedReceiver<PersistenceMsg>) {
    let method_id = match auth_type {
        xai_chat_state::AuthType::SessionToken => "cached_token",
        xai_chat_state::AuthType::ApiKey => "xai.api_key",
    };
    make_actor_with_method_and_credentials(auth_manager, method_id, auth_type, api_key).await
}

/// Pin the ACP `auth_method_id` and credential `auth_type` independently. The
/// gate keys off the stable `auth_method_id`, so this reproduces the regression:
/// a session method whose `creds.auth_type` has transiently collapsed to
/// `ApiKey` (session-token cache miss + `XAI_API_KEY`).
async fn make_actor_with_method_and_credentials(
    auth_manager: Option<Arc<AuthManager>>,
    auth_method_id: &str,
    auth_type: xai_chat_state::AuthType,
    api_key: String,
) -> (Arc<SessionActor>, mpsc::UnboundedReceiver<PersistenceMsg>) {
    let (gateway_tx, _) = mpsc::unbounded_channel();
    let (persistence_tx, persistence_rx) = mpsc::unbounded_channel();
    let mut actor = create_test_actor(50_000, 100_000, 85, gateway_tx, persistence_tx).await;
    actor.auth_manager = auth_manager;
    actor.auth_method_id = test_auth_method_id(auth_method_id);
    actor
        .chat_state_handle
        .update_credentials(xai_chat_state::Credentials::bound(
            Some(api_key),
            auth_type,
            match auth_type {
                xai_chat_state::AuthType::SessionToken => {
                    xai_grok_sampler::CredentialSource::XaiSession
                }
                xai_chat_state::AuthType::ApiKey => xai_grok_sampler::CredentialSource::ModelApiKey,
            },
        ));
    (Arc::new(actor), persistence_rx)
}

fn spawn_model_persistence_ack(mut persistence_rx: mpsc::UnboundedReceiver<PersistenceMsg>) {
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
}

/// Pin the fixture's synthetic model as a ready first-party bearer model.
/// `create_test_actor` intentionally uses a localhost endpoint and an
/// uncatalogued model, so production resolution otherwise classifies it as
/// unknown/custom and masks the session-method behavior these tests exercise.
async fn pin_first_party_session_model(actor: &SessionActor) {
    let cfg = actor.chat_state_handle.get_sampling_config().await;
    let model_id = cfg.as_ref().map(|c| c.model.clone()).unwrap_or_default();
    // Pin the *endpoint* too, not just the BYOK classification. Before #110 a
    // `NotByok` model skipped the endpoint check entirely, so pinning the
    // facts alone was enough to make this helper's name true. Now that the
    // gate consults the endpoint on every arm, the fixture's cleartext
    // localhost URL is precisely what "first party" excludes.
    if let Some(mut cfg) = cfg {
        cfg.base_url = "https://api.x.ai/v1".to_string();
        actor.chat_state_handle.update_sampling_config(cfg);
    }
    actor
        .model_auth_memo
        .replace(Some(crate::session::acp_session::ModelAuthMemo {
            model_id,
            facts: crate::agent::config::ModelAuthFacts {
                byok: crate::agent::auth_method::ModelByok::NotByok,
                auth_scheme: xai_grok_sampler::AuthScheme::Bearer,
                readiness: crate::agent::auth_method::ModelReadiness::Ready,
            },
            provider: None,
            catalog_generation: 0,
        }));
}

/// `(tempdir, manager)` holding a valid OIDC token (so `get_valid_token()` is a
/// cache hit). The tempdir must outlive the manager (auth.json path).
fn auth_manager_with_valid_token(key: &str) -> (tempfile::TempDir, Arc<AuthManager>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
    am.hot_swap(GrokAuth {
        key: key.into(),
        auth_mode: AuthMode::Oidc,
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        ..GrokAuth::test_default()
    });
    (dir, am)
}

/// Sub-case 1: no auth_manager -> falls through, no emit.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn no_emit_when_auth_manager_is_none() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_auth_manager(None).await;
            crate::auth::attribution::reset_test_emit_count();
            let _ = actor.handle_sampling_failure(auth_error()).await;
            assert_eq!(
                crate::auth::attribution::test_emit_count(),
                0,
                "auth arm must not emit attribution when no auth_manager is wired"
            );
        })
        .await;
}

/// Sub-case 2: no AuthManager → auth recovery is skipped entirely,
/// falls through to terminal error. Covers BYOK / API-key users
/// where no OIDC refresh is possible.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn no_recovery_without_auth_manager() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                xai_chat_state::AuthType::ApiKey,
                "xai-byok-key".to_string(),
            )
            .await;
            crate::auth::attribution::reset_test_emit_count();
            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                result.is_err(),
                "no auth manager must fall through to terminal error"
            );
            assert_eq!(
                crate::auth::attribution::test_emit_count(),
                0,
                "auth arm must not emit attribution without auth manager"
            );
        })
        .await;
}

/// Session-based auth + working refresher → RefreshAuthAndResubmit.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_recovery_returns_refresh_and_retry() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                        store: RecoveredStore::SessionToken,
                        ..
                    })
                ),
                "session-based auth with a working refresher must return RefreshAuthAndResubmit"
            );
            assert!(called.load(Ordering::SeqCst), "refresher must be invoked");
        })
        .await;
}

/// Regression: sampler 401 with API-key auth (BYOK `env_key` /
/// `XAI_API_KEY`) must NOT attempt an OIDC session-token refresh. The
/// bearer on the wire is the static API key, so refreshing the session
/// token reports success but the retry re-sends the same rejected key —
/// an invisible 401 loop that hangs the turn. Recovery is skipped and
/// the 401 surfaces as a terminal error.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn sampler_401_with_api_key_auth_skips_refresh_and_surfaces_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                Some(am),
                xai_chat_state::AuthType::ApiKey,
                "xai-byok-key".to_string(),
            )
            .await;

            let result = actor.handle_sampling_failure(auth_error()).await;

            assert!(
                result.is_err(),
                "API-key 401 must surface a terminal error, not retry"
            );
            assert!(
                !called.load(Ordering::SeqCst),
                "API-key 401 must NOT trigger an OIDC session-token refresh"
            );
        })
        .await;
}

/// Per-turn pre-flight refresh must not fire when `creds.auth_type` is
/// `ApiKey` (a BYOK model): the model's own API key must not be overwritten
/// by the session JWT.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_refresh_skips_api_key_auth_type() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                Some(am),
                xai_chat_state::AuthType::ApiKey,
                "byok-api-key".to_string(),
            )
            .await;
            actor.refresh_token_if_expired().await;
            assert!(
                !called.load(Ordering::SeqCst),
                "pre-flight refresh must NOT fire for ApiKey auth_type"
            );
            assert_eq!(
                actor.chat_state_handle.get_credentials().await.api_key(),
                Some("byok-api-key"),
                "BYOK api_key must not be overwritten by session token refresh"
            );
        })
        .await;
}

/// Hard-expired session token: pre-flight must call the refresher and must
/// not leave credentials stuck while pretending the JWT/config path applies.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_refreshes_hard_expired_session_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            assert!(
                !am.has_usable_token(),
                "precondition: access token is hard-expired"
            );

            let (actor, _rx) = make_actor_with_auth_manager(Some(am.clone())).await;
            actor.refresh_token_if_expired().await;

            assert!(
                called.load(Ordering::SeqCst),
                "pre-flight must invoke the refresher for a hard-expired session token"
            );
            assert_eq!(
                actor.chat_state_handle.get_credentials().await.api_key(),
                Some("refreshed-test-token"),
                "credentials must be updated to the refreshed bearer"
            );
            assert!(am.has_usable_token());
        })
        .await;
}

/// Hard-expired + failed refresh: do not fall through to JWT/config.toml;
/// strip the chat-state seed so default headers cannot carry a dead AT.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_hard_expired_refresh_failure_skips_jwt_fallthrough() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> = Arc::new({
                struct AlwaysFail(Arc<std::sync::atomic::AtomicU32>);
                #[async_trait::async_trait]
                impl crate::auth::refresh::TokenRefresher for AlwaysFail {
                    async fn refresh(
                        &self,
                        _: crate::auth::refresh::RefreshReason,
                    ) -> crate::auth::refresh::RefreshOutcome {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        crate::auth::refresh::RefreshOutcome::transient("refresh failed")
                    }
                }
                AlwaysFail(call_count.clone())
            });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_manager(Some(am.clone())).await;

            actor.refresh_token_if_expired().await;

            assert!(
                call_count.load(Ordering::SeqCst) >= 1,
                "pre-flight must attempt refresh"
            );
            assert_eq!(
                actor.chat_state_handle.get_credentials().await.api_key(),
                None,
                "hard-expired pre-flight failure must strip the chat-state seed"
            );
            assert!(
                !am.has_usable_token(),
                "token remains hard-expired after failed refresh"
            );
            assert!(
                am.permanent_failure().is_none(),
                "transient refresh failure must not poison permanent_failure"
            );
        })
        .await;
}

/// Soft-expired (early-invalidation buffer) + transient fail: retain the seed
/// so a still-accepted wire AT can continue until 401 recovery.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_soft_expired_transient_fail_retains_seed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> = Arc::new({
                struct AlwaysFail(Arc<std::sync::atomic::AtomicU32>);
                #[async_trait::async_trait]
                impl crate::auth::refresh::TokenRefresher for AlwaysFail {
                    async fn refresh(
                        &self,
                        _: crate::auth::refresh::RefreshReason,
                    ) -> crate::auth::refresh::RefreshOutcome {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        crate::auth::refresh::RefreshOutcome::transient("refresh failed")
                    }
                }
                AlwaysFail(call_count.clone())
            });
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            // Inside the early-invalidation buffer but still hard-valid.
            am.hot_swap(GrokAuth {
                key: "buffered-test-key".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::seconds(30)),
                ..GrokAuth::test_default()
            });
            am.set_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                Some(am.clone()),
                xai_chat_state::AuthType::SessionToken,
                "buffered-test-key".to_string(),
            )
            .await;

            actor.refresh_token_if_expired().await;

            assert!(
                call_count.load(Ordering::SeqCst) >= 1,
                "soft-expired pre-flight must still attempt refresh"
            );
            assert_eq!(
                actor.chat_state_handle.get_credentials().await.api_key(),
                Some("buffered-test-key"),
                "buffer-window soft-expired + transient fail must retain seed"
            );
            assert!(
                am.has_usable_token(),
                "token inside hard-expiry buffer remains usable"
            );
        })
        .await;
}

/// Proactive refresh keeps the cache hot so `refresh_token_if_expired`
/// (per-turn pre-flight) is a cache hit — the refresher fires once
/// (proactive), then the per-turn call sees the fresh token without
/// hitting the IdP again.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn proactive_refresh_makes_per_turn_refresh_a_cache_hit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> = Arc::new({
                struct Counting(Arc<std::sync::atomic::AtomicU32>);
                #[async_trait::async_trait]
                impl crate::auth::refresh::TokenRefresher for Counting {
                    async fn refresh(
                        &self,
                        _: crate::auth::refresh::RefreshReason,
                    ) -> crate::auth::refresh::RefreshOutcome {
                        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        crate::auth::refresh::RefreshOutcome::Success(Box::new(GrokAuth {
                            key: "proactive-fresh".into(),
                            auth_mode: AuthMode::Oidc,
                            refresh_token: Some("rt-new".into()),
                            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                            ..GrokAuth::test_default()
                        }))
                    }
                }
                Counting(call_count.clone())
            });

            let (_dir, am) = auth_manager_with_refresher(refresher);
            let cancel = tokio_util::sync::CancellationToken::new();
            am.start_proactive_refresh(cancel.clone());

            // Wait for the proactive task to fire; its first pass runs after
            // PROACTIVE_MIN_SLEEP, so the window must exceed the floor.
            tokio::time::sleep(
                crate::auth::manager::PROACTIVE_MIN_SLEEP + std::time::Duration::from_millis(1000),
            )
            .await;
            assert!(
                call_count.load(Ordering::SeqCst) >= 1,
                "proactive task must have fired"
            );
            let count_after_proactive = call_count.load(Ordering::SeqCst);

            // Now run refresh_token_if_expired (the per-turn pre-flight).
            // It should see the proactively-refreshed token and NOT invoke
            // the refresher again.
            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            actor.refresh_token_if_expired().await;

            assert_eq!(
                call_count.load(Ordering::SeqCst),
                count_after_proactive,
                "per-turn refresh must NOT call the refresher again (cache hit)"
            );
            assert_eq!(
                actor.chat_state_handle.get_credentials().await.api_key(),
                Some("proactive-fresh"),
                "per-turn refresh must pick up the proactively-refreshed token"
            );

            cancel.cancel();
        })
        .await;
}

fn model_not_found_error() -> xai_grok_sampler::SamplingErrorInfo {
    xai_grok_sampler::SamplingErrorInfo {
            kind: xai_grok_sampler::SamplingErrorKind::Api,
            message: "API error (status 404 Not Found): The model grok-build does not exist or your team does not have access".into(),
            status_code: Some(404),
            is_retryable: false,
            retry_after_secs: None,
            should_retry: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: xai_grok_sampling_types::SentCredential::Unknown,
        }
}

/// 404 model-not-found with a legacy WebLogin token appends a
/// "Legacy auth detected" hint to the error message.
#[tokio::test(flavor = "current_thread")]
async fn legacy_auth_hint_on_404_model_not_found() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            am.hot_swap(GrokAuth {
                key: "legacy-token".into(),
                auth_mode: AuthMode::WebLogin,
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor.handle_sampling_failure(model_not_found_error()).await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data.as_str().unwrap();
            assert!(
                msg.contains("deprecated authentication method"),
                "404 with WebLogin must include deprecation message, got: {msg}"
            );
            let prog = xai_grok_config::program_name::program_name_for_instruction()
                .expect("test binary argv0 is a plain program name");
            assert!(
                msg.contains(&format!("`{prog} logout`")),
                "hint must mention `{prog} logout`, got: {msg}"
            );
            assert!(
                msg.contains(&format!("`{prog} login`")),
                "hint must mention `{prog} login`, got: {msg}"
            );
            assert!(
                msg.contains("Version:"),
                "must show client version, got: {msg}"
            );
        })
        .await;
}

/// Build a 401-shaped error that bypasses step 4b's auth recovery.
///
/// In production, 401s arrive as `SamplingErrorKind::Auth` with
/// `status_code: None`. Step 4b intercepts `Auth`-kind errors and
/// runs the full recovery chain — which succeeds on devbox/CI
/// environments via SA-token mint, masking the hint.
///
/// Using `Api` kind + `status_code: Some(401)` exercises the hint
/// condition (`status_code == Some(401)`) without triggering
/// recovery, making the test environment-independent.
fn unauthorized_401_error() -> xai_grok_sampler::SamplingErrorInfo {
    xai_grok_sampler::SamplingErrorInfo {
            kind: xai_grok_sampler::SamplingErrorKind::Api,
            message: "Unauthorized (401) from https://cli-chat-proxy.grok.com/v1/responses: {\"error\":\"Invalid or expired credentials (auth_kind=bearer, x_xai_token_auth=xai-grok-cli, upstream=Unauthenticated, reason=no auth context)\"}".into(),
            status_code: Some(401),
            is_retryable: false,
            retry_after_secs: None,
            should_retry: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: xai_grok_sampling_types::SentCredential::Unknown,
        }
}

/// 401 Unauthorized with a legacy WebLogin token appends a
/// "Legacy auth detected" hint to the error message.
#[tokio::test(flavor = "current_thread")]
async fn legacy_auth_hint_on_401_unauthorized() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            am.hot_swap(GrokAuth {
                key: "legacy-token".into(),
                auth_mode: AuthMode::WebLogin,
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(unauthorized_401_error())
                .await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data.as_str().unwrap();
            assert!(
                msg.contains("deprecated authentication method"),
                "401 with WebLogin must include deprecation message, got: {msg}"
            );
            let prog = xai_grok_config::program_name::program_name_for_instruction()
                .expect("test binary argv0 is a plain program name");
            assert!(
                msg.contains(&format!("`{prog} logout`")),
                "hint must mention `{prog} logout`, got: {msg}"
            );
            assert!(
                msg.contains(&format!("`{prog} login`")),
                "hint must mention `{prog} login`, got: {msg}"
            );
        })
        .await;
}

/// 401 with OIDC auth must NOT append the legacy hint.
#[tokio::test(flavor = "current_thread")]
async fn no_legacy_hint_on_401_for_oidc_auth() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            am.hot_swap(GrokAuth {
                key: "oidc-token".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(unauthorized_401_error())
                .await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| data.as_str())
                .unwrap();
            assert!(
                !msg.contains("deprecated authentication method"),
                "OIDC auth must NOT trigger WebLogin deprecation on 401, got: {msg}"
            );
            assert!(
                msg.contains("Auth:      Oidc"),
                "OIDC 401 must show auth mode in enriched message, got: {msg}"
            );
        })
        .await;
}

/// 404 model-not-found with OIDC auth must NOT append the legacy hint.
#[tokio::test(flavor = "current_thread")]
async fn no_legacy_hint_for_oidc_auth() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            am.hot_swap(GrokAuth {
                key: "oidc-token".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor.handle_sampling_failure(model_not_found_error()).await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| data.as_str())
                .unwrap();
            assert!(
                !msg.contains("deprecated authentication method"),
                "OIDC auth must NOT trigger WebLogin deprecation, got: {msg}"
            );
            assert!(
                msg.contains("Auth:      Oidc"),
                "OIDC 404 must show auth mode in enriched message, got: {msg}"
            );
            assert!(
                msg.contains("Version:"),
                "OIDC 404 must show version in enriched message, got: {msg}"
            );
        })
        .await;
}

// Regression group: a live session whose `auth_type` transiently reads `ApiKey`
// must still recover, because the gate keys off the stable `auth_method_id`.
#[test]
fn session_token_auth_gate_truth_table() {
    use crate::agent::auth_method::{ModelByok, session_token_auth_gate as gate};
    // Non-session methods never refresh, regardless of BYOK status or endpoint.
    for fp in [false, true] {
        assert!(!gate(false, ModelByok::NotByok, fp));
        assert!(!gate(false, ModelByok::Byok, fp));
        assert!(!gate(false, ModelByok::Unknown, fp));
        // Session method: a genuine per-model Byok never refreshes, on any
        // endpoint.
        assert!(!gate(true, ModelByok::Byok, fp));
    }
    // Session method + a model carrying no credential of its own: refresh only
    // against a first-party host. `NotByok` used to ignore the endpoint on the
    // reasoning that it "only ever routes to the session endpoint" — but it
    // says nothing about where `base_url` points, and a catalog model with an
    // overridden endpoint is `NotByok` and third-party at the same time. This
    // arm was unconditionally `true` pre-fix (#110).
    assert!(gate(true, ModelByok::NotByok, true));
    assert!(!gate(true, ModelByok::NotByok, false));
    // Session method + Unknown BYOK: refresh only against a first-party xAI
    // host, so a transiently-unclassifiable config can't demote a live session
    // (the stale-token 401 regression) yet the session token never leaks to a
    // third-party BYOK endpoint. This arm was unconditionally `false` pre-fix.
    assert!(gate(true, ModelByok::Unknown, true));
    assert!(!gate(true, ModelByok::Unknown, false));
}

/// Pre-fix, the gate read `auth_type` and skipped recovery here, 401'ing every
/// turn until restart.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_session_method_with_stale_api_key_auth_type_still_recovers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            let result = actor.handle_sampling_failure(auth_error()).await;

            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "session-based method must recover even when auth_type transiently reads ApiKey"
            );
            assert!(
                called.load(Ordering::SeqCst),
                "the OIDC refresher must be invoked for a session-based method"
            );
        })
        .await;
}

/// A 401 for a credential the session does not own must surface, not trigger
/// session recovery.
///
/// Thirteenth #110 review finding. `sampling_config_for_model` already declined
/// to wire the session bearer resolver for a model authenticated by a
/// user-declared header, but this path asked the gate directly, so a provider
/// rejecting the *user's* key refreshed the xAI session and retried with the
/// same rejected header — hiding the real failure behind a retry.
///
/// Asserted as a pair: without the control case, a fixture that never reaches
/// the gate at all would satisfy the negative assertion and prove nothing.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_does_not_session_recover_a_user_declared_credential_header() {
    async fn recovers(declared_header: Option<(&str, &str)>) -> bool {
        let called = Arc::new(AtomicBool::new(false));
        let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
            Arc::new(AlwaysSucceedRefresher {
                called: called.clone(),
            });
        let (_dir, am) = auth_manager_with_refresher(refresher);
        let (actor, _rx) = make_actor_with_method_and_credentials(
            Some(am),
            "cached_token",
            xai_chat_state::AuthType::ApiKey,
            "session-jwt".to_string(),
        )
        .await;
        pin_first_party_session_model(&actor).await;
        if let Some((name, value)) = declared_header {
            let mut cfg = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("fixture sampling config");
            cfg.extra_headers
                .insert(name.to_string(), value.to_string());
            actor.chat_state_handle.update_sampling_config(cfg);
        }
        matches!(
            actor.handle_sampling_failure(auth_error()).await,
            Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
        )
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            assert!(
                recovers(None).await,
                "control: a first-party session model with no declared header must still \
                 recover, or the negative case below is vacuous"
            );
            assert!(
                !recovers(Some(("Authorization", "Bearer user-provider-key"))).await,
                "a 401 rejecting the user's own credential header must be surfaced, not \
                 answered by refreshing the xAI session and retrying the same header"
            );
        })
        .await;
}

/// Same regression via the `oidc` method id (the other session-based variant).
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_oidc_method_with_stale_api_key_auth_type_still_recovers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "oidc",
                xai_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            let result = actor.handle_sampling_failure(auth_error()).await;

            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "oidc method must recover even when auth_type transiently reads ApiKey"
            );
            assert!(
                called.load(Ordering::SeqCst),
                "the OIDC refresher must be invoked"
            );
        })
        .await;
}

/// #136 steps 2–3: a Ready session-token turn must re-emit the provenance
/// step 1 bound onto chat-state credentials. Header re-derivation alone
/// yields `None` on every ordinary turn (chat state keeps headers, not the
/// label); without the stored fall-back L3 sees an unlabelled ambient key.
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_ready_session_token_carries_stored_source() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "session-jwt".to_string(),
            )
            .await;
            pin_first_party_session_model(&actor).await;

            let cfg = actor.reconstruct_full_config().await;

            assert_eq!(
                cfg.credential_source,
                Some(xai_grok_sampler::CredentialSource::XaiSession),
                "Ready session-token turn must surface the stored provenance, not None"
            );
            assert!(
                cfg.api_key.is_some() || cfg.bearer_resolver.is_some(),
                "the live session credential must still be on the wire config"
            );
        })
        .await;
}

/// #136 steps 2–3: a Ready BYOK turn must re-emit `ModelApiKey` from the
/// stored source. Same hole as the session-token case — header maps alone
/// cannot recover a non-header credential's provenance.
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_ready_byok_carries_stored_source() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "xai.api_key",
                xai_chat_state::AuthType::ApiKey,
                "byok-model-key".to_string(),
            )
            .await;
            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::Byok,
                        auth_scheme: xai_grok_sampler::AuthScheme::Bearer,
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            let cfg = actor.reconstruct_full_config().await;

            assert_eq!(
                cfg.credential_source,
                Some(xai_grok_sampler::CredentialSource::ModelApiKey),
                "Ready BYOK turn must surface the stored provenance, not None"
            );
            assert_eq!(cfg.api_key.as_deref(), Some("byok-model-key"));
            assert!(cfg.bearer_resolver.is_none());
        })
        .await;
}

/// #180 seam: Ready dual-auth gateway — model owns an `api_key` *and* a
/// declared credential header still sits in the maps. Reconstruction must
/// keep the stored `ModelApiKey` label (bound with the secret), not invent
/// `ExplicitHeader` from the maps while leaving `api_key` set.
///
/// That invented pair is what L3 treats as ambient and refuses on External;
/// inventing it at this seam is how a legitimate gateway route died for
/// users. The sampler-side `dual_auth_gateway_…` constructs
/// `SamplingClient::new` directly and never reaches this function, so it
/// stays green under the header-preference mutation at
/// `sampler_turn.rs` Ready-path provenance.
///
/// Also asserts the reconstructed `SamplingConfig` is one
/// `SamplingClient::new` accepts — coverage through the crate boundary.
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_ready_dual_auth_keeps_model_api_key_not_explicit_header() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    const MODEL_KEY: &str = "sk-upstream-byok";
    const EDGE_HEADER: &str = "x-api-key";
    const EDGE_VALUE: &str = "sk-gateway-edge";
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "xai.api_key",
                xai_chat_state::AuthType::ApiKey,
                MODEL_KEY.to_string(),
            )
            .await;
            let mut cfg = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor sampling config");
            cfg.base_url = "https://gateway.example/v1".to_string();
            cfg.endpoint_trust = Some(xai_grok_sampler::EndpointTrustClass::External);
            cfg.extra_headers
                .insert(EDGE_HEADER.to_string(), EDGE_VALUE.to_string());
            let model = cfg.model.clone();
            actor.chat_state_handle.update_sampling_config(cfg);
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::Byok,
                        auth_scheme: xai_grok_sampler::AuthScheme::Bearer,
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                matches!(
                    cfg.credential_source,
                    Some(xai_grok_sampler::CredentialSource::ModelApiKey)
                ),
                "Ready dual-auth reconstruct must keep stored ModelApiKey; \
                 inventing ExplicitHeader from the maps while api_key remains \
                 is the #180 L3 false-refuse. got={:?}",
                cfg.credential_source
            );
            assert!(
                !matches!(
                    cfg.credential_source,
                    Some(xai_grok_sampler::CredentialSource::ExplicitHeader { .. })
                ),
                "Ready dual-auth reconstruct must not invent ExplicitHeader \
                 from the header maps. (Value withheld.)"
            );
            assert!(
                cfg.api_key.as_deref() == Some(MODEL_KEY),
                "Ready dual-auth reconstruct must keep the model-owned api_key. \
                 (Value withheld.)"
            );
            assert!(
                cfg.extra_headers
                    .get(EDGE_HEADER)
                    .is_some_and(|v| v.as_str() == EDGE_VALUE),
                "declared gateway edge header must still ship in extra_headers. \
                 (Value withheld.)"
            );

            xai_grok_sampler::SamplingClient::new(cfg).expect(
                "dual-auth gateway with stored ModelApiKey must construct on \
                 an external origin; re-labelling ExplicitHeader while keeping \
                 api_key is the #180 L3 false-refuse",
            );
        })
        .await;
}

/// Without the live bearer resolver here the sampler would sign requests with
/// the stale buffered token.
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_wires_bearer_resolver_for_session_method_despite_api_key_auth_type()
 {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("fresh-session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;
            pin_first_party_session_model(&actor).await;

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_some(),
                "session-based method must use the live bearer resolver, not the buffered key"
            );
        })
        .await;
}

/// Negative: a genuine `xai.api_key` method keeps its configured key on the
/// wire (no live resolver).
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_no_bearer_resolver_for_api_key_method() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "xai.api_key",
                xai_chat_state::AuthType::ApiKey,
                "xai-static-key".to_string(),
            )
            .await;

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_none(),
                "api-key method must keep its configured bearer (no live resolver)"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn codex_401_forces_one_refresh_one_retry_then_second_401_is_terminal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let manager = Arc::new(AuthManager::new_openai_codex(dir.path()));
            manager.hot_swap(GrokAuth {
                key: "rejected-codex-token".to_owned(),
                auth_mode: AuthMode::OpenAiCodex,
                refresh_token: Some("codex-rt-0".to_owned()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                account_id: Some("acct-codex".to_owned()),
                ..GrokAuth::test_default()
            });
            let refresh_calls = Arc::new(AtomicUsize::new(0));
            manager.set_refresher(Arc::new(CountingCodexRefresher {
                calls: refresh_calls.clone(),
            }));
            let provider = crate::auth::AuthProviderRef::openai_codex(manager);

            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "openai.codex",
                xai_chat_state::AuthType::ApiKey,
                "rejected-codex-token".to_owned(),
            )
            .await;
            let mut cfg = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor sampling config");
            cfg.api_backend = xai_grok_sampling_types::ApiBackend::CodexResponses;
            let model_id = cfg.model.clone();
            actor.chat_state_handle.update_sampling_config(cfg);
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id,
                    facts: crate::agent::config::ModelAuthFacts {
                        byok: crate::agent::auth_method::ModelByok::NotByok,
                        auth_scheme: xai_grok_sampler::AuthScheme::Bearer,
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: Some(provider),
                    catalog_generation: 0,
                }));

            let reconstructed = actor.reconstruct_full_config().await;
            let credential = reconstructed
                .bearer_resolver
                .as_ref()
                .and_then(|resolver| resolver.current_credential())
                .expect("Codex config uses the provider's structured resolver");
            assert_eq!(credential.access_token, "rejected-codex-token");
            assert_eq!(credential.account_id.as_deref(), Some("acct-codex"));

            let first = actor
                .handle_sampling_failure_with_codex_retry_policy(auth_error(), true)
                .await;
            assert!(matches!(
                first,
                Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                    store: RecoveredStore::AuthProvider,
                    ..
                })
            ));
            assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);

            let second = actor
                .handle_sampling_failure_with_codex_retry_policy(auth_error(), false)
                .await;
            assert!(second.is_err(), "the retried 401 must terminate the turn");
            assert_eq!(
                refresh_calls.load(Ordering::SeqCst),
                1,
                "the second 401 must not trigger another refresh"
            );
        })
        .await;
}

/// The pre-flight refresh heals a transiently-`ApiKey` session by writing the
/// fresh session token back into `creds.api_key`.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_refresh_heals_session_method_with_stale_api_key_auth_type() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("fresh-session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            actor.refresh_token_if_expired().await;

            assert_eq!(
                actor.chat_state_handle.get_credentials().await.api_key(),
                Some("fresh-session-token"),
                "session-based pre-flight refresh must heal a stale api_key with the live token"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.auth_type(),
                xai_chat_state::AuthType::SessionToken,
                "the refresh must heal the stale ApiKey auth_type, not just the secret"
            );
            assert_eq!(
                creds.source_cloned(),
                Some(xai_grok_sampler::CredentialSource::XaiSession),
                "an ambient session token must not keep a ModelApiKey label"
            );
        })
        .await;
}

/// End-to-end for the frozen-gate bug: a session born on `xai.api_key` (gate
/// inactive) must adopt a later OIDC `/login` on the SAME actor -- the shared
/// `auth_method_id` handle is flipped in place (no re-spawn), so the next turn
/// wires the live bearer resolver and heals the stale key.
#[tokio::test(flavor = "current_thread")]
async fn session_born_on_api_key_recovers_after_oidc_login_without_restart() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("fresh-oidc-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "xai.api_key",
                xai_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;
            pin_first_party_session_model(&actor).await;

            // Born on api_key: the gate is inactive, so no live resolver.
            assert!(
                actor
                    .reconstruct_full_config()
                    .await
                    .bearer_resolver
                    .is_none(),
                "api-key session must not use the live resolver before login"
            );

            // Simulate the agent's `authenticate` publishing an OIDC method into
            // the shared handle this running actor already holds (no re-spawn).
            actor
                .auth_method_id
                .store(Some(std::sync::Arc::new(acp::AuthMethodId::new("oidc"))));

            // The gate is recomputed each turn from the shared handle, so the
            // flip alone activates the live resolver on the very next turn --
            // no re-spawn, before any token refresh runs.
            assert!(
                actor
                    .reconstruct_full_config()
                    .await
                    .bearer_resolver
                    .is_some(),
                "flipping the shared handle activates the resolver on the next turn"
            );

            // The pre-flight refresh then heals the stale api_key with the live token.
            actor.refresh_token_if_expired().await;
            assert_eq!(
                actor.chat_state_handle.get_credentials().await.api_key(),
                Some("fresh-oidc-token"),
                "the stale api_key must be healed with the fresh OIDC token"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.auth_type(),
                xai_chat_state::AuthType::SessionToken,
                "the refresh must heal the stale ApiKey auth_type, not just the secret"
            );
            assert_eq!(
                creds.source_cloned(),
                Some(xai_grok_sampler::CredentialSource::XaiSession),
                "an ambient session token must not keep a ModelApiKey label"
            );
            assert_eq!(
                actor.reconstruct_full_config().await.credential_source,
                Some(xai_grok_sampler::CredentialSource::XaiSession),
            );
        })
        .await;
}

// Per-model BYOK memo (`SessionActor::model_auth_memo`): a definite cached
// status is served without recomputing, and the memo keys on `model_id`.

/// The cache-hit branch is what lets a later config parse failure (`Unknown`)
/// fall back to the last-known-good status.
#[tokio::test(flavor = "current_thread")]
async fn model_auth_memo_serves_cached_status_and_keys_on_model() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "k".to_string(),
            )
            .await;

            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: "model-a".to_string(),
                    facts: ModelAuthFacts {
                        byok: ModelByok::Byok,
                        auth_scheme: Default::default(),
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            // Cache hit: served without consulting config.
            assert_eq!(actor.model_auth_facts("model-a").byok, ModelByok::Byok);

            // Different model re-resolves rather than serving the stale `Byok`.
            assert_ne!(actor.model_auth_facts("model-b").byok, ModelByok::Byok);
        })
        .await;
}

/// A session method whose active model is a genuine per-model BYOK model keeps
/// the model's own key on the wire (no live resolver).
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_no_bearer_resolver_for_byok_model_on_session_method() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "byok-key".to_string(),
            )
            .await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::Byok,
                        auth_scheme: Default::default(),
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_none(),
                "a per-model BYOK model must keep its own key even on a session method"
            );
        })
        .await;
}

/// Session-based ACP method + `AuthScheme::None` must never attach the live
/// session bearer resolver — even if BYOK classification would otherwise keep
/// the gate active (model switch after OIDC login).
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_no_bearer_resolver_for_none_auth_scheme_on_session_method() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "stale-session-jwt".to_string(),
            )
            .await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        // NotByok keeps session_token_auth_gate active on a
                        // session method — the AuthScheme::None conjunction
                        // is what must suppress the resolver.
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::None,
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_none(),
                "AuthScheme::None must never attach the session bearer resolver"
            );
            assert_eq!(cfg.auth_scheme, AuthScheme::None);
            assert!(
                cfg.api_key.is_none(),
                "AuthScheme::None must strip stale chat-state session credentials"
            );
        })
        .await;
}

/// Unready catalog entries (missing BYOK / invalid auth_scheme) must strip at
/// turn-time reconstruct even when memo still claims Bearer + NotByok — the
/// final wire choke point cannot reattach ambient session credentials.
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_strips_credentials_when_model_not_ready() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "stale-session-jwt".to_string(),
            )
            .await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::Bearer,
                        readiness: crate::agent::auth_method::ModelReadiness::Unusable(
                            crate::agent::auth_method::UnusableReason("model is not ready".into()),
                        ),
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_none(),
                "unready model must never attach the session bearer resolver"
            );
            assert_eq!(cfg.auth_scheme, AuthScheme::None);
            assert!(cfg.api_key.is_none());
            assert!(cfg.user_id.is_none());
            assert!(cfg.deployment_id.is_none());
            assert_eq!(
                cfg.credential_source,
                Some(xai_grok_sampler::CredentialSource::Missing),
                "Unusable must label the gap Missing so a refusal can key on it"
            );
        })
        .await;
}

/// #133 load-bearing: refusal keys on `Unusable` alone. An `Unknown`
/// (uncatalogued) model must still prepare; a catalogued-unusable model on a
/// non-first-party origin must fail locally naming the reason. If this is
/// weakened back to treating Unknown like Unusable, the Unknown arm fails.
#[tokio::test(flavor = "current_thread")]
async fn prepare_refuses_unusable_external_but_allows_unknown() {
    use crate::agent::auth_method::{ModelByok, ModelReadiness, UnknownReason, UnusableReason};
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "stale-session-jwt".to_string(),
            )
            .await;

            let mut cfg = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("sampling config");
            cfg.base_url = "https://vendor.example/v1".to_string();
            cfg.endpoint_trust = Some(xai_grok_sampler::EndpointTrustClass::External);
            let model = cfg.model.clone();
            actor.chat_state_handle.update_sampling_config(cfg);

            // Unknown (uncatalogued): must still prepare.
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model.clone(),
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::Bearer,
                        readiness: ModelReadiness::Unknown(UnknownReason::NotInCatalog),
                    },
                    provider: None,
                    catalog_generation: 0,
                }));
            actor
                .prepare_chat_completion(false)
                .await
                .expect("Unknown (uncatalogued) must not be refused");

            // Unusable on a non-first-party origin: must refuse naming the reason.
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::Bearer,
                        readiness: ModelReadiness::Unusable(UnusableReason(
                            "invalid auth_scheme `not-a-scheme`".into(),
                        )),
                    },
                    provider: None,
                    catalog_generation: 0,
                }));
            let err = actor
                .prepare_chat_completion(false)
                .await
                .expect_err("Unusable external must be refused");
            let msg = format!("{err:?}");
            assert!(
                msg.contains("not ready") && msg.contains("invalid auth_scheme"),
                "refusal must name the model readiness reason, got: {msg}"
            );
        })
        .await;
}

/// The turn path owes the client a terminal report; the auxiliary path does not.
///
/// `run_turn_via_sampler` documents that every `Err` it returns has already
/// been reported via `RetryState::Failed`, so the refusal added for #133 has to
/// send one before propagating. `prepare_chat_completion` must **not**: it
/// serves compaction, goals, memory-dream and the laziness classifier, and
/// announcing a failed turn for background work would be a lie.
///
/// The existing refusal test drives only `prepare_chat_completion` and asserts
/// on the error text, so neither half of this was covered.
#[tokio::test(flavor = "current_thread")]
async fn an_unusable_route_reports_on_the_turn_path_and_stays_quiet_on_the_aux_path() {
    use crate::agent::auth_method::{ModelByok, ModelReadiness, UnusableReason};
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, mut rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "stale-session-jwt".to_string(),
            )
            .await;

            let mut cfg = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("sampling config");
            cfg.base_url = "https://vendor.example/v1".to_string();
            cfg.endpoint_trust = Some(xai_grok_sampler::EndpointTrustClass::External);
            let model = cfg.model.clone();
            actor.chat_state_handle.update_sampling_config(cfg);
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::Bearer,
                        readiness: ModelReadiness::Unusable(UnusableReason(
                            "invalid auth_scheme `not-a-scheme`".into(),
                        )),
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            // `RetryState` reaches the client through the persistence channel
            // as a session update, which is how the compaction tests observe it.
            let drain =
                |rx: &mut mpsc::UnboundedReceiver<crate::session::persistence::PersistenceMsg>| {
                    let mut seen: Vec<(String, String)> = Vec::new();
                    while let Ok(msg) = rx.try_recv() {
                        if let crate::session::persistence::PersistenceMsg::Update(
                            crate::session::storage::SessionUpdate::Xai(notif),
                        ) = msg
                            && let crate::extensions::notification::SessionUpdate::RetryState(
                                crate::extensions::notification::RetryState::Failed {
                                    error_type,
                                    message,
                                },
                            ) = &notif.update
                        {
                            seen.push((error_type.clone(), message.clone()));
                        }
                    }
                    seen
                };
            // Anything queued during actor construction is not ours.
            drain(&mut rx);

            actor
                .prepare_sampler_for_turn()
                .await
                .expect_err("an unusable external route must refuse");
            let after_turn = drain(&mut rx);
            assert!(
                after_turn
                    .iter()
                    .any(|(t, m)| t == "model_not_ready" && m.contains("invalid auth_scheme")),
                "the turn path owes one RetryState::Failed naming the reason; got {after_turn:?}"
            );
            assert!(
                !after_turn.iter().any(|(t, _)| t == "auth"),
                "must not classify as auth: an unusable model config is not an \
                 expired login, and `auth` is what raises the re-auth prompt; \
                 got {after_turn:?}"
            );

            actor
                .prepare_chat_completion(false)
                .await
                .expect_err("the aux path must refuse too");
            let after_aux = drain(&mut rx);
            assert!(
                after_aux.is_empty(),
                "compaction, goals and memory-dream are not turns; reporting a \
                 turn failure for them would be a lie; got {after_aux:?}"
            );
        })
        .await;
}

/// A model the catalog does not have must reach the wire with no credential,
/// **whatever the ACP auth method is**.
///
/// This is the test the first version of the tri-state did not have, and the
/// gap was not incidental: its sibling builds the actor with `cached_token`,
/// which is session-based, so the arm under test took a different branch and
/// the loosening was never exercised. Here the method is `xai_api_key`, which
/// is not session-based -- the case where the chat-state key was being retained
/// and sent on to whatever `base_url` the session held.
#[tokio::test(flavor = "current_thread")]
async fn absent_from_catalog_strips_the_credential_for_every_auth_method() {
    use crate::agent::auth_method::{ModelByok, ModelReadiness, UnknownReason};
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for method in ["xai_api_key", "cached_token", "not-a-known-method"] {
                let (_dir, am) = auth_manager_with_valid_token("session-token");
                let (actor, _rx) = make_actor_with_method_and_credentials(
                    Some(am),
                    method,
                    xai_chat_state::AuthType::ApiKey,
                    "chat-state-key".to_string(),
                )
                .await;
                if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                    cfg.base_url = "https://vendor.example/v1".to_string();
                    actor.chat_state_handle.update_sampling_config(cfg);
                }
                let model = actor
                    .chat_state_handle
                    .get_sampling_config()
                    .await
                    .map(|c| c.model)
                    .unwrap_or_default();
                actor
                    .model_auth_memo
                    .replace(Some(crate::session::acp_session::ModelAuthMemo {
                        model_id: model,
                        facts: ModelAuthFacts {
                            byok: ModelByok::NotByok,
                            auth_scheme: AuthScheme::Bearer,
                            readiness: ModelReadiness::Unknown(UnknownReason::NotInCatalog),
                        },
                        provider: None,
                        catalog_generation: 0,
                    }));

                let cfg = actor.reconstruct_full_config().await;

                assert!(
                    cfg.api_key.is_none(),
                    "method {method:?}: a model absent from the catalog must not carry the \
                     chat-state key to {}",
                    cfg.base_url
                );
                assert_eq!(
                    cfg.auth_scheme,
                    AuthScheme::None,
                    "method {method:?}: the scheme must be cleared with the credential"
                );
                assert!(
                    cfg.bearer_resolver.is_none(),
                    "method {method:?}: no resolver may survive"
                );
                assert_eq!(
                    cfg.credential_source,
                    Some(xai_grok_sampler::CredentialSource::Missing),
                    "method {method:?}: the gap must be labelled so the config \
                     does not still claim a credential it no longer carries"
                );
            }
        })
        .await;
}

/// #159: a model present only in the runtime/prefetched catalog (no local
/// `[model.*]`) must keep its credentials across turn reconstruction. Before
/// the fix, `resolve_model_auth_facts_and_provider` re-resolved config with
/// `prefetched = None`, classified the model as `NotInCatalog`, and the turn
/// stripped the session bearer while still aiming at the original endpoint.
#[tokio::test(flavor = "current_thread")]
async fn runtime_only_catalog_model_keeps_credentials_across_reconstruct() {
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "live-session-jwt".to_string(),
            )
            .await;

            // Runtime-only model: present in ModelsManager, absent from any
            // config-only resolve_model_list(&cfg, None).
            let remote_id = "remote-grok-x";
            let mut entry = crate::agent::config::ModelEntry::fallback(
                remote_id,
                &crate::agent::config::EndpointsConfig::default(),
            );
            entry.info.base_url = "https://api.x.ai/v1".to_string();
            entry.info.auth_scheme = AuthScheme::Bearer;
            actor.models_manager.insert_test_entry(remote_id, entry);
            // Non-empty catalog that does not rely on the synthetic "test" id.
            actor.models_manager.insert_test_entry(
                "other-bundled",
                crate::agent::config::ModelEntry::fallback(
                    "other-bundled",
                    &crate::agent::config::EndpointsConfig::default(),
                ),
            );

            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.model = remote_id.to_string();
                cfg.base_url = "https://api.x.ai/v1".to_string();
                actor.chat_state_handle.update_sampling_config(cfg);
            }
            actor.catalog_model_id.set(remote_id.to_string());
            // No seeded memo — force a fresh resolution through the session path.
            actor.model_auth_memo.replace(None);

            let facts = actor.model_auth_facts(remote_id);
            assert_eq!(
                facts.readiness,
                crate::agent::auth_method::ModelReadiness::Ready,
                "runtime-catalog hit must be Ready, not NotInCatalog"
            );

            let cfg = actor.reconstruct_full_config().await;
            assert_eq!(cfg.model, remote_id);
            assert_eq!(cfg.base_url, "https://api.x.ai/v1");
            assert!(
                cfg.api_key.is_some() || cfg.bearer_resolver.is_some(),
                "runtime-only model must keep credentials; got api_key={:?} resolver={}",
                cfg.api_key.as_ref().map(|_| "<redacted>"),
                cfg.bearer_resolver.is_some()
            );
            assert_ne!(
                cfg.auth_scheme,
                AuthScheme::None,
                "must not strip auth_scheme for a catalogued runtime-only model"
            );
            assert_ne!(
                cfg.credential_source,
                Some(xai_grok_sampler::CredentialSource::Missing),
                "must not label a surviving credential as Missing"
            );
        })
        .await;
}

/// #159 companion: a genuine miss against a non-empty runtime catalog still
/// strips. Without this, the fix could become a blanket credential keep.
#[tokio::test(flavor = "current_thread")]
async fn genuine_runtime_catalog_miss_still_strips_credentials() {
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "live-session-jwt".to_string(),
            )
            .await;

            // Authoritative catalog that does NOT contain the active model.
            actor.models_manager.insert_test_entry(
                "known-model",
                crate::agent::config::ModelEntry::fallback(
                    "known-model",
                    &crate::agent::config::EndpointsConfig::default(),
                ),
            );

            let missing_id = "definitely-not-in-catalog";
            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.model = missing_id.to_string();
                cfg.base_url = "https://vendor.example/v1".to_string();
                actor.chat_state_handle.update_sampling_config(cfg);
            }
            actor.catalog_model_id.set(missing_id.to_string());
            actor.model_auth_memo.replace(None);

            let facts = actor.model_auth_facts(missing_id);
            assert_eq!(
                facts.readiness,
                crate::agent::auth_method::ModelReadiness::Unknown(
                    crate::agent::auth_method::UnknownReason::NotInCatalog
                ),
            );

            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.api_key.is_none(),
                "genuine catalog miss must strip the chat-state key"
            );
            assert_eq!(cfg.auth_scheme, AuthScheme::None);
            assert!(cfg.bearer_resolver.is_none());
            assert_eq!(
                cfg.credential_source,
                Some(xai_grok_sampler::CredentialSource::Missing),
            );
        })
        .await;
}

/// #159: a verdict from an incomplete lookup must not freeze in the memo as
/// definite. Empty ModelsManager + config-only miss → CatalogUnavailable /
/// byok Unknown → memo stays empty so a later real catalog can re-resolve.
#[tokio::test(flavor = "current_thread")]
async fn incomplete_auth_lookup_is_not_memoized_as_definite() {
    use crate::agent::auth_method::{ModelByok, ModelReadiness, UnknownReason};
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "k".to_string(),
            )
            .await;

            // Default test ModelsManager is empty → runtime catalog not
            // authoritative; slug absent from config-only defaults.
            let id = "remote-only-never-in-config";
            assert!(
                actor.models_manager.models().is_empty(),
                "fixture starts with an empty runtime catalog"
            );
            actor.model_auth_memo.replace(None);

            let facts = actor.model_auth_facts(id);
            assert_eq!(facts.byok, ModelByok::Unknown);
            assert_eq!(
                facts.readiness,
                ModelReadiness::Unknown(UnknownReason::CatalogUnavailable),
            );
            assert!(
                actor.model_auth_memo.borrow().is_none(),
                "incomplete lookup must not populate the memo as definite"
            );

            // Later: the runtime catalog gains the model. Without a frozen
            // wrong memo, re-resolve must see Ready.
            let mut entry = crate::agent::config::ModelEntry::fallback(
                id,
                &crate::agent::config::EndpointsConfig::default(),
            );
            entry.info.auth_scheme = xai_grok_sampler::AuthScheme::Bearer;
            actor.models_manager.insert_test_entry(id, entry);
            // Also ensure non-empty catalog path is taken.
            actor.models_manager.insert_test_entry(
                "padding",
                crate::agent::config::ModelEntry::fallback(
                    "padding",
                    &crate::agent::config::EndpointsConfig::default(),
                ),
            );

            let facts = actor.model_auth_facts(id);
            assert_eq!(facts.readiness, ModelReadiness::Ready);
            assert_eq!(facts.byok, ModelByok::NotByok);
            assert!(
                actor.model_auth_memo.borrow().is_some(),
                "a definite Ready verdict may now be memoized"
            );
        })
        .await;
}

/// #159 F1: `on_auth_changed`'s bundled-fallback path wholesale-replaces the
/// catalog via `rebuild`. That mutation must bump `catalog_generation` so a
/// Ready memo under generation *N* cannot survive a swap that dropped its
/// subject (mirror of the etag-flicker permanent-strip case, other side).
///
/// Sequence under test:
/// 1. runtime-only model present (insert_test_entry; never a "real" fetch)
/// 2. session memoizes Ready at generation *N*
/// 3. `on_auth_changed` → remote fetch fails → `needs_bundled_fallback` → `rebuild`
/// 4. generation advances; re-resolve sees NotInCatalog (not the frozen Ready)
#[tokio::test(flavor = "current_thread")]
async fn on_auth_changed_bundled_fallback_rebuild_invalidates_model_auth_memo() {
    use crate::agent::auth_method::{ModelByok, ModelReadiness, UnknownReason};
    use crate::agent::models::{
        ModelFetchAuth, ModelsEndpoint, ModelsFetchFuture, ModelsManagerBuilder,
    };
    use indexmap::IndexMap;
    use xai_grok_sampler::AuthScheme;

    /// Fetch always fails so `on_auth_changed` lands in the bundled-fallback
    /// branch rather than publishing a real catalog.
    struct AlwaysFailEndpoint;
    impl ModelsEndpoint for AlwaysFailEndpoint {
        fn fetch_models(
            &self,
            _endpoints: crate::agent::config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            Box::pin(async move { None })
        }
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am.clone()),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "live-session-jwt".to_string(),
            )
            .await;

            // Unique Arc so we can swap in a manager with a failing endpoint.
            let mut actor = match Arc::try_unwrap(actor) {
                Ok(a) => a,
                Err(_) => panic!("actor Arc is unique"),
            };
            let fail_mgr = ModelsManagerBuilder::new(
                None,
                IndexMap::new(),
                acp::ModelId::new("default"),
                am,
                crate::agent::config::Config::default(),
            )
            .endpoint(Arc::new(AlwaysFailEndpoint))
            .build();
            actor.models_manager = fail_mgr;
            let actor = Arc::new(actor);

            let model_id = "rebuild-fallback-only-model";
            let mut entry = crate::agent::config::ModelEntry::fallback(
                model_id,
                &crate::agent::config::EndpointsConfig::default(),
            );
            entry.info.auth_scheme = AuthScheme::Bearer;
            entry.info.base_url = "https://api.x.ai/v1".to_string();
            // insert_test_entry (not apply_catalog): leaves has_fetched_real_catalog
            // false so on_auth_changed can still enter needs_bundled_fallback.
            actor.models_manager.insert_test_entry(model_id, entry);
            actor.models_manager.insert_test_entry(
                "padding",
                crate::agent::config::ModelEntry::fallback(
                    "padding",
                    &crate::agent::config::EndpointsConfig::default(),
                ),
            );

            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.model = model_id.to_string();
                cfg.base_url = "https://api.x.ai/v1".to_string();
                actor.chat_state_handle.update_sampling_config(cfg);
            }
            actor.catalog_model_id.set(model_id.to_string());
            actor.model_auth_memo.replace(None);

            let gen_before = actor.models_manager.catalog_generation();
            assert!(
                gen_before > 0,
                "insert_test_entry must have advanced generation before the memo"
            );

            let facts = actor.model_auth_facts(model_id);
            assert_eq!(facts.readiness, ModelReadiness::Ready);
            assert_eq!(facts.byok, ModelByok::NotByok);
            assert!(
                actor.model_auth_memo.borrow().is_some(),
                "Ready must freeze in the memo under generation {gen_before}"
            );
            assert_eq!(
                actor
                    .model_auth_memo
                    .borrow()
                    .as_ref()
                    .map(|m| m.catalog_generation),
                Some(gen_before),
            );

            // Auth change + failed remote fetch → rebuild to bundled defaults.
            actor.models_manager.on_auth_changed().await;

            let gen_after = actor.models_manager.catalog_generation();
            assert_ne!(
                gen_after, gen_before,
                "rebuild via on_auth_changed fallback must bump catalog_generation \
                 so the Ready memo cannot outlive a catalog that dropped its subject"
            );
            assert!(
                !actor.models_manager.models().contains_key(model_id),
                "bundled-fallback rebuild must drop the runtime-only model"
            );

            let facts = actor.model_auth_facts(model_id);
            assert_eq!(
                facts.readiness,
                ModelReadiness::Unknown(UnknownReason::NotInCatalog),
                "after rebuild, must not keep serving a frozen Ready memo"
            );
            let stripped = actor.reconstruct_full_config().await;
            assert!(
                stripped.api_key.is_none(),
                "model dropped by rebuild must strip credentials"
            );
            assert_eq!(stripped.auth_scheme, AuthScheme::None);
        })
        .await;
}

/// #159 F1: a catalog publish that momentarily drops a model must not leave
/// the session permanently stripped after the model is restored.
///
/// Without catalog-generation invalidation, the turn that sees the empty
/// publish freezes `NotByok`+`NotInCatalog` in the memo; the restore only
/// fires `notify_models_updated` and never clears the memo, so every later
/// turn keeps stripping credentials.
#[tokio::test(flavor = "current_thread")]
async fn catalog_refresh_that_drops_then_restores_must_not_leave_permanent_strip() {
    use crate::agent::auth_method::{ModelByok, ModelReadiness, UnknownReason};
    use indexmap::IndexMap;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "live-session-jwt".to_string(),
            )
            .await;

            let model_id = "etag-flicker-model";
            let mut entry = crate::agent::config::ModelEntry::fallback(
                model_id,
                &crate::agent::config::EndpointsConfig::default(),
            );
            entry.info.auth_scheme = AuthScheme::Bearer;
            entry.info.base_url = "https://api.x.ai/v1".to_string();

            // Present in catalog → Ready, memoized at current generation.
            // Must go through apply_catalog (not insert_test_entry) so the
            // generation key is proven on the production publish path.
            let mut with_model = IndexMap::new();
            with_model.insert(model_id.to_string(), entry.clone());
            // Padding keeps the catalog non-empty so a later miss is a real
            // NotInCatalog rather than an empty-catalog fallthrough.
            with_model.insert(
                "padding".to_string(),
                crate::agent::config::ModelEntry::fallback(
                    "padding",
                    &crate::agent::config::EndpointsConfig::default(),
                ),
            );
            // Session has M selected; first auth resolve lands after a bad
            // refresh (memo still empty) — the review's permanent-strip path.
            actor
                .models_manager
                .apply_catalog_for_test(with_model.clone());
            let gen_with_model = actor.models_manager.catalog_generation();
            assert!(
                gen_with_model > 0,
                "apply_catalog_for_test must bump generation on the production path"
            );

            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.model = model_id.to_string();
                cfg.base_url = "https://api.x.ai/v1".to_string();
                actor.chat_state_handle.update_sampling_config(cfg);
            }
            actor.catalog_model_id.set(model_id.to_string());
            actor.model_auth_memo.replace(None);

            // Transient etag refresh: model missing from an otherwise non-empty catalog.
            let mut without_model = IndexMap::new();
            without_model.insert(
                "padding".to_string(),
                crate::agent::config::ModelEntry::fallback(
                    "padding",
                    &crate::agent::config::EndpointsConfig::default(),
                ),
            );
            actor.models_manager.apply_catalog_for_test(without_model);
            assert_ne!(
                actor.models_manager.catalog_generation(),
                gen_with_model,
                "drop publish must advance catalog generation"
            );

            let facts = actor.model_auth_facts(model_id);
            assert_eq!(
                facts.readiness,
                ModelReadiness::Unknown(UnknownReason::NotInCatalog),
                "authoritative miss during the flicker is still NotInCatalog"
            );
            assert_eq!(facts.byok, ModelByok::NotByok);
            assert!(
                actor.model_auth_memo.borrow().is_some(),
                "definite NotInCatalog must freeze in the memo"
            );
            let stripped = actor.reconstruct_full_config().await;
            assert!(
                stripped.api_key.is_none(),
                "genuine miss during the flicker must strip"
            );
            assert_eq!(stripped.auth_scheme, AuthScheme::None);
            assert_eq!(
                stripped.credential_source,
                Some(xai_grok_sampler::CredentialSource::Missing),
            );

            // Restore through the real publish path only. Without generation
            // keyed into the memo, this freezes NotInCatalog forever.
            actor.models_manager.apply_catalog_for_test(with_model);

            let facts = actor.model_auth_facts(model_id);
            assert_eq!(
                facts.readiness,
                ModelReadiness::Ready,
                "after restore, must not keep serving a frozen NotInCatalog memo"
            );
            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.api_key.is_some() || cfg.bearer_resolver.is_some(),
                "restored catalog must keep credentials; got api_key={:?} resolver={}",
                cfg.api_key.as_ref().map(|_| "<redacted>"),
                cfg.bearer_resolver.is_some()
            );
            assert_ne!(
                cfg.auth_scheme,
                AuthScheme::None,
                "must not permanently strip after a transient catalog drop"
            );
        })
        .await;
}

/// A transient catalog failure is not a verdict: it must leave a live session
/// alone rather than de-credentialing the turn.
///
/// `CatalogUnavailable` and `UnidentifiedModel` were both `ready = true` before
/// the tri-state. `session_token_auth_gate` documents that an `Unknown`
/// classification must not demote a live session to non-refreshable api-key
/// mode; clearing the resolvers here would do worse -- send nothing at all, and
/// 401 every turn until restart.
///
/// Hand-seeds keep `byok = NotByok` so the memo is cacheable; a readiness-only
/// non-cacheable clause would bypass the seed and re-resolve both arms to
/// `CatalogUnavailable` against an empty test manager (#159 F2).
#[tokio::test(flavor = "current_thread")]
async fn a_transient_catalog_failure_leaves_a_live_session_intact() {
    use crate::agent::auth_method::{ModelByok, ModelReadiness, UnknownReason};
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for reason in [
                UnknownReason::CatalogUnavailable,
                UnknownReason::UnidentifiedModel,
            ] {
                let (_dir, am) = auth_manager_with_valid_token("session-token");
                let (actor, _rx) = make_actor_with_method_and_credentials(
                    Some(am),
                    "cached_token",
                    xai_chat_state::AuthType::SessionToken,
                    "session-jwt".to_string(),
                )
                .await;
                pin_first_party_session_model(&actor).await;
                let model_id = actor.catalog_model_id_str();
                actor
                    .model_auth_memo
                    .replace(Some(crate::session::acp_session::ModelAuthMemo {
                        model_id: model_id.clone(),
                        facts: ModelAuthFacts {
                            byok: ModelByok::NotByok,
                            auth_scheme: AuthScheme::Bearer,
                            readiness: ModelReadiness::Unknown(reason),
                        },
                        provider: None,
                        catalog_generation: actor.models_manager.catalog_generation(),
                    }));

                // F2: the seeded readiness must actually be served at the
                // reconstruct arm (`sampler_turn` Unknown(reason) path) — not
                // collapsed by a fresh empty-catalog resolve. Assert on
                // reason.as_str() so the two loop iterations are proven to
                // reach *different* reasons (auth_scheme alone does not).
                let served = actor.model_auth_facts(&model_id);
                match &served.readiness {
                    ModelReadiness::Unknown(got) => {
                        assert_eq!(
                            got.as_str(),
                            reason.as_str(),
                            "{reason:?}: memo must be served so UnidentifiedModel stays distinct from CatalogUnavailable"
                        );
                    }
                    other => panic!(
                        "{reason:?}: expected Unknown, got {other:?} (memo disarmed?)"
                    ),
                }

                let cfg = actor.reconstruct_full_config().await;

                assert_ne!(
                    cfg.auth_scheme,
                    AuthScheme::None,
                    "{reason:?}: a transient failure must not clear the scheme"
                );
                assert!(
                    cfg.bearer_resolver.is_some() || cfg.api_key.is_some(),
                    "{reason:?}: the live session must survive an unobtainable catalog"
                );
                // #151: the key survives, so its stored provenance must too.
                // Before step 3 this was None on CatalogUnavailable and L3
                // could not refuse ambient bytes on an external origin.
                assert_eq!(
                    cfg.credential_source,
                    Some(xai_grok_sampler::CredentialSource::XaiSession),
                    "{reason:?}: surviving chat-state credential must keep its stored source"
                );
            }
        })
        .await;
}

/// #136 / #151 load-bearing: `Unknown(CatalogUnavailable)` on an *external*
/// origin must refuse an ambient xAI credential at L3 (`SamplingClient::new`).
///
/// The first-party assertion in the sibling test only proves the label was
/// carried — #151's hole cannot occur on `api.x.ai`. This one asserts the
/// route is actually refused.
///
/// Construction goes through the open-then-login path: credentials start as
/// `Missing` (pre-auth spawn), then the session-refresh writer installs the
/// live JWT. With rebind the stored source becomes `XaiSession` and L3 fires;
/// with `replace_api_key` the label stays `Missing`, L3 is disarmed, and this
/// test fails.
#[tokio::test(flavor = "current_thread")]
async fn catalog_unavailable_external_refuses_ambient_session_credential() {
    use crate::agent::auth_method::{ModelByok, ModelReadiness, UnknownReason};
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("ambient-session-jwt");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "unused-initial".to_string(),
            )
            .await;
            // Pre-login spawn: production always pairs `Missing` with
            // `api_key: None`. In-session auth then writes the live JWT.
            actor
                .chat_state_handle
                .update_credentials(xai_chat_state::Credentials::bound(
                    None,
                    xai_chat_state::AuthType::SessionToken,
                    xai_grok_sampler::CredentialSource::Missing,
                ));
            // Gate must be active for the refresh writer: first-party Ready.
            pin_first_party_session_model(&actor).await;
            actor.refresh_token_if_expired().await;
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .source_cloned(),
                Some(xai_grok_sampler::CredentialSource::XaiSession),
                "session-refresh writer must rebind ambient provenance, not preserve Missing"
            );
            assert_eq!(
                actor.chat_state_handle.get_credentials().await.api_key(),
                Some("ambient-session-jwt"),
            );

            // The #151 hole: catalog unobtainable + external origin + ambient bytes.
            let mut cfg = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("sampling config");
            cfg.base_url = "https://vendor.example/v1".to_string();
            cfg.endpoint_trust = Some(xai_grok_sampler::EndpointTrustClass::External);
            let model = cfg.model.clone();
            actor.chat_state_handle.update_sampling_config(cfg);
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::Bearer,
                        readiness: ModelReadiness::Unknown(UnknownReason::CatalogUnavailable),
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            let cfg = actor.reconstruct_full_config().await;
            assert_eq!(
                cfg.credential_source,
                Some(xai_grok_sampler::CredentialSource::XaiSession),
                "CatalogUnavailable must keep the stored ambient source with the surviving key"
            );
            assert!(
                cfg.api_key.is_some() || cfg.bearer_resolver.is_some(),
                "the ambient credential must still be on the reconstructed config"
            );

            let err = xai_grok_sampler::SamplingClient::new(cfg)
                .expect_err("L3 must refuse an ambient xAI credential on a non-first-party origin");
            let rendered = format!("{err}");
            assert!(
                rendered.contains("ambient xAI credential is not allowed"),
                "refusal must name the ambient-origin rule, got: {rendered}"
            );
            assert!(
                !rendered.contains("ambient-session-jwt"),
                "the error leaked the credential: {rendered}"
            );
        })
        .await;
}

/// Same refusal as [`catalog_unavailable_external_refuses_ambient_session_credential`],
/// but External comes only from a non-first-party `base_url` with
/// `endpoint_trust: None` — the production arm at
/// `SamplingClient::new` (`xai-grok-sampler` URL-derived trust), not the
/// explicit-`Some(External)` match arm the sibling exercises.
#[tokio::test(flavor = "current_thread")]
async fn catalog_unavailable_url_derived_external_refuses_ambient_session_credential() {
    use crate::agent::auth_method::{ModelByok, ModelReadiness, UnknownReason};
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("ambient-session-jwt");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "unused-initial".to_string(),
            )
            .await;
            actor
                .chat_state_handle
                .update_credentials(xai_chat_state::Credentials::bound(
                    None,
                    xai_chat_state::AuthType::SessionToken,
                    xai_grok_sampler::CredentialSource::Missing,
                ));
            pin_first_party_session_model(&actor).await;
            actor.refresh_token_if_expired().await;
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .source_cloned(),
                Some(xai_grok_sampler::CredentialSource::XaiSession),
                "session-refresh writer must rebind ambient provenance, not preserve Missing"
            );

            let mut cfg = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("sampling config");
            cfg.base_url = "https://vendor.example/v1".to_string();
            // Production writes `endpoint_trust: None` and lets L3 derive
            // External from the URL. Do not set Some(External) here.
            cfg.endpoint_trust = None;
            let model = cfg.model.clone();
            actor.chat_state_handle.update_sampling_config(cfg);
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::Bearer,
                        readiness: ModelReadiness::Unknown(UnknownReason::CatalogUnavailable),
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            let cfg = actor.reconstruct_full_config().await;
            assert!(
                cfg.endpoint_trust.is_none(),
                "fixture must leave trust unset so L3 takes the URL-derived arm"
            );
            assert_eq!(
                cfg.credential_source,
                Some(xai_grok_sampler::CredentialSource::XaiSession),
                "CatalogUnavailable must keep the stored ambient source with the surviving key"
            );
            assert!(
                cfg.api_key.is_some() || cfg.bearer_resolver.is_some(),
                "the ambient credential must still be on the reconstructed config"
            );

            let err = xai_grok_sampler::SamplingClient::new(cfg).expect_err(
                "L3 must refuse an ambient xAI credential on a URL-derived external origin",
            );
            let rendered = format!("{err}");
            assert!(
                rendered.contains("ambient xAI credential is not allowed"),
                "refusal must name the ambient-origin rule, got: {rendered}"
            );
            assert!(
                !rendered.contains("ambient-session-jwt"),
                "the error leaked the credential: {rendered}"
            );
        })
        .await;
}

/// Pre-flight session refresh must also stay off for `AuthScheme::None`, so a
/// model switch cannot rewrite chat-state credentials with a live OIDC token.
#[tokio::test(flavor = "current_thread")]
async fn refresh_token_if_expired_skips_session_refresh_for_none_auth_scheme() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("fresh-session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "stale-session-jwt".to_string(),
            )
            .await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::None,
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            actor.refresh_token_if_expired().await;

            assert_eq!(
                actor.chat_state_handle.get_credentials().await.api_key(),
                Some("stale-session-jwt"),
                "AuthScheme::None must not heal credentials from the session token"
            );
        })
        .await;
}

/// When a custom `AuthScheme::None` alias shares a wire slug with a built-in
/// Bearer entry, auth facts must key off the catalog id — not the routing slug.
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_uses_catalog_key_for_none_alias_with_shared_wire_slug() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "stale-session-jwt".to_string(),
            )
            .await;

            let shared_slug = "shared-routing-slug";
            let catalog_key = "none-alias";
            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.model = shared_slug.to_string();
                actor.chat_state_handle.update_sampling_config(cfg);
            }
            actor.catalog_model_id.set(catalog_key.to_string());
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: catalog_key.to_string(),
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::None,
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            let cfg = actor.reconstruct_full_config().await;

            assert_eq!(
                cfg.model, shared_slug,
                "wire routing slug must stay on the sampler payload"
            );
            assert!(
                cfg.bearer_resolver.is_none(),
                "catalog None alias must not attach session bearer resolver"
            );
            assert_eq!(cfg.auth_scheme, AuthScheme::None);
            assert!(
                cfg.api_key.is_none(),
                "AuthScheme::None must strip stale session credentials"
            );
        })
        .await;
}

/// Switching via `handle_set_session_model` must persist the catalog key even
/// when the wire slug matches a different catalog entry.
#[tokio::test(flavor = "current_thread")]
async fn set_session_model_preserves_catalog_key_for_none_alias_with_shared_wire_slug() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, persistence_rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "stale-session-jwt".to_string(),
            )
            .await;
            spawn_model_persistence_ack(persistence_rx);

            let shared_slug = "shared-routing-slug";
            let catalog_key = "none-alias";
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: "builtin-bearer".to_string(),
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::Bearer,
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            let switch_cfg = xai_grok_sampler::SamplerConfig {
                api_key: Some("stale-session-jwt".to_string()),
                base_url: "http://127.0.0.1:11434/v1".to_string(),
                model: shared_slug.to_string(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                endpoint_trust: None,
                credential_source: None,
                api_backend: crate::sampling::ApiBackend::ChatCompletions,
                auth_scheme: AuthScheme::None,
                extra_headers: Default::default(),
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: 256_000,
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
                supports_backend_search: false,
                compactions_remaining: None,
                compaction_at_tokens: None,
                doom_loop_recovery: None,
                header_injector: None,
            };
            let returned = actor
                .handle_set_session_model(
                    acp::ModelId::new(catalog_key),
                    switch_cfg,
                    false,
                    false,
                    true,
                    85,
                    "grok-build",
                )
                .await
                .expect("model switch");

            assert_eq!(returned.0.as_ref(), catalog_key);
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: catalog_key.to_string(),
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: AuthScheme::None,
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            let cfg = actor.reconstruct_full_config().await;
            assert_eq!(cfg.model, shared_slug);
            assert!(cfg.bearer_resolver.is_none());
            assert_eq!(cfg.auth_scheme, AuthScheme::None);
        })
        .await;
}

/// Switching to `AuthScheme::None` must clear stale session credentials from
/// chat_state even when the caller passes a non-None `api_key` in the sampling
/// config (defense-in-depth against credential leakage on the wire).
#[tokio::test(flavor = "current_thread")]
async fn handle_set_session_model_clears_credentials_for_none() {
    use xai_grok_sampler::AuthScheme;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, persistence_rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "stale-session-jwt".to_string(),
            )
            .await;
            spawn_model_persistence_ack(persistence_rx);

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();
            let catalog_model = model.clone();

            let cfg = xai_grok_sampler::SamplerConfig {
                api_key: Some("stale-session-jwt".to_string()),
                base_url: "http://127.0.0.1:11434/v1".to_string(),
                model,
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                endpoint_trust: None,
                credential_source: None,
                api_backend: crate::sampling::ApiBackend::ChatCompletions,
                auth_scheme: AuthScheme::None,
                extra_headers: Default::default(),
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: 256_000,
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
                supports_backend_search: false,
                compactions_remaining: None,
                compaction_at_tokens: None,
                doom_loop_recovery: None,
                header_injector: None,
            };
            let _ = actor
                .handle_set_session_model(
                    acp::ModelId::new(catalog_model),
                    cfg,
                    false,
                    false,
                    true,
                    85,
                    "grok-build",
                )
                .await;

            let creds = actor.chat_state_handle.get_credentials().await;
            assert!(
                creds.api_key().is_none(),
                "AuthScheme::None model switch must clear stale session credentials from chat_state"
            );
            assert_eq!(
                creds.auth_type(),
                xai_chat_state::AuthType::ApiKey,
                "AuthScheme::None must not leave SessionToken residue in chat_state"
            );
        })
        .await;
}

/// Regression: a model-switch chokepoint must invalidate
/// the memo even when `model_id` is unchanged. Otherwise a config edit that
/// turns the current model into a per-model BYOK model on a third-party
/// `base_url` keeps serving the stale `NotByok`, leaving the gate active and
/// leaking the OIDC token cross-host.
#[tokio::test(flavor = "current_thread")]
async fn set_session_model_invalidates_byok_memo_for_same_model_id() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, persistence_rx) = make_actor_with_method_and_credentials(
                None,
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "k".to_string(),
            )
            .await;
            spawn_model_persistence_ack(persistence_rx);

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();

            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model.clone(),
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: Default::default(),
                        readiness: crate::agent::auth_method::ModelReadiness::Ready,
                    },
                    provider: None,
                    catalog_generation: 0,
                }));

            // Switch to the same model_id, now a per-model BYOK model on a
            // third-party endpoint.
            let cfg = xai_grok_sampler::SamplerConfig {
                api_key: Some("byok-key".to_string()),
                base_url: "https://third-party.example/v1".to_string(),
                model: model.clone(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                endpoint_trust: None,
                credential_source: None,
                api_backend: crate::sampling::ApiBackend::ChatCompletions,
                auth_scheme: Default::default(),
                extra_headers: Default::default(),
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: 256_000,
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
                supports_backend_search: false,
                compactions_remaining: None,
                compaction_at_tokens: None,
                doom_loop_recovery: None,
                header_injector: None,
            };
            let _ = actor
                .handle_set_session_model(
                    acp::ModelId::new(model.clone()),
                    cfg,
                    false,
                    false,
                    true,
                    85,
                    "grok-build",
                )
                .await;

            assert!(
                actor.model_auth_memo.borrow().is_none(),
                "a model switch must invalidate the per-model BYOK memo so the next \
                 reconstruct recomputes under the current config"
            );
        })
        .await;
}

use crate::auth::test_counting_provider as counting_provider;

/// Seed the per-model memo so `model_auth_provider` resolves without a
/// config load.
async fn seed_provider_memo(actor: &Arc<SessionActor>, provider: crate::auth::AuthProviderRef) {
    let model = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .map(|c| c.model)
        .unwrap_or_default();
    actor
        .model_auth_memo
        .replace(Some(crate::session::acp_session::ModelAuthMemo {
            model_id: model,
            facts: crate::agent::config::ModelAuthFacts {
                byok: crate::agent::auth_method::ModelByok::Byok,
                auth_scheme: Default::default(),
                readiness: crate::agent::auth_method::ModelReadiness::Ready,
            },
            provider: Some(provider),
            catalog_generation: 0,
        }));
}

/// Regression: switching from a provider-backed model to a first-party model
/// must drop the minted provider token from the chat credentials, so it can
/// never ride a later request to `api.x.ai`. Mirrors the forward direction in
/// `set_session_model_invalidates_byok_memo_for_same_model_id`.
#[tokio::test(flavor = "current_thread")]
async fn switch_to_first_party_model_drops_minted_provider_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("hall-pass", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();
            assert_eq!(token, "tok-1");

            let (actor, persistence_rx) =
                make_actor_with_auth_and_credentials(None, xai_chat_state::AuthType::ApiKey, token)
                    .await;
            spawn_model_persistence_ack(persistence_rx);
            seed_provider_memo(&actor, provider).await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();
            let catalog_model = model.clone();

            let cfg = xai_grok_sampler::SamplerConfig {
                api_key: Some("session-jwt".to_string()),
                base_url: "https://api.x.ai/v1".to_string(),
                model,
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                endpoint_trust: None,
                credential_source: None,
                api_backend: crate::sampling::ApiBackend::ChatCompletions,
                auth_scheme: Default::default(),
                extra_headers: Default::default(),
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: 256_000,
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
                supports_backend_search: false,
                compactions_remaining: None,
                compaction_at_tokens: None,
                doom_loop_recovery: None,
                header_injector: None,
            };
            let _ = actor
                .handle_set_session_model(
                    acp::ModelId::new(catalog_model),
                    cfg,
                    false,
                    false,
                    true,
                    85,
                    "grok-build",
                )
                .await;

            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key(),
                Some("session-jwt"),
                "switching to a first-party model must install the session credential, \
                 not the minted provider token"
            );
        })
        .await;
}

/// Arm 4c: a 401 on a provider-backed model re-mints once and resubmits.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_on_provider_model_remints_and_resubmits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-recover", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();
            assert_eq!(token, "tok-1");

            let (actor, _rx) =
                make_actor_with_auth_and_credentials(None, xai_chat_state::AuthType::ApiKey, token)
                    .await;
            seed_provider_memo(&actor, provider).await;
            crate::auth::test_backdate_provider_mint(
                "test-4c-recover",
                std::time::Duration::from_secs(60),
            );

            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                        store: RecoveredStore::AuthProvider,
                        ..
                    })
                ),
                "provider 401 must re-mint and resubmit via the provider store"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key(),
                Some("tok-2"),
                "chat-state credentials must carry the re-minted token"
            );
        })
        .await;
}

/// Arm 4c also fires for a bare 401 that did not classify as `Auth`-kind.
#[tokio::test(flavor = "current_thread")]
async fn sampler_non_auth_kind_401_on_provider_model_still_recovers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-non-auth-kind", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

            let (actor, _rx) =
                make_actor_with_auth_and_credentials(None, xai_chat_state::AuthType::ApiKey, token)
                    .await;
            seed_provider_memo(&actor, provider).await;
            crate::auth::test_backdate_provider_mint(
                "test-4c-non-auth-kind",
                std::time::Duration::from_secs(60),
            );

            let mut error = auth_error();
            error.kind = xai_grok_sampler::SamplingErrorKind::Api;
            let result = actor.handle_sampling_failure(error).await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "a non-Auth-kind 401 on a provider model must still recover via 4c"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(creds.api_key(), Some("tok-2"));
        })
        .await;
}

/// A 401 on a request that went out with no key mints instead of
/// recovering.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_with_no_key_on_provider_model_mints_and_resubmits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-no-key", dir.path());

            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                xai_chat_state::AuthType::ApiKey,
                "placeholder".to_string(),
            )
            .await;
            let mut creds = actor.chat_state_handle.get_credentials().await;
            creds.clear_api_key();
            actor.chat_state_handle.update_credentials(creds);
            seed_provider_memo(&actor, provider).await;

            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "an unauthenticated 401 on a provider model must mint and resubmit"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(creds.api_key(), Some("tok-1"));
        })
        .await;
}

/// A provider model's 401 goes through the provider, never the session
/// refresher (4a/4b vs 4c exclusivity). The actor uses a session-based method,
/// so the gate would be active for a non-BYOK model; the BYOK memo is what
/// shadows it, which is the invariant under test.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_on_provider_model_never_refreshes_session() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-exclusive", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                token,
            )
            .await;
            seed_provider_memo(&actor, provider).await;
            crate::auth::test_backdate_provider_mint(
                "test-4c-exclusive",
                std::time::Duration::from_secs(60),
            );

            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "the provider arm must recover"
            );
            assert!(
                !called.load(Ordering::SeqCst),
                "session refresh must never fire for a provider-backed model"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(creds.api_key(), Some("tok-2"));
        })
        .await;
}

/// The pre-turn mirror of the exclusivity test: a cold cache mints the
/// provider token into chat-state, and the session refresher never fires. The
/// actor uses a session-based method, so the gate would be active for a
/// non-BYOK model; the BYOK memo is what keeps the refresher silent.
#[tokio::test(flavor = "current_thread")]
async fn pre_turn_on_provider_model_never_installs_session_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-preturn-exclusive", dir.path());

            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn crate::auth::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                xai_chat_state::AuthType::SessionToken,
                "placeholder".to_string(),
            )
            .await;
            // Cold cache: no key on the wire yet.
            let mut creds = actor.chat_state_handle.get_credentials().await;
            creds.clear_api_key();
            actor.chat_state_handle.update_credentials(creds);
            seed_provider_memo(&actor, provider).await;

            actor.refresh_token_if_expired().await;

            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key(),
                Some("tok-1"),
                "the cold pre-turn hook must mint the provider token"
            );
            assert!(
                !called.load(Ordering::SeqCst),
                "the session refresher must never fire for a provider-backed model"
            );
        })
        .await;
}

/// A token rejected moments after mint surfaces the 401 (fresh-mint
/// guard).
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_on_fresh_provider_token_surfaces_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-guard", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                xai_chat_state::AuthType::ApiKey,
                token.clone(),
            )
            .await;
            seed_provider_memo(&actor, provider).await;

            let result = actor.handle_sampling_failure(auth_error()).await;
            assert!(
                result.is_err(),
                "a fresh-minted rejected token must surface the 401, not loop"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key(),
                Some(token.as_str()),
                "credentials must be unchanged when the guard blocks the re-mint"
            );
        })
        .await;
}
