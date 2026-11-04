//! Provider support shared by clients and the daemon. These declarations are
//! pure, allocation-free lookups; model options remain in `ProviderModel`, and
//! transport-dependent support is reported separately by the live runtime.

use serde::{Deserialize, Serialize};

use crate::compaction::CompactionCapabilities;
use crate::model::{ProviderKind, RuntimeMode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationCapabilities {
    /// Waku can rewind through either the transport or native session helpers.
    pub rollback: bool,
    pub fork: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegationCapabilities {
    /// The parent agent can call Waku's delegation tools.
    pub parent: bool,
    /// A child can enforce read/search access without writes, commands, or
    /// approvals that would need an unattended subtask to ask a human.
    pub review_subtask: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderCapabilities {
    /// Interactive session modes, in menu order. Review subtasks have their
    /// own execution policy and must not be treated as Full access sessions.
    pub access_modes: &'static [RuntimeMode],
    pub conversation: ConversationCapabilities,
    pub compaction: CompactionCapabilities,
    pub delegation: DelegationCapabilities,
    pub model_discovery: bool,
    pub goals: bool,
    /// A native plan mode Waku can toggle within a session (Claude's `plan`
    /// permission mode, Codex's `plan` collaboration mode).
    pub plan_mode: bool,
}

impl ProviderCapabilities {
    pub fn supports_access_mode(self, mode: RuntimeMode) -> bool {
        self.access_modes.contains(&mode)
    }

    /// A short-lived transport may need a supported launch mode just to fork
    /// native history. Its driver must be closed after the operation, never
    /// installed as an interactive runtime or persisted as the session's mode.
    pub fn access_mode_for_history(self, preferred: RuntimeMode) -> Option<RuntimeMode> {
        if self.supports_access_mode(preferred) {
            Some(preferred)
        } else {
            self.access_modes.first().copied()
        }
    }
}

/// Support negotiated by a particular transport. Sent on both start and
/// attach, so a reconnecting client uses the surviving runtime's capabilities.
/// This does not describe out-of-band features such as native history forks.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCapabilities {
    pub steer: bool,
}

const CONVERSATION: ConversationCapabilities = ConversationCapabilities {
    rollback: true,
    fork: true,
};

impl ProviderKind {
    /// The single declaration of Waku's provider-level support. Keep this
    /// match exhaustive so adding a provider requires an explicit decision
    /// for every feature instead of inheriting another provider's defaults.
    pub const fn capabilities(self) -> ProviderCapabilities {
        match self {
            Self::Claude => ProviderCapabilities {
                access_modes: &RuntimeMode::ACCESS_OPTIONS,
                conversation: CONVERSATION,
                compaction: CompactionCapabilities {
                    manual: true,
                    instructions: true,
                    lifecycle: true,
                    summary: false,
                },
                delegation: DelegationCapabilities {
                    parent: true,
                    review_subtask: true,
                },
                model_discovery: true,
                goals: false,
                plan_mode: true,
            },
            Self::Codex => ProviderCapabilities {
                access_modes: &RuntimeMode::ACCESS_OPTIONS,
                conversation: CONVERSATION,
                compaction: CompactionCapabilities {
                    manual: true,
                    instructions: false,
                    lifecycle: true,
                    summary: false,
                },
                delegation: DelegationCapabilities {
                    parent: true,
                    review_subtask: true,
                },
                model_discovery: true,
                goals: true,
                plan_mode: true,
            },
            Self::Pi => ProviderCapabilities {
                access_modes: &[RuntimeMode::FullAccess],
                conversation: CONVERSATION,
                compaction: CompactionCapabilities {
                    manual: true,
                    instructions: true,
                    lifecycle: true,
                    summary: true,
                },
                delegation: DelegationCapabilities {
                    parent: true,
                    review_subtask: true,
                },
                model_discovery: true,
                goals: false,
                plan_mode: false,
            },
        }
    }

    /// Validate without changing the caller's selected permission posture.
    pub fn validate_access_mode(self, mode: RuntimeMode) -> Result<(), UnsupportedAccessMode> {
        if self.capabilities().supports_access_mode(mode) {
            Ok(())
        } else {
            Err(UnsupportedAccessMode {
                provider: self,
                mode,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedAccessMode {
    pub provider: ProviderKind,
    pub mode: RuntimeMode,
}

impl std::fmt::Display for UnsupportedAccessMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let modes = self
            .provider
            .capabilities()
            .access_modes
            .iter()
            .map(|mode| mode.label())
            .collect::<Vec<_>>()
            .join(", ");
        formatter.write_str(&tr!(
            "errors.provider_access_mode_unsupported",
            provider = self.provider.display_name(),
            mode = self.mode.label(),
            modes = modes
        ))
    }
}

impl std::error::Error for UnsupportedAccessMode {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_modes_are_consistent_for_every_provider_and_keep_menu_order() {
        for provider in ProviderKind::ALL {
            let capabilities = provider.capabilities();
            assert!(!capabilities.access_modes.is_empty());
            let ordered = RuntimeMode::ACCESS_OPTIONS
                .into_iter()
                .filter(|mode| provider.validate_access_mode(*mode).is_ok())
                .collect::<Vec<_>>();
            assert_eq!(ordered, capabilities.access_modes);
        }
        for provider in [ProviderKind::Pi] {
            assert_eq!(
                provider.capabilities().access_modes,
                &[RuntimeMode::FullAccess]
            );
            let error = provider.validate_access_mode(RuntimeMode::Ask).unwrap_err();
            assert_eq!(error.mode, RuntimeMode::Ask);
            assert!(error.to_string().contains(provider.display_name()));
            assert!(error.to_string().contains(&RuntimeMode::FullAccess.label()));
        }
    }

    #[test]
    fn runtime_support_round_trips_for_start_and_attach() {
        use crate::ResponsePayload;
        let capabilities = RuntimeCapabilities { steer: true };
        for response in [
            ResponsePayload::Started { capabilities },
            ResponsePayload::SessionRuntime {
                runtime_id: Some(uuid::Uuid::new_v4()),
                capabilities,
            },
        ] {
            let json = serde_json::to_value(response).unwrap();
            assert_eq!(json["capabilities"]["steer"], true);
            let decoded: ResponsePayload = serde_json::from_value(json).unwrap();
            let (ResponsePayload::Started {
                capabilities: decoded,
            }
            | ResponsePayload::SessionRuntime {
                capabilities: decoded,
                ..
            }) = decoded
            else {
                panic!("wrong response variant");
            };
            assert_eq!(decoded, capabilities);
        }
        assert!(!RuntimeCapabilities::default().steer);
    }
}
