use peppy_domain::{CommandId, DeviceId, EnvelopeId, SourceSequence, VaultId};
use peppy_protocol::{
    CompactionMetadata, CompactionReference, Envelope, EnvelopeError, EnvelopePurpose,
    GatewayRoute, PairingQrRecord,
};
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

fn envelope() -> Envelope {
    Envelope {
        protocol_version: 1,
        envelope_id: EnvelopeId::new(),
        command_id: Some(CommandId::new()),
        vault_id: VaultId::new(),
        producer_device_id: DeviceId::new(),
        producer_sequence: SourceSequence(u64::MAX),
        key_epoch: 1,
        crypto_suite: 1,
        profile_fingerprint: "a".repeat(64),
        purpose: EnvelopePurpose::Command,
        route: Some(GatewayRoute {
            gateway_device_id: DeviceId::new(),
            subscription_id: "sim:42:generation:7".into(),
        }),
        compaction: None,
        ciphertext: vec![1, 2, 3],
    }
}

#[test]
fn compaction_is_outside_legacy_aad_and_digest_but_validated() {
    let mut event = envelope();
    event.purpose = EnvelopePurpose::Event;
    event.command_id = None;
    event.route = None;
    let digest = event.wire_digest().unwrap();
    event.compaction = Some(CompactionMetadata {
        key: vec![7; 32],
        terminal: true,
        supersedes: vec![CompactionReference {
            producer_device_id: DeviceId::new(),
            producer_sequence: SourceSequence(1),
        }],
        checkpoint: false,
    });
    assert_eq!(event.wire_digest().unwrap(), digest);
    let legacy = serde_json::to_value(event.compaction.as_ref().unwrap()).unwrap();
    assert!(
        legacy.get("checkpoint").is_none(),
        "false marker is omitted"
    );
    event.compaction.as_mut().unwrap().key.clear();
    assert_eq!(event.validate(), Err(EnvelopeError::InvalidCompaction));
}

#[test]
fn compaction_rejects_own_current_or_future_references() {
    let mut event = envelope();
    event.purpose = EnvelopePurpose::Event;
    event.command_id = None;
    event.route = None;
    let own = event.producer_device_id;
    let at = |sequence: u64| CompactionReference {
        producer_device_id: own,
        producer_sequence: SourceSequence(sequence),
    };
    for (sequence, ok) in [
        (event.producer_sequence.0 - 1, true),
        (event.producer_sequence.0, false),
    ] {
        event.compaction = Some(CompactionMetadata {
            key: vec![7; 32],
            terminal: false,
            supersedes: vec![at(sequence)],
            checkpoint: true,
        });
        assert_eq!(event.validate().is_ok(), ok, "{sequence}");
    }
    // Another producer's sequence numbers are independent.
    event.compaction = Some(CompactionMetadata {
        key: vec![7; 32],
        terminal: false,
        checkpoint: false,
        supersedes: vec![CompactionReference {
            producer_device_id: DeviceId::new(),
            producer_sequence: SourceSequence(u64::MAX),
        }],
    });
    assert!(event.validate().is_ok());
}

#[test]
fn json_keeps_64_bit_sequence_as_a_string() {
    let encoded = serde_json::to_string(&envelope()).unwrap();
    assert!(encoded.contains("\"producer_sequence\":\"18446744073709551615\""));
    assert_eq!(
        serde_json::from_str::<Envelope>(&encoded)
            .unwrap()
            .producer_sequence,
        SourceSequence(u64::MAX)
    );
}

#[test]
fn malformed_or_oversize_envelopes_are_rejected() {
    let mut malformed = envelope();
    malformed.command_id = None;
    assert_eq!(malformed.validate(), Err(EnvelopeError::CommandIdRequired));
    let mut oversize = envelope();
    oversize.ciphertext = vec![0; peppy_protocol::MAX_CIPHERTEXT_BYTES + 1];
    assert_eq!(oversize.validate(), Err(EnvelopeError::CiphertextTooLarge));
}

#[test]
fn command_payload_conflicts_are_exact_wire_conflicts() {
    let first = envelope();
    let mut second = first.clone();
    second.ciphertext.push(4);
    assert_ne!(first.wire_digest().unwrap(), second.wire_digest().unwrap());
    assert!(first.conflicts_with(&second).unwrap());
}

#[test]
fn same_command_id_in_another_vault_is_not_a_conflict() {
    let first = envelope();
    let mut other_vault = first.clone();
    other_vault.vault_id = VaultId::new();
    assert!(!first.conflicts_with(&other_vault).unwrap());
}

#[test]
fn aad_binds_profile_and_is_available_before_ciphertext_exists() {
    let mut header = envelope();
    header.ciphertext.clear();
    let original = header.aad_bytes().unwrap();
    header.profile_fingerprint.replace_range(..1, "b");
    assert_ne!(original, header.aad_bytes().unwrap());
    assert_eq!(
        header.validate_ciphertext(),
        Err(EnvelopeError::CiphertextTooLarge)
    );
}

#[test]
fn invalid_header_never_produces_a_digest_or_conflict_result() {
    let mut invalid = envelope();
    invalid.key_epoch = 0;
    assert_eq!(invalid.wire_digest(), Err(EnvelopeError::InvalidKeyEpoch));
    assert_eq!(
        invalid.conflicts_with(&envelope()),
        Err(EnvelopeError::InvalidKeyEpoch)
    );
}

#[test]
fn malformed_base64_is_rejected_before_large_decode() {
    let mut value = serde_json::to_value(envelope()).unwrap();
    value["ciphertext"] =
        serde_json::Value::String("A".repeat(peppy_protocol::MAX_BASE64_CIPHERTEXT_CHARS + 1));
    assert!(serde_json::from_value::<Envelope>(value).is_err());
}

#[test]
fn pairing_qr_requires_valid_origin_expiry_and_device_binding() {
    let qr = PairingQrRecord {
        https_origin: "https://example.test:8443".into(),
        challenge_token: "A".repeat(43),
        expires_at_unix_seconds: 101,
        protocol_version: 1,
        vault_id: VaultId::new(),
        key_epoch: 1,
        profile_fingerprint: "a".repeat(64),
        device_binding: DeviceId::new(),
    };
    assert!(qr.validate_at(100).is_ok());
    assert_eq!(
        qr.validate_at(101),
        Err(EnvelopeError::ExpiredPairingChallenge)
    );
}

#[test]
fn generator_is_idempotent_and_check_detects_drift() {
    let root = std::env::temp_dir().join(format!(
        "peppy-contract-test-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    assert!(!peppy_protocol::write_contracts(&root, true).unwrap());
    assert!(!peppy_protocol::write_contracts(&root, false).unwrap());
    assert!(peppy_protocol::write_contracts(&root, true).unwrap());
    fs::write(root.join("packages/generated/src/protocol.ts"), "drift").unwrap();
    assert!(!peppy_protocol::write_contracts(&root, true).unwrap());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn generated_schemas_have_resolvable_references_and_derived_ts() {
    let contracts = peppy_protocol::generate_contracts();
    let openapi: serde_json::Value = serde_json::from_str(&contracts.openapi).unwrap();
    assert!(peppy_protocol::all_schema_references_resolve(&openapi));
    assert!(contracts.typescript.contains("crypto_suite"));
    assert!(contracts.typescript.contains("profile_fingerprint"));
    assert!(contracts.typescript.contains("command_id?: string | null;"));
}

#[test]
fn event_envelope_conflict_is_scoped_to_vault_producer_and_envelope_id() {
    let mut first = envelope();
    first.purpose = EnvelopePurpose::Event;
    first.command_id = None;
    first.route = None;
    let mut changed = first.clone();
    changed.ciphertext.push(9);
    assert!(first.event_conflicts_with(&changed).unwrap());
    changed.envelope_id = EnvelopeId::new();
    assert!(!first.event_conflicts_with(&changed).unwrap());
}

#[test]
fn pairing_origin_and_token_reject_lookalikes() {
    let mut qr = PairingQrRecord {
        https_origin: "https://[::1]:443".into(),
        challenge_token: "A".repeat(43),
        expires_at_unix_seconds: 101,
        protocol_version: 1,
        vault_id: VaultId::new(),
        key_epoch: 1,
        profile_fingerprint: "a".repeat(64),
        device_binding: DeviceId::new(),
    };
    assert!(qr.validate_at(100).is_ok());
    for origin in [
        "https://[::1]evil",
        "https://[abc]",
        "https://user@example.test",
        "https://example.test/path",
        "https://example.test?x=1",
    ] {
        qr.https_origin = origin.into();
        assert_eq!(
            qr.validate_at(100),
            Err(EnvelopeError::InvalidPairingOrigin)
        );
    }
    qr.https_origin = "https://example.test".into();
    qr.challenge_token = "A".repeat(42);
    assert_eq!(qr.validate_at(100), Err(EnvelopeError::InvalidPairingToken));
}
