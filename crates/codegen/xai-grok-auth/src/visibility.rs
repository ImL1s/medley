/// Apply auth headers to outbound visibility requests.
/// Implemented by `xai-grok-shell::util::grok_auth_credentials::GrokAuthCredentials`
/// to keep credential construction owned by shell while letting data-collector
/// build the request without reaching back into shell types.
pub trait HttpAuth: Send + Sync {
    fn apply(&self, builder: reqwest::RequestBuilder, base_url: &str) -> reqwest::RequestBuilder;

    /// Whether bearer requests require the user/OAuth companion header.
    /// Deployment keys use a bare `Authorization: Bearer ...` header.
    fn needs_token_auth_header(&self) -> bool {
        true
    }
}
