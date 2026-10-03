use peppy_domain::{ConversationId, DeviceId, EventId, MessageId, SendState, SourceSequence};
use peppy_sync_core::{ReadState, ReduceResult, SyncEvent, reduce_event};

#[test]
fn duplicate_event_does_not_inflate_unread() {
    let mut state = ReadState::default();
    let event = SyncEvent::incoming(
        EventId::new(),
        ConversationId::new(),
        DeviceId::new(),
        SourceSequence(7),
        MessageId::new(),
    );
    assert_eq!(reduce_event(&mut state, &event), ReduceResult::Applied);
    assert_eq!(reduce_event(&mut state, &event), ReduceResult::Duplicate);
    assert_eq!(state.unread_count(), 1);
    assert_eq!(state.unread_count_for(event.conversation_id), 1);
}

#[test]
fn out_of_order_seen_does_not_mark_a_gap_read() {
    let mut state = ReadState::default();
    let conversation = ConversationId::new();
    let source = DeviceId::new();
    state.observe(conversation, source, SourceSequence(1));
    state.observe(conversation, source, SourceSequence(3));
    state.mark_seen(conversation, source, [SourceSequence(3)]);
    assert!(state.is_unread(conversation, source, SourceSequence(1)));
    assert!(!state.is_unread(conversation, source, SourceSequence(3)));
    assert!(state.has_gap(conversation, source, SourceSequence(2)));
}

#[test]
fn seen_sets_merge_monotonically() {
    let mut local = ReadState::default();
    let conversation = ConversationId::new();
    let source = DeviceId::new();
    local.observe(conversation, source, SourceSequence(1));
    let mut remote = ReadState::default();
    remote.observe(conversation, source, SourceSequence(1));
    remote.mark_seen(conversation, source, [SourceSequence(1)]);
    local.merge(&remote);
    assert!(!local.is_unread(conversation, source, SourceSequence(1)));
}

#[test]
fn read_state_round_trips_and_exposes_leading_gaps() {
    let mut state = ReadState::default();
    let conversation = ConversationId::new();
    let source = DeviceId::new();
    state.observe_imported_read(conversation, source, SourceSequence(3));
    let restored: ReadState =
        serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert!(restored.has_leading_gap(conversation, source));
    assert!(!restored.is_unread(conversation, source, SourceSequence(3)));
}

#[test]
fn same_event_id_with_different_payload_is_a_conflict() {
    let mut state = ReadState::default();
    let conversation = ConversationId::new();
    let producer = DeviceId::new();
    let first = SyncEvent::incoming(
        EventId::new(),
        conversation,
        producer,
        SourceSequence(1),
        MessageId::new(),
    );
    let mut conflict = first.clone();
    conflict.message_id = MessageId::new();
    assert_eq!(reduce_event(&mut state, &first), ReduceResult::Applied);
    assert_eq!(reduce_event(&mut state, &conflict), ReduceResult::Conflict);
}

#[test]
fn merge_reports_conflicts_without_replacing_existing_claims() {
    let conversation = ConversationId::new();
    let producer = DeviceId::new();
    let event_id = EventId::new();
    let first = SyncEvent::incoming(
        event_id,
        conversation,
        producer,
        SourceSequence(1),
        MessageId::new(),
    );
    let mut conflicting_id = first.clone();
    conflicting_id.message_id = MessageId::new();
    let mut local = ReadState::default();
    let mut remote = ReadState::default();
    assert_eq!(reduce_event(&mut local, &first), ReduceResult::Applied);
    assert_eq!(
        reduce_event(&mut remote, &conflicting_id),
        ReduceResult::Applied
    );
    assert_eq!(local.merge(&remote), peppy_sync_core::MergeResult::Conflict);
    assert_eq!(reduce_event(&mut local, &first), ReduceResult::Duplicate);
}

#[test]
fn sequence_claim_index_survives_json_round_trip() {
    let mut state = ReadState::default();
    let event = SyncEvent::incoming(
        EventId::new(),
        ConversationId::new(),
        DeviceId::new(),
        SourceSequence(1),
        MessageId::new(),
    );
    assert_eq!(reduce_event(&mut state, &event), ReduceResult::Applied);
    let mut restored: ReadState =
        serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert_eq!(reduce_event(&mut restored, &event), ReduceResult::Duplicate);
}

#[test]
fn unknown_state_never_allows_automatic_retry() {
    assert!(!peppy_sync_core::retry::may_retry_transport(
        SendState::OutcomeUnknown
    ));
    assert!(!peppy_sync_core::retry::may_retry_carrier(
        SendState::OutcomeUnknown
    ));
    assert!(SendState::OutcomeUnknown.requires_reconciliation());
}
