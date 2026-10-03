//! Shared two-client fixtures: real SQLCipher clients and a minimal in-memory server that
//! assigns contiguous vault cursors exactly like `/v1/events`.
#![allow(dead_code)] // each test crate uses a subset

pub use openpush_client_core::*;
use openpush_crypto::{create_vault_check_header, derive_root_key};
use tempfile::TempDir;

pub const PASSPHRASE: &str = "correct horse battery staple";
pub const DB_KEY: [u8; 32] = [7; 32];
pub const ADDRESS: &str = "+15555550100";

pub struct Vault {
    pub id: VaultId,
    pub profile: KeyProfile,
    pub header: VaultCheckHeader,
}
impl Vault {
    pub fn new() -> Self {
        let id = VaultId::new();
        Self::epoch(id, 1)
    }
    pub fn epoch(id: VaultId, epoch: u32) -> Self {
        let profile = KeyProfile::new(id.0, epoch).unwrap();
        let root = derive_root_key(PASSPHRASE, &profile).unwrap();
        let header = create_vault_check_header(&root, profile.clone()).unwrap();
        Self {
            id,
            profile,
            header,
        }
    }
}

pub fn config(dir: &TempDir, name: &str, vault: &Vault) -> ClientConfig {
    ClientConfig {
        database_path: dir.path().join(format!("{name}.db")),
        vault_id: vault.id,
        device_id: DeviceId::new(),
    }
}
pub fn open(config: &ClientConfig) -> Client {
    Client::open(config.clone(), DatabaseKey::new(&DB_KEY).unwrap()).unwrap()
}
pub fn unlocked(config: &ClientConfig, vault: &Vault) -> Client {
    let client = open(config);
    client
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();
    connect(&client);
    client
}
/// The host handshake with the mock `Server`, which stores compaction headers unchanged and
/// whose whole roster fences snapshots (`compaction_supported` and `compaction_active`).
pub fn connect(client: &Client) {
    client.set_server_compaction_state(true, true).unwrap();
}
pub fn route(gateway: &ClientConfig) -> GatewayRoute {
    GatewayRoute {
        gateway_device_id: gateway.device_id,
        subscription_id: "sim-1".into(),
    }
}
pub fn sms(conversation_id: ConversationId, body: &str) -> OutgoingSms {
    OutgoingSms {
        conversation_id,
        recipients: vec![ADDRESS.into()],
        body: body.into(),
    }
}
pub fn incoming(body: &str, provider_id: &str) -> IncomingSms {
    IncomingSms {
        conversation_id: None,
        sender_address: ADDRESS.into(),
        body: body.into(),
        provider_message_id: Some(provider_id.into()),
        imported: false,
    }
}

/// Vault-wide log with contiguous cursors starting at 1 and envelope-ID idempotency.
#[derive(Default)]
pub struct Server {
    pub log: Vec<Envelope>,
}
impl Server {
    pub fn upload(&mut self, client: &Client) -> usize {
        let pending = client.pending_outbox().unwrap();
        for envelope in &pending {
            if !self
                .log
                .iter()
                .any(|e| e.envelope_id == envelope.envelope_id)
            {
                self.log.push(envelope.clone());
            }
            client.ack_outbox(envelope.envelope_id).unwrap();
        }
        pending.len()
    }
    pub fn sync(&self, client: &Client) -> ApplyReport {
        let from = usize::try_from(client.receive_cursor().unwrap().0).unwrap();
        for (index, envelope) in self.log.iter().enumerate().skip(from) {
            let result = client.ingest(envelope, Cursor(index as u64 + 1)).unwrap();
            assert!(matches!(
                result,
                IngestResult::Journaled | IngestResult::Duplicate
            ));
        }
        assert_eq!(client.receive_cursor().unwrap().0, self.log.len() as u64);
        client.apply_pending(100).unwrap()
    }
}

pub fn only_message(client: &Client, conversation: ConversationId) -> Message {
    let mut messages = client.messages(conversation).unwrap();
    assert_eq!(messages.len(), 1);
    messages.remove(0)
}
