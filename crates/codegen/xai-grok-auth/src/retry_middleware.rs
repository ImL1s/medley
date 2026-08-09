//! `reqwest-middleware` layer: stamps auth headers and retries on 401.
//! Gated behind the `middleware` cargo feature.

use std::sync::Arc;

use reqwest::{Request, Response, StatusCode, header::HeaderValue};
use reqwest_middleware::{Error, Middleware, Next};

use crate::{AuthCredentialProvider, CredentialComparison};

/// Remove request URLs (which may contain userinfo or signed-query credentials)
/// before an HTTP error crosses the auth middleware boundary. Middleware errors
/// are opaque, so fail closed to a fixed category instead of retaining their
/// provider-controlled source chain.
fn sanitize_middleware_error(error: Error) -> Error {
    match error {
        Error::Reqwest(error) => Error::Reqwest(error.without_url()),
        Error::Middleware(_) => Error::middleware(std::io::Error::other("HTTP middleware failure")),
    }
}

/// Execute a request and return the response with the secret-free comparison
/// produced for the final request attempt.
pub async fn execute_with_auth_relation(
    client: &reqwest_middleware::ClientWithMiddleware,
    req: Request,
) -> reqwest_middleware::Result<(Response, CredentialComparison)> {
    let mut ext = http::Extensions::new();
    let resp = client
        .execute_with_extensions(req, &mut ext)
        .await
        .map_err(sanitize_middleware_error)?;
    let comparison = ext.get::<CredentialComparison>().copied().ok_or_else(|| {
        Error::middleware(std::io::Error::other(
            "credential relation unavailable: auth middleware did not record final attempt",
        ))
    })?;
    Ok((resp, comparison))
}

pub struct AuthRetryMiddleware {
    credentials: Arc<dyn AuthCredentialProvider>,
    max_retries: u32,
}

impl AuthRetryMiddleware {
    pub fn new(credentials: Arc<dyn AuthCredentialProvider>, max_retries: u32) -> Self {
        Self {
            credentials,
            max_retries,
        }
    }
}

fn apply_auth_headers(
    req: &mut Request,
    token: Option<&str>,
    needs_token_auth_header: bool,
) -> bool {
    let token_auth_header = reqwest::header::HeaderName::from_static("x-xai-token-auth");
    req.headers_mut().remove(reqwest::header::AUTHORIZATION);
    req.headers_mut().remove(&token_auth_header);
    let Some(token) = token else {
        return false;
    };
    match HeaderValue::from_str(&format!("Bearer {token}")) {
        Ok(val) => {
            req.headers_mut()
                .insert(reqwest::header::AUTHORIZATION, val);
            if needs_token_auth_header {
                req.headers_mut()
                    .insert(token_auth_header, HeaderValue::from_static("xai-grok-cli"));
            }
            true
        }
        Err(_) => {
            tracing::warn!("auth retry: failed to build Authorization header");
            false
        }
    }
}

#[async_trait::async_trait]
impl Middleware for AuthRetryMiddleware {
    async fn handle(
        &self,
        mut req: Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> Result<Response, Error> {
        let token = self.credentials.snapshot().token;
        let applied = apply_auth_headers(
            &mut req,
            token.as_deref(),
            self.credentials.needs_token_auth_header(),
        );

        let backup = req.try_clone();
        let resp = next
            .clone()
            .run(req, extensions)
            .await
            .map_err(sanitize_middleware_error)?;
        extensions.insert(
            self.credentials
                .compare_sent_credential(applied.then_some(token.as_deref()).flatten()),
        );

        if resp.status() != StatusCode::UNAUTHORIZED || self.max_retries == 0 {
            return Ok(resp);
        }
        let Some(backup) = backup else {
            return Ok(resp);
        };

        let mut last_resp = resp;
        for _ in 0..self.max_retries {
            if !self.credentials.refresh_after_unauthorized().await {
                break;
            }
            let Some(mut retry) = backup.try_clone() else {
                break;
            };
            let token = self.credentials.snapshot().token;
            if token.is_none() {
                break;
            }
            let applied = apply_auth_headers(
                &mut retry,
                token.as_deref(),
                self.credentials.needs_token_auth_header(),
            );
            last_resp = next
                .clone()
                .run(retry, extensions)
                .await
                .map_err(sanitize_middleware_error)?;
            extensions.insert(
                self.credentials
                    .compare_sent_credential(applied.then_some(token.as_deref()).flatten()),
            );
            if last_resp.status() != StatusCode::UNAUTHORIZED {
                return Ok(last_resp);
            }
        }

        Ok(last_resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CredentialSnapshot, HttpAuth};
    use reqwest_middleware::ClientBuilder;
    use std::sync::Mutex;

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

    struct MockProvider {
        token: Mutex<Option<String>>,
        refresh_result: bool,
        refresh_count: Mutex<u32>,
    }

    impl MockProvider {
        fn new(token: Option<&str>, refresh_result: bool) -> Self {
            Self {
                token: Mutex::new(token.map(|s| s.to_owned())),
                refresh_result,
                refresh_count: Mutex::new(0),
            }
        }

        fn refresh_count(&self) -> u32 {
            *self.refresh_count.lock().unwrap()
        }
    }

    impl HttpAuth for MockProvider {
        fn apply(&self, b: reqwest::RequestBuilder, _: &str) -> reqwest::RequestBuilder {
            b
        }
    }

    #[async_trait::async_trait]
    impl AuthCredentialProvider for MockProvider {
        fn snapshot(&self) -> CredentialSnapshot {
            CredentialSnapshot {
                token: self.token.lock().unwrap().clone(),
                ..Default::default()
            }
        }

        async fn refresh_after_unauthorized(&self) -> bool {
            *self.refresh_count.lock().unwrap() += 1;
            self.refresh_result
        }
    }

    async fn build_client(
        provider: Arc<dyn AuthCredentialProvider>,
        max_retries: u32,
    ) -> reqwest_middleware::ClientWithMiddleware {
        ClientBuilder::new(reqwest::Client::new())
            .with(AuthRetryMiddleware::new(provider, max_retries))
            .build()
    }

    #[tokio::test]
    async fn execute_without_auth_middleware_fails_closed() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/")
            .with_status(200)
            .expect(1)
            .create_async()
            .await;
        let client = ClientBuilder::new(reqwest::Client::new()).build();
        let request = reqwest::Client::new()
            .get(server.url())
            .build()
            .expect("request");

        let error = execute_with_auth_relation(&client, request)
            .await
            .expect_err("missing auth middleware must not fabricate not_sent");
        assert!(
            error
                .to_string()
                .contains("credential relation unavailable")
        );
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn transport_errors_strip_userinfo_and_query_credentials() {
        let sentinel = "cred_SENTINEL_0123456789abcdef";
        let url = format!("http://{sentinel}:password@127.0.0.1:1/path?token={sentinel}");
        let provider = Arc::new(MockProvider::new(None, false));
        let client = build_client(provider, 0).await;

        let error = client
            .get(url)
            .send()
            .await
            .expect_err("dead loopback port must fail");
        let rendered = format!("{error} {error:?}");

        assert_sentinel_absent(&rendered, sentinel);
    }

    #[test]
    fn opaque_middleware_errors_drop_provider_controlled_sources() {
        let sentinel = "cred_SENTINEL_0123456789abcdef";
        let raw = Error::middleware(std::io::Error::other(format!(
            "failed for http://{sentinel}:password@example.test/path?token={sentinel}"
        )));

        let error = sanitize_middleware_error(raw);
        let rendered = format!("{error} {error:?}");

        assert!(rendered.contains("HTTP middleware failure"));
        assert_sentinel_absent(&rendered, sentinel);
    }

    #[tokio::test]
    async fn test_401_no_refresh_returns_401() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/")
            .with_status(401)
            .expect(1)
            .create_async()
            .await;

        let p = Arc::new(MockProvider::new(Some("tok"), false));
        let client = build_client(p.clone(), 1).await;

        let resp = client.get(server.url()).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(p.refresh_count(), 1);
        m.assert_async().await;
    }

    /// Simulates a real auth manager: starts with stale token, refresh swaps to fresh.
    struct SimulatedAuthManager {
        token: Mutex<Option<String>>,
        fresh_token: String,
        refresh_count: Mutex<u32>,
    }

    impl SimulatedAuthManager {
        fn simulated(stale: &str, fresh: &str) -> Self {
            Self {
                token: Mutex::new(Some(stale.to_owned())),
                fresh_token: fresh.to_owned(),
                refresh_count: Mutex::new(0),
            }
        }
    }

    impl HttpAuth for SimulatedAuthManager {
        fn apply(&self, b: reqwest::RequestBuilder, _: &str) -> reqwest::RequestBuilder {
            b
        }
    }

    #[async_trait::async_trait]
    impl AuthCredentialProvider for SimulatedAuthManager {
        fn snapshot(&self) -> CredentialSnapshot {
            CredentialSnapshot {
                token: self.token.lock().unwrap().clone(),
                ..Default::default()
            }
        }

        async fn refresh_after_unauthorized(&self) -> bool {
            *self.refresh_count.lock().unwrap() += 1;
            *self.token.lock().unwrap() = Some(self.fresh_token.clone());
            true
        }
    }

    /// Models a refresh operation that succeeds as an operation but leaves no
    /// wire-valid credential (for example, a revoked login removed from disk).
    struct CredentialRemovedByRefresh {
        token: Mutex<Option<String>>,
        refresh_count: Mutex<u32>,
    }

    impl CredentialRemovedByRefresh {
        fn new(token: &str) -> Self {
            Self {
                token: Mutex::new(Some(token.to_owned())),
                refresh_count: Mutex::new(0),
            }
        }
    }

    impl HttpAuth for CredentialRemovedByRefresh {
        fn apply(&self, b: reqwest::RequestBuilder, _: &str) -> reqwest::RequestBuilder {
            b
        }
    }

    #[async_trait::async_trait]
    impl AuthCredentialProvider for CredentialRemovedByRefresh {
        fn snapshot(&self) -> CredentialSnapshot {
            CredentialSnapshot {
                token: self.token.lock().unwrap().clone(),
                ..Default::default()
            }
        }

        async fn refresh_after_unauthorized(&self) -> bool {
            *self.refresh_count.lock().unwrap() += 1;
            *self.token.lock().unwrap() = None;
            true
        }
    }

    #[tokio::test]
    async fn test_e2e_stale_token_refreshed_and_retried() {
        let mut server = mockito::Server::new_async().await;

        let m401 = server
            .mock("GET", "/api")
            .match_header("authorization", "Bearer stale-token")
            .with_status(401)
            .create_async()
            .await;
        let m200 = server
            .mock("GET", "/api")
            .match_header("authorization", "Bearer fresh-token")
            .with_status(200)
            .with_body(r#"{"ok":true}"#)
            .create_async()
            .await;

        let p = Arc::new(SimulatedAuthManager::simulated(
            "stale-token",
            "fresh-token",
        ));
        let client = build_client(p.clone(), 1).await;

        let resp = client
            .get(format!("{}/api", server.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(*p.refresh_count.lock().unwrap(), 1);
        m401.assert_async().await;
        m200.assert_async().await;
    }

    #[tokio::test]
    async fn test_e2e_auth_header_stamped_automatically() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/api")
            .match_header("authorization", "Bearer my-token")
            .match_header("x-xai-token-auth", "xai-grok-cli")
            .with_status(200)
            .create_async()
            .await;

        let p = Arc::new(MockProvider::new(Some("my-token"), false));
        let client = build_client(p.clone(), 1).await;

        let resp = client
            .get(format!("{}/api", server.url()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(p.refresh_count(), 0);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn refresh_that_removes_token_keeps_original_response_and_comparison() {
        let mut server = mockito::Server::new_async().await;
        let unauthorized = server
            .mock("GET", "/api")
            .match_header("authorization", "Bearer stale-token")
            .match_header("x-xai-token-auth", "xai-grok-cli")
            .with_status(401)
            .expect(1)
            .create_async()
            .await;

        let provider = Arc::new(CredentialRemovedByRefresh::new("stale-token"));
        let client = build_client(provider.clone(), 1).await;
        let request = client.get(format!("{}/api", server.url())).build().unwrap();

        let (response, comparison) = execute_with_auth_relation(&client, request).await.unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(comparison, CredentialComparison::same_as_current());
        assert_eq!(*provider.refresh_count.lock().unwrap(), 1);
        unauthorized.assert_async().await;
    }

    /// The stamp must describe the bearer of the attempt whose response
    /// the caller holds: after a 401 → refresh → retry, that is the
    /// FRESH token, not the stale one stamped on the first attempt.
    #[tokio::test]
    async fn execute_with_auth_relation_reports_final_attempt() {
        let mut server = mockito::Server::new_async().await;
        let m401 = server
            .mock("GET", "/api")
            .match_header("authorization", "Bearer stale-token")
            .with_status(401)
            .create_async()
            .await;
        let m200 = server
            .mock("GET", "/api")
            .match_header("authorization", "Bearer fresh-token")
            .with_status(200)
            .create_async()
            .await;

        let p = Arc::new(SimulatedAuthManager::simulated(
            "stale-token",
            "fresh-token",
        ));
        let client = build_client(p, 1).await;

        let req = client.get(format!("{}/api", server.url())).build().unwrap();
        let (resp, comparison) = execute_with_auth_relation(&client, req).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(comparison, CredentialComparison::same_as_current());
        m401.assert_async().await;
        m200.assert_async().await;
    }

    /// No credential ⇒ no stamp: attribution must see "nothing was sent",
    /// not an empty string or a stale record.
    #[tokio::test]
    async fn execute_with_auth_relation_reports_not_sent() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/")
            .with_status(401)
            .create_async()
            .await;

        let p = Arc::new(MockProvider::new(None, false));
        let client = build_client(p, 0).await;

        let req = client.get(server.url()).build().unwrap();
        let (resp, comparison) = execute_with_auth_relation(&client, req).await.unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(comparison, CredentialComparison::not_sent(false));
        m.assert_async().await;
    }

    #[test]
    fn invalid_header_is_not_sent_and_removes_existing_authorization() {
        let mut req = reqwest::Request::new(
            reqwest::Method::GET,
            "https://example.test".parse().unwrap(),
        );
        req.headers_mut().insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer stale"),
        );
        assert!(!apply_auth_headers(
            &mut req,
            Some("invalid\ncredential"),
            true
        ));
        assert!(req.headers().get(reqwest::header::AUTHORIZATION).is_none());
    }
    #[tokio::test]
    async fn test_max_retries_bounds_attempts() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/")
            .with_status(401)
            .expect(4)
            .create_async()
            .await;

        let p = Arc::new(MockProvider::new(Some("tok"), true));
        let client = build_client(p.clone(), 3).await;

        let resp = client.get(server.url()).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(p.refresh_count(), 3);
        m.assert_async().await;
    }
}
