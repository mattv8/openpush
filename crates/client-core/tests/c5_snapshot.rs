//! C5: staged snapshot resync, historical commands and the restore execution guard.
mod common;
use common::*;
use std::fs;
use tempfile::TempDir;

/// The agreed server shape: `{cursor, envelope}` records below a fixed high-water.
fn snapshot(server: &Server) -> Vec<SnapshotRecord> {
    server
        .log
        .iter()
        .enumerate()
        .map(|(i, envelope)| SnapshotRecord {
            cursor: Cursor(i as u64 + 1),
            envelope: envelope.clone(),
        })
        .collect()
}
fn import(
    client: &Client,
    records: &[SnapshotRecord],
    high_water: u64,
    purpose: SnapshotPurpose,
) -> SnapshotReport {
    let session = client
        .begin_snapshot(Cursor(high_water), records.len() as u64, purpose)
        .unwrap();
    for page in records.chunks(2) {
        client
            .append_snapshot_page(session.generation, page)
            .unwrap();
    }
    client.finish_snapshot(session.generation).unwrap()
}
/// Native upload stand-in (no network claim): reports a random server object ID.
fn upload_media(client: &Client) {
    for object in client.pending_uploads().unwrap() {
        assert!(fs::metadata(client.native_cipher_file(object.attachment_id).unwrap()).is_ok());
        client
            .mark_attachment_uploaded(object.attachment_id, &uuid::Uuid::new_v4().to_string())
            .unwrap();
    }
}

#[test]
fn new_device_resyncs_history_read_and_media_without_touching_local_work() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let gateway_cfg = config(&dir, "gateway", &vault);
    let gateway = unlocked(&gateway_cfg, &vault);
    let reader = unlocked(&config(&dir, "reader", &vault), &vault);
    let fresh_cfg = config(&dir, "fresh", &vault);
    let fresh = unlocked(&fresh_cfg, &vault);
    let mut server = Server::default();

    let history = gateway
        .capture_incoming(incoming("history", "sms-1"))
        .unwrap();
    let part = dir.path().join("part.png");
    fs::write(&part, b"\x89PNG\r\n\x1a\nsynthetic").unwrap();
    let info = gateway
        .prepare_attachment(&part, "image/png", "part.png")
        .unwrap();
    let mms = gateway
        .capture_incoming_mms(IncomingMms {
            conversation_id: None,
            sender_address: "+15555550199".into(),
            recipients: vec!["+15555550199".into()],
            subject: None,
            body: "pic".into(),
            provider_message_id: Some("mms-1".into()),
            imported: false,
            attachment_ids: vec![info.attachment_id],
        })
        .unwrap();
    upload_media(&gateway);
    server.upload(&gateway);
    server.sync(&reader);
    assert!(reader.mark_seen(history.message_id).unwrap());
    server.upload(&reader);
    assert_eq!(server.log.len(), 3);

    // Local work on the new device that a snapshot must never disturb.
    let local_conversation = ConversationId::new();
    let queued = fresh
        .queue_send(sms(local_conversation, "offline send"), route(&gateway_cfg))
        .unwrap();
    let draft = fresh.create_compose_draft(None).unwrap();
    let draft = fresh
        .save_compose_draft(
            draft.draft_id,
            0,
            ComposeDraftUpdate {
                text: "unsent".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let local_part = dir.path().join("local.png");
    fs::write(&local_part, b"local media").unwrap();
    let local_media = fresh
        .prepare_attachment(&local_part, "image/png", "local.png")
        .unwrap();
    let outbox_before = fresh.pending_outbox().unwrap();
    let conversations_before = fresh.list_conversations().unwrap();

    // Interrupted after one page: live state is untouched, and staging survives reopen.
    let records = snapshot(&server);
    let session = fresh
        .begin_snapshot(Cursor(3), 3, SnapshotPurpose::Resync)
        .unwrap();
    fresh
        .append_snapshot_page(session.generation, &records[..2])
        .unwrap();
    assert_eq!(fresh.receive_cursor().unwrap(), Cursor(0));
    assert_eq!(fresh.list_conversations().unwrap(), conversations_before);
    drop(fresh);
    let fresh = unlocked(&fresh_cfg, &vault);
    let progress = fresh.snapshot_progress().unwrap().unwrap();
    assert_eq!(
        (progress.received_records, progress.last_cursor),
        (2, Cursor(2))
    );
    assert_eq!(fresh.list_conversations().unwrap(), conversations_before);
    fresh
        .append_snapshot_page(session.generation, &records[2..])
        .unwrap();
    let report = fresh.finish_snapshot(session.generation).unwrap();
    assert_eq!(
        (
            report.journaled,
            report.duplicate,
            report.quarantined,
            report.receive_cursor
        ),
        (3, 0, 0, 3)
    );
    assert_eq!(fresh.snapshot_progress().unwrap(), None);
    assert_eq!(fresh.apply_pending(100).unwrap().applied, 3);

    // History, read state and media metadata arrived; local work did not change.
    assert!(only_message(&fresh, history.conversation_id).seen);
    assert_eq!(fresh.unread_count(mms.conversation_id).unwrap(), 1);
    assert!(
        only_message(&fresh, mms.conversation_id)
            .payload
            .record
            .attachments[0]
            .pending
    );
    assert_eq!(
        fresh.attachment_info(info.attachment_id).unwrap().state,
        AttachmentState::PendingDownload
    );
    assert_eq!(fresh.pending_outbox().unwrap(), outbox_before);
    assert_eq!(outbox_before[0].command_id, Some(queued.command_id));
    assert_eq!(fresh.compose_draft(draft.draft_id).unwrap(), Some(draft));
    assert_eq!(
        fresh
            .attachment_info(local_media.attachment_id)
            .unwrap()
            .state,
        AttachmentState::PendingUpload
    );
    assert_eq!(
        only_message(&fresh, local_conversation).send_state,
        Some(SendState::QueuedLocal)
    );

    // Repeating the import neither inflates unread nor emits anything.
    let again = import(&fresh, &records, 3, SnapshotPurpose::Resync);
    assert_eq!((again.journaled, again.duplicate), (0, 3));
    let replay = fresh.apply_pending(100).unwrap();
    assert_eq!(
        (
            replay.applied,
            replay.quarantined,
            replay.drained,
            replay.snapshot_remaining
        ),
        (0, 0, 3, 0),
        "replayed records drain as duplicates and apply nothing"
    );
    assert_eq!(fresh.unread_count(mms.conversation_id).unwrap(), 1);
    assert_eq!(fresh.pending_outbox().unwrap(), outbox_before);

    // Live replay continues at high-water + 1.
    let live = gateway
        .capture_incoming(incoming("after snapshot", "sms-2"))
        .unwrap();
    server.upload(&gateway);
    server.sync(&fresh);
    assert_eq!(fresh.receive_cursor().unwrap(), Cursor(4));
    assert_eq!(fresh.messages(live.conversation_id).unwrap().len(), 2);
}

#[test]
fn snapshot_commands_are_historical_and_restore_guard_blocks_new_permits() {
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
    let unknown = desktop
        .queue_send(sms(conversation, "attempted"), route(&gateway_cfg))
        .unwrap();
    let live = desktop
        .queue_send(
            sms(conversation, "live, not attempted"),
            route(&gateway_cfg),
        )
        .unwrap();
    server.upload(&desktop);
    server.sync(&gateway);
    assert!(matches!(
        gateway.begin_send_attempt(unknown.command_id).unwrap(),
        PermitDecision::Permit(_)
    ));
    drop(gateway); // crash after permit
    let gateway = unlocked(&gateway_cfg, &vault);
    server.upload(&gateway); // two receipts + one unknown status
    let historical = desktop
        .queue_send(sms(conversation, "only in snapshot"), route(&gateway_cfg))
        .unwrap();
    server.upload(&desktop);
    assert_eq!(server.log.len(), 6);

    let report = import(&gateway, &snapshot(&server), 6, SnapshotPurpose::Resync);
    assert_eq!(
        (report.journaled, report.duplicate, report.receive_cursor),
        (4, 2, 6)
    );
    assert_eq!(gateway.apply_pending(100).unwrap().applied, 1);
    assert!(
        gateway.pending_outbox().unwrap().is_empty(),
        "no receipt for a historical command"
    );
    assert!(!gateway.restore_guarded().unwrap());
    assert_eq!(
        gateway.begin_send_attempt(historical.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::Historical)
    );
    assert_eq!(
        gateway.begin_send_attempt(unknown.command_id).unwrap(),
        PermitDecision::AlreadyAttempted(SendState::OutcomeUnknown)
    );
    let pending: Vec<_> = gateway
        .pending_commands()
        .unwrap()
        .into_iter()
        .map(|c| c.command_id)
        .collect();
    assert_eq!(pending, vec![live.command_id]);
    let reasons = |client: &Client| -> Vec<_> {
        client
            .commands_needing_reconciliation()
            .unwrap()
            .into_iter()
            .map(|i| (i.command_id, i.reason))
            .collect()
    };
    assert_eq!(
        reasons(&gateway),
        vec![
            (unknown.command_id, ReconciliationReason::OutcomeUnknown),
            (historical.command_id, ReconciliationReason::Historical)
        ]
    );
    server.sync(&desktop);
    assert_eq!(
        desktop
            .messages(conversation)
            .unwrap()
            .iter()
            .find(|m| m.payload.body == "only in snapshot")
            .unwrap()
            .send_state,
        Some(SendState::AcceptedServer),
        "the gateway never claims to have persisted a historical command"
    );

    // Restored gateway: durable guard, no new permits, known attempts unchanged.
    gateway.mark_restored_gateway().unwrap();
    drop(gateway);
    let gateway = unlocked(&gateway_cfg, &vault);
    assert!(gateway.restore_guarded().unwrap());
    assert!(gateway.pending_commands().unwrap().is_empty());
    assert_eq!(
        gateway.begin_send_attempt(live.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::RestoreGuarded)
    );
    assert_eq!(
        gateway.begin_send_attempt(unknown.command_id).unwrap(),
        PermitDecision::AlreadyAttempted(SendState::OutcomeUnknown)
    );
    assert_eq!(
        reasons(&gateway),
        vec![
            (unknown.command_id, ReconciliationReason::OutcomeUnknown),
            (live.command_id, ReconciliationReason::RestoreGuarded),
            (historical.command_id, ReconciliationReason::Historical)
        ]
    );
    // Reconciliation with OS evidence is still allowed.
    assert_eq!(
        gateway
            .record_send_result(unknown.command_id, SendResult::Sent)
            .unwrap(),
        SendState::Sent
    );
}

#[test]
fn inconsistent_snapshots_fail_closed_and_missing_keys_wait() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let source = unlocked(&config(&dir, "source", &vault), &vault);
    let envelopes: Vec<_> = (0..3)
        .map(|n| {
            source
                .capture_incoming(incoming(&format!("m{n}"), &format!("p{n}")))
                .unwrap();
            source.pending_outbox_batch(1).unwrap().remove(0)
        })
        .inspect(|e| source.ack_outbox(e.envelope_id).unwrap())
        .collect();
    let record = |cursor: u64, n: usize| SnapshotRecord {
        cursor: Cursor(cursor),
        envelope: envelopes[n].clone(),
    };
    let target_cfg = config(&dir, "target", &vault);
    let target = open(&target_cfg); // locked: snapshot import needs no keys
    assert_eq!(
        target.ingest(&envelopes[0], Cursor(1)).unwrap(),
        IngestResult::Journaled
    );
    let live_state = |c: &Client| {
        (
            c.receive_cursor().unwrap(),
            c.list_conversations().unwrap(),
            c.quarantined().unwrap(),
        )
    };
    let before = live_state(&target);
    let expect_closed = |result: Result<SnapshotProgress, Error>| {
        assert_eq!(result.err(), Some(Error::SnapshotMismatch))
    };

    assert!(matches!(
        target.begin_snapshot(Cursor(0), 1, SnapshotPurpose::Resync),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        target.begin_snapshot(Cursor(2), 3, SnapshotPurpose::Resync),
        Err(Error::InvalidRequest(_))
    ));
    let s = target
        .begin_snapshot(Cursor(3), 2, SnapshotPurpose::Resync)
        .unwrap();
    assert!(matches!(
        target.append_snapshot_page(s.generation, &[]),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        target.append_snapshot_page(s.generation, &vec![record(1, 0); 501]),
        Err(Error::InvalidRequest(_))
    ));
    expect_closed(target.append_snapshot_page(s.generation + 1, &[record(1, 0)])); // stale generation
    expect_closed(target.append_snapshot_page(s.generation, &[record(2, 1), record(1, 0)])); // decreasing
    assert_eq!(
        target.snapshot_progress().unwrap(),
        None,
        "failed sessions are not resumable"
    );
    expect_closed(target.append_snapshot_page(s.generation, &[record(1, 0)]));
    assert_eq!(
        target.finish_snapshot(s.generation).err(),
        Some(Error::SnapshotMismatch)
    );

    let s = target
        .begin_snapshot(Cursor(3), 2, SnapshotPurpose::Resync)
        .unwrap();
    expect_closed(target.append_snapshot_page(s.generation, &[record(4, 2)])); // beyond high-water
    let s = target
        .begin_snapshot(Cursor(3), 2, SnapshotPurpose::Resync)
        .unwrap();
    expect_closed(
        target.append_snapshot_page(s.generation, &[record(1, 0), record(2, 1), record(3, 2)]),
    ); // over count
    let s = target
        .begin_snapshot(Cursor(3), 3, SnapshotPurpose::Resync)
        .unwrap();
    target
        .append_snapshot_page(s.generation, &[record(1, 0), record(2, 1)])
        .unwrap();
    assert_eq!(
        target.finish_snapshot(s.generation).err(),
        Some(Error::SnapshotMismatch)
    ); // short count
    let s = target
        .begin_snapshot(Cursor(3), 2, SnapshotPurpose::Resync)
        .unwrap();
    // Different bytes at an already journaled cursor: hard conflict at append.
    expect_closed(target.append_snapshot_page(s.generation, &[record(1, 1), record(3, 2)]));
    assert_eq!(
        live_state(&target),
        before,
        "every failure left live state unchanged"
    );
    assert!(!target.restore_guarded().unwrap());

    // Valid import while locked: records are durable and wait for keys.
    let report = import(
        &target,
        &[record(1, 0), record(2, 1), record(3, 2)],
        3,
        SnapshotPurpose::Restore,
    );
    assert_eq!(
        (report.journaled, report.duplicate, report.receive_cursor),
        (2, 1, 3)
    );
    assert!(
        target.restore_guarded().unwrap(),
        "restore imports set the guard"
    );
    assert_eq!(target.apply_pending(100).unwrap().waiting_for_keys, 3);

    // Locked captures beyond the implicit sealing bound are sealed in explicit batches.
    for n in 0..300 {
        target
            .capture_incoming(incoming("locked", &format!("bulk-{n}")))
            .unwrap();
    }
    target
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();
    assert_eq!(target.pending_outbox().unwrap().len(), MAX_SEAL_BATCH);
    assert_eq!(target.seal_pending_batch(10).unwrap(), 10);
    assert_eq!(
        target.seal_pending_batch(usize::MAX).unwrap(),
        300 - MAX_SEAL_BATCH - 10
    );
    assert_eq!(target.seal_pending_batch(10).unwrap(), 0);
    let applied = target.apply_pending(usize::MAX).unwrap();
    assert_eq!((applied.applied, applied.waiting_for_keys), (3, 0));
    assert_eq!(live_state(&target).0, Cursor(3));
}

fn captured_envelopes(source: &Client, bodies: &[&str]) -> Vec<Envelope> {
    bodies
        .iter()
        .enumerate()
        .map(|(n, body)| {
            source
                .capture_incoming(incoming(body, &format!("cap-{body}-{n}")))
                .unwrap();
            let envelope = source.pending_outbox_batch(1).unwrap().remove(0);
            source.ack_outbox(envelope.envelope_id).unwrap();
            envelope
        })
        .collect()
}
fn raw(cursor: u64, envelope: &Envelope) -> RawSnapshotRecord {
    RawSnapshotRecord {
        cursor: Cursor(cursor),
        envelope_json: serde_json::to_vec(envelope).unwrap(),
    }
}
fn bodies(client: &Client, conversation: ConversationId) -> Vec<String> {
    client
        .messages(conversation)
        .unwrap()
        .into_iter()
        .map(|m| m.payload.body)
        .collect()
}

#[test]
fn published_snapshot_drains_in_bounded_batches_across_crashes_and_keeps_tail_order() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let source = unlocked(&config(&dir, "source", &vault), &vault);
    let names: Vec<String> = (0..700).map(|n| format!("h{n}")).collect();
    let history = captured_envelopes(
        &source,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let tail = captured_envelopes(&source, &["tail"]).remove(0);
    let conversation = only_conversation(&source);
    let target_cfg = config(&dir, "target", &vault);
    let target = unlocked(&target_cfg, &vault);

    // 1200 raw records over three pages: 500 malformed records first (they drain straight to
    // quarantine and leave the apply budget unused), then 700 real messages.
    let records: Vec<RawSnapshotRecord> = (1..=500)
        .map(|c| RawSnapshotRecord {
            cursor: Cursor(c),
            envelope_json: format!("junk-{c}").into_bytes(),
        })
        .chain(
            history
                .iter()
                .enumerate()
                .map(|(i, e)| raw(501 + i as u64, e)),
        )
        .collect();
    let session = target
        .begin_snapshot(Cursor(1200), 1200, SnapshotPurpose::Resync)
        .unwrap();
    for page in records.chunks(400) {
        target
            .append_snapshot_raw_page(session.generation, page)
            .unwrap();
    }
    // Publishing copies nothing and only moves the cursor.
    let report = target.finish_snapshot(session.generation).unwrap();
    assert_eq!(
        (report.journaled, report.duplicate, report.receive_cursor),
        (1200, 0, 1200)
    );
    assert!(
        target.messages(conversation).unwrap().is_empty(),
        "publish did no per-record work"
    );
    assert!(target.quarantined().unwrap().is_empty());
    assert_eq!(
        target.ingest(&tail, Cursor(1201)).unwrap(),
        IngestResult::Journaled
    );
    assert_eq!(target.receive_cursor().unwrap(), Cursor(1201));
    drop(target); // crash after publish, before any drain

    let target = unlocked(&target_cfg, &vault);
    let first = target.apply_pending(500).unwrap();
    assert_eq!(
        (first.drained, first.applied, first.snapshot_remaining),
        (500, 0, 700)
    );
    assert_eq!(target.quarantined().unwrap().len(), 500);
    assert!(
        target.messages(conversation).unwrap().is_empty(),
        "the live tail must not overtake undrained history even with spare apply budget"
    );
    assert!(
        matches!(
            target.begin_snapshot(Cursor(1200), 0, SnapshotPurpose::Resync),
            Err(Error::InvalidRequest(_))
        ),
        "no new generation while one is draining"
    );
    drop(target); // crash mid-drain

    let target = unlocked(&target_cfg, &vault);
    let second = target.apply_pending(500).unwrap();
    assert_eq!(
        (second.drained, second.applied, second.snapshot_remaining),
        (500, 500, 200)
    );
    assert!(!bodies(&target, conversation).contains(&"tail".to_string()));
    let last = target.apply_pending(usize::MAX).unwrap();
    assert_eq!(
        (last.drained, last.applied, last.snapshot_remaining),
        (200, 201, 0)
    );
    let all = bodies(&target, conversation);
    assert_eq!(all.len(), 701);
    assert_eq!(
        all.last().map(String::as_str),
        Some("tail"),
        "tail applied after all history"
    );
    assert_eq!(target.apply_pending(100).unwrap(), ApplyReport::default());
    assert_eq!(target.quarantined().unwrap().len(), 500);
}

fn only_conversation(client: &Client) -> ConversationId {
    let conversations = client.list_conversations().unwrap();
    assert_eq!(conversations.len(), 1);
    conversations[0].conversation_id
}

#[test]
fn live_and_staged_overlap_is_coordinated() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let source = unlocked(&config(&dir, "source", &vault), &vault);
    let e = captured_envelopes(&source, &["e0", "e1", "e2", "e3", "e4", "e5", "e6", "e7"]);
    let conversation = only_conversation(&source);
    let target = unlocked(&config(&dir, "target", &vault), &vault);
    let typed = |cursor: u64, n: usize| SnapshotRecord {
        cursor: Cursor(cursor),
        envelope: e[n].clone(),
    };

    // Equal live overlap while staging is normal.
    target.ingest(&e[0], Cursor(1)).unwrap();
    let s = target
        .begin_snapshot(Cursor(3), 3, SnapshotPurpose::Resync)
        .unwrap();
    target
        .append_snapshot_page(s.generation, &[typed(1, 0), typed(2, 1)])
        .unwrap();
    assert_eq!(
        target.ingest(&e[1], Cursor(2)).unwrap(),
        IngestResult::Journaled
    );
    target
        .append_snapshot_page(s.generation, &[typed(3, 2)])
        .unwrap();
    let report = target.finish_snapshot(s.generation).unwrap();
    assert_eq!((report.journaled, report.duplicate), (2, 1));
    let applied = target.apply_pending(100).unwrap();
    assert_eq!((applied.drained, applied.applied), (3, 3));
    assert_eq!(bodies(&target, conversation), ["e0", "e1", "e2"]);

    // Published-but-undrained records: equal live replay is a duplicate, different is a conflict.
    let s = target
        .begin_snapshot(Cursor(5), 2, SnapshotPurpose::Resync)
        .unwrap();
    target
        .append_snapshot_page(s.generation, &[typed(4, 3), typed(5, 4)])
        .unwrap();
    target.finish_snapshot(s.generation).unwrap();
    assert_eq!(
        target.ingest(&e[3], Cursor(4)).unwrap(),
        IngestResult::Duplicate
    );
    assert_eq!(target.ingest(&e[0], Cursor(5)), Err(Error::Conflict));
    assert_eq!(target.apply_pending(100).unwrap().applied, 2);
    assert_eq!(
        bodies(&target, conversation),
        ["e0", "e1", "e2", "e3", "e4"]
    );

    // A different live record over unpublished staging wins; that generation can't publish.
    let s = target
        .begin_snapshot(Cursor(7), 2, SnapshotPurpose::Resync)
        .unwrap();
    target
        .append_snapshot_page(s.generation, &[typed(6, 5), typed(7, 6)])
        .unwrap();
    assert_eq!(
        target.ingest(&e[7], Cursor(6)).unwrap(),
        IngestResult::Journaled
    );
    assert_eq!(
        target.finish_snapshot(s.generation).err(),
        Some(Error::SnapshotMismatch)
    );
    assert_eq!(target.snapshot_progress().unwrap(), None);
    assert_eq!(target.receive_cursor().unwrap(), Cursor(6));
    assert_eq!(target.apply_pending(100).unwrap().applied, 1);
    assert_eq!(
        bodies(&target, conversation).last().map(String::as_str),
        Some("e7")
    );
}

#[test]
fn raw_malformed_and_oversize_records_match_live_ingest() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let live = open(&config(&dir, "live", &vault));
    let staged = open(&config(&dir, "staged", &vault));
    let garbage = b"{not json".to_vec();
    let oversize = vec![b'a'; 2 * 1024 * 1024 + 1];
    let mut other_oversize = oversize.clone();
    other_oversize[9] = b'b';
    live.ingest_raw(&garbage, Cursor(1)).unwrap();
    live.ingest_raw(&oversize, Cursor(2)).unwrap();

    let s = staged
        .begin_snapshot(Cursor(2), 2, SnapshotPurpose::Resync)
        .unwrap();
    let too_big: Vec<_> = (1..=5)
        .map(|c| RawSnapshotRecord {
            cursor: Cursor(c),
            envelope_json: oversize.clone(),
        })
        .collect();
    assert!(matches!(
        staged.append_snapshot_raw_page(s.generation, &too_big),
        Err(Error::InvalidRequest(_))
    ));
    assert!(
        staged.snapshot_progress().unwrap().is_some(),
        "a rejected page size does not fail the generation"
    );
    staged
        .append_snapshot_raw_page(
            s.generation,
            &[
                RawSnapshotRecord {
                    cursor: Cursor(1),
                    envelope_json: garbage.clone(),
                },
                RawSnapshotRecord {
                    cursor: Cursor(2),
                    envelope_json: oversize.clone(),
                },
            ],
        )
        .unwrap();
    staged.finish_snapshot(s.generation).unwrap();
    assert_eq!(staged.apply_pending(10).unwrap().drained, 2);

    let reasons = |c: &Client| {
        c.quarantined()
            .unwrap()
            .into_iter()
            .map(|q| (q.cursor, q.reason))
            .collect::<Vec<_>>()
    };
    assert_eq!(reasons(&live), reasons(&staged));
    assert_eq!(
        reasons(&live),
        vec![
            (Cursor(1), QuarantineReason::MalformedEnvelope),
            (Cursor(2), QuarantineReason::MalformedEnvelope)
        ]
    );
    for client in [&live, &staged] {
        assert_eq!(
            client.ingest_raw(&garbage, Cursor(1)).unwrap(),
            IngestResult::Duplicate
        );
        assert_eq!(
            client.ingest_raw(&oversize, Cursor(2)).unwrap(),
            IngestResult::Duplicate
        );
        assert_eq!(
            client.ingest_raw(&other_oversize, Cursor(2)),
            Err(Error::Conflict)
        );
        assert_eq!(client.receive_cursor().unwrap(), Cursor(2));
    }
}

#[test]
fn stale_generation_calls_never_disturb_a_newer_generation() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let client = open(&config(&dir, "device", &vault));
    let junk = |cursor: u64| RawSnapshotRecord {
        cursor: Cursor(cursor),
        envelope_json: format!("junk-{cursor}").into_bytes(),
    };
    let old = client
        .begin_snapshot(Cursor(2), 2, SnapshotPurpose::Resync)
        .unwrap();
    client
        .append_snapshot_raw_page(old.generation, &[junk(1)])
        .unwrap();
    let new = client
        .begin_snapshot(Cursor(2), 2, SnapshotPurpose::Resync)
        .unwrap();
    assert!(new.generation > old.generation);
    assert_eq!(
        client
            .append_snapshot_raw_page(old.generation, &[junk(2)])
            .err(),
        Some(Error::SnapshotMismatch)
    );
    assert_eq!(
        client
            .append_snapshot_raw_page(old.generation, &[junk(1), junk(1)])
            .err(),
        Some(Error::SnapshotMismatch)
    );
    assert_eq!(
        client.finish_snapshot(old.generation).err(),
        Some(Error::SnapshotMismatch)
    );
    let progress = client.snapshot_progress().unwrap().unwrap();
    assert_eq!(
        (progress.generation, progress.received_records),
        (new.generation, 0)
    );
    client
        .append_snapshot_raw_page(new.generation, &[junk(1), junk(2)])
        .unwrap();
    assert_eq!(
        client
            .finish_snapshot(new.generation)
            .unwrap()
            .receive_cursor,
        2
    );
    assert_eq!(client.apply_pending(10).unwrap().drained, 2);
    assert_eq!(client.quarantined().unwrap().len(), 2);
}

#[test]
fn empty_and_rejected_restores_still_set_the_guard() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let empty = open(&config(&dir, "empty", &vault));
    let s = empty
        .begin_snapshot(Cursor(0), 0, SnapshotPurpose::Restore)
        .unwrap();
    assert!(
        empty.restore_guarded().unwrap(),
        "guard is set at begin, even for an empty vault"
    );
    let report = empty.finish_snapshot(s.generation).unwrap();
    assert_eq!((report.journaled, report.receive_cursor), (0, 0));
    assert_eq!(empty.snapshot_progress().unwrap(), None);
    assert_eq!(empty.apply_pending(10).unwrap(), ApplyReport::default());

    let rejected = open(&config(&dir, "rejected", &vault));
    assert!(matches!(
        rejected.begin_snapshot(Cursor(1), 2, SnapshotPurpose::Restore),
        Err(Error::InvalidRequest(_))
    ));
    assert!(
        rejected.restore_guarded().unwrap(),
        "guard survives rejected restore arguments"
    );
    let resync = open(&config(&dir, "resync", &vault));
    resync
        .begin_snapshot(Cursor(0), 0, SnapshotPurpose::Resync)
        .unwrap();
    assert!(!resync.restore_guarded().unwrap());
}

#[test]
fn rolled_back_own_outbox_is_quarantined_and_reconcilable() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let gateway_cfg = config(&dir, "gateway", &vault);
    let gateway = open(&gateway_cfg);
    let captured = gateway
        .capture_incoming(incoming("before backup", "rb-1"))
        .unwrap();
    drop(gateway); // the capture is durable but unsealed (locked) in the backup
    let backup = |name: &str| {
        let path = dir.path().join(format!("{name}.db"));
        fs::copy(&gateway_cfg.database_path, &path).unwrap();
        ClientConfig {
            database_path: path,
            ..gateway_cfg.clone()
        }
    };
    let (unsealed_cfg, resealed_cfg) = (backup("restored-unsealed"), backup("restored-resealed"));
    let gateway = unlocked(&gateway_cfg, &vault);
    let mut server = Server::default();
    assert_eq!(server.upload(&gateway), 1);
    let accepted = server.log[0].clone();

    let check = |restored: &Client| {
        let s = restored
            .begin_snapshot(Cursor(1), 1, SnapshotPurpose::Restore)
            .unwrap();
        restored
            .append_snapshot_page(
                s.generation,
                &[SnapshotRecord {
                    cursor: Cursor(1),
                    envelope: accepted.clone(),
                }],
            )
            .unwrap();
        restored.finish_snapshot(s.generation).unwrap();
        let report = restored.apply_pending(10).unwrap();
        assert_eq!((report.drained, report.quarantined), (1, 0));
        assert_eq!(
            restored.quarantined().unwrap()[0].reason,
            QuarantineReason::EnvelopeConflict
        );
        let conflicts = restored.outbox_conflicts().unwrap();
        assert_eq!(
            conflicts,
            vec![OutboxConflict {
                envelope_id: accepted.envelope_id,
                command_id: None,
                cursor: Cursor(1)
            }]
        );
        assert!(
            restored.pending_outbox().unwrap().is_empty(),
            "never re-uploaded"
        );
        assert_eq!(
            restored.messages(captured.conversation_id).unwrap().len(),
            1,
            "local capture preserved"
        );
        assert!(restored.restore_guarded().unwrap());
    };

    // Rolled back before sealing: the conflicted row is never sealed later.
    let unsealed = open(&unsealed_cfg);
    check(&unsealed);
    unsealed
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();
    assert!(unsealed.pending_outbox().unwrap().is_empty());
    assert_eq!(unsealed.seal_pending_batch(10).unwrap(), 0);

    // Rolled back, then resealed with a fresh nonce before resync.
    let resealed = unlocked(&resealed_cfg, &vault);
    let resealed_wire = resealed.pending_outbox().unwrap();
    assert_eq!(resealed_wire.len(), 1);
    assert_eq!(resealed_wire[0].envelope_id, accepted.envelope_id);
    assert_ne!(resealed_wire[0], accepted);
    check(&resealed);
}

#[test]
fn snapshot_drain_respects_the_byte_budget_with_near_limit_records() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let client = open(&config(&dir, "device", &vault)); // no keys needed: drain classifies only
    // Six stored wires just under the 2 MiB raw limit; at most four fit an 8 MiB page.
    let near_limit = 2 * 1024 * 1024 - 64;
    let record = |cursor: u64| RawSnapshotRecord {
        cursor: Cursor(cursor),
        envelope_json: vec![b'x'; near_limit],
    };
    let session = client
        .begin_snapshot(Cursor(6), 6, SnapshotPurpose::Resync)
        .unwrap();
    client
        .append_snapshot_raw_page(session.generation, &(1..=4).map(record).collect::<Vec<_>>())
        .unwrap();
    client
        .append_snapshot_raw_page(session.generation, &(5..=6).map(record).collect::<Vec<_>>())
        .unwrap();
    client.finish_snapshot(session.generation).unwrap();

    let first = client.apply_pending(1_000).unwrap();
    assert_eq!(
        (first.drained, first.snapshot_remaining),
        (4, 2),
        "byte budget, not limit 1000"
    );
    let second = client.apply_pending(1_000).unwrap();
    assert_eq!((second.drained, second.snapshot_remaining), (2, 0));
    let cursors: Vec<_> = client
        .quarantined()
        .unwrap()
        .into_iter()
        .map(|q| q.cursor.0)
        .collect();
    assert_eq!(
        cursors,
        (1..=6).collect::<Vec<_>>(),
        "drained in cursor order"
    );
}
