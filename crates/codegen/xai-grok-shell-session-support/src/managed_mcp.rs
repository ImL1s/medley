//! Managed MCP gateway catalog and tool calls via the Grok API.
//!
//! Catalog: `GET /v1/mcp/tools/list` → `managed_gateway:*` rows.
//! Call: `POST /v1/mcp/tools/call`.
//!
//! Config-file/plugin merge (which reads shell's config system) lives
//! in shell's `session::managed_mcp`, which re-exports everything here.

use std::collections::{HashMap, HashSet};
use chrono::{DateTime, Utc};
use std::sync::Arc;

/// Agent-level cache for managed MCP gateway tool catalogs.

pub enum ManagedMcpCache {
    NotFetched,
    Fetching,
    Ready(Vec<ManagedMcpConfig>),
}

pub enum GatewayToolCatalogCache {
    NotFetched,
    /// Fetch in progress for the recorded gateway tool epoch.
    Fetching(u64),
    /// May be empty if the user has no gateway-exposed tools.
    Ready(GatewayToolCatalog),
}

pub struct ManagedMcpState {
    pub gateway_tools_active: bool,
    pub gateway_tool_epoch: u64,
    /// True from the start of an explicit gateway refresh until a fresh
    /// catalog commits. Session snapshots must not admit a cached catalog while
    /// this fence is raised, including snapshots captured before it was raised.
    pub gateway_refresh_in_progress: bool,
    pub gateway_tool_cache: GatewayToolCatalogCache,
    pub gateway_tool_fetch_notify: Arc<tokio::sync::Notify>,
    /// Retained across gateway disable/cache invalidation so the on-disk
    /// MCP descriptor mirror can remove stale gateway connector directories when
    /// the current catalog is empty or absent.
    pub gateway_tool_connectors_seen: HashSet<String>,
}

impl Default for ManagedMcpState {
    fn default() -> Self {
        Self {
            gateway_tools_active: false,
            gateway_tool_epoch: 0,
            gateway_refresh_in_progress: false,
            gateway_tool_cache: GatewayToolCatalogCache::NotFetched,
            gateway_tool_fetch_notify: Arc::new(tokio::sync::Notify::new()),
            gateway_tool_connectors_seen: HashSet::new(),
        }
    }
}

impl ManagedMcpState {
    pub fn enable_gateway_tools(&mut self) -> u64 {
        if !self.gateway_tools_active {
            self.gateway_tool_epoch = self.gateway_tool_epoch.wrapping_add(1);
        }
        self.gateway_tools_active = true;
        self.gateway_tool_epoch
    }

    /// Fence session admission before an explicit gateway catalog refresh.
    /// Rotating the epoch also prevents an older in-flight fetch from clearing
    /// the fence by committing after the refresh began.
    pub fn begin_gateway_tool_refresh(&mut self) -> u64 {
        self.gateway_refresh_in_progress = true;
        self.gateway_tool_epoch = self.gateway_tool_epoch.wrapping_add(1);
        self.gateway_tool_fetch_notify.notify_waiters();
        self.gateway_tool_epoch
    }

    pub fn start_gateway_tool_fetch(&mut self) -> Option<u64> {
        if !self.gateway_tools_active {
            return None;
        }
        self.gateway_tool_cache = GatewayToolCatalogCache::Fetching(self.gateway_tool_epoch);
        Some(self.gateway_tool_epoch)
    }

    pub fn complete_gateway_tool_fetch(
        &mut self,
        epoch: u64,
        mut catalog: GatewayToolCatalog,
    ) -> bool {
        if !self.gateway_tools_active || self.gateway_tool_epoch != epoch {
            self.gateway_tool_fetch_notify.notify_waiters();
            return false;
        }
        let rejected_count = catalog.retain_unambiguous_tools();
        if rejected_count > 0 {
            tracing::warn!(
                rejected_count,
                "Rejected ambiguous managed MCP gateway tool identities"
            );
        }
        self.gateway_tool_connectors_seen
            .extend(catalog.tools.iter().map(|tool| tool.connector_id.clone()));
        self.gateway_tool_cache = GatewayToolCatalogCache::Ready(catalog);
        self.gateway_refresh_in_progress = false;
        self.gateway_tool_fetch_notify.notify_waiters();
        true
    }

    pub fn fail_gateway_tool_fetch(&mut self, epoch: u64) {
        if self.gateway_tools_active
            && self.gateway_tool_epoch == epoch
            && matches!(self.gateway_tool_cache, GatewayToolCatalogCache::Fetching(fetch_epoch) if fetch_epoch == epoch)
        {
            self.gateway_tool_cache = GatewayToolCatalogCache::NotFetched;
        }
        self.gateway_tool_fetch_notify.notify_waiters();
    }

    /// Recover an abandoned gateway fetch (e.g. task cancellation) so waiters
    /// can retry instead of hanging behind a stale `Fetching` marker.
    pub fn abort_gateway_tool_fetch(&mut self) {
        if let GatewayToolCatalogCache::Fetching(epoch) = &self.gateway_tool_cache {
            self.fail_gateway_tool_fetch(*epoch);
            return;
        }
        self.gateway_tool_fetch_notify.notify_waiters();
    }

    pub fn disable_gateway_tools(&mut self) {
        self.gateway_tools_active = false;
        self.gateway_tool_epoch = self.gateway_tool_epoch.wrapping_add(1);
        self.gateway_refresh_in_progress = false;
        self.gateway_tool_cache = GatewayToolCatalogCache::NotFetched;
        self.gateway_tool_fetch_notify.notify_waiters();
    }
}

pub type ManagedMcpStateHandle = Arc<tokio::sync::Mutex<ManagedMcpState>>;

#[derive(Clone, serde::Deserialize)]
pub struct ManagedMcpConfig {
    /// Human-readable connector name (e.g. "Slack", "Linear").
    #[serde(default)]
    pub name: String,
    pub endpoint: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub token_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub scope_id: Option<String>,
    #[serde(default)]
    pub scope_name: Option<String>,
}

impl std::fmt::Debug for ManagedMcpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedMcpConfig")
            .field("name_present", &!self.name.is_empty())
            .field("endpoint_present", &!self.endpoint.is_empty())
            .field("header_count", &self.headers.len())
            .field("token_expiry_present", &self.token_expires_at.is_some())
            .field("scope_present", &self.scope.is_some())
            .field("scope_id_present", &self.scope_id.is_some())
            .field("scope_name_present", &self.scope_name.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct McpConfigsResponse {
    mcp_servers: Vec<ManagedMcpConfig>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GatewayToolCallRequest {
    pub call_id: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GatewayToolCallResponse {
    pub result: serde_json::Value,
    #[serde(default)]
    pub connectors_needing_reauth: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct GatewayToolCatalog {
    #[serde(default)]
    pub tools: Vec<GatewayTool>,
    #[serde(default)]
    pub total_tools: u32,
    #[serde(default)]
    pub connectors_needing_reauth: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct GatewayTool {
    pub connector_id: String,
    pub connector_name: String,
    pub tool_id: String,
    pub tool_name: String,
    pub call_id: String,
    pub description: String,
    pub json_schema: serde_json::Value,
}

impl GatewayTool {
    pub fn qualified_name(&self) -> String {
        format!("{}__{}", self.connector_id, self.tool_id)
    }

    pub fn validated_qualified_name(&self) -> Option<String> {
        fn valid_component(value: &str) -> bool {
            !value.is_empty()
                && value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
                && !value.contains("__")
        }

        if !valid_component(&self.connector_id)
            || !valid_component(&self.tool_id)
            || self.call_id.is_empty()
            || self.call_id.trim() != self.call_id
            || self.call_id.chars().any(char::is_control)
        {
            return None;
        }
        Some(self.qualified_name())
    }
}

impl GatewayToolCatalog {
    /// Remove every invalid or ambiguous identity from a fetched catalog.
    /// Duplicate qualified names and duplicate backend call IDs are rejected
    /// as a group, so input ordering can never select a privileged winner.
    pub fn retain_unambiguous_tools(&mut self) -> usize {
        let before = self.tools.len();
        let mut name_counts = HashMap::<String, usize>::new();
        let mut call_counts = HashMap::<String, usize>::new();
        for tool in &self.tools {
            if let Some(name) = tool.validated_qualified_name() {
                *name_counts.entry(name).or_default() += 1;
                *call_counts.entry(tool.call_id.clone()).or_default() += 1;
            }
        }
        self.tools.retain(|tool| {
            let Some(name) = tool.validated_qualified_name() else {
                return false;
            };
            name_counts.get(&name) == Some(&1) && call_counts.get(&tool.call_id) == Some(&1)
        });
        before - self.tools.len()
    }
}

/// Why a managed-MCP gateway fetch failed. Distinguishes "fetch failed" from
/// the legitimate "fetched, zero connectors" (`Ok` with an empty catalog) so
/// the agent cache never commits a transient failure as a permanent empty
/// catalog.
#[derive(Debug, thiserror::Error)]
pub enum ManagedMcpFetchError {
    #[error("HTTP {status}")]
    Status {
        status: reqwest::StatusCode,
        gateway_code: Option<ManagedMcpGatewayErrorCode>,
    },
    #[error("transport failure: {kind}")]
    Transport { kind: ManagedMcpTransportKind },
    #[error("invalid response")]
    InvalidResponse,
    /// No usable auth token at fetch time.
    #[error("no auth token available")]
    NoAuth,
}

/// Closed, credential-free classification of an allowlisted managed-MCP
/// gateway error code. Provider-controlled error text never crosses the HTTP
/// boundary; callers can still give the model a fixed recovery hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedMcpGatewayErrorCode {
    InvalidArguments,
    AuthenticationRequired,
    PermissionDenied,
    RateLimited,
    ConnectorReauthorizationRequired,
}

impl ManagedMcpGatewayErrorCode {
    fn from_wire_code(code: &str) -> Option<Self> {
        match code.trim() {
            "Client specified an invalid argument" | "INVALID_ARGUMENT" | "invalid_argument" => {
                Some(Self::InvalidArguments)
            }
            "UNAUTHENTICATED" | "AUTHENTICATION_REQUIRED" | "authentication_required" => {
                Some(Self::AuthenticationRequired)
            }
            "PERMISSION_DENIED" | "permission_denied" => Some(Self::PermissionDenied),
            "RESOURCE_EXHAUSTED" | "RATE_LIMITED" | "rate_limited" => Some(Self::RateLimited),
            "CONNECTOR_REAUTHORIZATION_REQUIRED"
            | "connector_reauthorization_required"
            | "REAUTHORIZATION_REQUIRED" => Some(Self::ConnectorReauthorizationRequired),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::AuthenticationRequired => "authentication_required",
            Self::PermissionDenied => "permission_denied",
            Self::RateLimited => "rate_limited",
            Self::ConnectorReauthorizationRequired => "connector_reauthorization_required",
        }
    }

    pub fn recovery_hint(self) -> &'static str {
        match self {
            Self::InvalidArguments => {
                "invalid arguments; review the connector tool schema and required fields"
            }
            Self::AuthenticationRequired => {
                "authentication required; reconnect the managed MCP connector"
            }
            Self::PermissionDenied => "permission denied; verify the connector permissions",
            Self::RateLimited => "rate limited; retry the connector tool later",
            Self::ConnectorReauthorizationRequired => {
                "connector reauthorization required; reconnect the managed MCP connector"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedMcpTransportKind {
    Timeout,
    Connect,
    Request,
}

impl ManagedMcpTransportKind {
    fn from_reqwest_error(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else if error.is_connect() {
            Self::Connect
        } else {
            Self::Request
        }
    }
}

impl std::fmt::Display for ManagedMcpTransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::Request => "request",
        })
    }
}

/// Fetch managed MCP configs from cli-chat-proxy (`GET /v1/mcp/configs`).
///
/// `Ok(vec![])` means the server answered and the user genuinely has no
/// managed connectors. `Err(_)` means we don't know (HTTP error, transport
/// failure, parse error) — callers must NOT cache the result as empty.
pub async fn get_authenticated_json<T: serde::de::DeserializeOwned>(
    url: &str,
    auth_key: &str,
    unavailable_message: &'static str,
    fetch_failed_message: &'static str,
    parse_error_message: &'static str,
) -> Result<T, ManagedMcpFetchError> {
    let resp = match xai_grok_http::shared_client()
        .get(url)
        .timeout(std::time::Duration::from_secs(10))
        .header("Authorization", format!("Bearer {auth_key}"))
        .header("X-XAI-Token-Auth", "xai-grok-cli")
        .header("x-grok-client-version", xai_grok_version::VERSION)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let status = r.status();
            tracing::warn!(status = %status, "{}", unavailable_message);
            return Err(ManagedMcpFetchError::Status {
                status,
                gateway_code: None,
            });
        }
        Err(e) => {
            tracing::warn!(error = %e, "{}", fetch_failed_message);
            return Err(ManagedMcpFetchError::Transport {
                kind: ManagedMcpTransportKind::from_reqwest_error(&e)
            });
        }
    };

    match resp.json::<T>().await {
        Ok(value) => Ok(value),
        Err(e) => {
            tracing::debug!(error = %e, "{}", parse_error_message);
            Err(ManagedMcpFetchError::InvalidResponse)
        }
    }
}

// Above the server-side tool-call budget so the client is not the first
// hop to abort a slow tool call.
const GATEWAY_TOOL_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(75);

pub async fn call_gateway_tool(
    proxy_base_url: &str,
    auth_key: &str,
    call_id: &str,
    arguments: serde_json::Value,
) -> Result<GatewayToolCallResponse, ManagedMcpFetchError> {
    let url = format!("{proxy_base_url}/mcp/tools/call");
    let arguments = if arguments.is_null() {
        serde_json::json!({})
    } else {
        arguments
    };
    let request = GatewayToolCallRequest {
        call_id: call_id.to_owned(),
        arguments,
    };

    let resp = match xai_grok_http::shared_client()
        .post(&url)
        .timeout(GATEWAY_TOOL_CALL_TIMEOUT)
        .header("Authorization", format!("Bearer {auth_key}"))
        .header("X-XAI-Token-Auth", "xai-grok-cli")
        .header("x-grok-client-version", xai_grok_version::VERSION)
        .json(&request)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let status = r.status();
            let gateway_code = gateway_error_code(r).await;
            tracing::warn!(
                status = %status,
                gateway_code = gateway_code.map(ManagedMcpGatewayErrorCode::as_str),
                "Managed MCP gateway tool call unavailable: HTTP {status}"
            );
            return Err(ManagedMcpFetchError::Status {
                status,
                gateway_code,
            });
        }
        Err(e) => {
            let kind = ManagedMcpTransportKind::from_reqwest_error(&e);
            tracing::warn!(
                transport_kind = %kind,
                "Managed MCP gateway tool call failed"
            );
            return Err(ManagedMcpFetchError::Transport { kind });
        }
    };

    match resp.json::<GatewayToolCallResponse>().await {
        Ok(response) => Ok(response),
        Err(_) => {
            tracing::debug!(
                failure_kind = "invalid_response",
                "Managed MCP gateway tool call parse error"
            );
            Err(ManagedMcpFetchError::InvalidResponse)
        }
    }
}

const GATEWAY_ERROR_BODY_MAX_BYTES: usize = 16 * 1024;

#[derive(serde::Deserialize)]
struct GatewayErrorEnvelope {
    code: Option<String>,
}

async fn gateway_error_code(mut response: reqwest::Response) -> Option<ManagedMcpGatewayErrorCode> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len().saturating_add(chunk.len()) > GATEWAY_ERROR_BODY_MAX_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }

    let envelope: GatewayErrorEnvelope = serde_json::from_slice(&body).ok()?;
    envelope
        .code
        .as_deref()
        .and_then(ManagedMcpGatewayErrorCode::from_wire_code)
}

/// Fetch the managed MCP gateway tool catalog from the Grok API
/// (`GET /v1/mcp/tools/list`).
///
/// `Ok(catalog)` means the server answered and the catalog contents are
/// authoritative for this fetch, even when empty. `Err(_)` means freshness is
/// unknown and callers must leave any cache retryable rather than committing an
/// empty catalog.
pub async fn fetch_gateway_tool_catalog(
    proxy_base_url: &str,
    auth_key: &str,
) -> Result<GatewayToolCatalog, ManagedMcpFetchError> {
    let url = format!("{proxy_base_url}/mcp/tools/list");

    let catalog: GatewayToolCatalog = get_authenticated_json(
        &url,
        auth_key,
        "Managed MCP gateway tools unavailable",
        "Managed MCP gateway tools fetch failed",
        "Managed MCP gateway tools parse error",
    )
    .await?;
    tracing::info!(
        count = catalog.tools.len(),
        total_tools = catalog.total_tools,
        reauth = catalog.connectors_needing_reauth.len(),
        "Fetched managed MCP gateway tool catalog"
    );
    Ok(catalog)
}

/// Invalidate only the gateway tool catalog so the next gateway-aware caller
/// refetches `/v1/mcp/tools/list`.
pub async fn invalidate_gateway_tool_cache(handle: &ManagedMcpStateHandle) {
    let mut state = handle.lock().await;
    state.gateway_tool_cache = GatewayToolCatalogCache::NotFetched;
}

/// Fetch-or-wait for the managed MCP gateway tool catalog.
///
/// Returns `Some(catalog)` for either a cached catalog or a successful fresh
/// fetch, including a genuine empty catalog. Returns `None` when gateway tools
/// are disabled by the caller, auth is unavailable, or the fetch failed. Failed
/// fetches roll back to `NotFetched`, so a later caller can retry.
pub async fn get_or_fetch_gateway_tool_catalog(
    handle: &ManagedMcpStateHandle,
    proxy_url: &str,
    auth_key: Option<&str>,
) -> Option<GatewayToolCatalog> {
    let fetch_epoch = loop {
        let maybe_notify = {
            let mut state = handle.lock().await;
            if !state.gateway_tools_active {
                return None;
            }
            match &state.gateway_tool_cache {
                GatewayToolCatalogCache::Ready(_) if state.gateway_refresh_in_progress => {
                    return None;
                }
                GatewayToolCatalogCache::Ready(catalog) => return Some(catalog.clone()),
                GatewayToolCatalogCache::Fetching(_) => {
                    Some(state.gateway_tool_fetch_notify.clone().notified_owned())
                }
                GatewayToolCatalogCache::NotFetched => {
                    let epoch = state.start_gateway_tool_fetch()?;
                    break epoch;
                }
            }
        };

        if let Some(notified) = maybe_notify {
            notified.await;
            continue;
        }
    };

    let result = match auth_key {
        Some(key) => fetch_gateway_tool_catalog(proxy_url, key).await,
        None => Err(ManagedMcpFetchError::NoAuth),
    };

    match result {
        Ok(catalog) => {
            let committed = handle
                .lock()
                .await
                .complete_gateway_tool_fetch(fetch_epoch, catalog.clone());
            committed.then_some(catalog)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Managed MCP gateway tool fetch failed; leaving cache unpopulated for retry"
            );
            handle.lock().await.fail_gateway_tool_fetch(fetch_epoch);
            None
        }
    }
}

