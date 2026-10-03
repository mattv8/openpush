//! Durable, host-neutral gateway policy. Platform permission, network, and carrier
//! facts are supplied by native code; this module neither discovers nor persists them.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayPlatform {
    Android,
    Ios,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewaySettings {
    pub mirroring_enabled: bool,
    pub mirroring_wifi_only: bool,
    pub skip_silent: bool,
    pub sms_sync_enabled: bool,
    pub mms_sync_enabled: bool,
    pub media_wifi_only: bool,
}

impl Default for GatewaySettings {
    fn default() -> Self {
        Self {
            mirroring_enabled: false,
            mirroring_wifi_only: false,
            skip_silent: true,
            sms_sync_enabled: true,
            mms_sync_enabled: true,
            media_wifi_only: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GatewayHostFacts {
    pub wifi_connected: bool,
    pub notification_listener_available: bool,
    pub sms_available: bool,
    pub notification_is_silent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayCapabilities {
    pub notification_mirroring_supported: bool,
    pub sms_sync_supported: bool,
    pub mms_sync_supported: bool,
    pub rcs_supported: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayPolicyDecision {
    pub capture_notification: bool,
    pub capture_sms: bool,
    pub capture_mms: bool,
    pub transfer_media: bool,
    pub rcs_supported: bool,
}

impl Client {
    pub fn gateway_settings(&self) -> Result<GatewaySettings, Error> {
        let store = self.lock()?;
        load(&store.conn)
    }

    pub fn set_gateway_settings(&self, settings: GatewaySettings) -> Result<(), Error> {
        let mut store = self.lock()?;
        let tx = store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        save(&tx, &settings)?;
        tx.commit()?;
        Ok(())
    }

    pub fn gateway_capabilities(
        platform: GatewayPlatform,
        facts: GatewayHostFacts,
    ) -> GatewayCapabilities {
        let carrier_supported = platform == GatewayPlatform::Android && facts.sms_available;
        GatewayCapabilities {
            notification_mirroring_supported: platform == GatewayPlatform::Android
                && facts.notification_listener_available,
            sms_sync_supported: carrier_supported,
            mms_sync_supported: carrier_supported,
            // Neither host has a legitimate general RCS integration.
            rcs_supported: false,
        }
    }

    pub fn gateway_policy_decision(
        &self,
        platform: GatewayPlatform,
        facts: GatewayHostFacts,
    ) -> Result<GatewayPolicyDecision, Error> {
        let settings = self.gateway_settings()?;
        let capabilities = Self::gateway_capabilities(platform, facts);
        let wifi_permits = !settings.mirroring_wifi_only || facts.wifi_connected;
        let notification_allowed = settings.mirroring_enabled
            && capabilities.notification_mirroring_supported
            && wifi_permits
            && (!settings.skip_silent || !facts.notification_is_silent);
        Ok(GatewayPolicyDecision {
            capture_notification: notification_allowed,
            capture_sms: settings.sms_sync_enabled && capabilities.sms_sync_supported,
            capture_mms: settings.sms_sync_enabled
                && settings.mms_sync_enabled
                && capabilities.mms_sync_supported,
            // Media transfer is separately gated; acquisition and SMS text capture are not.
            transfer_media: !settings.media_wifi_only || facts.wifi_connected,
            rcs_supported: capabilities.rcs_supported,
        })
    }
}

pub(super) fn seed(conn: &Connection, legacy_enabled_installation: bool) -> Result<(), Error> {
    let mut defaults = GatewaySettings::default();
    if legacy_enabled_installation {
        // Native hosts import their own legacy master preference exactly once. Core has no safe
        // cross-platform knowledge of that preference, so it must retain the fresh default here.
        defaults.skip_silent = false;
        defaults.media_wifi_only = false;
    }
    conn.execute(
        "INSERT OR IGNORE INTO gateway_settings(id,mirroring_enabled,mirroring_wifi_only,skip_silent,sms_sync_enabled,mms_sync_enabled,media_wifi_only) VALUES(1,?,?,?,?,?,?)",
        params![
            defaults.mirroring_enabled,
            defaults.mirroring_wifi_only,
            defaults.skip_silent,
            defaults.sms_sync_enabled,
            defaults.mms_sync_enabled,
            defaults.media_wifi_only,
        ],
    )?;
    Ok(())
}

fn load(conn: &Connection) -> Result<GatewaySettings, Error> {
    conn.query_row(
        "SELECT mirroring_enabled,mirroring_wifi_only,skip_silent,sms_sync_enabled,mms_sync_enabled,media_wifi_only FROM gateway_settings WHERE id=1",
        [],
        |r| Ok(GatewaySettings {
            mirroring_enabled: r.get(0)?, mirroring_wifi_only: r.get(1)?, skip_silent: r.get(2)?,
            sms_sync_enabled: r.get(3)?, mms_sync_enabled: r.get(4)?, media_wifi_only: r.get(5)?,
        }),
    ).map_err(Into::into)
}

fn save(conn: &Connection, settings: &GatewaySettings) -> Result<(), Error> {
    conn.execute(
        "UPDATE gateway_settings SET mirroring_enabled=?,mirroring_wifi_only=?,skip_silent=?,sms_sync_enabled=?,mms_sync_enabled=?,media_wifi_only=? WHERE id=1",
        params![settings.mirroring_enabled, settings.mirroring_wifi_only, settings.skip_silent, settings.sms_sync_enabled, settings.mms_sync_enabled, settings.media_wifi_only],
    )?;
    Ok(())
}
