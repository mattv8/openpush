mod common;

use common::*;

#[test]
fn gateway_settings_defaults_persist_and_gate_host_facts() {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::new();
    let config = config(&dir, "gateway-settings", &vault);
    let client = open(&config);

    assert_eq!(
        client.gateway_settings().unwrap(),
        GatewaySettings::default()
    );
    client
        .set_gateway_settings(GatewaySettings {
            mirroring_enabled: true,
            mirroring_wifi_only: true,
            skip_silent: true,
            sms_sync_enabled: false,
            mms_sync_enabled: true,
            media_wifi_only: true,
        })
        .unwrap();
    let decision = client
        .gateway_policy_decision(
            GatewayPlatform::Android,
            GatewayHostFacts {
                notification_listener_available: true,
                sms_available: true,
                notification_is_silent: true,
                ..GatewayHostFacts::default()
            },
        )
        .unwrap();
    assert!(
        !decision.capture_notification,
        "Wi-Fi and silent policies apply at effect time"
    );
    assert!(!decision.capture_sms);
    assert!(!decision.capture_mms);
    assert!(!decision.transfer_media);
    drop(client);

    let reopened = open(&config);
    assert!(reopened.gateway_settings().unwrap().mirroring_enabled);
    let ios = reopened
        .gateway_policy_decision(
            GatewayPlatform::Ios,
            GatewayHostFacts {
                notification_listener_available: true,
                sms_available: true,
                wifi_connected: true,
                ..GatewayHostFacts::default()
            },
        )
        .unwrap();
    assert!(!ios.capture_notification);
    assert!(!ios.capture_sms);
    assert!(!ios.rcs_supported);
}

#[test]
fn policy_is_not_a_destructive_sms_operation() {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::new();
    let config = config(&dir, "gateway-sms", &vault);
    let client = unlocked(&config, &vault);
    let captured = client
        .capture_incoming(incoming("kept", "provider-1"))
        .unwrap();
    let mut settings = client.gateway_settings().unwrap();
    settings.sms_sync_enabled = false;
    client.set_gateway_settings(settings).unwrap();
    assert_eq!(client.messages(captured.conversation_id).unwrap().len(), 1);
}

#[test]
fn version_17_upgrade_seeds_legacy_policy_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::new();
    let config = config(&dir, "gateway-upgrade", &vault);
    let client = open(&config);
    drop(client);

    // Simulate the final pre-settings schema without touching any SMS tables.
    let conn = rusqlite::Connection::open(&config.database_path).unwrap();
    conn.execute_batch(
        "PRAGMA key = \"x'0707070707070707070707070707070707070707070707070707070707070707'\";",
    )
    .unwrap();
    conn.execute_batch("DROP TABLE gateway_settings; UPDATE schema_meta SET version=17;")
        .unwrap();
    drop(conn);

    let upgraded = open(&config);
    let seeded = upgraded.gateway_settings().unwrap();
    // Mirroring must default to false on upgrade. Native host separately imports explicit legacy consent.
    assert!(!seeded.mirroring_enabled);
    assert!(!seeded.skip_silent);
    assert!(!seeded.media_wifi_only);

    // Verify explicit set persists on reopen
    upgraded
        .set_gateway_settings(GatewaySettings {
            mirroring_enabled: true,
            ..seeded
        })
        .unwrap();
    drop(upgraded);

    let reopened = open(&config);
    assert!(reopened.gateway_settings().unwrap().mirroring_enabled);
}
