//! WebSocket server for remote agent connections.
//!
//! This module provides a WebSocket server that allows remote TUI clients to
//! connect to a grok agent running on a different machine.
//!
//! The agent persists across WebSocket reconnections: a single MvpAgent instance
//! is created on first connection and reused for all subsequent connections. This
//! ensures that session actors (and any in-flight prompts) survive client
//! disconnects — when a client reconnects and loads an existing session, ongoing
//! work continues to stream to the new connection.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    extract::{
        ConnectInfo, Query, State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, simplex};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{info, warn};

use agent_client_protocol as acp;
use xai_acp_lib::{
    AcpAgentGatewayReceiver as GatewayReceiver, AcpAgentGatewaySender as GatewaySender,
    AcpClientMessage, LineBufferedRead,
};

use crate::agent::config::{Config as AgentConfig, ModelEntry};
use crate::agent::models::{ModelFetchAuth, prefetch_models_blocking};
use crate::agent::mvp_agent::MvpAgent;

use indexmap::IndexMap;
use sha2::{Digest as _, Sha256};

/// Swappable destination for the relay task.
///
/// Points at the current ACP connection's gateway sender. When no client is
/// connected, the value is `None` and outbound messages are silently dropped
/// (matching the old behaviour where the gateway channel's receiver was simply
/// gone).
type RelayDest = Rc<RefCell<Option<mpsc::Sender<AcpClientMessage>>>>;

const MAX_BUFFER_SIZE: usize = 8 * 1024 * 1024;
const KEEPALIVE_INTERVAL_SECS: u64 = 15;
const AUTH_FAILURE_WARN_INTERVAL_SECS: u64 = 30;
pub const MIN_REMOTE_SECRET_BYTES: usize = 32;
type SecretDigest = [u8; 32];

/// Maximum accepted inbound WebSocket text/binary payload size (bytes).
///
/// Chosen as an explicit transport hardening bound for remote ACP-over-WS:
/// large enough for substantial ACP JSON-RPC payloads, small enough that the
/// bounded bridge queues cap worst-case queued bytes per connection. This is
/// independent of tool-protocol `max_frame_bytes` (which is optional there).
/// Tungstenite's hard ceiling stays at [`MAX_BUFFER_SIZE`], so this
/// application-level check (close 1009) is what enforces the 1 MiB cap.
pub const MAX_INBOUND_WS_MESSAGE_BYTES: usize = 1_048_576;

/// Per-connection depth for the authenticated WS↔agent bridge queues.
///
/// Sized for a burst of ACP chatter (initialize, session ops, streamed
/// chunks) without letting one connection retain unbounded pending frames.
/// Bounds are **per connection** — not a global queue — so one saturated
/// client cannot starve unrelated sessions.
pub const WS_BRIDGE_QUEUE_CAPACITY: usize = 32;

/// Shared relay depth from persistent session actors into the currently active
/// ACP connection. Bounds apply across reconnects and cap burst retention when
/// the active WebSocket consumer slows down.
pub const WS_GATEWAY_QUEUE_CAPACITY: usize = 128;

fn close_reason_diagnostic(reason: &str) -> (bool, usize) {
    (!reason.is_empty(), reason.len())
}

fn oversized_close_frame() -> CloseFrame {
    CloseFrame {
        code: close_code::SIZE,
        reason: "".into(),
    }
}

fn overload_close_frame() -> CloseFrame {
    CloseFrame {
        code: close_code::AGAIN,
        reason: "".into(),
    }
}

/// Admit one inbound WS payload into the per-connection agent queue.
///
/// Returns `Ok(true)` when enqueued, `Ok(false)` when ignored (empty/ping),
/// or `Err(close)` when the frame must terminate the connection (1009 size /
/// 1013 overload). Inbound saturation **closes** the offender; outbound uses
/// async backpressure instead (see [`enqueue_outbound_with_backpressure`]).
///
/// A disconnected agent bridge returns `Err(None)` so the caller can stop
/// without emitting a policy close code.
fn admit_inbound_ws_payload(
    to_agent: &mpsc::Sender<String>,
    payload: &str,
) -> Result<bool, Option<CloseFrame>> {
    if payload.len() > MAX_INBOUND_WS_MESSAGE_BYTES {
        return Err(Some(oversized_close_frame()));
    }
    let trimmed = payload.trim_end_matches(['\r', '\n']);
    if trimmed == "ping" || trimmed.is_empty() {
        return Ok(false);
    }
    match to_agent.try_send(trimmed.to_string()) {
        Ok(()) => Ok(true),
        Err(mpsc::error::TrySendError::Full(_)) => Err(Some(overload_close_frame())),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(None),
    }
}

fn admit_inbound_ws_binary_payload(
    to_agent: &mpsc::Sender<String>,
    payload: &[u8],
) -> Result<bool, Option<CloseFrame>> {
    if payload.len() > MAX_INBOUND_WS_MESSAGE_BYTES {
        return Err(Some(oversized_close_frame()));
    }
    let Ok(text) = std::str::from_utf8(payload) else {
        return Ok(false);
    };
    admit_inbound_ws_payload(to_agent, text)
}

/// Forward one outbound agent message with backpressure.
///
/// A legitimate slow consumer on a large streamed response stalls here once
/// the per-connection outbound queue is full, instead of growing memory or
/// disconnecting mid-stream.
async fn enqueue_outbound_with_backpressure(
    to_ws: &mpsc::Sender<String>,
    msg: String,
) -> Result<(), mpsc::error::SendError<String>> {
    to_ws.send(msg).await
}

/// Configuration for the agent WebSocket server.
#[derive(Clone)]
pub struct ServerConfig {
    /// Address to bind the server to
    pub bind_addr: SocketAddr,
    /// Secret token for client authentication (required)
    pub secret: String,
    /// Explicit acknowledgement required when binding beyond loopback.
    pub allow_remote: bool,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("bind_addr", &self.bind_addr)
            .field("secret_present", &!self.secret.is_empty())
            .field("allow_remote", &self.allow_remote)
            .finish()
    }
}

/// Shared state for the WebSocket server.
struct ServerState {
    agent_config: AgentConfig,
    secret_digest: SecretDigest,
    allow_legacy_query_auth: bool,
    last_auth_failure_warning: AtomicU64,
    /// Channel to send new WebSocket connections to the persistent agent thread.
    /// Lazily initialised on first connection; protected by a tokio Mutex so the
    /// axum handler (which is `Send`) can acquire it.
    agent_conn_tx: tokio::sync::Mutex<Option<mpsc::UnboundedSender<NewConnectionChannels>>>,
}

/// Channels bridging a single WebSocket connection to the agent thread.
struct NewConnectionChannels {
    from_ws_rx: mpsc::Receiver<String>,
    to_ws_tx: mpsc::Sender<String>,
}

/// Query parameters for WebSocket connection.
// No `Debug` in the derive: the manual impl below reports
// `server_key_present` instead of the key. Upstream's derive would print it.
#[derive(serde::Deserialize, Default)]
pub(crate) struct WsQueryParams {
    #[serde(rename = "server-key")]
    pub server_key: Option<String>,
}

impl std::fmt::Debug for WsQueryParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsQueryParams")
            .field("server_key_present", &self.server_key.is_some())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthSource {
    Bearer,
    LegacyQuery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthDecision {
    Authenticated(AuthSource),
    Rejected,
}

fn digest_secret(secret: &str) -> SecretDigest {
    Sha256::digest(secret.as_bytes()).into()
}

fn secrets_match(candidate: &str, expected_digest: &SecretDigest) -> bool {
    let candidate_digest = digest_secret(candidate);
    constant_time_eq::constant_time_eq_32(&candidate_digest, expected_digest)
}

fn should_warn_auth_failure(last_warning: &AtomicU64, now: u64) -> bool {
    let mut previous = last_warning.load(Ordering::Relaxed);
    loop {
        if now.saturating_sub(previous) < AUTH_FAILURE_WARN_INTERVAL_SECS {
            return false;
        }
        match last_warning.compare_exchange_weak(
            previous,
            now,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(observed) => previous = observed,
        }
    }
}

/// Authenticate a request without allowing malformed headers to downgrade to
/// the legacy query-string mechanism.
fn validate_auth(
    headers: &HeaderMap,
    query: &WsQueryParams,
    expected_secret: &SecretDigest,
    allow_legacy_query_auth: bool,
) -> AuthDecision {
    let authorization_values = headers.get_all(axum::http::header::AUTHORIZATION);
    let mut values = authorization_values.iter();
    if let Some(value) = values.next() {
        // Multiple Authorization fields are ambiguous and must be rejected.
        if values.next().is_some() {
            return AuthDecision::Rejected;
        }
        let Ok(value) = value.to_str() else {
            return AuthDecision::Rejected;
        };
        let Some(token) = value.strip_prefix("Bearer ") else {
            return AuthDecision::Rejected;
        };
        return if !token.is_empty() && secrets_match(token, expected_secret) {
            AuthDecision::Authenticated(AuthSource::Bearer)
        } else {
            AuthDecision::Rejected
        };
    }

    if allow_legacy_query_auth
        && let Some(key) = query.server_key.as_deref()
        && !key.is_empty()
        && secrets_match(key, expected_secret)
    {
        return AuthDecision::Authenticated(AuthSource::LegacyQuery);
    }

    AuthDecision::Rejected
}

/// WebSocket upgrade handler with authentication.
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<WsQueryParams>,
) -> Response {
    // Validate secret token from header or query param
    match validate_auth(
        &headers,
        &query,
        &state.secret_digest,
        state.allow_legacy_query_auth,
    ) {
        AuthDecision::Authenticated(AuthSource::Bearer) => {}
        AuthDecision::Authenticated(AuthSource::LegacyQuery) => {
            warn!(
                peer = %addr,
                "Authenticated via deprecated server-key query parameter; use an Authorization Bearer header"
            );
        }
        AuthDecision::Rejected => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or_default();
            if should_warn_auth_failure(&state.last_auth_failure_warning, now) {
                warn!(peer = %addr, "Unauthorized connection attempt");
            }
            return (
                StatusCode::UNAUTHORIZED,
                "Invalid or missing authorization token",
            )
                .into_response();
        }
    }

    info!("Authenticated WebSocket connection from {}", addr);
    ws.max_message_size(MAX_BUFFER_SIZE)
        .max_frame_size(MAX_BUFFER_SIZE)
        .on_upgrade(move |socket| handle_connection(socket, state, addr))
}

/// Handle an authenticated WebSocket connection.
///
/// On first connection, spawns a persistent agent thread that owns the MvpAgent.
/// On subsequent connections (reconnects), sends new WS channels to the existing
/// agent thread so that session actors can continue streaming to the new client.
async fn handle_connection(ws: WebSocket, state: Arc<ServerState>, peer_addr: SocketAddr) {
    info!("New WebSocket connection from {}", peer_addr);

    let (mut ws_write, mut ws_read) = ws.split();

    // Per-connection bounded bridges — never share these across sessions.
    let (to_agent_tx, to_agent_rx) = mpsc::channel::<String>(WS_BRIDGE_QUEUE_CAPACITY);
    let (from_agent_tx, mut from_agent_rx) = mpsc::channel::<String>(WS_BRIDGE_QUEUE_CAPACITY);
    let (policy_close_tx, mut policy_close_rx) = mpsc::channel::<CloseFrame>(1);

    // Ensure the persistent agent thread is running (lazy init on first connection).
    // If the previous agent thread died (panic, etc.), clear the stale sender so we
    // respawn a fresh one.
    {
        let mut agent_tx_guard = state.agent_conn_tx.lock().await;

        // Check if existing sender is still alive (receiver not dropped)
        if let Some(ref tx) = *agent_tx_guard
            && tx.is_closed()
        {
            warn!("Persistent agent thread died — will respawn");
            *agent_tx_guard = None;
        }

        if agent_tx_guard.is_none() {
            let (conn_tx, conn_rx) = mpsc::unbounded_channel();

            let agent_config = state.agent_config.clone();
            let _agent_thread = thread::Builder::new()
                .name("agent-persistent".to_string())
                .spawn(move || {
                    // Prefetch models before creating the runtime (blocking is OK here)
                    let auth = agent_config.create_auth_manager().current();
                    let fetch_auth =
                        ModelFetchAuth::resolve(&agent_config.endpoints, auth.is_some());
                    let prefetched_models = if auth.is_some()
                        || agent_config.endpoints.has_custom_endpoint()
                        || fetch_auth != ModelFetchAuth::Session
                    {
                        prefetch_models_blocking(&agent_config.endpoints, auth.as_ref(), fetch_auth)
                    } else {
                        None
                    };

                    info!("Prefetched models: {:?}", prefetched_models);

                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create runtime for agent");

                    let local_set = tokio::task::LocalSet::new();
                    local_set.block_on(&rt, async move {
                        run_persistent_agent(agent_config, conn_rx, prefetched_models).await
                    });

                    warn!("Persistent agent thread exiting");
                });

            *agent_tx_guard = Some(conn_tx);
            info!("Persistent agent thread spawned");
        }

        // Send new WS channels to the agent thread
        if let Some(ref tx) = *agent_tx_guard
            && tx
                .send(NewConnectionChannels {
                    from_ws_rx: to_agent_rx,
                    to_ws_tx: from_agent_tx,
                })
                .is_err()
        {
            warn!("Failed to send connection channels to agent thread");
        }
    }

    // Task: Read from WS, admit into the bounded agent queue (close on policy reject)
    let read_task = tokio::spawn(async move {
        while let Some(msg) = ws_read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let text_str: &str = text.as_ref();
                    match admit_inbound_ws_payload(&to_agent_tx, text_str) {
                        Ok(_) => {}
                        Err(Some(frame)) => {
                            let _ = policy_close_tx.try_send(frame);
                            break;
                        }
                        Err(None) => break,
                    }
                }
                Ok(Message::Binary(bin)) => {
                    match admit_inbound_ws_binary_payload(&to_agent_tx, &bin) {
                        Ok(_) => {}
                        Err(Some(frame)) => {
                            let _ = policy_close_tx.try_send(frame);
                            break;
                        }
                        Err(None) => break,
                    }
                }
                Ok(Message::Close(frame)) => {
                    if let Some(f) = frame {
                        let (reason_present, reason_len) = close_reason_diagnostic(&f.reason);
                        info!(
                            peer = %peer_addr,
                            code = f.code,
                            reason_present,
                            reason_len,
                            "WebSocket close received"
                        );
                    }
                    break;
                }
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                Err(_) => {
                    warn!(peer = %peer_addr, error_class = "websocket_read", "WebSocket read error");
                    break;
                }
            }
        }
    });

    // Task: Read from agent thread, send to WS (with keepalive); emit policy closes
    let write_task = tokio::spawn(async move {
        let mut keepalive = tokio::time::interval(Duration::from_secs(KEEPALIVE_INTERVAL_SECS));

        loop {
            tokio::select! {
                Some(msg) = from_agent_rx.recv() => {
                    if ws_write.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
                Some(frame) = policy_close_rx.recv() => {
                    let _ = ws_write.send(Message::Close(Some(frame))).await;
                    break;
                }
                _ = keepalive.tick() => {
                    if ws_write.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
                else => break,
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = read_task => {}
        _ = write_task => {}
    }

    info!("WebSocket connection ended for {}", peer_addr);
}

/// Run the persistent agent on a dedicated thread with LocalSet.
///
/// The MvpAgent is created **once** and reused across WebSocket reconnections.
/// A persistent gateway channel ensures that session actors (which hold cloned
/// `GatewaySender` handles) can always send notifications. A relay task forwards
/// messages from the persistent channel to the *current* ACP connection's channel,
/// so notifications reach whichever client is currently connected.
async fn run_persistent_agent(
    agent_config: AgentConfig,
    mut connection_rx: mpsc::UnboundedReceiver<NewConnectionChannels>,
    prefetched_models: Option<IndexMap<String, ModelEntry>>,
) {
    // Persistent bounded gateway channel — the MvpAgent and all session actors
    // hold clones of `gw_tx`. This survives across reconnections while capping
    // how many outbound ACP messages can queue behind a slow WS consumer.
    let (gw_tx, mut gw_rx) =
        tokio::sync::mpsc::channel::<AcpClientMessage>(WS_GATEWAY_QUEUE_CAPACITY);
    let gateway = GatewaySender::new_bounded(gw_tx);

    // Create MvpAgent ONCE -- it persists for the lifetime of the server.
    let auth_manager = Arc::new(agent_config.create_auth_manager());
    // Proactive token refresh; runs until process exit.
    auth_manager.start_proactive_refresh(tokio_util::sync::CancellationToken::new());
    // Restore managed policy right before bootstrap reads it — the agent is created lazily here,
    // so an earlier restore could go stale before the gate.
    crate::managed_config::ensure_managed_policy_present(&auth_manager).await;
    crate::agent::app::apply_otel_config(&auth_manager, &agent_config.grok_com_config);
    let agent = Rc::new(
        MvpAgent::new(gateway, &agent_config, auth_manager, prefetched_models)
            .unwrap_or_else(crate::agent::init::exit_on_config_error),
    );

    let relay_dest: RelayDest = Rc::new(RefCell::new(None));

    // Relay task: reads from the persistent gateway channel and forwards to
    // whichever ACP connection is currently active.
    let relay_dest_for_task = relay_dest.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = gw_rx.recv().await {
            let maybe_tx = relay_dest_for_task.borrow().clone();
            if let Some(tx) = maybe_tx {
                if tx.send(msg).await.is_err() {
                    // Connection's gateway receiver was dropped — clear it.
                    *relay_dest_for_task.borrow_mut() = None;
                }
            }
            // If no connection, the message (and its response_tx) is dropped.
            // The caller (session actor) gets a send error which is already
            // handled with `let _ = ...`.
        }
    });

    // Accept new connections in a loop
    while let Some(channels) = connection_rx.recv().await {
        info!("Agent thread: setting up new ACP connection (reconnect)");
        setup_acp_connection(agent.clone(), channels, relay_dest.clone());
    }

    info!("Agent thread: connection channel closed, exiting");
}

/// Set up a new ACP connection for a WebSocket connection, reusing the existing
/// MvpAgent. The relay destination is updated so that session actor notifications
/// flow to the new client.
fn setup_acp_connection(
    agent: Rc<MvpAgent>,
    channels: NewConnectionChannels,
    relay_dest: RelayDest,
) {
    let NewConnectionChannels {
        mut from_ws_rx,
        to_ws_tx,
    } = channels;

    // Create new simplex IO streams for this ACP connection
    let (agent_read_rx, mut agent_read_tx) = simplex(MAX_BUFFER_SIZE);
    let (agent_write_rx, agent_write_tx) = simplex(MAX_BUFFER_SIZE);

    let incoming = agent_read_rx.compat();
    let outgoing = agent_write_tx.compat_write();

    // Create a per-connection gateway channel for the GatewayReceiver.
    // The relay task will forward persistent-channel messages here.
    let (conn_gw_tx, conn_gw_rx) =
        tokio::sync::mpsc::channel::<AcpClientMessage>(WS_GATEWAY_QUEUE_CAPACITY);

    // Point the relay at this new connection's channel
    *relay_dest.borrow_mut() = Some(conn_gw_tx);

    // Create new ACP connection reusing the same MvpAgent (via Rc clone).
    // `Agent` is implemented for `Rc<T: Agent>` so this works.
    let incoming = LineBufferedRead::spawn_local(incoming);
    let (conn, handle_io) = acp::AgentSideConnection::new(agent, outgoing, incoming, |fut| {
        tokio::task::spawn_local(fut);
    });
    tokio::task::spawn_local(
        GatewayReceiver::new_bounded(conn_gw_rx, conn)
            .with_on_meta(xai_file_utils::trace_context::span_from_meta_traceparent)
            .run(),
    );

    // Task: Forward WS messages → agent (incoming ACP bytes)
    tokio::task::spawn_local(async move {
        while let Some(msg) = from_ws_rx.recv().await {
            // Log messages that lack both `id` and `method` — the ACP layer
            // only prints "received message with neither id nor method" without
            // the payload, making debugging impossible.
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg)
                && v.get("id").is_none()
                && v.get("method").is_none()
            {
                warn!(
                    len = msg.len(),
                    "incoming WS message has neither id nor method"
                );
            }
            if agent_read_tx.write_all(msg.as_bytes()).await.is_err() {
                break;
            }
            if agent_read_tx.write_all(b"\n").await.is_err() {
                break;
            }
        }
        // WS disconnected — the simplex writer is dropped, causing `handle_io`
        // to complete. The GatewayReceiver for this connection will also stop.
        // But the MvpAgent and session actors stay alive, ready for the next
        // connection.
    });

    // Task: Forward agent messages → WS (outgoing ACP bytes)
    tokio::task::spawn_local(async move {
        let mut reader = BufReader::new(agent_write_rx);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let msg = line.trim_end_matches(['\r', '\n']);
                    if !msg.is_empty()
                        && enqueue_outbound_with_backpressure(&to_ws_tx, msg.to_string())
                            .await
                            .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Run the ACP IO handler — fire-and-forget since we don't block the
    // connection loop. It completes when the WS disconnects.
    tokio::task::spawn_local(async move {
        let _ = handle_io.await;
        info!("ACP connection IO handler completed");
    });
}

/// Run the agent WebSocket server.
///
/// This starts a WebSocket server that accepts authenticated connections from
/// remote TUI clients. A single agent instance is shared across all connections
/// (persisted across reconnections) so that in-flight session work survives
/// client disconnects.
///
/// # Arguments
/// * `config` - Server configuration (bind address and secret)
/// * `agent_config` - Agent configuration to use for each connection
///
/// # Example
/// ```ignore
/// let server_config = ServerConfig {
///     bind_addr: "0.0.0.0:9000".parse().unwrap(),
///     secret: std::env::var("GROK_AGENT_SECRET")
///         .expect("set a cryptographically random 256-bit token"),
///     allow_remote: true,
/// };
/// run_agent_server(server_config, agent_config).await?;
/// ```
fn validate_server_config(config: &ServerConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        !config.secret.is_empty(),
        "agent server requires a non-empty authentication secret"
    );
    anyhow::ensure!(
        config.bind_addr.ip().is_loopback() || config.allow_remote,
        "non-loopback agent server binding requires explicit remote acknowledgement"
    );
    anyhow::ensure!(
        config.bind_addr.ip().is_loopback() || config.secret.len() >= MIN_REMOTE_SECRET_BYTES,
        "non-loopback agent server requires an explicit secret of at least {MIN_REMOTE_SECRET_BYTES} bytes"
    );
    Ok(())
}

pub async fn run_agent_server(
    config: ServerConfig,
    agent_config: AgentConfig,
) -> anyhow::Result<()> {
    // This synchronous validation must remain before state construction, task
    // creation, and listener binding so rejected configurations have no side
    // effects.
    validate_server_config(&config)?;
    let allow_legacy_query_auth = config.bind_addr.ip().is_loopback();
    let secret_digest = digest_secret(&config.secret);

    let state = Arc::new(ServerState {
        agent_config,
        secret_digest,
        allow_legacy_query_auth,
        last_auth_failure_warning: AtomicU64::new(0),
        agent_conn_tx: tokio::sync::Mutex::new(None),
    });

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .with_state(state);

    let listener = TcpListener::bind(config.bind_addr).await?;
    info!("Agent server listening on ws://{}/ws", config.bind_addr);
    info!(
        "Clients should connect with: --remote ws://{}:{}/ws --secret <token>",
        config.bind_addr.ip(),
        config.bind_addr.port()
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderValue, header::AUTHORIZATION};

    const SECRET: &str = "GB002-server-auth-Q7w5E3r1T9y7Z6x4C2v8";

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers
    }

    fn query_with(key: &str) -> WsQueryParams {
        WsQueryParams {
            server_key: Some(key.to_owned()),
        }
    }

    fn validate_test_auth(
        headers: &HeaderMap,
        query: &WsQueryParams,
        allow_legacy_query_auth: bool,
    ) -> AuthDecision {
        validate_auth(
            headers,
            query,
            &digest_secret(SECRET),
            allow_legacy_query_auth,
        )
    }

    #[test]
    fn issue8_agent_serve_security_auth_debug_is_presence_only() {
        let config = ServerConfig {
            bind_addr: "127.0.0.1:9000".parse().unwrap(),
            secret: SECRET.to_owned(),
            allow_remote: false,
        };
        let query = WsQueryParams {
            server_key: Some(SECRET.to_owned()),
        };
        let rendered = format!("{config:?}\n{query:?}");
        assert!(rendered.contains("secret_present: true"));
        assert!(rendered.contains("server_key_present: true"));
        assert!(rendered.contains("allow_remote: false"));
        assert!(!rendered.contains(SECRET));
        for window in SECRET.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).unwrap();
            assert!(!rendered.contains(window), "leaked secret window: {window}");
        }
    }

    #[test]
    fn issue8_agent_serve_security_valid_bearer_authenticates() {
        assert_eq!(
            validate_test_auth(&bearer_headers(SECRET), &WsQueryParams::default(), false),
            AuthDecision::Authenticated(AuthSource::Bearer)
        );
    }

    #[test]
    fn issue8_agent_serve_security_bearer_mismatches_are_rejected() {
        let candidates = [
            "XB002-server-auth-Q7w5E3r1T9y7",
            "GB002-server-auth-X7w5E3r1T9y7",
            "GB002-server-auth-Q7w5E3r1T9yX",
            "GB002-server-auth-Q7w5E3r1T9y",
            "GB002-server-auth-Q7w5E3r1T9y7-extra",
        ];
        for candidate in candidates {
            assert_eq!(
                validate_test_auth(&bearer_headers(candidate), &WsQueryParams::default(), false),
                AuthDecision::Rejected,
                "unexpected match for {candidate}"
            );
        }
    }

    #[test]
    fn issue8_agent_serve_security_authorization_header_never_downgrades_to_query_auth() {
        let query = query_with(SECRET);

        let malformed_values = [
            "Basic credentials",
            "bearer not-case-insensitive",
            "Bearer ",
        ];
        for value in malformed_values {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            assert_eq!(
                validate_test_auth(&headers, &query, true),
                AuthDecision::Rejected
            );
        }

        let mut invalid_utf8 = HeaderMap::new();
        invalid_utf8.insert(
            AUTHORIZATION,
            HeaderValue::from_bytes(b"Bearer \xff").unwrap(),
        );
        assert_eq!(
            validate_test_auth(&invalid_utf8, &query, true),
            AuthDecision::Rejected
        );

        assert_eq!(
            validate_test_auth(&bearer_headers("wrong"), &query, true),
            AuthDecision::Rejected
        );

        let mut multiple = bearer_headers(SECRET);
        multiple.append(AUTHORIZATION, HeaderValue::from_static("Bearer wrong"));
        assert_eq!(
            validate_test_auth(&multiple, &query, true),
            AuthDecision::Rejected
        );
    }

    #[test]
    fn issue8_agent_serve_security_query_auth_is_loopback_compatibility_only() {
        assert_eq!(
            validate_test_auth(&HeaderMap::new(), &query_with(SECRET), true),
            AuthDecision::Authenticated(AuthSource::LegacyQuery)
        );
        assert_eq!(
            validate_test_auth(&HeaderMap::new(), &query_with(SECRET), false),
            AuthDecision::Rejected
        );
        assert_eq!(
            validate_test_auth(&HeaderMap::new(), &query_with("wrong"), true),
            AuthDecision::Rejected
        );
        assert_eq!(
            validate_test_auth(&HeaderMap::new(), &WsQueryParams::default(), true),
            AuthDecision::Rejected
        );
    }

    #[test]
    fn issue8_agent_serve_security_server_config_preflight_is_fail_closed() {
        for address in ["127.0.0.1:2419", "127.31.22.9:2419", "[::1]:2419"] {
            let config = ServerConfig {
                bind_addr: address.parse().unwrap(),
                secret: SECRET.to_owned(),
                allow_remote: false,
            };
            assert!(validate_server_config(&config).is_ok(), "{address}");
        }

        for address in [
            "0.0.0.0:2419",
            "192.168.1.4:2419",
            "10.0.0.8:2419",
            "203.0.113.10:2419",
            "[::]:2419",
        ] {
            let mut config = ServerConfig {
                bind_addr: address.parse().unwrap(),
                secret: SECRET.to_owned(),
                allow_remote: false,
            };
            assert!(validate_server_config(&config).is_err(), "{address}");
            config.allow_remote = true;
            assert!(validate_server_config(&config).is_ok(), "{address}");
        }

        let empty_secret = ServerConfig {
            bind_addr: "127.0.0.1:2419".parse().unwrap(),
            secret: String::new(),
            allow_remote: false,
        };
        assert!(validate_server_config(&empty_secret).is_err());

        let weak_remote_secret = ServerConfig {
            bind_addr: "0.0.0.0:2419".parse().unwrap(),
            secret: "too-short".to_owned(),
            allow_remote: true,
        };
        assert_eq!(
            validate_server_config(&weak_remote_secret)
                .expect_err("weak remote secret must fail")
                .to_string(),
            format!(
                "non-loopback agent server requires an explicit secret of at least {MIN_REMOTE_SECRET_BYTES} bytes"
            )
        );
    }

    #[test]
    fn issue8_agent_serve_security_auth_failure_warnings_are_rate_limited() {
        let last_warning = AtomicU64::new(0);
        assert!(should_warn_auth_failure(&last_warning, 100));
        assert!(!should_warn_auth_failure(&last_warning, 101));
        assert!(!should_warn_auth_failure(&last_warning, 129));
        assert!(should_warn_auth_failure(&last_warning, 130));
        assert_eq!(last_warning.load(Ordering::Relaxed), 130);
    }

    async fn unused_loopback_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve loopback port");
        let addr = listener.local_addr().expect("reserved loopback address");
        drop(listener);
        addr
    }

    async fn wait_for_loopback_listener(addr: SocketAddr) {
        for _ in 0..100 {
            if let Ok(stream) = tokio::net::TcpStream::connect(addr).await {
                drop(stream);
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("agent server did not listen on {addr}");
    }

    fn assert_unauthorized_handshake(error: tokio_tungstenite::tungstenite::Error) {
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("expected an HTTP authentication rejection, got {error:?}");
        };
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = response
            .body()
            .as_deref()
            .map(String::from_utf8_lossy)
            .unwrap_or_default();
        assert_eq!(body, "Invalid or missing authorization token");
        assert!(!body.contains(SECRET));
    }

    #[tokio::test]
    async fn issue8_agent_serve_security_loopback_server_startup_and_authentication_smoke() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

        let addr = unused_loopback_addr().await;
        let server = tokio::spawn(run_agent_server(
            ServerConfig {
                bind_addr: addr,
                secret: SECRET.to_owned(),
                allow_remote: false,
            },
            AgentConfig::default(),
        ));
        wait_for_loopback_listener(addr).await;

        let wrong_query =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws?server-key=wrong"))
                .await
                .expect_err("wrong query secret must be rejected");
        assert_unauthorized_handshake(wrong_query);

        let mut no_downgrade = format!("ws://{addr}/ws?server-key={SECRET}")
            .into_client_request()
            .expect("websocket request");
        no_downgrade.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_static("Basic invalid-credentials"),
        );
        let no_downgrade = tokio_tungstenite::connect_async(no_downgrade)
            .await
            .expect_err("malformed Authorization must not downgrade to query auth");
        assert_unauthorized_handshake(no_downgrade);

        let mut bearer = format!("ws://{addr}/ws")
            .into_client_request()
            .expect("websocket request");
        bearer.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {SECRET}")).unwrap(),
        );
        let (mut bearer_ws, bearer_response) = tokio_tungstenite::connect_async(bearer)
            .await
            .expect("Bearer authentication should upgrade");
        assert_eq!(bearer_response.status(), StatusCode::SWITCHING_PROTOCOLS);
        bearer_ws.close(None).await.expect("close Bearer websocket");

        let (mut query_ws, query_response) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/ws?server-key={SECRET}"))
                .await
                .expect("loopback legacy query authentication should upgrade");
        assert_eq!(query_response.status(), StatusCode::SWITCHING_PROTOCOLS);
        query_ws.close(None).await.expect("close query websocket");

        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn issue8_agent_serve_security_remote_rejection_precedes_listener_binding() {
        let addr = unused_loopback_addr().await;
        let remote_addr = SocketAddr::from(([0, 0, 0, 0], addr.port()));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            run_agent_server(
                ServerConfig {
                    bind_addr: remote_addr,
                    secret: SECRET.to_owned(),
                    allow_remote: false,
                },
                AgentConfig::default(),
            ),
        )
        .await
        .expect("remote preflight must return before listening")
        .expect_err("remote bind without acknowledgement must fail");
        assert_eq!(
            result.to_string(),
            "non-loopback agent server binding requires explicit remote acknowledgement"
        );

        let listener = TcpListener::bind(addr)
            .await
            .expect("rejected server must not consume the port");
        drop(listener);
    }

    #[test]
    fn websocket_close_diagnostic_omits_peer_reason_fragments() {
        const SENTINEL: &str = "ZXQ91vLmN7pR4tK8sW2cY6hF0aD3uB5e";
        let (present, len) = close_reason_diagnostic(SENTINEL);
        let rendered = format!("reason_present={present};reason_len={len}");
        assert!(present);
        assert!(!rendered.contains(SENTINEL));
        for window in SENTINEL.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII sentinel");
            assert!(!rendered.contains(fragment), "leaked fragment {fragment}");
        }
    }

    async fn connect_authorized_ws(
        addr: SocketAddr,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

        let mut request = format!("ws://{addr}/ws")
            .into_client_request()
            .expect("websocket request");
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {SECRET}")).unwrap(),
        );
        let (ws, response) = tokio_tungstenite::connect_async(request)
            .await
            .expect("authorized websocket upgrade");
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        ws
    }

    async fn spawn_loopback_agent_server()
    -> (SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>) {
        let addr = unused_loopback_addr().await;
        let server = tokio::spawn(run_agent_server(
            ServerConfig {
                bind_addr: addr,
                secret: SECRET.to_owned(),
                allow_remote: false,
            },
            AgentConfig::default(),
        ));
        wait_for_loopback_listener(addr).await;
        (addr, server)
    }

    async fn next_ws_close_code(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> u16 {
        use futures::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for websocket close frame"
            );
            let msg = tokio::time::timeout(remaining, ws.next())
                .await
                .expect("close wait")
                .expect("websocket stream ended without a close frame")
                .expect("websocket read");
            match msg {
                TungsteniteMessage::Close(frame) => {
                    return u16::from(frame.expect("close frame must carry a code").code);
                }
                TungsteniteMessage::Ping(payload) => {
                    let _ = ws.send(TungsteniteMessage::Pong(payload)).await;
                }
                TungsteniteMessage::Pong(_) => {}
                other => {
                    // Drain any agent chatter until the policy close arrives.
                    let _ = other;
                }
            }
        }
    }

    fn test_session_notification(marker: &str) -> acp::SessionNotification {
        acp::SessionNotification::new(
            acp::SessionId::new("issue41-session"),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(marker),
            ))),
        )
    }

    async fn assert_oversized_text_payload_closes_with_1009(payload: String) {
        use futures::SinkExt as _;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

        let (tx, _rx) = mpsc::channel::<String>(WS_BRIDGE_QUEUE_CAPACITY);
        let rejected = admit_inbound_ws_payload(&tx, &payload).expect_err("oversize must reject");
        assert_eq!(rejected.expect("oversize close").code, close_code::SIZE);

        let (addr, server) = spawn_loopback_agent_server().await;
        let mut ws = connect_authorized_ws(addr).await;

        assert!(payload.len() > MAX_INBOUND_WS_MESSAGE_BYTES);
        assert!(
            payload.len() < MAX_BUFFER_SIZE,
            "fixture must pass tungstenite's hard ceiling so only the app gate fires"
        );

        ws.send(TungsteniteMessage::Text(payload.into()))
            .await
            .expect("send oversized frame");

        let code = next_ws_close_code(&mut ws).await;
        assert_eq!(code, close_code::SIZE);

        server.abort();
        let _ = server.await;
    }

    async fn assert_oversized_binary_payload_closes_with_1009(payload: Vec<u8>) {
        use futures::SinkExt as _;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

        let (tx, _rx) = mpsc::channel::<String>(WS_BRIDGE_QUEUE_CAPACITY);
        let rejected =
            admit_inbound_ws_binary_payload(&tx, &payload).expect_err("oversize must reject");
        assert_eq!(rejected.expect("oversize close").code, close_code::SIZE);

        let (addr, server) = spawn_loopback_agent_server().await;
        let mut ws = connect_authorized_ws(addr).await;

        assert!(payload.len() > MAX_INBOUND_WS_MESSAGE_BYTES);
        assert!(
            payload.len() < MAX_BUFFER_SIZE,
            "fixture must pass tungstenite's hard ceiling so only the app gate fires"
        );

        ws.send(TungsteniteMessage::Binary(payload.into()))
            .await
            .expect("send oversized binary frame");

        let code = next_ws_close_code(&mut ws).await;
        assert_eq!(code, close_code::SIZE);

        server.abort();
        let _ = server.await;
    }

    /// Drives [`admit_inbound_ws_payload`]: fills the per-connection queue to
    /// capacity, then proves the next admit returns close 1013.
    ///
    /// Mutation that must fail this test: in `admit_inbound_ws_payload`, treat
    /// `TrySendError::Full` as success (remove the overload branch).
    #[test]
    fn issue41_bound_ws_queues_flood_closes_with_1013() {
        let (tx, _rx) = mpsc::channel::<String>(WS_BRIDGE_QUEUE_CAPACITY);
        for i in 0..WS_BRIDGE_QUEUE_CAPACITY {
            let payload = format!(r#"{{"jsonrpc":"2.0","id":{i},"method":"x"}}"#);
            assert_eq!(
                admit_inbound_ws_payload(&tx, &payload),
                Ok(true),
                "pre-saturation enqueue {i}"
            );
        }
        let rejected = admit_inbound_ws_payload(&tx, r#"{"jsonrpc":"2.0","id":999,"method":"x"}"#)
            .expect_err("saturated queue must reject");
        let frame = rejected.expect("overload must carry a close frame");
        assert_eq!(frame.code, close_code::AGAIN);
        assert!(frame.reason.is_empty());
    }

    /// Drives bounded persistent gateway admission: once the relay queue is
    /// full, fire-and-forget notifications are rejected instead of enqueuing
    /// without bound.
    ///
    /// Mutation that must fail this test: build the gateway sender with
    /// `GatewaySender::new` (unbounded) instead of `GatewaySender::new_bounded`.
    #[test]
    fn issue41_bound_ws_queues_gateway_queue_rejects_when_full() {
        let (gw_tx, _gw_rx) = mpsc::channel::<AcpClientMessage>(2);
        let gateway = GatewaySender::new_bounded(gw_tx);

        assert!(gateway.forward_fire_and_forget(test_session_notification("delta-0")));
        assert!(gateway.forward_fire_and_forget(test_session_notification("delta-1")));
        assert!(!gateway.forward_fire_and_forget(test_session_notification("delta-2")));
    }

    /// Drives the raw-length gate for text frames that become `"ping"` only
    /// after trimming CR/LF.
    ///
    /// Mutation that must fail this test: move the size check below
    /// `trim_end_matches` in `admit_inbound_ws_payload`.
    #[tokio::test]
    async fn issue41_bound_ws_queues_oversized_trailing_ping_closes_with_1009() {
        let payload = format!("ping{}", "\n".repeat(MAX_INBOUND_WS_MESSAGE_BYTES));
        assert_oversized_text_payload_closes_with_1009(payload).await;
    }

    /// Drives the raw-length gate for text frames that become empty after
    /// trimming CR/LF.
    ///
    /// Mutation that must fail this test: move the size check below
    /// `trim_end_matches` in `admit_inbound_ws_payload`.
    #[tokio::test]
    async fn issue41_bound_ws_queues_oversized_empty_after_trim_closes_with_1009() {
        let payload = "\n".repeat(MAX_INBOUND_WS_MESSAGE_BYTES + 1);
        assert_oversized_text_payload_closes_with_1009(payload).await;
    }

    /// Drives the raw-length gate for oversized non-UTF-8 binary frames.
    ///
    /// Mutation that must fail this test: in `handle_connection`, restore the
    /// early `from_utf8` `continue` before `admit_inbound_ws_binary_payload`.
    #[tokio::test]
    async fn issue41_bound_ws_queues_oversized_non_utf8_binary_closes_with_1009() {
        let payload = vec![0xFF; MAX_INBOUND_WS_MESSAGE_BYTES + 1];
        assert_oversized_binary_payload_closes_with_1009(payload).await;
    }

    /// Drives [`admit_inbound_ws_payload`] size gate and the live WS close path
    /// in `handle_connection` (policy_close → write task).
    ///
    /// Mutation that must fail this test: delete the
    /// `payload.len() > MAX_INBOUND_WS_MESSAGE_BYTES` check in
    /// `admit_inbound_ws_payload`.
    #[tokio::test]
    async fn issue41_bound_ws_queues_oversized_frame_closes_with_1009() {
        use futures::SinkExt as _;
        use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

        // Unit-level gate first (fast failure under mutation).
        let (tx, _rx) = mpsc::channel::<String>(WS_BRIDGE_QUEUE_CAPACITY);
        let rejected = admit_inbound_ws_payload(&tx, &"y".repeat(MAX_INBOUND_WS_MESSAGE_BYTES + 1))
            .expect_err("oversize must reject");
        assert_eq!(rejected.expect("oversize close").code, close_code::SIZE);

        let (addr, server) = spawn_loopback_agent_server().await;
        let mut ws = connect_authorized_ws(addr).await;

        let oversized = "x".repeat(MAX_INBOUND_WS_MESSAGE_BYTES + 1);
        assert!(oversized.len() > MAX_INBOUND_WS_MESSAGE_BYTES);
        assert!(
            oversized.len() < MAX_BUFFER_SIZE,
            "fixture must pass tungstenite's hard ceiling so only the app gate fires"
        );
        ws.send(TungsteniteMessage::Text(oversized.into()))
            .await
            .expect("send oversized frame");

        let code = next_ws_close_code(&mut ws).await;
        assert_eq!(code, close_code::SIZE);

        server.abort();
        let _ = server.await;
    }

    /// Drives [`enqueue_outbound_with_backpressure`]: once the per-connection
    /// outbound queue is full, the next enqueue waits instead of buffering
    /// without bound — so a legitimate slow consumer on a large streamed
    /// response stalls the producer without disconnecting.
    ///
    /// Mutation that must fail this test: replace the body with a non-blocking
    /// enqueue that ignores capacity (e.g. always `Ok(())` without awaiting a
    /// bounded `send`).
    #[tokio::test]
    async fn issue41_bound_ws_queues_slow_consumer_applies_outbound_backpressure() {
        let (tx, mut rx) = mpsc::channel::<String>(WS_BRIDGE_QUEUE_CAPACITY);
        for i in 0..WS_BRIDGE_QUEUE_CAPACITY {
            enqueue_outbound_with_backpressure(&tx, format!("msg-{i}"))
                .await
                .expect("fill outbound queue");
        }

        let mut blocked = std::pin::pin!(enqueue_outbound_with_backpressure(
            &tx,
            "blocked".to_owned()
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut blocked)
                .await
                .is_err(),
            "outbound enqueue must apply backpressure when the per-connection queue is full"
        );

        assert_eq!(rx.recv().await.as_deref(), Some("msg-0"));
        tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .expect("backpressure must release after capacity frees")
            .expect("sender still open");
    }

    /// Drives the authenticated WS path through ACP `initialize` to prove
    /// normal request/response still works under the bounded queues.
    ///
    /// Mutation that must fail this test: break `enqueue_outbound_with_backpressure`
    /// so outbound frames are dropped (`return Ok(())` without sending).
    #[tokio::test]
    async fn issue41_bound_ws_queues_normal_flow_initialize_round_trip() {
        use futures::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

        let (addr, server) = spawn_loopback_agent_server().await;
        let mut ws = connect_authorized_ws(addr).await;

        let init = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"#,
            r#""protocolVersion":1,"#,
            r#""clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"#,
            r#""_meta":{"startupHints":{"nonInteractive":true,"skipGitStatus":true,"skipProjectLayout":true},"#,
            r#""clientType":"issue41-test","clientVersion":"0.0.0-test"}}}"#
        );
        ws.send(TungsteniteMessage::Text(init.into()))
            .await
            .expect("send initialize");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut saw_initialize_result = false;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let msg = tokio::time::timeout(remaining, ws.next())
                .await
                .expect("initialize response wait")
                .expect("websocket ended before initialize response")
                .expect("websocket read");
            match msg {
                TungsteniteMessage::Text(text) => {
                    let value: serde_json::Value =
                        serde_json::from_str(text.as_ref()).expect("acp json");
                    if value.get("id") == Some(&serde_json::json!(1)) {
                        assert!(
                            value.get("result").is_some(),
                            "initialize must return a result, got {value}"
                        );
                        saw_initialize_result = true;
                        break;
                    }
                }
                TungsteniteMessage::Ping(payload) => {
                    let _ = ws.send(TungsteniteMessage::Pong(payload)).await;
                }
                TungsteniteMessage::Close(frame) => {
                    panic!("connection closed before initialize response: {frame:?}");
                }
                _ => {}
            }
        }
        assert!(
            saw_initialize_result,
            "bounded queues must still deliver ACP initialize responses"
        );

        let _ = ws.close(None).await;
        server.abort();
        let _ = server.await;
    }
}
