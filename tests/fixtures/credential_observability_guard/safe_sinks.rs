struct CredentialEnvelope {
    inner: Credentials,
}

impl std::fmt::Debug for CredentialEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialEnvelope")
            .field("credential_present", &true)
            .finish()
    }
}

fn safe_sinks(
    notification_command: Option<&str>,
    proxy_base_url: Option<&str>,
    log_url: &str,
    err: &reqwest::Error,
    status: u16,
) {
    tracing::warn!(
        command_configured = notification_command.is_some(),
        proxy_url_present = proxy_base_url.is_some(),
        url = %log_url,
        error = %err.without_url(),
        status,
        "provider request failed"
    );
}
