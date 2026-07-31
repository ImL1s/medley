//! OAuth configuration types for MCP servers.
//!
//! Constructed by the host's TOML parsing (`McpServerConfig::oauth_config`)
//! and consumed by [`crate::oauth`].

use std::collections::HashMap;

/// OAuth configuration extracted from an MCP server's config.
///
/// Travels alongside `acp::McpServer` (which can't be extended since it's
/// an external crate type). Keyed by server name in [`McpOAuthConfigMap`].
#[derive(Clone, Default)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub callback_port: Option<u16>,
}

impl std::fmt::Debug for McpOAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOAuthConfig")
            .field("client_id_present", &self.client_id.is_some())
            .field("client_secret_present", &self.client_secret.is_some())
            .field("scope_count", &self.scopes.as_ref().map_or(0, Vec::len))
            .field("callback_port_present", &self.callback_port.is_some())
            .finish()
    }
}

impl McpOAuthConfig {
    pub fn is_configured(&self) -> bool {
        self.client_id.is_some()
    }
}

/// Per-server OAuth configuration map, keyed by MCP server name.
pub type McpOAuthConfigMap = HashMap<String, McpOAuthConfig>;

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn oauth_config_debug_is_presence_only() {
        const CLIENT_ID: &str = "GB002CLIENT-Q7w5E3r1T9y7Z6x4C2v8";
        const CLIENT_SECRET: &str = "GB002SECRET-A7s5D3f1G9h7J6k4L2m8";
        const SCOPE: &str = "GB002SCOPE-U7i5O3p1A9s7D6f4G2h8";
        let config = McpOAuthConfig {
            client_id: Some(CLIENT_ID.to_owned()),
            client_secret: Some(CLIENT_SECRET.to_owned()),
            scopes: Some(vec![SCOPE.to_owned()]),
            callback_port: Some(24_019),
        };

        let output = format!("{config:?}");
        for sentinel in [CLIENT_ID, CLIENT_SECRET, SCOPE] {
            assert_no_secret_fragments(&output, sentinel);
        }
        assert!(output.contains("client_id_present: true"));
        assert!(output.contains("client_secret_present: true"));
        assert!(output.contains("scope_count: 1"));
        assert!(output.contains("callback_port_present: true"));
    }
}
