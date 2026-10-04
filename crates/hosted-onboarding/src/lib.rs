//! Debug-only hosted onboarding preview policy with no network or credential effects.

pub mod mobile_v2;

use serde_json::{Map, Value, json};

const VERSION: u64 = 1;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024;
pub const SCENARIOS: &[&str] = &[
    "new",
    "returning",
    "lapsed",
    "pending",
    "store_unavailable",
    "provision_retry",
    "approval_denied",
];
pub const SCREENS: &[&str] = &[
    "welcome",
    "signin",
    "subscribe",
    "subscription_verifying",
    "purchase_pending",
    "passphrase",
    "confirm",
    "provisioning",
    "join",
    "approval",
    "unlock",
    "lapsed",
    "permissions",
    "settings",
    "delete_account",
];
const ACCOUNTS: &[&str] = &[
    "anonymous",
    "signed_in",
    "new_account",
    "existing",
    "signed_out_new",
    "signed_out_existing",
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
];
const APPROVALS: &[&str] = &["not_requested", "awaiting", "approved", "denied", "expired"];
const STATUS_KEYS: &[&str] = &[
    "preview_error",
    "hosted_purchase_pending",
    "hosted_purchase_verifying",
    "hosted_subscribe_store_unavailable",
    "provisioning_error",
    "hosted_join_denied",
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
}

impl Snapshot {
    fn output(&self) -> String {
        json!({"version": VERSION, "scenario": self.scenario, "screen": self.screen, "account_state": self.account_state, "entitlement_state": self.entitlement_state, "provider": self.provider, "operation_id": self.operation_id, "status_key": self.status_key, "approval_state": self.approval_state, "unlocked": self.unlocked, "rejected": self.rejected}).to_string()
    }
    fn rejected(mut self) -> String {
        self.rejected = true;
        self.status_key = Some("preview_error".into());
        self.output()
    }
    fn operation(&mut self) {
        if self.operation_id.is_none() {
            self.operation_id = Some(format!("preview-provision-{}", self.scenario));
        }
    }
}

fn state(
    scenario: &str,
    screen: &str,
    account: &str,
    entitlement: &str,
    approval: &str,
    status: Option<&str>,
) -> Snapshot {
    Snapshot {
        scenario: scenario.into(),
        screen: screen.into(),
        account_state: account.into(),
        entitlement_state: entitlement.into(),
        provider: "preview".into(),
        operation_id: None,
        status_key: status.map(String::from),
        approval_state: approval.into(),
        unlocked: false,
        rejected: false,
    }
}
fn start_snapshot(scenario: &str) -> Snapshot {
    state(
        if SCENARIOS.contains(&scenario) {
            scenario
        } else {
            "new"
        },
        "welcome",
        "anonymous",
        "none",
        "not_requested",
        None,
    )
}
fn expected_operation(scenario: &str) -> String {
    format!("preview-provision-{scenario}")
}

fn valid_invariant(s: &Snapshot) -> bool {
    if s.unlocked && !["permissions", "settings", "delete_account"].contains(&s.screen.as_str()) {
        return false;
    }
    if s.operation_id
        .as_deref()
        .is_some_and(|id| id != expected_operation(&s.scenario))
    {
        return false;
    }
    if s.screen == "purchase_pending" && s.entitlement_state != "store_pending" {
        return false;
    }
    if s.screen == "provisioning" && (s.entitlement_state != "active" || s.operation_id.is_none()) {
        return false;
    }
    if ["passphrase", "confirm"].contains(&s.screen.as_str())
        && !(s.account_state == "new_account" && s.entitlement_state == "active")
    {
        return false;
    }
    if ["join", "approval", "unlock"].contains(&s.screen.as_str())
        && !(s.account_state == "existing"
            && ["active", "grace", "billing_retry"].contains(&s.entitlement_state.as_str()))
    {
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
fn parse_snapshot(input: &str) -> Result<Snapshot, ()> {
    if input.len() > MAX_SNAPSHOT_BYTES {
        return Err(());
    }
    let Value::Object(o) = serde_json::from_str(input).map_err(|_| ())? else {
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
    ];
    if o.len() != fields.len()
        || fields.iter().any(|key| !o.contains_key(*key))
        || o.keys().any(|key| !fields.contains(&key.as_str()))
        || o.get("version").and_then(Value::as_u64) != Some(VERSION)
    {
        return Err(());
    }
    fn text(o: &Map<String, Value>, key: &str) -> Result<String, ()> {
        o.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(())
    }
    let scenario = text(&o, "scenario")?;
    let screen = text(&o, "screen")?;
    let account_state = text(&o, "account_state")?;
    let entitlement_state = text(&o, "entitlement_state")?;
    let approval_state = text(&o, "approval_state")?;
    if !SCENARIOS.contains(&scenario.as_str())
        || !SCREENS.contains(&screen.as_str())
        || !ACCOUNTS.contains(&account_state.as_str())
        || !ENTITLEMENTS.contains(&entitlement_state.as_str())
        || !APPROVALS.contains(&approval_state.as_str())
        || text(&o, "provider")? != "preview"
    {
        return Err(());
    }
    let operation_id = match o.get("operation_id") {
        Some(Value::Null) => None,
        Some(Value::String(id)) if id == &expected_operation(&scenario) => Some(id.clone()),
        _ => return Err(()),
    };
    let status_key = match o.get("status_key") {
        Some(Value::Null) => None,
        Some(Value::String(key)) if STATUS_KEYS.contains(&key.as_str()) => Some(key.clone()),
        _ => return Err(()),
    };
    let snapshot = Snapshot {
        scenario,
        screen,
        account_state,
        entitlement_state,
        provider: "preview".into(),
        operation_id,
        status_key,
        approval_state,
        unlocked: o.get("unlocked").and_then(Value::as_bool).ok_or(())?,
        rejected: o.get("rejected").and_then(Value::as_bool).ok_or(())?,
    };
    valid_invariant(&snapshot).then_some(snapshot).ok_or(())
}
fn invalid() -> String {
    start_snapshot("new").rejected()
}
fn route_after_signin(s: &mut Snapshot) {
    if matches!(s.account_state.as_str(), "existing" | "signed_out_existing") {
        s.account_state = "existing".into();
        if ["expired", "revoked"].contains(&s.entitlement_state.as_str()) {
            s.screen = "lapsed".into();
        } else {
            s.screen = "join".into();
            s.approval_state = "awaiting".into();
        }
        return;
    }
    if matches!(s.account_state.as_str(), "new_account" | "signed_out_new")
        && ["active", "grace", "billing_retry"].contains(&s.entitlement_state.as_str())
    {
        s.account_state = "new_account".into();
        resume_paid_setup(s);
        return;
    }
    match s.scenario.as_str() {
        "returning" => *s = state("returning", "join", "existing", "active", "awaiting", None),
        "lapsed" => {
            *s = state(
                "lapsed",
                "lapsed",
                "existing",
                "expired",
                "not_requested",
                None,
            )
        }
        "pending" => {
            *s = state(
                "pending",
                "purchase_pending",
                "signed_in",
                "store_pending",
                "not_requested",
                Some("hosted_purchase_pending"),
            )
        }
        "store_unavailable" => {
            *s = state(
                "store_unavailable",
                "subscribe",
                "signed_in",
                "none",
                "not_requested",
                Some("hosted_subscribe_store_unavailable"),
            )
        }
        "provision_retry" => {
            *s = state(
                "provision_retry",
                "provisioning",
                "new_account",
                "active",
                "not_requested",
                Some("provisioning_error"),
            );
            s.operation();
        }
        "approval_denied" => {
            *s = state(
                "approval_denied",
                "join",
                "existing",
                "active",
                "denied",
                Some("hosted_join_denied"),
            )
        }
        _ => {
            s.account_state = "signed_in".into();
            s.screen = "subscribe".into();
        }
    }
}
fn resume_paid_setup(s: &mut Snapshot) {
    if s.account_state == "existing" {
        s.screen = "join".into();
        s.approval_state = "awaiting".into();
    } else if s.operation_id.is_some() {
        s.screen = "provisioning".into();
        s.status_key = Some("provisioning_error".into());
    } else {
        s.screen = "passphrase".into();
    }
}
fn verified_route(s: &mut Snapshot) {
    s.entitlement_state = "active".into();
    s.status_key = None;
    if matches!(s.account_state.as_str(), "existing" | "signed_out_existing") {
        s.account_state = "existing".into();
        s.screen = "join".into();
        s.approval_state = "awaiting".into();
    } else {
        s.account_state = "new_account".into();
        s.screen = "passphrase".into();
    }
}

pub fn start(scenario: &str) -> String {
    start_snapshot(scenario).output()
}
pub fn advance(snapshot: &str, event: &str) -> String {
    let Ok(mut s) = parse_snapshot(snapshot) else {
        return invalid();
    };
    if event == "entitlement_verified" && s.entitlement_state != "verifying" {
        return s.rejected();
    }
    s.rejected = false;
    s.status_key = None;
    match (s.screen.as_str(), event) {
        ("welcome", "hosted_start") => s.screen = "signin".into(),
        ("signin", "signed_in") => route_after_signin(&mut s),
        ("subscribe", "purchase_pending") => {
            s.screen = "purchase_pending".into();
            s.entitlement_state = "store_pending".into();
            s.status_key = Some("hosted_purchase_pending".into());
        }
        ("subscribe", "restore_succeeded")
            if ["active", "grace", "billing_retry"].contains(&s.entitlement_state.as_str())
                && matches!(s.account_state.as_str(), "new_account" | "existing") =>
        {
            resume_paid_setup(&mut s);
        }
        ("subscribe", "purchase_succeeded" | "restore_succeeded")
        | ("purchase_pending", "purchase_succeeded" | "restore_succeeded")
            if !["active", "grace", "billing_retry"].contains(&s.entitlement_state.as_str()) =>
        {
            s.screen = "subscription_verifying".into();
            s.entitlement_state = "store_succeeded_unverified".into();
            s.status_key = Some("hosted_purchase_verifying".into());
        }
        ("subscription_verifying", "verify_entitlement")
            if s.entitlement_state == "store_succeeded_unverified" =>
        {
            s.entitlement_state = "verifying".into();
        }
        ("subscription_verifying", "entitlement_verified") => verified_route(&mut s),
        ("subscription_verifying", "entitlement_rejected") => {
            s.screen = "subscribe".into();
            s.entitlement_state = "none".into();
            s.status_key = Some("preview_error".into());
        }
        ("subscribe", "store_unavailable") => {
            s.status_key = Some("hosted_subscribe_store_unavailable".into())
        }
        ("subscribe", "store_retry") => {}
        ("passphrase", "local_passphrase_accepted") => s.screen = "confirm".into(),
        ("confirm", "passphrase_confirmed") => {
            s.screen = "provisioning".into();
            s.operation();
        }
        ("provisioning", "provision_failed") => s.status_key = Some("provisioning_error".into()),
        ("provisioning", "provision_retry") => s.status_key = None,
        ("provisioning", "provision_finished") => {
            s.screen = "permissions".into();
            s.account_state = "existing".into();
            s.unlocked = true;
        }
        ("join", "show_approval") => s.screen = "approval".into(),
        ("join", "approval_granted") | ("approval", "approval_granted") => {
            s.screen = "unlock".into();
            s.approval_state = "approved".into();
        }
        ("join", "approval_denied" | "approval_expired")
        | ("approval", "approval_denied" | "approval_expired") => {
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
        ("unlock", "passphrase_confirmed") => {
            s.screen = "permissions".into();
            s.unlocked = true;
        }
        ("permissions", "permissions_done" | "permissions_skipped") => s.screen = "settings".into(),
        ("lapsed", "resubscribe") => s.screen = "subscribe".into(),
        ("settings", "resubscribe")
            if ["expired", "revoked"].contains(&s.entitlement_state.as_str()) =>
        {
            s.unlocked = false;
            s.screen = "subscribe".into();
        }
        (_, "entitlement_grace")
            if s.account_state == "existing" && s.entitlement_state == "active" =>
        {
            s.entitlement_state = "grace".into();
        }
        (_, "entitlement_billing_retry")
            if s.account_state == "existing"
                && ["active", "grace"].contains(&s.entitlement_state.as_str()) =>
        {
            s.entitlement_state = "billing_retry".into();
        }
        (_, "entitlement_expired")
            if s.account_state == "existing"
                && ["active", "grace", "billing_retry"].contains(&s.entitlement_state.as_str()) =>
        {
            let retain_local_access = s.screen == "settings" && s.unlocked;
            s.entitlement_state = "expired".into();
            if !retain_local_access {
                s.screen = "lapsed".into();
                s.unlocked = false;
            }
        }
        (_, "entitlement_revoked")
            if s.account_state == "existing"
                && ["active", "grace", "billing_retry"].contains(&s.entitlement_state.as_str()) =>
        {
            let retain_local_access = s.screen == "settings" && s.unlocked;
            s.entitlement_state = "revoked".into();
            if !retain_local_access {
                s.screen = "lapsed".into();
                s.unlocked = false;
            }
        }
        ("settings", "delete_account") => s.screen = "delete_account".into(),
        ("delete_account", "deletion_confirmed") => s = start_snapshot("new"),
        (_, "back" | "cancel") => match s.screen.as_str() {
            "signin" => s.screen = "welcome".into(),
            "confirm" => s.screen = "passphrase".into(),
            "passphrase" | "provisioning" => s.screen = "subscribe".into(),
            "approval" => s.screen = "join".into(),
            "unlock" => s.screen = "join".into(),
            "subscribe" | "purchase_pending" | "subscription_verifying" | "lapsed" => {
                s.screen = "welcome".into()
            }
            "delete_account" => s.screen = "settings".into(),
            "join" | "permissions" | "settings" => {
                s.screen = "welcome".into();
                s.unlocked = false;
            }
            _ => return s.rejected(),
        },
        (_, "signout") => {
            s.account_state = if s.account_state == "existing" {
                "signed_out_existing"
            } else {
                "signed_out_new"
            }
            .into();
            s.screen = "welcome".into();
            s.unlocked = false;
        }
        (_, "reset") => s = start_snapshot(&s.scenario),
        _ => return s.rejected(),
    }
    if valid_invariant(&s) {
        s.output()
    } else {
        invalid()
    }
}
pub fn resume(snapshot: &str) -> String {
    let Ok(mut s) = parse_snapshot(snapshot) else {
        return invalid();
    };
    s.rejected = false;
    match s.screen.as_str() {
        "confirm" => s.screen = "passphrase".into(),
        "approval" => s.screen = "join".into(),
        "provisioning" => s.status_key = Some("provisioning_error".into()),
        "purchase_pending" => s.status_key = Some("hosted_purchase_pending".into()),
        "unlock" => s.status_key = None,
        _ => {}
    };
    s.output()
}

fn word_at(index: usize) -> &'static str {
    include_str!("../assets/eff_large_wordlist.txt")
        .lines()
        .nth(index)
        .expect("maintained EFF word list")
        .split_once('\t')
        .expect("EFF word list format")
        .1
}
fn random_word_index() -> usize {
    let mut index = 0;
    for _ in 0..5 {
        let digit = loop {
            let mut byte = [0u8; 1];
            libsodium_rs::random::fill_bytes(&mut byte);
            if byte[0] < 252 {
                break (byte[0] % 6) as usize;
            }
        };
        index = index * 6 + digit;
    }
    index
}
/// Resamples six EFF large-list words until they pass the conservative local UI gate.
pub fn generate_passphrase() -> String {
    let _ = libsodium_rs::ensure_init();
    loop {
        let phrase = (0..6)
            .map(|_| word_at(random_word_index()))
            .collect::<Vec<_>>()
            .join(" ");
        if passphrase_acceptable(&phrase) {
            return phrase;
        }
    }
}
pub fn passphrase_acceptable(passphrase: &str) -> bool {
    let words: Vec<_> = passphrase.split_whitespace().collect();
    if words.len() < 4 || passphrase.chars().count() < 24 {
        return false;
    }
    if passphrase
        .chars()
        .all(|character| character.is_ascii_digit() || character.is_whitespace())
    {
        return false;
    }
    let lowered: Vec<_> = words.iter().map(|word| word.to_ascii_lowercase()).collect();
    if lowered.iter().any(|word| {
        matches!(
            word.as_str(),
            "password" | "letmein" | "qwerty" | "correct" | "horse" | "battery" | "staple"
        )
    }) {
        return false;
    }
    lowered.windows(2).all(|pair| pair[0] != pair[1])
        && lowered
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == lowered.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn advance_state(s: String, e: &str) -> String {
        super::advance(&s, e)
    }
    #[test]
    fn verified_entitlement_is_required_before_creation() {
        let s = advance_state(advance_state(start("new"), "hosted_start"), "signed_in");
        let s = advance_state(s, "purchase_succeeded");
        assert!(s.contains("\"entitlement_state\":\"store_succeeded_unverified\""));
        assert!(advance_state(s.clone(), "entitlement_verified").contains("\"rejected\":true"));
        let s = advance_state(s, "verify_entitlement");
        assert!(advance_state(s, "entitlement_verified").contains("\"screen\":\"passphrase\""));
    }
    #[test]
    fn returning_lapsed_and_resubscribe_route_existing_account() {
        let s = advance_state(
            advance_state(start("returning"), "hosted_start"),
            "signed_in",
        );
        assert!(s.contains("\"screen\":\"join\""));
        let s = advance_state(advance_state(start("lapsed"), "hosted_start"), "signed_in");
        let s = advance_state(s, "resubscribe");
        let s = advance_state(s, "purchase_succeeded");
        let s = advance_state(s, "verify_entitlement");
        assert!(advance_state(s, "entitlement_verified").contains("\"screen\":\"join\""));
    }
    #[test]
    fn resume_and_retries_preserve_facts() {
        let s = advance_state(
            advance_state(start("provision_retry"), "hosted_start"),
            "signed_in",
        );
        let operation = serde_json::from_str::<Value>(&s).unwrap()["operation_id"].clone();
        assert_eq!(
            serde_json::from_str::<Value>(&resume(&s)).unwrap()["operation_id"],
            operation
        );
        let s = advance_state(advance_state(start("pending"), "hosted_start"), "signed_in");
        assert!(resume(&s).contains("\"screen\":\"purchase_pending\""));
    }
    #[test]
    fn approval_unlock_and_permissions_are_separate() {
        let s = advance_state(
            advance_state(start("returning"), "hosted_start"),
            "signed_in",
        );
        let s = advance_state(s, "approval_granted");
        assert!(s.contains("\"screen\":\"unlock\""));
        let s = advance_state(s, "passphrase_confirmed");
        assert!(s.contains("\"screen\":\"permissions\""));
        assert!(s.contains("\"unlocked\":true"));
    }
    #[test]
    fn all_string_canaries_and_oversize_are_rejected() {
        let base = start("new");
        let mut value: Value = serde_json::from_str(&base).unwrap();
        for key in [
            "scenario",
            "screen",
            "account_state",
            "entitlement_state",
            "provider",
            "approval_state",
            "status_key",
            "operation_id",
        ] {
            value[key] = Value::String("canary-secret-proof-key".into());
            assert!(resume(&value.to_string()).contains("\"rejected\":true"));
            value = serde_json::from_str(&base).unwrap();
        }
        assert!(resume(&"x".repeat(MAX_SNAPSHOT_BYTES + 1)).contains("\"rejected\":true"));
    }
    #[test]
    fn generated_words_and_custom_repetition_gate() {
        let generated = generate_passphrase();
        assert_eq!(generated.split_whitespace().count(), 6);
        assert!(passphrase_acceptable(&generated));
        assert!(!passphrase_acceptable(
            "mango mango mango mango mango mango"
        ));
    }

    #[test]
    fn conservative_gate_rejects_common_components_and_duplicate_candidates() {
        for word in ["battery", "correct", "password", "staple"] {
            let candidate = format!("{word} willow canyon lantern meadow orchard");
            assert!(!passphrase_acceptable(&candidate));
        }
        assert!(!passphrase_acceptable(
            "willow canyon willow lantern meadow orchard"
        ));
    }

    #[test]
    fn public_vocabulary_matches_vocabulary_json() {
        let vocabulary: Value =
            serde_json::from_str(include_str!("../hosted_onboarding_vocabulary.json")).unwrap();
        let scenarios: Vec<_> = vocabulary["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        let screens: Vec<_> = vocabulary["screens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(SCENARIOS, scenarios);
        assert_eq!(SCREENS, screens);
    }

    #[test]
    fn emitted_states_are_closed_over_every_vocab_event_and_resume() {
        let vocabulary: Value =
            serde_json::from_str(include_str!("../hosted_onboarding_vocabulary.json")).unwrap();
        for scenario in SCENARIOS {
            let state = start(scenario);
            assert!(parse_snapshot(&state).is_ok());
            for event in vocabulary["events"].as_array().unwrap() {
                let output = advance_state(state.clone(), event.as_str().unwrap());
                assert!(parse_snapshot(&output).is_ok(), "{scenario}/{event}");
                let resumed = resume(&output);
                assert!(
                    parse_snapshot(&resumed).is_ok(),
                    "resume {scenario}/{event}"
                );
            }
        }
    }

    #[test]
    fn paid_setup_back_and_resume_do_not_repurchase_or_lose_operation() {
        let mut state = advance_state(start("new"), "hosted_start");
        state = advance_state(state, "signed_in");
        state = advance_state(state, "purchase_succeeded");
        state = advance_state(state, "verify_entitlement");
        state = advance_state(state, "entitlement_verified");
        let returned = advance_state(state.clone(), "back");
        assert!(returned.contains("\"entitlement_state\":\"active\""));
        assert!(
            advance_state(returned.clone(), "purchase_succeeded").contains("\"rejected\":true")
        );
        assert!(advance_state(returned, "restore_succeeded").contains("\"screen\":\"passphrase\""));

        state = advance_state(state, "local_passphrase_accepted");
        state = advance_state(state, "passphrase_confirmed");
        let operation = serde_json::from_str::<Value>(&state).unwrap()["operation_id"].clone();
        let returned = advance_state(state, "cancel");
        let resumed = advance_state(returned, "restore_succeeded");
        assert_eq!(
            serde_json::from_str::<Value>(&resumed).unwrap()["operation_id"],
            operation
        );
        assert!(resumed.contains("\"screen\":\"provisioning\""));
    }

    #[test]
    fn provisioned_existing_account_retains_local_access_and_lapsed_login() {
        let mut state = advance_state(start("new"), "hosted_start");
        for event in [
            "signed_in",
            "purchase_succeeded",
            "verify_entitlement",
            "entitlement_verified",
            "local_passphrase_accepted",
            "passphrase_confirmed",
            "provision_finished",
            "permissions_done",
        ] {
            state = advance_state(state, event);
        }
        assert!(state.contains("\"account_state\":\"existing\""));
        let expired = advance_state(state, "entitlement_expired");
        assert!(expired.contains("\"screen\":\"settings\""));
        assert!(expired.contains("\"unlocked\":true"));
        let renewed = advance_state(expired.clone(), "resubscribe");
        assert!(renewed.contains("\"screen\":\"subscribe\""));
        assert!(renewed.contains("\"account_state\":\"existing\""));
        assert!(!renewed.contains("\"rejected\":true"));
        let signed_out = advance_state(expired, "signout");
        let rejoined = advance_state(advance_state(signed_out, "hosted_start"), "signed_in");
        assert!(rejoined.contains("\"screen\":\"lapsed\""));
    }

    #[test]
    fn back_from_join_permissions_and_settings_preserves_existing_account() {
        let mut s = advance_state(
            advance_state(start("returning"), "hosted_start"),
            "signed_in",
        );
        for destination in ["join", "permissions", "settings"] {
            if destination == "permissions" {
                s = advance_state(
                    advance_state(s, "passphrase_fallback"),
                    "passphrase_confirmed",
                );
            } else if destination == "settings" {
                s = advance_state(s, "permissions_done");
            }
            let back = advance_state(s.clone(), "back");
            assert!(back.contains("\"screen\":\"welcome\""));
            assert!(back.contains("\"account_state\":\"existing\""));
            assert!(!back.contains("\"rejected\":true"));
        }
    }
}
