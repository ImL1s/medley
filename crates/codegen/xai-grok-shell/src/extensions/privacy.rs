//! `x.ai/privacy/setCodingDataRetention` extension handler.
//!
//! PUTs the new opt-out flag to cli-chat-proxy and updates local auth state
//! to match. The local update is fire-and-forget (best-effort cache refresh).

use agent_client_protocol as acp;
use serde::Deserialize;

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;

fn privacy_failure(error_class: &'static str, status: Option<u16>) -> acp::Error {
    tracing::warn!(error_class, status, "setCodingDataRetention request failed");
    let message = match status {
        Some(status) => format!("Privacy service returned HTTP {status}"),
        None => "Privacy service request failed".to_owned(),
    };
    acp::Error::internal_error().data(message)
}

async fn send_privacy_request(
    request: reqwest_middleware::RequestBuilder,
) -> Result<reqwest::Response, acp::Error> {
    let response = request
        .send()
        .await
        .map_err(|_| privacy_failure("request_transport_failed", None))?;
    if !response.status().is_success() {
        return Err(privacy_failure(
            "upstream_status",
            Some(response.status().as_u16()),
        ));
    }
    Ok(response)
}

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/privacy/setCodingDataRetention" => handle_set(agent, args).await,
        _ => Err(acp::Error::method_not_found()),
    }
}

async fn handle_set(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Params {
        coding_data_retention_opt_out: bool,
    }

    let params: Params = parse_params(args)?;

    let auth = agent.auth_manager.auth().await.map_err(|_| {
        tracing::warn!(
            error_class = "auth_resolution_failed",
            "privacy: auth resolution failed"
        );
        acp::Error::auth_required().data(crate::auth::with_login_instruction(
            |prog| format!("Authentication required. Run `{prog} login` to re-authenticate."),
            "Authentication required. Sign in again to re-authenticate.",
        ))
    })?;

    let proxy_url = agent.cfg.borrow().endpoints.proxy_url();
    let url = format!("{proxy_url}/privacy/coding-data-retention");
    let token_header = agent.auth_manager.grok_com_config().token_header.clone();

    let body = serde_json::json!({
        "codingDataRetentionOptOut": params.coding_data_retention_opt_out,
    });

    let provider: std::sync::Arc<dyn xai_grok_auth::AuthCredentialProvider> = std::sync::Arc::new(
        crate::auth::credential_provider::ShellAuthCredentialProvider::new(
            agent.auth_manager.clone(),
            None,
            None,
        ),
    );
    let client = crate::http::with_auth_retry(crate::http::shared_client(), provider);

    let _response = send_privacy_request(
        client
            .put(&url)
            .header("X-XAI-Token-Auth", &token_header)
            .header("x-grok-client-version", xai_grok_version::VERSION)
            .header(
                crate::http::CLIENT_MODE_HEADER,
                crate::http::process_client_mode(),
            )
            .json(&body),
    )
    .await?;

    // Update local auth state to reflect the change.
    // Use save_without_enrichment to avoid a race: update() spawns a
    // background GET /user enrichment that may read stale ACL state
    // and overwrite the opt-out flag back to its previous value.
    let mut updated = auth.clone();
    updated.coding_data_retention_opt_out = params.coding_data_retention_opt_out;
    let _ = agent.auth_manager.save_without_enrichment(updated).await;

    to_raw_response(&serde_json::json!({
        "codingDataRetentionOptOut": params.coding_data_retention_opt_out,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<String>>>);

    struct EventVisitor<'a>(&'a mut String);

    impl tracing::field::Visit for EventVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write;
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }

    impl tracing::Subscriber for LogCapture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut rendered = String::new();
            event.record(&mut EventVisitor(&mut rendered));
            self.0.lock().expect("log capture lock").push(rendered);
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    fn assert_sentinel_absent(rendered: &str, sentinel: &str) {
        assert!(
            !rendered.contains(sentinel),
            "leaked full sentinel: {rendered}"
        );
        for window in sentinel.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).expect("ASCII sentinel window");
            assert!(
                !rendered.contains(window),
                "leaked sentinel window {window:?}: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn authenticated_status_error_hides_url_and_reflected_body() {
        use axum::{Router, routing::put};
        use tokio::net::TcpListener;

        let sentinel = "cred_SENTINEL_0123456789abcdef";
        let reflected = serde_json::json!({"message": sentinel});
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/privacy",
                    put(move || {
                        let reflected = reflected.clone();
                        async move { (axum::http::StatusCode::BAD_GATEWAY, axum::Json(reflected)) }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let capture = LogCapture::default();
        let _guard = tracing::subscriber::set_default(capture.clone());
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let url = format!("http://{sentinel}:password@{addr}/privacy?token={sentinel}");

        let error = send_privacy_request(client.put(url).bearer_auth(sentinel))
            .await
            .expect_err("reflected upstream error must fail");
        let logs = capture.0.lock().expect("log capture lock").join("\n");
        let rendered = format!(
            "{error:?} {} {logs}",
            serde_json::to_string(&error).unwrap()
        );

        assert!(rendered.contains("upstream_status"));
        assert!(rendered.contains("502"));
        assert_sentinel_absent(&rendered, sentinel);
        server.abort();
    }

    #[tokio::test]
    async fn authenticated_transport_error_hides_url_and_credentials() {
        let sentinel = "cred_SENTINEL_0123456789abcdef";
        let capture = LogCapture::default();
        let _guard = tracing::subscriber::set_default(capture.clone());
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let url = format!("http://{sentinel}:password@127.0.0.1:1/privacy?token={sentinel}");

        let error = send_privacy_request(client.put(url).bearer_auth(sentinel))
            .await
            .expect_err("dead loopback port must fail");
        let logs = capture.0.lock().expect("log capture lock").join("\n");
        let rendered = format!(
            "{error:?} {} {logs}",
            serde_json::to_string(&error).unwrap()
        );

        assert!(rendered.contains("request_transport_failed"));
        assert_sentinel_absent(&rendered, sentinel);
    }
}
