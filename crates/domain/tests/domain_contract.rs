use openpush_domain::{CommandId, Cursor, SendState, SourceSequence};

#[test]
fn json_64_bit_values_are_decimal_strings() {
    let cursor = Cursor(u64::MAX);
    let sequence = SourceSequence(u64::MAX - 1);
    assert_eq!(
        serde_json::to_string(&cursor).unwrap(),
        "\"18446744073709551615\""
    );
    assert_eq!(
        serde_json::from_str::<Cursor>("\"18446744073709551615\"").unwrap(),
        cursor
    );
    assert_eq!(
        serde_json::to_string(&sequence).unwrap(),
        "\"18446744073709551614\""
    );
}

#[test]
fn decimal_values_reject_noncanonical_and_numeric_json() {
    assert!(serde_json::from_str::<Cursor>("\"01\"").is_err());
    assert!(serde_json::from_str::<Cursor>("1").is_err());
}

#[test]
fn unknown_send_outcome_cannot_be_retried() {
    assert!(!SendState::OutcomeUnknown.can_retry_transport());
    assert!(!SendState::OutcomeUnknown.can_retry_carrier());
}

#[test]
fn command_ids_are_opaque_uuid_strings() {
    let id = CommandId::new();
    assert_eq!(
        serde_json::from_str::<CommandId>(&serde_json::to_string(&id).unwrap()).unwrap(),
        id
    );
}
