//! Debug-only, host-driven hosted onboarding preview policy. No network,
//! storage, enrollment, proof, billing, or production-credential effect exists here.

#[uniffi::export]
pub fn hosted_preview_start(scenario: String) -> String {
    peppy_hosted_onboarding::start(&scenario)
}

#[uniffi::export]
pub fn hosted_preview_advance(snapshot: String, event: String) -> String {
    peppy_hosted_onboarding::advance(&snapshot, &event)
}

#[uniffi::export]
pub fn hosted_preview_resume(snapshot: String) -> String {
    peppy_hosted_onboarding::resume(&snapshot)
}

#[uniffi::export]
pub fn hosted_preview_v2_start(scenario: String) -> String {
    peppy_hosted_onboarding::mobile_v2::start(&scenario)
}

#[uniffi::export]
pub fn hosted_preview_v2_advance(snapshot: String, event: String) -> String {
    peppy_hosted_onboarding::mobile_v2::advance(&snapshot, &event)
}

#[uniffi::export]
pub fn hosted_preview_v2_resume(snapshot: String) -> String {
    peppy_hosted_onboarding::mobile_v2::resume(&snapshot)
}

/// Resamples six EFF large-list words until they pass the conservative local UI gate.
#[uniffi::export]
pub fn hosted_preview_passphrase() -> String {
    peppy_hosted_onboarding::generate_passphrase()
}

#[uniffi::export]
pub fn hosted_preview_passphrase_acceptable(passphrase: String) -> bool {
    peppy_hosted_onboarding::passphrase_acceptable(&passphrase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_wrappers_forward_the_closed_reducer_contract() {
        let started = hosted_preview_v2_start("new".into());
        let signin = hosted_preview_v2_advance(started, "hosted_start".into());
        let checking = hosted_preview_v2_advance(signin, "signed_in".into());
        assert!(checking.contains("\"screen\":\"checking_account\""));
        assert!(hosted_preview_v2_resume(checking).contains("\"version\":2"));
    }
}
