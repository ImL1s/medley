/// Safe comparison between the credential placed on an outbound request and
/// one current provider snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SentCredentialRelation {
    NotSent,
    CurrentUnavailable,
    SameAsCurrent,
    DifferentFromCurrent,
}

impl SentCredentialRelation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotSent => "not_sent",
            Self::CurrentUnavailable => "current_unavailable",
            Self::SameAsCurrent => "same_as_current",
            Self::DifferentFromCurrent => "different_from_current",
        }
    }

    pub const fn sent_credential_present(self) -> bool {
        !matches!(self, Self::NotSent)
    }
}

/// Secret-free diagnostic projection suitable for callbacks, request
/// extensions, logs, and telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CredentialComparison {
    pub relation: SentCredentialRelation,
    pub current_credential_present: bool,
}

impl CredentialComparison {
    pub const fn not_sent(current_credential_present: bool) -> Self {
        Self {
            relation: SentCredentialRelation::NotSent,
            current_credential_present,
        }
    }

    pub const fn current_unavailable() -> Self {
        Self {
            relation: SentCredentialRelation::CurrentUnavailable,
            current_credential_present: false,
        }
    }

    pub const fn same_as_current() -> Self {
        Self {
            relation: SentCredentialRelation::SameAsCurrent,
            current_credential_present: true,
        }
    }

    pub const fn different_from_current() -> Self {
        Self {
            relation: SentCredentialRelation::DifferentFromCurrent,
            current_credential_present: true,
        }
    }

    pub fn compare(sent: Option<&str>, current: Option<&str>) -> Self {
        match (sent, current) {
            (None, current) => Self::not_sent(current.is_some()),
            (Some(_), None) => Self::current_unavailable(),
            (Some(sent), Some(current)) if sent == current => Self::same_as_current(),
            (Some(_), Some(_)) => Self::different_from_current(),
        }
    }

    pub const fn sent_credential_present(self) -> bool {
        self.relation.sent_credential_present()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_truth_table() {
        assert_eq!(
            CredentialComparison::compare(None, None),
            CredentialComparison::not_sent(false)
        );
        assert_eq!(
            CredentialComparison::compare(None, Some("current")),
            CredentialComparison::not_sent(true)
        );
        assert_eq!(
            CredentialComparison::compare(Some("sent"), None),
            CredentialComparison::current_unavailable()
        );
        assert_eq!(
            CredentialComparison::compare(Some("same"), Some("same")),
            CredentialComparison::same_as_current()
        );
        assert_eq!(
            CredentialComparison::compare(Some("sent"), Some("other")),
            CredentialComparison::different_from_current()
        );
    }

    #[test]
    fn stable_strings_and_presence() {
        assert_eq!(SentCredentialRelation::NotSent.as_str(), "not_sent");
        assert_eq!(
            SentCredentialRelation::CurrentUnavailable.as_str(),
            "current_unavailable"
        );
        assert_eq!(
            SentCredentialRelation::SameAsCurrent.as_str(),
            "same_as_current"
        );
        assert_eq!(
            SentCredentialRelation::DifferentFromCurrent.as_str(),
            "different_from_current"
        );
        assert!(!CredentialComparison::not_sent(true).sent_credential_present());
        assert!(CredentialComparison::current_unavailable().sent_credential_present());
    }
}
