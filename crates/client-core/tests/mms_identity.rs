//! Own-address metadata is encrypted, convergent, and only changes reply presentation.
mod common;
use common::*;

#[test]
fn own_address_sync_unblocks_an_existing_mms_group_without_rekeying_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let vault = Vault::new();
    let phone_cfg = config(&dir, "phone", &vault);
    let desktop_cfg = config(&dir, "desktop", &vault);
    let phone = unlocked(&phone_cfg, &vault);
    let desktop = unlocked(&desktop_cfg, &vault);
    let mut server = Server::default();

    let acquisition = phone
        .begin_mms_acquisition(MmsAcquisitionInput {
            source: MmsSource {
                source_generation: "telephony-v1".into(),
                subscription_id: "sim-1".into(),
                provider_message_id: "42".into(),
                provider_thread_id: Some("9".into()),
            },
            direction: Direction::Incoming,
            sender_address: Some("+15555550101".into()),
            recipients: vec!["+15555550100".into(), "+15555550102".into()],
            subject: Some("trip".into()),
            body: "photo later".into(),
            imported: false,
            observed_at_ms: 1,
            transaction_id: None,
        })
        .unwrap();
    let captured = phone
        .complete_mms_acquisition(&acquisition.acquisition_id)
        .unwrap();
    server.upload(&phone);
    server.sync(&desktop);

    let before = desktop.mms_reply_context(captured.conversation_id).unwrap();
    assert!(before.recipients.is_empty());
    assert!(before.blocked_reason.is_some());
    phone.set_mms_own_address("sim-1", "+15555550100").unwrap();
    server.upload(&phone);
    server.sync(&desktop);

    let after = desktop.mms_reply_context(captured.conversation_id).unwrap();
    assert_eq!(after.recipients, vec!["+15555550101", "+15555550102"]);
    assert_eq!(after.subject.as_deref(), Some("trip"));
    assert_eq!(desktop.messages(captured.conversation_id).unwrap().len(), 1);
}

fn complete_contextual(
    client: &Client,
    id: &str,
    direction: Direction,
    sender: Option<&str>,
    recipients: &[&str],
) -> ConversationId {
    let acquisition = client
        .begin_mms_acquisition(MmsAcquisitionInput {
            source: MmsSource {
                source_generation: "telephony-v1".into(),
                subscription_id: "sim-1".into(),
                provider_message_id: id.into(),
                provider_thread_id: Some(format!("thread-{id}")),
            },
            direction,
            sender_address: sender.map(str::to_owned),
            recipients: recipients.iter().map(|value| (*value).into()).collect(),
            subject: None,
            body: "hello".into(),
            imported: false,
            observed_at_ms: 1,
            transaction_id: None,
        })
        .unwrap();
    client
        .complete_mms_acquisition(&acquisition.acquisition_id)
        .unwrap()
        .conversation_id
}

#[test]
fn outgoing_reply_uses_only_provider_recipients() {
    let dir = tempfile::TempDir::new().unwrap();
    let vault = Vault::new();
    let client = unlocked(&config(&dir, "phone", &vault), &vault);
    let conversation = complete_contextual(
        &client,
        "outgoing",
        Direction::Outgoing,
        Some("insert-address-token"),
        &["+15555550101"],
    );
    assert_eq!(
        client.mms_reply_context(conversation).unwrap().recipients,
        vec!["+15555550101"]
    );
}

#[test]
fn incoming_duplicate_participant_is_not_a_group() {
    let dir = tempfile::TempDir::new().unwrap();
    let vault = Vault::new();
    let client = unlocked(&config(&dir, "phone", &vault), &vault);
    let peer = "+15555550101";
    let conversation = complete_contextual(
        &client,
        "duplicate",
        Direction::Incoming,
        Some(peer),
        &[peer],
    );
    let context = client.mms_reply_context(conversation).unwrap();
    assert_eq!(context.recipients, vec![peer]);
    assert_eq!(context.blocked_reason, None);
}

#[test]
fn incoming_group_blocks_when_confirmed_own_format_matches_no_participant() {
    let dir = tempfile::TempDir::new().unwrap();
    let vault = Vault::new();
    let client = unlocked(&config(&dir, "phone", &vault), &vault);
    client.set_mms_own_address("sim-1", "5555550100").unwrap();
    let conversation = complete_contextual(
        &client,
        "format",
        Direction::Incoming,
        Some("+15555550101"),
        &["+15555550100", "+15555550102"],
    );
    let context = client.mms_reply_context(conversation).unwrap();
    assert!(context.recipients.is_empty());
    assert!(context.blocked_reason.is_some());
}
