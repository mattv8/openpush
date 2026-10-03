use crate::ReadState;
use crate::read_state::{EventFingerprint, EventRegistration};
use peppy_domain::{ConversationId, DeviceId, EventId, MessageId, SourceSequence};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncEvent {
    pub event_id: EventId,
    pub conversation_id: ConversationId,
    pub producer_device_id: DeviceId,
    pub source_sequence: SourceSequence,
    pub message_id: MessageId,
}
impl SyncEvent {
    pub fn incoming(
        event_id: EventId,
        conversation_id: ConversationId,
        producer_device_id: DeviceId,
        source_sequence: SourceSequence,
        message_id: MessageId,
    ) -> Self {
        Self {
            event_id,
            conversation_id,
            producer_device_id,
            source_sequence,
            message_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReduceResult {
    Applied,
    Duplicate,
    Conflict,
}

/// `source_sequence` is a per-conversation producer ordinal starting at one; it is not the envelope's global producer sequence.
pub fn reduce_event(state: &mut ReadState, event: &SyncEvent) -> ReduceResult {
    let fingerprint = EventFingerprint {
        conversation_id: event.conversation_id,
        producer_device_id: event.producer_device_id,
        source_sequence: event.source_sequence,
        message_id: event.message_id,
    };
    match state.register_event(event.event_id, fingerprint) {
        EventRegistration::New => {
            state.observe(
                event.conversation_id,
                event.producer_device_id,
                event.source_sequence,
            );
            ReduceResult::Applied
        }
        EventRegistration::Duplicate => ReduceResult::Duplicate,
        EventRegistration::Conflict => ReduceResult::Conflict,
    }
}
