use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SendState {
    QueuedLocal,
    AcceptedServer,
    PersistedGateway,
    AttemptRecorded,
    SubmittedToOs,
    Sent,
    Delivered,
    FailedBeforeSubmission,
    FailedConfirmed,
    OutcomeUnknown,
}

impl SendState {
    pub const fn can_retry_transport(self) -> bool {
        matches!(self, Self::QueuedLocal | Self::AcceptedServer)
    }
    pub const fn can_retry_carrier(self) -> bool {
        matches!(self, Self::FailedBeforeSubmission)
    }
    pub const fn requires_reconciliation(self) -> bool {
        matches!(self, Self::OutcomeUnknown)
    }
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Delivered | Self::FailedConfirmed)
    }
}
