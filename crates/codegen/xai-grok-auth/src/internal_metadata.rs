//! First-party request-metadata names that must not leave a trusted xAI origin.
//!
//! Shared by the sampler and web-search transports so the denylist cannot
//! drift. [`HeaderName`] comparison is case-insensitive; names are still
//! lowercased here so raw env/config keys cannot bypass the check.

use reqwest::header::{HeaderMap, HeaderName};

/// True when `name` is first-party identity, routing, tracing, or compaction
/// metadata. Comparison is ASCII case-insensitive.
pub fn is_internal_metadata_header_name(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    name.starts_with("x-grok-")
        || name.starts_with("x-xai-")
        || name == "x-compactions-remaining"
        || name == "x-compaction-at"
        || name == "x-authenticateresponse"
        || name == "traceparent"
        || name == "tracestate"
        || name == "baggage"
}

/// [`HeaderName`] form of [`is_internal_metadata_header_name`].
pub fn is_internal_metadata_header(name: &HeaderName) -> bool {
    is_internal_metadata_header_name(name.as_str())
}

/// Remove every internal-metadata header from `headers`.
pub fn strip_internal_metadata_headers(headers: &mut HeaderMap) {
    let names: Vec<HeaderName> = headers
        .keys()
        .filter(|name| is_internal_metadata_header(name))
        .cloned()
        .collect();
    for name in names {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn mixed_case_names_cannot_bypass_classification() {
        for name in [
            "X-Grok-Client-Version",
            "x-GROK-user-id",
            "X-XAI-Token-Auth",
            "Traceparent",
            "TRACESTATE",
            "Baggage",
            "X-Compactions-Remaining",
            "X-Compaction-At",
            "X-AuthenticateResponse",
        ] {
            assert!(
                is_internal_metadata_header_name(name),
                "raw name must classify as internal: {name}"
            );
            let header = HeaderName::from_bytes(name.as_bytes()).expect("valid header name");
            assert!(
                is_internal_metadata_header(&header),
                "HeaderName must classify as internal: {name}"
            );
        }
    }

    #[test]
    fn strip_removes_mixed_case_inserts_and_keeps_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_bytes(b"X-Grok-Client-Version").unwrap(),
            HeaderValue::from_static("9.9.9"),
        );
        headers.insert(
            HeaderName::from_bytes(b"X-XAI-Token-Auth").unwrap(),
            HeaderValue::from_static("xai-grok-cli"),
        );
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer keep"),
        );
        headers.insert(
            HeaderName::from_static("x-provider-key"),
            HeaderValue::from_static("configured"),
        );
        strip_internal_metadata_headers(&mut headers);
        assert!(headers.get("x-grok-client-version").is_none());
        assert!(headers.get("x-xai-token-auth").is_none());
        assert_eq!(headers.get("authorization").unwrap(), "Bearer keep");
        assert_eq!(headers.get("x-provider-key").unwrap(), "configured");
    }
}
