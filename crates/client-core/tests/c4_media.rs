//! C4: encrypted MMS media between two real clients. Object transfer is an in-memory map that
//! stands in for the native host's HTTP upload/download; it is not a claim about the S3 path.
mod common;
use common::*;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fs, path::Path};
use tempfile::TempDir;

/// Synthetic PNG-signature image with deterministic, non-repeating content.
fn image(len: usize, seed: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend((0..len).map(|i| (i as u32).wrapping_mul(2_654_435_761).to_be_bytes()[0] ^ seed));
    bytes
}
fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn media_dir(dir: &TempDir, name: &str, sub: &str) -> std::path::PathBuf {
    dir.path().join(format!("{name}.db.media")).join(sub)
}
fn scratch_is_empty(dir: &TempDir, name: &str) -> bool {
    ["tmp", "plain"].iter().all(|sub| {
        fs::read_dir(media_dir(dir, name, sub))
            .unwrap()
            .next()
            .is_none()
    })
}

/// Stand-in object store: the native host uploads ciphertext and reports the server object ID.
#[derive(Default)]
struct Objects(HashMap<String, Vec<u8>>);
impl Objects {
    fn upload_all(&mut self, client: &Client) -> usize {
        let pending = client.pending_uploads().unwrap();
        for object in &pending {
            assert_eq!(object.remote_object_id, None);
            let bytes = fs::read(client.native_cipher_file(object.attachment_id).unwrap()).unwrap();
            assert_eq!(
                (bytes.len() as u64, hex_sha256(&bytes)),
                (object.ciphertext_bytes, object.ciphertext_sha256.clone())
            );
            let remote = uuid::Uuid::new_v4().to_string();
            self.0.insert(remote.clone(), bytes);
            client
                .mark_attachment_uploaded(object.attachment_id, &remote)
                .unwrap();
        }
        pending.len()
    }
    fn download_to(&self, object: &CipherObject, path: &Path) -> Vec<u8> {
        let bytes = self.0[object.remote_object_id.as_ref().unwrap()].clone();
        fs::write(path, &bytes).unwrap();
        bytes
    }
}

#[test]
fn draft_mms_is_atomic_held_for_upload_and_permit_waits_for_verified_media() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let (desktop_cfg, gateway_cfg) = (
        config(&dir, "desktop", &vault),
        config(&dir, "gateway", &vault),
    );
    let desktop = unlocked(&desktop_cfg, &vault);
    let gateway = unlocked(&gateway_cfg, &vault);
    let (mut server, mut objects) = (Server::default(), Objects::default());

    let picture = image(150_000, 7);
    let picked = dir.path().join("picked.png");
    fs::write(&picked, &picture).unwrap();
    let info = desktop
        .prepare_attachment(&picked, "image/png", "../../Pictures/cat.png")
        .unwrap();
    assert_eq!(
        (info.display_name.as_str(), info.state),
        ("cat.png", AttachmentState::PendingUpload)
    );
    assert_eq!(info.plaintext_bytes, picture.len() as u64);
    let cipher = fs::read(desktop.native_cipher_file(info.attachment_id).unwrap()).unwrap();
    assert_eq!(
        (cipher.len() as u64, hex_sha256(&cipher)),
        (info.ciphertext_bytes, info.ciphertext_sha256.clone())
    );
    assert!(
        !cipher.windows(64).any(|w| w == &picture[1000..1064]),
        "no plaintext in the cipher file"
    );
    assert!(
        desktop.pending_uploads().unwrap().is_empty(),
        "draft-only media is not uploaded"
    );

    let draft = desktop.create_compose_draft(None).unwrap();
    let update = ComposeDraftUpdate {
        text: "look".into(),
        recipients: vec![ADDRESS.into()],
        attachment_ids: vec![info.attachment_id],
        route: Some(route(&gateway_cfg)),
    };
    desktop
        .save_compose_draft(draft.draft_id, 0, update)
        .unwrap();
    let queued = desktop.send_compose_draft(draft.draft_id, 1).unwrap();
    assert_eq!(
        desktop.send_compose_draft(draft.draft_id, 1),
        Err(Error::StaleDraft {
            current_revision: 2
        })
    );
    let cleared = desktop.compose_draft(draft.draft_id).unwrap().unwrap();
    assert_eq!(
        (
            cleared.text.as_str(),
            cleared.attachment_ids.len(),
            cleared.revision
        ),
        ("", 0, 2)
    );
    let sent = only_message(&desktop, draft.conversation_id);
    assert_eq!(sent.payload.transport, Transport::Mms);
    assert_eq!(
        sent.payload.record.attachments,
        vec![AttachmentReference {
            attachment_id: info.attachment_id,
            pending: false
        }]
    );
    assert_eq!(sent.send_state, Some(SendState::QueuedLocal));

    // Held (unsealed) until the object is uploaded, durably across reopen.
    assert!(desktop.pending_outbox().unwrap().is_empty());
    drop(desktop);
    let desktop = unlocked(&desktop_cfg, &vault);
    assert!(desktop.pending_outbox().unwrap().is_empty());
    assert_eq!(objects.upload_all(&desktop), 1);
    let remote = desktop.pending_downloads().unwrap();
    assert!(remote.is_empty());
    let outbox = desktop.pending_outbox().unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].command_id, Some(queued.command_id));
    let remote_id = objects.0.keys().next().unwrap().clone();
    desktop
        .mark_attachment_uploaded(info.attachment_id, &remote_id)
        .unwrap();
    assert_eq!(
        desktop.mark_attachment_uploaded(info.attachment_id, &uuid::Uuid::new_v4().to_string()),
        Err(Error::Conflict)
    );

    // Gateway learns metadata before bytes; no permit until the object is verified locally.
    server.upload(&desktop);
    server.sync(&gateway);
    let command = gateway.pending_commands().unwrap().remove(0);
    assert!(command.message.record.attachments[0].pending);
    assert_eq!(
        gateway.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::MediaUnavailable)
    );
    let downloads = gateway.pending_downloads().unwrap();
    assert_eq!(downloads.len(), 1);
    assert_eq!(
        downloads[0].remote_object_id.as_deref(),
        Some(remote_id.as_str())
    );
    let gateway_info = gateway.attachment_info(info.attachment_id).unwrap();
    assert_eq!(
        (
            gateway_info.media_type.as_str(),
            gateway_info.display_name.as_str(),
            gateway_info.state
        ),
        ("image/png", "cat.png", AttachmentState::PendingDownload)
    );
    assert!(matches!(
        gateway.open_native_plaintext(info.attachment_id),
        Err(Error::InvalidMedia)
    ));

    // Tampered, truncated, trailing and swapped objects are rejected with nothing promoted.
    let download = dir.path().join("download.bin");
    let genuine = objects.download_to(&downloads[0], &download);
    let other_path = dir.path().join("other.png");
    fs::write(&other_path, image(150_000, 9)).unwrap();
    let other = desktop
        .prepare_attachment(&other_path, "image/png", "other.png")
        .unwrap();
    let swapped = fs::read(desktop.native_cipher_file(other.attachment_id).unwrap()).unwrap();
    let mut flipped = genuine.clone();
    flipped[genuine.len() / 2] ^= 1;
    let mut trailing = genuine.clone();
    trailing.push(0);
    for bad in [
        flipped,
        genuine[..genuine.len() - 1].to_vec(),
        trailing,
        swapped,
    ] {
        fs::write(&download, &bad).unwrap();
        assert_eq!(
            gateway.install_downloaded_attachment(info.attachment_id, &download),
            Err(Error::InvalidMedia)
        );
        assert_eq!(
            gateway.attachment_info(info.attachment_id).unwrap().state,
            AttachmentState::PendingDownload
        );
        assert!(scratch_is_empty(&dir, "gateway"));
        assert!(
            !media_dir(&dir, "gateway", "cipher")
                .join(format!("{}.ppss", info.attachment_id))
                .exists()
        );
    }
    assert_eq!(
        gateway.begin_send_attempt(queued.command_id).unwrap(),
        PermitDecision::Blocked(PermitBlock::MediaUnavailable)
    );

    fs::write(&download, &genuine).unwrap();
    gateway
        .install_downloaded_attachment(info.attachment_id, &download)
        .unwrap();
    gateway
        .install_downloaded_attachment(info.attachment_id, &download)
        .unwrap(); // idempotent
    assert_eq!(
        gateway.attachment_info(info.attachment_id).unwrap().state,
        AttachmentState::Available
    );
    assert!(gateway.pending_downloads().unwrap().is_empty());
    {
        let plain = gateway.open_native_plaintext(info.attachment_id).unwrap();
        assert!(
            plain
                .path()
                .starts_with(media_dir(&dir, "gateway", "plain").canonicalize().unwrap())
        );
        assert_eq!(
            fs::read(plain.path()).unwrap(),
            picture,
            "exact bytes after install"
        );
    }
    assert!(
        scratch_is_empty(&dir, "gateway"),
        "plaintext removed when the handle drops"
    );

    let PermitDecision::Permit(permit) = gateway.begin_send_attempt(queued.command_id).unwrap()
    else {
        panic!("verified media must permit");
    };
    assert!(!permit.message.record.attachments[0].pending);
    assert_eq!(permit.message.body, "look");
    gateway
        .record_send_result(queued.command_id, SendResult::Sent)
        .unwrap();
    server.upload(&gateway);
    server.sync(&desktop);
    assert_eq!(
        only_message(&desktop, draft.conversation_id).send_state,
        Some(SendState::Sent)
    );
    assert!(desktop.quarantined().unwrap().is_empty() && gateway.quarantined().unwrap().is_empty());
}

#[test]
fn incoming_mms_metadata_before_bytes_keeps_unread_stable() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let gateway = unlocked(&config(&dir, "gateway", &vault), &vault);
    let desktop_cfg = config(&dir, "desktop", &vault);
    let desktop = unlocked(&desktop_cfg, &vault);
    let (mut server, mut objects) = (Server::default(), Objects::default());

    let part = image(40_000, 3);
    let part_path = dir.path().join("part-1");
    fs::write(&part_path, &part).unwrap();
    let info = gateway
        .prepare_attachment(&part_path, "image/jpeg", "IMG_0001.jpg")
        .unwrap();
    let mms = IncomingMms {
        conversation_id: None,
        sender_address: ADDRESS.into(),
        recipients: vec![ADDRESS.into()],
        subject: None,
        body: String::new(),
        provider_message_id: Some("mms-1".into()),
        imported: false,
        attachment_ids: vec![info.attachment_id],
    };
    let captured = gateway.capture_incoming_mms(mms.clone()).unwrap();
    assert!(gateway.capture_incoming_mms(mms).unwrap().duplicate);
    assert!(
        gateway.pending_outbox().unwrap().is_empty(),
        "event waits for the upload"
    );
    assert_eq!(objects.upload_all(&gateway), 1);
    assert_eq!(server.upload(&gateway), 1);

    server.sync(&desktop);
    assert_eq!(desktop.unread_count(captured.conversation_id).unwrap(), 1);
    let message = only_message(&desktop, captured.conversation_id);
    assert_eq!(
        message.payload.record.attachments,
        vec![AttachmentReference {
            attachment_id: info.attachment_id,
            pending: true
        }]
    );
    assert_eq!(
        desktop
            .attachment_info(info.attachment_id)
            .unwrap()
            .display_name,
        "IMG_0001.jpg"
    );

    // Same metadata replayed at a new cursor changes nothing.
    assert_eq!(
        desktop.ingest(&server.log[0], Cursor(2)).unwrap(),
        IngestResult::Duplicate
    );
    desktop.apply_pending(10).unwrap();
    assert_eq!(desktop.messages(captured.conversation_id).unwrap().len(), 1);
    assert_eq!(desktop.unread_count(captured.conversation_id).unwrap(), 1);

    // Received media cannot be re-sent as if prepared here.
    assert_eq!(
        desktop.queue_mms(
            OutgoingMms {
                conversation_id: captured.conversation_id,
                recipients: vec![ADDRESS.into()],
                body: String::new(),
                attachment_ids: vec![info.attachment_id],
                subject: None,
            },
            GatewayRoute {
                gateway_device_id: DeviceId::new(),
                subscription_id: "sim-1".into()
            },
        ),
        Err(Error::InvalidRequest(
            "attachment not prepared on this device"
        ))
    );

    let download = dir.path().join("download.bin");
    objects.download_to(&desktop.pending_downloads().unwrap()[0], &download);
    desktop
        .install_downloaded_attachment(info.attachment_id, &download)
        .unwrap();
    assert_eq!(
        desktop.unread_count(captured.conversation_id).unwrap(),
        1,
        "install never changes unread"
    );
    assert!(
        !only_message(&desktop, captured.conversation_id)
            .payload
            .record
            .attachments[0]
            .pending
    );
    drop(desktop);
    let desktop = unlocked(&desktop_cfg, &vault);
    assert_eq!(
        fs::read(
            desktop
                .open_native_plaintext(info.attachment_id)
                .unwrap()
                .path()
        )
        .unwrap(),
        part
    );
    assert!(desktop.mark_seen(captured.message_id).unwrap());
    assert_eq!(desktop.unread_count(captured.conversation_id).unwrap(), 0);
}

#[test]
fn text_only_group_mms_remains_explicit_mms() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let gateway_cfg = config(&dir, "gateway", &vault);
    let desktop = unlocked(&config(&dir, "desktop", &vault), &vault);
    let gateway = unlocked(&gateway_cfg, &vault);
    let conversation = ConversationId::new();
    let queued = desktop
        .queue_mms(
            OutgoingMms {
                conversation_id: conversation,
                recipients: vec![ADDRESS.into(), "+15555550198".into()],
                body: "group text without media".into(),
                attachment_ids: vec![],
                subject: Some("hello".into()),
            },
            route(&gateway_cfg),
        )
        .unwrap();
    let mut server = Server::default();
    server.upload(&desktop);
    server.sync(&gateway);
    let command = gateway.pending_commands().unwrap().remove(0);
    assert_eq!(command.command_id, queued.command_id);
    assert_eq!(command.message.transport, Transport::Mms);
    assert!(command.message.record.attachments.is_empty());
    assert_eq!(command.message.subject.as_deref(), Some("hello"));
    assert_eq!(command.message.recipients.len(), 2);
}

#[test]
fn prepare_attachment_bounds_names_and_cleans_up() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let client = open(&config(&dir, "device", &vault)); // media prep needs no vault keys
    let huge = dir.path().join("huge.bin");
    fs::File::create(&huge)
        .unwrap()
        .set_len(32 * 1024 * 1024 + 1)
        .unwrap();
    assert!(matches!(
        client.prepare_attachment(&huge, "image/png", "x"),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        client.prepare_attachment(dir.path(), "image/png", "x"),
        Err(Error::InvalidRequest(_))
    ));
    let small = dir.path().join("small.bin");
    fs::write(&small, b"tiny").unwrap();
    assert_eq!(
        client.prepare_attachment(&small, "Image/PNG", "x"),
        Err(Error::InvalidRequest("media type"))
    );
    let info = client
        .prepare_attachment(&small, "application/octet-stream", "a\u{0}b/..")
        .unwrap();
    assert_eq!(info.display_name, "attachment");
    assert_eq!(
        client
            .prepare_attachment(&small, "text/plain", "C:\\evil\\n\u{7}.txt")
            .unwrap()
            .display_name,
        "n_.txt"
    );
    assert!(scratch_is_empty(&dir, "device"));
    assert_eq!(
        fs::read_dir(media_dir(&dir, "device", "cipher"))
            .unwrap()
            .count(),
        2
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&media_dir(&dir, "device", "")), 0o700);
        assert_eq!(
            mode(&client.native_cipher_file(info.attachment_id).unwrap()),
            0o600
        );
    }
    assert_eq!(
        client.mark_attachment_uploaded(info.attachment_id, "not-a-uuid"),
        Err(Error::InvalidRequest("remote object id"))
    );
    assert_eq!(
        client.attachment_info(AttachmentId::new()),
        Err(Error::NotFound)
    );
}
