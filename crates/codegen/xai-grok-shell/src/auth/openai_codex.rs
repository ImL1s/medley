//! Provider-scoped OpenAI Codex OAuth compatibility surface.
//!
//! The constants in this module mirror the public-client contract used by the
//! official Codex CLI. They are intentionally centralized because the service
//! is not the OpenAI Platform API and can evolve independently.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use axum::{Router, extract::Query, http::StatusCode, response::Html, routing::get};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};
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
pub const REVOKE_ENDPOINT: &str = "https://auth.openai.com/oauth/revoke";
pub const CODEX_API_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
pub const ORIGINATOR: &str = "grok_build";
pub const CALLBACK_PORTS: &[u16] = &[1455, 1457];
pub const AUTH_CLAIM_NAMESPACE: &str = "https://api.openai.com/auth";

const CALLBACK_PATH: &str = "/auth/callback";
const CALLBACK_TIMEOUT: StdDuration = StdDuration::from_secs(600);
const REQUEST_TIMEOUT: StdDuration = StdDuration::from_secs(20);
const REVOKE_TIMEOUT: StdDuration = StdDuration::from_secs(10);
const EXPIRY_FALLBACK_SECONDS: i64 = 8 * 24 * 60 * 60;

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
    pub permanent_failure: bool,
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
struct AuthCodeTokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl std::fmt::Debug for AuthCodeTokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthCodeTokenResponse")
            .field("access_token_present", &!self.access_token.is_empty())
            .field("refresh_token_present", &!self.refresh_token.is_empty())
            .field("id_token_present", &!self.id_token.is_empty())
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Deserialize)]
struct RefreshTokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CodexWorkspaceClaims {
    account_id: Option<String>,
    is_fedramp: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RevokeTokenKind {
    Access,
    Refresh,
}

impl RevokeTokenKind {
    fn hint(self) -> &'static str {
        match self {
            Self::Access => "access_token",
            Self::Refresh => "refresh_token",
        }
    }
}

#[derive(Serialize)]
struct RevokeTokenRequest<'a> {
    token: &'a str,
    token_type_hint: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<&'static str>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum RevokeError {
    #[error("Codex OAuth revocation timed out")]
    Timeout,
    #[error("Codex OAuth revocation request failed")]
    Network,
    #[error("Codex OAuth revocation failed: HTTP {status}")]
    Http { status: u16 },
}

impl std::fmt::Debug for RefreshTokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshTokenResponse")
            .field("access_token_present", &self.access_token.is_some())
            .field("refresh_token_present", &self.refresh_token.is_some())
            .field("id_token_present", &self.id_token.is_some())
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OAuthErrorField {
    Code(String),
    Object {
        #[serde(default)]
        code: Option<String>,
    },
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    #[serde(default)]
    error: Option<OAuthErrorField>,
    #[serde(default)]
    code: Option<String>,
}

fn recognized_oauth_error(body: OAuthErrorResponse) -> Option<String> {
    let code = match body.error {
        Some(OAuthErrorField::Code(code)) => Some(code),
        Some(OAuthErrorField::Object { code }) => code,
        None => None,
    }
    .or(body.code);
    match code.as_deref() {
        Some(
            "invalid_grant"
            | "invalid_client"
            | "refresh_token_expired"
            | "refresh_token_reused"
            | "refresh_token_invalidated",
        ) => code,
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

async fn decode_token_response<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, TokenRequestError> {
    let status = resp.status();
    if !status.is_success() {
        let oauth_code = resp
            .json::<OAuthErrorResponse>()
            .await
            .ok()
            .and_then(recognized_oauth_error);
        return Err(TokenRequestError {
            status: Some(status.as_u16()),
            oauth_code,
            network_unreachable: false,
        });
    }
    resp.json::<T>().await.map_err(|_| TokenRequestError {
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
) -> Result<AuthCodeTokenResponse, TokenRequestError> {
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
) -> Result<RefreshTokenResponse, TokenRequestError> {
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

fn revocable_token(auth: &GrokAuth) -> Option<(&str, RevokeTokenKind)> {
    if !is_codex_credential(auth) {
        return None;
    }
    auth.refresh_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .map(|token| (token, RevokeTokenKind::Refresh))
        .or_else(|| (!auth.key.is_empty()).then_some((auth.key.as_str(), RevokeTokenKind::Access)))
}

async fn revoke_at(
    endpoint: &str,
    auth: &GrokAuth,
    timeout: StdDuration,
) -> Result<(), RevokeError> {
    let Some((token, kind)) = revocable_token(auth) else {
        return Ok(());
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|_| RevokeError::Network)?;
    let request = RevokeTokenRequest {
        token,
        token_type_hint: kind.hint(),
        client_id: (kind == RevokeTokenKind::Refresh).then_some(CLIENT_ID),
    };
    let response = client
        .post(endpoint)
        .json(&request)
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                RevokeError::Timeout
            } else {
                RevokeError::Network
            }
        })?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(RevokeError::Http {
            status: response.status().as_u16(),
        })
    }
}

fn workspace_claims_from_jwt(token: &str) -> Option<CodexWorkspaceClaims> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: HashMap<String, serde_json::Value> = serde_json::from_slice(&bytes).ok()?;
    let auth = claims
        .get(AUTH_CLAIM_NAMESPACE)
        .and_then(serde_json::Value::as_object);
    let account_id = auth
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| valid_account_id(value))
        .map(str::to_owned);
    let is_fedramp = auth
        .and_then(|auth| auth.get("chatgpt_account_is_fedramp"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Some(CodexWorkspaceClaims {
        account_id,
        is_fedramp,
    })
}

fn account_id_from_jwt(token: &str) -> Option<String> {
    workspace_claims_from_jwt(token)?.account_id
}

fn expiry_from_access_token(token: &str) -> Option<chrono::DateTime<Utc>> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: HashMap<String, serde_json::Value> = serde_json::from_slice(&bytes).ok()?;
    let seconds = claims.get("exp").and_then(|exp| {
        exp.as_i64()
            .or_else(|| exp.as_u64().and_then(|value| i64::try_from(value).ok()))
    })?;
    Utc.timestamp_opt(seconds, 0).single()
}

fn token_expiry(
    access_token: &str,
    expires_in: Option<u64>,
    now: chrono::DateTime<Utc>,
) -> Result<chrono::DateTime<Utc>, CodexOAuthError> {
    let expires_in = expires_in
        .map(|seconds| {
            let seconds =
                i64::try_from(seconds).map_err(|_| CodexOAuthError::InvalidTokenResponse)?;
            let duration =
                Duration::try_seconds(seconds).ok_or(CodexOAuthError::InvalidTokenResponse)?;
            now.checked_add_signed(duration)
                .ok_or(CodexOAuthError::InvalidTokenResponse)
        })
        .transpose()?;
    let jwt_expiry = expiry_from_access_token(access_token);
    match (expires_in, jwt_expiry) {
        (Some(relative), Some(jwt)) => Ok(relative.min(jwt)),
        (Some(relative), None) => Ok(relative),
        (None, Some(jwt)) => Ok(jwt),
        (None, None) => Duration::try_seconds(EXPIRY_FALLBACK_SECONDS)
            .and_then(|duration| now.checked_add_signed(duration))
            .ok_or(CodexOAuthError::InvalidTokenResponse),
    }
}

fn valid_account_id(value: &str) -> bool {
    !value.is_empty() && !value.contains(char::is_control)
}

fn build_login_auth(tokens: AuthCodeTokenResponse) -> Result<GrokAuth, CodexOAuthError> {
    if tokens.access_token.is_empty()
        || tokens.refresh_token.is_empty()
        || tokens.id_token.is_empty()
    {
        return Err(CodexOAuthError::InvalidTokenResponse);
    }
    let now = Utc::now();
    let expires_at = token_expiry(&tokens.access_token, tokens.expires_in, now)?;
    let workspace = workspace_claims_from_jwt(&tokens.id_token).unwrap_or_default();
    Ok(GrokAuth {
        key: tokens.access_token,
        auth_mode: AuthMode::OpenAiCodex,
        create_time: now,
        user_id: String::new(),
        refresh_token: Some(tokens.refresh_token),
        expires_at: Some(expires_at),
        oidc_issuer: Some(ISSUER.to_owned()),
        oidc_client_id: Some(CLIENT_ID.to_owned()),
        id_token: Some(tokens.id_token),
        account_id: workspace.account_id,
        chatgpt_account_is_fedramp: workspace.is_fedramp,
        ..GrokAuth::default()
    })
}

fn merge_refresh_auth(
    tokens: RefreshTokenResponse,
    previous: &GrokAuth,
) -> Result<GrokAuth, CodexOAuthError> {
    for token in [
        tokens.access_token.as_deref(),
        tokens.refresh_token.as_deref(),
        tokens.id_token.as_deref(),
    ] {
        if token.is_some_and(str::is_empty) {
            return Err(CodexOAuthError::InvalidTokenResponse);
        }
    }
    if tokens.access_token.is_none() && tokens.refresh_token.is_none() && tokens.id_token.is_none()
    {
        return Err(CodexOAuthError::InvalidTokenResponse);
    }

    let now = Utc::now();
    let new_workspace = tokens
        .id_token
        .as_deref()
        .map(|token| workspace_claims_from_jwt(token).unwrap_or_default());
    let mut merged = previous.clone();
    if let Some(access_token) = tokens.access_token {
        merged.expires_at = Some(token_expiry(&access_token, tokens.expires_in, now)?);
        merged.key = access_token;
        merged.create_time = now;
    }
    if let Some(refresh_token) = tokens.refresh_token {
        merged.refresh_token = Some(refresh_token);
    }
    if let Some(id_token) = tokens.id_token {
        merged.id_token = Some(id_token);
    }
    if let Some(workspace) = new_workspace {
        merged.account_id = workspace.account_id;
        merged.chatgpt_account_is_fedramp = workspace.is_fedramp;
    }
    Ok(merged)
}

pub(crate) fn is_codex_credential(auth: &GrokAuth) -> bool {
    auth.auth_mode == AuthMode::OpenAiCodex
        && auth.oidc_issuer.as_deref() == Some(ISSUER)
        && auth.oidc_client_id.as_deref() == Some(CLIENT_ID)
        && !auth.key.is_empty()
}

/// Re-derive routing metadata from the namespaced ID-token contract instead of
/// trusting independently persisted `account_id` / FedRAMP fields. This also
/// clears metadata written by older builds from generic top-level claims.
pub(crate) fn normalize_workspace_metadata(auth: &mut GrokAuth) {
    let workspace = auth
        .id_token
        .as_deref()
        .and_then(workspace_claims_from_jwt)
        .unwrap_or_default();
    auth.account_id = workspace.account_id;
    auth.chatgpt_account_is_fedramp = workspace.is_fedramp;
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
            let auth = build_login_auth(tokens)?;
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
        permanent_failure: manager.has_permanent_failure(),
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
        chatgpt_account_is_fedramp: auth.chatgpt_account_is_fedramp,
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
    logout_with_revoke_endpoint(manager, timeout, REVOKE_ENDPOINT).await
}

async fn logout_with_revoke_endpoint(
    manager: &AuthManager,
    timeout: StdDuration,
    revoke_endpoint: &str,
) -> Result<(), CodexOAuthError> {
    manager
        .clear_durable_with_current_scope(timeout, |auth| async move {
            if let Some(auth) = auth
                && let Err(error) =
                    revoke_at(revoke_endpoint, &auth, timeout.min(REVOKE_TIMEOUT)).await
            {
                tracing::warn!(error = %error, "Codex OAuth remote revocation failed; continuing local logout");
            }
        })
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
        Ok(tokens) => match merge_refresh_auth(tokens, auth) {
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
        Some(
            "invalid_grant"
            | "refresh_token_expired"
            | "refresh_token_reused"
            | "refresh_token_invalidated",
        ) => Some(RefreshTokenFailedReason::RefreshTokenRejected),
        Some("invalid_client") => Some(RefreshTokenFailedReason::ClientRejected),
        _ if error.status == Some(StatusCode::UNAUTHORIZED.as_u16()) => {
            Some(RefreshTokenFailedReason::Other)
        }
        _ => None,
    }
}

/// Resolve the Codex auth.json path the same way production construction does.
///
/// Tests that cannot pass a path into a production entry point should pin
/// [`CodexAuthPathGuard`] (thread-local) instead of `GROK_AUTH_PATH` (#343).
pub(crate) fn resolved_auth_path(grok_home: &Path) -> PathBuf {
    #[cfg(test)]
    if let Some(path) = AUTH_PATH_OVERRIDE.with(|slot| slot.borrow().clone()) {
        return path;
    }
    // In-process unit tests must not share process-global `GROK_AUTH_PATH`.
    // Production still honors the env so a user-specified auth.json works.
    if cfg!(test) {
        grok_home.join("auth.json")
    } else {
        std::env::var("GROK_AUTH_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| grok_home.join("auth.json"))
    }
}

#[cfg(test)]
thread_local! {
    static AUTH_PATH_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Thread-local pin of the Codex auth.json path for production constructors
/// that cannot take a path argument. Does not mutate process environment.
#[cfg(test)]
pub struct CodexAuthPathGuard {
    previous: Option<PathBuf>,
}

#[cfg(test)]
impl CodexAuthPathGuard {
    pub fn pin(path: impl Into<PathBuf>) -> Self {
        let previous = AUTH_PATH_OVERRIDE.with(|slot| slot.replace(Some(path.into())));
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for CodexAuthPathGuard {
    fn drop(&mut self) {
        AUTH_PATH_OVERRIDE.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

/// Convenience constructor that also installs the existing single-flight OIDC
/// refresher used by proactive and bounded 401 recovery paths.
pub fn manager(grok_home: &Path) -> Arc<AuthManager> {
    let manager = Arc::new(AuthManager::new_openai_codex(grok_home));
    manager.configure_refresher(None, None);
    manager
}

/// Like [`manager`], but bound to an exact auth.json path and independent of
/// `GROK_AUTH_PATH` (#343).
pub fn manager_at_path(path: PathBuf) -> Arc<AuthManager> {
    let manager = Arc::new(AuthManager::new_openai_codex_at_path(path));
    manager.configure_refresher(None, None);
    manager
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    #[serial_test::serial]
    fn openai_codex_path_constructor_ignores_grok_auth_path_env() {
        fn write_codex_auth(path: &Path, key: &str) {
            let auth = GrokAuth {
                key: key.into(),
                auth_mode: AuthMode::OpenAiCodex,
                refresh_token: Some(format!("{key}-refresh")),
                oidc_issuer: Some(ISSUER.into()),
                oidc_client_id: Some(CLIENT_ID.into()),
                expires_at: Some(Utc::now() + Duration::hours(1)),
                ..GrokAuth::default()
            };
            std::fs::write(
                path,
                serde_json::to_vec(&HashMap::from([(AUTH_SCOPE.to_owned(), auth)])).unwrap(),
            )
            .unwrap();
        }

        let poison_home = tempfile::tempdir().unwrap();
        write_codex_auth(&poison_home.path().join("auth.json"), "poison-token");
        let real_home = tempfile::tempdir().unwrap();
        let real_path = real_home.path().join("auth.json");
        write_codex_auth(&real_path, "real-token");

        // Process-global env must not be required (or used) by these constructors.
        let _clear = xai_grok_test_support::EnvGuard::unset("GROK_AUTH_PATH");

        let via_path = AuthManager::new_openai_codex_at_path(real_path.clone());
        assert_eq!(
            via_path
                .current()
                .expect("path constructor loaded fixture")
                .key,
            "real-token"
        );

        let _pin = CodexAuthPathGuard::pin(real_path);
        let via_pin = AuthManager::new_openai_codex(poison_home.path());
        assert_eq!(
            via_pin
                .current()
                .expect("thread-local pin loaded fixture")
                .key,
            "real-token"
        );
    }

    #[derive(Debug)]
    struct RotatingCodexRefresher {
        calls: Arc<AtomicUsize>,
    }

    #[derive(Debug)]
    struct RejectingCodexRefresher {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::auth::refresh::TokenRefresher for RotatingCodexRefresher {
        async fn refresh(
            &self,
            _reason: crate::auth::refresh::RefreshReason,
        ) -> crate::auth::refresh::RefreshOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(StdDuration::from_millis(25)).await;
            crate::auth::refresh::RefreshOutcome::Success(Box::new(GrokAuth {
                key: "rotated-access".into(),
                auth_mode: AuthMode::OpenAiCodex,
                refresh_token: Some("rotated-refresh".into()),
                expires_at: Some(Utc::now() + Duration::hours(1)),
                oidc_issuer: Some(ISSUER.into()),
                oidc_client_id: Some(CLIENT_ID.into()),
                id_token: Some(jwt(serde_json::json!({
                    AUTH_CLAIM_NAMESPACE: {
                        "chatgpt_account_id": "rotated-account",
                        "chatgpt_account_is_fedramp": true
                    }
                }))),
                account_id: Some("rotated-account".into()),
                chatgpt_account_is_fedramp: true,
                ..GrokAuth::default()
            }))
        }
    }

    #[async_trait::async_trait]
    impl crate::auth::refresh::TokenRefresher for RejectingCodexRefresher {
        async fn refresh(
            &self,
            _reason: crate::auth::refresh::RefreshReason,
        ) -> crate::auth::refresh::RefreshOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let attempted = GrokAuth {
                key: "expiring-access".into(),
                auth_mode: AuthMode::OpenAiCodex,
                refresh_token: Some("rejected-refresh".into()),
                ..GrokAuth::default()
            };
            crate::auth::refresh::RefreshOutcome::permanent_for(
                RefreshTokenFailedReason::RefreshTokenRejected,
                &attempted,
            )
        }
    }

    fn jwt(claims: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
        format!("{header}.{payload}.")
    }

    fn previous_codex_auth() -> GrokAuth {
        GrokAuth {
            key: "old-access".into(),
            auth_mode: AuthMode::OpenAiCodex,
            create_time: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
            refresh_token: Some("old-refresh".into()),
            expires_at: Some(Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap()),
            oidc_issuer: Some(ISSUER.into()),
            oidc_client_id: Some(CLIENT_ID.into()),
            id_token: Some(jwt(serde_json::json!({
                AUTH_CLAIM_NAMESPACE: {
                    "chatgpt_account_id": "old-account",
                    "chatgpt_account_is_fedramp": true
                }
            }))),
            account_id: Some("old-account".into()),
            chatgpt_account_is_fedramp: true,
            ..GrokAuth::default()
        }
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
        let revoke_calls = Arc::new(AtomicUsize::new(0));
        let captured_calls = Arc::clone(&revoke_calls);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/revoke",
            axum::routing::post(move || {
                let captured_calls = Arc::clone(&captured_calls);
                async move {
                    captured_calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::NO_CONTENT
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let revoke_endpoint = format!("http://{addr}/revoke");
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
            logout_with_revoke_endpoint(&manager, StdDuration::from_millis(20), &revoke_endpoint,)
                .await,
            Err(CodexOAuthError::StoreBusy)
        ));
        assert_eq!(
            revoke_calls.load(Ordering::SeqCst),
            0,
            "logout must not revoke a cached credential before acquiring the durable-store lock"
        );
        assert!(status(&manager).signed_in, "memory must remain coherent");
        assert!(
            status(&AuthManager::new_openai_codex(dir.path())).signed_in,
            "disk credential must remain available to another process"
        );

        drop(held_lock);
        logout_with_revoke_endpoint(&manager, StdDuration::from_secs(1), &revoke_endpoint)
            .await
            .unwrap();
        assert!(!status(&manager).signed_in);
        assert!(!status(&AuthManager::new_openai_codex(dir.path())).signed_in);
        assert_eq!(revoke_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn logout_revokes_disk_current_generation_while_holding_store_lock() {
        use axum::extract::Json;
        use tokio::sync::{Barrier, Mutex, Notify};

        let dir = tempfile::tempdir().unwrap();
        let xai = GrokAuth {
            key: "xai-secret".into(),
            auth_mode: AuthMode::Oidc,
            ..GrokAuth::default()
        };
        super::super::storage::write_auth_json(
            &dir.path().join("auth.json"),
            &std::collections::BTreeMap::from([("xai::scope".to_owned(), xai)]),
        )
        .unwrap();

        let logout_manager = manager(dir.path());
        logout_manager
            .save_without_enrichment(previous_codex_auth())
            .await
            .unwrap();
        let rotating_manager = manager(dir.path());

        let rotation_barrier = Arc::new(Barrier::new(2));
        let rotating_task = {
            let rotating_manager = Arc::clone(&rotating_manager);
            let rotation_barrier = Arc::clone(&rotation_barrier);
            tokio::spawn(async move {
                rotation_barrier.wait().await;
                let rotation_lock = rotating_manager
                    .try_lock_auth_file_async(StdDuration::from_secs(1))
                    .await
                    .expect("rotating manager must acquire the shared auth-file lock");
                let mut rotated = previous_codex_auth();
                rotated.key = "rotated-access".into();
                rotated.refresh_token = Some("rotated-refresh".into());
                rotated.account_id = Some("rotated-account".into());
                rotating_manager
                    .save_without_enrichment(rotated)
                    .await
                    .unwrap();
                drop(rotation_lock);
            })
        };
        rotation_barrier.wait().await;
        tokio::time::timeout(StdDuration::from_secs(5), rotating_task)
            .await
            .expect("credential rotation must not deadlock")
            .unwrap();
        assert_eq!(
            logout_manager
                .current_or_expired()
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("old-refresh"),
            "logout manager must retain the deliberately stale cached generation"
        );

        let captured_bodies = Arc::new(Mutex::new(Vec::new()));
        let revoke_started = Arc::new(Notify::new());
        let release_revoke = Arc::new(Notify::new());
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/revoke",
            axum::routing::post({
                let captured_bodies = Arc::clone(&captured_bodies);
                let revoke_started = Arc::clone(&revoke_started);
                let release_revoke = Arc::clone(&release_revoke);
                move |Json(body): Json<serde_json::Value>| {
                    let captured_bodies = Arc::clone(&captured_bodies);
                    let revoke_started = Arc::clone(&revoke_started);
                    let release_revoke = Arc::clone(&release_revoke);
                    async move {
                        captured_bodies.lock().await.push(body);
                        revoke_started.notify_one();
                        release_revoke.notified().await;
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let revoke_endpoint = format!("http://{addr}/revoke");
        let logout_task = {
            let logout_manager = Arc::clone(&logout_manager);
            tokio::spawn(async move {
                logout_with_revoke_endpoint(
                    &logout_manager,
                    StdDuration::from_secs(1),
                    &revoke_endpoint,
                )
                .await
            })
        };
        tokio::time::timeout(StdDuration::from_secs(5), revoke_started.notified())
            .await
            .expect("logout must reach remote revocation without deadlocking");

        let bodies = captured_bodies.lock().await;
        assert_eq!(bodies.len(), 1);
        assert_eq!(
            bodies[0]["token"], "rotated-refresh",
            "logout must revoke the generation reread from disk under lock"
        );
        drop(bodies);
        assert!(
            rotating_manager
                .try_lock_auth_file_async(StdDuration::from_millis(20))
                .await
                .is_none(),
            "the auth-file lock must remain held while remote revocation is in flight"
        );

        release_revoke.notify_one();
        tokio::time::timeout(StdDuration::from_secs(5), logout_task)
            .await
            .expect("logout must finish after revocation is released")
            .unwrap()
            .unwrap();
        assert!(!status(&logout_manager).signed_in);
        let store = super::super::storage::read_auth_json(&dir.path().join("auth.json")).unwrap();
        assert!(!store.contains_key(AUTH_SCOPE));
        assert_eq!(store.get("xai::scope").unwrap().key, "xai-secret");
    }

    #[tokio::test]
    async fn revoke_prefers_refresh_then_falls_back_to_access() {
        use axum::extract::Json;
        let bodies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&bodies);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/revoke",
            axum::routing::post(move |Json(body): Json<serde_json::Value>| {
                let captured = Arc::clone(&captured);
                async move {
                    captured.lock().await.push(body);
                    StatusCode::NO_CONTENT
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let endpoint = format!("http://{addr}/revoke");

        let refresh = previous_codex_auth();
        revoke_at(&endpoint, &refresh, StdDuration::from_secs(1))
            .await
            .unwrap();
        let access = GrokAuth {
            key: "access-only-secret".into(),
            refresh_token: None,
            auth_mode: AuthMode::OpenAiCodex,
            oidc_issuer: Some(ISSUER.into()),
            oidc_client_id: Some(CLIENT_ID.into()),
            ..GrokAuth::default()
        };
        revoke_at(&endpoint, &access, StdDuration::from_secs(1))
            .await
            .unwrap();

        let bodies = bodies.lock().await;
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["token"], "old-refresh");
        assert_eq!(bodies[0]["token_type_hint"], "refresh_token");
        assert_eq!(bodies[0]["client_id"], CLIENT_ID);
        assert_eq!(bodies[1]["token"], "access-only-secret");
        assert_eq!(bodies[1]["token_type_hint"], "access_token");
        assert!(bodies[1].get("client_id").is_none());
    }

    #[tokio::test]
    async fn revoke_timeout_and_http_errors_are_secret_safe() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new()
            .route(
                "/slow",
                axum::routing::post(|| async {
                    tokio::time::sleep(StdDuration::from_secs(1)).await;
                    StatusCode::NO_CONTENT
                }),
            )
            .route(
                "/failure",
                axum::routing::post(|| async {
                    (StatusCode::INTERNAL_SERVER_ERROR, "server-body-SENTINEL")
                }),
            );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let auth = GrokAuth {
            key: "access-SENTINEL".into(),
            refresh_token: Some("refresh-SENTINEL".into()),
            auth_mode: AuthMode::OpenAiCodex,
            oidc_issuer: Some(ISSUER.into()),
            oidc_client_id: Some(CLIENT_ID.into()),
            ..GrokAuth::default()
        };

        let timeout = revoke_at(
            &format!("http://{addr}/slow"),
            &auth,
            StdDuration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert_eq!(timeout, RevokeError::Timeout);
        let http = revoke_at(
            &format!("http://{addr}/failure"),
            &auth,
            StdDuration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(http, RevokeError::Http { status: 500 });
        let rendered = format!("{timeout:?} {timeout} {http:?} {http}");
        for secret in [
            "access-SENTINEL",
            "refresh-SENTINEL",
            "server-body-SENTINEL",
        ] {
            assert!(!rendered.contains(secret), "revoke error leaked {secret}");
        }
    }

    #[tokio::test]
    async fn remote_revoke_failure_never_blocks_provider_scoped_local_logout() {
        let dir = tempfile::tempdir().unwrap();
        let xai = GrokAuth {
            key: "xai-secret".into(),
            auth_mode: AuthMode::Oidc,
            ..GrokAuth::default()
        };
        super::super::storage::write_auth_json(
            &dir.path().join("auth.json"),
            &std::collections::BTreeMap::from([("xai::scope".to_owned(), xai)]),
        )
        .unwrap();
        let manager = manager(dir.path());
        manager
            .save_without_enrichment(previous_codex_auth())
            .await
            .unwrap();

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/revoke",
            axum::routing::post(|| async {
                (StatusCode::BAD_GATEWAY, "refresh-SENTINEL must stay secret")
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        logout_with_revoke_endpoint(
            &manager,
            StdDuration::from_secs(1),
            &format!("http://{addr}/revoke"),
        )
        .await
        .unwrap();

        assert!(!status(&manager).signed_in);
        let store = super::super::storage::read_auth_json(&dir.path().join("auth.json")).unwrap();
        assert!(!store.contains_key(AUTH_SCOPE));
        assert_eq!(store.get("xai::scope").unwrap().key, "xai-secret");
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

    async fn token_error(body: serde_json::Value, status: StatusCode) -> TokenRequestError {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/token",
            axum::routing::post(move || {
                let body = body.clone();
                async move { (status, axum::Json(body)) }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        refresh_at(&format!("http://{addr}/token"), "rejected-refresh")
            .await
            .expect_err("error response must fail refresh")
    }

    #[tokio::test]
    async fn official_refresh_rejection_shapes_are_terminal() {
        for body in [
            serde_json::json!({"error": "refresh_token_expired"}),
            serde_json::json!({"error": {"code": "refresh_token_reused"}}),
            serde_json::json!({"code": "refresh_token_invalidated"}),
        ] {
            let error = token_error(body, StatusCode::BAD_REQUEST).await;
            assert_eq!(
                classify_refresh_error(&error),
                Some(RefreshTokenFailedReason::RefreshTokenRejected)
            );
        }
    }

    #[tokio::test]
    async fn invalid_client_is_terminal_for_nested_error_shape() {
        let error = token_error(
            serde_json::json!({"error": {"code": "invalid_client"}}),
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(
            classify_refresh_error(&error),
            Some(RefreshTokenFailedReason::ClientRejected)
        );
    }

    #[tokio::test]
    async fn unclassified_unauthorized_refresh_is_terminal_other() {
        let error = token_error(
            serde_json::json!({"error": {"code": "not_allowlisted"}}),
            StatusCode::UNAUTHORIZED,
        )
        .await;
        assert!(error.oauth_code.is_none());
        assert_eq!(
            classify_refresh_error(&error),
            Some(RefreshTokenFailedReason::Other)
        );
    }

    async fn classify_invalid_grant_description(description: &'static str) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/token",
            axum::routing::post(move || async move {
                (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": "invalid_grant",
                        "error_description": description,
                    })),
                )
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let error = refresh_at(&format!("http://{addr}/token"), "rejected-refresh")
            .await
            .expect_err("invalid_grant must fail refresh");
        assert_eq!(
            classify_refresh_error(&error),
            Some(RefreshTokenFailedReason::RefreshTokenRejected)
        );
    }

    #[tokio::test]
    async fn revoked_refresh_invalid_grant_is_terminal_refresh_rejection() {
        classify_invalid_grant_description("refresh token revoked").await;
    }

    #[tokio::test]
    async fn reused_refresh_invalid_grant_is_terminal_refresh_rejection() {
        classify_invalid_grant_description("refresh token already used").await;
    }

    #[test]
    fn workspace_claims_require_the_namespaced_typed_contract() {
        let token = jwt(serde_json::json!({
            AUTH_CLAIM_NAMESPACE: {
                "chatgpt_account_id": "acct-123",
                "chatgpt_account_is_fedramp": true
            }
        }));
        assert_eq!(
            workspace_claims_from_jwt(&token),
            Some(CodexWorkspaceClaims {
                account_id: Some("acct-123".into()),
                is_fedramp: true,
            })
        );

        for generic in [
            serde_json::json!({"account_id": "acct-generic"}),
            serde_json::json!({"chatgpt_account_id": "acct-flat"}),
            serde_json::json!({
                "https://api.openai.com/auth/chatgpt_account_id": "acct-slash"
            }),
        ] {
            let claims = workspace_claims_from_jwt(&jwt(generic)).unwrap();
            assert_eq!(claims.account_id, None);
            assert!(!claims.is_fedramp);
        }
        assert_eq!(workspace_claims_from_jwt("not-a-jwt"), None);
    }

    #[test]
    fn fedramp_claim_accepts_only_a_namespaced_json_boolean() {
        for value in [
            serde_json::json!("true"),
            serde_json::json!(1),
            serde_json::Value::Null,
        ] {
            let token = jwt(serde_json::json!({
                AUTH_CLAIM_NAMESPACE: { "chatgpt_account_is_fedramp": value }
            }));
            assert!(!workspace_claims_from_jwt(&token).unwrap().is_fedramp);
        }

        let generic = jwt(serde_json::json!({"chatgpt_account_is_fedramp": true}));
        assert!(!workspace_claims_from_jwt(&generic).unwrap().is_fedramp);
    }

    #[test]
    fn persisted_workspace_metadata_is_rederived_and_legacy_generic_values_are_cleared() {
        let mut auth = GrokAuth {
            id_token: Some(jwt(serde_json::json!({
                "account_id": "legacy-generic",
                "chatgpt_account_is_fedramp": true
            }))),
            account_id: Some("persisted-untrusted".into()),
            chatgpt_account_is_fedramp: true,
            ..previous_codex_auth()
        };

        normalize_workspace_metadata(&mut auth);

        assert!(auth.account_id.is_none());
        assert!(!auth.chatgpt_account_is_fedramp);
    }

    #[test]
    fn valid_jwt_without_account_claim_returns_none() {
        let token = jwt(serde_json::json!({
            "sub": "user-123",
            "email": "person@example.test"
        }));
        assert_eq!(account_id_from_jwt(&token), None);
    }

    #[test]
    fn access_token_jwt_exp_drives_expiry_without_expires_in() {
        let now = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let expected = now.checked_add_signed(Duration::hours(1)).unwrap();
        let access_token = jwt(serde_json::json!({"exp": expected.timestamp()}));
        assert_eq!(token_expiry(&access_token, None, now).unwrap(), expected);
    }

    #[test]
    fn missing_token_expiry_uses_checked_eight_day_fallback() {
        let now = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let expected = now
            .checked_add_signed(Duration::seconds(EXPIRY_FALLBACK_SECONDS))
            .unwrap();
        assert_eq!(
            token_expiry("opaque-access-token", None, now).unwrap(),
            expected
        );
    }

    #[test]
    fn earlier_expiry_wins_and_relative_overflow_is_rejected() {
        let now = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let jwt_expiry = now.checked_add_signed(Duration::hours(1)).unwrap();
        let access_token = jwt(serde_json::json!({"exp": jwt_expiry.timestamp()}));
        assert_eq!(
            token_expiry(&access_token, Some(7200), now).unwrap(),
            jwt_expiry
        );

        assert!(matches!(
            token_expiry("opaque-access-token", Some(u64::MAX), now),
            Err(CodexOAuthError::InvalidTokenResponse)
        ));
    }

    #[tokio::test]
    async fn older_codex_auth_schema_loads_and_migrates_with_safe_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let older = serde_json::json!({
            AUTH_SCOPE: {
                "key": "legacy-access",
                "auth_mode": "open_ai_codex",
                "create_time": "2026-01-01T00:00:00Z",
                "user_id": "",
                "email": null,
                "refresh_token": "legacy-refresh",
                "expires_at": "2099-01-01T00:00:00Z",
                "oidc_issuer": ISSUER,
                "oidc_client_id": CLIENT_ID
            }
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&older).unwrap()).unwrap();

        let manager = Arc::new(AuthManager::new_openai_codex(dir.path()));
        let auth = manager
            .current_or_expired()
            .expect("older Codex record loads");
        assert_eq!(auth.key, "legacy-access");
        assert_eq!(auth.refresh_token.as_deref(), Some("legacy-refresh"));
        assert!(auth.coding_data_retention_opt_out);
        assert!(auth.id_token.is_none());
        assert!(auth.account_id.is_none());
        assert!(!auth.chatgpt_account_is_fedramp);

        manager.save_without_enrichment(auth).await.unwrap();
        let migrated: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(migrated[AUTH_SCOPE]["coding_data_retention_opt_out"], true);
        assert_eq!(migrated[AUTH_SCOPE]["auth_mode"], "open_ai_codex");
        assert!(migrated[AUTH_SCOPE].get("account_id").is_none());
        assert!(
            migrated[AUTH_SCOPE]
                .get("chatgpt_account_is_fedramp")
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_codex_refresh_is_single_flight_and_persists_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new_openai_codex(dir.path()));
        manager
            .save_without_enrichment(GrokAuth {
                key: "expiring-access".into(),
                auth_mode: AuthMode::OpenAiCodex,
                refresh_token: Some("old-refresh".into()),
                expires_at: Some(Utc::now() + Duration::seconds(30)),
                oidc_issuer: Some(ISSUER.into()),
                oidc_client_id: Some(CLIENT_ID.into()),
                account_id: Some("old-account".into()),
                ..GrokAuth::default()
            })
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        manager.set_refresher(Arc::new(RotatingCodexRefresher {
            calls: Arc::clone(&calls),
        }));

        let results = futures_util::future::join_all((0..8).map(|_| {
            let manager = Arc::clone(&manager);
            async move { manager.auth().await }
        }))
        .await;

        assert!(results.iter().all(|result| {
            result.as_ref().is_ok_and(|auth| {
                auth.key == "rotated-access"
                    && auth.refresh_token.as_deref() == Some("rotated-refresh")
            })
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let disk = super::super::storage::read_auth_json(&dir.path().join("auth.json")).unwrap();
        let persisted = disk.get(AUTH_SCOPE).expect("rotated credential persisted");
        assert_eq!(persisted.key, "rotated-access");
        assert_eq!(persisted.refresh_token.as_deref(), Some("rotated-refresh"));
        assert_eq!(persisted.account_id.as_deref(), Some("rotated-account"));
        assert!(persisted.chatgpt_account_is_fedramp);
    }

    #[tokio::test]
    async fn near_expiry_terminal_rejection_makes_codex_unready() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new_openai_codex(dir.path()));
        manager
            .save_without_enrichment(GrokAuth {
                key: "expiring-access".into(),
                auth_mode: AuthMode::OpenAiCodex,
                refresh_token: Some("rejected-refresh".into()),
                expires_at: Some(Utc::now() + Duration::seconds(30)),
                oidc_issuer: Some(ISSUER.into()),
                oidc_client_id: Some(CLIENT_ID.into()),
                ..GrokAuth::default()
            })
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        manager.set_refresher(Arc::new(RejectingCodexRefresher {
            calls: Arc::clone(&calls),
        }));

        assert!(manager.auth().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!status(&manager).signed_in);
    }

    #[tokio::test]
    async fn status_exposes_active_permanent_failure_without_secret_details() {
        for reason in [
            RefreshTokenFailedReason::ClientRejected,
            RefreshTokenFailedReason::Other,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let manager = AuthManager::new_openai_codex(dir.path());
            let auth = previous_codex_auth();
            let key = auth.key.clone();
            manager.save_without_enrichment(auth).await.unwrap();
            manager.record_permanent_failure(key, reason.into());

            let auth_status = status(&manager);
            assert!(auth_status.permanent_failure);
            assert!(manager.has_permanent_failure());
        }
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
                axum::Json(serde_json::json!({
                    "access_token":"access",
                    "refresh_token":"refresh",
                    "id_token":"id",
                    "expires_in":3600
                }))
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
        assert_eq!(tokens.refresh_token, "refresh");
        assert_eq!(tokens.id_token, "id");
    }

    #[tokio::test]
    async fn code_exchange_missing_required_tokens_fails_closed() {
        for body in [
            serde_json::json!({"refresh_token":"refresh","id_token":"id"}),
            serde_json::json!({"access_token":"access","id_token":"id"}),
            serde_json::json!({"access_token":"access","refresh_token":"refresh"}),
        ] {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let router = Router::new().route(
                "/token",
                axum::routing::post(move || {
                    let body = body.clone();
                    async move { axum::Json(body) }
                }),
            );
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

            exchange_code_at(
                &format!("http://{addr}/token"),
                "code",
                "http://localhost:1455/auth/callback",
                "verifier",
            )
            .await
            .expect_err("missing required auth-code token must be rejected");
        }
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
        let previous = GrokAuth {
            key: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            auth_mode: AuthMode::OpenAiCodex,
            oidc_issuer: Some(ISSUER.into()),
            oidc_client_id: Some(CLIENT_ID.into()),
            ..GrokAuth::default()
        };
        let auth = merge_refresh_auth(tokens, &previous).unwrap();
        assert_eq!(auth.key, "new-access");
        assert_eq!(auth.refresh_token.as_deref(), Some("new-refresh"));
    }

    #[tokio::test]
    async fn refresh_token_only_rotation_preserves_access_expiry_and_identity() {
        use axum::extract::Json;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/token",
            axum::routing::post(|Json(body): Json<serde_json::Value>| async move {
                assert_eq!(body["refresh_token"], "old-refresh");
                axum::Json(serde_json::json!({"refresh_token":"rotated-refresh"}))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let previous = previous_codex_auth();
        let tokens = refresh_at(&format!("http://{addr}/token"), "old-refresh")
            .await
            .unwrap();
        let merged = merge_refresh_auth(tokens, &previous).unwrap();

        assert_eq!(merged.key, previous.key);
        assert_eq!(merged.refresh_token.as_deref(), Some("rotated-refresh"));
        assert_eq!(merged.id_token, previous.id_token);
        assert_eq!(merged.account_id, previous.account_id);
        assert_eq!(
            merged.chatgpt_account_is_fedramp,
            previous.chatgpt_account_is_fedramp
        );
        assert_eq!(merged.expires_at, previous.expires_at);
        assert_eq!(merged.create_time, previous.create_time);
    }

    #[tokio::test]
    async fn access_token_only_refresh_preserves_id_token_workspace_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(AuthManager::new_openai_codex(dir.path()));
        let previous = previous_codex_auth();
        manager
            .save_without_enrichment(previous.clone())
            .await
            .unwrap();

        let expiry = Utc
            .timestamp_opt((Utc::now() + Duration::hours(1)).timestamp(), 0)
            .unwrap();
        let access_token = jwt(serde_json::json!({
            "exp": expiry.timestamp(),
            AUTH_CLAIM_NAMESPACE: {
                "chatgpt_account_id": "new-account",
                "chatgpt_account_is_fedramp": true
            }
        }));
        let response_access_token = access_token.clone();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/token",
            axum::routing::post(move || {
                let access_token = response_access_token.clone();
                async move { axum::Json(serde_json::json!({"access_token":access_token})) }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let tokens = refresh_at(&format!("http://{addr}/token"), "old-refresh")
            .await
            .unwrap();
        let merged = merge_refresh_auth(tokens, &previous).unwrap();
        manager.update(merged).await.unwrap();

        let disk = super::super::storage::read_auth_json(&dir.path().join("auth.json")).unwrap();
        let persisted = disk.get(AUTH_SCOPE).expect("merged credential persisted");
        assert_eq!(persisted.key, access_token);
        assert_eq!(persisted.refresh_token, previous.refresh_token);
        assert_eq!(persisted.id_token, previous.id_token);
        assert_eq!(persisted.account_id.as_deref(), Some("old-account"));
        assert!(persisted.chatgpt_account_is_fedramp);
        assert_eq!(persisted.expires_at, Some(expiry));
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
