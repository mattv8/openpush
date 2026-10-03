use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use peppy_domain::{CommandId, DeviceId, EnvelopeId, SourceSequence, VaultId};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_CIPHERTEXT_BYTES: usize = 1_048_576;
pub const MAX_BASE64_CIPHERTEXT_CHARS: usize = MAX_CIPHERTEXT_BYTES.div_ceil(3) * 4;
pub const COMPACTION_KEY_BYTES: usize = 32;
pub const MAX_COMPACTION_SUPERSEDES: usize = 128;
const PROFILE_FINGERPRINT_HEX_LEN: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopePurpose {
    Command,
    Event,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GatewayRoute {
    pub gateway_device_id: DeviceId,
    pub subscription_id: String,
}

/// Opaque, unauthenticated-to-the-server compaction bookkeeping. Its exact
/// authenticated copy is carried in the encrypted event payload by clients.
/// It deliberately stays out of legacy AAD and `wire_digest`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompactionMetadata {
    #[serde(
        serialize_with = "serialize_base64",
        deserialize_with = "deserialize_compaction_key"
    )]
    #[schemars(with = "String")]
    pub key: Vec<u8>,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub supersedes: Vec<CompactionReference>,
    /// Authenticated maintenance marker: this record repeats its producer's current state
    /// only to fold a large unsuperseded frontier. Readers apply it as history (no OS,
    /// banner or unread effects). Omitted when false, so legacy encodings are unchanged.
    #[serde(default, skip_serializing_if = "is_false")]
    pub checkpoint: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompactionReference {
    pub producer_device_id: DeviceId,
    pub producer_sequence: SourceSequence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Envelope {
    pub protocol_version: u16,
    pub envelope_id: EnvelopeId,
    pub command_id: Option<CommandId>,
    pub vault_id: VaultId,
    pub producer_device_id: DeviceId,
    pub producer_sequence: SourceSequence,
    pub key_epoch: u32,
    pub crypto_suite: u16,
    pub profile_fingerprint: String,
    pub purpose: EnvelopePurpose,
    pub route: Option<GatewayRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionMetadata>,
    #[serde(
        serialize_with = "serialize_base64",
        deserialize_with = "deserialize_base64"
    )]
    #[schemars(with = "String")]
    pub ciphertext: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum EnvelopeError {
    #[error("unsupported protocol version")]
    UnsupportedVersion,
    #[error("command envelopes require a command id")]
    CommandIdRequired,
    #[error("event envelopes cannot have a command id")]
    UnexpectedCommandId,
    #[error("command envelopes require a gateway route")]
    RouteRequired,
    #[error("event envelopes cannot have a gateway route")]
    UnexpectedRoute,
    #[error("event envelope conflict comparison requires event envelopes")]
    EventEnvelopeRequired,
    #[error("gateway subscription id is empty or too long")]
    InvalidSubscription,
    #[error("producer sequence must start at one")]
    InvalidProducerSequence,
    #[error("key epoch must start at one")]
    InvalidKeyEpoch,
    #[error("crypto suite must be nonzero")]
    InvalidCryptoSuite,
    #[error("profile fingerprint must be 64 lower-case hexadecimal characters")]
    InvalidProfileFingerprint,
    #[error("ciphertext is empty or too large")]
    CiphertextTooLarge,
    #[error("compaction metadata is invalid")]
    InvalidCompaction,
    #[error("invalid pairing HTTPS origin")]
    InvalidPairingOrigin,
    #[error("invalid pairing challenge token")]
    InvalidPairingToken,
    #[error("invalid pairing device binding")]
    InvalidDeviceBinding,
    #[error("pairing challenge is expired")]
    ExpiredPairingChallenge,
}

impl Envelope {
    pub fn validate_header(&self) -> Result<(), EnvelopeError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(EnvelopeError::UnsupportedVersion);
        }
        if self.producer_sequence.0 == 0 {
            return Err(EnvelopeError::InvalidProducerSequence);
        }
        if self.key_epoch == 0 {
            return Err(EnvelopeError::InvalidKeyEpoch);
        }
        if self.crypto_suite == 0 {
            return Err(EnvelopeError::InvalidCryptoSuite);
        }
        validate_profile_fingerprint(&self.profile_fingerprint)?;
        match (self.purpose, self.command_id.is_some(), self.route.as_ref()) {
            (EnvelopePurpose::Command, false, _) => Err(EnvelopeError::CommandIdRequired),
            (EnvelopePurpose::Command, true, None) => Err(EnvelopeError::RouteRequired),
            (EnvelopePurpose::Event, true, _) => Err(EnvelopeError::UnexpectedCommandId),
            (EnvelopePurpose::Event, false, Some(_)) => Err(EnvelopeError::UnexpectedRoute),
            (_, _, Some(route))
                if route.subscription_id.is_empty() || route.subscription_id.len() > 512 =>
            {
                Err(EnvelopeError::InvalidSubscription)
            }
            _ => Ok(()),
        }?;
        if let Some(compaction) = &self.compaction
            && (self.purpose != EnvelopePurpose::Event
                || compaction.key.len() != COMPACTION_KEY_BYTES
                || compaction.supersedes.len() > MAX_COMPACTION_SUPERSEDES
                || compaction.supersedes.iter().any(|reference| {
                    // Same-producer references must point backward: a current/future reference
                    // could pre-plant deletion of a later record.
                    reference.producer_sequence.0 == 0
                        || (reference.producer_device_id == self.producer_device_id
                            && reference.producer_sequence >= self.producer_sequence)
                })
                || compaction
                    .supersedes
                    .iter()
                    .enumerate()
                    .any(|(index, reference)| {
                        compaction.supersedes[..index]
                            .iter()
                            .any(|prior| prior == reference)
                    }))
        {
            return Err(EnvelopeError::InvalidCompaction);
        }
        Ok(())
    }
    pub fn validate_ciphertext(&self) -> Result<(), EnvelopeError> {
        if self.ciphertext.is_empty() || self.ciphertext.len() > MAX_CIPHERTEXT_BYTES {
            Err(EnvelopeError::CiphertextTooLarge)
        } else {
            Ok(())
        }
    }
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        self.validate_header()?;
        self.validate_ciphertext()
    }

    /// Stable versioned binary AAD. It authenticates only header context and deliberately accepts an empty ciphertext.
    pub fn aad_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        self.validate_header()?;
        let mut output = Vec::with_capacity(
            192 + self
                .route
                .as_ref()
                .map_or(0, |route| route.subscription_id.len()),
        );
        output.extend_from_slice(b"peppy-envelope-aad-v1\0");
        output.extend_from_slice(&self.protocol_version.to_be_bytes());
        output.extend_from_slice(self.envelope_id.0.as_bytes());
        output.extend_from_slice(self.vault_id.0.as_bytes());
        output.extend_from_slice(self.producer_device_id.0.as_bytes());
        output.extend_from_slice(&self.producer_sequence.0.to_be_bytes());
        output.extend_from_slice(&self.key_epoch.to_be_bytes());
        output.extend_from_slice(&self.crypto_suite.to_be_bytes());
        output.extend_from_slice(self.profile_fingerprint.as_bytes());
        output.push(match self.purpose {
            EnvelopePurpose::Command => 1,
            EnvelopePurpose::Event => 2,
        });
        match self.command_id {
            Some(id) => {
                output.push(1);
                output.extend_from_slice(id.0.as_bytes());
            }
            None => output.push(0),
        }
        match &self.route {
            Some(route) => {
                output.push(1);
                output.extend_from_slice(route.gateway_device_id.0.as_bytes());
                write_string(&mut output, &route.subscription_id);
            }
            None => output.push(0),
        }
        Ok(output)
    }
    pub fn wire_digest(&self) -> Result<[u8; 32], EnvelopeError> {
        self.validate()?;
        let mut digest = Sha256::new();
        digest.update(self.aad_bytes()?);
        digest.update(&self.ciphertext);
        Ok(digest.finalize().into())
    }
    pub fn conflicts_with(&self, other: &Self) -> Result<bool, EnvelopeError> {
        self.validate()?;
        other.validate()?;
        Ok(self.vault_id == other.vault_id
            && self.producer_device_id == other.producer_device_id
            && self.command_id == other.command_id
            && self.command_id.is_some()
            && self.wire_digest()? != other.wire_digest()?)
    }

    /// Event idempotency is scoped by `(vault_id, producer_device_id, envelope_id)`.
    /// The server must additionally enforce independent `(vault_id, producer_device_id, producer_sequence)` uniqueness.
    pub fn event_conflicts_with(&self, other: &Self) -> Result<bool, EnvelopeError> {
        self.validate()?;
        other.validate()?;
        if self.purpose != EnvelopePurpose::Event || other.purpose != EnvelopePurpose::Event {
            return Err(EnvelopeError::EventEnvelopeRequired);
        }
        Ok(self.vault_id == other.vault_id
            && self.producer_device_id == other.producer_device_id
            && self.envelope_id == other.envelope_id
            && self.wire_digest()? != other.wire_digest()?)
    }
}

fn validate_profile_fingerprint(value: &str) -> Result<(), EnvelopeError> {
    if value.len() == PROFILE_FINGERPRINT_HEX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(EnvelopeError::InvalidProfileFingerprint)
    }
}
fn write_string(output: &mut Vec<u8>, value: &str) {
    output.extend_from_slice(&(value.len() as u16).to_be_bytes());
    output.extend_from_slice(value.as_bytes());
}
fn serialize_base64<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&STANDARD.encode(value))
}
fn deserialize_base64<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let encoded = String::deserialize(deserializer)?;
    if encoded.len() > MAX_BASE64_CIPHERTEXT_CHARS {
        return Err(serde::de::Error::custom("ciphertext base64 exceeds limit"));
    }
    let decoded = STANDARD.decode(encoded).map_err(serde::de::Error::custom)?;
    if decoded.len() > MAX_CIPHERTEXT_BYTES {
        return Err(serde::de::Error::custom("ciphertext exceeds limit"));
    }
    Ok(decoded)
}
fn deserialize_compaction_key<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let key = deserialize_base64(deserializer)?;
    if key.len() != COMPACTION_KEY_BYTES {
        return Err(serde::de::Error::custom("compaction key must be 32 bytes"));
    }
    Ok(key)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PairingQrRecord {
    pub https_origin: String,
    pub challenge_token: String,
    pub expires_at_unix_seconds: u64,
    pub protocol_version: u16,
    pub vault_id: VaultId,
    pub key_epoch: u32,
    pub profile_fingerprint: String,
    pub device_binding: DeviceId,
}

impl PairingQrRecord {
    pub fn validate_at(&self, now_unix_seconds: u64) -> Result<(), EnvelopeError> {
        if !is_strict_https_origin(&self.https_origin) {
            return Err(EnvelopeError::InvalidPairingOrigin);
        }
        if self.challenge_token.len() != 43
            || URL_SAFE_NO_PAD
                .decode(&self.challenge_token)
                .map_or(true, |bytes| bytes.len() != 32)
        {
            return Err(EnvelopeError::InvalidPairingToken);
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(EnvelopeError::UnsupportedVersion);
        }
        if self.key_epoch == 0 {
            return Err(EnvelopeError::InvalidKeyEpoch);
        }
        validate_profile_fingerprint(&self.profile_fingerprint)?;
        if self.device_binding.0.is_nil() {
            return Err(EnvelopeError::InvalidDeviceBinding);
        }
        if self.expires_at_unix_seconds <= now_unix_seconds {
            return Err(EnvelopeError::ExpiredPairingChallenge);
        }
        Ok(())
    }
}

fn is_strict_https_origin(value: &str) -> bool {
    if !value.starts_with("https://") || value.contains(['?', '#', '@', ' ']) {
        return false;
    }
    let authority = &value[8..];
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.is_empty()
        || authority.contains('/')
        || !authority.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')
        })
    {
        return false;
    }
    let (host, port) = if authority.starts_with('[') {
        let Some((host, remainder)) = authority.split_once(']') else {
            return false;
        };
        let port = if remainder.is_empty() {
            None
        } else if let Some(port) = remainder.strip_prefix(':') {
            Some(port)
        } else {
            return false;
        };
        let Some(host) = host.strip_prefix('[') else {
            return false;
        };
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        (host, port)
    } else {
        authority
            .split_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)))
    };
    !host.is_empty() && port.is_none_or(|port| !port.is_empty() && port.parse::<u16>().is_ok())
}
