use std::borrow::Cow;

use thiserror::Error;
use xai_grok_config::program_name::program_name_for_instruction;

/// Instruction to run login, or a command-free alternative when the invoked
/// name is unusable (#117).
pub(crate) fn with_login_instruction(
    when_named: impl FnOnce(&str) -> String,
    when_unnamed: &str,
) -> String {
    match program_name_for_instruction() {
        Some(prog) => when_named(prog),
        None => when_unnamed.to_owned(),
    }
}

fn not_logged_in_msg() -> String {
    with_login_instruction(
        |prog| format!("Not logged in. Run `{prog} login`."),
        "Not logged in. Sign in again.",
    )
}

fn token_expired_msg() -> String {
    with_login_instruction(
        |prog| format!("Token expired. Run `{prog} login` to re-authenticate."),
        "Token expired. Sign in again to re-authenticate.",
    )
}

fn server_rejected_msg() -> String {
    with_login_instruction(
        |prog| format!("Authentication rejected by server. Run `{prog} login` to re-authenticate."),
        "Authentication rejected by server. Sign in again to re-authenticate.",
    )
}

fn pinned_team_suffix() -> String {
    with_login_instruction(
        |prog| format!(" Run `{prog} login` to sign in with the required team."),
        " Sign in again with the required team.",
    )
}

fn api_key_auth_disabled_msg() -> String {
    with_login_instruction(
        |prog| {
            format!(
                "API-key auth is disabled by your administrator. Run `{prog} login` to authenticate."
            )
        },
        "API-key auth is disabled by your administrator. Sign in to authenticate.",
    )
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AuthError {
    #[error("{}", not_logged_in_msg())]
    NotLoggedIn,

    /// Token expired and no refresh authority available.
    #[error("{}", token_expired_msg())]
    TokenExpiredNoRefresh,

    /// Server rejected the token (401) with no recovery path.
    #[error("{}", server_rejected_msg())]
    ServerRejectedNoRecovery,

    /// All recovery strategies exhausted.
    #[error("Auth recovery exhausted; re-authentication required.")]
    RecoveryExhausted,

    /// A session's team principal violates the `force_login_team_uuid` pin.
    /// `message` states which team is required vs. returned.
    #[error("{}{}", .message, pinned_team_suffix())]
    PinnedTeamMismatch { message: String },

    /// Cached API-key session rejected because API-key auth is disabled.
    #[error("{}", api_key_auth_disabled_msg())]
    ApiKeyAuthDisabled,

    /// Outcome of a refresh-authority attempt. Recoverability (and, for
    /// permanent failures, the reason) lives in [`RefreshTokenError`].
    #[error(transparent)]
    Refresh(#[from] RefreshTokenError),
}

/// Recoverability axis of a token-refresh attempt. Deliberately total (no
/// `#[non_exhaustive]`): "permanent vs transient" is a closed decision every
/// caller must make, so a future third state should break consumers loudly.
#[derive(Debug, Error)]
pub enum RefreshTokenError {
    /// The credential is dead; the user must re-authenticate.
    #[error(transparent)]
    Permanent(#[from] RefreshTokenFailedError),
    /// Network / 5xx / unknown blip; safe to retry later. Carries the cause.
    #[error(transparent)]
    Transient(RefreshTransientError),
}

/// A retryable refresh failure, wrapping its cause. No public `From`:
/// construct only via [`AuthError::transient`] /
/// [`AuthError::transient_source`], so a stray `?` on some error can't silently
/// classify a permanent failure as retryable (mirrors the dedicated
/// [`RefreshTokenFailedError`] on the permanent arm). Display frames the cause
/// as an auth-refresh failure so internal messages (lock timeout, sleep defer)
/// don't surface bare; the permanent arm derives its copy from
/// [`RefreshTokenFailedReason::user_message`] and is not prefixed.
#[derive(Debug, Error)]
#[error("auth refresh failed: {0}")]
pub struct RefreshTransientError(#[source] Box<dyn std::error::Error + Send + Sync>);

/// A terminal refresh failure. `reason` is machine-readable; the user-facing
/// copy is derived from it via [`RefreshTokenFailedReason::user_message`], so
/// the two can never drift.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{}", .reason.user_message())]
#[non_exhaustive]
pub struct RefreshTokenFailedError {
    pub reason: RefreshTokenFailedReason,
}

impl From<RefreshTokenFailedReason> for RefreshTokenFailedError {
    fn from(reason: RefreshTokenFailedReason) -> Self {
        Self { reason }
    }
}

/// Why a token refresh terminally failed, grounded in the OAuth2 error codes
/// our IdP actually emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefreshTokenFailedReason {
    /// `invalid_grant` — the refresh token is no longer valid (expired, reused,
    /// or revoked; the IdP does not distinguish these).
    RefreshTokenRejected,
    /// `invalid_client` — the client/app credential was rejected.
    ClientRejected,
    /// The operator's `auth_provider_command` could not mint a credential in a
    /// headless run (`GROK_AUTH_EXPIRED=1`).
    ProviderInteractiveRequired,
    /// Escalation from repeated transient failures (OIDC). Never a raw IdP
    /// code: an unrecognized terminal code is classified transient, not
    /// `Other` (see `classify_terminal`).
    Other,
}

impl RefreshTokenFailedReason {
    /// Sticky until the credential changes (never ages out): a revoked refresh
    /// token never self-heals, whereas client rotation / transient escalation
    /// recover, so those age out past the TTL.
    pub(crate) fn is_sticky(self) -> bool {
        match self {
            Self::RefreshTokenRejected => true,
            Self::ClientRejected | Self::ProviderInteractiveRequired | Self::Other => false,
        }
    }

    /// Whether the verdict rules out an unattended retry for as long as it
    /// stands. Orthogonal to [`Self::is_sticky`], which is about whether the
    /// verdict ever ages out.
    pub(crate) fn blocks_unattended_retry(self) -> bool {
        match self {
            Self::RefreshTokenRejected | Self::ProviderInteractiveRequired => true,
            Self::ClientRejected | Self::Other => false,
        }
    }

    /// User-facing copy for a terminal refresh failure; the raw IdP code stays
    /// in logs.
    pub(crate) fn user_message(self) -> Cow<'static, str> {
        match self {
            Self::RefreshTokenRejected => with_login_instruction(
                |prog| {
                    format!("Your session has expired. Run `{prog} login` to sign in again.")
                },
                "Your session has expired. Sign in again.",
            )
            .into(),
            Self::ClientRejected => with_login_instruction(
                |prog| {
                    format!(
                        "Authentication is temporarily unavailable. Run `{prog} login` if this persists."
                    )
                },
                "Authentication is temporarily unavailable. Sign in again if this persists.",
            )
            .into(),
            Self::ProviderInteractiveRequired => provider_login_message(None),
            Self::Other => with_login_instruction(
                |prog| {
                    format!(
                        "Authentication could not be refreshed. Run `{prog} login` to sign in again."
                    )
                },
                "Authentication could not be refreshed. Sign in again.",
            )
            .into(),
        }
    }
}

/// `label` is the operator's `auth_provider_label`, where the surface has one.
pub(crate) fn provider_login_message(label: Option<&str>) -> Cow<'static, str> {
    match label {
        Some(label) => format!(
            "Your session expired and {label} could not renew it in the background. \
             Run /login to sign in again."
        )
        .into(),
        None => "Your session expired and your sign-in helper could not renew it in the \
                 background. Run /login to sign in again."
            .into(),
    }
}

impl AuthError {
    /// A retryable refresh failure with a message-only cause, for the genuinely
    /// message-only sites (lock timeout, sleep/dark-wake defer, no refresher);
    /// use [`Self::transient_source`] when a real error is in hand.
    pub(crate) fn transient(message: impl Into<String>) -> Self {
        Self::transient_source(message.into())
    }

    /// A retryable refresh failure that preserves `source` in the error chain
    /// (`Transient` carries the cause), so callers with a real error don't
    /// flatten it to a string.
    pub(crate) fn transient_source(
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        AuthError::Refresh(RefreshTokenError::Transient(RefreshTransientError(
            source.into(),
        )))
    }

    /// A terminal refresh failure for an already-classified `reason`.
    pub(crate) fn permanent(reason: RefreshTokenFailedReason) -> Self {
        AuthError::Refresh(RefreshTokenError::Permanent(reason.into()))
    }

    /// Retryable refresh failure (network, 5xx, sleep/dark-wake defer, etc.).
    /// Permanent failures, NotLoggedIn, and policy rejects are not transient.
    pub(crate) fn is_transient(&self) -> bool {
        matches!(self, AuthError::Refresh(RefreshTokenError::Transient(_)))
    }
}

#[cfg(test)]
mod auth_instruction_guard_tests {
    /// Scans this crate's own `src/`. A guard in the pager cannot see this
    /// crate, and this crate holds the routine 401/expiry copy — the messages
    /// a user is most likely to be reading when they are told to sign in.
    #[test]
    fn no_hardcoded_auth_instructions() {
        xai_grok_config::auth_instruction_guard::assert_no_hardcoded_auth_instructions(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        ));
    }
}
