use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use xai_grok_auth::CredentialComparison;

/// Resolves the current API key for tool HTTP requests.
pub trait ApiKeyProvider: Send + Sync + 'static {
    /// Sync cached read (no refresh). Override point for static providers.
    fn current_api_key(&self) -> Option<String>;

    /// Per-request resolve. `AuthManager` overrides this to drive the
    /// refresh chain; default delegates to the sync method.
    fn current_api_key_async(&self) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        Box::pin(std::future::ready(self.current_api_key()))
    }

    /// Compare against the same async resolution ladder used to build tool
    /// requests. This matters when session refresh fails and resolution falls
    /// through to a static key that differs from the sync cached candidate.
    fn compare_sent_credential<'a>(
        &'a self,
        sent: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = CredentialComparison> + Send + 'a>> {
        Box::pin(async move {
            let current = self.current_api_key_async().await;
            CredentialComparison::compare(sent, current.as_deref())
        })
    }
}

/// Shared provider used across tool clients.
pub type SharedApiKeyProvider = Arc<dyn ApiKeyProvider>;

/// Resolve the bearer for the next request from the provider.
pub(crate) async fn resolve_bearer(provider: Option<&SharedApiKeyProvider>) -> Option<String> {
    match provider {
        Some(p) => p.current_api_key_async().await,
        None => None,
    }
}

pub(crate) async fn compare_sent_bearer(
    provider: Option<&SharedApiKeyProvider>,
    sent: Option<&str>,
) -> CredentialComparison {
    match provider {
        Some(provider) => provider.compare_sent_credential(sent).await,
        None => CredentialComparison::compare(sent, None),
    }
}

/// Return the bearer credential from the fully-built request. Building the
/// request first is important: this observes client default headers as well as
/// any per-request override, so attribution describes the credential that was
/// actually sent on the wire.
pub(crate) fn request_credential(request: &reqwest::Request) -> Option<String> {
    let bearer = request
        .headers()
        .get(reqwest::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    bearer.or_else(|| {
        request
            .headers()
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    })
}

pub(crate) fn is_auth_failure(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_credential_reads_final_bearer_override() {
        let client = reqwest::Client::builder()
            .default_headers(reqwest::header::HeaderMap::from_iter([(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_static("Bearer default-secret"),
            )]))
            .build()
            .unwrap();
        let request = client
            .get("https://example.com")
            .header(reqwest::header::AUTHORIZATION, "Bearer final-secret")
            .build()
            .unwrap();
        assert_eq!(
            request_credential(&request).as_deref(),
            Some("final-secret")
        );
    }

    #[test]
    fn request_credential_falls_back_to_x_api_key() {
        let request = reqwest::Client::new()
            .get("https://example.com")
            .header("x-api-key", "x-api-final-secret")
            .build()
            .unwrap();
        assert_eq!(
            request_credential(&request).as_deref(),
            Some("x-api-final-secret")
        );
    }
}
