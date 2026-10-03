//! C3: pre-cutover command retirement, native key cache, atomic compose drafts, hardening.
mod common;
use common::*;
use openpush_crypto::{KeyPurpose, derive_purpose_key, derive_root_key, encrypt};
use std::collections::HashMap;
use tempfile::TempDir;

fn states(client: &Client, conversation: ConversationId) -> HashMap<MessageId, Option<SendState>> {
    client
        .messages(conversation)
        .unwrap()
        .into_iter()
        .map(|m| (m.payload.record.message_id, m.send_state))
        .collect()
}

#[test]
fn cutover_retires_commands_received_before_activation_once() {
    let dir = TempDir::new().unwrap();
    let e1 = Vault::new();
    let (e2, e3) = (Vault::epoch(e1.id, 2), Vault::epoch(e1.id, 3));
    let (desktop_cfg, gateway_cfg) = (config(&dir, "desktop", &e1), config(&dir, "gateway", &e1));
    let desktop = unlocked(&desktop_cfg, &e1);
    let gateway = unlocked(&gateway_cfg, &e1);
    let mut server = Server::default();
    let conversation = ConversationId::new();
    let queue = |body: &str| {
        desktop
            .queue_send(sms(conversation, body), route(&gateway_cfg))
            .unwrap()
    };
    let (sent, attempted, waiting) = (queue("sent"), queue("attempted"), queue("waiting"));
    server.upload(&desktop);
    server.sync(&gateway);
    assert_eq!(gateway.pending_commands().unwrap().len(), 3);
    assert!(matches!(
        gateway.begin_send_attempt(sent.command_id).unwrap(),
        PermitDecision::Permit(_)
    ));
    gateway
        .record_send_result(sent.command_id, SendResult::Sent)
        .unwrap();
    assert!(matches!(
        gateway.begin_send_attempt(attempted.command_id).unwrap(),
        PermitDecision::Permit(_)
    ));

    let before = gateway.pending_outbox().unwrap().len();
    gateway.unlock(&e2.profile, &e2.header, PASSPHRASE).unwrap();
    assert_eq!(
        gateway.pending_outbox().unwrap().len(),
        before,
        "unlock alone is not a cutover"
    );
    gateway.activate_epoch(2).unwrap();
    let after = gateway.pending_outbox().unwrap();
    assert_eq!(
        after.len(),
        before + 1,
        "exactly one final status for the never-attempted command"
    );
    assert_eq!(after.last().unwrap().key_epoch, 2);

    assert_eq!(
        gateway.begin_send_attempt(waiting.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::StaleEpoch)
    );
    assert_eq!(
        gateway.begin_send_attempt(sent.command_id).unwrap(),
        PermitDecision::AlreadyAttempted(SendState::Sent)
    );
    assert_eq!(
        gateway.begin_send_attempt(attempted.command_id).unwrap(),
        PermitDecision::AlreadyAttempted(SendState::AttemptRecorded)
    );
    assert!(gateway.pending_commands().unwrap().is_empty());
    let local = states(&gateway, conversation);
    assert_eq!(
        local[&waiting.message_id],
        Some(SendState::FailedBeforeSubmission)
    );
    assert_eq!(local[&sent.message_id], Some(SendState::Sent));
    assert_eq!(
        local[&attempted.message_id],
        Some(SendState::PersistedGateway)
    );
    // Pre-cutover attempts keep reconciling normally.
    gateway
        .record_send_result(attempted.command_id, SendResult::Sent)
        .unwrap();

    assert_eq!(gateway.activate_epoch(2), Err(Error::InvalidProfile));
    server.upload(&gateway);
    desktop.unlock(&e2.profile, &e2.header, PASSPHRASE).unwrap();
    server.sync(&desktop);
    let remote = states(&desktop, conversation);
    assert_eq!(
        remote[&waiting.message_id],
        Some(SendState::FailedBeforeSubmission)
    );
    assert_eq!(remote[&sent.message_id], Some(SendState::Sent));
    assert_eq!(remote[&attempted.message_id], Some(SendState::Sent));

    // A later cutover does not retire the same commands again.
    gateway.unlock(&e3.profile, &e3.header, PASSPHRASE).unwrap();
    gateway.activate_epoch(3).unwrap();
    assert!(gateway.pending_outbox().unwrap().is_empty());
}

#[test]
fn native_key_cache_restores_keys_and_rejects_tamper_device_and_profile() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let e2 = Vault::epoch(vault.id, 2);
    let (desktop_cfg, gateway_cfg) = (
        config(&dir, "desktop", &vault),
        config(&dir, "gateway", &vault),
    );
    let desktop = unlocked(&desktop_cfg, &vault);
    let gateway = unlocked(&gateway_cfg, &vault);
    let mut server = Server::default();
    let cache = gateway
        .export_native_key_cache(1)
        .unwrap()
        .native_storage_bytes()
        .to_vec();
    assert!(matches!(
        gateway.export_native_key_cache(2),
        Err(Error::KeysUnavailable)
    ));
    let desktop_cache = desktop
        .export_native_key_cache(1)
        .unwrap()
        .native_storage_bytes()
        .to_vec();
    drop(gateway);

    let gateway = open(&gateway_cfg);
    let captured = gateway
        .capture_incoming(incoming("restored without passphrase", "c-1"))
        .unwrap();
    let locked = KeyStatus {
        active_epoch: Some(1),
        unlocked_epochs: vec![],
    };
    let reject = |bytes: Vec<u8>, error: Error| {
        let result = gateway.import_native_key_cache(&NativeKeyCache::from_native_storage(bytes));
        assert_eq!(result, Err(error));
        assert_eq!(gateway.key_status().unwrap(), locked);
        assert!(
            gateway.pending_outbox().unwrap().is_empty(),
            "nothing sealed with rejected keys"
        );
    };
    let mutate = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut bytes = cache.clone();
        f(&mut bytes);
        bytes
    };
    reject(mutate(&|b| b[150] ^= 1), Error::InvalidKeyCache);
    // V2 layout: ... | command key 105..137 | event key 137..169 | compaction key 169..201.
    assert_eq!(cache.len(), 201);
    reject(
        mutate(&|b| {
            let (command, event) = b[105..169].split_at_mut(32);
            command.swap_with_slice(event);
        }),
        Error::InvalidKeyCache,
    );
    reject(mutate(&|b| b.truncate(200)), Error::InvalidKeyCache);
    // A V1 version byte on V2-length bytes, and an unknown version, are both rejected.
    reject(mutate(&|b| b[4] = 1), Error::InvalidKeyCache);
    reject(mutate(&|b| b[4] = 3), Error::InvalidKeyCache);
    reject(desktop_cache, Error::IdentityMismatch);
    reject(
        mutate(&|b| b[41] = if b[41] == b'0' { b'1' } else { b'0' }),
        Error::InvalidProfile,
    );
    reject(mutate(&|b| b[40] = 2), Error::InvalidProfile); // epoch 2 has no pinned profile

    gateway
        .import_native_key_cache(&NativeKeyCache::from_native_storage(cache.clone()))
        .unwrap();
    assert_eq!(
        gateway.key_status().unwrap(),
        KeyStatus {
            active_epoch: Some(1),
            unlocked_epochs: vec![1]
        }
    );
    assert_eq!(
        server.upload(&gateway),
        1,
        "locked capture sealed with the restored event key"
    );
    server.sync(&desktop);
    assert_eq!(
        only_message(&desktop, captured.conversation_id)
            .payload
            .body,
        "restored without passphrase"
    );

    // Command, permit and encrypted status all work with restored keys.
    let conversation = ConversationId::new();
    let queued = desktop
        .queue_send(sms(conversation, "via cache"), route(&gateway_cfg))
        .unwrap();
    server.upload(&desktop);
    server.sync(&gateway);
    assert!(matches!(
        gateway.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::Permit(_)
    ));
    gateway
        .record_send_result(queued.command_id, SendResult::Sent)
        .unwrap();
    server.upload(&gateway);
    server.sync(&desktop);
    assert_eq!(
        only_message(&desktop, conversation).send_state,
        Some(SendState::Sent)
    );

    // A cache never establishes a profile on a database without this device's pin.
    let fresh_cfg = ClientConfig {
        database_path: dir.path().join("fresh.db"),
        ..gateway_cfg.clone()
    };
    let fresh = open(&fresh_cfg);
    assert_eq!(
        fresh.import_native_key_cache(&NativeKeyCache::from_native_storage(cache.clone())),
        Err(Error::InvalidProfile)
    );
    assert_eq!(
        fresh.key_status().unwrap(),
        KeyStatus {
            active_epoch: None,
            unlocked_epochs: vec![]
        }
    );

    // After cutover an imported old-epoch cache is decrypt-only and never re-activates.
    gateway.unlock(&e2.profile, &e2.header, PASSPHRASE).unwrap();
    gateway.activate_epoch(2).unwrap();
    let cache2 = gateway
        .export_native_key_cache(2)
        .unwrap()
        .native_storage_bytes()
        .to_vec();
    server.upload(&gateway);
    drop(gateway);
    let gateway = open(&gateway_cfg);
    gateway
        .import_native_key_cache(&NativeKeyCache::from_native_storage(cache))
        .unwrap();
    assert_eq!(
        gateway.key_status().unwrap(),
        KeyStatus {
            active_epoch: Some(2),
            unlocked_epochs: vec![1]
        }
    );
    let old = desktop
        .queue_send(
            sms(conversation, "old epoch after cutover"),
            route(&gateway_cfg),
        )
        .unwrap();
    server.upload(&desktop);
    server.sync(&gateway);
    assert!(
        gateway
            .messages(conversation)
            .unwrap()
            .iter()
            .any(|m| m.payload.body == "old epoch after cutover")
    );
    assert_eq!(
        gateway.begin_send_attempt(old.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::StaleEpoch)
    );
    assert!(
        gateway.pending_outbox().unwrap().is_empty(),
        "epoch-2 status waits for epoch-2 keys"
    );
    gateway
        .import_native_key_cache(&NativeKeyCache::from_native_storage(cache2))
        .unwrap();
    let sealed = gateway.pending_outbox().unwrap();
    assert_eq!((sealed.len(), sealed[0].key_epoch), (1, 2));
}

#[test]
fn compose_draft_send_is_atomic_across_double_submit_and_reopen() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let desktop_cfg = config(&dir, "desktop", &vault);
    let gateway_cfg = config(&dir, "gateway", &vault);
    let desktop = unlocked(&desktop_cfg, &vault);

    let draft = desktop.create_compose_draft(None).unwrap();
    assert_eq!((draft.revision, draft.text.as_str()), (0, ""));
    let update = ComposeDraftUpdate {
        text: "hello from a new conversation".into(),
        recipients: vec![ADDRESS.into()],
        attachment_ids: vec![],
        route: Some(route(&gateway_cfg)),
    };
    let saved = desktop
        .save_compose_draft(draft.draft_id, 0, update.clone())
        .unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(
        desktop.save_compose_draft(
            draft.draft_id,
            0,
            ComposeDraftUpdate {
                text: "stale".into(),
                ..update.clone()
            }
        ),
        Err(Error::StaleDraft {
            current_revision: 1
        })
    );

    drop(desktop);
    let desktop = unlocked(&desktop_cfg, &vault);
    assert_eq!(
        desktop.compose_drafts().unwrap(),
        vec![saved.clone()],
        "new-conversation draft survives restart"
    );
    assert_eq!(
        desktop
            .create_compose_draft(Some(draft.conversation_id))
            .unwrap(),
        saved,
        "stable draft ID"
    );

    // An attachment not prepared on this device leaves the draft and outbox untouched.
    let with_attachment = desktop
        .save_compose_draft(
            draft.draft_id,
            1,
            ComposeDraftUpdate {
                attachment_ids: vec![AttachmentId::new()],
                ..update.clone()
            },
        )
        .unwrap();
    assert_eq!(
        desktop.send_compose_draft(draft.draft_id, 2),
        Err(Error::InvalidRequest(
            "attachment not prepared on this device"
        ))
    );
    assert_eq!(
        desktop.compose_draft(draft.draft_id).unwrap(),
        Some(with_attachment)
    );
    assert!(desktop.pending_outbox().unwrap().is_empty());
    desktop
        .save_compose_draft(draft.draft_id, 2, update.clone())
        .unwrap();

    // Two windows submit the same revision concurrently: exactly one command is queued.
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let desktop = desktop.clone();
                scope.spawn(move || desktop.send_compose_draft(draft.draft_id, 3))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(results.contains(&Err(Error::StaleDraft {
        current_revision: 4
    })));
    let queued = results.into_iter().find_map(Result::ok).unwrap();

    drop(desktop); // crash after the committed send
    let desktop = unlocked(&desktop_cfg, &vault);
    let outbox = desktop.pending_outbox().unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].command_id, Some(queued.command_id));
    let cleared = desktop.compose_draft(draft.draft_id).unwrap().unwrap();
    assert_eq!((cleared.text.as_str(), cleared.revision), ("", 4));
    assert_eq!(cleared.recipients, update.recipients);
    assert_eq!(cleared.route, update.route);
    assert_eq!(
        desktop.send_compose_draft(draft.draft_id, 3),
        Err(Error::StaleDraft {
            current_revision: 4
        })
    );
    assert_eq!(
        desktop.send_compose_draft(draft.draft_id, 4),
        Err(Error::InvalidRequest("body"))
    );
    assert_eq!(desktop.pending_outbox().unwrap().len(), 1);
    let messages = desktop.messages(draft.conversation_id).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].payload.body, "hello from a new conversation");
    assert_eq!(
        desktop.send_compose_draft(DraftId::new(), 0),
        Err(Error::NotFound)
    );
    // Legacy text-only drafts are unaffected.
    assert_eq!(
        desktop
            .save_draft(draft.conversation_id, "legacy", 0)
            .unwrap()
            .revision,
        1
    );
}

#[test]
fn oversize_markers_conflict_and_bounded_outbox_batches() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let client = open(&config(&dir, "device", &vault));
    let oversize = vec![b'a'; 2 * 1024 * 1024 + 1];
    let mut different = oversize.clone();
    different[7] = b'b';
    let malformed = IngestResult::Quarantined(QuarantineReason::MalformedEnvelope);
    assert_eq!(client.ingest_raw(&oversize, Cursor(1)).unwrap(), malformed);
    assert_eq!(
        client.ingest_raw(&oversize, Cursor(1)).unwrap(),
        IngestResult::Duplicate
    );
    assert_eq!(
        client.ingest_raw(&different, Cursor(1)),
        Err(Error::Conflict)
    );
    assert_eq!(client.receive_cursor().unwrap(), Cursor(1));

    client
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();
    for n in 0..3 {
        client
            .capture_incoming(incoming("batch", &format!("p-{n}")))
            .unwrap();
    }
    let all = client.pending_outbox().unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(client.pending_outbox_batch(2).unwrap(), all[..2].to_vec());
    assert!(client.pending_outbox_batch(0).unwrap().is_empty());
}

#[test]
fn authenticated_status_with_out_of_range_sequence_is_quarantined_not_stalled() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let client = unlocked(&config(&dir, "desktop", &vault), &vault);
    let root = derive_root_key(PASSPHRASE, &vault.profile).unwrap();
    let key = derive_purpose_key(&root, &vault.profile, KeyPurpose::Event).unwrap();
    let mut envelope = Envelope {
        protocol_version: 1,
        envelope_id: EnvelopeId::new(),
        command_id: None,
        vault_id: vault.id,
        producer_device_id: DeviceId::new(),
        producer_sequence: SourceSequence(u64::MAX),
        key_epoch: 1,
        crypto_suite: vault.profile.crypto_suite,
        profile_fingerprint: vault.profile.fingerprint().unwrap(),
        purpose: EnvelopePurpose::Event,
        route: None,
        compaction: None,
        ciphertext: Vec::new(),
    };
    let plain = serde_json::to_vec(&PrivatePayload::SendStatus {
        command_id: CommandId::new(),
        state: SendState::Sent,
    })
    .unwrap();
    let sealed = encrypt(&key, &envelope.aad_bytes().unwrap(), &plain).unwrap();
    envelope.ciphertext = [sealed.nonce.as_slice(), &sealed.ciphertext].concat();
    assert_eq!(
        client.ingest(&envelope, Cursor(1)).unwrap(),
        IngestResult::Journaled
    );

    let peer = unlocked(&config(&dir, "gateway", &vault), &vault);
    let captured = peer
        .capture_incoming(incoming("after poison", "p-1"))
        .unwrap();
    assert_eq!(
        client
            .ingest(&peer.pending_outbox().unwrap()[0], Cursor(2))
            .unwrap(),
        IngestResult::Journaled
    );
    let report = client.apply_pending(10).unwrap();
    assert_eq!((report.applied, report.quarantined), (1, 1));
    assert_eq!(
        client.quarantined().unwrap()[0].reason,
        QuarantineReason::InvalidPayload
    );
    assert_eq!(
        only_message(&client, captured.conversation_id).payload.body,
        "after poison"
    );
}
