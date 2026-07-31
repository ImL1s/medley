/// Structured errors for the `web_fetch` tool.
use std::net::IpAddr;

#[derive(Debug, thiserror::Error)]
pub enum WebFetchError {
    #[error("URL exceeds maximum length of {max} characters")]
    UrlTooLong { max: usize },

    #[error("unsupported URL scheme: {scheme} (only http/https allowed)")]
    UnsupportedScheme { scheme: String },

    #[error("URLs with embedded credentials are not allowed")]
    CredentialsInUrl,

    #[error("hostname must have at least two dot-separated parts, got: {host}")]
    SingleLabelHost { host: String },

    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    #[error("SSRF blocked: {host} resolves to private/internal IP {ip}{}", ssrf_recovery_hint(.host))]
    SsrfBlocked { host: String, ip: IpAddr },

    #[error("DNS resolution failed for {host}: {source}")]
    DnsResolution {
        host: String,
        source: std::io::Error,
    },

    #[error("DNS resolution returned no addresses for {0}")]
    DnsEmpty(String),

    #[error("failed to build HTTP client")]
    ClientBuildError,

    #[error("HTTP request failed")]
    HttpRequest(#[source] reqwest::Error),

    #[error("invalid redirect URL")]
    InvalidRedirect,

    #[error("too many redirects (max {max})")]
    TooManyRedirects { max: usize },

    #[error("response body exceeds maximum size of {max} bytes")]
    ResponseTooLarge { max: usize },

    #[error("invalid proxy configuration")]
    ProxyConfigError,

    #[error("failed to save downloaded file: {0}")]
    IoError(#[from] std::io::Error),

    #[error("unsupported content type {content_type}")]
    UnsupportedContentType { content_type: String },

    #[error("content body does not match claimed content type {content_type}")]
    ContentTypeMismatch { content_type: String },
}

impl From<reqwest::Error> for WebFetchError {
    fn from(error: reqwest::Error) -> Self {
        Self::HttpRequest(error.without_url())
    }
}

/// Extra recovery guidance appended to an [`WebFetchError::SsrfBlocked`] message.
///
/// `web_fetch` can't reach internal/private hosts, but GitHub / GitHub
/// Enterprise hosts (including internal GHE hostnames) are reachable via the
/// authenticated `gh` CLI. When the blocked host looks like GitHub **and `gh`
/// is actually installed**, point the agent at `gh` instead of letting it
/// conclude the resource is inaccessible and give up. If `gh` is not on `PATH`
/// (or the host isn't GitHub), fall back to the bare SSRF message by returning
/// an empty string.
fn ssrf_recovery_hint(host: &str) -> &'static str {
    if is_github_host(host) && gh_available() {
        ". Use the `gh` CLI instead (e.g. `gh pr view` or `gh api`)."
    } else {
        ""
    }
}

/// Whether `host` is a GitHub / GitHub Enterprise host (one `gh` can reach).
fn is_github_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "github.com" || h.ends_with(".github.com") || h.contains("github")
}

/// Whether the `gh` CLI is available on `PATH`, via the same `which` lookup the
/// rest of the codebase uses for binary discovery (e.g. `xai-grok-mcp`).
fn gh_available() -> bool {
    which::which("gh").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_no_secret_fragments(output: &str, secret: &str) {
        assert!(!output.contains(secret), "full credential leaked: {output}");
        for window in secret.as_bytes().windows(8) {
            let fragment = std::str::from_utf8(window).expect("ASCII test sentinel");
            assert!(
                !output.contains(fragment),
                "credential fragment {fragment:?} leaked: {output}"
            );
        }
    }

    #[tokio::test]
    async fn request_error_omits_url_credentials() {
        let sentinel = "cred_SENTINEL_0123456789abcdef";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("http://user:{sentinel}@127.0.0.1:{port}/resource?token={sentinel}");
        let raw = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .expect_err("closed listener must reject request");
        let error = WebFetchError::from(raw);
        assert_no_secret_fragments(&error.to_string(), sentinel);
        assert_no_secret_fragments(&format!("{error:?}"), sentinel);
    }

    #[test]
    fn github_host_detection() {
        assert!(is_github_host("github.com"));
        assert!(is_github_host("api.github.com"));
        assert!(is_github_host("github.ghe.example.com")); // synthetic GHE-style
        assert!(!is_github_host("ghe.example.com"));
        assert!(!is_github_host("internal-wiki.corp.example.com"));
        assert!(!is_github_host("gitlab.example.com"));
    }

    #[test]
    fn which_detects_gh_in_dir() {
        // Exercises the same `which` lookup `gh_available` uses, with a
        // controlled search dir so it doesn't depend on the test host's PATH.
        let dir = tempfile::tempdir().unwrap();
        // No gh in this dir yet.
        assert!(which::which_in("gh", Some(dir.path()), dir.path()).is_err());
        // Create an executable `gh`.
        let gh = dir.path().join("gh");
        std::fs::write(&gh, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(which::which_in("gh", Some(dir.path()), dir.path()).is_ok());
    }

    #[test]
    fn ssrf_non_github_host_never_hints() {
        let err = WebFetchError::SsrfBlocked {
            host: "internal-wiki.corp.example.com".to_string(),
            ip: "10.0.0.5".parse().unwrap(),
        };
        let msg = err.to_string();
        assert!(msg.contains("resolves to private/internal IP 10.0.0.5"));
        assert!(
            !msg.contains("gh"),
            "non-github host should not mention gh: {msg}"
        );
    }

    #[test]
    fn ssrf_github_host_hint_follows_gh_availability() {
        let err = WebFetchError::SsrfBlocked {
            // Synthetic host must contain "github" for is_github_host; IP is RFC1918 example.
            host: "github.ghe.example.com".to_string(),
            ip: "10.0.0.1".parse().unwrap(),
        };
        let msg = err.to_string();
        assert!(msg.contains("resolves to private/internal IP 10.0.0.1"));
        if gh_available() {
            assert!(msg.contains("`gh` CLI"), "gh present -> should hint: {msg}");
            assert!(msg.contains("gh pr view") && msg.contains("gh api"));
        } else {
            // Host names like "github…" contain the substring "gh"; assert on the
            // hint markers only, not a bare "gh" contains check.
            assert!(
                !msg.contains("`gh` CLI") && !msg.contains("gh pr view") && !msg.contains("gh api"),
                "gh absent -> previous behavior, no hint: {msg}"
            );
        }
    }
}
