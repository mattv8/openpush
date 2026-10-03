use crate::{
    Client, ConversationId, DeviceId, Error, MAX_ADDRESS_BYTES, MmsReplyContext, PrivatePayload,
    Transport, enqueue, require_address, to_i64,
};
use peppy_protocol::EnvelopePurpose;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::collections::BTreeSet;

impl Client {
    /// Stores an explicitly confirmed own address for a SIM and publishes one encrypted metadata
    /// event. It never changes carrier state or existing conversation identity.
    pub fn set_mms_own_address(&self, subscription_id: &str, address: &str) -> Result<(), Error> {
        validate_own_address(address)?;
        if subscription_id.is_empty() || subscription_id.len() > crate::MAX_SUBSCRIPTION_BYTES {
            return Err(Error::InvalidRequest("subscription id"));
        }
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revision = tx
            .query_row(
                "SELECT revision FROM mms_own_addresses WHERE source_device_id=? AND subscription_id=?",
                params![ctx.device_id.to_string(), subscription_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|value| u64::try_from(value).map_err(|_| Error::Database))
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(Error::Database)?;
        apply_mms_own_address(&tx, ctx.device_id, subscription_id, address, revision)?;
        enqueue(
            &tx,
            &ctx,
            EnvelopePurpose::Event,
            None,
            None,
            &PrivatePayload::MmsOwnAddress {
                source_device_id: ctx.device_id,
                subscription_id: subscription_id.to_owned(),
                address: address.to_owned(),
                revision,
            },
        )?;
        Ok(tx.commit()?)
    }

    /// Reply recipients for the latest MMS in a conversation. Incoming group replies require a
    /// confirmed own address for the immutable message's source SIM.
    pub fn mms_reply_context(
        &self,
        conversation_id: ConversationId,
    ) -> Result<MmsReplyContext, Error> {
        let s = self.lock()?;
        let payload: Option<crate::MessagePayload> = s.conn.query_row(
            "SELECT payload FROM messages WHERE conversation_id=? AND json_extract(payload,'$.transport')='mms' ORDER BY local_order DESC LIMIT 1",
            params![conversation_id.to_string()],
            |row| row.get::<_, String>(0),
        ).optional()?.map(|json| crate::decode(json.as_bytes())).transpose()?;
        let Some(payload) = payload.filter(|payload| payload.transport == Transport::Mms) else {
            return Err(Error::NotFound);
        };
        // Locally created outgoing replies have no provider source; retain the latest immutable
        // incoming/contextual source in this conversation for own-address confirmation.
        let contextual: Option<crate::MessagePayload> = s.conn.query_row(
            "SELECT payload FROM messages WHERE conversation_id=? AND json_extract(payload,'$.mms_context') IS NOT NULL ORDER BY local_order DESC LIMIT 1",
            params![conversation_id.to_string()], |row| row.get::<_, String>(0),
        ).optional()?.map(|json| crate::decode(json.as_bytes())).transpose()?;
        let identity = payload
            .mms_context
            .as_ref()
            .map(|context| (&payload, context))
            .or_else(|| {
                contextual.as_ref().and_then(|message| {
                    message
                        .mms_context
                        .as_ref()
                        .map(|context| (message, context))
                })
            });
        let own = match identity {
            Some((message, context)) => s
                .conn
                .query_row(
                    "SELECT address FROM mms_own_addresses WHERE source_device_id=? AND subscription_id=?",
                    params![message.source_device_id.to_string(), context.subscription_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?,
            None => None,
        };
        let candidates = match payload.direction {
            crate::Direction::Incoming => payload
                .sender_address
                .into_iter()
                .chain(payload.recipients)
                .collect::<Vec<_>>(),
            // The provider sender of an outgoing MMS is self (and may be the literal AOSP
            // insert-address-token), never a reply recipient.
            crate::Direction::Outgoing => payload.recipients,
        };
        let mut unique_keys = BTreeSet::new();
        let unique = candidates
            .into_iter()
            .filter(|address| unique_keys.insert(address_key(address)))
            .collect::<Vec<_>>();
        let group = unique.len() > 1;
        let own_key = own.as_deref().map(address_key);
        if payload.direction == crate::Direction::Incoming && group {
            let blocked_reason = if own_key.is_none() {
                Some("Own address confirmation is required for this incoming MMS group")
            } else if !unique
                .iter()
                .any(|address| own_key.as_ref() == Some(&address_key(address)))
            {
                Some("Confirmed own number not found among participants")
            } else {
                None
            };
            if let Some(reason) = blocked_reason {
                return Ok(MmsReplyContext {
                    recipients: Vec::new(),
                    blocked_reason: Some(reason.into()),
                    subject: payload.subject,
                });
            }
        }
        let recipients = unique
            .into_iter()
            .filter(|address| {
                own_key
                    .as_ref()
                    .is_none_or(|own| own != &address_key(address))
            })
            .collect();
        Ok(MmsReplyContext {
            recipients,
            blocked_reason: None,
            subject: payload.subject,
        })
    }
}

pub(crate) fn valid_mms_own_address(subscription_id: &str, address: &str, revision: u64) -> bool {
    validate_own_address(address).is_ok()
        && !subscription_id.is_empty()
        && subscription_id.len() <= crate::MAX_SUBSCRIPTION_BYTES
        && i64::try_from(revision).is_ok()
}

pub(crate) fn apply_mms_own_address(
    conn: &rusqlite::Connection,
    source_device_id: DeviceId,
    subscription_id: &str,
    address: &str,
    revision: u64,
) -> Result<bool, Error> {
    validate_own_address(address)?;
    if subscription_id.is_empty() || subscription_id.len() > crate::MAX_SUBSCRIPTION_BYTES {
        return Err(Error::InvalidRequest("subscription id"));
    }
    let revision = to_i64(revision)?;
    let changed = conn.execute(
        "INSERT INTO mms_own_addresses(source_device_id,subscription_id,address,revision) VALUES(?,?,?,?)
         ON CONFLICT(source_device_id,subscription_id) DO UPDATE SET address=excluded.address,revision=excluded.revision
         WHERE excluded.revision > mms_own_addresses.revision",
        params![source_device_id.to_string(), subscription_id, address, revision],
    )?;
    Ok(changed != 0)
}

fn validate_own_address(address: &str) -> Result<(), Error> {
    require_address(address)?;
    if address.len() > MAX_ADDRESS_BYTES || address.trim() != address {
        return Err(Error::InvalidRequest("own MMS address"));
    }
    let digits = address.strip_prefix('+').unwrap_or(address);
    if !(7..=15).contains(&digits.len()) || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::InvalidRequest("own MMS address"));
    }
    Ok(())
}

fn address_key(address: &str) -> String {
    let trimmed = address.trim();
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    // Only remove presentation punctuation for an already unambiguous phone number.
    if trimmed.starts_with('+')
        && !digits.is_empty()
        && (7..=15).contains(&digits.len())
        && trimmed
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '+' | ' ' | '-' | '(' | ')' | '.'))
    {
        format!("+{digits}")
    } else {
        trimmed.to_owned()
    }
}
