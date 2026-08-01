//! Provider-scoped OpenAI Codex OAuth compatibility surface.
//!
//! The constants in this module mirror the public-client contract used by the
//! official Codex CLI. They are intentionally centralized because the service
//! is not the OpenAI Platform API and can evolve independently.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::{Router, extract::Query, http::StatusCode, response::Html, routing::get};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::error::RefreshTokenFailedReason;
use super::oidc::OidcRefreshResult;
use super::{AuthManager, AuthMode, GrokAuth, ProviderCredentialSnapshot};

pub const AUTH_SCOPE: &str = "openai::codex";
pub const ISSUER: &str = "https://auth.openai.com";
pub const AUTHORIZE_ENDPOINT: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_API_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub const ORIGINATOR: &str = "grok_build";
pub const CALLBACK_PORTS: &[u16] = &[1455, 1457];
pub const AUTH_CLAIM_NAMESPACE: &str = "https://api.openai.com/auth";
pub const ACCOUNT_ID_CLAIM: &str = "https://api.openai.com/auth/chatgpt_account_id";

const CALLBACK_PATH: &str = "/auth/callback";
const CALLBACK_TIMEOUT: StdDuration = StdDuration::from_secs(600);
const REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum CodexOAuthError {
    #[error("failed to bind the Codex OAuth loopback callback")]
    BindLoopback,
    #[error("Codex OAuth callback timed out")]
    CallbackTimeout,
    #[error("Codex OAuth login was cancelled")]
    Cancelled,
    #[error("Codex OAuth callback was rejected: {0}")]
    Callback(&'static str),
    #[error("Codex OAuth state mismatch")]
    StateMismatch,
    #[error("Codex OAuth token request failed: HTTP {status}")]
    TokenHttp { status: u16 },
    #[error("Codex OAuth token response was invalid")]
    InvalidTokenResponse,
    #[error("failed to persist Codex OAuth credential: {0}")]
    Persist(#[from] std::io::Error),
    #[error("Codex OAuth credential store is busy")]
    StoreBusy,
    #[error("Codex OAuth network request failed")]
    Network,
}

/// Non-secret readiness information for provider status UIs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAuthStatus {
    pub signed_in: bool,
    pub expired: bool,
    pub refreshable: bool,
    pub account_id_present: bool,
    pub expires_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Clone)]
struct Pkce {
    verifier: String,
    challenge: String,
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier_present", &true)
            .field("challenge_present", &true)
            .finish()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token_present", &!self.access_token.is_empty())
            .field("refresh_token_present", &self.refresh_token.is_some())
            .field("id_token_present", &self.id_token.is_some())
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    #[serde(default)]
    error: Option<String>,
}

fn recognized_oauth_error(code: Option<String>) -> Option<String> {
    match code.as_deref() {
        Some("invalid_grant") | Some("invalid_client") => code,
        _ => None,
    }
}

#[derive(Debug)]
struct TokenRequestError {
    status: Option<u16>,
    oauth_code: Option<String>,
    network_unreachable: bool,
}

#[derive(Deserialize)]
struct Callback {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Clone, Copy)]
enum CallbackPage {
    Success,
    Failure,
}

struct CallbackSession {
    callback: Option<Callback>,
    completion: Option<tokio::sync::oneshot::Sender<CallbackPage>>,
    shutdown: CancellationToken,
    server_task: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

impl CallbackSession {
    fn take_callback(&mut self) -> Callback {
        self.callback
            .take()
            .expect("callback is consumed exactly once")
    }

    async fn complete(mut self, page: CallbackPage) {
        if let Some(completion) = self.completion.take() {
            let _ = completion.send(page);
        }
        self.shutdown.cancel();
        if tokio::time::timeout(StdDuration::from_secs(2), &mut self.server_task)
            .await
            .is_err()
        {
            self.server_task.abort();
            let _ = (&mut self.server_task).await;
        }
    }
}

impl Drop for CallbackSession {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.server_task.abort();
    }
}

fn generate_pkce() -> Pkce {
    let random_bytes: [u8; 64] = rand::random();
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

fn generate_state() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

fn redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}{CALLBACK_PATH}")
}

fn build_authorize_url(redirect_uri: &str, pkce: &Pkce, state: &str) -> String {
    let mut url = url::Url::parse(AUTHORIZE_ENDPOINT).expect("constant authorize URL is valid");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR);
    url.into()
}

fn safe_callback_error(value: Option<&str>) -> &'static str {
    match value {
        Some("access_denied") => "access_denied",
        Some("invalid_request") => "invalid_request",
        Some("unauthorized_client") => "unauthorized_client",
        Some("invalid_scope") => "invalid_scope",
        Some("server_error") => "server_error",
        Some("temporarily_unavailable") => "temporarily_unavailable",
        _ => "oauth_callback_error",
    }
}

fn validate_state(expected: &str, received: Option<&str>) -> Result<(), CodexOAuthError> {
    if received == Some(expected) {
        Ok(())
    } else {
        Err(CodexOAuthError::StateMismatch)
    }
}

async fn bind_callback() -> Result<(TcpListener, u16), CodexOAuthError> {
    for port in CALLBACK_PORTS {
        if let Ok(listener) = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port)).await {
            return Ok((listener, *port));
        }
    }
    Err(CodexOAuthError::BindLoopback)
}

async fn wait_for_callback(
    listener: TcpListener,
    cancellation: CancellationToken,
    expected_state: &str,
) -> Result<CallbackSession, CodexOAuthError> {
    wait_for_callback_with_timeout(listener, cancellation, expected_state, CALLBACK_TIMEOUT).await
}

async fn wait_for_callback_with_timeout(
    listener: TcpListener,
    cancellation: CancellationToken,
    expected_state: &str,
    timeout: StdDuration,
) -> Result<CallbackSession, CodexOAuthError> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let expected_state = expected_state.to_owned();
    let router = Router::new().route(
        CALLBACK_PATH,
        get(move |query: Query<Callback>| {
            let tx = tx.clone();
            let expected_state = expected_state.clone();
            async move {
                let callback = query.0;
                if validate_state(&expected_state, callback.state.as_deref()).is_err() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Html("Sign-in request did not match. Return to Grok Build."),
                    );
                }
                let (completion, completed) = tokio::sync::oneshot::channel();
                if tx.try_send((callback, completion)).is_err() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Html("Sign-in failed. Return to Grok Build."),
                    );
                }
                match completed.await.unwrap_or(CallbackPage::Failure) {
                    CallbackPage::Success => (
                        StatusCode::OK,
                        Html("Signed in. You can close this window."),
                    ),
                    CallbackPage::Failure => (
                        StatusCode::BAD_REQUEST,
                        Html("Sign-in failed. Return to Grok Build."),
                    ),
                }
            }
        }),
    );
    let shutdown = CancellationToken::new();
    let server =
        axum::serve(listener, router).with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let server_task = tokio::spawn(async move { server.await });
    let result = tokio::select! {
        callback = rx.recv() => callback.ok_or(CodexOAuthError::Callback("callback_channel_closed")),
        _ = tokio::time::sleep(timeout) => Err(CodexOAuthError::CallbackTimeout),
        _ = cancellation.cancelled() => Err(CodexOAuthError::Cancelled),
    };
    match result {
        Ok((callback, completion)) => Ok(CallbackSession {
            callback: Some(callback),
            completion: Some(completion),
            shutdown,
            server_task,
        }),
        Err(error) => {
            shutdown.cancel();
            let mut server_task = server_task;
            if tokio::time::timeout(StdDuration::from_secs(2), &mut server_task)
                .await
                .is_err()
            {
                server_task.abort();
                let _ = server_task.await;
            }
            Err(error)
        }
    }
}

fn validated_callback_code(
    callback: Callback,
    expected_state: &str,
) -> Result<String, CodexOAuthError> {
    if let Some(error) = callback.error.as_deref() {
        return Err(CodexOAuthError::Callback(safe_callback_error(Some(error))));
    }
    validate_state(expected_state, callback.state.as_deref())?;
    callback
        .code
        .filter(|code| !code.is_empty())
        .ok_or(CodexOAuthError::Callback("missing_code"))
}

fn token_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("Codex OAuth HTTP client configuration is valid")
}

async fn decode_token_response(
    resp: reqwest::Response,
) -> Result<TokenResponse, TokenRequestError> {
    let status = resp.status();
    if !status.is_success() {
        let oauth_code = recognized_oauth_error(
            resp.json::<OAuthErrorResponse>()
                .await
                .ok()
                .and_then(|body| body.error),
        );
        return Err(TokenRequestError {
            status: Some(status.as_u16()),
            oauth_code,
            network_unreachable: false,
        });
    }
    resp.json::<TokenResponse>()
        .await
        .map_err(|_| TokenRequestError {
            status: Some(status.as_u16()),
            oauth_code: None,
            network_unreachable: false,
        })
}

async fn exchange_code_at(
    endpoint: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenResponse, TokenRequestError> {
    let resp = token_http_client()
        .post(endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|error| TokenRequestError {
            status: None,
            oauth_code: None,
            network_unreachable: error.is_connect() || error.is_timeout(),
        })?;
    decode_token_response(resp).await
}

async fn refresh_at(
    endpoint: &str,
    refresh_token: &str,
) -> Result<TokenResponse, TokenRequestError> {
    let resp = token_http_client()
        .post(endpoint)
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|error| TokenRequestError {
            status: None,
            oauth_code: None,
            network_unreachable: error.is_connect() || error.is_timeout(),
        })?;
    decode_token_response(resp).await
}

fn account_id_from_jwt(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: HashMap<String, serde_json::Value> = serde_json::from_slice(&bytes).ok()?;
    let nested = claims
        .get(AUTH_CLAIM_NAMESPACE)
        .and_then(serde_json::Value::as_object)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| valid_account_id(value))
        .map(str::to_owned);
    if nested.is_some() {
        return nested;
    }
    [ACCOUNT_ID_CLAIM, "chatgpt_account_id", "account_id"]
        .into_iter()
        .find_map(|key| {
            claims
                .get(key)?
                .as_str()
                .filter(|value| valid_account_id(value))
        })
        .map(str::to_owned)
}

fn valid_account_id(value: &str) -> bool {
    !value.is_empty() && !value.contains(char::is_control)
}

fn build_auth(
    tokens: TokenResponse,
    previous: Option<&GrokAuth>,
) -> Result<GrokAuth, CodexOAuthError> {
    if tokens.access_token.is_empty() {
        return Err(CodexOAuthError::InvalidTokenResponse);
    }
    let account_id = tokens
        .id_token
        .as_deref()
        .and_then(account_id_from_jwt)
        .or_else(|| account_id_from_jwt(&tokens.access_token))
        .or_else(|| previous.and_then(|auth| auth.account_id.clone()));
    let refresh_token = tokens
        .refresh_token
        .or_else(|| previous.and_then(|auth| auth.refresh_token.clone()));
    let id_token = tokens
        .id_token
        .or_else(|| previous.and_then(|auth| auth.id_token.clone()));
    Ok(GrokAuth {
        key: tokens.access_token,
        auth_mode: AuthMode::OpenAiCodex,
        create_time: Utc::now(),
        user_id: String::new(),
        refresh_token,
        expires_at: tokens
            .expires_in
            .map(|seconds| Utc::now() + Duration::seconds(seconds as i64)),
        oidc_issuer: Some(ISSUER.to_owned()),
        oidc_client_id: Some(CLIENT_ID.to_owned()),
        id_token,
        account_id,
        ..GrokAuth::default()
    })
}

pub(crate) fn is_codex_credential(auth: &GrokAuth) -> bool {
    auth.auth_mode == AuthMode::OpenAiCodex
        && auth.oidc_issuer.as_deref() == Some(ISSUER)
        && auth.oidc_client_id.as_deref() == Some(CLIENT_ID)
        && !auth.key.is_empty()
}

/// Run browser/headless Codex OAuth and persist the resulting provider scope.
/// The callback receives the authorization URL exactly once so the CLI can
/// print it; browser opening is best-effort and never required for success.
pub async fn login_with_manager<F>(
    manager: &Arc<AuthManager>,
    cancellation: CancellationToken,
    open_browser: bool,
    announce_url: F,
) -> Result<GrokAuth, CodexOAuthError>
where
    F: FnOnce(&str),
{
    let (listener, port) = bind_callback().await?;
    let redirect_uri = redirect_uri(port);
    let pkce = generate_pkce();
    let state = generate_state();
    let authorize_url = build_authorize_url(&redirect_uri, &pkce, &state);
    announce_url(&authorize_url);
    if open_browser {
        let target = authorize_url.clone();
        tokio::task::spawn_blocking(move || webbrowser::open(&target));
    }
    let mut callback_session = wait_for_callback(listener, cancellation.clone(), &state).await?;
    let callback = callback_session.take_callback();
    let login_result: Result<GrokAuth, CodexOAuthError> = tokio::select! {
        result = async {
            let code = validated_callback_code(callback, &state)?;
            let tokens = exchange_code_at(TOKEN_ENDPOINT, &code, &redirect_uri, &pkce.verifier)
                .await
                .map_err(|error| match error.status {
                    Some(status) => CodexOAuthError::TokenHttp { status },
                    None => CodexOAuthError::Network,
                })?;
            let auth = build_auth(tokens, None)?;
            let lock = manager
                .try_lock_auth_file_async(StdDuration::from_secs(10))
                .await
                .ok_or(CodexOAuthError::StoreBusy)?;
            if !lock.still_live(manager.auth_json_path()) {
                return Err(CodexOAuthError::StoreBusy);
            }
            manager
                .save_without_enrichment(auth)
                .await
                .map_err(Into::into)
        } => result,
        _ = cancellation.cancelled() => Err(CodexOAuthError::Cancelled),
    };
    callback_session
        .complete(if login_result.is_ok() {
            CallbackPage::Success
        } else {
            CallbackPage::Failure
        })
        .await;
    login_result
}

/// High-level login entry point for CLI callers that do not already hold the
/// provider manager. The returned credential is also persisted under
/// `openai::codex` before this future completes.
pub async fn login<F>(
    grok_home: &Path,
    cancellation: CancellationToken,
    open_browser: bool,
    announce_url: F,
) -> Result<GrokAuth, CodexOAuthError>
where
    F: FnOnce(&str),
{
    let manager = manager(grok_home);
    login_with_manager(&manager, cancellation, open_browser, announce_url).await
}

/// Return non-secret state for provider readiness/status output.
pub fn status(manager: &AuthManager) -> CodexAuthStatus {
    let auth = manager.current_or_expired();
    CodexAuthStatus {
        signed_in: auth.is_some(),
        expired: manager.is_expired(),
        refreshable: auth
            .as_ref()
            .is_some_and(|auth| auth.refresh_token.is_some()),
        account_id_present: auth.as_ref().is_some_and(|auth| auth.account_id.is_some()),
        expires_at: auth.and_then(|auth| auth.expires_at),
    }
}

/// Atomically snapshot the access token and account id from one credential.
pub fn credential_snapshot(manager: &AuthManager) -> Option<ProviderCredentialSnapshot> {
    let auth = manager.current()?;
    Some(ProviderCredentialSnapshot {
        access_token: auth.key,
        expires_at: auth.expires_at,
        account_id: auth.account_id,
        issuer: auth.oidc_issuer,
    })
}

/// Load a request-time snapshot directly from the provider-scoped store.
pub fn load_snapshot(grok_home: &Path) -> Option<ProviderCredentialSnapshot> {
    credential_snapshot(&AuthManager::new_openai_codex(grok_home))
}

/// Remove only `openai::codex`; the xAI session and official Codex CLI store
/// are not read or modified. Memory is cleared only after durable removal.
pub async fn logout(manager: &AuthManager) -> Result<(), CodexOAuthError> {
    logout_with_timeout(manager, StdDuration::from_secs(10)).await
}

async fn logout_with_timeout(
    manager: &AuthManager,
    timeout: StdDuration,
) -> Result<(), CodexOAuthError> {
    manager
        .clear_durable(timeout)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::WouldBlock => CodexOAuthError::StoreBusy,
            _ => CodexOAuthError::Persist(error),
        })
}

pub async fn logout_at(grok_home: &Path) -> Result<(), CodexOAuthError> {
    logout(&AuthManager::new_openai_codex(grok_home)).await
}

/// Ensure the current provider credential is usable, refreshing through the
/// manager's existing single-flight state machine when near expiry.
pub async fn ensure_fresh(manager: &Arc<AuthManager>) -> Result<GrokAuth, super::AuthError> {
    manager.auth().await
}

pub(crate) async fn refresh_auth(auth: &GrokAuth) -> OidcRefreshResult {
    if !is_codex_credential(auth) {
        return OidcRefreshResult::Failed {
            network_unreachable: false,
        };
    }
    let Some(refresh_token) = auth.refresh_token.as_deref() else {
        return OidcRefreshResult::Failed {
            network_unreachable: false,
        };
    };
    match refresh_at(TOKEN_ENDPOINT, refresh_token).await {
        Ok(tokens) => match build_auth(tokens, Some(auth)) {
            Ok(new_auth) => OidcRefreshResult::Success(Box::new(new_auth)),
            Err(_) => OidcRefreshResult::Failed {
                network_unreachable: false,
            },
        },
        Err(error) => {
            let terminal = classify_refresh_error(&error);
            match terminal {
                Some(reason) => OidcRefreshResult::TerminalError { reason },
                None => OidcRefreshResult::Failed {
                    network_unreachable: error.network_unreachable,
                },
            }
        }
    }
}

fn classify_refresh_error(error: &TokenRequestError) -> Option<RefreshTokenFailedReason> {
    match error.oauth_code.as_deref() {
        Some("invalid_grant") => Some(RefreshTokenFailedReason::RefreshTokenRejected),
        Some("invalid_client") => Some(RefreshTokenFailedReason::ClientRejected),
        _ => None,
    }
}

/// Convenience constructor that also installs the existing single-flight OIDC
/// refresher used by proactive and bounded 401 recovery paths.
pub fn manager(grok_home: &Path) -> Arc<AuthManager> {
    let manager = Arc::new(AuthManager::new_openai_codex(grok_home));
    manager.configure_refresher(None, None);
    manager
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.")
    }

    #[test]
    fn authorize_url_has_official_contract_and_pkce() {
        let pkce = generate_pkce();
        assert!(pkce.verifier.len() >= 43);
        let url = url::Url::parse(&build_authorize_url(
            "http://localhost:1455/auth/callback",
            &pkce,
            "state",
        ))
        .unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.as_str().split('?').next(), Some(AUTHORIZE_ENDPOINT));
        assert_eq!(query.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(query.get("state").map(String::as_str), Some("state"));
        assert_eq!(
            query.get("originator").map(String::as_str),
            Some(ORIGINATOR)
        );
        assert_eq!(
            query.get("id_token_add_organizations").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn state_validation_is_strict() {
        assert!(validate_state("expected", Some("expected")).is_ok());
        assert!(validate_state("expected", Some("wrong")).is_err());
        assert!(validate_state("expected", None).is_err());
    }

    #[test]
    fn callback_errors_and_missing_code_fail_closed() {
        let denied = Callback {
            code: None,
            state: Some("state".into()),
            error: Some("access_denied".into()),
        };
        assert!(matches!(
            validated_callback_code(denied, "state"),
            Err(CodexOAuthError::Callback("access_denied"))
        ));
        let missing = Callback {
            code: None,
            state: Some("state".into()),
            error: None,
        };
        assert!(matches!(
            validated_callback_code(missing, "state"),
            Err(CodexOAuthError::Callback("missing_code"))
        ));
    }

    #[tokio::test]
    async fn callback_cancellation_closes_listener() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            wait_for_callback_with_timeout(
                listener,
                cancellation,
                "state",
                StdDuration::from_secs(1),
            )
            .await,
            Err(CodexOAuthError::Cancelled)
        ));
        TcpListener::bind(addr)
            .await
            .expect("listener released after cancellation");
    }

    #[tokio::test]
    async fn callback_timeout_closes_listener() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(matches!(
            wait_for_callback_with_timeout(
                listener,
                CancellationToken::new(),
                "state",
                StdDuration::from_millis(10),
            )
            .await,
            Err(CodexOAuthError::CallbackTimeout)
        ));
        TcpListener::bind(addr)
            .await
            .expect("listener released after timeout");
    }

    #[tokio::test]
    async fn wrong_state_callback_does_not_consume_listener() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let callback_task = tokio::spawn(wait_for_callback_with_timeout(
            listener,
            CancellationToken::new(),
            "expected-state",
            StdDuration::from_secs(1),
        ));
        let client = reqwest::Client::new();

        let wrong = client
            .get(format!(
                "http://{addr}{CALLBACK_PATH}?code=forged&state=wrong-state"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
        assert!(
            !callback_task.is_finished(),
            "wrong-state callback must not complete the login listener"
        );

        let correct = tokio::spawn(async move {
            client
                .get(format!(
                    "http://{addr}{CALLBACK_PATH}?code=real-code&state=expected-state"
                ))
                .send()
                .await
                .unwrap()
        });
        let mut callback_session = callback_task.await.unwrap().unwrap();
        assert!(
            !correct.is_finished(),
            "browser success must wait for exchange and durable persistence"
        );
        let callback = callback_session.take_callback();
        assert_eq!(
            validated_callback_code(callback, "expected-state").unwrap(),
            "real-code"
        );
        callback_session.complete(CallbackPage::Success).await;
        let correct = correct.await.unwrap();
        assert_eq!(correct.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn callback_reports_failure_when_completion_does_not_persist() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let callback_task = tokio::spawn(wait_for_callback_with_timeout(
            listener,
            CancellationToken::new(),
            "expected-state",
            StdDuration::from_secs(5),
        ));
        tokio::task::yield_now().await;
        let request = tokio::spawn(async move {
            reqwest::Client::new()
                .get(format!(
                    "http://{addr}{CALLBACK_PATH}?code=real-code&state=expected-state"
                ))
                .send()
                .await
                .unwrap()
        });
        let callback_session = callback_task.await.unwrap().unwrap();
        assert!(!request.is_finished());
        callback_session.complete(CallbackPage::Failure).await;
        let response = request.await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.text().await.unwrap(),
            "Sign-in failed. Return to Grok Build."
        );
    }

    #[tokio::test]
    async fn logout_does_not_clear_memory_or_report_success_while_store_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        manager
            .save_without_enrichment(GrokAuth {
                key: "access-token".into(),
                auth_mode: AuthMode::OpenAiCodex,
                oidc_issuer: Some(ISSUER.into()),
                oidc_client_id: Some(CLIENT_ID.into()),
                ..GrokAuth::default()
            })
            .await
            .unwrap();
        let held_lock = manager
            .try_lock_auth_file_async(StdDuration::from_secs(1))
            .await
            .expect("test lock acquired");

        assert!(matches!(
            logout_with_timeout(&manager, StdDuration::from_millis(20)).await,
            Err(CodexOAuthError::StoreBusy)
        ));
        assert!(status(&manager).signed_in, "memory must remain coherent");
        assert!(
            status(&AuthManager::new_openai_codex(dir.path())).signed_in,
            "disk credential must remain available to another process"
        );

        drop(held_lock);
        logout_with_timeout(&manager, StdDuration::from_secs(1))
            .await
            .unwrap();
        assert!(!status(&manager).signed_in);
        assert!(!status(&AuthManager::new_openai_codex(dir.path())).signed_in);
    }

    #[test]
    fn invalid_grant_is_terminal_refresh_rejection() {
        let error = TokenRequestError {
            status: Some(400),
            oauth_code: Some("invalid_grant".into()),
            network_unreachable: false,
        };
        assert_eq!(
            classify_refresh_error(&error),
            Some(RefreshTokenFailedReason::RefreshTokenRejected)
        );
    }

    #[test]
    fn account_claim_is_compatibility_metadata_only() {
        let token = jwt(serde_json::json!({
            AUTH_CLAIM_NAMESPACE: { "chatgpt_account_id": "acct-123" }
        }));
        assert_eq!(account_id_from_jwt(&token).as_deref(), Some("acct-123"));
        let fallback = jwt(serde_json::json!({ ACCOUNT_ID_CLAIM: "acct-flat" }));
        assert_eq!(account_id_from_jwt(&fallback).as_deref(), Some("acct-flat"));
        assert_eq!(account_id_from_jwt("not-a-jwt"), None);
    }

    #[test]
    fn debug_redacts_all_codex_secrets_and_account() {
        let auth = GrokAuth {
            key: "access-SENTINEL".into(),
            refresh_token: Some("refresh-SENTINEL".into()),
            id_token: Some("id-SENTINEL".into()),
            account_id: Some("account-SENTINEL".into()),
            auth_mode: AuthMode::OpenAiCodex,
            ..GrokAuth::default()
        };
        let debug = format!("{auth:?}");
        for secret in [
            "access-SENTINEL",
            "refresh-SENTINEL",
            "id-SENTINEL",
            "account-SENTINEL",
        ] {
            assert!(!debug.contains(secret), "debug leaked {secret}: {debug}");
        }
    }

    #[test]
    fn codex_manager_does_not_adopt_or_modify_other_scope() {
        let dir = tempfile::tempdir().unwrap();
        let xai = GrokAuth {
            key: "xai-secret".into(),
            auth_mode: AuthMode::Oidc,
            ..GrokAuth::default()
        };
        let store = std::collections::BTreeMap::from([("xai::scope".to_owned(), xai)]);
        super::super::storage::write_auth_json(&dir.path().join("auth.json"), &store).unwrap();
        let manager = AuthManager::new_openai_codex(dir.path());
        assert!(manager.current_or_expired().is_none());
        let stored = super::super::storage::read_auth_json(&dir.path().join("auth.json")).unwrap();
        assert_eq!(stored.get("xai::scope").unwrap().key, "xai-secret");
    }

    #[tokio::test]
    async fn code_exchange_is_form_encoded() {
        use axum::extract::Form;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/token",
            axum::routing::post(|Form(form): Form<HashMap<String, String>>| async move {
                assert_eq!(
                    form.get("grant_type").map(String::as_str),
                    Some("authorization_code")
                );
                assert_eq!(
                    form.get("code_verifier").map(String::as_str),
                    Some("verifier")
                );
                axum::Json(serde_json::json!({"access_token":"access","expires_in":3600}))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let tokens = exchange_code_at(
            &format!("http://{addr}/token"),
            "code",
            "http://localhost:1455/auth/callback",
            "verifier",
        )
        .await
        .unwrap();
        assert_eq!(tokens.access_token, "access");
    }

    #[tokio::test]
    async fn refresh_is_json_encoded_and_rotation_is_retained() {
        use axum::extract::Json;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/token",
            axum::routing::post(|Json(body): Json<serde_json::Value>| async move {
                assert_eq!(body["grant_type"], "refresh_token");
                assert_eq!(body["refresh_token"], "old-refresh");
                axum::Json(serde_json::json!({
                    "access_token":"new-access",
                    "refresh_token":"new-refresh",
                    "expires_in":3600
                }))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let tokens = refresh_at(&format!("http://{addr}/token"), "old-refresh")
            .await
            .unwrap();
        let auth = build_auth(tokens, None).unwrap();
        assert_eq!(auth.key, "new-access");
        assert_eq!(auth.refresh_token.as_deref(), Some("new-refresh"));
    }

    #[tokio::test]
    async fn strict_provider_save_does_not_publish_when_atomic_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new_openai_codex(dir.path()));
        let initial = GrokAuth {
            key: "initial-access".into(),
            refresh_token: Some("initial-refresh".into()),
            auth_mode: AuthMode::OpenAiCodex,
            oidc_issuer: Some(ISSUER.into()),
            oidc_client_id: Some(CLIENT_ID.into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            ..GrokAuth::default()
        };
        manager.update(initial).await.unwrap();

        let path = dir.path().join("auth.json");
        *super::super::storage::WRITE_STORAGE_FULL_FAULT_PATH
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(path.clone());
        let rotated = GrokAuth {
            key: "rotated-access".into(),
            refresh_token: Some("rotated-refresh".into()),
            auth_mode: AuthMode::OpenAiCodex,
            oidc_issuer: Some(ISSUER.into()),
            oidc_client_id: Some(CLIENT_ID.into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            ..GrokAuth::default()
        };
        let result = manager.update(rotated).await;
        *super::super::storage::WRITE_STORAGE_FULL_FAULT_PATH
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;

        assert!(result.is_err());
        assert_eq!(manager.current_or_expired().unwrap().key, "initial-access");
        let disk = super::super::storage::read_auth_json(&path).unwrap();
        assert_eq!(disk.get(AUTH_SCOPE).unwrap().key, "initial-access");
    }
}
