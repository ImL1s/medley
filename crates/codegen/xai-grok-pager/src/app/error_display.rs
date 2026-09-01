//! User-facing formatting for terminal request / API errors.
//!
//! Turns raw ACP / `RetryState` dumps (`API error (status 500): {"error":…}`)
//! into the same kind of short warning banner used for 401 re-auth.

/// Wire `RetryState::Failed.error_type` values the pager understands: the
/// shell's `SamplingErrorKind::as_str` tags plus its special-cased tags
/// (`context_length`, `legacy_auth`, …). Unknown strings map to
/// [`WireErrorType::Other`] rather than being matched as raw `&str` at call
/// sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireErrorType {
    Auth,
    AuthTransient,
    LegacyAuth,
    ContextLength,
    EncryptedContentMismatch,
    DiskFull,
    Api,
    Http,
    IdleTimeout,
    EmptyResponse,
    Serialization,
    RateLimited,
    MaxTokensTruncation,
    Other,
}

impl WireErrorType {
    pub(crate) fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("auth") => Self::Auth,
            Some("auth_transient") => Self::AuthTransient,
            Some("legacy_auth") => Self::LegacyAuth,
            Some("context_length") => Self::ContextLength,
            Some("encrypted_content_mismatch") => Self::EncryptedContentMismatch,
            Some(s) if s == xai_grok_shell::extensions::notification::DISK_FULL_ERROR_TYPE => {
                Self::DiskFull
            }
            Some("api") => Self::Api,
            Some("http") => Self::Http,
            Some("idle_timeout") => Self::IdleTimeout,
            Some("empty_response") => Self::EmptyResponse,
            Some("serialization") => Self::Serialization,
            Some("rate_limited") => Self::RateLimited,
            Some("max_tokens_truncation") => Self::MaxTokensTruncation,
            _ => Self::Other,
        }
    }
}

/// Clean banner for a terminal request failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormattedRequestFailure {
    pub status: Option<u16>,
    pub headline: String,
    pub detail: String,
}

/// `Headline — detail` (headline alone when there is no detail). Shared with
/// the scrollback block so the two renderings can't drift.
pub(crate) fn banner_message(headline: &str, detail: &str) -> String {
    if detail.is_empty() {
        headline.to_string()
    } else {
        format!("{headline} \u{2014} {detail}")
    }
}

impl FormattedRequestFailure {
    pub(crate) fn message(&self) -> String {
        banner_message(&self.headline, &self.detail)
    }

    pub(crate) fn into_session_event(self) -> crate::scrollback::blocks::SessionEvent {
        crate::scrollback::blocks::SessionEvent::RequestFailed {
            status: self.status,
            headline: self.headline,
            detail: self.detail,
        }
    }
}

/// Compose the shipped TUI banner for a provider / HTTP request failure.
///
/// `raw` may already be [`format_request_failure`] output (`Headline — detail`)
/// or a raw carrier (`Provider request failed (HTTP 400).`, ACP
/// `Internal error: {…}`). Returns `None` for non-request failures so the
/// caller can keep a generic `TurnFailed` marker.
pub(crate) fn compose_typed_provider_failure(
    status: Option<u16>,
    error_type: Option<&str>,
    raw: &str,
) -> Option<FormattedRequestFailure> {
    if let Some(formatted) = split_existing_banner(status, raw) {
        return Some(formatted);
    }
    if !looks_like_typed_provider_failure(status, error_type, raw) {
        return None;
    }
    Some(format_request_failure(status, error_type, raw))
}

/// Our own banner shape: `Headline (NNN) — detail`. Used so PromptResponse
/// (already formatted by `format_acp_error`) is not re-run through classify
/// and does not lose a safe provider reason.
///
/// The headline must be one this module *can emit*, not merely one a status
/// parses out of. A provider-controlled message can carry both features — an
/// em dash and a parseable status — on its own
/// (`Provider request failed (HTTP 400). Invalid parameter — {…}`), and was
/// then copied through verbatim, bypassing the carrier strip, JSON extraction
/// and detail cleanup that keep a raw provider payload out of the banner.
/// Validation asks [`classify`] to rebuild the canonical headline for the
/// parsed status and requires an exact, case-sensitive match: the accepted
/// set is therefore *derived* from [`format_request_failure`] and cannot
/// drift from it. Case matters because [`parse_http_status`] matches its
/// markers case-insensitively, so `bad request (400)` parses but is not
/// something this module ever wrote.
///
/// A canonical headline can still be forged exactly, and the detail half
/// arrives verbatim from its producer either way, so this rail hands it to
/// **the same two functions [`format_request_failure`] uses**, in the same
/// order: [`extract_error_detail`] then [`finish_detail`]. Not a copy of
/// their steps — the functions themselves. Every previous round of this fix
/// re-implemented a subset and the subset drifted: first the carrier strips
/// and the second [`resolve_embedded_json`] pass were missing (so
/// `{"error":"see {\"secret\":\"x\"}"}` kept its nested payload, and a
/// `Model:`/`Auth:` dump survived whole), then [`is_server_fault`] and
/// [`is_headline_echo`] were missing (so a 5xx body reached the user and a
/// self-echo rendered as `Rate limited (429) — Rate limited (429)`).
/// Calling both outright is what makes the claim above true by construction
/// rather than by review.
///
/// It is a no-op on genuine banner text in almost every case — such a detail
/// carries no carrier prefix, no `from http…` clause and no `Model:` dump,
/// cannot start with `{`, cannot restate its own headline, and cannot be a
/// 5xx body — but **not in all**, and the exception is worth knowing.
///
/// `extract_error_detail` is not a fixed point of itself. Its first two
/// strips are prefix-anchored (`failed after N retries: `, `Internal error: `)
/// while the carrier strips after them are position-tolerant. At production a
/// carrier hides that text from the anchored strips; on re-compose the same
/// text sits at position 0 and they fire. So a genuine
/// `Bad request (400) — failed after 3 retries: the real reason` re-composes
/// as `… — the real reason`. Measured: 3 shapes × the 100 4xx codes, 300 of
/// 2400 round-trips. 5xx is immune because `is_server_fault` drops the body
/// either way. Text loss on a genuine banner, not a leak.
///
/// Reordering the two anchored strips after the carrier strips would make
/// this function genuinely idempotent and is the real fix; it changes shared
/// behaviour, so it is tracked separately rather than folded into this P2.
///
/// #429: this used to parse its own status from `headline` and gate
/// [`finish_detail`] on *that*, while the caller separately overwrote the
/// returned `status` field with `caller_status.or(that_same_parse)` — two
/// reads of "the status" landing in two different places. A raw that
/// forges an exact 4xx headline over a caller-reported 5xx made them
/// disagree: the field said 500 (a server fault) while the gate was asked
/// about 400, so the 5xx body it should have suppressed reached the user.
/// `status` below is computed once, from `caller_status` — the trustworthy
/// one, since it is the transport's own ACP `http_status` — and reused for
/// both the gate and the returned field, so there is only one place left
/// that could disagree with itself. `banner_status` stays local: it is only
/// ever a question about the headline text ("is this the canonical
/// headline for the status it itself claims"), not about what actually
/// happened.
///
/// #471: recognition still keys off `banner_status` (idempotence for a
/// genuine same-status round-trip). Rendering keys off the winning
/// `status`. When the caller and the banner text disagree, the headline,
/// action, and default_why are re-classified from the caller so the user
/// is not told "400 / malformed" while the field says "500 / retry".
fn split_existing_banner(caller_status: Option<u16>, raw: &str) -> Option<FormattedRequestFailure> {
    let (headline, detail) = raw.split_once(" \u{2014} ")?;
    let headline = headline.trim();
    let banner_status = parse_http_status(headline)?;
    // `classify` ignores the wire type once the status is known, so `Other`
    // reproduces exactly the headline any caller would have got.
    let banner_class = classify(Some(banner_status), WireErrorType::Other);
    if headline != banner_class.headline {
        return None;
    }
    // The caller's status still wins, as it always has; the banner's own
    // parsed status is only the fallback. Bound once here, then threaded
    // into both the gate and the field below — never read a second time.
    let status = caller_status.or(Some(banner_status));
    // Same-status path keeps the recognised headline bytes (idempotence for
    // #244 / #383). Disagreeing caller (#471) re-renders from `status`.
    let class = if status == Some(banner_status) {
        banner_class
    } else {
        classify(status, WireErrorType::Other)
    };
    let detail = finish_detail(
        extract_error_detail(detail),
        status,
        WireErrorType::Other,
        &class,
    );
    Some(FormattedRequestFailure {
        status,
        headline: class.headline,
        detail,
    })
}

fn looks_like_typed_provider_failure(
    status: Option<u16>,
    error_type: Option<&str>,
    raw: &str,
) -> bool {
    if status.is_some() {
        return true;
    }
    match WireErrorType::parse(error_type) {
        WireErrorType::Api | WireErrorType::Http => return true,
        _ => {}
    }
    let lower = raw.to_ascii_lowercase();
    lower.contains("provider request failed")
        || (lower.contains("internal error:") && raw.contains('{'))
        || lower.contains("\"http_status\"")
        || lower.contains("api error (status ")
}

/// Format a terminal request / API error for the TUI.
///
/// `status` is preferred when the caller already parsed it (ACP `http_status`
/// field). Otherwise the status is recovered from the message text.
///
/// Shape: `Headline (code) — optional why. What to do.` Server text is kept
/// only when it adds information: never for server faults (5xx bodies are
/// internal detail like "upstream exploded") and never as a headline echo.
/// A status-level next step is always kept when we have one.
pub(crate) fn format_request_failure(
    status: Option<u16>,
    error_type: Option<&str>,
    raw: &str,
) -> FormattedRequestFailure {
    let wire = WireErrorType::parse(error_type);
    // Text-sniffed status must not demote a dedicated wire-type headline to
    // generic status copy (an `auth_transient` message contains
    // "Unauthorized (401)"); only the untyped rails recover it from the text.
    let status = status.or_else(|| {
        matches!(wire, WireErrorType::Api | WireErrorType::Other)
            .then(|| parse_http_status(raw))
            .flatten()
    });
    let extracted = extract_error_detail(raw);
    let class = classify(status, wire);
    let detail = finish_detail(extracted, status, wire, &class);
    FormattedRequestFailure {
        status,
        headline: class.headline,
        detail,
    }
}

/// Turn a cleaned-up server reason into the banner's detail half.
///
/// The tail both rails share, so neither can gain a gate the other lacks:
/// drop a reason we must not show ([`is_server_fault`] — 5xx bodies are
/// internal detail; [`is_headline_echo`] — it just restates the headline),
/// fall back to our own copy, then compose `why. action`.
fn finish_detail(
    extracted: Option<String>,
    status: Option<u16>,
    wire: WireErrorType,
    class: &Classified,
) -> String {
    let why = extracted
        .filter(|d| !is_server_fault(status, wire) && !is_headline_echo(d, &class.headline))
        .or_else(|| class.default_why.map(str::to_string));
    compose_detail(why.as_deref(), class.action)
}

struct Classified {
    headline: String,
    /// What the user can do. Omitted when we have no real next step.
    action: Option<&'static str>,
    /// Used only when the server body added nothing.
    default_why: Option<&'static str>,
}

fn classify(status: Option<u16>, wire: WireErrorType) -> Classified {
    if let Some(code) = status {
        let (prefix, action, default_why) = match code {
            400 | 422 => (
                "Bad request",
                None,
                Some("The server rejected this request."),
            ),
            403 => (
                "Request denied",
                None,
                Some("You don't have permission to do this."),
            ),
            404 => (
                "Not found",
                Some("Run /model to pick another."),
                Some("This model isn't available."),
            ),
            408 | 504 => (
                "Request timed out",
                Some("Try again shortly."),
                Some("The server took too long to respond."),
            ),
            409 => (
                "Conflict",
                Some("Try again."),
                Some("The request conflicted with the current state."),
            ),
            413 => (
                "Request too large",
                Some("Try a smaller prompt or run /compact."),
                None,
            ),
            429 => (
                "Rate limited",
                Some("Try again later."),
                Some("You've hit the rate limit for your plan."),
            ),
            502 | 503 => (
                "Service unavailable",
                Some("The service is busy. Wait a minute and send again."),
                None,
            ),
            100..=399 => (
                "Request failed",
                None,
                Some("The request did not complete successfully."),
            ),
            400..=499 => (
                "Request failed",
                None,
                Some("The server rejected this request."),
            ),
            _ => (
                "Server error",
                Some("Something went wrong on our side. Wait a minute and send again."),
                None,
            ),
        };
        return Classified {
            headline: format!("{prefix} ({code})"),
            action,
            default_why,
        };
    }
    let (headline, action, default_why) = match wire {
        WireErrorType::IdleTimeout => (
            "No response from the model",
            Some("Try sending again."),
            Some("It may be stuck."),
        ),
        WireErrorType::EmptyResponse => (
            "Empty response",
            Some("Try sending again."),
            Some("The model returned no content."),
        ),
        WireErrorType::Serialization => (
            "Couldn't read the response",
            Some("Try sending again."),
            None,
        ),
        WireErrorType::Http => (
            "Connection failed",
            Some("Check your network and try again."),
            None,
        ),
        WireErrorType::MaxTokensTruncation => (
            "Response truncated",
            Some("Try asking for a shorter answer."),
            Some("The model hit its output limit."),
        ),
        WireErrorType::RateLimited => (
            "Rate limited",
            Some("Try again later."),
            Some("You've hit the rate limit for your plan."),
        ),
        WireErrorType::Api => (
            "Server error",
            Some("Something went wrong on our side. Wait a minute and send again."),
            None,
        ),
        WireErrorType::AuthTransient => (
            "Authentication temporarily unavailable",
            Some("Try sending again in a moment."),
            None,
        ),
        _ => (
            "Request failed",
            Some("Try sending again."),
            Some("Something went wrong."),
        ),
    };
    Classified {
        headline: headline.to_string(),
        action,
        default_why,
    }
}

/// `why. action`, deduplicating (ignoring case/punctuation) when one already
/// contains the other.
fn compose_detail(why: Option<&str>, action: Option<&str>) -> String {
    match (why, action) {
        (None, None) => String::new(),
        (None, Some(one)) | (Some(one), None) => one.to_string(),
        (Some(why), Some(action)) => {
            let (w, a) = (normalize_phrase(why), normalize_phrase(action));
            if w.contains(&a) {
                why.to_string()
            } else if a.contains(&w) {
                action.to_string()
            } else {
                format!("{}. {action}", why.trim_end_matches('.').trim_end())
            }
        }
    }
}

/// Server-fault responses (5xx and their wire equivalents) carry internal
/// detail ("upstream exploded") users can't act on — always use our copy.
/// 429 stays client-side: its body may explain plan limits.
fn is_server_fault(status: Option<u16>, wire: WireErrorType) -> bool {
    match status {
        Some(code) => code >= 500,
        // The wire type `classify` headlines as "Server error".
        None => wire == WireErrorType::Api,
    }
}

/// The server body restates the headline (e.g. "Not Found" under a 404).
fn is_headline_echo(detail: &str, headline: &str) -> bool {
    let detail = normalize_phrase(detail);
    let headline = normalize_phrase(headline);
    detail.is_empty() || headline.starts_with(&detail) || detail.starts_with(&headline)
}

/// Lowercase, alphanumeric words joined by single spaces.
fn normalize_phrase(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Pull an HTTP error status (4xx/5xx only — prose like "status 200" or a
/// year must never classify a failure) out of a raw dump
/// (`API error (status 500): …`, `Unauthorized (401)`, or an
/// already-formatted `Server error (500) — …`). The exact legacy shell shape
/// `Provider request failed (HTTP N)` is accepted without treating arbitrary
/// `HTTP N` prose as structured status.
pub(crate) fn parse_http_status(raw: &str) -> Option<u16> {
    // Bounded live-shell form: `Provider request failed (HTTP 400). …`
    const PROVIDER_HTTP: &str = "Provider request failed (HTTP ";
    if let Some(i) = find_ignore_ascii_case(raw, PROVIDER_HTTP) {
        let after = &raw[i + PROVIDER_HTTP.len()..];
        if let Some(code) = parse_status_digits(after, false) {
            return Some(code);
        }
    }
    // ACP `Internal error` envelope: `{"message":"…","http_status":400}`.
    const HTTP_STATUS_JSON: &str = "\"http_status\":";
    if let Some(i) = find_ignore_ascii_case(raw, HTTP_STATUS_JSON) {
        let after = raw[i + HTTP_STATUS_JSON.len()..].trim_start();
        if let Some(code) = parse_status_digits(after, false) {
            return Some(code);
        }
    }
    // Every "status " occurrence, so "status unknown; … status 503" still
    // finds the code.
    let mut from = 0;
    while let Some(i) = find_ignore_ascii_case(&raw[from..], "status ") {
        let after = from + i + "status ".len();
        if let Some(code) = parse_status_digits(&raw[after..], false) {
            return Some(code);
        }
        from = after;
    }
    const MARKERS: &[&str] = &[
        "Unauthorized (",
        "Forbidden (",
        "Not Found (",
        "Bad Request (",
        "Payment Required (",
        "Too Many Requests (",
        "Internal Server Error (",
        "Bad Gateway (",
        "Service Unavailable (",
        "Gateway Timeout (",
        "Payload Too Large (",
        "Request Entity Too Large (",
        "Server error (",
        "Request denied (",
        "Request failed (",
        "Not found (",
        "Bad request (",
        "Request too large (",
        "Service unavailable (",
        "Rate limited (",
        "Request timed out (",
        "Conflict (",
        "Provider request failed (HTTP ",
    ];
    for marker in MARKERS {
        if let Some(i) = find_ignore_ascii_case(raw, marker)
            && let Some(code) = parse_status_digits(&raw[i + marker.len()..], true)
        {
            return Some(code);
        }
    }
    None
}

/// Exactly three digits in 400..600. `require_close_paren` for the
/// `"… ("` markers, so prose like "merge conflict (300 files" can't match.
fn parse_status_digits(s: &str, require_close_paren: bool) -> Option<u16> {
    let bytes = s.as_bytes();
    if bytes.len() < 3 || !bytes[..3].iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes.get(3).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    if require_close_paren && bytes.get(3) != Some(&b')') {
        return None;
    }
    let code: u16 = s[..3].parse().ok()?;
    (400..600).contains(&code).then_some(code)
}

fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn extract_error_detail(raw: &str) -> Option<String> {
    let mut s = raw.trim().to_string();
    if s.is_empty() {
        return None;
    }

    if let Some(stripped) = strip_retry_prefix(&s) {
        s = stripped;
    }

    if let Some(stripped) = strip_internal_error_prefix(&s) {
        s = stripped;
    }

    // JSON before carrier-prefix strips: an Internal-error envelope
    // (`{"message":"Provider request failed (HTTP 400).","http_status":400}`)
    // must not be sliced mid-object by the provider-prefix matcher.
    if let Some(resolved) = resolve_embedded_json(&s) {
        s = resolved;
    }

    if let Some(rest) = strip_provider_request_failed_prefix(&s) {
        s = rest;
    }

    if let Some(rest) = strip_api_error_prefix(&s) {
        s = rest;
    }

    // The API-error strip can uncover a second carrier prefix
    // (`API error: Provider request failed (HTTP 400). …`).
    if let Some(rest) = strip_provider_request_failed_prefix(&s) {
        s = rest;
    }

    // A remaining JSON object after the prefix strips (API error + body), taken
    // before the URL-clause strip: a URL inside a JSON string would otherwise
    // split the body at its own ": " and leave garbage.
    if let Some(resolved) = resolve_embedded_json(&s) {
        s = resolved;
    }

    s = strip_from_url_clause(&s);

    // Prefer the "X is not in your available models" sentence when present
    // (before dropping the Model/Auth/Version dump that contains it).
    if let Some(idx) = s.find("is not in your available models") {
        let line_start = s[..idx].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = s[idx..].find('\n').map(|i| idx + i).unwrap_or(s.len());
        let snippet = s[line_start..line_end].trim();
        if !snippet.is_empty() {
            s = snippet.to_string();
        }
    } else {
        for marker in ["\n\n  Model:", "\n  Model:", "\n\nModel:", "\nModel:"] {
            if let Some(idx) = s.find(marker) {
                s.truncate(idx);
                break;
            }
        }
    }

    clean_detail(&s)
}

/// Strip the bounded `Provider request failed (HTTP N).` carrier, leaving
/// only the already-sanitized trailing detail (if any).
///
/// Tolerant about *where* the carrier sits and about its ASCII case — an ACP
/// `Internal error:` envelope or an `API error` wrapper can carry it, and the
/// JSON unwrap above hands it over mid-string. Strict about the code: it must
/// be a three-digit 4xx/5xx status closed by `)`, so arbitrary `HTTP N` prose
/// (`(HTTP 40x)`, `(timeout)`) is never treated as a structured carrier and
/// its text is left intact for the noise filter to judge.
///
/// A bare carrier with nothing after it yields `Some("")` rather than `None`,
/// so the carrier is dropped and the banner falls back to the canned
/// status copy — carrier text can never reach the user even if
/// [`is_noise_detail`]'s list changes.
fn strip_provider_request_failed_prefix(s: &str) -> Option<String> {
    const HEAD: &str = "Provider request failed (HTTP ";
    let start = find_ignore_ascii_case(s, HEAD)?;
    let after = &s[start + HEAD.len()..];
    // Proves three ASCII digits in 400..600 with `)` at index 3, so the
    // slice below is in bounds and on a char boundary.
    parse_status_digits(after, true)?;
    let rest = after[4..].trim_start_matches('.').trim();
    Some(rest.to_string())
}

fn strip_retry_prefix(s: &str) -> Option<String> {
    let rest = s.strip_prefix("failed after ")?;
    let idx = rest.find(" retries: ")?;
    Some(rest[idx + " retries: ".len()..].to_string())
}

/// ACP `Error` Display when `data` was serialized instead of unwrapped:
/// `Internal error: {"message":"…","http_status":400}`.
fn strip_internal_error_prefix(s: &str) -> Option<String> {
    const HEAD: &str = "Internal error:";
    let trimmed = s.trim();
    if trimmed.len() < HEAD.len()
        || !trimmed.as_bytes()[..HEAD.len()].eq_ignore_ascii_case(HEAD.as_bytes())
    {
        return None;
    }
    Some(trimmed[HEAD.len()..].trim().to_string())
}

fn strip_api_error_prefix(s: &str) -> Option<String> {
    let start = find_ignore_ascii_case(s, "API error (status ")?;
    let after = &s[start + "API error (status ".len()..];
    let colon = after.find("): ")?;
    Some(after[colon + 3..].trim().to_string())
}

fn strip_from_url_clause(s: &str) -> String {
    // "Unauthorized (401) from https://…: body" → keep body when present,
    // otherwise drop the URL clause.
    if let Some(from) = find_ignore_ascii_case(s, " from http") {
        let after_from = &s[from + " from ".len()..];
        if let Some(colon) = after_from.find(": ") {
            let body = after_from[colon + 2..].trim();
            if !body.is_empty() && !body.starts_with("http") {
                return body.to_string();
            }
        }
        return s[..from].trim().to_string();
    }
    s.to_string()
}

/// The first balanced `{…}` span that is itself valid JSON, as byte offsets.
///
/// Locating the object by *scanning* rather than by parsing the whole suffix
/// from the first `{` is the difference between a check and a formality: a
/// provider error sentence ends in a full stop as a matter of course, and
/// `{"k":"v"}.` is not a JSON document, so a suffix parse says "no payload
/// here" for the most ordinary shape there is. Every `{` is tried, so an
/// earlier brace that is only prose (`{x}`) does not shadow a real payload
/// after it. Braces inside JSON strings, and `\"` escapes, do not open or
/// close a span.
///
/// Objects only, deliberately. A balanced `[…]` is not a payload marker:
/// `[0]` and `[1, 2]` are valid JSON *and* ordinary prose ("index [0] out of
/// range"), so keying on brackets would eat real reasons. `{0}` and `{x}`
/// are not valid JSON, which is what makes braces safe to key on.
///
/// The boundary that governs is *strictly valid JSON*, not *object-shaped*.
/// `{'secret': 'payload'}` (a gateway echoing Python's `str(dict)`),
/// `{"a":1,}` and `{"secret":"pay\nload"}` are all rejected here and reach
/// the user on the normal path with no forgery involved. That is chosen, not
/// missed: tightening to "looks like an object" is what would drop the real
/// reasons the `{'a','b'}` case pins.
///
/// Leftmost-outermost, not innermost-first. A single left-to-right pass with
/// a stack would be linear, but popping the innermost span first breaks
/// envelope reading: `{"error":{"code":"x"},"message":"real"}` pops
/// `{"code":"x"}`, which parses and names no `error`/`message`, so the drop
/// path fires and the real message is lost. Bounding the input instead keeps
/// the outermost-first order that envelope extraction depends on.
fn find_json_object(s: &str) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    for start in 0..bytes.len() {
        if bytes[start] != b'{' {
            continue;
        }
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        for (i, &b) in bytes.iter().enumerate().skip(start) {
            if in_string {
                match b {
                    _ if escaped => escaped = false,
                    b'\\' => escaped = true,
                    b'"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match b {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    // `depth` is at least 1: the loop starts on a `{`.
                    depth -= 1;
                    if depth == 0 {
                        let end = i + 1;
                        if serde_json::from_str::<serde_json::Value>(&s[start..end]).is_ok() {
                            return Some((start, end));
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Resolve an embedded JSON object: the message it carries, or nothing.
///
/// [`extract_from_json`] answers only the first half. When the object parses
/// but names no `error` / `message` the payload is a machine envelope the
/// banner has no reading of (`Invalid parameter {"key":"value"}.`), and
/// showing it raw is exactly the unprocessed-provider-output leak this module
/// exists to prevent — so the object and everything after it are dropped,
/// keeping only the prose before. Text after an envelope is part of the same
/// dump; on a redaction path that is the direction to err in.
///
/// Returns `None` when no balanced object parses, so the caller leaves `s`
/// alone and the noise filter judges it.
fn resolve_embedded_json(s: &str) -> Option<String> {
    if s.len() > MAX_JSON_SCAN {
        // Too big to scan — so drop from the first `{` without reading it,
        // the same conservative direction an unreadable payload takes below.
        // Simply declining to scan is not safe: an oversized `Invalid
        // parameter {…}` keeps a detail that does not start with `{`, so
        // `is_noise_detail` passes it and `sanitize_user_error` ships its
        // first 180 characters — `Invalid parameter {"secret":"pppp…` — which
        // is the leak this function exists to prevent.
        let start = s.find('{')?;
        return Some(drop_json_tail(&s[..start]));
    }
    let (start, end) = find_json_object(s)?;
    if let Some(extracted) = extract_from_json(&s[start..end]) {
        return Some(extracted);
    }
    Some(drop_json_tail(&s[..start]))
}

/// The prose left behind once an unreadable object is excised, with any
/// carrier prefix stripped and then its dangling separator trimmed.
///
/// Carrier strips run *first*, on the untrimmed prefix — order matters
/// (#431). `strip_api_error_prefix` anchors on the trailing `"): "` that
/// sits right where the object used to be; running [`trim_dangling_separator`]
/// first eats that colon before the carrier can be recognized, so the
/// carrier itself — not the sanitized empty detail — is what came out of
/// this function and reached the user as the reason. Applying the same three
/// strips [`extract_error_detail`] runs on the full message (provider-failed,
/// api-error, provider-failed again for the nested-carrier case) means a
/// bare carrier collapses to `""` here, which `clean_detail` turns into
/// `None` so the banner falls back to its own copy — the guarantee
/// [`strip_provider_request_failed_prefix`]'s doc already states for its own
/// shape now holds for `strip_api_error_prefix`'s too.
fn drop_json_tail(prefix: &str) -> String {
    let mut out = prefix.to_string();
    if let Some(rest) = strip_provider_request_failed_prefix(&out) {
        out = rest;
    }
    if let Some(rest) = strip_api_error_prefix(&out) {
        out = rest;
    }
    if let Some(rest) = strip_provider_request_failed_prefix(&out) {
        out = rest;
    }
    trim_dangling_separator(&out)
}

/// Drop the separator an excised object hung off (`… parameter — {…}`),
/// leaving the sentence punctuation before it alone.
fn trim_dangling_separator(s: &str) -> String {
    s.trim_end()
        .trim_end_matches(['\u{2014}', '-', ':', ','])
        .trim_end()
        .to_string()
}

/// Longest detail [`find_json_object`] will scan, in bytes.
///
/// The scan is quadratic in unmatched `{`: every one starts a candidate, and
/// a candidate whose depth never returns to zero runs to end-of-string. The
/// trigger is not adversarial — it is **truncation**, which is how a large
/// error body ordinarily arrives, and a body cut before its closers leaves
/// every enclosing `{` unmatched. Measured in a debug build: 2 KiB 5 ms,
/// 4 KiB 15 ms, 8 KiB 41 ms, 17 KiB 168 ms, 34 KiB 670 ms, 68 KiB 2.7 s.
///
/// Those figures are one shape (`function(){if(a){`, ~17 bytes per unmatched
/// open). All-braces is ~8× denser, and `extract_error_detail` calls this
/// twice, so the retained worst case per *format* is higher than per call:
/// measured at the bound, ~49 ms for one call and ~93 ms for a format. The
/// absolute milliseconds are machine- and load-dependent; the ~4× growth per
/// doubling and the 2× are not.
///
/// 4 KiB stays far above anything this module needs to read: the longest
/// detail it can *render* is ~200 characters
/// ([`crate::app::effects::sanitize_user_error`] truncates past that), and the
/// largest envelope in this file's fixtures is a few hundred bytes.
///
/// There is no second bite. `extract_error_detail` calls this before the
/// carrier strips as well as after, so an oversized body has already had
/// everything from its first `{` cut by the time the strips run — only the two
/// prefix-anchored strips (~25 and ~16 bytes) precede that call. A readable
/// envelope just over the bound is therefore dropped unread rather than
/// extracted, which is the conservative direction and a real discontinuity.
const MAX_JSON_SCAN: usize = 4 * 1024;

fn extract_from_json(s: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(s.trim()).ok()?;
    if let Some(err) = value.get("error") {
        if let Some(msg) = err.as_str().filter(|m| !m.is_empty()) {
            return Some(msg.to_string());
        }
        if let Some(msg) = err
            .get("message")
            .and_then(|m| m.as_str())
            .filter(|m| !m.is_empty())
        {
            return Some(msg.to_string());
        }
    }
    value
        .get("message")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .map(str::to_string)
}

fn clean_detail(s: &str) -> Option<String> {
    let sanitized = crate::app::effects::sanitize_user_error(s.trim());
    let stripped = strip_urls(&sanitized);
    let t = stripped.trim();
    if t.is_empty() || is_noise_detail(t) {
        return None;
    }
    Some(t.to_string())
}

/// Remove `http(s)://…` tokens so no endpoint leaks into a banner —
/// `sanitize_user_error` only rewrites known service names, not URLs.
/// Also drops the `for url (…)` clause reqwest wraps its URL in.
fn strip_urls(s: &str) -> String {
    if !s.contains("http://") && !s.contains("https://") {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let Some(i) = rest
            .find("http://")
            .into_iter()
            .chain(rest.find("https://"))
            .min()
        else {
            out.push_str(rest);
            break;
        };
        let url_end = rest[i..]
            .find(|c: char| c.is_whitespace() || c == ')')
            .map_or(rest.len(), |e| i + e);
        let head = &rest[..i];
        let head = head
            .strip_suffix("for url (")
            .or_else(|| head.strip_suffix('('))
            .unwrap_or(head);
        out.push_str(head);
        rest = rest[url_end..]
            .strip_prefix(')')
            .unwrap_or(&rest[url_end..]);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bodies that just restate an HTTP reason phrase or generic filler add
/// nothing over the headline + canned copy.
fn is_noise_detail(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "internal server error"
            | "internal error"
            | "server error"
            | "forbidden"
            | "unauthorized"
            | "bad request"
            | "not found"
            | "error"
            | "unknown error"
            | "unknown"
            | "none"
            | "ok"
            | "request error"
            | "model does not exist"
            | "model not found"
            | "too many requests"
            | "payment required"
            | "gateway timeout"
            | "request timeout"
            | "conflict"
            | "service unavailable"
            | "bad gateway"
            | "payload too large"
            | "request entity too large"
            | "overloaded"
    ) || lower.starts_with("json-rpc")
        || lower.starts_with("request error -")
        || lower.starts_with("provider request failed")
        || s.starts_with('{')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One code per `classify` arm, including the two ranged fallbacks.
    /// Shared so the idempotence loop and the divergence sweep cannot drift.
    const CANONICAL_HEADLINES: &[(u16, &str)] = &[
        (400, "Bad request (400)"),
        (422, "Bad request (422)"),
        (403, "Request denied (403)"),
        (404, "Not found (404)"),
        (408, "Request timed out (408)"),
        (504, "Request timed out (504)"),
        (409, "Conflict (409)"),
        (413, "Request too large (413)"),
        (429, "Rate limited (429)"),
        (502, "Service unavailable (502)"),
        (503, "Service unavailable (503)"),
        (401, "Request failed (401)"),
        (500, "Server error (500)"),
    ];

    #[test]
    fn formats_500_json_dump() {
        let formatted = format_request_failure(
            None,
            Some("api"),
            r#"API error (status 500 Internal Server Error): {"error":"upstream exploded","request_id":"abc"}"#,
        );
        assert_eq!(formatted.status, Some(500));
        assert_eq!(formatted.headline, "Server error (500)");
        assert_eq!(
            formatted.detail,
            "Something went wrong on our side. Wait a minute and send again."
        );
        assert_eq!(
            formatted.message(),
            "Server error (500) \u{2014} Something went wrong on our side. Wait a minute and send again."
        );
        assert!(!formatted.message().contains("exploded"));
    }

    /// A parsed provider reason on a 4xx survives the banner formatting
    /// end-to-end. The message shape is what `user_facing_api_error_message`
    /// produces (via `SamplingError::Api` Display) now that the sampler's
    /// body parser recovers double-encoded relay bodies.
    #[test]
    fn keeps_parsed_provider_reason_on_4xx() {
        let formatted = format_request_failure(
            Some(400),
            Some("api"),
            "API error (status 400 Bad Request): invalid_request_error: \
             Values detected in request that violate rules: JWT Token",
        );
        assert_eq!(formatted.headline, "Bad request (400)");
        assert!(
            formatted
                .detail
                .contains("Values detected in request that violate rules: JWT Token"),
            "provider reason must survive: {}",
            formatted.detail
        );
    }

    #[test]
    fn dedups_action_already_in_client_error_detail() {
        // 4xx bodies are kept (unlike 5xx), and the canned action is dropped
        // when the server text already says it.
        let formatted = format_request_failure(
            None,
            Some("api"),
            "API error (status 429 Too Many Requests): Plan limit reached, try again later",
        );
        assert_eq!(
            formatted.message(),
            "Rate limited (429) \u{2014} Plan limit reached, try again later"
        );
    }

    #[test]
    fn formats_403_with_server_message() {
        let formatted = format_request_failure(
            None,
            Some("api"),
            "API error (status 403 Forbidden): Access to the chat endpoint is denied",
        );
        assert_eq!(
            formatted.message(),
            "Request denied (403) \u{2014} Access to the chat endpoint is denied"
        );
    }

    #[test]
    fn formats_401_dump_without_triggering_reauth_copy() {
        // Re-auth is a dedicated banner; this helper only pretty-prints.
        let formatted = format_request_failure(
            Some(401),
            Some("api"),
            r#"Unauthorized (401) from https://cli-chat-proxy.grok.com/v1/responses: {"error":"Invalid or expired credentials (auth_kind=bearer)"}"#,
        );
        assert_eq!(formatted.status, Some(401));
        assert!(formatted.detail.contains("Invalid or expired credentials"));
        assert!(!formatted.message().contains("cli-chat-proxy"));
        assert!(!formatted.message().contains("https://"));
    }

    #[test]
    fn formats_413() {
        let formatted = format_request_failure(
            None,
            Some("api"),
            "API error (status 413 Payload Too Large): request too large",
        );
        assert_eq!(
            formatted.message(),
            "Request too large (413) \u{2014} Try a smaller prompt or run /compact."
        );
    }

    #[test]
    fn formats_openai_shaped_json() {
        let formatted = format_request_failure(
            None,
            Some("api"),
            r#"API error (status 400 Bad Request): {"error":{"message":"model does not support tools","type":"invalid_request_error"}}"#,
        );
        assert_eq!(
            formatted.message(),
            "Bad request (400) \u{2014} model does not support tools"
        );
    }

    #[test]
    fn formats_idle_timeout_without_status() {
        let formatted = format_request_failure(
            None,
            Some("idle_timeout"),
            "inference idle timeout after 90s with no chunks",
        );
        assert_eq!(formatted.status, None);
        assert_eq!(
            formatted.message(),
            "No response from the model \u{2014} inference idle timeout after 90s with no chunks. \
             Try sending again."
        );
    }

    #[test]
    fn typed_headline_survives_status_in_message_text() {
        // `auth_transient` copy routinely embeds "Unauthorized (401)"; the
        // sniffed status must not demote the dedicated headline to generic
        // "Request failed (401)" copy.
        let formatted = format_request_failure(
            None,
            Some("auth_transient"),
            "Unauthorized (401): token refresh already in progress",
        );
        assert_eq!(formatted.status, None);
        assert_eq!(formatted.headline, "Authentication temporarily unavailable");

        // An explicit caller-parsed status still wins over the wire type.
        let formatted = format_request_failure(Some(503), Some("idle_timeout"), "whatever");
        assert_eq!(formatted.headline, "Service unavailable (503)");
    }

    #[test]
    fn formats_exhausted_retry_prefix() {
        let formatted = format_request_failure(
            None,
            None,
            r#"failed after 3 retries: API error (status 503): {"error":"overloaded"}"#,
        );
        assert_eq!(
            formatted.message(),
            "Service unavailable (503) \u{2014} The service is busy. Wait a minute and send again."
        );
    }

    #[test]
    fn formats_404_short_body_tells_user_to_switch_model() {
        let formatted = format_request_failure(
            None,
            Some("api"),
            r#"API error (status 404 Not Found): {"error":"model does not exist"}"#,
        );
        assert_eq!(
            formatted.message(),
            "Not found (404) \u{2014} This model isn't available. Run /model to pick another."
        );
    }

    #[test]
    fn drops_model_catalog_dump() {
        let formatted = format_request_failure(
            None,
            Some("api"),
            "API error (status 404 Not Found): model does not exist\n\n  Model:     grok-foo\n  Auth:      ApiKey\n  Version:   0.1.0\n  Available: grok-build\n\n  'grok-foo' is not in your available models.\n  Switch models with /model or start a new session.",
        );
        assert_eq!(formatted.status, Some(404));
        assert_eq!(
            formatted.message(),
            "Not found (404) \u{2014} 'grok-foo' is not in your available models. \
             Run /model to pick another."
        );
        assert!(!formatted.message().contains("Available:"));
        assert!(!formatted.message().contains("Version:"));
    }

    #[test]
    fn recovers_status_from_own_banner_text() {
        // PromptResponse race fallbacks (401 re-auth, 402 credit limit) sniff
        // the already-formatted error text when the http_status field is
        // absent — our own headlines must parse.
        assert_eq!(
            parse_http_status("Request failed (402) \u{2014} usage balance exhausted"),
            Some(402)
        );
        assert_eq!(
            parse_http_status("Server error (500) \u{2014} Something went wrong on our side."),
            Some(500)
        );
        assert_eq!(
            parse_http_status(
                "Provider request failed (HTTP 400). invalid field `reasoning.effort`"
            ),
            Some(400)
        );
    }

    #[test]
    fn formats_legacy_provider_400_as_bad_request_with_safe_detail() {
        let formatted = format_request_failure(
            None,
            Some("api"),
            "Provider request failed (HTTP 400). invalid field `reasoning.effort`",
        );
        assert_eq!(formatted.status, Some(400));
        assert_eq!(formatted.headline, "Bad request (400)");
        assert_eq!(formatted.detail, "invalid field `reasoning.effort`");
    }

    #[test]
    fn strips_reqwest_url_from_connection_error() {
        let formatted = format_request_failure(
            None,
            Some("http"),
            "error sending request for url (https://server.grok.com/v1/responses)",
        );
        assert!(
            !formatted.message().contains("http"),
            "{}",
            formatted.message()
        );
        assert_eq!(
            formatted.message(),
            "Connection failed \u{2014} error sending request. \
             Check your network and try again."
        );
    }

    #[test]
    fn url_inside_json_body_does_not_mangle_detail() {
        let formatted = format_request_failure(
            None,
            Some("api"),
            r#"API error (status 400 Bad Request): {"error":"fetch from https://example.com: connection refused"}"#,
        );
        assert_eq!(
            formatted.message(),
            "Bad request (400) \u{2014} connection refused"
        );
    }

    #[test]
    fn parse_http_status_rejects_non_status_numbers() {
        // Success codes, years, and prose parentheticals are not failures.
        assert_eq!(parse_http_status("expected status 200 but got EOF"), None);
        assert_eq!(parse_http_status("status 2024 items processed"), None);
        assert_eq!(
            parse_http_status("merge conflict (300 files changed)"),
            None
        );
        assert_eq!(
            parse_http_status("proxy noted HTTP 400 in free-form prose"),
            None
        );
        assert_eq!(
            parse_http_status("Provider request failed (HTTP 40x). malformed"),
            None
        );
        // A later occurrence still parses.
        assert_eq!(
            parse_http_status("status unknown; API error (status 503): overloaded"),
            Some(503)
        );
    }

    #[test]
    fn wire_error_type_parse_known_and_unknown() {
        assert_eq!(WireErrorType::parse(Some("auth")), WireErrorType::Auth);
        assert_eq!(
            WireErrorType::parse(Some("auth_transient")),
            WireErrorType::AuthTransient
        );
        assert_eq!(
            WireErrorType::parse(Some("encrypted_content_mismatch")),
            WireErrorType::EncryptedContentMismatch
        );
        assert_eq!(
            WireErrorType::parse(Some("disk_full")),
            WireErrorType::DiskFull
        );
        assert_eq!(
            WireErrorType::parse(Some("rate_limited")),
            WireErrorType::RateLimited
        );
        assert_eq!(WireErrorType::parse(Some("nope")), WireErrorType::Other);
        assert_eq!(WireErrorType::parse(None), WireErrorType::Other);
    }

    #[test]
    fn parse_http_status_from_common_shapes() {
        assert_eq!(
            parse_http_status("API error (status 502 Bad Gateway): nope"),
            Some(502)
        );
        assert_eq!(
            parse_http_status("Unauthorized (401) from https://x"),
            Some(401)
        );
        assert_eq!(
            parse_http_status("Server error (500) \u{2014} boom"),
            Some(500)
        );
        assert_eq!(parse_http_status("connection reset"), None);
    }

    #[test]
    fn parse_http_status_provider_request_failed_http_form() {
        assert_eq!(
            parse_http_status("Provider request failed (HTTP 400)."),
            Some(400)
        );
        assert_eq!(
            parse_http_status(
                "Provider request failed (HTTP 400). Invalid value for reasoning.effort"
            ),
            Some(400)
        );
        assert_eq!(
            parse_http_status("Provider request failed (timeout)."),
            None,
            "must not free-form match provider text"
        );
    }

    #[test]
    fn format_request_failure_provider_http_400_is_bad_request_not_server_error() {
        let formatted = format_request_failure(
            None,
            Some("api"),
            "Provider request failed (HTTP 400). Invalid value for reasoning.effort",
        );
        assert_eq!(formatted.status, Some(400));
        assert_eq!(formatted.headline, "Bad request (400)");
        assert!(
            formatted
                .detail
                .contains("Invalid value for reasoning.effort"),
            "safe field-level detail must remain visible, got {}",
            formatted.detail
        );
        assert!(
            !formatted.message().contains("Server error"),
            "must not misclassify 4xx as a server fault: {}",
            formatted.message()
        );
    }

    #[test]
    fn issue244_typed_provider_failure_internal_error_envelope_is_not_user_facing() {
        let envelope = "Internal error: {\n  \"message\": \"Provider request failed (HTTP 400).\",\n  \"http_status\": 400\n}";
        let formatted = compose_typed_provider_failure(None, Some("api"), envelope)
            .expect("ACP Internal-error envelope must compose as a typed 400");
        let msg = formatted.message();
        assert_eq!(formatted.status, Some(400));
        assert_eq!(formatted.headline, "Bad request (400)");
        assert!(
            !msg.contains("Retry failed"),
            "non-retryable 4xx must not be labelled Retry failed: {msg}"
        );
        assert!(
            !msg.contains('{'),
            "internal envelope must not be the user-facing line: {msg}"
        );
        assert!(
            !msg.contains("http_status"),
            "envelope field names must not leak: {msg}"
        );
        assert!(
            !msg.contains("Internal error"),
            "ACP Display prefix must not leak: {msg}"
        );
        assert!(
            msg.contains("rejected") || msg.contains("Bad request"),
            "must stay actionable, got {msg}"
        );
    }

    #[test]
    fn issue244_typed_provider_failure_http_400_keeps_safe_reason() {
        let formatted = compose_typed_provider_failure(
            Some(400),
            Some("api"),
            "Provider request failed (HTTP 400). Invalid value for reasoning.effort",
        )
        .expect("provider 400 must compose");
        let msg = formatted.message();
        assert_eq!(formatted.headline, "Bad request (400)");
        assert!(
            formatted
                .detail
                .contains("Invalid value for reasoning.effort"),
            "safe reason must survive: {}",
            formatted.detail
        );
        assert!(!msg.contains("Retry failed"), "{msg}");
        assert!(!msg.contains('{'), "{msg}");
    }

    #[test]
    fn issue244_typed_provider_failure_does_not_reformat_existing_banner() {
        let already = "Bad request (400) \u{2014} Invalid value for reasoning.effort";
        let formatted = compose_typed_provider_failure(Some(400), None, already)
            .expect("already-formatted banner is typed");
        assert_eq!(formatted.headline, "Bad request (400)");
        assert_eq!(formatted.detail, "Invalid value for reasoning.effort");
    }

    /// #429: the fast rail used to gate [`finish_detail`] on the status
    /// parsed out of the banner's own headline text, while
    /// `compose_typed_provider_failure` separately reported the caller's
    /// status in the returned field. An exactly-forged 4xx headline over a
    /// caller-reported 5xx made the two disagree: the field said 500 (a
    /// server fault the module promises never to show server text for) but
    /// the suppression gate was asked about 400, so the 5xx body reached
    /// the user anyway. This is issue #429's own worked example.
    ///
    /// #471: once the gate follows the caller, the headline must too —
    /// otherwise the user still acts on a forged 400 while the field says
    /// 500. Recognition stays on the banner's own status (idempotence);
    /// rendering follows the winning `status`.
    #[test]
    fn issue429_suppression_gate_matches_the_reported_status_not_the_banner_text() {
        let raw = "Bad request (400) \u{2014} upstream exploded at pod-42";
        let formatted = compose_typed_provider_failure(Some(500), None, raw)
            .expect("an exactly-forged 4xx banner over a caller 5xx is still typed");
        assert_eq!(
            formatted.status,
            Some(500),
            "the caller's transport status must win the reported field"
        );
        assert_eq!(
            formatted.headline, "Server error (500)",
            "headline must follow the reported status, not the forged banner text (#471)"
        );
        let msg = formatted.message();
        assert!(
            !msg.contains("upstream exploded") && !msg.contains("pod-42"),
            "status field says 500 (a server fault) — its own gate must \
             agree and suppress the server body, not the banner text's 400: {msg}"
        );
        // 500's classify has no default_why; action is the retry hint.
        assert!(
            formatted
                .detail
                .contains("Something went wrong on our side"),
            "suppressed body must fall back to the caller's class copy: {}",
            formatted.detail
        );
        // The gate and the field must always be looking at the same value:
        // suppression must track whatever `formatted.status` itself says,
        // for every disagreeing pair, not just this one.
        for (caller, banner_code, banner_headline) in [
            (500u16, 400u16, "Bad request (400)"),
            (503, 404, "Not found (404)"),
            (429, 500, "Server error (500)"),
        ] {
            let raw = format!("{banner_headline} \u{2014} internal server detail xyz");
            let formatted = compose_typed_provider_failure(Some(caller), None, &raw)
                .expect("a canonical forged headline is still typed");
            assert_eq!(formatted.status, Some(caller));
            let caller_class = classify(Some(caller), WireErrorType::Other);
            assert_eq!(
                formatted.headline, caller_class.headline,
                "headline must match classify(caller) when statuses disagree (#471)"
            );
            let is_fault = formatted.status.is_some_and(|s| s >= 500);
            let banner_is_fault = banner_code >= 500;
            let leaked = formatted.detail.contains("internal server detail");
            assert_eq!(
                leaked,
                !is_fault,
                "gate must follow the reported status ({}), not the banner's \
                 own ({banner_code}): detail={:?}",
                formatted.status.unwrap(),
                formatted.detail
            );
            // Sanity: this triple is only interesting when the two actually
            // disagree about fault-ness.
            assert_ne!(is_fault, banner_is_fault);
        }
    }

    #[test]
    fn issue471_same_status_banner_still_round_trips_unchanged() {
        // Idempotence constraint from #471: when caller and banner agree,
        // recognition must not rewrite the headline.
        let already = "Bad request (400) \u{2014} Invalid value for reasoning.effort";
        let formatted = compose_typed_provider_failure(Some(400), None, already)
            .expect("same-status banner stays on the fast path");
        assert_eq!(formatted.headline, "Bad request (400)");
        assert_eq!(formatted.status, Some(400));
        assert_eq!(formatted.detail, "Invalid value for reasoning.effort");
    }

    #[test]
    fn issue244_typed_provider_failure_skips_generic_connection_copy() {
        assert!(
            compose_typed_provider_failure(None, None, "connection reset").is_none(),
            "generic transport copy stays a TurnFailed marker"
        );
        assert!(
            compose_typed_provider_failure(None, None, "Internal error: session failed to respond")
                .is_none(),
            "ACP Display without an envelope is not a typed provider failure"
        );
    }

    #[test]
    fn parse_http_status_from_internal_error_envelope() {
        assert_eq!(
            parse_http_status(
                r#"Internal error: {"message":"Provider request failed (HTTP 400).","http_status":400}"#
            ),
            Some(400)
        );
        assert_eq!(
            parse_http_status(r#"{"http_status": 503, "message": "overloaded"}"#),
            Some(503)
        );
    }

    /// #383: a raw provider message can contain both features
    /// `split_existing_banner` used to accept on — an em dash and a parseable
    /// status — without this module ever having written it. Accepting it
    /// copied both halves verbatim, so the provider's own envelope became the
    /// user-facing line.
    #[test]
    fn issue383_raw_provider_message_with_em_dash_is_not_treated_as_a_banner() {
        let raw =
            "Provider request failed (HTTP 400). Invalid parameter \u{2014} {\"key\":\"value\"}";
        // Both rails: headless passes the wire status through, and the
        // PromptResponse rail recovers it from the text.
        for (status, error_type) in [(Some(400), Some("api")), (None, None)] {
            let formatted = compose_typed_provider_failure(status, error_type, raw)
                .expect("a provider 400 must still compose as a typed failure");
            let msg = formatted.message();
            assert_eq!(formatted.status, Some(400));
            assert_eq!(
                formatted.headline, "Bad request (400)",
                "provider text must not be adopted as our headline: {msg}"
            );
            assert_ne!(msg, raw, "the raw message must not be copied verbatim");
            assert!(
                !msg.contains('{') && !msg.contains("key") && !msg.contains("value"),
                "the unprocessed provider payload must not survive: {msg}"
            );
            assert!(
                !msg.contains("Provider request failed"),
                "the carrier prefix must be stripped: {msg}"
            );
            assert!(
                formatted.detail.contains("Invalid parameter"),
                "the safe provider reason must still survive: {}",
                formatted.detail
            );
        }
    }

    /// `parse_http_status` matches its markers case-insensitively, so a
    /// lowercased forgery parses. Only an exact match against what `classify`
    /// would have written rejects it.
    #[test]
    fn issue383_case_forged_banner_headline_is_reformatted() {
        let raw = "bad request (400) \u{2014} {\"secret\":\"payload\"}";
        let formatted = compose_typed_provider_failure(Some(400), Some("api"), raw)
            .expect("a 400 composes as a typed failure");
        let msg = formatted.message();
        assert_eq!(formatted.headline, "Bad request (400)");
        assert!(
            !msg.contains('{') && !msg.contains("secret"),
            "forged-case headline must not carry its payload through: {msg}"
        );
    }

    /// The set the fast path accepts, spelled out: `{prefix} ({code})` for
    /// every prefix `classify` can write. This is what
    /// `issue244_typed_provider_failure_does_not_reformat_existing_banner`
    /// protects, widened to the whole set — a genuine banner is passed
    /// through unmodified, never re-classified.
    ///
    /// Idempotence only. This loop cannot say anything about `is_server_fault`
    /// or `is_headline_echo`: its fixture is genuine formatter output, so at
    /// 5xx the body was already replaced at production and asserting its
    /// absence here detects nothing. That enforcement is covered by
    /// `issue383_fast_path_applies_the_server_fault_and_echo_filters`, which
    /// feeds a forged banner and does fail when the filters are removed.
    #[test]
    fn issue383_every_formatter_produced_headline_still_takes_the_fast_path() {
        for (code, headline) in CANONICAL_HEADLINES {
            // Exactly what `format_request_failure` would have written.
            assert_eq!(
                &classify(Some(*code), WireErrorType::Other).headline,
                headline,
                "the expectation itself must match the formatter"
            );
            // The fixture is real formatter output, not a hand-assembled
            // string. Idempotence is the property under test, so the input has
            // to be something this module can actually emit: a hand-written
            // detail that omits the status's canned next step is not, and
            // asserting on one would test a shape that never reaches this rail.
            let genuine = format_request_failure(Some(*code), None, "provider said no");
            assert_eq!(&genuine.headline, headline);
            let round_tripped =
                compose_typed_provider_failure(Some(*code), None, &genuine.message())
                    .expect("an already-formatted banner is typed");
            assert_eq!(round_tripped.headline, genuine.headline);
            assert_eq!(
                round_tripped.detail, genuine.detail,
                "a genuine banner must round-trip unchanged"
            );
        }
    }

    /// The fast path still receives the detail half verbatim from its
    /// producer, so it re-runs the redaction `clean_detail` applies. Both
    /// steps are idempotent on genuine banner text (asserted above).
    #[test]
    fn issue383_fast_path_detail_is_still_redacted() {
        let raw = "Bad request (400) \u{2014} rejected by cli-chat-proxy at https://internal.example.com/v1";
        let formatted = compose_typed_provider_failure(Some(400), None, raw)
            .expect("an already-formatted banner is typed");
        let msg = formatted.message();
        assert_eq!(formatted.headline, "Bad request (400)");
        assert!(
            !msg.contains("cli-chat-proxy"),
            "internal service names must not reach the banner: {msg}"
        );
        assert!(
            !msg.contains("https://") && !msg.contains("internal.example.com"),
            "endpoints must not reach the banner: {msg}"
        );
        assert!(msg.contains("rejected by server"), "{msg}");

        // The headline can also be forged exactly. The detail half is still
        // the producer's, so an unreadable payload is dropped there too and
        // the banner degrades to the headline alone.
        let forged = "Bad request (400) \u{2014} {\"key\":\"value\"}";
        let formatted = compose_typed_provider_failure(Some(400), None, forged)
            .expect("an already-formatted banner is typed");
        let msg = formatted.message();
        assert_eq!(formatted.headline, "Bad request (400)");
        // Expectation raised: the emptied detail used to leave a bare
        // headline. Now that this rail also runs `finish_detail`, it falls
        // back to our own copy, exactly as the normal path does.
        assert_eq!(
            formatted.detail,
            format_request_failure(Some(400), None, "").detail
        );
        assert!(
            !msg.contains('{') && !msg.contains("key"),
            "an exactly-forged headline must not carry its payload through: {msg}"
        );
    }

    /// An embedded object the extractor cannot read is a machine payload, not
    /// a reason: it is dropped rather than shown. Brace-prose is not valid
    /// JSON and stays.
    #[test]
    fn issue383_unreadable_embedded_json_is_dropped_but_brace_prose_stays() {
        let formatted = format_request_failure(
            None,
            Some("api"),
            "API error (status 400 Bad Request): Invalid parameter {\"key\":\"value\"}",
        );
        assert_eq!(formatted.headline, "Bad request (400)");
        assert_eq!(
            formatted.detail, "Invalid parameter",
            "the unreadable payload must be dropped, not shown"
        );

        let formatted = format_request_failure(
            None,
            Some("api"),
            "API error (status 400 Bad Request): Supported values are: {'a','b'}",
        );
        assert_eq!(
            formatted.detail, "Supported values are: {'a','b'}",
            "prose containing a brace is not a payload and must survive"
        );
    }

    /// A carrier prefix immediately followed by the object it introduces —
    /// no prose between them — used to end up *as* the reason: the
    /// dangling-separator trim ate the `": "` that `strip_api_error_prefix`
    /// anchors on, so the carrier survived where the strip should have run
    /// (#431). Every carrier shape must fall back to the canned copy here,
    /// the same as the with-prose case above.
    #[test]
    fn issue431_carrier_immediately_before_brace_falls_back_to_canned_copy() {
        let canned_400 = format_request_failure(Some(400), None, "").detail;

        for raw in [
            "API error (status 400 Bad Request): {\"key\":\"value\"}",
            "API error (status 400 Bad Request): {\"code\":123,\"trace\":\"abc\"}",
            "Provider request failed (HTTP 400). {\"key\":\"value\"}",
        ] {
            let formatted = format_request_failure(None, Some("api"), raw);
            assert_eq!(formatted.headline, "Bad request (400)", "{raw}");
            assert_eq!(
                formatted.detail, canned_400,
                "the carrier must not stand in for the reason: {raw}"
            );
            let lower = formatted.detail.to_ascii_lowercase();
            assert!(
                !lower.contains("api error") && !lower.contains("provider request failed"),
                "carrier jargon must never reach the user: {raw} -> {}",
                formatted.detail
            );
        }

        // Same bug, amplified: an oversized envelope behind a carrier used
        // to lose the real reason *and* show the jargon. The real reason is
        // still lost past MAX_JSON_SCAN (documented, conservative), but the
        // carrier must not fill in for it either.
        let oversized = format!(
            "API error (status 400 Bad Request): {{\"error\":{{\"message\":\"real reason\",\"param\":\"{}\"}}}}",
            "p".repeat(5 * 1024)
        );
        let formatted = format_request_failure(None, Some("api"), &oversized);
        assert_eq!(formatted.detail, canned_400);
    }

    /// A suffix parse from the first `{` is defeated by one trailing
    /// character, and a full stop is how a provider sentence ordinarily ends
    /// — so the check has to *locate* the object, not assume it runs to the
    /// end of the string. An earlier brace that is only prose must not shadow
    /// a real payload after it either.
    #[test]
    fn issue383_embedded_json_is_found_by_balanced_scan_not_suffix_parse() {
        for raw in [
            // Trailing full stop: the suffix is no longer one JSON value.
            "Provider request failed (HTTP 400). Invalid parameter {\"secret\":\"payload\"}.",
            // The first `{` is prose; the payload is the second object.
            "Provider request failed (HTTP 400). Invalid parameter {x} {\"secret\":\"payload\"}",
            // Braces inside a JSON string must not close the span early.
            "Provider request failed (HTTP 400). Invalid parameter {\"secret\":\"pay}load\"}.",
        ] {
            let formatted = compose_typed_provider_failure(Some(400), Some("api"), raw)
                .expect("a provider 400 must still compose as a typed failure");
            let msg = formatted.message();
            assert_eq!(formatted.headline, "Bad request (400)");
            assert!(
                !msg.contains("secret") && !msg.contains("payload"),
                "the payload must be found wherever it sits: {msg}"
            );
        }

        // The span itself, so a regression is legible without the banner.
        assert_eq!(
            find_json_object("a {\"k\":\"v\"}. b").map(|(s, e)| &"a {\"k\":\"v\"}. b"[s..e]),
            Some("{\"k\":\"v\"}")
        );
        assert_eq!(find_json_object("no braces here"), None);
        assert_eq!(find_json_object("prose {x} only"), None);
        // Brackets are not a payload marker: `[0]` is valid JSON and prose.
        let formatted = format_request_failure(
            None,
            Some("api"),
            "API error (status 400): index [0] out of range",
        );
        assert_eq!(
            formatted.detail, "index [0] out of range",
            "bracket prose must survive; only objects are payloads"
        );
    }

    /// The fast path skipped the last two of the normal path's four detail
    /// gates. `is_server_fault` exists because 5xx bodies are internal detail
    /// ("upstream exploded", internal IPs, pod traces); `is_headline_echo`
    /// exists because a body that restates the headline says nothing twice.
    #[test]
    fn issue383_fast_path_applies_the_server_fault_and_echo_filters() {
        let fast = compose_typed_provider_failure(
            Some(500),
            None,
            "Server error (500) \u{2014} {\"error\":\"upstream exploded\"}",
        )
        .expect("an already-formatted banner is typed")
        .message();
        assert!(
            !fast.contains("exploded"),
            "a 5xx body is internal detail and must not reach the user: {fast}"
        );
        // The two rails must agree on the same payload — that is the property,
        // not the exact copy.
        let normal = format_request_failure(
            None,
            Some("api"),
            r#"API error (status 500 Internal Server Error): {"error":"upstream exploded"}"#,
        )
        .message();
        assert_eq!(fast, normal, "both rails must render a 5xx identically");

        let echo = compose_typed_provider_failure(
            Some(429),
            None,
            "Rate limited (429) \u{2014} Rate limited (429)",
        )
        .expect("an already-formatted banner is typed");
        assert_eq!(
            echo.message(),
            format_request_failure(Some(429), None, "").message(),
            "a detail that only restates the headline must fall back to our copy"
        );
        assert_ne!(
            echo.detail, "Rate limited (429)",
            "the headline must not be repeated as its own detail"
        );
    }

    /// Every canonical headline crossed with every nasty detail half, each
    /// compared against `finish_detail(extract_error_detail(half), …)` — the
    /// normal path's own tail. Green by construction now that
    /// `split_existing_banner` calls those two functions rather than copying
    /// their steps, which is exactly the point: this is the tripwire for a
    /// future subset creeping back in. Three previous rounds each shipped a
    /// subset that looked complete.
    #[test]
    fn issue383_fast_rail_never_diverges_from_the_normal_tail() {
        const HALVES: &[&str] = &[
            // Nested payload inside a JSON string — needs the second
            // `resolve_embedded_json` pass the fast rail used to skip.
            r#"{"error":"see {\"secret\":\"x\"}"}"#,
            // Model/Auth catalog dump — needs the truncation branch.
            "reason\n\n  Model: gpt-9\n  Auth: oauth\n  Version: 0.1.0",
            "reason\n  Model:     gpt-9\n  Available: a, b",
            // Carrier prefixes.
            "API error (status 500): whatever",
            "API error (status 400 Bad Request): invalid field `x`",
            "failed after 3 retries: API error (status 503): {\"error\":\"overloaded\"}",
            "Internal error: {\"message\":\"boom\",\"http_status\":400}",
            "Provider request failed (HTTP 400). invalid field `x`",
            "API error: Provider request failed (HTTP 400). nested carrier",
            // URL clauses.
            "Unauthorized (401) from https://internal.example.com/v1: real reason",
            "error sending request for url (https://server.example.com/v1)",
            r#"{"error":"fetch from https://example.com: connection refused"}"#,
            // Objects the extractor can and cannot read.
            r#"{"error":{"message":"model does not support tools"}}"#,
            r#"{"error":{"code":"x"},"message":"real"}"#,
            r#"{"key":"value"}"#,
            "Invalid parameter {\"secret\":\"payload\"}.",
            "Invalid parameter {x} {\"secret\":\"payload\"}",
            // Brace prose that must survive.
            "Supported values are: {'a','b'}",
            "index [0] out of range",
            // Noise the filter drops.
            "Internal Server Error",
            "unknown",
            "JSON-RPC error -32000",
            // Ordinary reasons, echoes, and whitespace.
            "invalid field `reasoning.effort`",
            "Bad request (400)",
            "   padded reason   ",
            "a — b — c",
        ];

        let mut divergences = Vec::new();
        let mut compared = 0usize;
        for (code, headline) in CANONICAL_HEADLINES {
            for half in HALVES {
                let raw = banner_message(headline, half);
                let Some(fast) = compose_typed_provider_failure(Some(*code), None, &raw) else {
                    divergences.push(format!("{headline:?} + {half:?}: fast rail declined"));
                    continue;
                };
                let class = classify(Some(*code), WireErrorType::Other);
                let want = finish_detail(
                    extract_error_detail(half),
                    Some(*code),
                    WireErrorType::Other,
                    &class,
                );
                compared += 1;
                if fast.detail != want {
                    divergences.push(format!(
                        "{headline:?} + {half:?}\n     fast={:?}\n     norm={:?}",
                        fast.detail, want
                    ));
                }
            }
        }
        assert_eq!(
            compared,
            CANONICAL_HEADLINES.len() * HALVES.len(),
            "every pair must reach the fast rail"
        );
        assert!(
            divergences.is_empty(),
            "{} divergence(s) of {compared} pairs:\n  {}",
            divergences.len(),
            divergences.join("\n  ")
        );
    }

    /// The two shapes the fast rail used to keep whole because it skipped the
    /// carrier strips and the second JSON pass.
    #[test]
    fn issue383_fast_rail_strips_carriers_and_nested_payloads() {
        // A payload nested inside a JSON string: reading the envelope once
        // uncovers a second object, which the normal path resolves again.
        let nested = compose_typed_provider_failure(
            Some(400),
            None,
            r#"Bad request (400) — {"error":"see {\"secret\":\"x\"}"}"#,
        )
        .expect("an already-formatted banner is typed");
        assert_eq!(nested.detail, "see");
        assert!(
            !nested.message().contains("secret"),
            "the nested payload must not survive: {}",
            nested.message()
        );

        // A model/auth catalog dump behind a real reason.
        let dump = compose_typed_provider_failure(
            Some(400),
            None,
            "Bad request (400) \u{2014} reason\n\n  Model: gpt-9\n  Auth: oauth",
        )
        .expect("an already-formatted banner is typed");
        assert_eq!(dump.detail, "reason");
        assert!(
            !dump.message().contains("Auth:"),
            "the catalog dump must not survive: {}",
            dump.message()
        );

        // A carrier prefix that used to ride through intact.
        let carrier = compose_typed_provider_failure(
            Some(400),
            None,
            "Bad request (400) \u{2014} API error (status 500): whatever",
        )
        .expect("an already-formatted banner is typed");
        assert_eq!(carrier.detail, "whatever");
    }

    /// The scan is quadratic in unmatched `{`, and truncation — not an
    /// attacker — is what produces them: a large body cut before its closers
    /// leaves every enclosing brace open. Both ingresses are unbounded.
    #[test]
    fn issue383_truncated_body_does_not_stall_the_scan() {
        // 68 KiB of unmatched opens took 2.7 s unbounded (debug).
        let truncated = format!("Invalid parameter {}", "{\"a\":".repeat(14_000));
        assert!(truncated.len() > 68 * 1024);
        let started = std::time::Instant::now();
        let formatted = format_request_failure(Some(400), None, &truncated);
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "a truncated body must not stall the banner: took {elapsed:?}"
        );
        // Over the bound the object is dropped unread rather than scanned —
        // declining to scan would leave `Invalid parameter {"a":{"a":…` for
        // `sanitize_user_error` to truncate and ship.
        assert!(
            !formatted.message().contains('{'),
            "an unscanned payload must still be dropped: {}",
            formatted.message()
        );

        // Same shape, with a payload that would leak under a bare fall-through.
        let oversized = format!(
            "Bad request (400) \u{2014} Invalid parameter {{\"secret\":\"{}\"}}",
            "p".repeat(20_000)
        );
        let formatted = compose_typed_provider_failure(Some(400), None, &oversized)
            .expect("an already-formatted banner is typed");
        assert_eq!(formatted.detail, "Invalid parameter");
        assert!(
            !formatted.message().contains("secret"),
            "an oversized payload must not survive: {}",
            formatted.message()
        );
    }
}
