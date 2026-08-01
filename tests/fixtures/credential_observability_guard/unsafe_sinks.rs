fn unsafe_sinks(
    notification_command: &str,
    secret: &str,
    proxy_base_url: &str,
    err: &reqwest::Error,
    response_body: &str,
    body_preview: &str,
) -> anyhow::Error {
    tracing::warn!(command = %notification_command, "notification failed");
    tracing::debug!(token = %redact_middle(secret), "partially redacted credential");
    tracing::error!(url = %proxy_base_url, "authenticated request failed");
    tracing::warn!(error = %err, "provider HTTP request failed");
    tracing::error!(body = %response_body, "provider response failed");
    tracing::debug!(payload = %compact_json(&request), "ACP request");
    anyhow::anyhow!("provider response preview: {body_preview}")
}

#[derive(Debug, thiserror::Error)]
enum ProviderHttpError {
    #[error("request failed")]
    Transport(#[from] reqwest::Error),
    #[error("request to {config_url} failed: {response_body}")]
    Status {
        config_url: String,
        response_body: String,
    },
}
