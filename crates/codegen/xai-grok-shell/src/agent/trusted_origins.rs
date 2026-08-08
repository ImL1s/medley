//! #123: user-declared trusted origins for ambient xAI credentials.
//!
//! The #110 origin gate binds an ambient xAI credential (session token,
//! `XAI_API_KEY`) to first-party origins, recomputed from the URL. That is the
//! correct default — but it leaves a user fronting xAI with their own gateway
//! no supported way to say "this origin is mine". This module is that way:
//! an exact-origin allowlist declared in **local, user-tier config only**.
//!
//! ```toml
//! # ~/.medley/config.toml (or a managed config layer)
//! trusted_xai_origins = ["https://gateway.internal", "https://gateway.internal:8443"]
//! ```
//!
//! The constraints are the feature:
//!
//! - **Local and explicit.** The list is read from the raw local disk layers
//!   (`system_managed`, `managed`, `user`) of [`crate::config::ConfigLayers`]
//!   and nowhere else. It is never read from the merged effective config, so
//!   server-synced `requirements.toml` and remote `[[campaigns]]` cannot inject
//!   it; project `.grok/config.toml` never enters `ConfigLayers` at all (it
//!   contributes MCP servers, plugins, and permissions only — see
//!   [`crate::config::PROJECT_INERT_MODEL_SECTIONS`], which reports this key
//!   as inert when written there). There is deliberately no env-var form:
//!   `.envrc` arrives with a cloned repo, so an env-declared trust origin is
//!   not reliably the user's own decision.
//! - **https only.** Non-https entries are rejected at parse, the matcher
//!   requires an https candidate, and the sampler's L3 re-computes
//!   `scheme == "https"` from the URL before honouring the label.
//! - **Exact origin.** Scheme + host + effective port. No suffix or wildcard
//!   matching; declaring `https://gateway.internal` does not trust
//!   `https://evil.gateway.internal` or `https://gateway.internal:8443`.
//! - **Narrow grant.** A matched origin is emitted as
//!   [`xai_grok_sampler::EndpointTrustClass::UserDeclared`]: the ambient
//!   credential may flow and the session bearer may refresh, but xAI identity
//!   headers stay off and the external metadata boundary keeps stripping
//!   `x-grok-*` / `x-xai-*`. The user trusted the origin with a credential,
//!   not with their account identity.
//! - **Fail closed.** Absent, malformed, or non-matching declarations change
//!   nothing; there is no fallback classifier. Rejected entries warn with the
//!   reason and a sanitized rendering — never the raw value, which can be
//!   credential-shaped.
//!
//! Private CA: TLS verification is not relaxed. A gateway serving a
//! private-CA certificate additionally needs `GROK_EXTRA_CA_BUNDLE` pointing
//! at the CA PEM (`xai-grok-extra-ca`).

use std::sync::atomic::{AtomicU64, Ordering};

/// The top-level config key. Top-level (not per-model) because the declaration
/// is a property of the *origin*, not of one model entry.
pub(crate) const TRUSTED_XAI_ORIGINS_KEY: &str = "trusted_xai_origins";

/// One validated declaration: the https origin's normalized (host, effective
/// port). The scheme is not stored — entries are https by construction and the
/// matcher refuses non-https candidates, so a declaration can never lend trust
/// to a cleartext route.
#[derive(Clone, PartialEq, Eq, Hash)]
struct DeclaredOrigin {
    host: String,
    port: u16,
}

impl std::fmt::Debug for DeclaredOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display())
    }
}

impl DeclaredOrigin {
    /// `https://host`, with `:port` only when non-default. Origins are not
    /// credentials (userinfo/query/fragment are rejected at parse), so this
    /// is safe to log and to show in `grok inspect`.
    fn display(&self) -> String {
        if self.port == 443 {
            format!("https://{}", self.host)
        } else {
            format!("https://{}:{}", self.host, self.port)
        }
    }
}

/// The parsed allowlist plus the entries that were rejected (with reasons, for
/// the load-time warning and `grok inspect`). `Default` is the empty list —
/// the fail-closed value every "config unreadable" path must produce.
#[derive(Debug, Clone, Default)]
pub(crate) struct TrustedXaiOrigins {
    origins: Vec<DeclaredOrigin>,
    /// `(sanitized rendering, why it was refused)` per rejected entry.
    rejected: Vec<(String, &'static str)>,
}

impl TrustedXaiOrigins {
    /// Read the declaration from the local disk layers of `layers` —
    /// `system_managed`, `managed`, and `user`, in that order. Requirements
    /// and campaign layers are *not consulted*: those are server-synced /
    /// remote-influenced, and a trust decision that arrives over the network
    /// is not the user's local act.
    pub(crate) fn from_config_layers(layers: &crate::config::ConfigLayers) -> Self {
        let mut out = Self::default();
        for layer in [&layers.system_managed, &layers.managed, &layers.user] {
            out.absorb_layer(layer);
        }
        out
    }

    /// Load the current declaration from disk. Any layer-load failure fails
    /// closed (empty list) with a warning — a half-read config must not
    /// quietly widen or silently deny; the strip side is the safe side.
    pub(crate) fn load() -> Self {
        match crate::config::ConfigLayers::load() {
            Ok(layers) => {
                let trusted = Self::from_config_layers(&layers);
                trusted.warn_once();
                trusted
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "trusted_xai_origins: config layers unreadable; no origin is trusted"
                );
                Self::default()
            }
        }
    }

    /// True when `url`'s origin was declared. The candidate must itself be
    /// https — the declaration names https origins, so a cleartext candidate
    /// can never match, even on the same host and port.
    pub(crate) fn is_trusted(&self, url: &str) -> bool {
        let Ok(parsed) = reqwest::Url::parse(url) else {
            return false;
        };
        if parsed.scheme() != "https" {
            return false;
        }
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let port = parsed.port_or_known_default().unwrap_or(443);
        self.origins
            .iter()
            .any(|origin| origin.host == host && origin.port == port)
    }

    /// Declared origins in display form (`https://host[:port]`), for
    /// `grok inspect` and the load-time warning.
    pub(crate) fn declared_display(&self) -> Vec<String> {
        self.origins.iter().map(|o| o.display()).collect()
    }

    /// `(sanitized entry, reason)` pairs for entries that were refused.
    pub(crate) fn rejected(&self) -> &[(String, &'static str)] {
        &self.rejected
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.origins.is_empty()
    }

    fn absorb_layer(&mut self, layer: &toml::Value) {
        let Some(value) = layer.get(TRUSTED_XAI_ORIGINS_KEY) else {
            return;
        };
        let Some(entries) = value.as_array() else {
            self.rejected.push((
                String::from(TRUSTED_XAI_ORIGINS_KEY),
                "must be an array of origin strings",
            ));
            return;
        };
        for entry in entries {
            let Some(raw) = entry.as_str() else {
                self.rejected
                    .push((String::from("<non-string entry>"), "not a string"));
                continue;
            };
            match parse_entry(raw) {
                Ok(origin) => {
                    if !self.origins.contains(&origin) {
                        self.origins.push(origin);
                    }
                }
                Err(reason) => self.rejected.push((sanitize_entry(raw), reason)),
            }
        }
    }

    /// Log the active declaration and every rejection, once per process per
    /// distinct content (the same hash-dedup pattern as
    /// `warn_inert_project_model_sections`). A user who widened their
    /// credential's blast radius must be able to see that they did.
    fn warn_once(&self) {
        if self.origins.is_empty() && self.rejected.is_empty() {
            return;
        }
        use std::hash::{Hash as _, Hasher as _};
        static LAST_LOGGED: AtomicU64 = AtomicU64::new(0);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.origins.hash(&mut hasher);
        self.rejected.hash(&mut hasher);
        let hash = hasher.finish().max(1);
        if LAST_LOGGED.swap(hash, Ordering::Relaxed) == hash {
            return;
        }
        for origin in &self.origins {
            tracing::warn!(
                origin = %origin.display(),
                "trusted_xai_origins: ambient xAI credentials (session token, XAI_API_KEY) \
                 will be sent to this user-declared origin"
            );
        }
        for (entry, reason) in &self.rejected {
            tracing::warn!(
                entry = %entry,
                reason = *reason,
                "trusted_xai_origins: entry ignored"
            );
        }
    }
}

/// Validate and normalize one entry. Rejections name the rule, never the
/// value. A path is normalized away (matching is per-origin); userinfo, query,
/// and fragment are refused outright — that is where a URL hides a credential.
fn parse_entry(raw: &str) -> Result<DeclaredOrigin, &'static str> {
    let url = reqwest::Url::parse(raw).map_err(|_| "not a valid URL")?;
    if url.scheme() != "https" {
        return Err("scheme must be https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("userinfo is not allowed in a trust declaration");
    }
    if url.query().is_some() {
        return Err("query strings are not allowed in a trust declaration");
    }
    if url.fragment().is_some() {
        return Err("fragments are not allowed in a trust declaration");
    }
    let Some(host) = url.host_str() else {
        return Err("no host");
    };
    Ok(DeclaredOrigin {
        host: host.to_owned(),
        port: url.port_or_known_default().unwrap_or(443),
    })
}

/// Render a rejected entry with every place a credential could hide stripped
/// (userinfo, query, fragment). Unparseable entries render as a placeholder.
fn sanitize_entry(raw: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return String::from("<unparseable entry>");
    };
    url.set_username("").ok();
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// Warn once per origin per process when a declaration actually changes an
/// outcome — an ambient xAI credential is being forwarded to a user-declared
/// origin that the #110 gate would otherwise have stripped it from. A warning
/// that fires on every turn is decorative; once per origin keeps it a signal.
pub(crate) fn warn_declared_origin_in_use_once(base_url: &str) {
    use std::sync::{Mutex, OnceLock};
    static WARNED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let warned = WARNED.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    let origin = sanitize_entry(base_url);
    let Ok(mut warned) = warned.lock() else {
        return;
    };
    if warned.insert(origin.clone()) {
        tracing::warn!(
            origin = %origin,
            "trusted_xai_origins: forwarding the ambient xAI credential to this \
             user-declared origin (declared in local config; remove the \
             trusted_xai_origins entry to revoke)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers_with(key_owner: &str, entry: &str) -> crate::config::ConfigLayers {
        let value: toml::Value =
            toml::from_str(&format!("{TRUSTED_XAI_ORIGINS_KEY} = [{entry:?}]"))
                .expect("test toml parses");
        let mut layers = crate::config::ConfigLayers::default();
        match key_owner {
            "user" => layers.user = value,
            "managed" => layers.managed = value,
            "system_managed" => layers.system_managed = value,
            "user_requirements" => layers.user_requirements = Some(value),
            "system_requirements" => layers.system_requirements = Some(value),
            "mdm_requirements" => layers.mdm_requirements = Some(value),
            other => panic!("unknown test layer {other}"),
        }
        layers
    }

    #[test]
    fn trusted_xai_origins_parse_normalizes_origin() {
        let layers = layers_with("user", "https://GATEWAY.internal/v1/chat");
        let trusted = TrustedXaiOrigins::from_config_layers(&layers);
        assert_eq!(trusted.declared_display(), vec!["https://gateway.internal"]);
        assert!(trusted.rejected().is_empty());
        assert!(trusted.is_trusted("https://gateway.internal/v1"));
        assert!(trusted.is_trusted("https://gateway.internal"));
        assert!(trusted.is_trusted("https://gateway.internal:443/v1"));
    }

    #[test]
    fn trusted_xai_origins_parse_rejects_non_https_and_credential_shaped_entries() {
        for (raw, reason) in [
            ("http://gateway.internal", "scheme must be https"),
            (
                "https://user:pw@gateway.internal",
                "userinfo is not allowed in a trust declaration",
            ),
            (
                "https://gateway.internal?api_key=x",
                "query strings are not allowed in a trust declaration",
            ),
            (
                "https://gateway.internal#frag",
                "fragments are not allowed in a trust declaration",
            ),
            ("not a url", "not a valid URL"),
        ] {
            let layers = layers_with("user", raw);
            let trusted = TrustedXaiOrigins::from_config_layers(&layers);
            assert!(trusted.is_empty(), "{raw} must not be trusted");
            assert_eq!(trusted.rejected().len(), 1, "{raw}");
            assert_eq!(trusted.rejected()[0].1, reason, "{raw}");
            assert!(
                !trusted.is_trusted("https://gateway.internal"),
                "a rejected entry must not accidentally match: {raw}"
            );
        }
    }

    #[test]
    fn trusted_xai_origins_rejected_rendering_strips_credential_hiding_places() {
        let layers = layers_with(
            "user",
            "https://user:S3CRET@gateway.internal/v1?api_key=S3CRET#S3CRET",
        );
        let trusted = TrustedXaiOrigins::from_config_layers(&layers);
        assert_eq!(trusted.rejected().len(), 1);
        let rendered = &trusted.rejected()[0].0;
        assert!(
            !rendered.contains("S3CRET"),
            "rejection rendering must not carry credential-shaped content: {rendered}"
        );
    }

    #[test]
    fn trusted_xai_origins_ignored_in_untrusted_layers() {
        // The declaration is honoured from each local disk layer...
        for owner in ["user", "managed", "system_managed"] {
            let trusted = TrustedXaiOrigins::from_config_layers(&layers_with(
                owner,
                "https://gateway.internal",
            ));
            assert!(
                trusted.is_trusted("https://gateway.internal/v1"),
                "{owner} layer must be honoured"
            );
        }
        // ...and is invisible from the server-synced / MDM requirements
        // layers, which merge into the effective config but are not local
        // trust decisions.
        for owner in [
            "user_requirements",
            "system_requirements",
            "mdm_requirements",
        ] {
            let trusted = TrustedXaiOrigins::from_config_layers(&layers_with(
                owner,
                "https://gateway.internal",
            ));
            assert!(
                trusted.is_empty(),
                "{owner} must not be able to declare a trusted origin"
            );
            assert!(
                !trusted.is_trusted("https://gateway.internal/v1"),
                "{owner} must not be able to declare a trusted origin"
            );
        }
    }

    #[test]
    fn trusted_xai_origins_matches_exact_origin_only() {
        let layers = layers_with("user", "https://gateway.internal");
        let trusted = TrustedXaiOrigins::from_config_layers(&layers);
        for candidate in [
            "https://gateway.internal.evil.example/v1", // suffix attack
            "https://evil-gateway.internal/v1",
            "https://sub.gateway.internal/v1", // subdomain is a different origin
            "https://gateway.internal:8443/v1", // non-default port is a different origin
            "http://gateway.internal/v1",      // cleartext never matches
        ] {
            assert!(
                !trusted.is_trusted(candidate),
                "{candidate} must not be trusted by a declaration of https://gateway.internal"
            );
        }
        // A declaration on a non-default port matches that port only.
        let layers = layers_with("user", "https://gateway.internal:8443");
        let trusted = TrustedXaiOrigins::from_config_layers(&layers);
        assert!(trusted.is_trusted("https://gateway.internal:8443/v1"));
        assert!(!trusted.is_trusted("https://gateway.internal/v1"));
    }

    #[test]
    fn trusted_xai_origins_layers_union_and_dedupe() {
        let mut layers = layers_with("user", "https://gateway.internal");
        layers.managed = toml::from_str(&format!(
            "{TRUSTED_XAI_ORIGINS_KEY} = [\"https://gateway.internal\", \"https://ops.internal:8443\"]"
        ))
        .expect("test toml parses");
        let trusted = TrustedXaiOrigins::from_config_layers(&layers);
        assert_eq!(
            trusted.declared_display(),
            vec!["https://gateway.internal", "https://ops.internal:8443"],
            "system_managed, then managed, then user, deduped"
        );
    }
}
