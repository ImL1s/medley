//! OAuth flow orchestration for local MCP servers.
//!
//! Uses rmcp's `AuthorizationManager` for RFC-compliant discovery (RFC 8414 +
//! 9728), DCR, PKCE, and token exchange. Proactive discovery determines auth
//! requirements before connecting (not reactively after a 401), and `AuthClient`
//! wraps the transport for transparent token injection and refresh.
//!
//! This module handles the interactive browser-based consent flow and
//! cross-process dedup. Credential persistence is delegated to
//! [`crate::credentials::McpCredentialStoreAdapter`] (implements rmcp's
//! `CredentialStore` trait).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{Mutex, oneshot, watch};

use crate::oauth_config::McpOAuthConfig;
use crate::rmcp::transport::auth::{AuthorizationManager, OAuthClientConfig};

/// Client name advertised to MCP servers during Dynamic Client Registration
/// (RFC 7591). Surfaces as the application name on third-party OAuth consent
/// screens (e.g. Linear, GitHub), so keep this human-recognizable.
const MCP_OAUTH_CLIENT_NAME: &str = "Grok";

/// How often the interactive OAuth flow polls the credential store to detect
/// a login completed in another window or process.
const CREDENTIAL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Overall budget for one interactive browser consent flow (waiting for the
/// loopback callback / disk poll after opening the browser). Mirrors the main
/// grok.com login's 10-minute callback budget. Without a bound, an abandoned
/// browser tab left the leader parked in its `select!` forever — holding both
/// the in-process watch channel and the cross-process `mcp_auth_*.lock`, so
/// every other session blocked indefinitely on the same server's auth.
const BROWSER_AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Convert rmcp's potentially provider-controlled OAuth errors into a fixed
/// diagnostic category. Several variants contain response bodies, callback
/// descriptions, issuer URLs, or reqwest errors with attached URLs.
pub(crate) fn oauth_error_class(error: &crate::rmcp::transport::auth::AuthError) -> &'static str {
    use crate::rmcp::transport::auth::AuthError;

    match error {
        AuthError::AuthorizationRequired => "authorization_required",
        AuthError::AuthorizationFailed(_) => "authorization_failed",
        AuthError::TokenExchangeFailed(_) => "token_exchange_failed",
        AuthError::TokenRefreshFailed(_) => "token_refresh_failed",
        AuthError::HttpError(err) if err.is_timeout() => "http_timeout",
        AuthError::HttpError(err) if err.is_connect() => "http_connect",
        AuthError::HttpError(_) => "http_error",
        AuthError::OAuthError(_) => "oauth_error",
        AuthError::MetadataError(_) => "metadata_error",
        AuthError::UrlError(_) => "url_error",
        AuthError::NoAuthorizationSupport => "no_authorization_support",
        AuthError::InternalError(_) => "internal_error",
        AuthError::InvalidTokenType(_) => "invalid_token_type",
        AuthError::TokenExpired => "token_expired",
        AuthError::InvalidScope(_) => "invalid_scope",
        AuthError::RegistrationFailed(_) => "registration_failed",
        AuthError::InsufficientScope { .. } => "insufficient_scope",
        AuthError::AuthorizationServerMismatch { .. } => "issuer_mismatch",
        AuthError::AuthorizationServerMissingIssuer { .. } => "issuer_missing",
        AuthError::ClientCredentialsError(_) => "client_credentials_error",
        _ => "oauth_error",
    }
}

/// How long a follower waits for the cross-process auth lock before giving up
/// on dedup and proceeding with its own flow. Slightly above
/// [`BROWSER_AUTH_TIMEOUT`] so a legitimately-slow leader (user reading the
/// consent screen) finishes first and the follower reuses its token.
#[cfg(unix)]
const AUTH_LOCK_WAIT: std::time::Duration =
    BROWSER_AUTH_TIMEOUT.saturating_add(std::time::Duration::from_secs(60));

// ---------------------------------------------------------------------------
// Two-layer dedup: prevents duplicate browser tabs both within one process
// (multiple async tasks / sessions) and across separate processes (leader
// mode disabled, multiple `grok` invocations).
//
// Layer 1 (cross-process): filesystem lock at $GROK_HOME/mcp_auth_{safe_name}.lock
// Layer 2 (in-process):    watch channel so only one task runs the flow
// ---------------------------------------------------------------------------

/// In-process in-flight auth tracker. Keyed by server name.
/// Each entry has a generation counter so that when a forced override evicts
/// a stale leader, the old leader's cleanup doesn't clobber the new entry.
struct InFlightEntry {
    rx: watch::Receiver<Option<Result<(), String>>>,
    generation: u64,
}

#[allow(clippy::type_complexity)]
static IN_FLIGHT_AUTH: std::sync::LazyLock<Mutex<HashMap<String, InFlightEntry>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Run the browser-based OAuth flow with dedup.
///
/// If another task or process is already running the flow for the same server,
/// waits for it instead of opening another browser tab. The provided
/// `AuthorizationManager` must already have metadata set (from
/// `discover_metadata`). On success, the manager's credentials are updated
/// and persisted via its `CredentialStore`.
///
/// When `force` is true (user-initiated auth), any existing in-flight entry
/// is evicted so a fresh browser flow starts immediately. The old leader's
/// browser tab becomes orphaned but its cleanup is generation-safe.
pub async fn authenticate_mcp_server_dedup(
    server_name: &str,
    server_url: &str,
    auth_manager: &Arc<Mutex<AuthorizationManager>>,
    byo_config: Option<&McpOAuthConfig>,
    force: bool,
) -> Result<(), String> {
    // --- Layer 2: in-process dedup via watch channel ---
    let mut in_flight = IN_FLIGHT_AUTH.lock().await;

    // Remove stale entries left by panicked leaders (sender dropped).
    if in_flight
        .get(server_name)
        .is_some_and(|e| e.rx.has_changed().is_err())
    {
        in_flight.remove(server_name);
    }

    if let Some(entry) = in_flight.get(server_name) {
        if force {
            tracing::info!(
                server = server_name,
                "User-initiated auth override; evicting stale in-flight entry"
            );
            in_flight.remove(server_name);
        } else {
            let mut rx = entry.rx.clone();
            drop(in_flight);
            tracing::info!(
                server = server_name,
                "Another task in this process is already authenticating; waiting..."
            );
            loop {
                let snapshot = rx.borrow_and_update().clone();
                if let Some(result) = snapshot {
                    if result.is_ok() {
                        let mut mgr = auth_manager.lock().await;
                        let _ = mgr.initialize_from_store().await;
                    }
                    return result;
                }
                if rx.changed().await.is_err() {
                    return Err("Auth leader dropped".to_string());
                }
            }
        }
    }

    // We are the in-process leader.
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = watch::channel::<Option<Result<(), String>>>(None);
    in_flight.insert(server_name.to_string(), InFlightEntry { rx, generation });
    drop(in_flight);

    // --- Layer 1: cross-process dedup via filesystem lock (Unix only) ---
    // When force is set, skip the fs lock — the old leader may still hold it
    // and we don't want to block behind a stale browser flow.
    #[cfg(unix)]
    let result = if force {
        run_browser_auth_flow(server_name, server_url, auth_manager, byo_config).await
    } else {
        authenticate_with_fs_lock(server_name, server_url, auth_manager, byo_config).await
    };
    #[cfg(not(unix))]
    let result = run_browser_auth_flow(server_name, server_url, auth_manager, byo_config).await;

    // Broadcast to in-process followers and clean up.
    // Only remove if our generation is still current (a force override may
    // have replaced us).
    let _ = tx.send(Some(result.clone()));
    let mut in_flight = IN_FLIGHT_AUTH.lock().await;
    if in_flight
        .get(server_name)
        .is_some_and(|e| e.generation == generation)
    {
        in_flight.remove(server_name);
    }

    result
}

/// Acquire a filesystem lock, then either run the auth flow (if we're the
/// first process) or reload credentials from disk (if another process
/// already completed auth while we waited for the lock).
#[cfg(unix)]
async fn authenticate_with_fs_lock(
    server_name: &str,
    server_url: &str,
    auth_manager: &Arc<Mutex<AuthorizationManager>>,
    byo_config: Option<&McpOAuthConfig>,
) -> Result<(), String> {
    use oauth2::TokenResponse as _;

    let lock_path = auth_lock_path(server_name);

    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Snapshot the current access token before waiting for the lock.
    // After acquiring, we compare to detect if another process authed.
    let token_before = {
        let mgr = auth_manager.lock().await;
        mgr.get_credentials()
            .await
            .ok()
            .and_then(|(_, tok)| tok)
            .map(|t| t.access_token().secret().to_string())
    };

    let lock_file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(_) => {
            tracing::warn!(
                error_class = "io_error",
                "Failed to create auth lock file; proceeding without cross-process dedup"
            );
            return run_browser_auth_flow(server_name, server_url, auth_manager, byo_config).await;
        }
    };

    // Bounded, non-blocking poll instead of an unbounded `flock(LOCK_EX)`:
    // the leader can legitimately hold this lock for minutes (user consent),
    // but an abandoned/wedged leader must not park followers forever. On
    // timeout we fall back to running our own flow (same as lock-acquisition
    // failure), which the token-changed re-check below keeps from producing a
    // duplicate consent when the leader did finish.
    let lock_file = tokio::task::spawn_blocking(move || {
        use std::os::unix::io::AsRawFd;
        let fd = lock_file.as_raw_fd();
        let deadline = std::time::Instant::now() + AUTH_LOCK_WAIT;
        loop {
            if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Some(lock_file);
            }
            let err = std::io::Error::last_os_error();
            match err.kind() {
                std::io::ErrorKind::Interrupted => continue,
                std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                _ => return None,
            }
        }
    })
    .await
    .ok()
    .flatten();

    if lock_file.is_none() {
        tracing::warn!("Timed out waiting for auth lock; re-checking the store before a new flow");
    }
    // On timeout: proceed unlocked; the token-changed re-check below
    // dedups a leader that finished just past our deadline.
    let _lock_guard = lock_file;

    // Reload from disk and check whether another process wrote a DIFFERENT
    // token while we waited (not just any token). This runs on the timeout
    // path too: a leader whose token exchange finished just past our deadline
    // has already written fresh tokens, and opening a second consent browser
    // would be strictly worse than this unlocked best-effort read.
    {
        let mut mgr = auth_manager.lock().await;
        if let Ok(true) = mgr.initialize_from_store().await {
            let token_after = mgr
                .get_credentials()
                .await
                .ok()
                .and_then(|(_, tok)| tok)
                .map(|t| t.access_token().secret().to_string());
            if token_after != token_before {
                tracing::info!(
                    server = server_name,
                    "Another process already authenticated; reusing fresh token"
                );
                return Ok(());
            }
        }
    }

    run_browser_auth_flow(server_name, server_url, auth_manager, byo_config).await
}

#[cfg(unix)]
fn auth_lock_path(server_name: &str) -> std::path::PathBuf {
    let safe: String = server_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    xai_grok_config::grok_home().join(format!("mcp_auth_{safe}.lock"))
}

/// Run the interactive browser-based OAuth flow.
///
/// Drives the `AuthorizationManager` (which must already have metadata set)
/// through client setup, authorization URL generation, browser consent, and
/// code exchange. Credentials are auto-persisted by the manager's
/// `CredentialStore` on successful token exchange.
async fn run_browser_auth_flow(
    server_name: &str,
    server_url: &str,
    auth_manager: &Arc<Mutex<AuthorizationManager>>,
    byo_config: Option<&McpOAuthConfig>,
) -> Result<(), String> {
    // 1. Try token refresh first (no browser needed).
    {
        let mgr = auth_manager.lock().await;
        match mgr.refresh_token().await {
            Ok(_) => {
                tracing::info!(
                    server = server_name,
                    "Token refreshed successfully (no browser)"
                );
                return Ok(());
            }
            Err(e) => {
                tracing::info!(
                    server = server_name,
                    oauth_error_class = oauth_error_class(&e),
                    "Token refresh failed, falling through to browser auth"
                );
            }
        }
    }

    // 2. Bind the loopback callback port (fixed BYO port if set, else ephemeral).
    let requested_port = byo_config.and_then(|b| b.callback_port).unwrap_or(0);
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], requested_port)))
            .await
            .map_err(|_| "Failed to bind OAuth loopback listener".to_string())?;
    let port = listener
        .local_addr()
        .map_err(|_| "Failed to inspect OAuth loopback listener".to_string())?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    // 3. Configure client and get authorization URL (lock held briefly).
    let byo_scopes: Vec<String> = byo_config
        .and_then(|b| b.scopes.as_ref())
        .cloned()
        .unwrap_or_default();

    let auth_url = {
        let mut mgr = auth_manager.lock().await;

        let scopes: Vec<String>;
        if let Some(byo) = byo_config
            && let Some(client_id) = byo.client_id.clone()
        {
            tracing::info!(
                server = server_name,
                "Using BYO client credentials (oauth_client_id from config)"
            );
            scopes = byo_scopes;
            let mut config =
                OAuthClientConfig::new(client_id, redirect_uri.clone()).with_scopes(scopes.clone());
            config.client_secret = byo.client_secret.clone();
            mgr.configure_client(config)
                .map_err(|_| "Failed to configure BYO OAuth client".to_string())?;
        } else {
            scopes = if byo_scopes.is_empty() {
                mgr.select_scopes(None, &[])
            } else {
                byo_scopes
            };
            let scope_refs: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
            mgr.register_client(MCP_OAUTH_CLIENT_NAME, &redirect_uri, &scope_refs)
                .await
                .map_err(|_| "Dynamic OAuth client registration failed".to_string())?;
        }
        let scopes: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();

        mgr.get_authorization_url(&scopes)
            .await
            .map_err(|_| "Failed to create OAuth authorization URL".to_string())?
    };
    // Lock released — browser flow can take minutes.

    // Snapshot token before browser opens so we can detect if another flow
    // (e.g. the old evicted leader, or another process) writes fresh tokens.
    let token_before_browser = {
        let mgr = auth_manager.lock().await;
        mgr.get_credentials()
            .await
            .ok()
            .and_then(|(_, tok)| tok)
            .map(|t| {
                use oauth2::TokenResponse as _;
                t.access_token().secret().to_string()
            })
    };

    // 4. Open browser for user consent.
    tracing::info!(server = server_name, "Opening browser for OAuth consent");
    if webbrowser::open(&auth_url).is_err() {
        // eprintln! corrupts the TUI alternate screen (in-process, fd 2).
        // Do not surface the authorization URL: it contains OAuth state and
        // other security-sensitive query parameters.
        tracing::warn!(
            "Failed to open browser for MCP OAuth; restart authentication with a supported browser"
        );
    }

    // 5. Wait for the OAuth callback OR for tokens to appear on disk.
    //    The credential store poll catches the case where a force-evicted
    //    old leader (or another process) completed auth via a different
    //    browser tab while we're waiting for our own callback.
    //
    //    IMPORTANT: peek the on-disk file directly via `McpCredentialStore::load_default`
    //    rather than calling `mgr.initialize_from_store()`. The latter has the
    //    side effect of running `configure_client_id(stored.client_id)`, which
    //    replaces `oauth_client`'s freshly-DCR'd `client_id` and ephemeral
    //    `redirect_uri` with the *old* stored values (`redirect_uri = base_url`).
    //    If the user then completes the browser flow before another process
    //    writes new tokens, `exchange_code_for_token` would use the clobbered
    //    config and the server would reject with `invalid_grant: Invalid redirect_uri`.
    let parsed_server_url = match url::Url::parse(server_url) {
        Ok(u) => Some(u),
        Err(_) => {
            tracing::warn!(
                server = server_name,
                server_url_present = !server_url.is_empty(),
                "could not parse server URL for credential-store poll; falling back to callback-only auth-completion detection"
            );
            None
        }
    };
    let server_name_for_poll = server_name.to_string();
    let token_snapshot = token_before_browser.clone();
    let poll_store = async move {
        let Some(url) = parsed_server_url else {
            // No credential-store key — disable the poll. Callback path still works.
            std::future::pending::<()>().await;
            return;
        };
        loop {
            tokio::time::sleep(CREDENTIAL_POLL_INTERVAL).await;
            let Ok(store) = crate::credentials::McpCredentialStore::load_default() else {
                continue;
            };
            let token_now = store
                .get(&server_name_for_poll, &url)
                .and_then(|entry| entry.token_response.as_ref())
                .map(|t| {
                    use oauth2::TokenResponse as _;
                    t.access_token().secret().to_string()
                });
            if token_now.is_some() && token_now != token_snapshot {
                return;
            }
        }
    };

    let (callback_server, callback_rx) = start_oauth_callback_server(listener);

    tokio::select! {
        result = callback_rx => {
            callback_server.abort();
            let callback = result
                .map_err(|_| "Callback channel dropped".to_string())?
                .map_err(|_| "OAuth callback failed".to_string())?;

            // 6. Exchange code for tokens (auto-persists via CredentialStore).
            // Pass RFC 9207 `iss` when present (required if the AS advertises it).
            let mgr = auth_manager.lock().await;
            mgr.exchange_code_for_token_with_issuer(
                &callback.code,
                &callback.state,
                callback.issuer.as_deref(),
            )
            .await
            .map_err(|_| "OAuth token exchange failed".to_string())?;

            tracing::info!(server = server_name, "MCP OAuth authentication successful");
        }
        _ = poll_store => {
            callback_server.abort();
            tracing::info!(
                server = server_name,
                "Fresh tokens detected on disk from another auth flow; skipping callback wait"
            );
        }
        // Abandoned consent: bound the wait so this leader releases the
        // in-process watch and the cross-process `mcp_auth_*.lock` instead of
        // wedging every future auth attempt for this server (see
        // `BROWSER_AUTH_TIMEOUT`).
        _ = tokio::time::sleep(BROWSER_AUTH_TIMEOUT) => {
            callback_server.abort();
            tracing::warn!(
                server = server_name,
                timeout_secs = BROWSER_AUTH_TIMEOUT.as_secs(),
                "OAuth consent timed out (browser flow abandoned?)"
            );
            return Err(format!(
                "OAuth consent timed out after {}s; re-run authentication to try again",
                BROWSER_AUTH_TIMEOUT.as_secs()
            ));
        }
    }

    Ok(())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[derive(Clone, PartialEq, Eq)]
struct OAuthCallbackPayload {
    code: String,
    state: String,
    /// RFC 9207 `iss` (optional; required when the AS advertises support).
    issuer: Option<String>,
}

impl std::fmt::Debug for OAuthCallbackPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCallbackPayload")
            .field("code_present", &!self.code.is_empty())
            .field("state_present", &!self.state.is_empty())
            .field("issuer_present", &self.issuer.is_some())
            .finish()
    }
}

fn parse_oauth_callback_params(
    params: &HashMap<String, String>,
) -> Result<OAuthCallbackPayload, String> {
    if let Some(error) = params.get("error") {
        return Err(format!(
            "OAuth authorization failed ({})",
            safe_oauth_callback_error_code(error)
        ));
    }
    let code = params
        .get("code")
        .filter(|s| !s.is_empty())
        .cloned()
        .ok_or_else(|| "Missing authorization code".to_string())?;
    let state = params
        .get("state")
        .filter(|s| !s.is_empty())
        .cloned()
        .ok_or_else(|| "Missing state parameter".to_string())?;
    let issuer = params.get("iss").cloned();
    Ok(OAuthCallbackPayload {
        code,
        state,
        issuer,
    })
}

/// OAuth callback parameters are provider-controlled and may echo credentials.
/// Retain only standardized error categories; never render descriptions or
/// unknown values into the browser, terminal, logs, or propagated errors.
fn safe_oauth_callback_error_code(value: &str) -> &'static str {
    match value {
        "access_denied" => "access_denied",
        "account_selection_required" => "account_selection_required",
        "consent_required" => "consent_required",
        "interaction_required" => "interaction_required",
        "invalid_request" => "invalid_request",
        "invalid_scope" => "invalid_scope",
        "login_required" => "login_required",
        "server_error" => "server_error",
        "temporarily_unavailable" => "temporarily_unavailable",
        "unauthorized_client" => "unauthorized_client",
        "unsupported_response_type" => "unsupported_response_type",
        _ => "unknown",
    }
}

/// Loopback OAuth callback server. Returns (server task, oneshot for payload).
/// Caller must abort the server task.
#[allow(clippy::type_complexity)]
fn start_oauth_callback_server(
    listener: tokio::net::TcpListener,
) -> (
    tokio::task::JoinHandle<()>,
    oneshot::Receiver<Result<OAuthCallbackPayload, String>>,
) {
    use axum::{Router, extract::Query, response::Html, routing::get};

    let (tx, rx) = oneshot::channel::<Result<OAuthCallbackPayload, String>>();
    let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));

    let handler = {
        let tx = tx.clone();
        move |Query(params): Query<HashMap<String, String>>| {
            let tx = tx.clone();
            async move {
                let result = parse_oauth_callback_params(&params);

                let html = match &result {
                    Ok(_) => {
                        r#"<!DOCTYPE html><html><head><title>Authorization Complete</title></head>
                    <body style="font-family: sans-serif; text-align: center; padding: 50px;">
                    <h1>Authorization Complete</h1>
                    <p>You can close this window and return to the terminal.</p>
                    <script>window.close();</script></body></html>"#
                            .to_string()
                    }
                    Err(e) => {
                        let msg = html_escape(e);
                        format!(
                            r#"<!DOCTYPE html><html><head><title>Authorization Failed</title></head>
                            <body style="font-family: sans-serif; text-align: center; padding: 50px;">
                            <h1>Authorization Failed</h1>
                            <p>{msg}</p>
                            <p>You can close this window and return to the terminal.</p>
                            </body></html>"#
                        )
                    }
                };

                if let Some(tx) = tx.lock().await.take() {
                    let _ = tx.send(result);
                }

                Html(html)
            }
        }
    };

    let app = Router::new().route("/callback", get(handler.clone()).post(handler));

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (server, rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rmcp::transport::auth::{
        AuthorizationManager, AuthorizationMetadata, OAuthClientConfig,
    };

    const TEST_ISSUER: &str = "https://auth.example.com";

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn assert_no_secret_fragments(output: &str, sentinel: &str) {
        assert!(!output.contains(sentinel));
        for window in sentinel.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(
                !output.contains(fragment),
                "credential fragment {fragment:?} leaked in {output:?}"
            );
        }
    }

    const CREDENTIAL_SENTINELS: &[&str] = &[
        "ACCESS_TOKEN_A1b2C3d4E5f6G7h8",
        "REFRESH_TOKEN_R8s7T6u5V4w3X2y1",
        "DEPLOYMENT_KEY_D9e8P7l6O5y4",
        "Bearer b3A4r5E6r7T8o9K0e1N2",
        "x-api-key=K3y4V5a6L7u8E9",
        "https://mcp.example.test/oauth/callback?code=Q1w2E3r4T5y6&state=S7t8A9t0E1",
    ];

    fn assert_no_credentials(output: &str) {
        for sentinel in CREDENTIAL_SENTINELS {
            assert_no_secret_fragments(output, sentinel);
        }
    }

    #[test]
    fn callback_parses_code_state_and_rfc9207_iss() {
        let p = params(&[
            ("code", "auth-code"),
            ("state", "csrf"),
            ("iss", TEST_ISSUER),
        ]);
        let got = parse_oauth_callback_params(&p).unwrap();
        assert_eq!(got.code, "auth-code");
        assert_eq!(got.state, "csrf");
        assert_eq!(got.issuer.as_deref(), Some(TEST_ISSUER));
    }

    #[test]
    fn callback_issuer_optional_for_legacy_servers() {
        let p = params(&[("code", "c"), ("state", "s")]);
        let got = parse_oauth_callback_params(&p).unwrap();
        assert!(got.issuer.is_none());
    }

    #[test]
    fn callback_payload_debug_never_emits_code_state_or_issuer() {
        let sentinel = "R7t5Y3u1I9o7P5a3S1d9F7g5";
        let got = parse_oauth_callback_params(&params(&[
            ("code", sentinel),
            ("state", sentinel),
            ("iss", sentinel),
        ]))
        .expect("callback should parse");
        assert_no_secret_fragments(&format!("{got:?}"), sentinel);
    }

    #[test]
    fn callback_requires_code_and_state() {
        assert!(parse_oauth_callback_params(&params(&[("state", "s")])).is_err());
        assert!(parse_oauth_callback_params(&params(&[("code", "c")])).is_err());
    }

    #[test]
    fn callback_classifies_oauth_error_without_description() {
        let p = params(&[
            ("error", "access_denied"),
            ("error_description", "user said no"),
        ]);
        let err = parse_oauth_callback_params(&p).unwrap_err();
        assert!(err.contains("access_denied"));
        assert!(!err.contains("user said no"));
    }

    #[test]
    fn callback_errors_never_echo_provider_controlled_credentials() {
        let sentinel = "M8n6B4v2C0x8Z6l4K2j0H8g6";
        let p = params(&[("error", sentinel), ("error_description", sentinel)]);
        let err = parse_oauth_callback_params(&p).unwrap_err();
        assert_eq!(err, "OAuth authorization failed (unknown)");
        assert_no_secret_fragments(&err, sentinel);
    }

    #[test]
    fn callback_debug_and_failures_hide_all_credential_shapes_and_url_queries() {
        let joined = CREDENTIAL_SENTINELS.join(" | ");
        let payload = parse_oauth_callback_params(&params(&[
            ("code", &joined),
            ("state", &joined),
            ("iss", &joined),
        ]))
        .expect("callback should parse");
        assert_no_credentials(&format!("{payload:?}"));

        let error = parse_oauth_callback_params(&params(&[
            ("error", &joined),
            ("error_description", &joined),
            ("error_uri", &joined),
        ]))
        .expect_err("provider error should fail");
        assert_eq!(error, "OAuth authorization failed (unknown)");
        assert_no_credentials(&error);
    }

    fn require_iss_metadata(token_endpoint: String) -> AuthorizationMetadata {
        // non_exhaustive: build via Default.
        let mut meta = AuthorizationMetadata::default();
        meta.authorization_endpoint = "https://auth.example.com/authorize".to_string();
        meta.token_endpoint = token_endpoint;
        meta.issuer = Some(TEST_ISSUER.to_string());
        meta.additional_fields.insert(
            "authorization_response_iss_parameter_supported".to_string(),
            serde_json::json!(true),
        );
        meta
    }

    async fn manager_ready_for_exchange(token_endpoint: String) -> (AuthorizationManager, String) {
        let mut mgr = AuthorizationManager::new("http://localhost/mcp")
            .await
            .unwrap();
        mgr.set_metadata(require_iss_metadata(token_endpoint));
        mgr.configure_client(
            OAuthClientConfig::new("grok-test-client", "http://127.0.0.1:0/callback")
                .with_application_type("native"),
        )
        .unwrap();
        let auth_url = mgr.get_authorization_url(&[]).await.unwrap();
        let state = url::Url::parse(&auth_url)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "state")
            .expect("auth URL must include state")
            .1
            .into_owned();
        (mgr, state)
    }

    async fn start_mock_token_endpoint() -> String {
        use axum::{Router, body::Body, http::Response, routing::post};
        let app = Router::new().route(
            "/token",
            post(|| async {
                Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"access_token":"at-ok","token_type":"Bearer","expires_in":3600,"refresh_token":"rt-ok"}"#,
                    ))
                    .unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/token")
    }

    #[tokio::test]
    async fn after_fix_passes_iss_and_token_exchange_succeeds() {
        let token_ep = start_mock_token_endpoint().await;
        let (mgr, state) = manager_ready_for_exchange(token_ep).await;

        let callback = parse_oauth_callback_params(&params(&[
            ("code", "auth-code"),
            ("state", &state),
            ("iss", TEST_ISSUER),
        ]))
        .unwrap();

        let token = mgr
            .exchange_code_for_token_with_issuer(
                &callback.code,
                &callback.state,
                callback.issuer.as_deref(),
            )
            .await
            .expect("with_issuer must succeed when callback iss matches AS");

        use oauth2::TokenResponse as _;
        assert_eq!(token.access_token().secret(), "at-ok");
    }

    #[tokio::test]
    async fn callback_http_server_forwards_iss_query_param() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (server, rx) = start_oauth_callback_server(listener);

        let url = format!(
            "http://{addr}/callback?code=c1&state=s1&iss={}",
            urlencoding_encode(TEST_ISSUER)
        );
        let resp = reqwest::get(&url).await.unwrap();
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert!(body.contains("Authorization Complete"));

        let payload = rx.await.unwrap().unwrap();
        assert_eq!(payload.code, "c1");
        assert_eq!(payload.state, "s1");
        assert_eq!(payload.issuer.as_deref(), Some(TEST_ISSUER));
        server.abort();
    }

    fn urlencoding_encode(s: &str) -> String {
        s.replace(':', "%3A").replace('/', "%2F")
    }
}
