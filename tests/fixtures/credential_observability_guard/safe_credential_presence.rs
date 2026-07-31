fn presence_only(config: &Config, self_ref: &Session) {
    tracing::debug!(api_key_present = config.api_key.is_some());
    tracing::debug!(token_len = self_ref.access_token.len());
    tracing::debug!(secret_present = config.client_secret.as_ref().is_some());
}

#[derive(Clone)]
struct SafePresenceCarrier {
    deployment_key: Option<String>,
}

impl std::fmt::Debug for SafePresenceCarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SafePresenceCarrier")
            .field("deployment_key_present", &self.deployment_key.is_some())
            .finish()
    }
}

fn generic_credential_presence(auth: &GrokAuth, snapshot: &CredentialSnapshot) {
    tracing::debug!(
        auth_key_present = !auth.key.is_empty(),
        snapshot_token_present = !snapshot.token.is_empty(),
    );
}
