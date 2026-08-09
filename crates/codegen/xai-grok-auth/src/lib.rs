//! Auth dependency-inversion seam shared between `xai-file-utils`
//! (the holder) and `xai-grok-shell` (the implementer). Keeps shell types
//! out of data-collector's import graph while still letting refresh-aware
//! token resolution drive HTTP requests.

pub mod auth_provider;
pub mod bearer_fragment;
pub mod credential_diagnostics;
#[cfg(feature = "middleware")]
pub mod retry_middleware;
pub mod visibility;

pub use auth_provider::{AuthCredentialProvider, CredentialSnapshot, StaticAuthCredentialProvider};
pub use bearer_fragment::{BEARER_SUFFIX_LEN, bearer_suffix};
pub use credential_diagnostics::{CredentialComparison, SentCredentialRelation};
#[cfg(feature = "middleware")]
pub use retry_middleware::{AuthRetryMiddleware, execute_with_auth_relation};
pub use visibility::HttpAuth;
