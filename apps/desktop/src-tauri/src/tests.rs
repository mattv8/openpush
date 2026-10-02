//! Host integration tests over real client-core databases (SQLCipher) with an in-memory secure
//! store. `real_server_*` tests additionally run the actual server router over TCP against the
//! local PostgreSQL (and SeaweedFS for media); they are ignored unless explicitly requested.
use crate::{
    credentials::{
        ensure_database_key, parse_credential, store_credential, tests::credential_json,
        tests::TOKEN, Binding,
    },
    dto::GatewayView,
    media::tests::sample_png,
    secure_store::{MemoryStore, SecretStore},
    session::{open_session, DraftInput, Notifier, Session},
};
use openpush_client_core::{
    ComposeDraftUpdate, Direction, IncomingSms, KeyProfile, MmsAcquisitionInput, MmsSource,
    Transport, VaultCheckHeader,
};
use openpush_crypto::{create_vault_check_header, derive_root_key};
use std::{path::Path, sync::Arc};

pub const PHRASE: &str = "correct horse battery staple";

pub(crate) fn noop() -> Notifier {
    Arc::new(|| {})
}

pub(crate) struct Fixture {
    pub dir: tempfile::TempDir,
    pub store: MemoryStore,
    pub binding: Binding,
    pub profile: KeyProfile,
    pub header: VaultCheckHeader,
}

fn fixture() -> Fixture {
    // Port 9 (discard) on loopback: nothing listens; these tests never touch the network.
    fixture_at("http://127.0.0.1:9")
}

pub(crate) fn fixture_at(origin: &str) -> Fixture {
    let vault = uuid::Uuid::new_v4();
    let device = uuid::Uuid::new_v4();
    let profile = KeyProfile::new(vault, 1).unwrap();
    let root = derive_root_key(PHRASE, &profile).unwrap();
    let header = create_vault_check_header(&root, profile.clone()).unwrap();
    let store = MemoryStore::default();
    let credential = parse_credential(&credential_json(
        origin,
        &vault.to_string(),
        &device.to_string(),
        TOKEN,
    ))
    .unwrap();
    store_credential(&store, &credential).unwrap();
    Fixture {
        dir: tempfile::tempdir().unwrap(),
        store,
        binding: credential.binding(),
        profile,
        header,
    }
}

pub(crate) fn open(f: &Fixture, binding: &Binding, epochs: &[u32]) -> Session {
    open_session(f.dir.path(), &f.store, binding, epochs, noop()).unwrap()
}

pub(crate) fn input(
    id: &str,
    conversation: &str,
    text: &str,
    recipients: &[&str],
    revision: &str,
) -> DraftInput {
    serde_json::from_value(serde_json::json!({
        "id": id, "conversationId": conversation, "text": text, "recipientIds": recipients,
        "attachmentIds": [], "expectedRevision": revision, "revision": "ignored-extra-field"
    }))
    .unwrap()
}

pub(crate) fn routed(mut draft: DraftInput, gateway: &str, sim: &str) -> DraftInput {
    draft.gateway_id = Some(gateway.into());
    draft.sim_id = Some(sim.into());
    draft
}

pub(crate) fn gateway(id: &str, sms: bool, mms: bool) -> GatewayView {
    GatewayView {
        id: id.into(),
        name: "Test SIM".into(),
        sim_id: "sim-1".into(),
        online: false,
        simulated: true,
        supports_sms: sms,
        supports_mms: mms,
        capability_note: None,
        mms_content_version: mms.then_some(2),
        mms_max_bytes: mms.then_some(300 * 1024),
        mms_limit_source: mms.then_some("fallback".into()),
        mms_max_recipients: mms.then_some(20),
    }
}

fn assert_sanitized(json: &str, dir: &Path) {
    for forbidden in [
        TOKEN,
        "file_key",
        "fileKey",
        "deviceToken",
        "db-key",
        "sqlcipher",
        ".opss",
    ] {
        assert!(!json.contains(forbidden), "snapshot leaked {forbidden}");
    }
    assert!(
        !json.contains(&dir.to_string_lossy().to_string()),
        "snapshot leaked a local path"
    );
}

fn unlock(session: &Session, fixture: &Fixture) {
    session
        .client
        .unlock(&fixture.profile, &fixture.header, PHRASE)
        .unwrap();
}

fn completed_mms(
    session: &Session,
    source_id: &str,
    thread_id: &str,
    direction: Direction,
    sender: Option<&str>,
    recipients: &[&str],
    attachments: Vec<openpush_client_core::AttachmentId>,
) -> openpush_client_core::ConversationId {
    let acquisition = session
        .client
        .begin_mms_acquisition(MmsAcquisitionInput {
            source: MmsSource {
                source_generation: "desktop-native-test".into(),
                subscription_id: "sim-1".into(),
                provider_message_id: source_id.into(),
                provider_thread_id: Some(thread_id.into()),
            },
            direction,
            sender_address: sender.map(str::to_owned),
            recipients: recipients.iter().map(|value| (*value).into()).collect(),
            subject: None,
            body: "existing MMS".into(),
            imported: false,
            observed_at_ms: 1,
            transaction_id: None,
        })
        .unwrap();
    for (index, attachment) in attachments.into_iter().enumerate() {
        session
            .client
            .set_mms_acquisition_part(
                &acquisition.acquisition_id,
                &format!("part-{index}"),
                attachment,
            )
            .unwrap();
    }
    session
        .client
        .complete_mms_acquisition(&acquisition.acquisition_id)
        .unwrap()
        .conversation_id
}

fn set_gateway(session: &Session, mut route: GatewayView) {
    route.sim_id = "sim-1".into();
    session.set_status(|status| {
        status.gateways = vec![route];
        status.gateways_known = true;
    });
}

fn stored_revision(session: &Session, id: &str) -> String {
    session
        .client
        .compose_draft(id.parse().unwrap())
        .unwrap()
        .unwrap()
        .revision
        .to_string()
}

#[test]
fn existing_mms_single_text_reply_is_v1_denied_unchanged_then_v2_accepted_as_mms() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    unlock(&session, &f);
    let conversation = completed_mms(
        &session,
        "existing-single",
        "thread-single",
        Direction::Outgoing,
        None,
        &["+15555550101"],
        vec![],
    );
    let draft = session
        .save_draft(&input(
            "draft-new",
            &conversation.to_string(),
            "reply",
            &["+15555550101"],
            "0",
        ))
        .unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();
    let mut v1 = gateway(&gateway_id, true, true);
    v1.mms_content_version = Some(1);
    set_gateway(&session, v1);
    let send = routed(
        input(&draft.id, &draft.conversation_id, "", &[], &draft.revision),
        &gateway_id,
        "sim-1",
    );
    assert_eq!(
        session.send_draft(&send).unwrap_err().code,
        "mms-unsupported"
    );
    assert_eq!(stored_revision(&session, &draft.id), draft.revision);

    set_gateway(&session, gateway(&gateway_id, true, true));
    assert!(session.send_draft(&send).unwrap().accepted);
    assert_eq!(
        session
            .client
            .messages(conversation)
            .unwrap()
            .last()
            .unwrap()
            .payload
            .transport,
        Transport::Mms
    );
}

#[test]
fn latest_sms_after_historical_mms_uses_legacy_sms_route() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    unlock(&session, &f);
    let conversation = completed_mms(
        &session,
        "historical-mms",
        "thread-latest-sms",
        Direction::Outgoing,
        None,
        &["+15555550101"],
        vec![],
    );
    session
        .client
        .capture_incoming(IncomingSms {
            conversation_id: Some(conversation),
            sender_address: "+15555550101".into(),
            body: "newer SMS".into(),
            provider_message_id: Some("latest-sms".into()),
            imported: false,
        })
        .unwrap();
    let draft = session
        .save_draft(&input(
            "draft-new",
            &conversation.to_string(),
            "SMS reply",
            &["+15555550101"],
            "0",
        ))
        .unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();
    set_gateway(&session, gateway(&gateway_id, true, false));
    assert!(
        session
            .send_draft(&routed(
                input(&draft.id, &draft.conversation_id, "", &[], &draft.revision),
                &gateway_id,
                "sim-1",
            ))
            .unwrap()
            .accepted
    );
    assert_eq!(
        session
            .client
            .messages(conversation)
            .unwrap()
            .last()
            .unwrap()
            .payload
            .transport,
        Transport::Sms
    );
}

#[test]
fn inferred_group_recipients_are_v1_denied_before_draft_cas() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    unlock(&session, &f);
    let conversation = completed_mms(
        &session,
        "existing-group",
        "thread-group",
        Direction::Outgoing,
        None,
        &["+15555550101", "+15555550102"],
        vec![],
    );
    let draft_id = session
        .client
        .create_compose_draft(Some(conversation))
        .unwrap()
        .draft_id;
    let draft = session
        .client
        .save_compose_draft(
            draft_id,
            0,
            ComposeDraftUpdate {
                text: "group reply".into(),
                recipients: vec![],
                attachment_ids: vec![],
                route: None,
            },
        )
        .unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();
    let mut v1 = gateway(&gateway_id, true, true);
    v1.mms_content_version = Some(1);
    set_gateway(&session, v1);
    let error = session
        .send_draft(&routed(
            input(
                &draft_id.to_string(),
                &conversation.to_string(),
                "",
                &[],
                &draft.revision.to_string(),
            ),
            &gateway_id,
            "sim-1",
        ))
        .unwrap_err();
    assert_eq!(error.code, "mms-unsupported");
    assert_eq!(
        stored_revision(&session, &draft_id.to_string()),
        draft.revision.to_string()
    );
}

#[test]
fn mms_route_recipient_and_byte_limits_refuse_without_revision_changes() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    unlock(&session, &f);
    let gateway_id = uuid::Uuid::new_v4().to_string();
    let mut limited = gateway(&gateway_id, true, true);
    limited.mms_max_recipients = Some(2);
    limited.mms_max_bytes = Some(2);
    set_gateway(&session, limited);

    let recipients = session
        .save_draft(&input(
            "draft-new",
            "",
            "x",
            &["+15555550101", "+15555550102", "+15555550103"],
            "0",
        ))
        .unwrap();
    let recipients_error = session
        .send_draft(&routed(
            input(&recipients.id, "", "", &[], &recipients.revision),
            &gateway_id,
            "sim-1",
        ))
        .unwrap_err();
    assert_eq!(recipients_error.code, "mms-too-many-recipients");
    assert_eq!(
        stored_revision(&session, &recipients.id),
        recipients.revision
    );

    let oversized = session
        .save_draft(&input(
            "draft-new",
            "",
            "abc",
            &["+15555550101", "+15555550102"],
            "0",
        ))
        .unwrap();
    let size_error = session
        .send_draft(&routed(
            input(&oversized.id, "", "", &[], &oversized.revision),
            &gateway_id,
            "sim-1",
        ))
        .unwrap_err();
    assert_eq!(size_error.code, "mms-too-large");
    assert_eq!(stored_revision(&session, &oversized.id), oversized.revision);
}

#[test]
fn legacy_single_recipient_sms_still_sends_on_sms_only_route() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    unlock(&session, &f);
    let gateway_id = uuid::Uuid::new_v4().to_string();
    set_gateway(&session, gateway(&gateway_id, true, false));
    let draft = session
        .save_draft(&input(
            "draft-new",
            "",
            "legacy SMS",
            &["+15555550101"],
            "0",
        ))
        .unwrap();
    assert!(
        session
            .send_draft(&routed(
                input(&draft.id, "", "", &[], &draft.revision),
                &gateway_id,
                "sim-1",
            ))
            .unwrap()
            .accepted
    );
}

#[test]
fn incoming_group_blocks_unknown_self_then_confirmation_excludes_own_address() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    unlock(&session, &f);
    let conversation = completed_mms(
        &session,
        "incoming-group",
        "thread-incoming-group",
        Direction::Incoming,
        Some("+15555550101"),
        &["+15555550100", "+15555550102"],
        vec![],
    );
    let before = session.client.mms_reply_context(conversation).unwrap();
    assert!(before.recipients.is_empty());
    assert!(before.blocked_reason.is_some());
    let draft_id = session
        .client
        .create_compose_draft(Some(conversation))
        .unwrap()
        .draft_id;
    let draft = session
        .client
        .save_compose_draft(
            draft_id,
            0,
            ComposeDraftUpdate {
                text: "reply".into(),
                recipients: vec![],
                attachment_ids: vec![],
                route: None,
            },
        )
        .unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();
    set_gateway(&session, gateway(&gateway_id, true, true));
    let send = routed(
        input(
            &draft_id.to_string(),
            &conversation.to_string(),
            "",
            &[],
            &draft.revision.to_string(),
        ),
        &gateway_id,
        "sim-1",
    );
    assert_eq!(
        session.send_draft(&send).unwrap_err().code,
        "mms-reply-blocked"
    );
    assert_eq!(
        stored_revision(&session, &draft_id.to_string()),
        draft.revision.to_string()
    );

    session
        .client
        .set_mms_own_address("sim-1", "+15555550100")
        .unwrap();
    let after = session.client.mms_reply_context(conversation).unwrap();
    assert_eq!(after.recipients, vec!["+15555550101", "+15555550102"]);
    assert!(after.blocked_reason.is_none());
    assert!(session.send_draft(&send).unwrap().accepted);
    assert_eq!(
        session
            .client
            .messages(conversation)
            .unwrap()
            .last()
            .unwrap()
            .payload
            .recipients,
        after.recipients
    );
}

#[test]
fn retry_clears_only_recorded_failure_and_preserves_outbox_and_attempt() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    unlock(&session, &f);
    let path = f.dir.path().join("retry.png");
    std::fs::write(&path, sample_png(8, 8)).unwrap();
    let attachment = session.prepare_attachment(&path).unwrap();
    let attachment_id = attachment.id.parse().unwrap();
    session
        .transfer_errors
        .lock()
        .unwrap()
        .insert(attachment_id, "failed".into());

    let gateway_id = f.binding.device_id.clone();
    set_gateway(&session, gateway(&gateway_id, true, false));
    let draft = session
        .save_draft(&input("draft-new", "", "attempt", &["+15555550101"], "0"))
        .unwrap();
    session
        .send_draft(&routed(
            input(&draft.id, "", "", &[], &draft.revision),
            &gateway_id,
            "sim-1",
        ))
        .unwrap();
    let outbox_before = session.client.pending_outbox_batch(100).unwrap();
    let commands_before = session.client.pending_commands().unwrap();
    let messages_before = session
        .client
        .messages(draft.conversation_id.parse().unwrap())
        .unwrap();

    session.retry_attachment(&attachment.id).unwrap();
    assert!(!session
        .transfer_errors
        .lock()
        .unwrap()
        .contains_key(&attachment_id));
    assert_eq!(
        session.client.pending_outbox_batch(100).unwrap(),
        outbox_before
    );
    assert_eq!(
        session
            .client
            .messages(draft.conversation_id.parse().unwrap())
            .unwrap(),
        messages_before
    );
    assert_eq!(session.client.pending_commands().unwrap(), commands_before);
    assert!(session
        .retry_attachment(&uuid::Uuid::new_v4().to_string())
        .is_err());
    assert_eq!(
        session.retry_attachment(&attachment.id).unwrap_err().code,
        "attachment-not-retryable"
    );
}

#[test]
fn attachment_scope_accepts_own_conversation_and_rejects_foreign_conversation() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    unlock(&session, &f);
    let first_path = f.dir.path().join("first.png");
    let second_path = f.dir.path().join("second.png");
    std::fs::write(&first_path, sample_png(8, 8)).unwrap();
    std::fs::write(&second_path, sample_png(9, 9)).unwrap();
    let first = session.prepare_attachment(&first_path).unwrap();
    let second = session.prepare_attachment(&second_path).unwrap();
    let first_conversation = completed_mms(
        &session,
        "scope-first",
        "scope-thread-first",
        Direction::Incoming,
        Some("+15555550101"),
        &[],
        vec![first.id.parse().unwrap()],
    );
    let second_conversation = completed_mms(
        &session,
        "scope-second",
        "scope-thread-second",
        Direction::Incoming,
        Some("+15555550102"),
        &[],
        vec![second.id.parse().unwrap()],
    );
    assert!(session
        .attachment_in_conversation(&first.id, first_conversation)
        .unwrap());
    assert!(!session
        .attachment_in_conversation(&first.id, second_conversation)
        .unwrap());
}

#[test]
fn legacy_gateway_cache_without_mms_version_is_normalized_to_mms_denied() {
    let f = fixture();
    let data_dir = f.binding.data_dir(f.dir.path());
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        data_dir.join(crate::session::GATEWAYS_FILE),
        serde_json::to_vec(&serde_json::json!([{
            "id": uuid::Uuid::new_v4().to_string(),
            "name": "Legacy gateway",
            "simId": "sim-1",
            "online": false,
            "simulated": false,
            "supportsSms": true,
            "supportsMms": true
        }]))
        .unwrap(),
    )
    .unwrap();
    let session = open(&f, &f.binding, &[]);
    let (gateways, known) = session.gateways();
    assert!(known);
    assert!(gateways[0].supports_sms);
    assert!(!gateways[0].supports_mms);
    assert_eq!(gateways[0].mms_content_version, None);
}

#[test]
fn drafts_send_atomically_through_checked_routes_and_map_to_sanitized_dtos() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    session
        .client
        .unlock(&f.profile, &f.header, PHRASE)
        .unwrap();
    let gateway_id = uuid::Uuid::new_v4().to_string();

    // New-conversation draft from the UI placeholder; recipients normalized; extra fields ignored.
    let draft = session
        .save_draft(&input(
            "draft-new",
            "",
            "hello",
            &["+1 (555) 555-0100"],
            "0",
        ))
        .unwrap();
    assert_eq!(draft.revision, "1");
    assert_eq!(draft.recipient_ids, vec!["+15555550100".to_string()]);
    assert!(session
        .save_draft(&input(
            &draft.id,
            &draft.conversation_id,
            "x",
            &["not a phone"],
            "1"
        ))
        .is_err());
    assert_eq!(
        session
            .save_draft(&input(&draft.id, &draft.conversation_id, "stale", &[], "0"))
            .unwrap_err()
            .code,
        "stale-draft"
    );

    // Gateway capabilities not loaded yet: refused, draft untouched.
    let send = routed(
        input(&draft.id, &draft.conversation_id, "", &[], "1"),
        &gateway_id,
        "sim-1",
    );
    assert_eq!(
        session.send_draft(&send).unwrap_err().code,
        "gateways-unknown"
    );
    session.set_status(|s| {
        s.gateways = vec![gateway(&gateway_id, true, false)];
        s.gateways_known = true;
    });
    // Unreported route and stale revision are refused without queueing anything.
    assert_eq!(
        session
            .send_draft(&routed(
                input(&draft.id, "", "", &[], "1"),
                &gateway_id,
                "sim-9"
            ))
            .unwrap_err()
            .code,
        "gateway-unavailable"
    );
    assert_eq!(
        session
            .send_draft(&routed(
                input(&draft.id, "", "", &[], "0"),
                &gateway_id,
                "sim-1"
            ))
            .unwrap_err()
            .code,
        "stale-draft"
    );
    assert!(session.client.pending_outbox_batch(10).unwrap().is_empty());

    // Accepted == durably queued locally; the route was persisted with CAS before the send.
    let result = session.send_draft(&send).unwrap();
    assert!(result.accepted);
    assert_eq!(result.status, "queued-local");
    assert_eq!(session.client.pending_outbox_batch(10).unwrap().len(), 1);
    // Double submit of the same revision cannot queue a second command.
    assert_eq!(session.send_draft(&send).unwrap_err().code, "stale-draft");
    assert_eq!(session.client.pending_outbox_batch(10).unwrap().len(), 1);

    let (snapshot, _) = session
        .snapshot(
            Some(&draft.conversation_id),
            crate::head(),
            Some("http://127.0.0.1:9".into()),
            crate::notifications::NotificationPreferences::default(),
        )
        .unwrap();
    let conversation = snapshot
        .conversations
        .iter()
        .find(|c| c.id == draft.conversation_id)
        .unwrap();
    assert_eq!(conversation.name, "+15555550100");
    let message = conversation.messages.last().unwrap();
    assert_eq!(
        (message.body.as_str(), message.sender, message.status),
        ("hello", "self", Some("queued-local"))
    );
    assert!(
        message.timestamp.is_empty(),
        "no fabricated wall-clock time"
    );
    let stored = snapshot.draft.as_ref().unwrap();
    assert_eq!(stored.text, "");
    assert_eq!(stored.gateway_id.as_deref(), Some(gateway_id.as_str()));
    assert_eq!(stored.sim_id.as_deref(), Some("sim-1"));
    assert_eq!(snapshot.encryption.state, "unlocked");
    assert_eq!(snapshot.pending_count, 1);
    assert_sanitized(&serde_json::to_string(&snapshot).unwrap(), f.dir.path());

    // MMS: attachments need an MMS-capable route; previews are native re-encoded data URLs.
    let png = f.dir.path().join("photo.png");
    std::fs::write(&png, sample_png(64, 48)).unwrap();
    let attachment = session.prepare_attachment(&png).unwrap();
    assert_eq!(attachment.media_type, "image/png");
    assert!(attachment
        .preview_url
        .as_deref()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    let mut with_media = input(
        &stored.id,
        &stored.conversation_id,
        "",
        &[],
        &stored.revision,
    );
    with_media.attachment_ids = vec![attachment.id.clone()];
    let saved = session.save_draft(&with_media).unwrap();
    assert_eq!(
        saved.gateway_id.as_deref(),
        Some(gateway_id.as_str()),
        "route survives saves that omit it"
    );
    let send_mms = routed(
        input(&saved.id, "", "", &[], &saved.revision),
        &gateway_id,
        "sim-1",
    );
    assert_eq!(
        session.send_draft(&send_mms).unwrap_err().code,
        "mms-unsupported"
    );
    session.set_status(|s| s.gateways = vec![gateway(&gateway_id, true, true)]);
    assert!(session.send_draft(&send_mms).unwrap().accepted);
    assert_eq!(
        session.client.pending_uploads().unwrap().len(),
        1,
        "media upload is pending before the MMS can seal"
    );
    let (snapshot, _) = session
        .snapshot(
            Some(&draft.conversation_id),
            crate::head(),
            None,
            crate::notifications::NotificationPreferences::default(),
        )
        .unwrap();
    let message = snapshot
        .conversations
        .iter()
        .find(|c| c.id == draft.conversation_id)
        .unwrap()
        .messages
        .last()
        .unwrap()
        .attachments
        .clone();
    assert_eq!(message.len(), 1);
    assert_eq!(message[0].state, "uploading");
    assert!(message[0]
        .preview_url
        .as_deref()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    assert_sanitized(&serde_json::to_string(&snapshot).unwrap(), f.dir.path());

    assert_eq!(
        session.mark_seen(&["nope".into()]).unwrap_err().code,
        "invalid-message"
    );
}

#[test]
fn clearing_saved_draft_recipients_persists_and_new_draft_cannot_send() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    unlock(&session, &f);
    let draft = session
        .save_draft(&input("draft-new", "", "attempt", &["+15555550100"], "0"))
        .unwrap();

    let cleared = session
        .save_draft(&input(
            &draft.id,
            &draft.conversation_id,
            "attempt",
            &[],
            &draft.revision,
        ))
        .unwrap();
    assert!(cleared.recipient_ids.is_empty());
    let (snapshot, _) = session
        .snapshot(
            Some(&draft.conversation_id),
            crate::head(),
            None,
            crate::notifications::NotificationPreferences::default(),
        )
        .unwrap();
    assert!(snapshot.draft.unwrap().recipient_ids.is_empty());

    let gateway_id = uuid::Uuid::new_v4().to_string();
    set_gateway(&session, gateway(&gateway_id, true, false));
    assert_eq!(
        session
            .send_draft(&routed(
                input(
                    &cleared.id,
                    &cleared.conversation_id,
                    "",
                    &[],
                    &cleared.revision,
                ),
                &gateway_id,
                "sim-1",
            ))
            .unwrap_err()
            .code,
        "invalid-recipient"
    );
    assert!(session.client.pending_outbox_batch(10).unwrap().is_empty());
}

#[test]
fn saved_empty_recipients_reply_uses_established_conversation_at_send_time() {
    let f = fixture();
    let session = open(&f, &f.binding, &[]);
    unlock(&session, &f);
    let received = session
        .client
        .capture_incoming(IncomingSms {
            conversation_id: None,
            sender_address: "+15555550101".into(),
            body: "existing message".into(),
            provider_message_id: Some("established-reply".into()),
            imported: false,
        })
        .unwrap();
    assert!(!received.duplicate);

    let draft = session
        .save_draft(&input(
            "draft-new",
            &received.conversation_id.to_string(),
            "reply",
            &[],
            "0",
        ))
        .unwrap();
    assert!(draft.recipient_ids.is_empty());

    let gateway_id = uuid::Uuid::new_v4().to_string();
    set_gateway(&session, gateway(&gateway_id, true, false));
    let outbox_before = session.client.pending_outbox_batch(10).unwrap().len();
    assert!(
        session
            .send_draft(&routed(
                input(&draft.id, &draft.conversation_id, "", &[], &draft.revision),
                &gateway_id,
                "sim-1",
            ))
            .unwrap()
            .accepted
    );
    assert_eq!(
        session.client.pending_outbox_batch(10).unwrap().len(),
        outbox_before + 1
    );
    assert_eq!(
        session
            .client
            .messages(received.conversation_id)
            .unwrap()
            .last()
            .unwrap()
            .payload
            .recipients,
        vec!["+15555550101"]
    );
}

#[test]
fn reopen_preserves_key_and_cached_unlock_and_bindings_never_share_clients() {
    let f = fixture();
    let first_key = {
        let session = open(&f, &f.binding, &[]);
        session
            .client
            .unlock(&f.profile, &f.header, PHRASE)
            .unwrap();
        session
            .save_draft(&input("draft-new", "", "kept", &["+15555550100"], "0"))
            .unwrap();
        session.persist_gateways(&[gateway(&uuid::Uuid::new_v4().to_string(), true, false)]);
        let cache = session.client.export_native_key_cache(1).unwrap();
        f.store
            .set(
                &f.binding.key_cache_account(1),
                cache.native_storage_bytes(),
            )
            .unwrap();
        f.store
            .get(&f.binding.db_key_account())
            .unwrap()
            .unwrap()
            .to_vec()
    };
    // Reimport/reopen: the existing key is preserved and the database opens with its data;
    // the native key cache restores the unlocked state without a passphrase.
    assert_eq!(
        ensure_database_key(&f.store, &f.binding, &f.binding.database_path(f.dir.path()))
            .unwrap()
            .as_slice(),
        first_key.as_slice()
    );
    let reopened = open(&f, &f.binding, &[1]);
    assert_eq!(reopened.client.compose_drafts().unwrap()[0].text, "kept");
    let keys = reopened.client.key_status().unwrap();
    assert!(keys.active_epoch == Some(1) && keys.unlocked_epochs.contains(&1));
    let (gateways, known) = reopened.gateways();
    assert!(
        known && gateways.len() == 1,
        "last reported routes survive restart for offline sends"
    );
    drop(reopened);

    // Another device of the same vault: separate key, database and client; no shared state.
    let other_device = uuid::Uuid::new_v4().to_string();
    let other = parse_credential(&credential_json(
        "http://127.0.0.1:9",
        &f.binding.vault_id,
        &other_device,
        TOKEN,
    ))
    .unwrap();
    store_credential(&f.store, &other).unwrap();
    let second = open(&f, &other.binding(), &[1]);
    assert!(second.client.compose_drafts().unwrap().is_empty());
    assert_ne!(
        f.store
            .get(&other.binding().db_key_account())
            .unwrap()
            .unwrap()
            .to_vec(),
        first_key
    );
    assert_eq!(
        second.client.key_status().unwrap().active_epoch,
        None,
        "another device's cache is never applied"
    );
    drop(second);

    // A wrong stored key is reported, never treated as a new database; nothing is reset.
    f.store
        .set(&f.binding.db_key_account(), &[7u8; 32])
        .unwrap();
    let error = open_session(f.dir.path(), &f.store, &f.binding, &[], noop())
        .err()
        .unwrap();
    assert_eq!(error.code, "database-key-mismatch");
    f.store
        .set(&f.binding.db_key_account(), &first_key)
        .unwrap();
    assert_eq!(
        open(&f, &f.binding, &[]).client.compose_drafts().unwrap()[0].text,
        "kept"
    );
}

#[test]
fn missing_credential_or_origin_mismatch_fails_closed() {
    let f = fixture();
    let mut moved = f.binding.clone();
    moved.origin = "https://elsewhere.test".into();
    assert_eq!(
        open_session(f.dir.path(), &f.store, &moved, &[], noop())
            .err()
            .unwrap()
            .code,
        "origin-binding"
    );
    let mut unknown = f.binding.clone();
    unknown.device_id = uuid::Uuid::new_v4().to_string();
    assert_eq!(
        open_session(f.dir.path(), &f.store, &unknown, &[], noop())
            .err()
            .unwrap()
            .code,
        "credentials-required"
    );
}

/// Real server router + PostgreSQL over TCP, real HTTP and WebSocket from the native supervisor.
#[cfg(test)]
mod real_server {
    use super::*;
    use crate::{session::VaultSummary, sync, AppState};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use ed25519_dalek::{Signer, SigningKey};
    use openpush_client_core::{
        Client, ClientConfig, Cursor, DatabaseKey, DeviceId, IncomingSms, PermitDecision,
        SendResult, VaultId,
    };
    use serde_json::{json, Value};
    use sqlx::postgres::PgPoolOptions;
    use std::{str::FromStr, time::Duration};

    const ADDRESS: &str = "+15555550123";

    struct Server {
        url: String,
        owner: String,
        vault: VaultId,
        profile: KeyProfile,
        header: VaultCheckHeader,
        fingerprint: String,
        admin: sqlx::PgPool,
        pool: sqlx::PgPool,
        schema: String,
        task: tokio::task::JoinHandle<()>,
    }

    async fn start() -> Server {
        let database_url = std::env::var("TEST_DATABASE_URL").expect(
            "TEST_DATABASE_URL required; source .opencode/sessions/messaging-foundation/test.env",
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .unwrap();
        let schema = format!("openpush_desktop_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut isolated: url::Url = database_url.parse().unwrap();
        isolated
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(isolated.as_str())
            .await
            .unwrap();
        sqlx::migrate!("../../../services/server/migrations")
            .run(&pool)
            .await
            .unwrap();
        let vault = VaultId::new();
        let profile = KeyProfile::new(vault.0, 1).unwrap();
        let fingerprint = profile.fingerprint().unwrap();
        let root = derive_root_key(PHRASE, &profile).unwrap();
        let header = create_vault_check_header(&root, profile.clone()).unwrap();
        let owner = openpush_server::api::create_owner(
            &pool,
            serde_json::to_value(&profile).unwrap(),
            serde_json::to_vec(&header).unwrap(),
            fingerprint.clone(),
            1,
        )
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_pool = pool.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, openpush_server::api::router(server_pool))
                .await
                .unwrap();
        });
        Server {
            url: format!("http://{address}"),
            owner: owner.device_token,
            vault,
            profile,
            header,
            fingerprint,
            admin,
            pool,
            schema,
            task,
        }
    }

    impl Server {
        async fn pair(&self, role: &str) -> (DeviceId, String) {
            let key = SigningKey::from_bytes(&[11; 32]);
            let id = DeviceId::new();
            let public = json!({"ed25519_public_key": URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())});
            let http = reqwest::Client::new();
            let challenge: Value = http.post(format!("{}/v1/pairing", self.url)).bearer_auth(&self.owner)
                .json(&json!({"device_id":id.0,"public_key":public,"profile_fingerprint":self.fingerprint,"key_epoch":1,"requested_role":role}))
                .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
            let token = challenge["challenge_token"].as_str().unwrap();
            let raw: [u8; 32] = URL_SAFE_NO_PAD.decode(token).unwrap().try_into().unwrap();
            let proof = openpush_protocol::pairing_proof_message(
                &raw,
                self.vault,
                id,
                &self.fingerprint,
                1,
                role,
            );
            let body: Value = http.post(format!("{}/v1/pairing/consume", self.url))
                .json(&json!({"challenge_token":token,"device_id":id.0,"public_key":public,"profile_fingerprint":self.fingerprint,"key_epoch":1,"signature":URL_SAFE_NO_PAD.encode(key.sign(&proof).to_bytes())}))
                .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
            (id, body["device_token"].as_str().unwrap().to_owned())
        }
        async fn shutdown(self) {
            self.task.abort();
            let _ = self.task.await;
            sqlx::query(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
                .execute(&self.admin)
                .await
                .unwrap();
        }
    }

    /// Minimal gateway peer using only public client-core APIs and the server's HTTP routes.
    struct Gateway {
        client: Client,
        token: String,
        url: String,
        _dir: tempfile::TempDir,
    }
    impl Gateway {
        async fn sync(&self) {
            let http = reqwest::Client::new();
            for envelope in self.client.pending_outbox().unwrap() {
                http.post(format!("{}/v1/events", self.url))
                    .bearer_auth(&self.token)
                    .json(&envelope)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
                self.client.ack_outbox(envelope.envelope_id).unwrap();
            }
            let after = self.client.receive_cursor().unwrap().0;
            let page: Value = http
                .get(format!("{}/v1/events?after={after}&limit=100", self.url))
                .bearer_auth(&self.token)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            for event in page["events"].as_array().unwrap() {
                let cursor: u64 = event["cursor"].as_str().unwrap().parse().unwrap();
                self.client
                    .ingest_raw(event["envelope"].to_string().as_bytes(), Cursor(cursor))
                    .unwrap();
            }
            while self.client.apply_pending(1000).unwrap().applied > 0 {}
        }
    }

    async fn until(what: &str, seconds: u64, mut check: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        while !check() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires TEST_DATABASE_URL (local PostgreSQL); run with --include-ignored, see reports/DU-final.md"]
    async fn real_server_import_unlock_ws_send_status_and_incoming() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let server = start().await;
        let (desktop_id, desktop_token) = server.pair("device").await;
        let (gateway_id, gateway_token) = server.pair("gateway").await;
        // The gateway reports one simulated SIM through the capability contract.
        reqwest::Client::new().post(format!("{}/v1/capabilities", server.url)).bearer_auth(&gateway_token)
            .json(&json!({"simulator":true,"capabilities":{"sims":[{"subscription_id":"sim-1","label":"SIM 1","sms":"available","mms":"unsupported"}]}}))
            .send().await.unwrap().error_for_status().unwrap();
        let gateway_dir = tempfile::tempdir().unwrap();
        let gateway = Gateway {
            client: Client::open(
                ClientConfig {
                    database_path: gateway_dir.path().join("gateway.db"),
                    vault_id: server.vault,
                    device_id: gateway_id,
                },
                DatabaseKey::new(&[3; 32]).unwrap(),
            )
            .unwrap(),
            token: gateway_token,
            url: server.url.clone(),
            _dir: gateway_dir,
        };
        gateway
            .client
            .unlock(&server.profile, &server.header, PHRASE)
            .unwrap();

        // Desktop: import through the same path as the native command (secure store = memory).
        let root = tempfile::tempdir().unwrap();
        let hints = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hints.clone();
        let state = AppState::new(
            root.path().to_path_buf(),
            Arc::new(MemoryStore::default()),
            Arc::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }),
        );
        // A credential for another origin is refused once a server is configured.
        state
            .update_config(|config| config.select_origin(&server.url))
            .unwrap();
        let foreign = parse_credential(&credential_json(
            "https://elsewhere.test",
            &server.vault.to_string(),
            &desktop_id.to_string(),
            &desktop_token,
        ))
        .unwrap();
        assert_eq!(
            crate::import_credential(&state, Arc::new(foreign))
                .await
                .unwrap_err()
                .code,
            "origin-binding"
        );
        // A wrong token is rejected by the real server before anything is stored.
        let wrong = parse_credential(&credential_json(
            &server.url,
            &server.vault.to_string(),
            &desktop_id.to_string(),
            TOKEN,
        ))
        .unwrap();
        assert_eq!(
            crate::import_credential(&state, Arc::new(wrong))
                .await
                .unwrap_err()
                .code,
            "revoked"
        );
        let credential = parse_credential(&credential_json(
            &server.url,
            &server.vault.to_string(),
            &desktop_id.to_string(),
            &desktop_token,
        ))
        .unwrap();
        crate::import_credential(&state, Arc::new(credential))
            .await
            .unwrap();
        let session = state.session().await.unwrap().unwrap();
        let key = state
            .store
            .get(&session.binding.db_key_account())
            .unwrap()
            .unwrap()
            .to_vec();

        // Reimport of the same credential keeps the database key and identity.
        let again = parse_credential(&credential_json(
            &server.url,
            &server.vault.to_string(),
            &desktop_id.to_string(),
            &desktop_token,
        ))
        .unwrap();
        crate::import_credential(&state, Arc::new(again))
            .await
            .unwrap();
        let session = state.session().await.unwrap().unwrap();
        assert_eq!(
            state
                .store
                .get(&session.binding.db_key_account())
                .unwrap()
                .unwrap()
                .to_vec(),
            key
        );

        let vault = sync::fetch_vault(
            &session.api,
            &session.binding.vault_id,
            &session.binding.device_id,
        )
        .await
        .unwrap();
        let (profile, header) = sync::vault_header(&vault).unwrap();
        crate::unlock_with(
            &state,
            &session,
            profile,
            header,
            zeroize::Zeroizing::new(PHRASE.into()),
            vault.profile_fingerprint.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            session.status.lock().unwrap().vault,
            Some(VaultSummary {
                epoch: 1,
                fingerprint: server.fingerprint.clone()
            })
        );

        until("live websocket ready and gateways discovered", 20, || {
            let status = session.status.lock().unwrap();
            status.live && status.gateways_known
        })
        .await;

        let draft = session
            .save_draft(&input(
                "draft-new",
                "",
                "hello from desktop",
                &[ADDRESS],
                "0",
            ))
            .unwrap();
        let send = routed(
            input(&draft.id, "", "", &[], &draft.revision),
            &gateway_id.to_string(),
            "sim-1",
        );
        let result = session.send_draft(&send).unwrap();
        assert!(result.accepted && result.status == "queued-local");
        until("outbox uploaded and acknowledged", 20, || {
            session.client.pending_outbox_batch(10).unwrap().is_empty()
        })
        .await;

        // Gateway executes the command once and reports Sent; the desktop learns it over WS.
        gateway.sync().await;
        let command = gateway
            .client
            .pending_commands()
            .unwrap()
            .into_iter()
            .next()
            .expect("gateway received the routed command");
        assert_eq!(command.subscription_id, "sim-1");
        assert!(matches!(
            gateway
                .client
                .begin_send_attempt(command.command_id)
                .unwrap(),
            PermitDecision::Permit(_)
        ));
        gateway
            .client
            .record_send_result(command.command_id, SendResult::Sent)
            .unwrap();
        gateway.sync().await;
        let conversation =
            openpush_client_core::ConversationId::from_str(&draft.conversation_id).unwrap();
        until("sent status delivered over the live connection", 20, || {
            session
                .client
                .messages(conversation)
                .unwrap()
                .iter()
                .any(|m| m.send_state == Some(openpush_client_core::SendState::Sent))
        })
        .await;

        // Incoming carrier SMS captured by the gateway reaches the desktop as unread.
        gateway
            .client
            .capture_incoming(IncomingSms {
                conversation_id: None,
                sender_address: ADDRESS.into(),
                body: "reply from phone".into(),
                provider_message_id: Some("p-1".into()),
                imported: false,
            })
            .unwrap();
        gateway.sync().await;
        until("incoming message applied", 20, || {
            session
                .client
                .list_conversations()
                .unwrap()
                .iter()
                .any(|c| c.unread_count >= 1)
        })
        .await;
        let (snapshot, _) = session
            .snapshot(
                None,
                crate::head(),
                Some(server.url.clone()),
                crate::notifications::NotificationPreferences::default(),
            )
            .unwrap();
        assert_eq!(snapshot.connection.state, "connected");
        assert!(snapshot
            .conversations
            .iter()
            .any(|c| c.preview == "reply from phone"
                || c.messages.iter().any(|m| m.body == "reply from phone")));
        assert_sanitized(&serde_json::to_string(&snapshot).unwrap(), root.path());
        assert!(
            hints.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "state hints were emitted"
        );

        // Revocation stops networking and is visible; local data stays.
        let http = reqwest::Client::new();
        http.post(format!("{}/v1/devices/{}/revoke", server.url, desktop_id.0))
            .bearer_auth(&server.owner)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        session.request_work();
        until("revocation observed", 40, || {
            session.status.lock().unwrap().revoked
        })
        .await;
        let (snapshot, _) = session
            .snapshot(
                None,
                crate::head(),
                None,
                crate::notifications::NotificationPreferences::default(),
            )
            .unwrap();
        assert_eq!(
            (snapshot.connection.state, snapshot.connection.error_code),
            ("error", Some("revoked"))
        );
        assert!(!snapshot.conversations.is_empty());

        state.close_session().await;
        server.shutdown().await;
    }

    async fn desktop(
        server: &Server,
        device: DeviceId,
        token: &str,
    ) -> (tempfile::TempDir, AppState, Arc<Session>) {
        let root = tempfile::tempdir().unwrap();
        let state = AppState::new(
            root.path().to_path_buf(),
            Arc::new(MemoryStore::default()),
            Arc::new(|| {}),
        );
        let credential = parse_credential(&credential_json(
            &server.url,
            &server.vault.to_string(),
            &device.to_string(),
            token,
        ))
        .unwrap();
        crate::import_credential(&state, Arc::new(credential))
            .await
            .unwrap();
        let session = state.session().await.unwrap().unwrap();
        let vault = sync::fetch_vault(
            &session.api,
            &session.binding.vault_id,
            &session.binding.device_id,
        )
        .await
        .unwrap();
        let (profile, header) = sync::vault_header(&vault).unwrap();
        crate::unlock_with(
            &state,
            &session,
            profile,
            header,
            zeroize::Zeroizing::new(PHRASE.into()),
            vault.profile_fingerprint.clone(),
        )
        .await
        .unwrap();
        (root, state, session)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires TEST_DATABASE_URL plus the local SeaweedFS S3 from test.env; run with --include-ignored"]
    async fn real_server_mms_streaming_transfer_and_public_copy() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let server = start().await;
        let (sender_id, sender_token) = server.pair("device").await;
        let (receiver_id, receiver_token) = server.pair("device").await;
        let (gateway_id, gateway_token) = server.pair("gateway").await;
        reqwest::Client::new().post(format!("{}/v1/capabilities", server.url)).bearer_auth(&gateway_token)
            .json(&json!({"simulator":true,"capabilities":{"sims":[{"subscription_id":"sim-1","sms":"available","mms":"available"}]}}))
            .send().await.unwrap().error_for_status().unwrap();
        let (sender_root, sender_state, sender) = desktop(&server, sender_id, &sender_token).await;
        let (_receiver_root, receiver_state, receiver) =
            desktop(&server, receiver_id, &receiver_token).await;
        until("sender gateways discovered", 20, || {
            sender.status.lock().unwrap().gateways_known
        })
        .await;

        let png = sender_root.path().join("holiday photo.png");
        std::fs::write(&png, sample_png(300, 200)).unwrap();
        let s = sender.clone();
        let picked = sync::blocking(move || s.prepare_attachment(&png))
            .await
            .unwrap();
        let mut draft = input("draft-new", "", "photo", &[ADDRESS], "0");
        draft.attachment_ids = vec![picked.id.clone()];
        let draft = sender.save_draft(&draft).unwrap();
        let result = sender
            .send_draft(&routed(
                input(&draft.id, "", "", &[], &draft.revision),
                &gateway_id.to_string(),
                "sim-1",
            ))
            .unwrap();
        assert!(result.accepted);
        until(
            "ciphertext streamed, finalized and the held MMS uploaded",
            30,
            || {
                sender.client.pending_uploads().unwrap().is_empty()
                    && sender.client.pending_outbox_batch(10).unwrap().is_empty()
            },
        )
        .await;
        assert!(sender.transfer_errors.lock().unwrap().is_empty());

        // Another desktop downloads the ciphertext by streaming and installs it through core.
        let attachment = openpush_client_core::AttachmentId::from_str(&picked.id).unwrap();
        until("receiver downloaded and verified the media", 40, || {
            receiver
                .client
                .attachment_info(attachment)
                .is_ok_and(|info| info.state == openpush_client_core::AttachmentState::Available)
        })
        .await;
        let (snapshot, _) = receiver
            .snapshot(
                Some(&draft.conversation_id),
                crate::head(),
                None,
                crate::notifications::NotificationPreferences::default(),
            )
            .unwrap();
        let view = snapshot
            .conversations
            .iter()
            .flat_map(|c| &c.messages)
            .flat_map(|m| &m.attachments)
            .find(|a| a.id == picked.id)
            .unwrap();
        assert_eq!(view.state, "ready");
        assert!(view
            .preview_url
            .as_deref()
            .unwrap()
            .starts_with("data:image/png;base64,"));

        // Public copy: separate re-encoded upload; retrievable without credentials.
        let remote = sender
            .remote_ids
            .lock()
            .unwrap()
            .get(attachment)
            .expect("upload recorded the server object");
        let copy = crate::create_public_copy(&sender, attachment, "holiday photo.png", &remote)
            .await
            .unwrap();
        assert!(copy
            .url
            .starts_with(&format!("{}/file/mms-usercontent/", server.url)));
        assert!(copy.url.ends_with("/holidayphoto.png"));
        let response = reqwest::get(&copy.url).await.unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.bytes().await.unwrap();
        assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 300);

        // The gateway receives the MMS command but no permit until media is verified locally.
        let gateway_dir = tempfile::tempdir().unwrap();
        let gateway = Gateway {
            client: Client::open(
                ClientConfig {
                    database_path: gateway_dir.path().join("gateway.db"),
                    vault_id: server.vault,
                    device_id: gateway_id,
                },
                DatabaseKey::new(&[4; 32]).unwrap(),
            )
            .unwrap(),
            token: gateway_token,
            url: server.url.clone(),
            _dir: gateway_dir,
        };
        gateway
            .client
            .unlock(&server.profile, &server.header, PHRASE)
            .unwrap();
        gateway.sync().await;
        let command = gateway
            .client
            .pending_commands()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(matches!(
            gateway
                .client
                .begin_send_attempt(command.command_id)
                .unwrap(),
            PermitDecision::Blocked(openpush_client_core::PermitBlock::MediaUnavailable)
        ));

        sender_state.close_session().await;
        receiver_state.close_session().await;
        server.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires TEST_DATABASE_URL (local PostgreSQL); run with --include-ignored"]
    async fn real_server_expired_cursor_resyncs_by_staged_snapshot_then_goes_live() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let server = start().await;
        let (first_id, first_token) = server.pair("device").await;
        let (late_id, late_token) = server.pair("device").await;
        let (gateway_id, gateway_token) = server.pair("gateway").await;
        reqwest::Client::new().post(format!("{}/v1/capabilities", server.url)).bearer_auth(&gateway_token)
            .json(&json!({"simulator":true,"capabilities":{"sims":[{"subscription_id":"sim-1","sms":"available","mms":"unsupported"}]}}))
            .send().await.unwrap().error_for_status().unwrap();
        let (_first_root, first_state, first) = desktop(&server, first_id, &first_token).await;
        until("gateways discovered", 20, || {
            first.status.lock().unwrap().gateways_known
        })
        .await;
        let draft = first
            .save_draft(&input("draft-new", "", "history one", &[ADDRESS], "0"))
            .unwrap();
        assert!(
            first
                .send_draft(&routed(
                    input(&draft.id, "", "", &[], &draft.revision),
                    &gateway_id.to_string(),
                    "sim-1"
                ))
                .unwrap()
                .accepted
        );
        until("history uploaded", 20, || {
            first.client.pending_outbox_batch(10).unwrap().is_empty()
        })
        .await;
        first_state.close_session().await;

        // Expire the whole transport log; immutable encrypted records remain for snapshots.
        sqlx::query("UPDATE event_log SET created_at=now()-interval '40 days' WHERE vault_id=$1")
            .bind(server.vault.0)
            .execute(&server.pool)
            .await
            .unwrap();
        assert!(
            openpush_server::api::prune_replay_log(&server.pool, Duration::from_secs(30 * 86_400))
                .await
                .unwrap()
                > 0
        );

        // A late device's replay from cursor 0 now gets 409 resync_required and stages a snapshot.
        let expired = reqwest::Client::new()
            .get(format!("{}/v1/events?after=0", server.url))
            .bearer_auth(&late_token)
            .send()
            .await
            .unwrap();
        assert_eq!(expired.status(), 409);
        let (_late_root, late_state, late) = desktop(&server, late_id, &late_token).await;
        let conversation =
            openpush_client_core::ConversationId::from_str(&draft.conversation_id).unwrap();
        until("snapshot history drained and applied", 30, || {
            late.client
                .messages(conversation)
                .unwrap()
                .iter()
                .any(|m| m.payload.body == "history one")
        })
        .await;
        assert!(
            !late.client.restore_guarded().unwrap(),
            "a desktop resync never sets the gateway restore guard"
        );
        until("live after resync", 20, || late.status.lock().unwrap().live).await;

        // New live traffic after the snapshot high-water applies over the socket.
        let gateway_dir = tempfile::tempdir().unwrap();
        let gateway = Gateway {
            client: Client::open(
                ClientConfig {
                    database_path: gateway_dir.path().join("gateway.db"),
                    vault_id: server.vault,
                    device_id: gateway_id,
                },
                DatabaseKey::new(&[5; 32]).unwrap(),
            )
            .unwrap(),
            token: gateway_token,
            url: server.url.clone(),
            _dir: gateway_dir,
        };
        gateway
            .client
            .unlock(&server.profile, &server.header, PHRASE)
            .unwrap();
        gateway
            .client
            .capture_incoming(IncomingSms {
                conversation_id: None,
                sender_address: "+15555550999".into(),
                body: "after resync".into(),
                provider_message_id: Some("p-9".into()),
                imported: false,
            })
            .unwrap();
        let http = reqwest::Client::new();
        for envelope in gateway.client.pending_outbox().unwrap() {
            http.post(format!("{}/v1/events", server.url))
                .bearer_auth(&gateway.token)
                .json(&envelope)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap();
        }
        until("live event after the snapshot applied", 20, || {
            late.client
                .list_conversations()
                .unwrap()
                .iter()
                .any(|c| c.unread_count == 1)
        })
        .await;
        late_state.close_session().await;
        server.shutdown().await;
    }

    /// A received (not self-uploaded) MMS image can be published from another desktop of the
    /// same vault through the explicit native path: the receiving desktop learns the server
    /// object ID from the bounded download metadata, re-encodes a separate copy, and the server
    /// authorizes creation by same-vault access. The private original stays private.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires TEST_DATABASE_URL plus the local SeaweedFS S3 from test.env; run with --include-ignored"]
    async fn real_server_received_image_can_be_published_as_a_separate_copy() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let server = start().await;
        let (sender_id, sender_token) = server.pair("device").await;
        let (receiver_id, receiver_token) = server.pair("device").await;
        let (gateway_id, gateway_token) = server.pair("gateway").await;
        reqwest::Client::new().post(format!("{}/v1/capabilities", server.url)).bearer_auth(&gateway_token)
            .json(&json!({"simulator":true,"capabilities":{"sims":[{"subscription_id":"sim-1","sms":"available","mms":"available"}]}}))
            .send().await.unwrap().error_for_status().unwrap();
        let (sender_root, sender_state, sender) = desktop(&server, sender_id, &sender_token).await;
        let (_receiver_root, receiver_state, receiver) =
            desktop(&server, receiver_id, &receiver_token).await;
        until("sender gateways discovered", 20, || {
            sender.status.lock().unwrap().gateways_known
        })
        .await;

        let png = sender_root.path().join("received photo.png");
        std::fs::write(&png, sample_png(120, 90)).unwrap();
        let s = sender.clone();
        let picked = sync::blocking(move || s.prepare_attachment(&png))
            .await
            .unwrap();
        let mut draft = input("draft-new", "", "photo", &[ADDRESS], "0");
        draft.attachment_ids = vec![picked.id.clone()];
        let draft = sender.save_draft(&draft).unwrap();
        assert!(
            sender
                .send_draft(&routed(
                    input(&draft.id, "", "", &[], &draft.revision),
                    &gateway_id.to_string(),
                    "sim-1"
                ))
                .unwrap()
                .accepted
        );
        until("sender uploaded the private ciphertext", 30, || {
            sender.client.pending_uploads().unwrap().is_empty()
                && sender.client.pending_outbox_batch(10).unwrap().is_empty()
        })
        .await;

        let attachment = openpush_client_core::AttachmentId::from_str(&picked.id).unwrap();
        until(
            "receiver downloaded and verified the received image",
            40,
            || {
                receiver
                    .client
                    .attachment_info(attachment)
                    .is_ok_and(|info| {
                        info.state == openpush_client_core::AttachmentState::Available
                    })
            },
        )
        .await;
        // The receiving desktop never uploaded this object; its server ID came from the
        // authenticated download metadata.
        let remote = receiver
            .remote_ids
            .lock()
            .unwrap()
            .get(attachment)
            .expect("download recorded the server object ID");
        assert_eq!(
            Some(remote.clone()),
            sender.remote_ids.lock().unwrap().get(attachment)
        );

        // Explicit native publication path (the command adds the native confirmation dialog).
        let info = receiver.client.attachment_info(attachment).unwrap();
        let prepared = crate::prepare_public_copy(&receiver, attachment, &info.display_name)
            .await
            .unwrap();
        assert!(
            crate::public_copy_prompt(&info.display_name, &prepared).contains("receivedphoto.png")
        );
        let copy = crate::upload_public_copy(&receiver, &remote, prepared)
            .await
            .expect("same-vault receiver may create a separate public copy");
        assert!(copy.url.ends_with("/receivedphoto.png"));
        let public = reqwest::get(&copy.url).await.unwrap();
        assert_eq!(public.status(), 200);
        let public = public.bytes().await.unwrap();
        let decoded = image::load_from_memory(&public).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (120, 90));

        // The original stays private ciphertext: unauthenticated access is refused, and the public
        // bytes are a different object from the private ciphertext.
        let anonymous = reqwest::get(format!("{}/v1/attachments/{remote}", server.url))
            .await
            .unwrap();
        assert_eq!(anonymous.status(), 401);
        let private = reqwest::Client::new()
            .get(format!("{}/v1/attachments/{remote}", server.url))
            .bearer_auth(&receiver_token)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_ne!(private.as_ref(), public.as_ref());
        assert!(
            image::load_from_memory(&private).is_err(),
            "private object is ciphertext, not an image"
        );

        sender_state.close_session().await;
        receiver_state.close_session().await;
        server.shutdown().await;
    }
}

/// Retained-stack notification/SMS smoke. This is deliberately ignored: it uses the existing
/// paired API-35 emulator and private disposable credentials named by environment variables.
/// It drives only production Android callbacks, client-core APIs, and the real desktop supervisor.
#[cfg(feature = "retained-stack-smoke")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires retained emulator-5582, localhost:18089, and OPENPUSH_E2E_* private paths"]
async fn retained_android_notification_round_trip_smoke() {
    use crate::{sync, AppState};
    use std::{process::Command, time::Duration};

    fn required(name: &str) -> String {
        std::env::var(name)
            .unwrap_or_else(|_| panic!("{name} must name a private retained-stack input"))
    }
    fn adb(args: &[&str]) -> String {
        let binary = std::env::var("OPENPUSH_E2E_ADB").unwrap_or_else(|_| "adb".into());
        let output = Command::new(binary)
            .arg("-s")
            .arg("emulator-5582")
            .args(args)
            .output()
            .expect("adb executable");
        assert!(
            output.status.success(),
            "adb command failed (secret-free stderr): {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("adb emitted UTF-8")
    }
    fn post(tag: &str, title: &str, body: &str) {
        adb(&[
            "shell",
            "cmd",
            "notification",
            "post",
            "-t",
            title,
            tag,
            body,
        ]);
    }
    fn listed(tag: &str) -> bool {
        adb(&["shell", "cmd", "notification", "list"])
            .lines()
            .any(|line| line.contains("com.android.shell") && line.contains(&format!("|{tag}|")))
    }
    fn swipe_title(title: &str) {
        adb(&["shell", "cmd", "statusbar", "expand-notifications"]);
        std::thread::sleep(Duration::from_secs(1));
        adb(&["shell", "uiautomator", "dump", "/sdcard/openpush-e2e.xml"]);
        let xml = adb(&["shell", "cat", "/sdcard/openpush-e2e.xml"]);
        let needle = format!("text=\"{title}\"");
        let at = xml
            .find(&needle)
            .expect("synthetic notification title is visible in shade");
        let start = xml[..at].rfind("<node").expect("title node starts");
        let end = xml[at..]
            .find("/>")
            .map(|n| at + n)
            .expect("title node ends");
        let node = &xml[start..end];
        let bounds = node
            .split("bounds=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .expect("title bounds");
        let numbers: Vec<i32> = bounds
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(numbers.len(), 4, "four title bounds");
        let y = (numbers[1] + numbers[3]) / 2;
        adb(&[
            "shell",
            "input",
            "swipe",
            "950",
            &y.to_string(),
            "50",
            &y.to_string(),
            "350",
        ]);
        std::thread::sleep(Duration::from_secs(1));
        adb(&["shell", "cmd", "statusbar", "collapse"]);
    }
    fn force_phone_sync() {
        // A synthetic emulator SMS traverses the existing production receiver and enqueues the
        // same one-time GatewayWork pass as carrier input, without relying on ephemeral scheduler
        // IDs or private app hooks. It is emulator-only test traffic.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            .to_string();
        adb(&[
            "emu",
            "sms",
            "send",
            "+15555550198",
            &format!("OpenPush-sync-trigger-{nonce}"),
        ]);
    }
    async fn until(what: &str, seconds: u64, mut check: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        while !check() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    async fn pump_desktop(session: &Arc<Session>) {
        for envelope in session.client.pending_outbox().unwrap() {
            session
                .api
                .post_discard("/v1/events", &envelope)
                .await
                .unwrap();
            session.client.ack_outbox(envelope.envelope_id).unwrap();
        }
        let after = session.client.receive_cursor().unwrap().0;
        let page: serde_json::Value = session
            .api
            .get_json(
                &format!("/v1/events?after={after}&limit=100"),
                crate::net::MAX_PAGE_BYTES,
            )
            .await
            .unwrap();
        for event in page["events"].as_array().unwrap() {
            let cursor = event["cursor"].as_str().unwrap().parse().unwrap();
            session
                .client
                .ingest_raw(
                    event["envelope"].to_string().as_bytes(),
                    openpush_client_core::Cursor(cursor),
                )
                .unwrap();
        }
        while session.client.apply_pending(1000).unwrap().applied > 0 {}
    }
    async fn until_synced(
        what: &str,
        seconds: u64,
        session: &Arc<Session>,
        mut check: impl FnMut() -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        loop {
            pump_desktop(session).await;
            if check() {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    let credential_path = required("OPENPUSH_E2E_DESKTOP_CREDENTIAL");
    let phrase_path = required("OPENPUSH_E2E_PASSPHRASE");
    let credential_bytes = std::fs::read(credential_path).expect("read private desktop credential");
    let credential = parse_credential(&credential_bytes).expect("parse desktop credential");
    let credential_url = url::Url::parse(&credential.origin).expect("credential origin URL");
    assert!(
        matches!(
            credential_url.host_str(),
            Some("127.0.0.1" | "localhost" | "::1")
        ),
        "retained-stack credential origin must use a loopback host"
    );
    let credential = Arc::new(credential);
    drop(credential_bytes);
    let phrase = zeroize::Zeroizing::new(
        std::fs::read_to_string(phrase_path)
            .expect("read private passphrase")
            .trim_end_matches(['\r', '\n'])
            .to_owned(),
    );
    let root = tempfile::tempdir().unwrap();
    let state = AppState::new(
        root.path().to_path_buf(),
        Arc::new(MemoryStore::default()),
        Arc::new(|| {}),
    );
    crate::import_credential(&state, credential).await.unwrap();
    let session = state.session().await.unwrap().unwrap();
    let vault = sync::fetch_vault(
        &session.api,
        &session.binding.vault_id,
        &session.binding.device_id,
    )
    .await
    .unwrap();
    let (profile, header) = sync::vault_header(&vault).unwrap();
    crate::unlock_with(
        &state,
        &session,
        profile,
        header,
        phrase,
        vault.profile_fingerprint,
    )
    .await
    .unwrap();
    until("desktop native session live", 30, || {
        session.status.lock().unwrap().live
    })
    .await;
    // Normalize any pending control record left by an interrupted prior smoke before capturing.
    force_phone_sync();
    tokio::time::sleep(Duration::from_secs(3)).await;

    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let first_tag = format!("op-e2e-first-{nonce}");
    let first_title = format!("OpenPushE2EFirst{nonce}");
    let first_body = format!("notification-body-{nonce}");
    post(&first_tag, &first_title, &first_body);
    let first = {
        let mut found = None;
        until(
            "Android notification in desktop native snapshot",
            40,
            || {
                found = session
                    .client
                    .notification_snapshot()
                    .unwrap()
                    .notifications
                    .into_iter()
                    .find(|n| n.title == first_title && n.text == first_body);
                found.is_some()
            },
        )
        .await;
        found.unwrap()
    };
    let candidates = session.client.pending_banner_candidates(20).unwrap();
    assert!(
        candidates
            .iter()
            .any(|c| c.kind == "notification" && c.title == first_title && c.body == first_body),
        "live Android post produced a desktop native banner candidate"
    );

    // Upload the desktop-origin mute and force a production phone sync before posting. The
    // desktop is also muted locally, so the assertion proves suppressed desktop presentation;
    // it does not by itself prove that the phone emitted no ciphertext.
    session
        .client
        .set_app_muted(
            &first.target.source_device_id,
            &first.package_name,
            &first.app_name,
            true,
        )
        .unwrap();
    session.request_work();
    until("desktop mute uploaded", 20, || {
        session.client.pending_outbox_batch(10).unwrap().is_empty()
    })
    .await;
    force_phone_sync();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let muted_tag = format!("op-e2e-muted-{nonce}");
    let muted_title = format!("OpenPushE2EMuted{nonce}");
    post(&muted_tag, &muted_title, "must-not-mirror");
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        session
            .client
            .notification_snapshot()
            .unwrap()
            .notifications
            .iter()
            .all(|n| n.title != muted_title),
        "desktop mute suppressed the notification from its native snapshot"
    );

    // Unmute is delivered by a normal production worker pass, then a new post mirrors again.
    session
        .client
        .set_app_muted(
            &first.target.source_device_id,
            &first.package_name,
            &first.app_name,
            false,
        )
        .unwrap();
    session.request_work();
    until("desktop unmute uploaded", 20, || {
        session.client.pending_outbox_batch(10).unwrap().is_empty()
    })
    .await;
    force_phone_sync();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let second_tag = format!("op-e2e-second-{nonce}");
    let second_title = format!("OpenPushE2ESecond{nonce}");
    post(&second_tag, &second_title, "unmuted-body");
    let second = {
        let mut found = None;
        until_synced("post-unmute notification mirrored", 40, &session, || {
            found = session
                .client
                .notification_snapshot()
                .unwrap()
                .notifications
                .into_iter()
                .find(|n| n.title == second_title);
            found.is_some()
        })
        .await;
        found.unwrap()
    };

    // Desktop dismissal uploads, is applied by Android's NLS, and the OS notification disappears.
    session
        .client
        .dismiss_notification(second.target.clone())
        .unwrap();
    session.request_work();
    until("desktop dismissal uploaded", 20, || {
        session.client.pending_outbox_batch(10).unwrap().is_empty()
    })
    .await;
    force_phone_sync();
    until("desktop dismissal applied to Android OS", 20, || {
        !listed(&second_tag)
    })
    .await;
    let dismissed_target = second.target.clone();
    until_synced(
        "phone removal round-tripped to desktop snapshot",
        20,
        &session,
        || {
            session
                .client
                .notification_snapshot()
                .unwrap()
                .notifications
                .iter()
                .all(|notification| notification.target != dismissed_target)
        },
    )
    .await;

    // Removed/reposted-key safety: an old-lifetime dismissal removes that OS item, then the same
    // Android tag/key is posted again. The retained old dismissal must not cancel the new lifetime.
    let safety_tag = format!("op-e2e-safety-{nonce}");
    let old_title = format!("OpenPushE2EOld{nonce}");
    post(&safety_tag, &old_title, "old-lifetime");
    let old = {
        let mut found = None;
        until_synced("old safety lifetime mirrored", 40, &session, || {
            found = session
                .client
                .notification_snapshot()
                .unwrap()
                .notifications
                .into_iter()
                .find(|n| n.title == old_title);
            found.is_some()
        })
        .await;
        found.unwrap()
    };
    session
        .client
        .dismiss_notification(old.target.clone())
        .unwrap();
    session.request_work();
    until("old lifetime dismissal uploaded", 20, || {
        session.client.pending_outbox_batch(10).unwrap().is_empty()
    })
    .await;
    force_phone_sync();
    until("old Android lifetime removed", 20, || !listed(&safety_tag)).await;
    let new_title = format!("OpenPushE2ENew{nonce}");
    post(&safety_tag, &new_title, "new-lifetime");
    until("reposted Android item active", 10, || listed(&safety_tag)).await;
    let new_item = {
        let mut found = None;
        until_synced("reposted lifetime mirrored", 40, &session, || {
            found = session
                .client
                .notification_snapshot()
                .unwrap()
                .notifications
                .into_iter()
                .find(|n| n.title == new_title);
            found.is_some()
        })
        .await;
        found.unwrap()
    };
    assert_ne!(
        old.target.lifetime, new_item.target.lifetime,
        "repost allocated a new lifetime"
    );
    assert!(
        listed(&safety_tag),
        "old dismissal did not cancel reposted OS notification"
    );

    // Existing synthetic emulator SMS still traverses the production receiver/encryption/server path.
    let sms_body = format!("OpenPush-E2E-SMS-{nonce}");
    adb(&["emu", "sms", "send", "+15555550199", &sms_body]);
    until_synced(
        "synthetic SMS in desktop native snapshot",
        40,
        &session,
        || {
            session
                .client
                .list_conversations()
                .unwrap()
                .into_iter()
                .any(|conversation| {
                    session
                        .client
                        .messages(conversation.conversation_id)
                        .unwrap()
                        .iter()
                        .any(|message| message.payload.body == sms_body)
                })
        },
    )
    .await;

    // Restore the per-app allow state and clear retained synthetic shell notifications.
    session
        .client
        .set_app_muted(
            &first.target.source_device_id,
            &first.package_name,
            &first.app_name,
            false,
        )
        .unwrap();
    session
        .client
        .dismiss_notification(new_item.target)
        .unwrap();
    session.request_work();
    until("cleanup controls uploaded", 20, || {
        session.client.pending_outbox_batch(10).unwrap().is_empty()
    })
    .await;
    force_phone_sync();
    until("safety notification cleanup", 20, || !listed(&safety_tag)).await;
    if listed(&first_tag) {
        swipe_title(&first_title);
    }
    if listed(&muted_tag) {
        swipe_title(&muted_title);
    }
    state.close_session().await;
}
