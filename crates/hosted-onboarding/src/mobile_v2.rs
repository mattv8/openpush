//! Mobile-only v2 reducer. Checkpoints are navigation hints, never authentication or billing proof.

use serde_json::{Map, Value, json};

const VERSION: u64 = 2;
pub const SCENARIOS: &[&str] = super::SCENARIOS;
pub const SCREENS: &[&str] = &[
    "welcome",
    "signin",
    "checking_account",
    "subscribe",
    "subscription_verifying",
    "purchase_pending",
    "passphrase",
    "confirm",
    "provisioning",
    "join",
    "approval",
    "unlock",
    "syncing",
    "permissions",
    "settings",
    "delete_account",
    "lapsed",
];
pub const EVENTS: &[&str] = &[
    "hosted_start",
    "signed_in",
    "session_restored",
    "account_new",
    "account_existing",
    "account_existing_active",
    "account_existing_grace",
    "account_existing_billing_retry",
    "account_incomplete",
    "account_incomplete_active",
    "account_incomplete_grace",
    "account_incomplete_billing_retry",
    "account_incomplete_provisioning",
    "account_incomplete_provisioning_grace",
    "account_incomplete_provisioning_billing_retry",
    "account_lapsed",
    "account_lapsed_revoked",
    "account_pending",
    "account_existing_pending",
    "account_verification_pending",
    "account_existing_verification_pending",
    "account_store_unavailable",
    "account_existing_store_unavailable",
    "account_incomplete_store_unavailable",
    "account_lookup_failed",
    "account_retry",
    "purchase_succeeded",
    "purchase_pending",
    "verify_entitlement",
    "entitlement_verified",
    "entitlement_rejected",
    "store_retry",
    "local_passphrase_accepted",
    "passphrase_confirmed",
    "provision_failed",
    "provision_retry",
    "provision_finished",
    "show_approval",
    "approval_granted",
    "approval_denied",
    "approval_expired",
    "passphrase_fallback",
    "unlock_succeeded",
    "sync_finished",
    "sync_failed",
    "sync_retry",
    "permissions_done",
    "permissions_skipped",
    "resubscribe",
    "renew",
    "delete_account",
    "deletion_confirmed",
    "back",
    "cancel",
    "signout",
    "reset",
];
const ACCOUNTS: &[&str] = &[
    "anonymous",
    "signed_in",
    "new_account",
    "incomplete",
    "existing",
];
const ENTITLEMENTS: &[&str] = &[
    "none",
    "store_pending",
    "store_succeeded_unverified",
    "verifying",
    "active",
    "grace",
    "billing_retry",
    "expired",
    "revoked",
    "unavailable",
];
const APPROVALS: &[&str] = &["not_requested", "awaiting", "approved", "denied", "expired"];
const STATUS_KEYS: &[&str] = &[
    "preview_error",
    "hosted_purchase_pending",
    "hosted_purchase_verifying",
    "hosted_subscribe_store_unavailable",
    "provisioning_error",
    "hosted_join_denied",
    "hosted_account_check_failed",
    "hosted_data_prepare_failed",
];
const RESUME_SCREENS: &[&str] = &[
    "subscribe",
    "purchase_pending",
    "subscription_verifying",
    "passphrase",
    "provisioning",
    "join",
    "unlock",
    "lapsed",
];

#[derive(Clone)]
struct Snapshot {
    scenario: String,
    screen: String,
    account_state: String,
    entitlement_state: String,
    provider: String,
    operation_id: Option<String>,
    status_key: Option<String>,
    approval_state: String,
    unlocked: bool,
    rejected: bool,
    resume_screen: Option<String>,
    resume_account_state: Option<String>,
}

impl Snapshot {
    fn output(&self) -> String {
        json!({"version": VERSION, "scenario": self.scenario, "screen": self.screen,
            "account_state": self.account_state, "entitlement_state": self.entitlement_state,
            "provider": self.provider, "operation_id": self.operation_id,
            "status_key": self.status_key, "approval_state": self.approval_state,
            "unlocked": self.unlocked, "rejected": self.rejected,
            "resume_screen": self.resume_screen,
            "resume_account_state": self.resume_account_state})
        .to_string()
    }
    fn rejected(mut self) -> String {
        self.rejected = true;
        self.status_key = Some("preview_error".into());
        self.output()
    }
    fn operation(&mut self) {
        if self.operation_id.is_none() {
            self.operation_id = Some(super::expected_operation(&self.scenario));
        }
    }
    fn paid(&self) -> bool {
        ["active", "grace", "billing_retry"].contains(&self.entitlement_state.as_str())
    }
}

fn state(scenario: &str) -> Snapshot {
    Snapshot {
        scenario: scenario.into(),
        screen: "welcome".into(),
        account_state: "anonymous".into(),
        entitlement_state: "none".into(),
        provider: "preview".into(),
        operation_id: None,
        status_key: None,
        approval_state: "not_requested".into(),
        unlocked: false,
        rejected: false,
        resume_screen: None,
        resume_account_state: None,
    }
}
fn invalid() -> String {
    state("new").rejected()
}

fn valid(s: &Snapshot) -> bool {
    if !SCENARIOS.contains(&s.scenario.as_str())
        || !SCREENS.contains(&s.screen.as_str())
        || !ACCOUNTS.contains(&s.account_state.as_str())
        || !ENTITLEMENTS.contains(&s.entitlement_state.as_str())
        || !APPROVALS.contains(&s.approval_state.as_str())
        || s.provider != "preview"
        || s.operation_id
            .as_deref()
            .is_some_and(|id| id != super::expected_operation(&s.scenario))
        || s.status_key
            .as_deref()
            .is_some_and(|key| !STATUS_KEYS.contains(&key))
        || s.resume_screen
            .as_deref()
            .is_some_and(|screen| !RESUME_SCREENS.contains(&screen))
        || s.resume_account_state
            .as_deref()
            .is_some_and(|account| !matches!(account, "new_account" | "incomplete" | "existing"))
    {
        return false;
    }
    if ["passphrase", "confirm", "provisioning"].contains(&s.screen.as_str())
        && (!(s.account_state == "new_account" || s.account_state == "incomplete") || !s.paid())
    {
        return false;
    }
    if s.screen == "provisioning" && s.operation_id.is_none() {
        return false;
    }
    if s.screen == "subscription_verifying"
        && (!matches!(s.account_state.as_str(), "new_account" | "existing")
            || !matches!(
                s.entitlement_state.as_str(),
                "store_succeeded_unverified" | "verifying"
            ))
    {
        return false;
    }
    if s.unlocked
        && !["syncing", "permissions", "settings", "delete_account"].contains(&s.screen.as_str())
    {
        return false;
    }
    if s.screen == "checking_account" && (s.account_state != "signed_in" || s.unlocked) {
        return false;
    }
    if s.screen == "signin" && s.unlocked {
        return false;
    }
    if s.screen == "subscribe"
        && (s.paid()
            || !matches!(s.account_state.as_str(), "new_account" | "existing")
            || !matches!(
                s.entitlement_state.as_str(),
                "none" | "unavailable" | "expired" | "revoked"
            ))
    {
        return false;
    }
    if ["join", "approval", "unlock"].contains(&s.screen.as_str())
        && !(s.account_state == "existing" && s.paid())
    {
        return false;
    }
    if s.screen == "syncing" && !(s.account_state == "existing" && s.paid() && s.unlocked) {
        return false;
    }
    if s.screen == "purchase_pending" && s.entitlement_state != "store_pending" {
        return false;
    }
    if s.screen == "lapsed"
        && !(s.account_state == "existing"
            && ["expired", "revoked"].contains(&s.entitlement_state.as_str()))
    {
        return false;
    }
    true
}

fn text(o: &Map<String, Value>, key: &str) -> Result<String, ()> {
    o.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(())
}
fn optional_text(o: &Map<String, Value>, key: &str) -> Result<Option<String>, ()> {
    match o.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(()),
    }
}
fn parse(snapshot: &str, allow_missing_resume_account_state: bool) -> Result<Snapshot, ()> {
    if snapshot.len() > super::MAX_SNAPSHOT_BYTES {
        return Err(());
    }
    let Value::Object(o) = serde_json::from_str(snapshot).map_err(|_| ())? else {
        return Err(());
    };
    let fields = [
        "version",
        "scenario",
        "screen",
        "account_state",
        "entitlement_state",
        "provider",
        "operation_id",
        "status_key",
        "approval_state",
        "unlocked",
        "rejected",
        "resume_screen",
        "resume_account_state",
    ];
    let has_legacy_fields = allow_missing_resume_account_state
        && o.len() + 1 == fields.len()
        && !o.contains_key("resume_account_state");
    if (!has_legacy_fields && o.len() != fields.len())
        || fields
            .iter()
            .filter(|key| **key != "resume_account_state" || !has_legacy_fields)
            .any(|key| !o.contains_key(*key))
        || o.keys().any(|key| !fields.contains(&key.as_str()))
        || o.get("version").and_then(Value::as_u64) != Some(VERSION)
    {
        return Err(());
    }
    let s = Snapshot {
        scenario: text(&o, "scenario")?,
        screen: text(&o, "screen")?,
        account_state: text(&o, "account_state")?,
        entitlement_state: text(&o, "entitlement_state")?,
        provider: text(&o, "provider")?,
        operation_id: optional_text(&o, "operation_id")?,
        status_key: optional_text(&o, "status_key")?,
        approval_state: text(&o, "approval_state")?,
        unlocked: o.get("unlocked").and_then(Value::as_bool).ok_or(())?,
        rejected: o.get("rejected").and_then(Value::as_bool).ok_or(())?,
        resume_screen: optional_text(&o, "resume_screen")?,
        resume_account_state: if has_legacy_fields {
            None
        } else {
            optional_text(&o, "resume_account_state")?
        },
    };
    valid(&s).then_some(s).ok_or(())
}

pub fn start(scenario: &str) -> String {
    if SCENARIOS.contains(&scenario) {
        state(scenario).output()
    } else {
        invalid()
    }
}

fn account_check(s: &mut Snapshot) {
    if matches!(
        s.account_state.as_str(),
        "new_account" | "incomplete" | "existing"
    ) {
        s.resume_account_state = Some(s.account_state.clone());
    }
    s.screen = "checking_account".into();
    s.account_state = "signed_in".into();
    s.unlocked = false;
}
fn clear_resolved_hint(s: &mut Snapshot) {
    s.resume_screen = None;
    s.resume_account_state = None;
}
fn existing_route(s: &mut Snapshot, entitlement: &str) {
    s.account_state = "existing".into();
    s.entitlement_state = entitlement.into();
    s.screen = "join".into();
    s.approval_state = "awaiting".into();
    clear_resolved_hint(s);
}
fn incomplete_route(s: &mut Snapshot, entitlement: &str, provisioning: bool) {
    s.account_state = "incomplete".into();
    s.entitlement_state = entitlement.into();
    s.screen = if provisioning {
        "provisioning"
    } else {
        "passphrase"
    }
    .into();
    if provisioning {
        s.operation();
    }
    clear_resolved_hint(s);
}
fn normalized_hint(screen: &str) -> Option<String> {
    match screen {
        "confirm" => Some("passphrase".into()),
        "approval" => Some("join".into()),
        "syncing" | "unlock" => Some("unlock".into()),
        screen if RESUME_SCREENS.contains(&screen) => Some(screen.into()),
        _ => None,
    }
}

pub fn advance(snapshot: &str, event: &str) -> String {
    let Ok(mut s) = parse(snapshot, false) else {
        return invalid();
    };
    s.rejected = false;
    s.status_key = None;
    match (s.screen.as_str(), event) {
        ("welcome", "hosted_start") => s.screen = "signin".into(),
        ("signin", "signed_in" | "session_restored") => account_check(&mut s),
        ("checking_account", "account_new") => {
            s.account_state = "new_account".into();
            s.entitlement_state = "none".into();
            s.screen = "subscribe".into();
            clear_resolved_hint(&mut s);
        }
        ("checking_account", "account_existing" | "account_existing_active") => {
            existing_route(&mut s, "active")
        }
        ("checking_account", "account_existing_grace") => existing_route(&mut s, "grace"),
        ("checking_account", "account_existing_billing_retry") => {
            existing_route(&mut s, "billing_retry")
        }
        ("checking_account", "account_incomplete" | "account_incomplete_active") => {
            incomplete_route(&mut s, "active", false)
        }
        ("checking_account", "account_incomplete_grace") => {
            incomplete_route(&mut s, "grace", false)
        }
        ("checking_account", "account_incomplete_billing_retry") => {
            incomplete_route(&mut s, "billing_retry", false)
        }
        ("checking_account", "account_incomplete_provisioning") => {
            incomplete_route(&mut s, "active", true)
        }
        ("checking_account", "account_incomplete_provisioning_grace") => {
            incomplete_route(&mut s, "grace", true)
        }
        ("checking_account", "account_incomplete_provisioning_billing_retry") => {
            incomplete_route(&mut s, "billing_retry", true)
        }
        ("checking_account", "account_lapsed") => {
            s.account_state = "existing".into();
            s.entitlement_state = "expired".into();
            s.screen = "lapsed".into();
            clear_resolved_hint(&mut s);
        }
        ("checking_account", "account_lapsed_revoked") => {
            s.account_state = "existing".into();
            s.entitlement_state = "revoked".into();
            s.screen = "lapsed".into();
            clear_resolved_hint(&mut s);
        }
        ("checking_account", "account_pending") => {
            s.account_state = "new_account".into();
            s.entitlement_state = "store_pending".into();
            s.screen = "purchase_pending".into();
            s.status_key = Some("hosted_purchase_pending".into());
            clear_resolved_hint(&mut s);
        }
        ("checking_account", "account_existing_pending") => {
            s.account_state = "existing".into();
            s.entitlement_state = "store_pending".into();
            s.screen = "purchase_pending".into();
            s.status_key = Some("hosted_purchase_pending".into());
            clear_resolved_hint(&mut s);
        }
        ("checking_account", "account_verification_pending") => {
            s.account_state = "new_account".into();
            s.entitlement_state = "store_succeeded_unverified".into();
            s.screen = "subscription_verifying".into();
            s.status_key = Some("hosted_purchase_verifying".into());
            clear_resolved_hint(&mut s);
        }
        ("checking_account", "account_existing_verification_pending") => {
            s.account_state = "existing".into();
            s.entitlement_state = "store_succeeded_unverified".into();
            s.screen = "subscription_verifying".into();
            s.status_key = Some("hosted_purchase_verifying".into());
            clear_resolved_hint(&mut s);
        }
        ("checking_account", "account_store_unavailable") => {
            s.account_state = "new_account".into();
            s.entitlement_state = "unavailable".into();
            s.screen = "subscribe".into();
            s.status_key = Some("hosted_subscribe_store_unavailable".into());
            clear_resolved_hint(&mut s);
        }
        ("checking_account", "account_existing_store_unavailable") => {
            s.account_state = "existing".into();
            s.entitlement_state = "unavailable".into();
            s.screen = "subscribe".into();
            s.status_key = Some("hosted_subscribe_store_unavailable".into());
            clear_resolved_hint(&mut s);
        }
        ("checking_account", "account_incomplete_store_unavailable") => {
            s.account_state = "signed_in".into();
            s.entitlement_state = "unavailable".into();
            s.screen = "checking_account".into();
            s.status_key = Some("hosted_subscribe_store_unavailable".into());
            s.resume_account_state = Some("incomplete".into());
        }
        ("checking_account", "account_lookup_failed") => {
            s.status_key = Some("hosted_account_check_failed".into())
        }
        ("checking_account", "account_retry") => account_check(&mut s),
        ("purchase_pending", "account_retry") => {
            s.resume_screen = Some("purchase_pending".into());
            account_check(&mut s);
        }
        ("subscribe", "purchase_pending") if !s.paid() && s.entitlement_state != "unavailable" => {
            s.entitlement_state = "store_pending".into();
            s.screen = "purchase_pending".into();
            s.status_key = Some("hosted_purchase_pending".into());
        }
        ("subscribe" | "purchase_pending", "purchase_succeeded")
            if !s.paid() && s.entitlement_state != "unavailable" =>
        {
            s.entitlement_state = "store_succeeded_unverified".into();
            s.screen = "subscription_verifying".into();
            s.status_key = Some("hosted_purchase_verifying".into());
        }
        ("subscription_verifying", "verify_entitlement")
            if s.entitlement_state == "store_succeeded_unverified" =>
        {
            s.entitlement_state = "verifying".into()
        }
        ("subscription_verifying", "entitlement_verified")
            if s.entitlement_state == "verifying" =>
        {
            s.entitlement_state = "active".into();
            if s.account_state == "existing" {
                existing_route(&mut s, "active");
            } else {
                s.account_state = "new_account".into();
                s.screen = "passphrase".into();
            }
        }
        ("subscription_verifying", "entitlement_rejected") => {
            s.entitlement_state = "none".into();
            s.screen = "subscribe".into();
            s.status_key = Some("preview_error".into());
        }
        ("subscribe", "store_retry") => account_check(&mut s),
        ("passphrase", "local_passphrase_accepted") => s.screen = "confirm".into(),
        ("confirm", "passphrase_confirmed") => {
            s.screen = "provisioning".into();
            s.operation();
        }
        ("provisioning", "provision_failed") => s.status_key = Some("provisioning_error".into()),
        ("provisioning", "provision_retry") => {}
        ("provisioning", "provision_finished") => {
            s.account_state = "existing".into();
            s.screen = "permissions".into();
        }
        ("join", "show_approval") => s.screen = "approval".into(),
        ("join" | "approval", "approval_granted") => {
            s.approval_state = "approved".into();
            s.screen = "unlock".into();
        }
        ("join" | "approval", "approval_denied" | "approval_expired") => {
            s.screen = "join".into();
            s.approval_state = if event == "approval_expired" {
                "expired"
            } else {
                "denied"
            }
            .into();
            s.status_key = Some("hosted_join_denied".into());
        }
        ("join", "passphrase_fallback") => s.screen = "unlock".into(),
        ("unlock", "unlock_succeeded") => {
            s.unlocked = true;
            s.screen = "syncing".into();
        }
        ("syncing", "sync_finished") => s.screen = "permissions".into(),
        ("syncing", "sync_failed") => s.status_key = Some("hosted_data_prepare_failed".into()),
        ("syncing", "sync_retry") => {}
        ("permissions", "permissions_done" | "permissions_skipped") => s.screen = "settings".into(),
        ("lapsed", "resubscribe" | "renew") => s.screen = "subscribe".into(),
        ("settings", "delete_account") => s.screen = "delete_account".into(),
        ("delete_account", "deletion_confirmed") => s = state("new"),
        (_, "signout") => s = state(&s.scenario),
        (_, "reset") => s = state(&s.scenario),
        (_, "back" | "cancel") => match s.screen.as_str() {
            "signin" | "subscribe" | "purchase_pending" | "subscription_verifying" | "lapsed" => {
                s.screen = "welcome".into()
            }
            "confirm" => s.screen = "passphrase".into(),
            "passphrase" | "provisioning" => s.screen = "welcome".into(),
            "approval" | "unlock" => s.screen = "join".into(),
            "delete_account" => s.screen = "settings".into(),
            "join" | "syncing" | "permissions" | "settings" => {
                s.screen = "welcome".into();
                s.unlocked = false;
            }
            _ => return s.rejected(),
        },
        _ => return s.rejected(),
    }
    if valid(&s) { s.output() } else { invalid() }
}

fn migrated_v1(snapshot: &super::Snapshot) -> Snapshot {
    let mut s = state(&snapshot.scenario);
    s.operation_id = snapshot.operation_id.clone();
    s.account_state = match snapshot.account_state.as_str() {
        "existing" | "signed_out_existing" => "existing",
        "new_account" | "signed_out_new" => "new_account",
        "signed_in" => "signed_in",
        _ => "anonymous",
    }
    .into();
    if matches!(
        s.account_state.as_str(),
        "new_account" | "incomplete" | "existing"
    ) {
        s.resume_account_state = Some(s.account_state.clone());
    }
    s.entitlement_state = match snapshot.entitlement_state.as_str() {
        "none"
        | "store_pending"
        | "store_succeeded_unverified"
        | "verifying"
        | "active"
        | "grace"
        | "billing_retry"
        | "expired"
        | "revoked" => snapshot.entitlement_state.clone(),
        _ => "none".into(),
    };
    s.resume_screen = normalized_hint(&snapshot.screen);
    if snapshot.account_state != "anonymous" {
        s.screen = "signin".into();
    }
    s
}

/// Validates v2 directly or strictly parses a v1 checkpoint before migration.
pub fn resume(snapshot: &str) -> String {
    let mut s = if let Ok(v2) = parse(snapshot, true) {
        v2
    } else if let Ok(v1) = super::parse_snapshot(snapshot) {
        migrated_v1(&v1)
    } else {
        return invalid();
    };
    s.rejected = false;
    s.unlocked = false;
    s.status_key = None;
    if s.screen != "welcome" {
        if s.resume_account_state.is_none()
            && matches!(
                s.account_state.as_str(),
                "new_account" | "incomplete" | "existing"
            )
        {
            s.resume_account_state = Some(s.account_state.clone());
        }
        if s.resume_screen.is_none() {
            s.resume_screen = normalized_hint(&s.screen);
        }
        s.screen = "signin".into();
    }
    if valid(&s) { s.output() } else { invalid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn step(s: String, event: &str) -> String {
        advance(&s, event)
    }
    fn field(s: &str, field: &str) -> Value {
        serde_json::from_str::<Value>(s).unwrap()[field].clone()
    }

    #[test]
    fn confirmed_new_account_must_be_paid_before_passphrase_setup() {
        let s = step(
            step(step(start("new"), "hosted_start"), "signed_in"),
            "account_new",
        );
        assert_eq!(field(&s, "screen"), "subscribe");
        assert_eq!(
            field(&step(s, "local_passphrase_accepted"), "rejected"),
            true
        );
    }
    #[test]
    fn returning_unlock_prepares_and_retries_without_new_setup() {
        let s = step(
            step(step(start("returning"), "hosted_start"), "signed_in"),
            "account_existing",
        );
        let s = step(step(s, "approval_granted"), "unlock_succeeded");
        assert_eq!(field(&s, "screen"), "syncing");
        let failed = step(s, "sync_failed");
        assert_eq!(field(&failed, "status_key"), "hosted_data_prepare_failed");
        assert_eq!(field(&step(failed, "sync_retry"), "screen"), "syncing");
    }
    #[test]
    fn pending_requires_another_explicit_recheck_epoch() {
        let s = step(
            step(step(start("pending"), "hosted_start"), "signed_in"),
            "account_pending",
        );
        let s = step(s, "account_retry");
        assert_eq!(field(&s, "screen"), "checking_account");
        let s = step(s, "account_pending");
        assert_eq!(field(&s, "screen"), "purchase_pending");
        assert_eq!(field(&step(s, "account_pending"), "rejected"), true);
    }
    #[test]
    fn resume_migrates_v1_but_never_uses_its_unlocked_or_paid_claim() {
        let v1 = super::super::advance(
            &super::super::advance(&super::super::start("returning"), "hosted_start"),
            "signed_in",
        );
        let migrated = resume(&v1);
        assert_eq!(field(&migrated, "version"), 2);
        assert_eq!(field(&migrated, "screen"), "signin");
        assert_eq!(field(&migrated, "account_state"), "existing");
        assert_eq!(field(&migrated, "unlocked"), false);
        assert_eq!(
            field(&advance(&migrated, "account_existing"), "rejected"),
            true
        );
    }
    #[test]
    fn invalid_and_wrong_version_input_is_recoverable_welcome() {
        for input in ["{}", "x", &"x".repeat(super::super::MAX_SNAPSHOT_BYTES + 1)] {
            let output = resume(input);
            assert_eq!(field(&output, "screen"), "welcome");
            assert_eq!(field(&output, "rejected"), true);
        }
    }
    #[test]
    fn vocabulary_is_closed_and_emitted_states_validate() {
        let vocabulary: Value =
            serde_json::from_str(include_str!("../mobile_v2_vocabulary.json")).unwrap();
        let values = |name: &str| {
            vocabulary[name]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(SCENARIOS, values("scenarios"));
        assert_eq!(SCREENS, values("screens"));
        assert_eq!(EVENTS, values("events"));
        for scenario in SCENARIOS {
            for event in EVENTS {
                assert!(
                    parse(&advance(&start(scenario), event), false).is_ok(),
                    "{scenario}/{event}"
                );
            }
        }
    }

    fn checked(event: &str) -> String {
        step(step(start("new"), "hosted_start"), event)
    }

    #[test]
    fn provider_facts_preserve_existing_identity_and_entitlement_kind() {
        for (event, entitlement) in [
            ("account_existing_active", "active"),
            ("account_existing_grace", "grace"),
            ("account_existing_billing_retry", "billing_retry"),
        ] {
            let s = step(checked("signed_in"), event);
            assert_eq!(field(&s, "screen"), "join");
            assert_eq!(field(&s, "account_state"), "existing");
            assert_eq!(field(&s, "entitlement_state"), entitlement);
        }
        let pending = step(checked("signed_in"), "account_pending");
        assert_eq!(field(&pending, "account_state"), "new_account");
        assert_eq!(field(&pending, "screen"), "purchase_pending");
        let unavailable = step(checked("signed_in"), "account_existing_store_unavailable");
        assert_eq!(field(&unavailable, "account_state"), "existing");
        assert_eq!(field(&unavailable, "entitlement_state"), "unavailable");
        assert_eq!(field(&unavailable, "screen"), "subscribe");
    }

    #[test]
    fn renewal_and_paid_setup_back_never_offer_or_accept_another_purchase() {
        let lapsed = step(checked("signed_in"), "account_lapsed");
        let renewing = step(lapsed, "renew");
        assert_eq!(field(&renewing, "account_state"), "existing");
        let verified = step(
            step(step(renewing, "purchase_succeeded"), "verify_entitlement"),
            "entitlement_verified",
        );
        assert_eq!(field(&verified, "screen"), "join");

        let paid = step(
            step(
                step(
                    step(checked("signed_in"), "account_incomplete_active"),
                    "local_passphrase_accepted",
                ),
                "back",
            ),
            "back",
        );
        assert_eq!(field(&paid, "screen"), "welcome");
        assert_eq!(field(&step(paid, "purchase_succeeded"), "rejected"), true);
    }

    #[test]
    fn unavailable_store_retries_lookup_without_purchase() {
        let unavailable = step(checked("signed_in"), "account_store_unavailable");
        assert_eq!(
            field(&step(unavailable.clone(), "purchase_succeeded"), "rejected"),
            true
        );
        let retry = step(unavailable, "store_retry");
        assert_eq!(field(&retry, "screen"), "checking_account");
        assert_eq!(field(&retry, "account_state"), "signed_in");
    }

    #[test]
    fn resume_hint_is_normalized_retained_until_lookup_and_then_cleared() {
        let v1 = super::super::advance(
            &super::super::advance(&super::super::start("provision_retry"), "hosted_start"),
            "signed_in",
        );
        let first = resume(&v1);
        assert_eq!(field(&first, "resume_screen"), "provisioning");
        assert_eq!(field(&first, "resume_account_state"), "new_account");
        let second = resume(&first);
        assert_eq!(field(&second, "resume_screen"), "provisioning");
        let checking = step(second, "session_restored");
        assert_eq!(field(&checking, "resume_screen"), "provisioning");
        let resolved = step(checking, "account_existing_active");
        assert_eq!(field(&resolved, "resume_screen"), Value::Null);
    }

    #[test]
    fn migration_keeps_v1_signed_in_unclassified_and_provider_controls_provisioning() {
        let v1 = super::super::advance(&super::super::start("new"), "hosted_start");
        let v1 = super::super::advance(&v1, "signed_in");
        let migrated = resume(&v1);
        assert_eq!(field(&migrated, "account_state"), "signed_in");
        let checking = step(migrated, "signed_in");
        let passphrase = step(checking.clone(), "account_incomplete_active");
        assert_eq!(field(&passphrase, "screen"), "passphrase");
        let provisioning = step(checking, "account_incomplete_provisioning");
        assert_eq!(field(&provisioning, "screen"), "provisioning");
        assert_eq!(
            field(&provisioning, "operation_id"),
            "preview-provision-new"
        );
    }

    #[test]
    fn crafted_invalid_combinations_and_illegal_events_are_rejected() {
        let mut value: Value = serde_json::from_str(&start("new")).unwrap();
        value["screen"] = Value::String("checking_account".into());
        assert_eq!(
            field(&advance(&value.to_string(), "account_new"), "rejected"),
            true
        );
        let mut value: Value = serde_json::from_str(&start("new")).unwrap();
        value["screen"] = Value::String("subscribe".into());
        value["account_state"] = Value::String("new_account".into());
        value["entitlement_state"] = Value::String("active".into());
        assert_eq!(
            field(
                &advance(&value.to_string(), "purchase_succeeded"),
                "rejected"
            ),
            true
        );
        let mut value: Value = serde_json::from_str(&start("new")).unwrap();
        value["resume_screen"] = Value::String("settings".into());
        assert_eq!(field(&resume(&value.to_string()), "rejected"), true);
        let mut value: Value = serde_json::from_str(&start("new")).unwrap();
        value["resume_account_state"] = Value::String("signed_in".into());
        assert_eq!(field(&resume(&value.to_string()), "rejected"), true);
        let mut value: Value = serde_json::from_str(&start("new")).unwrap();
        value["unexpected"] = Value::Bool(true);
        assert_eq!(field(&resume(&value.to_string()), "rejected"), true);
        assert_eq!(
            field(
                &advance(&start("new"), "account_existing_active"),
                "rejected"
            ),
            true
        );
    }

    #[test]
    fn existing_pending_survives_renewal_recheck_and_verifies_to_join() {
        let lapsed = step(checked("signed_in"), "account_lapsed");
        let pending = step(step(lapsed, "renew"), "purchase_pending");
        let checking = step(pending, "account_retry");
        let pending = step(checking, "account_existing_pending");
        assert_eq!(field(&pending, "account_state"), "existing");
        let verified = step(
            step(step(pending, "purchase_succeeded"), "verify_entitlement"),
            "entitlement_verified",
        );
        assert_eq!(field(&verified, "screen"), "join");
        let direct = step(checked("signed_in"), "account_existing_pending");
        assert_eq!(field(&direct, "account_state"), "existing");
        assert_eq!(field(&direct, "screen"), "purchase_pending");
    }

    #[test]
    fn unlocked_delete_account_is_valid_and_confirmation_resets_safely() {
        let existing = step(checked("signed_in"), "account_existing_active");
        let synced = step(
            step(step(existing, "approval_granted"), "unlock_succeeded"),
            "sync_finished",
        );
        let deleting = step(step(synced, "permissions_done"), "delete_account");
        assert_eq!(field(&deleting, "screen"), "delete_account");
        let reset = step(deleting, "deletion_confirmed");
        assert_eq!(field(&reset, "screen"), "welcome");
        assert_eq!(field(&reset, "account_state"), "anonymous");
    }

    #[test]
    fn incomplete_unavailable_waits_in_checking_for_retry_or_signout() {
        let unavailable = step(checked("signed_in"), "account_incomplete_store_unavailable");
        assert_eq!(field(&unavailable, "screen"), "checking_account");
        assert_eq!(field(&unavailable, "account_state"), "signed_in");
        assert_eq!(
            field(&unavailable, "status_key"),
            "hosted_subscribe_store_unavailable"
        );
        assert_eq!(
            field(&step(unavailable.clone(), "account_retry"), "screen"),
            "checking_account"
        );
        assert_eq!(field(&step(unavailable, "signout"), "screen"), "welcome");
    }

    #[test]
    fn unverified_receipt_recovery_rechecks_before_routing_without_purchase() {
        for (fact, destination) in [
            ("account_verification_pending", "passphrase"),
            ("account_existing_verification_pending", "join"),
        ] {
            let interrupted = step(checked("signed_in"), fact);
            assert_eq!(field(&interrupted, "screen"), "subscription_verifying");
            let resumed = resume(&interrupted);
            assert_eq!(field(&resumed, "screen"), "signin");
            assert_eq!(field(&resumed, "resume_screen"), "subscription_verifying");
            assert_eq!(
                field(&resumed, "resume_account_state"),
                if fact == "account_verification_pending" {
                    "new_account"
                } else {
                    "existing"
                }
            );
            let checking = step(resumed, "session_restored");
            let recovered = step(checking, fact);
            let verifying = step(recovered, "verify_entitlement");
            let finished = step(verifying, "entitlement_verified");
            assert_eq!(field(&finished, "screen"), destination);
            assert_eq!(field(&finished, "rejected"), false);
        }
    }

    #[test]
    fn classified_hint_survives_cold_check_errors_and_retries_without_routing() {
        let pending = step(checked("signed_in"), "account_existing_pending");
        let checking = step(pending, "account_retry");
        assert_eq!(field(&checking, "account_state"), "signed_in");
        assert_eq!(field(&checking, "resume_account_state"), "existing");
        let resumed = resume(&checking);
        let checking = step(resumed, "session_restored");
        let failed = step(checking, "account_lookup_failed");
        assert_eq!(field(&failed, "resume_account_state"), "existing");
        let resumed = resume(&failed);
        let checking = step(resumed, "session_restored");
        let resolved = step(checking, "account_existing_pending");
        assert_eq!(field(&resolved, "resume_account_state"), Value::Null);
        assert_eq!(field(&resolved, "screen"), "purchase_pending");

        let incomplete = step(checked("signed_in"), "account_incomplete_active");
        let resumed = resume(&incomplete);
        assert_eq!(field(&resumed, "resume_account_state"), "incomplete");
        let failed = step(step(resumed, "session_restored"), "account_lookup_failed");
        assert_eq!(field(&failed, "account_state"), "signed_in");
        assert_eq!(field(&failed, "resume_account_state"), "incomplete");

        let unavailable = step(checked("signed_in"), "account_incomplete_store_unavailable");
        assert_eq!(field(&unavailable, "account_state"), "signed_in");
        assert_eq!(field(&unavailable, "entitlement_state"), "unavailable");
        assert_eq!(field(&unavailable, "resume_account_state"), "incomplete");
        let resumed = resume(&unavailable);
        assert_eq!(field(&resumed, "screen"), "signin");
        assert_eq!(field(&resumed, "resume_account_state"), "incomplete");
        let checking = step(resumed, "session_restored");
        assert_eq!(field(&checking, "account_state"), "signed_in");
        assert_eq!(field(&checking, "resume_account_state"), "incomplete");
    }

    #[test]
    fn resume_explicitly_upgrades_prior_v2_without_the_classification_hint() {
        let mut prior: Value = serde_json::from_str(&start("new")).unwrap();
        prior
            .as_object_mut()
            .unwrap()
            .remove("resume_account_state");
        assert_eq!(
            field(&advance(&prior.to_string(), "hosted_start"), "rejected"),
            true
        );
        let upgraded = resume(&prior.to_string());
        assert_eq!(field(&upgraded, "rejected"), false);
        assert_eq!(field(&upgraded, "resume_account_state"), Value::Null);
    }
}
