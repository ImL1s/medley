#[derive(Debug, thiserror::Error)]
enum SafeTransportError {
    #[error("network request failed")]
    Network(#[source] reqwest::Error),
}

impl From<reqwest::Error> for SafeTransportError {
    fn from(error: reqwest::Error) -> Self {
        Self::Network(error.without_url())
    }
}

#[derive(Debug, thiserror::Error)]
enum SafeHelperTransportError {
    #[error("network request failed: {0}")]
    Http(reqwest::Error),
}

impl SafeHelperTransportError {
    pub fn http(error: reqwest::Error) -> Self {
        Self::Http(error.without_url())
    }
}

impl From<reqwest::Error> for SafeHelperTransportError {
    fn from(error: reqwest::Error) -> Self {
        Self::http(error)
    }
}
