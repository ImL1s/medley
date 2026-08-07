//! Security-relevant capability descriptors for tools exposed to subagents.
//!
//! Descriptors are intentionally independent of any particular tool registry
//! or configuration source. Runtime layers decide which metadata sources are
//! trusted and must treat missing metadata as [`ToolCapabilityDescriptor::Unclassified`].

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::SubagentCapabilityMode;

/// Observable effects a tool may perform.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ToolEffect {
    LocalRead,
    LocalWrite,
    Execute,
    NetworkRead,
    ExternalMutation,
    SecretAccess,
    SubagentSpawn,
}

/// Security classification for a tool.
///
/// `Unclassified` is the fail-closed default. Only the explicitly unrestricted
/// [`SubagentCapabilityMode::All`] mode accepts it without a trusted override.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "classification")]
pub enum ToolCapabilityDescriptor {
    Classified {
        effects: BTreeSet<ToolEffect>,
    },
    #[default]
    Unclassified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCapabilityDenial {
    Unclassified,
    Effect(ToolEffect),
}

impl ToolCapabilityDescriptor {
    pub fn classified(effects: impl IntoIterator<Item = ToolEffect>) -> Self {
        Self::Classified {
            effects: effects.into_iter().collect(),
        }
    }

    pub fn effects(&self) -> Option<&BTreeSet<ToolEffect>> {
        match self {
            Self::Classified { effects } => Some(effects),
            Self::Unclassified => None,
        }
    }
}

impl SubagentCapabilityMode {
    /// Return the first effect rejected by this mode, or `None` when every
    /// declared effect is permitted. Unclassified tools are rejected by every
    /// restricted mode and accepted only by `All`.
    pub fn denied_tool_capability(
        self,
        descriptor: &ToolCapabilityDescriptor,
    ) -> Option<ToolCapabilityDenial> {
        if self == Self::All {
            return None;
        }
        let ToolCapabilityDescriptor::Classified { effects } = descriptor else {
            return Some(ToolCapabilityDenial::Unclassified);
        };
        effects
            .iter()
            .copied()
            .find(|effect| !self.allows_tool_effect(*effect))
            .map(ToolCapabilityDenial::Effect)
    }

    pub fn allows_tool_capability(self, descriptor: &ToolCapabilityDescriptor) -> bool {
        if matches!(descriptor, ToolCapabilityDescriptor::Unclassified) {
            return self == Self::All;
        }
        self.denied_tool_capability(descriptor).is_none()
    }

    pub fn allows_tool_effect(self, effect: ToolEffect) -> bool {
        use SubagentCapabilityMode as Mode;
        use ToolEffect as Effect;

        match self {
            Mode::All => true,
            Mode::ReadOnly => matches!(effect, Effect::LocalRead | Effect::NetworkRead),
            Mode::ReadWrite => matches!(
                effect,
                Effect::LocalRead | Effect::LocalWrite | Effect::NetworkRead
            ),
            Mode::Execute => matches!(
                effect,
                Effect::LocalRead | Effect::Execute | Effect::NetworkRead | Effect::SubagentSpawn
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unclassified_is_fail_closed_outside_all() {
        for mode in [
            SubagentCapabilityMode::ReadOnly,
            SubagentCapabilityMode::ReadWrite,
            SubagentCapabilityMode::Execute,
        ] {
            assert!(!mode.allows_tool_capability(&ToolCapabilityDescriptor::Unclassified));
        }
        assert!(
            SubagentCapabilityMode::All
                .allows_tool_capability(&ToolCapabilityDescriptor::Unclassified)
        );
    }

    #[test]
    fn every_declared_effect_must_be_allowed() {
        let read_and_write =
            ToolCapabilityDescriptor::classified([ToolEffect::LocalRead, ToolEffect::LocalWrite]);
        assert!(!SubagentCapabilityMode::ReadOnly.allows_tool_capability(&read_and_write));
        assert!(SubagentCapabilityMode::ReadWrite.allows_tool_capability(&read_and_write));
        assert!(!SubagentCapabilityMode::Execute.allows_tool_capability(&read_and_write));
        assert!(SubagentCapabilityMode::All.allows_tool_capability(&read_and_write));
    }

    #[test]
    fn restricted_mode_effect_matrix_is_explicit() {
        let rows = [
            (ToolEffect::LocalRead, true, true, true),
            (ToolEffect::LocalWrite, false, true, false),
            (ToolEffect::Execute, false, false, true),
            (ToolEffect::NetworkRead, true, true, true),
            (ToolEffect::ExternalMutation, false, false, false),
            (ToolEffect::SecretAccess, false, false, false),
            (ToolEffect::SubagentSpawn, false, false, true),
        ];
        for (effect, read_only, read_write, execute) in rows {
            assert_eq!(
                SubagentCapabilityMode::ReadOnly.allows_tool_effect(effect),
                read_only,
                "ReadOnly / {effect:?}"
            );
            assert_eq!(
                SubagentCapabilityMode::ReadWrite.allows_tool_effect(effect),
                read_write,
                "ReadWrite / {effect:?}"
            );
            assert_eq!(
                SubagentCapabilityMode::Execute.allows_tool_effect(effect),
                execute,
                "Execute / {effect:?}"
            );
            assert!(SubagentCapabilityMode::All.allows_tool_effect(effect));
        }
    }

    #[test]
    fn descriptor_serde_uses_stable_kebab_case_contract() {
        let descriptor =
            ToolCapabilityDescriptor::classified([ToolEffect::NetworkRead, ToolEffect::LocalRead]);
        let value = serde_json::to_value(&descriptor).unwrap();
        assert_eq!(value["classification"], "classified");
        assert_eq!(
            value["effects"],
            serde_json::json!(["local-read", "network-read"])
        );
        assert_eq!(
            serde_json::from_value::<ToolCapabilityDescriptor>(value).unwrap(),
            descriptor
        );
    }
}
