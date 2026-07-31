#[derive(Clone, Debug)]
struct SamplerConfig {
    api_key: Option<String>,
}

#[derive(Debug)]
struct SamplingConfig {
    api_key: Option<String>,
}

#[derive(Debug)]
struct GrokComConfig {
    auth_provider_command: Option<String>,
}

#[derive(Debug)]
pub struct ModelEntryConfig {
    api_key: Option<String>,
}

#[derive(Debug)]
struct ModelsConfig {
    extra_headers: Vec<(String, String)>,
}

#[derive(Debug)]
struct RemoteConfig {
    secret: Option<String>,
}

#[derive(Debug)]
struct ModelInfo {
    base_url: String,
}

#[derive(Debug)]
struct ModelEntry {
    api_key: Option<String>,
}

#[derive(Debug)]
struct OtelExporterConfig {
    headers: Vec<(String, String)>,
}

#[derive(Debug)]
struct ManagedMcpConfig {
    endpoint: String,
    headers: std::collections::HashMap<String, String>,
}

#[derive(Debug)]
struct MultipartInitResponse {
    upload_id: String,
    part_urls: Vec<SignedPartUrl>,
}

#[derive(Debug)]
struct SignedPartUrl {
    url: String,
}

#[derive(Debug)]
struct ExternalOtelConfig {
    logs_endpoint: String,
    logs_headers: Vec<(String, String)>,
}

#[derive(Debug)]
struct ExternalOtelFileConfig {
    endpoint: Option<String>,
}

#[derive(Debug)]
struct TelemetryConfig {
    events_api_key: Option<String>,
    mixpanel_token: Option<String>,
}

#[derive(Debug)]
struct McpOAuthConfig {
    client_secret: Option<String>,
}

#[derive(Debug)]
struct ServeArgs {
    secret: Option<String>,
}

#[derive(Debug)]
struct DeploymentConfig {
    deployment_key: Option<String>,
}

#[derive(Debug)]
struct AlphaTestConfig {
    alpha_test_key: Option<String>,
}

#[derive(Debug)]
struct ServiceAccountConfig {
    service_account_key: Option<String>,
}

#[derive(Debug)]
struct PrivateKeyConfig {
    private_key: Option<String>,
}

#[derive(Debug)]
struct AuthorizationConfig {
    authorization: Option<String>,
}
