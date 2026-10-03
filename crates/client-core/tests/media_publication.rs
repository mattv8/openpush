//! Private MMS envelopes remain out of the normal outbox until their media is uploaded.
mod common;
use common::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn private_media_holds_only_its_envelope_until_upload_across_reopen() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let (desktop_cfg, gateway_cfg) = (
        config(&dir, "desktop", &vault),
        config(&dir, "gateway", &vault),
    );
    let client = unlocked(&desktop_cfg, &vault);

    let source = dir.path().join("private.png");
    fs::write(&source, b"private MMS bytes").unwrap();
    let attachment = client
        .prepare_attachment(&source, "image/png", "private.png")
        .unwrap();
    let draft = client.create_compose_draft(None).unwrap();
    client
        .save_compose_draft(
            draft.draft_id,
            draft.revision,
            ComposeDraftUpdate {
                text: "private media".into(),
                recipients: vec![ADDRESS.into()],
                attachment_ids: vec![attachment.attachment_id],
                route: Some(route(&gateway_cfg)),
            },
        )
        .unwrap();
    let media = client.send_compose_draft(draft.draft_id, 1).unwrap();
    let text = client
        .queue_send(
            sms(ConversationId::new(), "unrelated text remains eligible"),
            route(&gateway_cfg),
        )
        .unwrap();

    let pending = client.pending_outbox_batch(10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].envelope_id, text.envelope_id,
        "the unuploaded private attachment must not consume the normal outbox batch"
    );

    drop(client);
    let client = unlocked(&desktop_cfg, &vault);
    assert_eq!(
        client
            .pending_outbox_batch(10)
            .unwrap()
            .iter()
            .map(|envelope| envelope.envelope_id)
            .collect::<Vec<_>>(),
        vec![text.envelope_id],
        "reopening must preserve the media hold"
    );

    client
        .mark_attachment_uploaded(attachment.attachment_id, &uuid::Uuid::new_v4().to_string())
        .unwrap();
    assert_eq!(
        client
            .pending_outbox_batch(10)
            .unwrap()
            .iter()
            .map(|envelope| envelope.envelope_id)
            .collect::<Vec<_>>(),
        vec![media.envelope_id, text.envelope_id],
        "uploading releases the held media envelope without withholding text"
    );
}
