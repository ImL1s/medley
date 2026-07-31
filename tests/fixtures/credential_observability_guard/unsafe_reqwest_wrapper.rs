#[derive(Debug, thiserror::Error)]
enum DerivedTransportError {
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),
}

#[derive(Debug, thiserror::Error)]
enum RawConstructorError {
    #[error("network request failed")]
    Network(#[source] reqwest::Error),
}

impl From<reqwest::Error> for RawConstructorError {
    fn from(error: reqwest::Error) -> Self {
        Self::Network(error.without_url())
    }
}

fn bypasses_sanitized_conversion(error: reqwest::Error) -> RawConstructorError {
    RawConstructorError::Network(error)
}
