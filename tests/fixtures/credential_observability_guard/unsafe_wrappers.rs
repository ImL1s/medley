type CredentialAlias = Credentials;

struct CredentialEnvelope(CredentialAlias);

impl std::fmt::Display for CredentialEnvelope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Debug for CredentialAlias {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialAlias")
            .field("api_key", &self.api_key)
            .finish()
    }
}

#[derive(Serialize)]
#[serde(transparent)]
struct CredentialWire(CredentialEnvelope);
