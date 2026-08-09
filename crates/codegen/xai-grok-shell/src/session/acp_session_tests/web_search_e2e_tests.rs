use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use xai_grok_tools::computer::local::{LocalFs, LocalTerminalBackend};
use xai_grok_tools::computer::types::{AsyncFileSystem, TerminalBackend};
use xai_grok_tools::notification::ToolNotificationHandle;
use xai_grok_tools::registry::types::{SessionContext, ToolConfig, ToolServerConfig};

#[tokio::test]
async fn web_search_uses_model_override_from_config_end_to_end() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    async fn handle_request(
        State(tx): State<tokio::sync::mpsc::UnboundedSender<Value>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let _ = tx.send(body);
        Json(json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "enterprise-search",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "search result",
                    "annotations": []
                }]
            }]
        }))
    }
    let app = Router::new()
        .route("/responses", post(handle_request))
        .with_state(tx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let raw_config: toml::Value = toml::from_str(&format!(
        r#"
            [models]
            web_search = "enterprise-search"

            [model.enterprise-search]
            model = "enterprise-search"
            base_url = "http://{addr}"
            api_key = "enterprise-key"
            context_window = 256000
            api_backend = "responses"
            "#,
    ))
    .unwrap();
    let web_search_model =
        crate::config::ModelOverrideConfig::resolve(None, None, &raw_config, None).web_search;
    let agent_cfg = crate::agent::config::Config::new_from_toml_cfg(&raw_config).unwrap();
    let models = crate::agent::config::resolve_model_list(&agent_cfg, None);
    let entry = models.get(web_search_model.as_str()).unwrap();
    let resolved = crate::agent::config::sampling_config_for_model(
        entry,
        crate::agent::config::resolve_credentials(entry, None),
        None,
        None,
        None,
        None,
        &crate::agent::trusted_origins::TrustedXaiOrigins::default(),
    );
    let web_search_sampling = crate::tools::config::web_search_sampling_config(resolved);

    let builder = crate::tools::bridge::ToolBridge::get_builder();
    let config = ToolServerConfig {
        tools: vec![ToolConfig {
            id: "GrokBuild:web_search".into(),
            params: None,
            name_override: None,
            params_name_overrides: None,
            description_override: None,
            behavior_version: None,
            kind: None,
        }],
        behavior_preset: None,
    };
    let fs: std::sync::Arc<dyn AsyncFileSystem> = std::sync::Arc::new(LocalFs);
    let terminal: std::sync::Arc<dyn TerminalBackend> =
        std::sync::Arc::new(LocalTerminalBackend::new());
    let ctx = SessionContext {
        backend: terminal,
        fs,
        cwd: std::env::temp_dir(),
        session_folder: std::env::temp_dir().join("grok-web-search-e2e"),
        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: std::env::temp_dir().join("grok-web-search-e2e/state.json"),
        memory_backend: None,
        web_search_config: xai_grok_tools::implementations::web_search::WebSearchConfig::Enabled {
            api_key: web_search_sampling.api_key.clone(),
            base_url: web_search_sampling.base_url.clone(),
            model: web_search_sampling.model.clone(),
            extra_headers: web_search_sampling.extra_headers.clone(),
            env_http_headers: web_search_sampling.env_http_headers.clone(),
            // The optional extra access key is no longer carried on
            // `SamplerConfig`. The shell-level value flows in via
            // `Credentials` at session-spawn time; in this self-contained
            // test fixture there's no extra access key in scope.
            alpha_test_key: None,
            api_key_provider: None,
        },
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: Default::default(),
        video_gen_config: Default::default(),
        app_builder_deployer_config: Default::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
    };
    let bridge = crate::tools::bridge::ToolBridge::finalize_builder(builder, config, ctx)
        .await
        .expect("finalize_builder should succeed");
    let result = bridge
        .call(
            "web_search",
            json!({
                "query": "test query",
                "allowed_domains": ["example.com"]
            }),
            "web-search-e2e",
        )
        .await;
    assert!(
        result.is_ok(),
        "web_search should succeed: {:?}",
        result.err()
    );

    let request = rx.recv().await.expect("mock server should receive request");
    assert_eq!(
        request.get("model").and_then(|v| v.as_str()),
        Some(web_search_model.as_str())
    );

    server.abort();
}

/// #160: enabling `web_search` for a header-authenticated model is only half
/// the fix. The credential the user supplied has to reach the wire, and the
/// bearer they never supplied must not.
///
/// The negative half is the load-bearing one. `extra_headers` are applied after
/// the bearer, so an invented `Authorization` would be overwritten for the
/// `authorization` flavour and the bug would hide — but an `x-api-key` route
/// would ship a bogus empty bearer alongside its real credential. Reverting
/// `api_key` to a `String` fails the `AUTHORIZATION` assertion below.
#[tokio::test]
async fn web_search_sends_an_explicit_header_and_invents_no_bearer() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<axum::http::HeaderMap>();
    async fn handle_request(
        State(tx): State<tokio::sync::mpsc::UnboundedSender<axum::http::HeaderMap>>,
        headers: axum::http::HeaderMap,
        Json(_body): Json<Value>,
    ) -> Json<Value> {
        let _ = tx.send(headers);
        Json(json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "hdr-search",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "search result",
                    "annotations": []
                }]
            }]
        }))
    }
    let app = Router::new()
        .route("/responses", post(handle_request))
        .with_state(tx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let builder = crate::tools::bridge::ToolBridge::get_builder();
    let config = ToolServerConfig {
        tools: vec![ToolConfig {
            id: "GrokBuild:web_search".into(),
            params: None,
            name_override: None,
            params_name_overrides: None,
            description_override: None,
            behavior_version: None,
            kind: None,
        }],
        behavior_preset: None,
    };
    let fs: std::sync::Arc<dyn AsyncFileSystem> = std::sync::Arc::new(LocalFs);
    let terminal: std::sync::Arc<dyn TerminalBackend> =
        std::sync::Arc::new(LocalTerminalBackend::new());
    let ctx = SessionContext {
        backend: terminal,
        fs,
        cwd: std::env::temp_dir(),
        session_folder: std::env::temp_dir().join("grok-web-search-hdr"),
        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: std::env::temp_dir().join("grok-web-search-hdr/state.json"),
        memory_backend: None,
        // Exactly what `spawn.rs` now produces for an `ExplicitHeader` route:
        // no bearer, credential in the headers.
        web_search_config: xai_grok_tools::implementations::web_search::WebSearchConfig::Enabled {
            api_key: None,
            base_url: format!("http://{addr}"),
            model: "hdr-search".to_string(),
            extra_headers: [("x-api-key".to_string(), "user-supplied-key".to_string())]
                .into_iter()
                .collect(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: None,
        },
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: Default::default(),
        video_gen_config: Default::default(),
        app_builder_deployer_config: Default::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
    };
    let bridge = crate::tools::bridge::ToolBridge::finalize_builder(builder, config, ctx)
        .await
        .expect("finalize_builder should succeed");
    let result = bridge
        .call(
            "web_search",
            json!({ "query": "test query" }),
            "web-search-hdr",
        )
        .await;
    assert!(
        result.is_ok(),
        "web_search should succeed: {:?}",
        result.err()
    );

    let headers = rx.recv().await.expect("mock server should receive request");
    assert_eq!(
        headers.get("x-api-key").and_then(|v| v.to_str().ok()),
        Some("user-supplied-key"),
        "the user's credential header must reach the request"
    );
    assert!(
        headers.get(axum::http::header::AUTHORIZATION).is_none(),
        "no bearer may be invented for a route that supplied its own header",
    );

    server.abort();
}

/// Drive one real `web_search` call against a mock Responses endpoint and hand
/// back the headers it received. `make_config` gets the bound address so it can
/// point `base_url` at the server.
async fn web_search_request_headers(
    session_tag: &str,
    make_config: impl FnOnce(String) -> xai_grok_tools::implementations::web_search::WebSearchConfig,
) -> axum::http::HeaderMap {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<axum::http::HeaderMap>();
    async fn handle_request(
        State(tx): State<tokio::sync::mpsc::UnboundedSender<axum::http::HeaderMap>>,
        headers: axum::http::HeaderMap,
        Json(_body): Json<Value>,
    ) -> Json<Value> {
        let _ = tx.send(headers);
        Json(json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "env-hdr-search",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "search result", "annotations": []}]
            }]
        }))
    }
    let app = Router::new()
        .route("/responses", post(handle_request))
        .with_state(tx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let builder = crate::tools::bridge::ToolBridge::get_builder();
    let config = ToolServerConfig {
        tools: vec![ToolConfig {
            id: "GrokBuild:web_search".into(),
            params: None,
            name_override: None,
            params_name_overrides: None,
            description_override: None,
            behavior_version: None,
            kind: None,
        }],
        behavior_preset: None,
    };
    let fs: std::sync::Arc<dyn AsyncFileSystem> = std::sync::Arc::new(LocalFs);
    let terminal: std::sync::Arc<dyn TerminalBackend> =
        std::sync::Arc::new(LocalTerminalBackend::new());
    let ctx = SessionContext {
        backend: terminal,
        fs,
        cwd: std::env::temp_dir(),
        session_folder: std::env::temp_dir().join(format!("grok-ws-{session_tag}")),
        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: std::env::temp_dir().join(format!("grok-ws-{session_tag}/state.json")),
        memory_backend: None,
        web_search_config: make_config(format!("http://{addr}")),
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: Default::default(),
        video_gen_config: Default::default(),
        app_builder_deployer_config: Default::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
    };
    let bridge = crate::tools::bridge::ToolBridge::finalize_builder(builder, config, ctx)
        .await
        .expect("finalize_builder should succeed");
    let result = bridge
        .call("web_search", json!({ "query": "test query" }), session_tag)
        .await;
    assert!(
        result.is_ok(),
        "web_search should succeed: {:?}",
        result.err()
    );
    let headers = rx.recv().await.expect("mock server should receive request");
    server.abort();
    headers
}

/// #160: a model authenticated by `env_http_headers` is labelled
/// `ExplicitHeader` by the classifier and therefore keeps `web_search` — so the
/// credential that label refers to has to reach the wire.
///
/// It travels as a header **name to environment variable name**, resolved at
/// client build; nothing upstream ever holds the value, which is why the
/// sampler resolves its own the same way. Deleting the resolution loop in
/// `web_search/client.rs` fails this test.
#[tokio::test]
#[serial_test::serial]
async fn web_search_resolves_an_env_backed_credential_header() {
    const VAR: &str = "MEDLEY_TEST_WS_ENV_HEADER_160";
    let _guard = xai_grok_test_support::EnvGuard::set(VAR, "env-sourced-secret");

    let headers = web_search_request_headers("env-hdr", |base_url| {
        xai_grok_tools::implementations::web_search::WebSearchConfig::Enabled {
            api_key: None,
            base_url,
            model: "env-hdr-search".to_string(),
            extra_headers: Default::default(),
            env_http_headers: [("x-api-key".to_string(), VAR.to_string())]
                .into_iter()
                .collect(),
            alpha_test_key: None,
            api_key_provider: None,
        }
    })
    .await;

    assert_eq!(
        headers.get("x-api-key").and_then(|v| v.to_str().ok()),
        Some("env-sourced-secret"),
        "an env-backed credential header must be resolved onto the request"
    );
    assert!(
        headers.get(axum::http::header::AUTHORIZATION).is_none(),
        "still no invented bearer for a header-authenticated route",
    );
}

/// #160: `env_http_headers` is applied **after** `extra_headers`, so an
/// env-backed header wins over a literal of the same name — the sampler's
/// order, and the one that lets a user override a checked-in placeholder from
/// their environment.
///
/// Swapping the two loops in `web_search/client.rs` fails this test; nothing
/// else pins the ordering.
#[tokio::test]
#[serial_test::serial]
async fn web_search_env_header_overrides_a_literal_of_the_same_name() {
    const VAR: &str = "MEDLEY_TEST_WS_ENV_OVERRIDE_160";
    let _guard = xai_grok_test_support::EnvGuard::set(VAR, "from-environment");

    let headers = web_search_request_headers("env-override", |base_url| {
        xai_grok_tools::implementations::web_search::WebSearchConfig::Enabled {
            api_key: None,
            base_url,
            model: "env-hdr-search".to_string(),
            extra_headers: [("x-api-key".to_string(), "from-literal".to_string())]
                .into_iter()
                .collect(),
            env_http_headers: [("x-api-key".to_string(), VAR.to_string())]
                .into_iter()
                .collect(),
            alpha_test_key: None,
            api_key_provider: None,
        }
    })
    .await;

    assert_eq!(
        headers.get("x-api-key").and_then(|v| v.to_str().ok()),
        Some("from-environment"),
        "the env-backed header must win over the literal of the same name"
    );
}

#[tokio::test]
async fn web_search_errors_when_configured_model_cannot_be_resolved() {
    let builder = crate::tools::bridge::ToolBridge::get_builder();
    let config = ToolServerConfig {
        tools: vec![ToolConfig {
            id: "GrokBuild:web_search".into(),
            params: None,
            name_override: None,
            params_name_overrides: None,
            description_override: None,
            behavior_version: None,
            kind: None,
        }],
        behavior_preset: None,
    };
    let fs: std::sync::Arc<dyn AsyncFileSystem> = std::sync::Arc::new(LocalFs);
    let terminal: std::sync::Arc<dyn TerminalBackend> =
        std::sync::Arc::new(LocalTerminalBackend::new());
    let ctx = SessionContext {
        backend: terminal,
        fs,
        cwd: std::env::temp_dir(),
        session_folder: std::env::temp_dir().join("grok-web-search-disabled"),
        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: std::env::temp_dir().join("grok-web-search-disabled/state.json"),
        memory_backend: None,
        web_search_config: xai_grok_tools::implementations::web_search::WebSearchConfig::Disabled,
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: Default::default(),
        video_gen_config: Default::default(),
        app_builder_deployer_config: Default::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: xai_grok_tools::reminders::DEFAULT_REMINDER_TAG,
    };
    let bridge = crate::tools::bridge::ToolBridge::finalize_builder(builder, config, ctx)
        .await
        .expect("finalize_builder should succeed");
    let result = bridge
        .call(
            "web_search",
            json!({
                "query": "test query"
            }),
            "web-search-disabled",
        )
        .await;
    assert!(result.is_err(), "web_search should fail when disabled");
}
