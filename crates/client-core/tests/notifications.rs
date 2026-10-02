//! SQLCipher-backed notification lifecycle regression coverage.
mod common;
use common::*;
use tempfile::TempDir;

fn capture(key: &str, instance: &str, text: &str) -> NotificationCapture {
    NotificationCapture {
        notification_key: key.into(),
        instance: instance.into(),
        package_name: "com.example.mail".into(),
        app_name: "Mail".into(),
        title: "Subject".into(),
        text: text.into(),
        category: None,
        posted_at: 42,
        dismissible: true,
    }
}

#[test]
fn capture_update_remove_repost_and_duplicate_are_durable() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let phone_cfg = config(&dir, "phone", &vault);
    let desktop_cfg = config(&dir, "desktop", &vault);
    let phone = unlocked(&phone_cfg, &vault);
    let desktop = unlocked(&desktop_cfg, &vault);
    let mut server = Server::default();
    assert_eq!(
        phone
            .capture_notification(capture("n", "one", "first"))
            .unwrap(),
        NotificationCaptureOutcome::Captured
    );
    assert_eq!(
        phone
            .capture_notification(capture("n", "one", "first"))
            .unwrap(),
        NotificationCaptureOutcome::Duplicate
    );
    assert_eq!(
        phone
            .capture_notification(capture("n", "one", "updated"))
            .unwrap(),
        NotificationCaptureOutcome::Captured
    );
    server.upload(&phone);
    server.sync(&desktop);
    let mut snapshot = desktop.notification_snapshot().unwrap();
    let first = snapshot.notifications.remove(0);
    assert_eq!(first.text, "updated");
    phone.remove_notification("n", "one").unwrap();
    server.upload(&phone);
    server.sync(&desktop);
    assert!(
        desktop
            .notification_snapshot()
            .unwrap()
            .notifications
            .is_empty()
    );
    assert_eq!(
        phone
            .capture_notification(capture("n", "two", "repost"))
            .unwrap(),
        NotificationCaptureOutcome::Captured
    );
    server.upload(&phone);
    server.sync(&desktop);
    let mut snapshot = desktop.notification_snapshot().unwrap();
    let repost = snapshot.notifications.remove(0);
    assert_ne!(first.target.lifetime, repost.target.lifetime);
    assert_eq!(repost.text, "repost");
}

#[test]
fn filters_purge_only_notification_banners_and_locked_capture_has_no_queue() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let cfg = config(&dir, "phone", &vault);
    let phone = unlocked(&cfg, &vault);
    phone
        .capture_incoming(incoming("carrier", "sms-1"))
        .unwrap();
    phone
        .capture_notification(capture("n", "one", "body"))
        .unwrap();
    // Phone-local captures never generate a desktop banner candidate.
    assert!(phone.pending_banner_candidates(10).unwrap().is_empty());
    phone
        .set_app_muted(&cfg.device_id.to_string(), "com.example.mail", "Mail", true)
        .unwrap();
    let candidates = phone.pending_banner_candidates(10).unwrap();
    assert!(candidates.is_empty());
    let locked_cfg = config(&dir, "locked", &vault);
    let locked = open(&locked_cfg);
    assert_eq!(
        locked
            .capture_notification(capture("locked", "one", "never persisted"))
            .unwrap(),
        NotificationCaptureOutcome::DroppedLocked
    );
    assert!(locked.pending_outbox().unwrap().is_empty());
    assert!(
        locked
            .notification_snapshot()
            .unwrap()
            .notifications
            .is_empty()
    );
}

#[test]
fn snapshot_notification_history_has_no_banner_or_phone_effect() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let phone_cfg = config(&dir, "phone", &vault);
    let restore_cfg = config(&dir, "restore", &vault);
    let phone = unlocked(&phone_cfg, &vault);
    let restore = unlocked(&restore_cfg, &vault);
    let mut server = Server::default();
    phone
        .capture_notification(capture("n", "one", "history"))
        .unwrap();
    server.upload(&phone);
    let progress = restore
        .begin_snapshot(Cursor(1), 1, SnapshotPurpose::Resync)
        .unwrap();
    restore
        .append_snapshot_page(
            progress.generation,
            &[SnapshotRecord {
                cursor: Cursor(1),
                envelope: server.log[0].clone(),
            }],
        )
        .unwrap();
    restore.finish_snapshot(progress.generation).unwrap();
    restore.apply_pending(10).unwrap();
    assert_eq!(
        restore.notification_snapshot().unwrap().notifications.len(),
        1
    );
    assert!(restore.pending_banner_candidates(10).unwrap().is_empty());
    assert!(
        restore
            .pending_notification_dismissals(10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn concurrent_filters_converge_and_observed_clock_advances() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let phone_cfg = config(&dir, "phone-filter", &vault);
    let desktop_cfg = config(&dir, "desktop-filter", &vault);
    let phone = unlocked(&phone_cfg, &vault);
    let desktop = unlocked(&desktop_cfg, &vault);
    let source = phone_cfg.device_id.to_string();

    phone
        .set_app_muted(&source, "com.example.mail", "Mail", true)
        .unwrap();
    desktop
        .set_app_muted(&source, "com.example.mail", "Mail", false)
        .unwrap();

    let mut server = Server::default();
    server.upload(&phone);
    server.upload(&desktop);
    server.sync(&phone);
    server.sync(&desktop);
    let phone_filter = phone.notification_snapshot().unwrap().app_filters.remove(0);
    let desktop_filter = desktop
        .notification_snapshot()
        .unwrap()
        .app_filters
        .remove(0);
    assert_eq!(phone_filter, desktop_filter);

    // A change made after observing the other writer must dominate both revision-1 writes.
    phone
        .set_app_muted(
            &source,
            "com.example.mail",
            "Mail renamed after sync",
            !phone_filter.muted,
        )
        .unwrap();
    server.upload(&phone);
    server.sync(&desktop);
    assert_eq!(
        phone.notification_snapshot().unwrap().app_filters,
        desktop.notification_snapshot().unwrap().app_filters
    );
}

#[test]
fn muting_purges_only_unacknowledged_notification_post_envelopes() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let phone_cfg = config(&dir, "phone-purge", &vault);
    let desktop_cfg = config(&dir, "desktop-purge", &vault);
    let phone = unlocked(&phone_cfg, &vault);
    let desktop = unlocked(&desktop_cfg, &vault);

    phone
        .capture_incoming(incoming("carrier", "sms-purge"))
        .unwrap();
    phone
        .capture_notification(capture("purged", "one", "secret"))
        .unwrap();
    phone
        .set_app_muted(
            &phone_cfg.device_id.to_string(),
            "com.example.mail",
            "Mail",
            true,
        )
        .unwrap();

    // The sealed notification-post is gone, while the SMS and filter event remain uploadable.
    assert_eq!(phone.pending_outbox().unwrap().len(), 2);
    let mut server = Server::default();
    server.upload(&phone);
    server.sync(&desktop);
    assert!(
        desktop
            .notification_snapshot()
            .unwrap()
            .notifications
            .is_empty()
    );
    assert_eq!(desktop.list_conversations().unwrap().len(), 1);
}

#[test]
fn dismissal_effect_is_lifetime_bound_gated_and_completion_is_not_removal() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let phone_cfg = config(&dir, "phone-dismiss", &vault);
    let desktop_cfg = config(&dir, "desktop-dismiss", &vault);
    let phone = unlocked(&phone_cfg, &vault);
    let desktop = unlocked(&desktop_cfg, &vault);
    let mut server = Server::default();

    phone
        .capture_notification(capture("dismiss", "one", "first"))
        .unwrap();
    server.upload(&phone);
    server.sync(&desktop);
    let first = desktop
        .notification_snapshot()
        .unwrap()
        .notifications
        .remove(0);
    desktop.dismiss_notification(first.target.clone()).unwrap();
    assert!(desktop.notification_snapshot().unwrap().notifications[0].dismissal_pending);
    server.upload(&desktop);
    server.sync(&phone);

    let effects = phone.pending_notification_dismissals(10).unwrap();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].target, first.target);
    assert_eq!(effects[0].instance, "one");
    phone
        .complete_notification_dismissal(&effects[0].id)
        .unwrap();
    assert!(
        phone
            .pending_notification_dismissals(10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        phone.notification_snapshot().unwrap().notifications.len(),
        1
    );

    phone.remove_notification("dismiss", "one").unwrap();
    server.upload(&phone);
    server.sync(&desktop);
    assert!(
        desktop
            .notification_snapshot()
            .unwrap()
            .notifications
            .is_empty()
    );

    phone
        .capture_notification(capture("dismiss", "two", "repost"))
        .unwrap();
    server.upload(&phone);
    server.sync(&desktop);
    let repost = desktop
        .notification_snapshot()
        .unwrap()
        .notifications
        .remove(0);
    assert_ne!(first.target.lifetime, repost.target.lifetime);
    assert!(
        phone
            .pending_notification_dismissals(10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn non_dismissible_notification_refuses_dismissal() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let phone_cfg = config(&dir, "phone-fixed", &vault);
    let desktop_cfg = config(&dir, "desktop-fixed", &vault);
    let phone = unlocked(&phone_cfg, &vault);
    let desktop = unlocked(&desktop_cfg, &vault);
    let mut item = capture("fixed", "one", "cannot clear");
    item.dismissible = false;
    phone.capture_notification(item).unwrap();
    let mut server = Server::default();
    server.upload(&phone);
    server.sync(&desktop);
    let target = desktop
        .notification_snapshot()
        .unwrap()
        .notifications
        .remove(0)
        .target;
    assert!(matches!(
        desktop.dismiss_notification(target),
        Err(Error::InvalidRequest(_))
    ));
    assert!(desktop.pending_outbox().unwrap().is_empty());
}

#[test]
fn remote_banners_are_first_lifetime_only_with_full_content_and_sms_read_gating() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let phone_cfg = config(&dir, "phone-banner", &vault);
    let desktop_cfg = config(&dir, "desktop-banner", &vault);
    let phone = unlocked(&phone_cfg, &vault);
    let desktop = unlocked(&desktop_cfg, &vault);
    let mut server = Server::default();

    phone
        .capture_notification(capture("banner", "one", "full body"))
        .unwrap();
    assert!(phone.pending_banner_candidates(10).unwrap().is_empty());
    server.upload(&phone);
    server.sync(&phone); // own echo remains non-presentational
    server.sync(&desktop);
    let first = desktop.pending_banner_candidates(10).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].title, "Subject");
    assert_eq!(first[0].body, "full body");
    let stable_id = first[0].id.clone();

    phone
        .capture_notification(capture("banner", "one", "updated body"))
        .unwrap();
    server.upload(&phone);
    server.sync(&desktop);
    let after_update = desktop.pending_banner_candidates(10).unwrap();
    assert_eq!(after_update.len(), 1);
    assert_eq!(after_update[0].id, stable_id);
    assert_eq!(after_update[0].body, "full body");

    phone
        .capture_incoming(incoming("sms body", "sms-banner"))
        .unwrap();
    server.upload(&phone);
    server.sync(&desktop);
    let conversation = desktop
        .list_conversations()
        .unwrap()
        .remove(0)
        .conversation_id;
    let message = desktop.messages(conversation).unwrap().remove(0);
    desktop
        .mark_seen(message.payload.record.message_id)
        .unwrap();
    assert!(
        desktop
            .pending_banner_candidates(10)
            .unwrap()
            .iter()
            .all(|candidate| candidate.kind != "message")
    );
}

#[test]
fn dismissal_effects_stay_blocked_after_reopen_until_unlock_and_after_rotation() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let pc = config(&dir, "effect-phone", &vault);
    let dc = config(&dir, "effect-desktop", &vault);
    let phone = unlocked(&pc, &vault);
    let desktop = unlocked(&dc, &vault);
    let mut server = Server::default();
    phone
        .capture_notification(capture("effect", "1", "body"))
        .unwrap();
    server.upload(&phone);
    server.sync(&desktop);
    desktop
        .dismiss_notification(
            desktop.notification_snapshot().unwrap().notifications[0]
                .target
                .clone(),
        )
        .unwrap();
    server.upload(&desktop);
    server.sync(&phone);
    assert_eq!(phone.pending_notification_dismissals(10).unwrap().len(), 1);
    drop(phone);
    let phone = open(&pc);
    assert!(
        phone
            .pending_notification_dismissals(10)
            .unwrap()
            .is_empty()
    );
    phone
        .unlock(&vault.profile, &vault.header, PASSPHRASE)
        .unwrap();
    assert_eq!(phone.pending_notification_dismissals(10).unwrap().len(), 1);
    let next = Vault::epoch(vault.id, 2);
    phone
        .unlock(&next.profile, &next.header, PASSPHRASE)
        .unwrap();
    phone.activate_epoch(2).unwrap();
    assert!(
        phone
            .pending_notification_dismissals(10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn dismissal_imported_in_phone_snapshot_cannot_execute() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let pc = config(&dir, "history-phone", &vault);
    let dc = config(&dir, "history-desktop", &vault);
    let phone = unlocked(&pc, &vault);
    let desktop = unlocked(&dc, &vault);
    let mut server = Server::default();
    phone
        .capture_notification(capture("history", "1", "body"))
        .unwrap();
    server.upload(&phone);
    server.sync(&desktop);
    desktop
        .dismiss_notification(
            desktop.notification_snapshot().unwrap().notifications[0]
                .target
                .clone(),
        )
        .unwrap();
    server.upload(&desktop);
    // Restore the same phone identity into another empty database: history is presentation only.
    let mut restore_cfg = pc.clone();
    restore_cfg.database_path = dir.path().join("restored.db");
    let restored = unlocked(&restore_cfg, &vault);
    let progress = restored
        .begin_snapshot(Cursor(2), 2, SnapshotPurpose::Resync)
        .unwrap();
    let records: Vec<_> = server
        .log
        .iter()
        .enumerate()
        .map(|(i, e)| SnapshotRecord {
            cursor: Cursor(i as u64 + 1),
            envelope: e.clone(),
        })
        .collect();
    restored
        .append_snapshot_page(progress.generation, &records)
        .unwrap();
    restored.finish_snapshot(progress.generation).unwrap();
    restored.apply_pending(100).unwrap();
    assert!(restored.notification_snapshot().unwrap().notifications[0].dismissal_pending);
    assert!(
        restored
            .pending_notification_dismissals(100)
            .unwrap()
            .is_empty()
    );
    assert!(restored.pending_banner_candidates(100).unwrap().is_empty());
}

#[test]
fn coalesced_os_update_removal_retires_the_captured_lifetime() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let pc = config(&dir, "coalesced", &vault);
    let phone = unlocked(&pc, &vault);
    phone
        .capture_notification(capture("key", "A", "first"))
        .unwrap();
    let old = phone.notification_snapshot().unwrap().notifications[0]
        .target
        .clone();
    // Host confirmed the OS key is absent; update B was removed before its coalescer flushed.
    phone.remove_notification("key", "B").unwrap();
    assert!(
        phone
            .notification_snapshot()
            .unwrap()
            .notifications
            .is_empty()
    );
    phone
        .capture_notification(capture("key", "C", "repost"))
        .unwrap();
    assert_ne!(
        old.lifetime,
        phone.notification_snapshot().unwrap().notifications[0]
            .target
            .lifetime
    );
}

#[test]
fn os_updates_keep_pending_dismissal_executable_without_duplicate_history() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let pc = config(&dir, "updated-phone", &vault);
    let dc = config(&dir, "updated-desktop", &vault);
    let phone = unlocked(&pc, &vault);
    let desktop = unlocked(&dc, &vault);
    let mut server = Server::default();
    phone
        .capture_notification(capture("key", "A", "first"))
        .unwrap();
    server.upload(&phone);
    server.sync(&desktop);
    desktop
        .dismiss_notification(
            desktop.notification_snapshot().unwrap().notifications[0]
                .target
                .clone(),
        )
        .unwrap();
    server.upload(&desktop);
    server.sync(&phone);
    assert_eq!(
        phone
            .capture_notification(capture("key", "B", "first"))
            .unwrap(),
        NotificationCaptureOutcome::Duplicate
    );
    assert!(phone.pending_outbox().unwrap().is_empty());
    assert_eq!(
        phone.pending_notification_dismissals(10).unwrap()[0].instance,
        "B"
    );
    phone
        .capture_notification(capture("key", "C", "new text"))
        .unwrap();
    assert_eq!(
        phone.pending_notification_dismissals(10).unwrap()[0].instance,
        "C"
    );
}

#[test]
fn mute_racing_upload_ack_is_harmless_and_unmute_can_recapture_purged_content() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let pc = config(&dir, "race", &vault);
    let phone = unlocked(&pc, &vault);
    phone
        .capture_notification(capture("key", "A", "first"))
        .unwrap();
    let in_flight = phone.pending_outbox().unwrap()[0].envelope_id;
    phone
        .set_app_muted(&pc.device_id.to_string(), "com.example.mail", "Mail", true)
        .unwrap();
    phone.ack_outbox(in_flight).unwrap();
    phone
        .set_app_muted(&pc.device_id.to_string(), "com.example.mail", "Mail", false)
        .unwrap();
    assert_eq!(
        phone
            .capture_notification(capture("key", "A", "first"))
            .unwrap(),
        NotificationCaptureOutcome::Captured
    );
}

#[test]
fn remove_then_immediate_repost_does_not_rebind_old_pending_dismissal() {
    let dir = TempDir::new().unwrap();
    let vault = Vault::new();
    let pc = config(&dir, "repost-pending", &vault);
    let phone = unlocked(&pc, &vault);
    phone
        .capture_notification(capture("key", "A", "first"))
        .unwrap();
    let target = phone.notification_snapshot().unwrap().notifications[0]
        .target
        .clone();
    phone.dismiss_notification(target.clone()).unwrap();
    assert_eq!(phone.pending_notification_dismissals(10).unwrap().len(), 1);
    phone.remove_notification("key", "A").unwrap();
    phone
        .capture_notification(capture("key", "B", "repost"))
        .unwrap();
    assert_ne!(
        target.lifetime,
        phone.notification_snapshot().unwrap().notifications[0]
            .target
            .lifetime
    );
    assert!(
        phone
            .pending_notification_dismissals(10)
            .unwrap()
            .is_empty()
    );
}
