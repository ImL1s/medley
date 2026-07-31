//! Shared parser for an auth command's stdout.
//!
//! Both auth paths run a command that prints a bearer token and parse it here:
//! the session external-auth path ([`super::external_auth`]) and the per-model
//! provider mint ([`super::auth_provider`]).

#[derive(serde::Deserialize)]
pub(crate) struct ExternalAuthOutput {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    /// An xAI issuer marks the credential as first-party
    /// (see [`crate::auth::GrokAuth::is_xai_auth`]).
    #[serde(default)]
    pub issuer: Option<String>,
}

/// A bearer must be a single line: reject control characters (including an
/// interior newline) so a malformed token can never be smuggled onto an HTTP
/// header, rather than relying on the HTTP layer to reject it later.
fn reject_control_chars(token: &str) -> anyhow::Result<()> {
    if token.contains(char::is_control) {
        anyhow::bail!("token contains control characters");
    }
    Ok(())
}

/// `now + secs`, or `None` on overflow.
pub(crate) fn expiry_after_seconds(secs: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    let secs = i64::try_from(secs).ok()?;
    chrono::Utc::now().checked_add_signed(chrono::Duration::try_seconds(secs)?)
}

pub(crate) struct ParsedTokenOutput {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub issuer: Option<String>,
}

/// Accepts a bare token or JSON `{access_token, expires_in, issuer, ...}`. A
/// non-zero exit, non-UTF-8 or empty stdout, an empty `access_token`, or
/// JSON-object output that is not a valid token payload are all errors, so a
/// malformed mint fails closed rather than putting garbage on the wire.
pub(crate) fn parse_token_output(
    output: &std::process::Output,
) -> anyhow::Result<ParsedTokenOutput> {
    if !output.status.success() {
        anyhow::bail!("exited with {}", output.status);
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| anyhow::anyhow!("produced non-UTF-8 output on stdout"))?
        .trim();
    if stdout.is_empty() {
        anyhow::bail!("produced no output on stdout");
    }

    // Output that starts with `{` is meant to be a token payload: require it to
    // parse and carry a non-empty access_token. Anything else is a bare token
    // (JWTs and opaque tokens never start with `{`), so an error object like
    // `{"error":"expired"}` can never be mistaken for a bearer.
    if stdout.starts_with('{') {
        let parsed: ExternalAuthOutput = serde_json::from_str(stdout)
            .map_err(|e| anyhow::anyhow!("produced JSON that is not a token payload: {e}"))?;
        let access_token = parsed.access_token.trim().to_owned();
        if access_token.is_empty() {
            anyhow::bail!("produced JSON with an empty access_token");
        }
        reject_control_chars(&access_token)?;
        let issuer = parsed
            .issuer
            .map(|issuer| issuer.trim().to_owned())
            .filter(|issuer| !issuer.is_empty());
        tracing::debug!(
            has_refresh_token = parsed.refresh_token.is_some(),
            expires_in = ?parsed.expires_in,
            issuer_present = issuer.is_some(),
            "auth: parsed external provider output as JSON"
        );
        return Ok(ParsedTokenOutput {
            access_token,
            refresh_token: parsed.refresh_token,
            expires_at: parsed.expires_in.and_then(expiry_after_seconds),
            issuer,
        });
    }

    reject_control_chars(stdout)?;
    tracing::debug!(
        stdout_len = stdout.len(),
        "auth: treating output as bare token"
    );
    Ok(ParsedTokenOutput {
        access_token: stdout.to_owned(),
        refresh_token: None,
        expires_at: None,
        issuer: None,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    struct EventCollector(Arc<Mutex<Vec<String>>>);

    impl<S> Layer<S> for EventCollector
    where
        S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = EventVisitor::default();
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.rendered);
        }
    }

    #[derive(Default)]
    struct EventVisitor {
        rendered: String,
    }

    impl Visit for EventVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write;
            let _ = write!(&mut self.rendered, "{}={value:?};", field.name());
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            use std::fmt::Write;
            let _ = write!(&mut self.rendered, "{}={value};", field.name());
        }
    }

    #[test]
    fn expiry_after_seconds_returns_none_on_overflow() {
        assert_eq!(expiry_after_seconds(u64::MAX), None);
        assert_eq!(expiry_after_seconds(u64::try_from(i64::MAX).unwrap()), None);
        assert!(expiry_after_seconds(3600).is_some());
    }

    /// The provider path reads `refresh_token`, which the bare-token fallback
    /// cannot carry; only JSON output does.
    #[test]
    fn parse_token_output_reads_refresh_token_from_json_only() {
        let ok = |stdout: &str| std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
        };

        let parsed =
            parse_token_output(&ok(r#"{"access_token":"a","refresh_token":"r"}"#)).unwrap();
        assert_eq!(parsed.access_token, "a");
        assert_eq!(parsed.refresh_token.as_deref(), Some("r"));

        assert_eq!(parse_token_output(&ok("bare")).unwrap().refresh_token, None);
    }

    /// JSON-shaped output must be a valid, non-empty token payload; a botched or
    /// error payload fails closed instead of going on the wire as a bearer.
    #[test]
    fn parse_token_output_rejects_invalid_json_payloads() {
        let ok = |stdout: &str| std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
        };

        assert!(parse_token_output(&ok(r#"{"access_token":""}"#)).is_err());
        assert!(parse_token_output(&ok(r#"{"access_token":"   "}"#)).is_err());
        assert!(parse_token_output(&ok(r#"{"error":"expired"}"#)).is_err());
        assert!(parse_token_output(&ok("{not valid json}")).is_err());

        // A JSON payload's access_token is trimmed of surrounding whitespace.
        let parsed = parse_token_output(&ok("{\"access_token\":\" tok \"}")).unwrap();
        assert_eq!(parsed.access_token, "tok");

        // An interior control character is rejected on both paths, so a
        // malformed token can never reach an HTTP header.
        assert!(parse_token_output(&ok("{\"access_token\":\"tok\\ninjected\"}")).is_err());
        assert!(parse_token_output(&ok("tok\ninjected")).is_err());
    }

    #[test]
    fn parse_token_output_debug_reports_only_issuer_presence() {
        const ISSUER_SENTINEL: &str = "GB002ISSUER-Q7w5E3r1T9y7Z6x4C2v8";
        let output = std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: format!(r#"{{"access_token":"a","issuer":"{ISSUER_SENTINEL}"}}"#).into_bytes(),
            stderr: vec![],
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(EventCollector(events.clone()));

        tracing::subscriber::with_default(subscriber, || {
            let parsed = parse_token_output(&output).unwrap();
            assert_eq!(parsed.issuer.as_deref(), Some(ISSUER_SENTINEL));
        });

        let rendered = events.lock().unwrap().join("\n");
        assert!(rendered.contains("issuer_present=true"), "{rendered}");
        assert!(!rendered.contains(ISSUER_SENTINEL), "{rendered}");
        for window in ISSUER_SENTINEL.as_bytes().windows(8) {
            let window = std::str::from_utf8(window).unwrap();
            assert!(
                !rendered.contains(window),
                "issuer window {window:?} leaked in {rendered}"
            );
        }
    }
}
