use crate::{AttachmentId, ConversationId, MessageId, SourceSequence};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentReference {
    pub attachment_id: AttachmentId,
    pub pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MessageRecord {
    pub message_id: MessageId,
    pub conversation_id: ConversationId,
    #[schemars(with = "String")]
    pub source_sequence: SourceSequence,
    pub attachments: Vec<AttachmentReference>,
}
