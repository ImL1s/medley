//! URL helpers aligned with `xai-grok-shell-base::util` for first-party detection.

/// True when `candidate` matches `trusted_base` (scheme, host, port, path prefix).
fn matches_trusted_base_url(candidate: &str, trusted_base: &str) -> bool {
    let Ok(candidate) = reqwest::Url::parse(candidate) else {
        return false;
    };
    let Ok(trusted) = reqwest::Url::parse(trusted_base) else {
        return false;
    };
    let trusted_path = trusted.path();
    let candidate_path = candidate.path();
    let path_matches = candidate_path == trusted_path
        || candidate_path
            .strip_prefix(trusted_path)
            .is_some_and(|suffix| suffix.starts_with('/'));
    candidate.scheme() == trusted.scheme()
        && candidate.host_str() == trusted.host_str()
        && candidate.port_or_known_default() == trusted.port_or_known_default()
        && path_matches
}

/// True for cli-chat-proxy URLs and loopback (local mock servers).
fn is_cli_chat_proxy_url(url: &str) -> bool {
    if matches_trusted_base_url(url, xai_grok_env::PROD_CLI_CHAT_PROXY_BASE_URL) {
        return true;
    }
    if let Ok(u) = reqwest::Url::parse(url)
        && let Some(h) = u.host_str()
        && (h == "localhost" || h == "127.0.0.1" || h == "::1")
    {
        return true;
    }
    false
}

/// True for xAI-operated endpoints (`*.x.ai`, cli-chat-proxy, loopback mocks).
pub(crate) fn is_xai_api_url(url: &str) -> bool {
    if is_cli_chat_proxy_url(url) {
        return true;
    }
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|host| host == "x.ai" || host.ends_with(".x.ai"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_xai_api_url_accepts_xai_hosts_and_rejects_third_party() {
        assert!(is_xai_api_url("https://api.x.ai/v1"));
        assert!(is_xai_api_url("http://127.0.0.1:11434/v1"));
        assert!(!is_xai_api_url("https://api.openai.com/v1"));
        assert!(!is_xai_api_url("https://api.anthropic.com/v1"));
    }
}
