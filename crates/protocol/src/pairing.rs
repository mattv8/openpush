use peppy_domain::{DeviceId, VaultId};

/// Canonical bytes that an enrolling device signs to prove possession of its
/// pinned Ed25519 private key. The challenge is the decoded 32-byte QR token;
/// role is the owner-approved role, never a consumer-supplied value.
pub fn pairing_proof_message(
    challenge: &[u8; 32],
    vault_id: VaultId,
    device_id: DeviceId,
    profile_fingerprint: &str,
    key_epoch: u32,
    approved_role: &str,
) -> Vec<u8> {
    let mut message = Vec::with_capacity(160);
    message.extend_from_slice(b"peppy-pairing-proof-v1\0");
    message.extend_from_slice(challenge);
    message.extend_from_slice(vault_id.0.as_bytes());
    message.extend_from_slice(device_id.0.as_bytes());
    message.extend_from_slice(profile_fingerprint.as_bytes());
    message.extend_from_slice(&key_epoch.to_be_bytes());
    message.extend_from_slice(approved_role.as_bytes());
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_is_stable() {
        let bytes = pairing_proof_message(
            &[7; 32],
            "00000000-0000-0000-0000-000000000000".parse().unwrap(),
            "00000000-0000-0000-0000-000000000000".parse().unwrap(),
            &"a".repeat(64),
            1,
            "gateway",
        );
        let prefix = b"peppy-pairing-proof-v1\0";
        assert!(bytes.starts_with(prefix));
        assert_eq!(bytes.len(), prefix.len() + 32 + 16 + 16 + 64 + 4 + 7);
    }
}
