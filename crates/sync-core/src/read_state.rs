use peppy_domain::{ConversationId, DeviceId, EventId, SourceSequence};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Retained-history read state. BTree sets are intentionally bounded by the retained local history;
/// interval compaction is deferred until it is required by measured history sizes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadState {
    conversations: BTreeMap<ConversationId, ConversationReadState>,
    events: BTreeMap<EventId, EventFingerprint>,
    producer_sequences:
        BTreeMap<ConversationId, BTreeMap<DeviceId, BTreeMap<SourceSequence, EventFingerprint>>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ConversationReadState {
    observed: BTreeMap<DeviceId, BTreeSet<SourceSequence>>,
    seen: BTreeMap<DeviceId, BTreeSet<SourceSequence>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct EventFingerprint {
    pub conversation_id: ConversationId,
    pub producer_device_id: DeviceId,
    pub source_sequence: SourceSequence,
    pub message_id: peppy_domain::MessageId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventRegistration {
    New,
    Duplicate,
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeResult {
    Merged,
    Conflict,
}

impl ReadState {
    pub(crate) fn register_event(
        &mut self,
        event_id: EventId,
        fingerprint: EventFingerprint,
    ) -> EventRegistration {
        if let Some(previous) = self.events.get(&event_id) {
            return if *previous == fingerprint {
                EventRegistration::Duplicate
            } else {
                EventRegistration::Conflict
            };
        }
        if let Some(previous) = self
            .producer_sequences
            .get(&fingerprint.conversation_id)
            .and_then(|by_producer| by_producer.get(&fingerprint.producer_device_id))
            .and_then(|by_sequence| by_sequence.get(&fingerprint.source_sequence))
        {
            return if *previous == fingerprint {
                EventRegistration::Duplicate
            } else {
                EventRegistration::Conflict
            };
        }
        self.events.insert(event_id, fingerprint);
        self.producer_sequences
            .entry(fingerprint.conversation_id)
            .or_default()
            .entry(fingerprint.producer_device_id)
            .or_default()
            .insert(fingerprint.source_sequence, fingerprint);
        EventRegistration::New
    }

    pub fn observe(
        &mut self,
        conversation: ConversationId,
        source: DeviceId,
        sequence: SourceSequence,
    ) {
        self.conversations
            .entry(conversation)
            .or_default()
            .observed
            .entry(source)
            .or_default()
            .insert(sequence);
    }
    pub fn observe_imported_read(
        &mut self,
        conversation: ConversationId,
        source: DeviceId,
        sequence: SourceSequence,
    ) {
        self.observe(conversation, source, sequence);
        self.mark_seen(conversation, source, [sequence]);
    }
    pub fn mark_seen(
        &mut self,
        conversation: ConversationId,
        source: DeviceId,
        sequences: impl IntoIterator<Item = SourceSequence>,
    ) {
        self.conversations
            .entry(conversation)
            .or_default()
            .seen
            .entry(source)
            .or_default()
            .extend(sequences);
    }
    pub fn merge(&mut self, other: &Self) -> MergeResult {
        let mut conflicted = false;
        for (conversation, incoming) in &other.conversations {
            let local = self.conversations.entry(*conversation).or_default();
            for (source, entries) in &incoming.observed {
                local.observed.entry(*source).or_default().extend(entries);
            }
            for (source, entries) in &incoming.seen {
                local.seen.entry(*source).or_default().extend(entries);
            }
        }
        for (event_id, fingerprint) in &other.events {
            if let Some(current) = self.events.get(event_id) {
                conflicted |= current != fingerprint;
            } else {
                self.events.insert(*event_id, *fingerprint);
            }
        }
        for (conversation, by_producer) in &other.producer_sequences {
            for (producer, by_sequence) in by_producer {
                for (sequence, fingerprint) in by_sequence {
                    let local = self
                        .producer_sequences
                        .entry(*conversation)
                        .or_default()
                        .entry(*producer)
                        .or_default();
                    if let Some(current) = local.get(sequence) {
                        conflicted |= current != fingerprint;
                    } else {
                        local.insert(*sequence, *fingerprint);
                    }
                }
            }
        }
        if conflicted {
            MergeResult::Conflict
        } else {
            MergeResult::Merged
        }
    }
    pub fn is_unread(
        &self,
        conversation: ConversationId,
        source: DeviceId,
        sequence: SourceSequence,
    ) -> bool {
        self.conversations
            .get(&conversation)
            .and_then(|state| state.observed.get(&source))
            .is_some_and(|items| items.contains(&sequence))
            && !self
                .conversations
                .get(&conversation)
                .and_then(|state| state.seen.get(&source))
                .is_some_and(|items| items.contains(&sequence))
    }
    pub fn has_gap(
        &self,
        conversation: ConversationId,
        source: DeviceId,
        sequence: SourceSequence,
    ) -> bool {
        let Some(items) = self
            .conversations
            .get(&conversation)
            .and_then(|state| state.observed.get(&source))
        else {
            return false;
        };
        items.range(..sequence).next_back().is_some()
            && items
                .range((
                    std::ops::Bound::Excluded(sequence),
                    std::ops::Bound::Unbounded,
                ))
                .next()
                .is_some()
            && !items.contains(&sequence)
    }
    pub fn has_leading_gap(&self, conversation: ConversationId, source: DeviceId) -> bool {
        self.conversations
            .get(&conversation)
            .and_then(|state| state.observed.get(&source))
            .and_then(|items| items.first())
            .is_some_and(|first| first.0 > 1)
    }
    pub fn unread_count(&self) -> usize {
        self.conversations
            .values()
            .map(unread_count_for_state)
            .sum()
    }
    pub fn unread_count_for(&self, conversation: ConversationId) -> usize {
        self.conversations
            .get(&conversation)
            .map(unread_count_for_state)
            .unwrap_or_default()
    }
}

fn unread_count_for_state(state: &ConversationReadState) -> usize {
    state
        .observed
        .iter()
        .map(|(source, items)| {
            items
                .iter()
                .filter(|sequence| {
                    !state
                        .seen
                        .get(source)
                        .is_some_and(|seen| seen.contains(sequence))
                })
                .count()
        })
        .sum()
}
