use agent_client_protocol as acp;

use crate::auth::{AuthManager, GrokAuth};

/// Require xAI auth from a sync context, accepting tokens in the client-side buffer window.
///
/// Messages take `impl Into<String>` (not `&'static str`) because several
/// callers build the login instruction dynamically via
/// [`crate::auth::with_login_instruction`], which returns `String`.
pub(crate) fn require_xai_auth(
    auth_manager: &AuthManager,
    missing_message: impl Into<String>,
    non_xai_message: impl Into<String>,
) -> Result<GrokAuth, acp::Error> {
    let auth = auth_manager
        .current_or_expired()
        .ok_or_else(|| acp::Error::auth_required().data(missing_message.into()))?;
    if !auth.is_xai_auth() {
        return Err(acp::Error::auth_required().data(non_xai_message.into()));
    }
    Ok(auth)
}
