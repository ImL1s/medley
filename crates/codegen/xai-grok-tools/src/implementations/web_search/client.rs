use super::types::WebSearchConfig;
use crate::attribution::{SharedAttributionCallback, ToolConsumer};
use crate::types::{ApiCredential, ApiTransportProfile, SharedApiKeyProvider};
use async_openai::types::responses as rs;
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};

const CHATGPT_ACCOUNT_ID: HeaderName = HeaderName::from_static("chatgpt-account-id");
const OPENAI_FEDRAMP: HeaderName = HeaderName::from_static("x-openai-fedramp");
const ORIGINATOR: HeaderName = HeaderName::from_static("originator");
const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Session-wide auth belongs only to xAI-operated endpoints. A custom model's
/// static key is scoped to its configured endpoint and must never be replaced
/// with the current xAI session bearer merely because the caller supplied a
/// default provider.
fn accepts_xai_session_provider(base_url: &str) -> bool {
    let Ok(candidate) = reqwest::Url::parse(base_url) else {
        return false;
    };
    if candidate.scheme() != "https"
        || candidate.port_or_known_default() != Some(443)
        || !candidate.username().is_empty()
        || candidate.password().is_some()
    {
        return false;
    }

    let host = candidate.host_str().unwrap_or_default();
    if host == "x.ai" || host.ends_with(".x.ai") {
        return true;
    }

    let Ok(proxy) = reqwest::Url::parse(xai_grok_env::PROD_CLI_CHAT_PROXY_BASE_URL) else {
        return false;
    };
    let trusted_path = proxy.path();
    let candidate_path = candidate.path();
    candidate.scheme() == proxy.scheme()
        && candidate.host_str() == proxy.host_str()
        && candidate.port_or_known_default() == proxy.port_or_known_default()
        && (candidate_path == trusted_path
            || candidate_path
                .strip_prefix(trusted_path)
                .is_some_and(|suffix| suffix.starts_with('/')))
}

fn strip_codex_routing_headers(headers: &mut HeaderMap) {
    headers.remove(CHATGPT_ACCOUNT_ID);
    headers.remove(OPENAI_FEDRAMP);
    headers.remove(ORIGINATOR);
}

fn should_follow_redirect_with_dynamic_credential(
    _previous: &reqwest::Url,
    _next: &reqwest::Url,
) -> bool {
    false
}

fn grok_build_user_agent() -> Result<HeaderValue, &'static str> {
    HeaderValue::from_str(&format!("xai-grok-build/{}", xai_grok_version::VERSION))
        .map_err(|_| "Grok Build version produced an invalid user agent.")
}

fn normalize_codex_base_url(base_url: &str) -> Result<String, &'static str> {
    let url = reqwest::Url::parse(base_url)
        .map_err(|_| "OpenAI Codex traffic must use https://chatgpt.com/backend-api/codex.")?;
    let is_production = url.scheme() == "https"
        && url.host_str() == Some("chatgpt.com")
        && url.port().is_none()
        && url.path().trim_end_matches('/') == "/backend-api/codex"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        // Keep the production destination byte-for-byte canonical (apart from
        // an optional trailing slash). This also rejects an explicit default
        // port such as `:443`, which `Url` otherwise normalizes away.
        && base_url.trim_end_matches('/') == CODEX_BASE_URL;
    if is_production {
        return Ok(CODEX_BASE_URL.to_string());
    }

    // Tests may point Codex transport at an in-process mock server. This seam
    // is absent from production builds and does not relax the production
    // destination allowlist.
    #[cfg(test)]
    {
        let is_loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"))
            && matches!(url.scheme(), "http" | "https")
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none();
        if is_loopback {
            return Ok(base_url.trim_end_matches('/').to_string());
        }
    }

    Err("OpenAI Codex traffic must use https://chatgpt.com/backend-api/codex.")
}

fn retain_codex_headers(
    headers: &mut HeaderMap,
    authorization: HeaderValue,
    account_id: Option<&str>,
    chatgpt_account_is_fedramp: bool,
) -> Result<(), &'static str> {
    let content_type = headers.get(CONTENT_TYPE).cloned();
    headers.clear();
    if let Some(value) = content_type {
        headers.insert(CONTENT_TYPE, value);
    }
    headers.insert(USER_AGENT, grok_build_user_agent()?);
    headers.insert(AUTHORIZATION, authorization);
    if let Some(account_id) = account_id {
        let value = HeaderValue::from_str(account_id)
            .map_err(|_| "Provider returned an invalid account identifier.")?;
        headers.insert(CHATGPT_ACCOUNT_ID, value);
    }
    if chatgpt_account_is_fedramp {
        headers.insert(OPENAI_FEDRAMP, HeaderValue::from_static("true"));
    }
    headers.insert(ORIGINATOR, HeaderValue::from_static("grok_build"));
    Ok(())
}
/// A minimal, purpose-built HTTP client for calling the Responses API
/// with web search capability.
#[derive(Clone)]
pub struct WebSearchClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key_provider: Option<SharedApiKeyProvider>,
    provider_scoped: bool,
    transport_profile: ApiTransportProfile,
    /// Optional 401-attribution hook. Callers can wire this so a 401
    /// from the Responses API emits an `auth_401_attribution` event
    /// with `consumer == "WebSearch"`.
    attribution_callback: Option<SharedAttributionCallback>,
}
impl WebSearchClient {
    /// Create a new web search client from `WebSearchConfig::Enabled`.
    ///
    /// Returns `Err` if the config is `Disabled` or if header values are invalid.
    pub fn new(
        config: &WebSearchConfig,
        default_api_key_provider: Option<SharedApiKeyProvider>,
    ) -> Result<Self, xai_tool_runtime::ToolError> {
        let WebSearchConfig::Enabled {
            api_key,
            base_url,
            model,
            extra_headers,
            env_http_headers,
            alpha_test_key,
            api_key_provider,
        } = config
        else {
            return Err(xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                "Cannot create WebSearchClient from disabled config".to_string(),
            ));
        };
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        // Only attach a bearer when the route actually has one. A
        // header-authenticated model (`CredentialSource::ExplicitHeader`)
        // carries its credential in `extra_headers` instead, and inventing an
        // empty `Bearer` for it is not harmless: the `authorization` flavour
        // would have it overwritten by the loop below, but the `x-api-key`
        // flavour would send the bogus bearer alongside the real credential.
        // The sampler skips it the same way for its own `Option` key (#160).
        if let Some(api_key) = api_key {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|e| {
                    xai_tool_runtime::ToolError::execution(
                        xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                        format!("Invalid API key for header: {e}"),
                    )
                })?,
            );
        }
        for (key, value) in extra_headers {
            let header_name = HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    format!("Invalid header name '{key}': {e}"),
                )
            })?;
            let header_value = HeaderValue::from_str(value).map_err(|e| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    format!("Invalid header value for '{key}': {e}"),
                )
            })?;
            headers.insert(header_name, header_value);
        }
        // Resolved here, not folded into `extra_headers` upstream, so an
        // env-sourced secret never enters the config struct — the same reason
        // the sampler resolves its own `env_http_headers` at client build.
        // Applied after `extra_headers` so an env-backed header wins over a
        // literal of the same name, matching the sampler's order. Unset, blank
        // and unrepresentable entries are skipped rather than failing the
        // build: a mapping to a variable the user has not exported is a
        // configuration the sampler already tolerates (#160).
        for (key, env_var) in env_http_headers {
            let Ok(value) = std::env::var(env_var) else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            let (Ok(header_name), Ok(header_value)) = (
                HeaderName::from_bytes(key.as_bytes()),
                HeaderValue::from_str(value),
            ) else {
                tracing::warn!(
                    header = %key,
                    env_var = %env_var,
                    "skipping env_http_header with an invalid header name or value"
                );
                continue;
            };
            headers.insert(header_name, header_value);
        }
        let _ = alpha_test_key;
        let provider_scoped = api_key_provider.is_some();
        let transport_profile = api_key_provider
            .as_ref()
            .map(|provider| provider.transport_profile())
            .unwrap_or_default();
        let base_url = if transport_profile == ApiTransportProfile::CodexResponses {
            normalize_codex_base_url(base_url).map_err(|message| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    message.to_string(),
                )
            })?
        } else {
            base_url.clone()
        };
        if transport_profile == ApiTransportProfile::CodexResponses {
            let content_type = headers.get(CONTENT_TYPE).cloned();
            headers.clear();
            if let Some(value) = content_type {
                headers.insert(CONTENT_TYPE, value);
            }
            headers.insert(
                USER_AGENT,
                grok_build_user_agent().map_err(|message| {
                    xai_tool_runtime::ToolError::execution(
                        xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                        message.to_string(),
                    )
                })?,
            );
        } else {
            strip_codex_routing_headers(&mut headers);
        }
        let request_api_key_provider = api_key_provider.clone().or_else(|| {
            if accepts_xai_session_provider(&base_url) {
                default_api_key_provider
            } else {
                None
            }
        });
        let mut http_builder = reqwest::Client::builder().default_headers(headers);
        if request_api_key_provider.is_some() {
            // A dynamic credential is valid only for the configured Responses
            // endpoint. Do not let bearer/account headers follow a same-origin
            // path redirect, downgrade to cleartext, or escape to another origin.
            http_builder = http_builder.redirect(reqwest::redirect::Policy::custom(|attempt| {
                let Some(previous) = attempt.previous().last() else {
                    return attempt.stop();
                };
                if should_follow_redirect_with_dynamic_credential(previous, attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }));
        } else {
            // Static keys get no redirect at all: a bearer attached at build
            // time must never follow a redirect to an origin that was never
            // classified, matching the sampler-wide no-redirect policy.
            http_builder = http_builder.redirect(reqwest::redirect::Policy::none());
        }
        let http = xai_grok_extra_ca::with_extra_root_certificates(http_builder)
            .build()
            .map_err(|_| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    "Failed to build HTTP client.".to_string(),
                )
            })?;
        Ok(Self {
            http,
            base_url,
            model: model.clone(),
            api_key_provider: request_api_key_provider,
            provider_scoped,
            transport_profile,
            attribution_callback: None,
        })
    }
    /// Wire a 401-attribution callback into this client. Idempotent;
    /// safe to call before or after the first request.
    pub fn with_attribution_callback(
        mut self,
        callback: Option<SharedAttributionCallback>,
    ) -> Self {
        self.attribution_callback = callback;
        self
    }
    async fn current_credential(&self) -> Option<ApiCredential> {
        crate::types::api_key_provider::resolve_credential(self.api_key_provider.as_ref()).await
    }
    async fn compare_sent_credential(
        &self,
        sent_bearer: Option<&str>,
    ) -> xai_grok_auth::CredentialComparison {
        crate::types::api_key_provider::compare_sent_bearer(
            self.api_key_provider.as_ref(),
            sent_bearer,
        )
        .await
    }
    fn record_401_attribution(&self, comparison: xai_grok_auth::CredentialComparison) {
        crate::attribution::emit_401(
            self.attribution_callback.as_ref(),
            ToolConsumer::WebSearch,
            comparison,
        );
    }

    async fn execute_authenticated<T: serde::Serialize + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<reqwest::Response, xai_tool_runtime::ToolError> {
        let mut recovered_once = false;
        loop {
            let request = self.build_authenticated_request(url, body).await?;
            let sent_bearer = crate::types::api_key_provider::request_credential(&request);
            let response = self.http.execute(request).await.map_err(|_| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    "Responses API transport failed.".to_string(),
                )
            })?;
            let status = response.status();
            if !crate::types::api_key_provider::is_auth_failure(status) {
                return Ok(response);
            }

            let comparison = self.compare_sent_credential(sent_bearer.as_deref()).await;
            self.record_401_attribution(comparison);
            drop(response);

            let provider_owns_auth_retry = status == reqwest::StatusCode::UNAUTHORIZED
                && self.provider_scoped
                && self.transport_profile == ApiTransportProfile::CodexResponses;
            if !recovered_once
                && provider_owns_auth_retry
                && let Some(rejected_bearer) = sent_bearer.as_deref()
                && crate::types::api_key_provider::recover_rejected_bearer(
                    self.api_key_provider.as_ref(),
                    rejected_bearer,
                )
                .await
            {
                recovered_once = true;
                continue;
            }

            return Err(xai_tool_runtime::ToolError::unauthorized(format!(
                "Responses API authentication failed (HTTP {status})."
            ))
            .with_details(serde_json::json!({
                "tool_id": "web_search",
                "status": status.as_u16(),
                (crate::types::PROVIDER_AUTH_RETRY_HANDLED_DETAILS_KEY): provider_owns_auth_retry,
            })));
        }
    }

    async fn build_authenticated_request<T: serde::Serialize + ?Sized>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<reqwest::Request, xai_tool_runtime::ToolError> {
        if self.transport_profile == ApiTransportProfile::CodexResponses
            && url != format!("{}/responses", self.base_url)
        {
            return Err(xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                "OpenAI Codex requests must use the configured canonical Responses endpoint."
                    .to_string(),
            ));
        }
        let live_credential = self.current_credential().await;
        if self.provider_scoped && live_credential.is_none() {
            return Err(xai_tool_runtime::ToolError::unauthorized(
                "Provider-scoped web search credential is unavailable.".to_string(),
            ));
        }
        let mut request = self.http.post(url).json(body).build().map_err(|_| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                "Failed to build HTTP request.".to_string(),
            )
        })?;
        if let Some(credential) = live_credential {
            let authorization =
                HeaderValue::from_str(&format!("Bearer {}", credential.access_token)).map_err(
                    |_| {
                        xai_tool_runtime::ToolError::execution(
                            xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                            "Provider returned an invalid bearer credential.".to_string(),
                        )
                    },
                )?;
            if self.transport_profile == ApiTransportProfile::CodexResponses {
                retain_codex_headers(
                    request.headers_mut(),
                    authorization,
                    credential.account_id.as_deref(),
                    credential.chatgpt_account_is_fedramp,
                )
                .map_err(|message| {
                    xai_tool_runtime::ToolError::execution(
                        xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                        message.to_string(),
                    )
                })?;
            } else {
                request.headers_mut().insert(AUTHORIZATION, authorization);
                strip_codex_routing_headers(request.headers_mut());
            }
        } else {
            strip_codex_routing_headers(request.headers_mut());
        }
        Ok(request)
    }
    /// Perform a web search query using the Responses API.
    ///
    /// Returns `(content, citations)` where content is the assistant's text
    /// and citations are unique URLs found in the response annotations.
    pub async fn search(
        &self,
        query: &str,
        allowed_domains: Option<Vec<String>>,
    ) -> Result<(String, Vec<String>), xai_tool_runtime::ToolError> {
        let web_search = rs::WebSearchToolArgs::default()
            .filters(rs::WebSearchToolFilters { allowed_domains })
            .build()
            .map_err(|e| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    format!("Failed to build web search tool: {e}"),
                )
            })?;
        let mut request_builder = rs::CreateResponseArgs::default();
        request_builder
            .model(self.model.clone())
            .input(query.to_string())
            .tools(vec![rs::Tool::WebSearch(web_search)])
            .store(false)
            .max_output_tokens(8192u32);
        if self.transport_profile != ApiTransportProfile::CodexResponses {
            request_builder.temperature(0.1).top_p(0.95);
        }
        let request = request_builder.build().map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("Failed to build request: {e}"),
            )
        })?;
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let response = self.execute_authenticated(&url, &request).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("Responses API request failed (HTTP {status})."),
            ));
        }
        let bytes = response.bytes().await.map_err(|_| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                "Failed to read Responses API response.".to_string(),
            )
        })?;
        let response_obj: rs::Response = serde_json::from_slice(&bytes).map_err(|_| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                "Responses API returned an invalid response.".to_string(),
            )
        })?;
        let content = response_obj
            .output_text()
            .unwrap_or_else(|| "No search results found.".to_string());
        let citations = extract_citations(&response_obj);
        Ok((content, citations))
    }
    /// Same as [`Self::search`] but also extracts per-citation titles when
    /// the Responses API surfaces them. Returns `(content, citations_with_titles)`
    /// where each citation is `(title, url)`. Empty `title` strings indicate
    /// the upstream didn't supply one for that URL.
    ///
    /// Used by the cursor-compat `WebSearch` adapter to render a
    /// `Links:\n1. [title](url)` list instead of the LLM synthesis text.
    pub async fn search_with_titles(
        &self,
        query: &str,
        allowed_domains: Option<Vec<String>>,
    ) -> Result<(String, Vec<(String, String)>), xai_tool_runtime::ToolError> {
        let web_search = rs::WebSearchToolArgs::default()
            .filters(rs::WebSearchToolFilters { allowed_domains })
            .build()
            .map_err(|e| {
                xai_tool_runtime::ToolError::execution(
                    xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                    format!("Failed to build web search tool: {e}"),
                )
            })?;
        let mut request_builder = rs::CreateResponseArgs::default();
        request_builder
            .model(self.model.clone())
            .input(query.to_string())
            .tools(vec![rs::Tool::WebSearch(web_search)])
            .store(false)
            .max_output_tokens(8192u32);
        if self.transport_profile != ApiTransportProfile::CodexResponses {
            request_builder.temperature(0.1).top_p(0.95);
        }
        let request = request_builder.build().map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("Failed to build request: {e}"),
            )
        })?;
        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        let response = self.execute_authenticated(&url, &request).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                format!("Responses API request failed (HTTP {status})."),
            ));
        }
        let bytes = response.bytes().await.map_err(|_| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                "Failed to read Responses API response.".to_string(),
            )
        })?;
        let response_obj: rs::Response = serde_json::from_slice(&bytes).map_err(|_| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("web_search").expect("valid"),
                "Responses API returned an invalid response.".to_string(),
            )
        })?;
        let content = response_obj
            .output_text()
            .unwrap_or_else(|| "No search results found.".to_string());
        let pairs = extract_citation_pairs(&response_obj);
        Ok((content, pairs))
    }
}
/// Extract citation URLs from the Response output items.
/// The async-openai crate doesn't provide a helper for this, and the `url` field
/// in `UrlCitationBody` is private, so we serialize to JSON to extract it.
fn extract_citations(response: &rs::Response) -> Vec<String> {
    let mut citations = Vec::new();
    for output_item in &response.output {
        if let rs::OutputItem::Message(output_message) = output_item {
            for message_content in &output_message.content {
                if let rs::OutputMessageContent::OutputText(text_content) = message_content {
                    for annotation in &text_content.annotations {
                        if let rs::Annotation::UrlCitation(url_citation) = annotation
                            && let Ok(json) = serde_json::to_value(url_citation)
                            && let Some(url) = json.get("url").and_then(|v| v.as_str())
                        {
                            citations.push(url.to_string());
                        }
                    }
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    citations.retain(|url| seen.insert(url.clone()));
    citations
}
/// Extract `(title, url)` pairs from the Responses API annotations.
///
/// `title` may be an empty string when upstream doesn't supply one. URLs
/// are deduplicated while preserving the first-seen order so the rendered
/// `Links:` list is stable and free of duplicates.
fn extract_citation_pairs(response: &rs::Response) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for output_item in &response.output {
        if let rs::OutputItem::Message(output_message) = output_item {
            for message_content in &output_message.content {
                if let rs::OutputMessageContent::OutputText(text_content) = message_content {
                    for annotation in &text_content.annotations {
                        if let rs::Annotation::UrlCitation(url_citation) = annotation
                            && let Ok(json) = serde_json::to_value(url_citation)
                        {
                            let url = json.get("url").and_then(|v| v.as_str()).unwrap_or("");
                            if url.is_empty() {
                                continue;
                            }
                            let title = json
                                .get("title")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            pairs.push((title, url.to_string()));
                        }
                    }
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    pairs.retain(|(_t, url)| seen.insert(url.clone()));
    pairs
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_web_search_omits_fedramp_for_untrusted_snapshot() {
        let mut headers = HeaderMap::from_iter([
            (CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (OPENAI_FEDRAMP, HeaderValue::from_static("true")),
        ]);
        retain_codex_headers(
            &mut headers,
            HeaderValue::from_static("Bearer trusted"),
            Some("workspace-123"),
            false,
        )
        .unwrap();
        assert!(headers.get(OPENAI_FEDRAMP).is_none());
        assert_eq!(headers[ORIGINATOR], "grok_build");
        assert_eq!(
            headers[USER_AGENT],
            format!("xai-grok-build/{}", xai_grok_version::VERSION)
        );
    }
    use indexmap::IndexMap;
    /// Helper to create a Response from JSON for testing.
    fn response_from_json(json: serde_json::Value) -> rs::Response {
        serde_json::from_value(json).expect("Failed to parse test Response JSON")
    }
    #[test]
    fn test_new_client_uses_configured_model() {
        let config = WebSearchConfig::Enabled {
            api_key: Some("test-key".to_string()),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "custom-enterprise-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: None,
        };
        let client = WebSearchClient::new(&config, None).expect("client should build");
        assert_eq!(client.model, "custom-enterprise-model");
    }
    /// Counts attribution callback invocations for the test below.
    #[derive(Default, Debug)]
    struct CountingCallback {
        invocations: std::sync::Mutex<Vec<(ToolConsumer, xai_grok_auth::CredentialComparison)>>,
    }
    impl crate::attribution::Auth401AttributionCallback for CountingCallback {
        fn record_401(
            &self,
            consumer: ToolConsumer,
            comparison: xai_grok_auth::CredentialComparison,
        ) {
            self.invocations
                .lock()
                .unwrap()
                .push((consumer, comparison));
        }
    }
    /// `record_401_attribution` invokes the wired callback with
    /// `ToolConsumer::WebSearch` and a safe credential relation.
    /// No credential value crosses the trait boundary.
    #[test]
    fn record_401_attribution_passes_safe_relation_to_callback() {
        let cb = std::sync::Arc::new(CountingCallback::default());
        let cb_dyn: crate::attribution::SharedAttributionCallback = cb.clone();
        let config = WebSearchConfig::Enabled {
            api_key: Some("ignored".to_string()),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: None,
        };
        let client = WebSearchClient::new(&config, None)
            .expect("client should build")
            .with_attribution_callback(Some(cb_dyn));
        client
            .record_401_attribution(xai_grok_auth::CredentialComparison::different_from_current());
        let calls = cb.invocations.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, ToolConsumer::WebSearch);
        assert_eq!(
            calls[0].1,
            xai_grok_auth::CredentialComparison::different_from_current(),
        );
    }
    /// `record_401_attribution` is a no-op when no callback is wired
    /// -- the BYOK / standalone case must not panic or allocate.
    #[test]
    fn record_401_attribution_is_noop_without_callback() {
        let config = WebSearchConfig::Enabled {
            api_key: Some("test-key".to_string()),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: None,
        };
        let client = WebSearchClient::new(&config, None).expect("client should build");
        client.record_401_attribution(xai_grok_auth::CredentialComparison::same_as_current());
        client.record_401_attribution(xai_grok_auth::CredentialComparison::not_sent(false));
    }
    #[test]
    fn test_extract_citations_empty_response() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "output": [],
            "model": "test-model"
        }));
        let citations = extract_citations(&response);
        assert!(citations.is_empty());
    }
    #[test]
    fn test_extract_citations_with_url_citations() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Here is some info about Rust.",
                            "annotations": [
                                {
                                    "type": "url_citation",
                                    "url": "https://www.rust-lang.org/",
                                    "title": "Rust Programming Language",
                                    "start_index": 0,
                                    "end_index": 10
                                },
                                {
                                    "type": "url_citation",
                                    "url": "https://docs.rs/",
                                    "title": "Docs.rs",
                                    "start_index": 11,
                                    "end_index": 20
                                }
                            ]
                        }
                    ]
                }
            ]
        }));
        let citations = extract_citations(&response);
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0], "https://www.rust-lang.org/");
        assert_eq!(citations[1], "https://docs.rs/");
    }
    #[test]
    fn test_extract_citations_deduplicates() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Info with duplicate citations.",
                            "annotations": [
                                {
                                    "type": "url_citation",
                                    "url": "https://example.com/page1",
                                    "title": "Page 1",
                                    "start_index": 0,
                                    "end_index": 5
                                },
                                {
                                    "type": "url_citation",
                                    "url": "https://example.com/page2",
                                    "title": "Page 2",
                                    "start_index": 6,
                                    "end_index": 10
                                },
                                {
                                    "type": "url_citation",
                                    "url": "https://example.com/page1",
                                    "title": "Page 1 Again",
                                    "start_index": 11,
                                    "end_index": 15
                                }
                            ]
                        }
                    ]
                }
            ]
        }));
        let citations = extract_citations(&response);
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0], "https://example.com/page1");
        assert_eq!(citations[1], "https://example.com/page2");
    }
    #[test]
    fn test_extract_citations_multiple_messages() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "First message",
                            "annotations": [
                                {
                                    "type": "url_citation",
                                    "url": "https://first.com/",
                                    "title": "First",
                                    "start_index": 0,
                                    "end_index": 5
                                }
                            ]
                        }
                    ]
                },
                {
                    "type": "message",
                    "id": "msg_2",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Second message",
                            "annotations": [
                                {
                                    "type": "url_citation",
                                    "url": "https://second.com/",
                                    "title": "Second",
                                    "start_index": 0,
                                    "end_index": 6
                                }
                            ]
                        }
                    ]
                }
            ]
        }));
        let citations = extract_citations(&response);
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0], "https://first.com/");
        assert_eq!(citations[1], "https://second.com/");
    }
    #[test]
    fn test_extract_citations_ignores_non_url_annotations() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Some text",
                            "annotations": [
                                {
                                    "type": "url_citation",
                                    "url": "https://valid.com/",
                                    "title": "Valid",
                                    "start_index": 0,
                                    "end_index": 4
                                }
                            ]
                        }
                    ]
                }
            ]
        }));
        let citations = extract_citations(&response);
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0], "https://valid.com/");
    }
    /// A provider that always returns `None`, simulating an API-key user
    /// whose token has aged past the client-side TTL.
    struct NoneProvider;
    impl crate::types::ApiKeyProvider for NoneProvider {
        fn current_api_key(&self) -> Option<String> {
            None
        }
    }
    struct ScopedProvider;
    impl crate::types::ApiKeyProvider for ScopedProvider {
        fn current_api_key(&self) -> Option<String> {
            Some("codex-key".to_string())
        }

        fn transport_profile(&self) -> ApiTransportProfile {
            ApiTransportProfile::CodexResponses
        }

        fn current_credential_async(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<ApiCredential>> + Send + '_>>
        {
            Box::pin(std::future::ready(Some(ApiCredential {
                access_token: "codex-key".to_string(),
                account_id: Some("codex-account".to_string()),
                chatgpt_account_is_fedramp: true,
            })))
        }
    }

    fn codex_config(base_url: &str) -> WebSearchConfig {
        WebSearchConfig::Enabled {
            api_key: Some("stale-codex-key".to_string()),
            base_url: base_url.to_string(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: Some(std::sync::Arc::new(ScopedProvider)),
        }
    }

    #[test]
    fn codex_web_search_rejects_noncanonical_production_destinations() {
        let secret = "must-not-appear-0123456789";
        for base_url in [
            "http://chatgpt.com/backend-api/codex",
            "https://evil.example/backend-api/codex",
            "https://chatgpt.com/backend-api",
            "https://chatgpt.com/backend-api/codex/extra",
            "https://chatgpt.com:444/backend-api/codex",
            "https://chatgpt.com:443/backend-api/codex",
            "https://user@chatgpt.com/backend-api/codex",
            "https://chatgpt.com/backend-api/codex?target=evil",
            "https://chatgpt.com/backend-api/codex#evil",
            "https://evil.example/backend-api/codex?token=must-not-appear-0123456789",
        ] {
            let error = match WebSearchClient::new(&codex_config(base_url), None) {
                Ok(_) => panic!("noncanonical Codex destination was accepted: {base_url}"),
                Err(error) => error,
            };
            let message = error.to_string();
            assert!(
                message.contains(CODEX_BASE_URL),
                "unexpected safe error for {base_url}: {message}"
            );
            assert!(
                !message.contains(secret),
                "credential-bearing destination leaked into error: {message}"
            );
        }
    }

    #[tokio::test]
    async fn codex_web_search_uses_canonical_responses_endpoint_before_auth() {
        let client = WebSearchClient::new(
            &codex_config("https://chatgpt.com/backend-api/codex/"),
            None,
        )
        .expect("canonical Codex destination should build");
        assert_eq!(client.base_url, CODEX_BASE_URL);

        let endpoint = format!("{}/responses", client.base_url);
        let request = client
            .build_authenticated_request(&endpoint, &serde_json::json!({}))
            .await
            .expect("canonical Codex request should build");
        assert_eq!(
            request.url().as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(request.headers()[AUTHORIZATION], "Bearer codex-key");
        assert_eq!(request.headers()[CHATGPT_ACCOUNT_ID], "codex-account");
        assert_eq!(request.headers()[OPENAI_FEDRAMP], "true");

        let secret = "must-not-appear-0123456789";
        let error = client
            .build_authenticated_request(
                &format!("https://evil.example/responses?token={secret}"),
                &serde_json::json!({}),
            )
            .await
            .expect_err("noncanonical request URL must fail before auth attachment");
        assert!(!error.to_string().contains(secret));
    }

    #[derive(Default)]
    struct RecoveringCodexProvider {
        recovered: std::sync::atomic::AtomicBool,
        recovery_calls: std::sync::atomic::AtomicUsize,
    }
    impl RecoveringCodexProvider {
        fn credential(&self) -> ApiCredential {
            if self.recovered.load(std::sync::atomic::Ordering::SeqCst) {
                ApiCredential {
                    access_token: "fresh-codex-key".to_string(),
                    account_id: Some("fresh-codex-account".to_string()),
                    chatgpt_account_is_fedramp: true,
                }
            } else {
                ApiCredential {
                    access_token: "rejected-codex-key".to_string(),
                    account_id: Some("rejected-codex-account".to_string()),
                    chatgpt_account_is_fedramp: true,
                }
            }
        }
    }
    impl crate::types::ApiKeyProvider for RecoveringCodexProvider {
        fn current_api_key(&self) -> Option<String> {
            Some(self.credential().access_token)
        }

        fn transport_profile(&self) -> ApiTransportProfile {
            ApiTransportProfile::CodexResponses
        }

        fn current_credential_async(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<ApiCredential>> + Send + '_>>
        {
            Box::pin(std::future::ready(Some(self.credential())))
        }

        fn recover_rejected_credential_async<'a>(
            &'a self,
            rejected_bearer: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
            Box::pin(async move {
                self.recovery_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if rejected_bearer != "rejected-codex-key" {
                    return false;
                }
                !self
                    .recovered
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            })
        }
    }
    struct GenericScopedProvider;
    impl crate::types::ApiKeyProvider for GenericScopedProvider {
        fn current_api_key(&self) -> Option<String> {
            Some("generic-key".to_string())
        }
    }

    fn successful_search_response(text: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": []
                }]
            }]
        })
    }

    #[tokio::test]
    async fn codex_web_search_omits_unsupported_sampling_parameters() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(successful_search_response("codex result")),
            )
            .expect(2)
            .mount(&server)
            .await;

        let scoped: SharedApiKeyProvider = std::sync::Arc::new(ScopedProvider);
        let config = WebSearchConfig::Enabled {
            api_key: Some("stale-codex-key".to_string()),
            base_url: server.uri(),
            model: "codex-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: Some(scoped),
        };
        let client = WebSearchClient::new(&config, None).expect("client should build");
        client.search("first query", None).await.unwrap();
        client
            .search_with_titles("second query", None)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert!(
                body.get("temperature").is_none(),
                "Codex request must omit temperature: {body}"
            );
            assert!(
                body.get("top_p").is_none(),
                "Codex request must omit top_p: {body}"
            );
        }
    }

    #[tokio::test]
    async fn codex_web_search_recovers_scoped_401_for_both_entry_points() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        for with_titles in [false, true] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/responses"))
                .and(header("Authorization", "Bearer rejected-codex-key"))
                .and(header("chatgpt-account-id", "rejected-codex-account"))
                .respond_with(ResponseTemplate::new(401))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/responses"))
                .and(header("Authorization", "Bearer fresh-codex-key"))
                .and(header("chatgpt-account-id", "fresh-codex-account"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(successful_search_response("recovered result")),
                )
                .expect(1)
                .mount(&server)
                .await;

            let provider = std::sync::Arc::new(RecoveringCodexProvider::default());
            let scoped: SharedApiKeyProvider = provider.clone();
            let config = WebSearchConfig::Enabled {
                api_key: Some("snapshot-codex-key".to_string()),
                base_url: server.uri(),
                model: "codex-model".to_string(),
                extra_headers: IndexMap::new(),
                env_http_headers: Default::default(),
                alpha_test_key: None,
                api_key_provider: Some(scoped),
            };
            let client = WebSearchClient::new(&config, None).expect("client should build");

            let content = if with_titles {
                client
                    .search_with_titles("query", None)
                    .await
                    .expect("titled search should recover")
                    .0
            } else {
                client
                    .search("query", None)
                    .await
                    .expect("search should recover")
                    .0
            };
            assert_eq!(content, "recovered result");
            assert_eq!(
                provider
                    .recovery_calls
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn codex_web_search_scoped_401_recovery_retries_only_once() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("Authorization", "Bearer rejected-codex-key"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("Authorization", "Bearer fresh-codex-key"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;

        let provider = std::sync::Arc::new(RecoveringCodexProvider::default());
        let scoped: SharedApiKeyProvider = provider.clone();
        let config = WebSearchConfig::Enabled {
            api_key: Some("snapshot-codex-key".to_string()),
            base_url: server.uri(),
            model: "codex-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: Some(scoped),
        };
        let client = WebSearchClient::new(&config, None).expect("client should build");

        let error = client.search("query", None).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 401"));
        assert_eq!(
            error
                .details
                .as_ref()
                .and_then(
                    |details| details.get(crate::types::PROVIDER_AUTH_RETRY_HANDLED_DETAILS_KEY)
                )
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "session-wide auth retry must not replace the exhausted scoped credential"
        );
        assert_eq!(
            provider
                .recovery_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn generic_web_search_preserves_sampling_parameters() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(successful_search_response("generic result")),
            )
            .mount(&server)
            .await;
        let config = WebSearchConfig::Enabled {
            api_key: Some("generic-key".to_string()),
            base_url: server.uri(),
            model: "generic-model".to_string(),
            extra_headers: IndexMap::from([
                (
                    "chatgpt-account-id".to_string(),
                    "must-not-leak".to_string(),
                ),
                ("x-openai-fedramp".to_string(), "true".to_string()),
                ("originator".to_string(), "codex_cli_rs".to_string()),
            ]),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: Some(std::sync::Arc::new(GenericScopedProvider)),
        };
        let client = WebSearchClient::new(&config, None).expect("client should build");
        client.search("generic query", None).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body.get("temperature"), Some(&serde_json::json!(0.1)));
        assert_eq!(body.get("top_p"), Some(&serde_json::json!(0.95)));
        for name in ["chatgpt-account-id", "x-openai-fedramp", "originator"] {
            assert!(
                requests[0].headers.get(name).is_none(),
                "Codex-only routing header leaked to generic Responses: {name}"
            );
        }
    }
    /// When the dynamic provider returns `None`, the static `api_key`
    /// from config must still be sent as the Authorization header.
    /// This is a regression scenario: API-key users
    /// past the 30-day client TTL saw 401 because no auth was sent.
    #[tokio::test]
    async fn static_api_key_is_fallback_when_provider_returns_none() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("Authorization", "Bearer static-key-from-config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "status": "completed",
                "model": "test-model",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "search result",
                        "annotations": []
                    }]
                }]
            })))
            .mount(&server)
            .await;
        let config = WebSearchConfig::Enabled {
            api_key: Some("static-key-from-config".to_string()),
            base_url: server.uri(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: None,
        };
        let provider: SharedApiKeyProvider = std::sync::Arc::new(NoneProvider);
        let client = WebSearchClient::new(&config, Some(provider)).expect("client should build");
        let (content, _citations) = client
            .search("test query", None)
            .await
            .expect("search must succeed with static key fallback");
        assert_eq!(content, "search result");
    }
    /// A session-wide provider belongs to xAI's first-party transport and must
    /// never replace a model-scoped static key on an external endpoint.
    #[tokio::test]
    async fn external_endpoint_uses_static_key_not_session_default_provider() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        struct FreshProvider(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl crate::types::ApiKeyProvider for FreshProvider {
            fn current_api_key(&self) -> Option<String> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some("fresh-key-from-provider".to_string())
            }
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("Authorization", "Bearer model-static-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "status": "completed",
                "model": "test-model",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "fresh result",
                        "annotations": []
                    }]
                }]
            })))
            .mount(&server)
            .await;
        let config = WebSearchConfig::Enabled {
            api_key: Some("model-static-key".to_string()),
            base_url: server.uri(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: None,
        };
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: SharedApiKeyProvider =
            std::sync::Arc::new(FreshProvider(std::sync::Arc::clone(&calls)));
        let client = WebSearchClient::new(&config, Some(provider)).expect("client should build");
        let (content, _citations) = client
            .search("test query", None)
            .await
            .expect("search must use the credential scoped to the external model");
        assert_eq!(content, "fresh result");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an external model must not even resolve the session-wide xAI credential"
        );
    }

    #[tokio::test]
    async fn first_party_xai_endpoint_uses_session_default_provider() {
        struct FreshProvider;
        impl crate::types::ApiKeyProvider for FreshProvider {
            fn current_api_key(&self) -> Option<String> {
                Some("fresh-xai-session-key".to_string())
            }
        }

        let config = WebSearchConfig::Enabled {
            api_key: Some("stale-static-key".to_string()),
            base_url: "https://api.x.ai/v1".to_string(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: None,
        };
        let provider: SharedApiKeyProvider = std::sync::Arc::new(FreshProvider);
        let client = WebSearchClient::new(&config, Some(provider)).expect("client should build");
        let request = client
            .build_authenticated_request(
                "https://api.x.ai/v1/responses",
                &serde_json::json!({"model": "test-model"}),
            )
            .await
            .expect("first-party request should build");
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer fresh-xai-session-key"
        );
    }

    struct AmbientRedirectProvider;
    impl crate::types::ApiKeyProvider for AmbientRedirectProvider {
        fn current_api_key(&self) -> Option<String> {
            Some("ambient-xai-session-key".to_string())
        }
    }

    fn production_proxy_client_with_local_transport(base_url: String) -> WebSearchClient {
        let config = WebSearchConfig::Enabled {
            api_key: Some("stale-static-key".to_string()),
            base_url: xai_grok_env::PROD_CLI_CHAT_PROXY_BASE_URL.to_string(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: None,
        };
        let provider: SharedApiKeyProvider = std::sync::Arc::new(AmbientRedirectProvider);
        let mut client =
            WebSearchClient::new(&config, Some(provider)).expect("production client should build");
        // Keep the redirect policy selected for the validated production proxy,
        // but route the request through an in-process server for determinism.
        client.base_url = base_url;
        client
    }

    #[tokio::test]
    async fn ambient_provider_blocks_same_host_production_proxy_path_redirect_with_bearer() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer ambient-xai-session-key",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(307)
                    .insert_header("Location", "/v1/redirected-responses"),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/redirected-responses"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer ambient-xai-session-key",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(successful_search_response("redirected result")),
            )
            .mount(&server)
            .await;

        let client = production_proxy_client_with_local_transport(server.uri());
        let error = client.search("test query", None).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 307"));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "an ambient bearer must never reach a redirected proxy path"
        );
    }

    #[test]
    fn dynamic_credential_policy_blocks_https_to_http_same_host_443_redirect() {
        let secure = reqwest::Url::parse("https://cli-chat-proxy.grok.com/v1/responses").unwrap();
        let cleartext =
            reqwest::Url::parse("http://cli-chat-proxy.grok.com:443/v1/capture").unwrap();
        assert_eq!(secure.host_str(), cleartext.host_str());
        assert_eq!(
            secure.port_or_known_default(),
            cleartext.port_or_known_default()
        );
        assert_ne!(secure.scheme(), cleartext.scheme());
        assert!(
            !should_follow_redirect_with_dynamic_credential(&secure, &cleartext),
            "a dynamic credential must not follow an HTTPS-to-HTTP redirect"
        );
    }

    #[test]
    fn session_default_provider_allowlist_rejects_lookalikes_and_cleartext() {
        for allowed in [
            "https://api.x.ai/v1",
            xai_grok_env::PROD_CLI_CHAT_PROXY_BASE_URL,
        ] {
            assert!(accepts_xai_session_provider(allowed), "rejected {allowed}");
        }
        for denied in [
            "http://api.x.ai/v1",
            "https://api.x.ai:8443/v1",
            "https://user@api.x.ai/v1",
            "https://api.x.ai.evil.example/v1",
            "https://cli-chat-proxy.grok.com.evil.example/v1",
            "https://cli-chat-proxy.grok.com/not-v1",
            "http://127.0.0.1:8080/v1",
        ] {
            assert!(!accepts_xai_session_provider(denied), "accepted {denied}");
        }
    }

    /// A model-scoped provider must win over the session-wide default. Codex
    /// web search uses this boundary so an xAI session token can never replace
    /// its refreshed OpenAI bearer on a request to the Codex endpoint.
    #[tokio::test]
    async fn provider_scoped_key_wins_over_default_provider() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        struct StaticProvider(&'static str);
        impl crate::types::ApiKeyProvider for StaticProvider {
            fn current_api_key(&self) -> Option<String> {
                Some(self.0.to_string())
            }
        }

        struct RefreshingScopedProvider;
        impl crate::types::ApiKeyProvider for RefreshingScopedProvider {
            fn current_api_key(&self) -> Option<String> {
                Some("stale-codex-key".to_string())
            }

            fn transport_profile(&self) -> ApiTransportProfile {
                ApiTransportProfile::CodexResponses
            }

            fn current_api_key_async(
                &self,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + '_>>
            {
                Box::pin(std::future::ready(Some("refreshed-codex-key".to_string())))
            }

            fn current_credential_async(
                &self,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Option<ApiCredential>> + Send + '_>,
            > {
                Box::pin(std::future::ready(Some(ApiCredential {
                    access_token: "refreshed-codex-key".to_string(),
                    account_id: Some("refreshed-codex-account".to_string()),
                    chatgpt_account_is_fedramp: true,
                })))
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("Authorization", "Bearer refreshed-codex-key"))
            .and(header("chatgpt-account-id", "refreshed-codex-account"))
            .and(header("x-openai-fedramp", "true"))
            .and(header("originator", "grok_build"))
            .and(header(
                "user-agent",
                format!("xai-grok-build/{}", xai_grok_version::VERSION),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "resp_test",
                "object": "response",
                "created_at": 1234567890,
                "status": "completed",
                "model": "test-model",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "provider scoped result",
                        "annotations": []
                    }]
                }]
            })))
            .mount(&server)
            .await;

        let scoped: SharedApiKeyProvider = std::sync::Arc::new(RefreshingScopedProvider);
        let default: SharedApiKeyProvider = std::sync::Arc::new(StaticProvider("xai-session-key"));
        let extra_headers = IndexMap::from([
            ("authorization".to_string(), "Bearer attacker".to_string()),
            (
                "chatgpt-account-id".to_string(),
                "stale-codex-account".to_string(),
            ),
            ("x-openai-fedramp".to_string(), "false".to_string()),
            ("originator".to_string(), "codex_cli_rs".to_string()),
            ("x-api-key".to_string(), "must-not-leak".to_string()),
            ("traceparent".to_string(), "must-not-leak".to_string()),
            ("user-agent".to_string(), "codex_cli_rs/0.0.0".to_string()),
        ]);
        let config = WebSearchConfig::Enabled {
            api_key: Some("snapshot-codex-key".to_string()),
            base_url: server.uri(),
            model: "test-model".to_string(),
            extra_headers,
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: Some(scoped),
        };
        let client = WebSearchClient::new(&config, Some(default)).expect("client should build");
        let (content, _) = client
            .search("test query", None)
            .await
            .expect("scoped provider must authenticate the request");
        assert_eq!(content, "provider scoped result");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        for name in ["x-api-key", "traceparent"] {
            assert!(
                requests[0].headers.get(name).is_none(),
                "hostile header bypassed the Codex allowlist: {name}"
            );
        }
    }

    #[tokio::test]
    async fn missing_scoped_key_fails_closed_without_using_default_provider() {
        struct DefaultProvider;
        impl crate::types::ApiKeyProvider for DefaultProvider {
            fn current_api_key(&self) -> Option<String> {
                Some("xai-session-key".to_string())
            }
        }

        let server = wiremock::MockServer::start().await;
        let scoped: SharedApiKeyProvider = std::sync::Arc::new(NoneProvider);
        let default: SharedApiKeyProvider = std::sync::Arc::new(DefaultProvider);
        let config = WebSearchConfig::Enabled {
            api_key: Some("stale-codex-snapshot".to_string()),
            base_url: server.uri(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::from([(
                "chatgpt-account-id".to_string(),
                "stale-codex-account".to_string(),
            )]),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: Some(scoped),
        };
        let client = WebSearchClient::new(&config, Some(default)).expect("client should build");
        let error = client.search("test query", None).await.unwrap_err();
        assert!(
            error.to_string().contains("credential is unavailable"),
            "unexpected error: {error}"
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "a missing scoped credential must not fall back to the session-wide bearer"
        );
    }

    #[tokio::test]
    async fn provider_scoped_search_does_not_follow_same_origin_redirect() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer codex-key",
            ))
            .and(wiremock::matchers::header(
                "chatgpt-account-id",
                "codex-account",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(307).insert_header("Location", "/outside-codex"),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/outside-codex"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let scoped: SharedApiKeyProvider = std::sync::Arc::new(ScopedProvider);
        let config = WebSearchConfig::Enabled {
            api_key: Some("stale-codex-key".to_string()),
            base_url: server.uri(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: Some(scoped),
        };
        let client = WebSearchClient::new(&config, None).expect("client should build");
        let error = client.search("test query", None).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 307"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/outside-codex")
                .count(),
            0,
            "provider-scoped credentials must not follow redirects outside the fixed path"
        );
    }

    /// A static (build-time) key has no dynamic provider, so the builder's
    /// dynamic-credential redirect policy never installs. The fallback must
    /// be Policy::none(): a 307 is surfaced, never followed with the bearer.
    #[tokio::test]
    async fn static_key_search_does_not_follow_redirect() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(
                wiremock::ResponseTemplate::new(307).insert_header("Location", "/elsewhere"),
            )
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/elsewhere"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let config = WebSearchConfig::Enabled {
            api_key: Some("static-key".to_string()),
            base_url: server.uri(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: None,
        };
        let client = WebSearchClient::new(&config, None).expect("client should build");
        let error = client.search("test query", None).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 307"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/elsewhere")
                .count(),
            0,
            "static-key clients must not follow redirects"
        );
    }

    #[tokio::test]
    async fn provider_scoped_search_does_not_follow_cross_origin_redirect() {
        let sink = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&sink)
            .await;
        let source = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/responses"))
            .respond_with(
                wiremock::ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/capture", sink.uri())),
            )
            .mount(&source)
            .await;

        let scoped: SharedApiKeyProvider = std::sync::Arc::new(ScopedProvider);
        let config = WebSearchConfig::Enabled {
            api_key: Some("stale-codex-key".to_string()),
            base_url: source.uri(),
            model: "test-model".to_string(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: Some(scoped),
        };
        let client = WebSearchClient::new(&config, None).expect("client should build");
        let error = client.search("test query", None).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 307"));
        assert!(
            sink.received_requests().await.unwrap().is_empty(),
            "provider-scoped credential metadata must not cross an origin redirect"
        );
    }

    #[derive(Debug)]
    struct RotatingProvider {
        values: std::sync::Mutex<std::collections::VecDeque<Option<String>>>,
    }
    impl crate::types::ApiKeyProvider for RotatingProvider {
        fn current_api_key(&self) -> Option<String> {
            self.values.lock().unwrap().pop_front().flatten()
        }
    }

    fn assert_secret_absent(text: &str, secret: &str) {
        assert!(!text.contains(secret), "full credential leaked: {text}");
        for window in secret.as_bytes().windows(8) {
            let part = std::str::from_utf8(window).unwrap();
            assert!(!text.contains(part), "credential fragment leaked: {text}");
        }
    }

    #[tokio::test]
    async fn auth_failure_uses_final_wire_credential_and_redacts_provider_body() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let sent = "sent-credential-0123456789";
        let current = "rotated-credential-9876543210";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("Authorization", format!("Bearer {sent}")))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string(format!("provider echoed {sent} and {}", &sent[5..17])),
            )
            .mount(&server)
            .await;

        let provider: SharedApiKeyProvider = std::sync::Arc::new(RotatingProvider {
            values: std::sync::Mutex::new(std::collections::VecDeque::from([
                Some(sent.to_string()),
                Some(current.to_string()),
            ])),
        });
        let callback = std::sync::Arc::new(CountingCallback::default());
        let config = WebSearchConfig::Enabled {
            api_key: Some("static-fallback".into()),
            base_url: server.uri(),
            model: "test-model".into(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: Some(provider),
        };
        let client = WebSearchClient::new(&config, None)
            .unwrap()
            .with_attribution_callback(Some(callback.clone()));

        let error = client.search("query", None).await.unwrap_err().to_string();
        assert_secret_absent(&error, sent);
        let calls = callback.invocations.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, ToolConsumer::WebSearch);
        assert_eq!(
            calls[0].1,
            xai_grok_auth::CredentialComparison::different_from_current()
        );
    }

    #[tokio::test]
    async fn transport_failure_never_exposes_credential_bearing_url() {
        let secret = "ZXCVBNMASDFGHJKL0123456789";
        let config = WebSearchConfig::Enabled {
            api_key: Some("api-key".into()),
            base_url: format!("http://127.0.0.1:0/path/{secret}?token={secret}"),
            model: "test-model".into(),
            extra_headers: IndexMap::new(),
            env_http_headers: Default::default(),
            alpha_test_key: None,
            api_key_provider: None,
        };
        let client = WebSearchClient::new(&config, None).unwrap();
        let error = client.search("query", None).await.unwrap_err().to_string();
        assert_secret_absent(&error, secret);
        assert!(error.contains("transport failed"), "got: {error}");
    }
    #[test]
    fn test_extract_citations_no_annotations() {
        let response = response_from_json(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 1234567890,
            "status": "completed",
            "model": "test-model",
            "output": [
                {
                    "type": "message",
                    "id": "msg_1",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "Plain text with no annotations",
                            "annotations": []
                        }
                    ]
                }
            ]
        }));
        let citations = extract_citations(&response);
        assert!(citations.is_empty());
    }
}
