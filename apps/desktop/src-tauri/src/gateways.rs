//! Gateway/SIM discovery from the authenticated `/v1/devices` and `/v1/capabilities` routes.
//!
//! Capability report contract (posted by a gateway to `POST /v1/capabilities`):
//! `{"simulator": bool, "capabilities": {"sims": [{"subscription_id": "<opaque>",
//!   "label": "SIM 1", "sms": "available", "mms": "unsupported"}]}}` where statuses use the
//! domain `CapabilityStatus` names. Anything malformed is shown as "not reported" and cannot be
//! selected for sending; nothing is assumed.
use crate::dto::GatewayView;
use serde::Deserialize;

const MAX_GATEWAYS: usize = 16;
const MAX_SIMS: usize = 8;
const MAX_SUBSCRIPTION_CHARS: usize = 128;
const MAX_LABEL_CHARS: usize = 48;
const FALLBACK_MMS_MAX_BYTES: u64 = 300 * 1024;
const MAX_MMS_BYTES: u64 = 10 * 1024 * 1024;
const MAX_MMS_RECIPIENTS: usize = 20;

#[derive(Deserialize)]
pub struct DevicesResponse {
    pub devices: Vec<DeviceRow>,
}
#[derive(Deserialize)]
pub struct DeviceRow {
    pub device_id: String,
    pub role: String,
    pub revoked: bool,
}
#[derive(Deserialize)]
pub struct CapabilitiesResponse {
    pub capabilities: Vec<CapabilityRow>,
}
#[derive(Deserialize)]
pub struct CapabilityRow {
    pub device_id: String,
    pub simulator: bool,
    pub capabilities: serde_json::Value,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Available,
    PermissionRequired,
    ApprovalRequired,
    RegionRestricted,
    Experimental,
    Unsupported,
    #[serde(other)]
    Unknown,
}
impl Status {
    fn describe(self) -> &'static str {
        match self {
            Status::Available => "available",
            Status::PermissionRequired => "needs permission on the phone",
            Status::ApprovalRequired => "awaiting approval",
            Status::RegionRestricted => "region restricted",
            Status::Experimental => "disabled",
            Status::Unsupported => "unsupported",
            Status::Unknown => "unknown",
        }
    }
}

#[derive(Deserialize)]
struct SimReport {
    subscription_id: String,
    #[serde(default)]
    label: Option<String>,
    sms: Status,
    mms: Status,
    #[serde(default)]
    mms_content_version: Option<serde_json::Value>,
    #[serde(default)]
    mms_max_bytes: Option<serde_json::Value>,
    #[serde(default)]
    mms_limit_source: Option<serde_json::Value>,
    #[serde(default)]
    mms_max_recipients: Option<serde_json::Value>,
}

fn bounded_u64(value: Option<&serde_json::Value>, min: u64, max: u64) -> Option<u64> {
    value?
        .as_u64()
        .filter(|value| (*value >= min) && (*value <= max))
}

fn mms_limits(sim: &SimReport) -> (Option<u32>, Option<u64>, Option<String>, Option<usize>) {
    let version =
        bounded_u64(sim.mms_content_version.as_ref(), 1, u32::MAX as u64).map(|value| value as u32);
    let max_bytes = bounded_u64(sim.mms_max_bytes.as_ref(), 1, MAX_MMS_BYTES);
    let source = match (
        max_bytes,
        sim.mms_limit_source
            .as_ref()
            .and_then(|value| value.as_str()),
    ) {
        (Some(_), Some("carrier")) => Some("carrier".into()),
        (Some(_), Some("fallback")) => Some("fallback".into()),
        (Some(_), _) => Some("fallback".into()),
        (None, _) => Some("fallback".into()),
    };
    let max_bytes = max_bytes.or(Some(FALLBACK_MMS_MAX_BYTES));
    let recipients = bounded_u64(
        sim.mms_max_recipients.as_ref(),
        1,
        MAX_MMS_RECIPIENTS as u64,
    )
    .map(|value| value as usize)
    .or(Some(MAX_MMS_RECIPIENTS));
    (version, max_bytes, source, recipients)
}

fn valid_optional_mms_fields(sim: &SimReport) -> bool {
    sim.mms_limit_source
        .as_ref()
        .is_none_or(|value| matches!(value.as_str(), Some("carrier" | "fallback")))
        && sim
            .mms_content_version
            .as_ref()
            .is_none_or(|value| bounded_u64(Some(value), 1, u32::MAX as u64).is_some())
        && sim
            .mms_max_bytes
            .as_ref()
            .is_none_or(|value| bounded_u64(Some(value), 1, MAX_MMS_BYTES).is_some())
        && sim
            .mms_max_recipients
            .as_ref()
            .is_none_or(|value| bounded_u64(Some(value), 1, MAX_MMS_RECIPIENTS as u64).is_some())
}

fn clean_label(value: &str) -> Option<String> {
    let label: String = value
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL_CHARS)
        .collect();
    let label = label.trim().to_owned();
    (!label.is_empty()).then_some(label)
}

fn valid_subscription(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= MAX_SUBSCRIPTION_CHARS
        && value.chars().all(|c| c.is_ascii_graphic())
}

fn sims(value: &serde_json::Value) -> Option<Vec<SimReport>> {
    let list = value.get("sims")?.as_array()?;
    if list.is_empty() || list.len() > MAX_SIMS {
        return None;
    }
    let sims: Vec<SimReport> = list
        .iter()
        .map(|item| serde_json::from_value(item.clone()).ok())
        .collect::<Option<_>>()?;
    sims.iter()
        .all(|sim| valid_subscription(&sim.subscription_id))
        .then_some(sims)
}

pub fn gateway_views(
    devices: &DevicesResponse,
    capabilities: &CapabilitiesResponse,
) -> Vec<GatewayView> {
    let mut views = Vec::new();
    for device in devices
        .devices
        .iter()
        .filter(|d| d.role == "gateway" && !d.revoked)
        .take(MAX_GATEWAYS)
    {
        let Ok(id) = uuid::Uuid::parse_str(&device.device_id) else {
            continue;
        };
        let short = id.simple().to_string()[..8].to_owned();
        let report = capabilities
            .capabilities
            .iter()
            .find(|row| uuid::Uuid::parse_str(&row.device_id).is_ok_and(|other| other == id));
        let simulated = report.is_some_and(|row| row.simulator);
        let presence = "Presence is not reported by the server; commands wait durably until the gateway syncs.";
        match report.and_then(|row| sims(&row.capabilities)) {
            Some(sims) => {
                for sim in sims {
                    let limits = mms_limits(&sim);
                    let name = clean_label(sim.label.as_deref().unwrap_or("")).unwrap_or_else(|| format!("Gateway {short}"));
                    let mut note = format!("SMS {}; MMS {}. {presence}", sim.sms.describe(), sim.mms.describe());
                    if simulated {
                        note = format!("SIMULATED gateway: carrier delivery is simulated. {note}");
                    }
                    views.push(GatewayView {
                        id: id.to_string(),
                        name: if simulated { format!("{name} (simulated)") } else { name },
                        sim_id: sim.subscription_id.clone(),
                        online: false,
                        simulated,
                        supports_sms: sim.sms == Status::Available,
                        // Capability v2 is required for an MMS route. A legacy report may still
                        // advertise MMS, but is deliberately not safe enough to send it.
                        supports_mms: sim.sms == Status::Available
                            && sim.mms == Status::Available
                            && valid_optional_mms_fields(&sim)
                            && limits.0.is_some_and(|version| version >= 2),
                        capability_note: Some(note),
                        mms_content_version: limits.0,
                        mms_max_bytes: limits.1,
                        mms_limit_source: limits.2,
                        mms_max_recipients: limits.3,
                    });
                }
            }
            None => views.push(GatewayView {
                id: id.to_string(),
                name: format!("Gateway {short}"),
                sim_id: String::new(),
                online: false,
                simulated,
                supports_sms: false,
                supports_mms: false,
                capability_note: Some("This gateway has not reported valid SIM capabilities; sending through it is disabled.".into()),
                mms_content_version: None,
                mms_max_bytes: None,
                mms_limit_source: None,
                mms_max_recipients: None,
            }),
        }
    }
    views
}

/// The selectable route matching an exact gateway and SIM, if it can send SMS.
pub fn find_route<'a>(
    gateways: &'a [GatewayView],
    gateway_id: &str,
    sim_id: &str,
) -> Option<&'a GatewayView> {
    gateways
        .iter()
        .find(|view| view.id == gateway_id && view.sim_id == sim_id && !view.sim_id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_sims_become_routes_and_unreported_gateways_cannot_send() {
        let gateway = uuid::Uuid::new_v4().to_string();
        let silent = uuid::Uuid::new_v4().to_string();
        let revoked = uuid::Uuid::new_v4().to_string();
        let desktop = uuid::Uuid::new_v4().to_string();
        let devices: DevicesResponse = serde_json::from_value(serde_json::json!({"devices":[
            {"device_id":gateway,"role":"gateway","revoked":false},
            {"device_id":silent,"role":"gateway","revoked":false},
            {"device_id":revoked,"role":"gateway","revoked":true},
            {"device_id":desktop,"role":"device","revoked":false}]}))
        .unwrap();
        let capabilities: CapabilitiesResponse = serde_json::from_value(serde_json::json!({"capabilities":[
            {"device_id":gateway,"simulator":true,"capabilities":{"sims":[
                {"subscription_id":"sim-1","label":"Work\u{0007} SIM","sms":"available","mms":"available","mms_content_version":2,"mms_max_bytes":307200,"mms_limit_source":"carrier","mms_max_recipients":20},
                {"subscription_id":"sim-2","sms":"permission_required","mms":"unsupported"}]}},
            {"device_id":revoked,"simulator":false,"capabilities":{"sims":[{"subscription_id":"x","sms":"available","mms":"available"}]}}]})).unwrap();
        let views = gateway_views(&devices, &capabilities);
        assert_eq!(views.len(), 3);
        assert_eq!(views[0].name, "Work SIM (simulated)");
        assert!(views[0].supports_sms && views[0].supports_mms && views[0].simulated);
        assert!(views[0]
            .capability_note
            .as_deref()
            .unwrap()
            .starts_with("SIMULATED"));
        assert!(!views[1].supports_sms && !views[1].supports_mms);
        assert!(views[2].sim_id.is_empty() && !views[2].supports_sms);
        assert!(find_route(&views, &gateway, "sim-1").is_some());
        assert!(find_route(&views, &gateway, "sim-9").is_none());
        assert!(find_route(&views, &silent, "").is_none());
        assert!(find_route(&views, &revoked, "x").is_none());
    }

    #[test]
    fn malformed_or_oversized_reports_are_not_trusted() {
        let gateway = uuid::Uuid::new_v4().to_string();
        let devices: DevicesResponse = serde_json::from_value(
            serde_json::json!({"devices":[{"device_id":gateway,"role":"gateway","revoked":false}]}),
        )
        .unwrap();
        for report in [
            serde_json::json!({"sims":[{"subscription_id":"has space","sms":"available","mms":"available"}]}),
            serde_json::json!({"sims":[{"subscription_id":"a","sms":true,"mms":"available"}]}),
            serde_json::json!({"sims":(0..9).map(|i| serde_json::json!({"subscription_id":format!("s{i}"),"sms":"available","mms":"available"})).collect::<Vec<_>>()}),
            serde_json::json!({"sims":[]}),
            serde_json::json!("available"),
        ] {
            let capabilities: CapabilitiesResponse = serde_json::from_value(serde_json::json!({"capabilities":[{"device_id":gateway,"simulator":false,"capabilities":report}]})).unwrap();
            let views = gateway_views(&devices, &capabilities);
            assert_eq!(views.len(), 1);
            assert!(!views[0].supports_sms);
        }
    }

    #[test]
    fn mms_v2_is_required_without_disabling_sms_for_optional_field_errors() {
        let gateway = uuid::Uuid::new_v4().to_string();
        let devices: DevicesResponse = serde_json::from_value(
            serde_json::json!({"devices":[{"device_id":gateway,"role":"gateway","revoked":false}]}),
        )
        .unwrap();
        for extra in [
            serde_json::json!({}),
            serde_json::json!({"mms_content_version":1}),
            serde_json::json!({"mms_content_version":"2"}),
            serde_json::json!({"mms_content_version":2,"mms_limit_source":5}),
            serde_json::json!({"mms_content_version":2,"mms_max_bytes":0}),
            serde_json::json!({"mms_content_version":2,"mms_max_recipients":0}),
            serde_json::json!({"mms_content_version":2,"mms_max_recipients":21}),
        ] {
            let mut sim =
                serde_json::json!({"subscription_id":"sim-1","sms":"available","mms":"available"});
            sim.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let capabilities: CapabilitiesResponse = serde_json::from_value(serde_json::json!({"capabilities":[{"device_id":gateway,"simulator":false,"capabilities":{"sims":[sim]}}]})).unwrap();
            let view = gateway_views(&devices, &capabilities).remove(0);
            assert!(view.supports_sms);
            assert!(!view.supports_mms);
        }
    }
}
