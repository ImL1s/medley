use super::support::*;
use super::*;
use tokio::sync::mpsc;
/// Test that `last_api_request_at` is recorded and used for idle detection.
///
/// The `maybe_refresh_model_metadata_on_resume` method checks this timestamp
/// to decide whether to proactively refresh model metadata from cli-chat-proxy.
/// This test verifies the timestamp recording and idle detection logic.
#[tokio::test(flavor = "current_thread")]
async fn test_last_api_request_at_idle_detection() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = mpsc::unbounded_channel();
            let (persistence_tx, _) = mpsc::unbounded_channel();
            let actor = create_test_actor(50_000, 100_000, 85, gateway_tx, persistence_tx).await;
            let initial = actor
                .last_api_request_at
                .load(std::sync::atomic::Ordering::Relaxed);
            assert_eq!(initial, 0, "last_api_request_at should be 0 initially");
            actor.record_api_request_time();
            let recorded = actor
                .last_api_request_at
                .load(std::sync::atomic::Ordering::Relaxed);
            assert!(
                recorded > 0,
                "last_api_request_at should be set after recording"
            );
            let now_ms = chrono::Utc::now().timestamp_millis();
            let diff = (now_ms - recorded).abs();
            assert!(
                diff < 1000,
                "recorded timestamp should be within 1 second of now"
            );
            let idle_secs = (now_ms - recorded) / 1000;
            assert!(
                idle_secs < SessionActor::IDLE_REFRESH_THRESHOLD_SECS,
                "should be within idle threshold immediately after recording"
            );
        })
        .await;
}
/// An untrusted loopback model endpoint must never receive session credentials
/// during idle-resume metadata refresh, and cached metadata stays unchanged.
#[tokio::test(flavor = "current_thread")]
async fn test_idle_resume_untrusted_endpoint_is_zero_request_noop() {
    use axum::routing::get;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = requests.clone();
            let app = axum::Router::new().route(
                "/v1/models-v2",
                get(move || {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async {
                        axum::Json(serde_json::json!({
                            "data": [{
                                "model": "test-model",
                                "context_window": 300_000,
                                "max_completion_tokens": 16384
                            }]
                        }))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mock_url = format!("http://{}/v1", listener.local_addr().unwrap());
            let server = tokio::task::spawn_local(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let (gateway_tx, _) = mpsc::unbounded_channel();
            let (persistence_tx, _) = mpsc::unbounded_channel();
            let mut actor =
                create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await;
            actor.auth_method_id = test_auth_method_id("cached_token");
            let dir = tempfile::tempdir().unwrap();
            let manager = Arc::new(crate::auth::AuthManager::new(
                dir.path(),
                crate::auth::GrokComConfig::default(),
            ));
            manager.hot_swap(crate::auth::GrokAuth {
                key: "resume-session-sentinel".into(),
                auth_mode: crate::auth::AuthMode::Oidc,
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..crate::auth::GrokAuth::test_default()
            });
            actor.auth_manager = Some(manager);

            let mut config = actor.chat_state_handle.get_sampling_config().await.unwrap();
            config.base_url = mock_url;
            config.model = "test-model".into();
            config.max_completion_tokens = Some(8192);
            actor.chat_state_handle.update_sampling_config(config);
            tokio::task::yield_now().await;
            actor.last_api_request_at.store(
                chrono::Utc::now().timestamp_millis() - 11 * 60 * 1000,
                std::sync::atomic::Ordering::Relaxed,
            );

            actor.maybe_refresh_model_metadata_on_resume().await;
            let after = actor.chat_state_handle.get_sampling_config().await.unwrap();
            server.abort();
            assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(after.context_window.get(), 200_000);
            assert_eq!(after.max_completion_tokens, Some(8192));
        })
        .await;
}

/// The dedicated async catalog client exposes redirects but never follows them.
#[tokio::test]
async fn test_idle_resume_catalog_client_does_not_follow_redirects() {
    use axum::{http::StatusCode, routing::get};
    let target_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let target_seen = target_requests.clone();
    let target_app = axum::Router::new().route(
        "/target",
        get(move || {
            target_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { StatusCode::OK }
        }),
    );
    let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_url = format!("http://{}/target", target_listener.local_addr().unwrap());
    let target_server = tokio::spawn(async move {
        axum::serve(target_listener, target_app).await.unwrap();
    });

    let source_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source_seen = source_requests.clone();
    let source_app = axum::Router::new().route(
        "/models-v2",
        get(move || {
            source_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let location = target_url.clone();
            async move { (StatusCode::FOUND, [("location", location)]) }
        }),
    );
    let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source_url = format!("http://{}/models-v2", source_listener.local_addr().unwrap());
    let source_server = tokio::spawn(async move {
        axum::serve(source_listener, source_app).await.unwrap();
    });

    let response = crate::remote::client::models_catalog_async_client()
        .get(source_url)
        .send()
        .await
        .unwrap();
    source_server.abort();
    target_server.abort();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(source_requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(target_requests.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// Verify `maybe_refresh_model_metadata_on_resume` is a no-op when idle < 10 min.
#[tokio::test(flavor = "current_thread")]
async fn test_idle_resume_noop_when_not_idle_enough() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await;
            let five_minutes_ago_ms = chrono::Utc::now().timestamp_millis() - (5 * 60 * 1000);
            actor
                .last_api_request_at
                .store(five_minutes_ago_ms, std::sync::atomic::Ordering::Relaxed);
            let cfg_before = actor.chat_state_handle.get_sampling_config().await.unwrap();
            actor.maybe_refresh_model_metadata_on_resume().await;
            let cfg_after = actor.chat_state_handle.get_sampling_config().await.unwrap();
            assert_eq!(
                cfg_before.context_window, cfg_after.context_window,
                "config should not change when idle < 10 min"
            );
        })
        .await;
}
