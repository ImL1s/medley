fn direct_and_aliased_credentials(config: &Config, self_ref: &Session) {
    tracing::debug!(?config.api_key, "direct config credential");
    tracing::debug!(%self_ref.access_token, "direct session credential");
    tracing::debug!(%config.alpha_test_key, "direct alpha credential");
    tracing::debug!(%config.authorization, "direct authorization header");
    tracing::debug!(%config.x_api_key, "direct x-api-key header");
    tracing::debug!(%config.management_api_key, "direct management credential");
    tracing::debug!(%config.events_api_key, "direct events credential");
    tracing::debug!(%config.mixpanel_token, "direct telemetry credential");
    tracing::debug!(%config.jwt_token, "direct JWT credential");

    let value = config.deployment_key.clone();
    tracing::warn!(%value, "aliased deployment credential");

    let field_read = self_ref.refresh_token.as_str();
    tracing::error!("aliased refresh credential: {field_read}");

    let endpoint = config.grok_ws_url.clone();
    tracing::info!(%endpoint, "aliased credential-capable URL");

    tracing::warn!(%config.cli_chat_proxy_base_url, "raw CLI proxy URL");
    tracing::warn!(%config.xai_api_base_url, "raw xAI API URL");
    tracing::warn!(%config.hub_url, "raw hub URL");
    tracing::warn!(%config.npm_registry, "raw registry URL");
    tracing::warn!(%config.grok_ws_origin, "raw websocket origin");
}

#[derive(Debug, thiserror::Error)]
#[error("authentication failed for {client_secret}")]
struct UnsafeAuthError {
    client_secret: String,
}
