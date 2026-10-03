//! Typed IPC errors. Messages are fixed, human-readable strings; they never echo credential
//! contents, server bodies, filesystem paths or key material.
use peppy_client_core::Error as CoreError;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeError {
    pub code: &'static str,
    pub message: String,
    /// The stored draft revision when it is known to differ from the caller's, so the UI can
    /// continue without a stale-revision conflict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<String>,
}

impl BridgeError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            current_revision: None,
        }
    }
    pub fn with_revision(mut self, revision: u64) -> Self {
        self.current_revision = Some(revision.to_string());
        self
    }
    pub fn host_state() -> Self {
        Self::new("host-state", "Native state is unavailable.")
    }
    pub fn no_session() -> Self {
        Self::new(
            "credentials-required",
            "Import a device credential for the configured server first.",
        )
    }
}

pub type BridgeResult<T> = Result<T, BridgeError>;

/// Maps client-core errors to stable UI codes. Every core message is a static string, so
/// forwarding `InvalidRequest` text cannot leak secrets.
pub fn core_error(error: CoreError) -> BridgeError {
    match error {
        CoreError::WrongPassphrase | CoreError::InvalidPassphrase => {
            BridgeError::new("unlock-failed", "The passphrase did not unlock this vault. No local data was changed.")
        }
        CoreError::StaleDraft { current_revision } => BridgeError::new(
            "stale-draft",
            format!("The draft changed elsewhere (current revision {current_revision}); both versions were kept."),
        )
        .with_revision(current_revision),
        CoreError::NotFound => BridgeError::new("not-found", "The requested stored item was not found."),
        CoreError::WrongDatabaseKey => BridgeError::new(
            "database-key-mismatch",
            "The protected database key does not open the local database. Nothing was reset.",
        ),
        CoreError::IdentityMismatch => BridgeError::new(
            "credential-mismatch",
            "The local database belongs to a different vault or device. Nothing was reset.",
        ),
        CoreError::InvalidProfile => BridgeError::new(
            "profile-mismatch",
            "The server key profile does not match the profile pinned on this device.",
        ),
        CoreError::KeysUnavailable => BridgeError::new("locked", "Unlock sync before this operation."),
        CoreError::InvalidRequest(reason) => BridgeError::new("invalid-request", format!("Request rejected: {reason}.")),
        CoreError::InvalidMedia => BridgeError::new(
            "attachment-invalid",
            "The attachment failed length, digest or decryption verification and was not installed.",
        ),
        CoreError::Storage => BridgeError::new("attachment-local", "Local attachment storage is unavailable."),
        CoreError::SnapshotMismatch => {
            BridgeError::new("resync-required", "History import was inconsistent; live state is unchanged.")
        }
        CoreError::Conflict => BridgeError::new("sync-conflict", "The server returned conflicting history; sync stopped."),
        other => BridgeError::new("core", other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_errors_map_to_stable_codes() {
        assert_eq!(
            core_error(CoreError::StaleDraft {
                current_revision: 4
            })
            .code,
            "stale-draft"
        );
        assert_eq!(
            core_error(CoreError::WrongDatabaseKey).code,
            "database-key-mismatch"
        );
        assert_eq!(core_error(CoreError::KeysUnavailable).code, "locked");
    }
}
