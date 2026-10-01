//! Two-client SMS checkpoint tests through the public API and a minimal in-memory server that
//! assigns contiguous vault cursors exactly like `/v1/events`.
mod common;
use common::*;
use tempfile::TempDir;

#[test]
fn desktop_send_reaches_gateway_once_and_encrypted_status_returns() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let (desktop_cfg, gateway_cfg) = (
        config(&dir, "desktop", &vault),
        config(&dir, "gateway", &vault),
    );
    let desktop = unlocked(&desktop_cfg, &vault);
    let gateway = unlocked(&gateway_cfg, &vault);
    let mut server = Server::default();
    let conversation = ConversationId::new();
    let body = "unique-plaintext-body-1234";

    let queued = desktop
        .queue_send(sms(conversation, body), route(&gateway_cfg))
        .unwrap();
    assert_eq!(
        only_message(&desktop, conversation).send_state,
        Some(SendState::QueuedLocal)
    );
    let wire = desktop.pending_outbox().unwrap();
    assert_eq!(wire.len(), 1);
    assert_eq!(wire[0].envelope_id, queued.envelope_id);
    assert_eq!(wire[0].command_id, Some(queued.command_id));
    assert!(!serde_json::to_string(&wire[0]).unwrap().contains(body));
    assert_eq!(
        desktop.pending_outbox().unwrap(),
        wire,
        "retries are byte-identical"
    );

    assert_eq!(server.upload(&desktop), 1);
    assert!(desktop.pending_outbox().unwrap().is_empty());
    assert_eq!(
        only_message(&desktop, conversation).send_state,
        Some(SendState::AcceptedServer)
    );
    assert_eq!(desktop.ack_outbox(EnvelopeId::new()), Err(Error::NotFound));

    // Gateway journals, decrypts, ledgers and receipts the command.
    let report = server.sync(&gateway);
    assert_eq!(report.applied, 1);
    let commands = gateway.pending_commands().unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].message.body, body);
    assert_eq!(commands[0].subscription_id, "sim-1");

    let PermitDecision::Permit(permit) = gateway.begin_send_attempt(queued.command_id).unwrap()
    else {
        panic!("first attempt must be permitted");
    };
    assert_eq!(permit.message.recipients, vec![ADDRESS.to_string()]);
    assert_eq!(
        gateway.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::AlreadyAttempted(SendState::AttemptRecorded),
        "a second call never produces a second permit"
    );
    assert!(gateway.pending_commands().unwrap().is_empty());
    assert_eq!(
        gateway.begin_send_attempt(CommandId::new()).unwrap(),
        PermitDecision::Blocked(PermitBlock::NotReceived)
    );
    // The desktop itself never ledgers a command routed to the gateway.
    assert_eq!(
        desktop.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::NotReceived)
    );

    assert_eq!(
        gateway
            .record_send_result(queued.command_id, SendResult::Submitted)
            .unwrap(),
        SendState::SubmittedToOs
    );
    assert_eq!(
        gateway
            .record_send_result(queued.command_id, SendResult::Sent)
            .unwrap(),
        SendState::Sent
    );
    let outbox_before = gateway.pending_outbox().unwrap().len();
    assert_eq!(
        gateway
            .record_send_result(queued.command_id, SendResult::Sent)
            .unwrap(),
        SendState::Sent
    );
    assert_eq!(
        gateway.pending_outbox().unwrap().len(),
        outbox_before,
        "repeated result emits nothing"
    );
    assert_eq!(
        gateway.record_send_result(queued.command_id, SendResult::FailedBeforeSubmission),
        Err(Error::IllegalTransition {
            from: SendState::Sent,
            to: SendState::FailedBeforeSubmission
        })
    );
    assert_eq!(
        gateway.record_send_result(CommandId::new(), SendResult::Sent),
        Err(Error::NoAttempt)
    );
    assert_eq!(
        only_message(&gateway, conversation).send_state,
        Some(SendState::Sent)
    );

    // persisted_gateway + submitted + sent statuses.
    assert_eq!(server.upload(&gateway), 3);
    server.sync(&desktop);
    let message = only_message(&desktop, conversation);
    assert_eq!(message.send_state, Some(SendState::Sent));
    assert!(message.seen);
    assert_eq!(desktop.unread_count(conversation).unwrap(), 0);

    // Full replay from the server is idempotent on both clients.
    for client in [&desktop, &gateway] {
        for (index, envelope) in server.log.iter().enumerate() {
            assert_eq!(
                client.ingest(envelope, Cursor(index as u64 + 1)).unwrap(),
                IngestResult::Duplicate
            );
        }
        assert_eq!(client.apply_pending(100).unwrap(), ApplyReport::default());
    }
    assert_eq!(
        gateway.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::AlreadyAttempted(SendState::Sent)
    );
    assert!(desktop.quarantined().unwrap().is_empty());
    assert!(gateway.quarantined().unwrap().is_empty());
}

#[test]
fn locked_capture_survives_reopen_and_is_sealed_exactly_once() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let gateway_cfg = config(&dir, "gateway", &vault);
    let gateway = open(&gateway_cfg);
    assert_eq!(
        gateway.key_status().unwrap(),
        KeyStatus {
            active_epoch: None,
            unlocked_epochs: vec![]
        }
    );

    let captured = gateway
        .capture_incoming(incoming("locked hello", "sms-1"))
        .unwrap();
    assert!(!captured.duplicate);
    assert_eq!(gateway.unread_count(captured.conversation_id).unwrap(), 1);
    assert!(
        gateway.pending_outbox().unwrap().is_empty(),
        "nothing is encrypted while locked"
    );
    let again = gateway
        .capture_incoming(incoming("locked hello", "sms-1"))
        .unwrap();
    assert!(again.duplicate);
    assert_eq!(again.message_id, captured.message_id);

    drop(gateway);
    let gateway = open(&gateway_cfg);
    assert_eq!(
        only_message(&gateway, captured.conversation_id)
            .payload
            .body,
        "locked hello"
    );
    assert!(gateway.pending_outbox().unwrap().is_empty());
    gateway
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();
    let sealed = gateway.pending_outbox().unwrap();
    assert_eq!(sealed.len(), 1);

    drop(gateway);
    let gateway = open(&gateway_cfg);
    assert_eq!(
        gateway.pending_outbox().unwrap(),
        sealed,
        "sealed wire needs no keys and is never re-sealed"
    );
    gateway
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();
    assert_eq!(gateway.pending_outbox().unwrap(), sealed);

    let mut server = Server::default();
    server.upload(&gateway);
    let desktop = unlocked(&config(&dir, "desktop", &vault), &vault);
    server.sync(&desktop);
    let message = only_message(&desktop, captured.conversation_id);
    assert_eq!(message.payload.record.message_id, captured.message_id);
    assert!(!message.seen);
    assert_eq!(desktop.unread_count(captured.conversation_id).unwrap(), 1);
    assert_eq!(
        server.sync(&gateway).applied,
        0,
        "own echo is a duplicate, not a second message"
    );
}

#[test]
fn read_before_arrival_out_of_order_buffering_and_duplicate_replay() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let gateway = unlocked(&config(&dir, "gateway", &vault), &vault);
    let first = unlocked(&config(&dir, "desktop-1", &vault), &vault);
    let second = unlocked(&config(&dir, "desktop-2", &vault), &vault);
    let mut server = Server::default();

    let captured = gateway
        .capture_incoming(incoming("hello", "sms-7"))
        .unwrap();
    server.upload(&gateway);
    server.sync(&first);
    assert_eq!(first.unread_count(captured.conversation_id).unwrap(), 1);
    assert_eq!(
        first.ingest(&server.log[0], Cursor(1)).unwrap(),
        IngestResult::Duplicate
    );
    assert_eq!(first.apply_pending(10).unwrap().applied, 0);
    assert_eq!(
        first.unread_count(captured.conversation_id).unwrap(),
        1,
        "duplicate never double counts"
    );
    assert_eq!(
        first.list_conversations().unwrap(),
        vec![Conversation {
            conversation_id: captured.conversation_id,
            unread_count: 1
        }]
    );

    assert!(first.mark_seen(captured.message_id).unwrap());
    assert!(!first.mark_seen(captured.message_id).unwrap());
    assert_eq!(first.mark_seen(MessageId::new()), Err(Error::NotFound));
    assert_eq!(server.upload(&first), 1);
    let (message_event, read_event) = (server.log[0].clone(), server.log[1].clone());

    // The second desktop sees the read state before the message, and cursor 2 before cursor 1.
    assert_eq!(
        second.ingest(&message_event, Cursor(2)).unwrap(),
        IngestResult::Journaled
    );
    assert_eq!(second.receive_cursor().unwrap(), Cursor(0));
    assert_eq!(
        second.apply_pending(10).unwrap(),
        ApplyReport::default(),
        "buffered beyond the frontier"
    );
    assert_eq!(
        second.ingest(&read_event, Cursor(1)).unwrap(),
        IngestResult::Journaled
    );
    assert_eq!(second.receive_cursor().unwrap(), Cursor(2));
    assert_eq!(second.apply_pending(10).unwrap().applied, 2);
    assert!(only_message(&second, captured.conversation_id).seen);
    assert_eq!(second.unread_count(captured.conversation_id).unwrap(), 0);

    // Same envelope at another cursor is a duplicate; a different record at a used cursor conflicts.
    assert_eq!(
        second.ingest(&message_event, Cursor(3)).unwrap(),
        IngestResult::Duplicate
    );
    assert_eq!(second.receive_cursor().unwrap(), Cursor(3));
    assert_eq!(second.ingest(&read_event, Cursor(3)), Err(Error::Conflict));
    assert_eq!(second.messages(captured.conversation_id).unwrap().len(), 1);

    // The gateway applies the remote read state to its own capture.
    server.sync(&gateway);
    assert!(only_message(&gateway, captured.conversation_id).seen);

    // Imported history starts read.
    let imported = gateway
        .capture_incoming(IncomingSms {
            imported: true,
            ..incoming("old", "sms-1")
        })
        .unwrap();
    assert_eq!(
        imported.conversation_id, captured.conversation_id,
        "resolved by sender address"
    );
    assert_eq!(gateway.unread_count(captured.conversation_id).unwrap(), 0);
}

#[test]
fn wrong_key_profile_passphrase_and_cross_vault_never_reset_or_apply() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let gateway_cfg = config(&dir, "gateway", &vault);
    let gateway = open(&gateway_cfg);

    assert_eq!(
        gateway.unlock(&vault.profile, &vault.header, "wrong horse battery staple"),
        Err(Error::WrongPassphrase)
    );
    assert_eq!(
        gateway.unlock(&vault.profile, &vault.header, " correct horse"),
        Err(Error::InvalidPassphrase)
    );
    assert!(gateway.key_status().unwrap().unlocked_epochs.is_empty());
    gateway
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();

    // A same-epoch profile with a different salt is rejected before any KDF and keeps the pin.
    let mut tampered = vault.profile.clone();
    tampered.salt[0] ^= 1;
    let tampered_header = VaultCheckHeader {
        profile: tampered.clone(),
        check: vault.header.check.clone(),
    };
    assert_eq!(
        gateway.unlock(&tampered, &tampered_header, PASSPHRASE),
        Err(Error::InvalidProfile)
    );
    let other = Vault::new();
    assert_eq!(
        gateway.unlock(&other.profile, &other.header, PASSPHRASE),
        Err(Error::InvalidProfile)
    );
    let captured = gateway
        .capture_incoming(incoming("still works", "sms-1"))
        .unwrap();
    assert_eq!(gateway.pending_outbox().unwrap().len(), 1);

    // Shared owner: wrong key or identity is rejected and the owner keeps working.
    assert_eq!(
        Client::open(gateway_cfg.clone(), DatabaseKey::new(&[8; 32]).unwrap()).err(),
        Some(Error::WrongDatabaseKey)
    );
    let wrong_device = ClientConfig {
        device_id: DeviceId::new(),
        ..gateway_cfg.clone()
    };
    assert_eq!(
        Client::open(wrong_device.clone(), DatabaseKey::new(&DB_KEY).unwrap()).err(),
        Some(Error::IdentityMismatch)
    );
    assert_eq!(gateway.unread_count(captured.conversation_id).unwrap(), 1);
    drop(gateway);
    // Fresh connection: SQLCipher rejects the wrong key; identity is persisted.
    assert_eq!(
        Client::open(gateway_cfg.clone(), DatabaseKey::new(&[8; 32]).unwrap()).err(),
        Some(Error::WrongDatabaseKey)
    );
    assert_eq!(
        Client::open(wrong_device, DatabaseKey::new(&DB_KEY).unwrap()).err(),
        Some(Error::IdentityMismatch)
    );
    let gateway = open(&gateway_cfg);
    assert_eq!(
        gateway.unread_count(captured.conversation_id).unwrap(),
        1,
        "no reset"
    );
    gateway
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();

    // Cross-vault and poison records are quarantined while the frontier advances.
    let foreign = unlocked(&config(&dir, "foreign", &other), &other);
    foreign
        .capture_incoming(incoming("foreign", "f-1"))
        .unwrap();
    let foreign_envelope = foreign.pending_outbox().unwrap().remove(0);
    assert_eq!(
        gateway.ingest(&foreign_envelope, Cursor(1)).unwrap(),
        IngestResult::Quarantined(QuarantineReason::WrongVault)
    );
    assert_eq!(
        gateway.ingest_raw(b"{not json", Cursor(2)).unwrap(),
        IngestResult::Quarantined(QuarantineReason::MalformedEnvelope)
    );
    assert_eq!(
        gateway.ingest(&foreign_envelope, Cursor(0)),
        Err(Error::InvalidCursor)
    );

    // Same-vault ciphertext tampering fails authentication at apply time, never promoting a body.
    let desktop = unlocked(&config(&dir, "desktop", &vault), &vault);
    let conversation = ConversationId::new();
    desktop
        .queue_send(sms(conversation, "tamper me"), route(&gateway_cfg))
        .unwrap();
    let genuine = desktop.pending_outbox().unwrap().remove(0);
    let mut forged = genuine.clone();
    let last = forged.ciphertext.len() - 1;
    forged.ciphertext[last] ^= 1;
    assert_eq!(
        gateway.ingest(&forged, Cursor(3)).unwrap(),
        IngestResult::Journaled
    );
    assert_eq!(gateway.receive_cursor().unwrap(), Cursor(3));
    let report = gateway.apply_pending(10).unwrap();
    assert_eq!((report.applied, report.quarantined), (0, 1));
    assert!(gateway.messages(conversation).unwrap().is_empty());
    assert!(gateway.pending_commands().unwrap().is_empty());
    assert_eq!(
        gateway.ingest(&genuine, Cursor(4)).unwrap(),
        IngestResult::Quarantined(QuarantineReason::EnvelopeConflict)
    );
    let reasons: Vec<_> = gateway
        .quarantined()
        .unwrap()
        .into_iter()
        .map(|q| (q.cursor.0, q.reason))
        .collect();
    assert_eq!(
        reasons,
        vec![
            (1, QuarantineReason::WrongVault),
            (2, QuarantineReason::MalformedEnvelope),
            (3, QuarantineReason::AuthenticationFailed),
            (4, QuarantineReason::EnvelopeConflict),
        ]
    );
    assert_eq!(gateway.receive_cursor().unwrap(), Cursor(4));
}

#[test]
fn crash_after_permit_reopens_as_unknown_without_a_second_permit() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let (desktop_cfg, gateway_cfg) = (
        config(&dir, "desktop", &vault),
        config(&dir, "gateway", &vault),
    );
    let desktop = unlocked(&desktop_cfg, &vault);
    let gateway = unlocked(&gateway_cfg, &vault);
    let mut server = Server::default();
    let conversation = ConversationId::new();
    let queued = desktop
        .queue_send(sms(conversation, "maybe sent"), route(&gateway_cfg))
        .unwrap();
    server.upload(&desktop);
    server.sync(&gateway);
    assert!(matches!(
        gateway.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::Permit(_)
    ));
    drop(gateway); // crash between journal and carrier result

    let gateway = open(&gateway_cfg);
    assert_eq!(
        gateway.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::AlreadyAttempted(SendState::OutcomeUnknown)
    );
    assert!(gateway.pending_commands().unwrap().is_empty());
    assert_eq!(
        gateway.pending_outbox().unwrap().len(),
        1,
        "unknown status waits for keys"
    );
    gateway
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();
    assert_eq!(gateway.pending_outbox().unwrap().len(), 2);
    server.upload(&gateway);
    server.sync(&desktop);
    assert_eq!(
        only_message(&desktop, conversation).send_state,
        Some(SendState::OutcomeUnknown)
    );

    drop(gateway);
    let gateway = open(&gateway_cfg);
    assert_eq!(
        gateway.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::AlreadyAttempted(SendState::OutcomeUnknown),
        "reopen again neither re-permits nor re-emits"
    );
    assert!(gateway.pending_outbox().unwrap().is_empty());
    // OS evidence reconciles the unknown outcome.
    gateway
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();
    assert_eq!(
        gateway
            .record_send_result(queued.command_id, SendResult::Sent)
            .unwrap(),
        SendState::Sent
    );
    server.upload(&gateway);
    server.sync(&desktop);
    assert_eq!(
        only_message(&desktop, conversation).send_state,
        Some(SendState::Sent)
    );
}

#[test]
fn epoch_cutover_blocks_old_commands_and_keeps_old_history_readable() {
    let dir = TempDir::new().unwrap();
    let epoch1 = Vault::new();
    let epoch2 = Vault::epoch(epoch1.id, 2);
    let (desktop_cfg, gateway_cfg) = (
        config(&dir, "desktop", &epoch1),
        config(&dir, "gateway", &epoch1),
    );
    let desktop = unlocked(&desktop_cfg, &epoch1);
    let gateway = unlocked(&gateway_cfg, &epoch1);
    let mut server = Server::default();
    let conversation = ConversationId::new();
    let queued = desktop
        .queue_send(sms(conversation, "old epoch"), route(&gateway_cfg))
        .unwrap();
    server.upload(&desktop);

    gateway
        .unlock(&epoch2.profile, &epoch2.header, PASSPHRASE)
        .unwrap();
    assert_eq!(
        gateway.key_status().unwrap().active_epoch,
        Some(1),
        "newer epoch needs explicit cutover"
    );
    gateway.activate_epoch(2).unwrap();
    assert_eq!(gateway.activate_epoch(1), Err(Error::InvalidProfile));
    server.sync(&gateway);
    assert_eq!(
        only_message(&gateway, conversation).payload.body,
        "old epoch",
        "old history still decrypts"
    );
    assert!(gateway.pending_commands().unwrap().is_empty());
    assert_eq!(
        gateway.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::StaleEpoch)
    );
    // The receipt is sealed under epoch 2; the epoch-1-only desktop waits rather than failing.
    server.upload(&gateway);
    let report = server.sync(&desktop);
    assert_eq!(report.waiting_for_keys, 1);
    assert!(desktop.quarantined().unwrap().is_empty());
}
