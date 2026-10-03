import XCTest
@testable import PeppyHostedPreview

@MainActor
final class HostedPreviewModelTests: XCTestCase {
    func testBridgeHappyPathPreservesExplicitNullCheckpoint() async {
        let model = HostedPreviewModel(checkpoint: nil)
        XCTAssertTrue(model.rawCheckpoint.contains("\"operation_id\":null"))
        XCTAssertTrue(model.rawCheckpoint.contains("\"status_key\":null"))
        model.advance("hosted_start")
        await model.signIn()
        await model.subscribe()
        XCTAssertEqual(model.snapshot.screen, "passphrase")
        XCTAssertFalse(model.generatedPassphrase.isEmpty)
        model.acknowledgement = true
        model.acceptPassphrase()
        XCTAssertEqual(model.snapshot.screen, "confirm")
        model.confirmationPassphrase = model.generatedPassphrase
        model.confirmPassphrase()
        XCTAssertEqual(model.snapshot.screen, "provisioning")
        await model.provision()
        XCTAssertEqual(model.snapshot.screen, "permissions")
        model.advance("permissions_done")
        XCTAssertEqual(model.snapshot.screen, "settings")
    }

    func testReturningUnlockDoesNotRequireCreatedPassphrase() async {
        let model = HostedPreviewModel(checkpoint: nil)
        model.start(scenario: "returning")
        model.advance("hosted_start")
        await model.signIn()
        XCTAssertEqual(model.snapshot.screen, "join")
        model.advance("passphrase_fallback")
        XCTAssertEqual(model.snapshot.screen, "unlock")
        model.confirmationPassphrase = "local returning simulation"
        model.confirmPassphrase(unlock: true)
        XCTAssertEqual(model.snapshot.screen, "permissions")
    }

    func testRestoreUsesRestoreRatherThanPurchase() async {
        let purchase = CountingPurchaseProvider()
        let model = HostedPreviewModel(purchase: purchase, checkpoint: nil)
        model.advance("hosted_start")
        await model.signIn()
        await model.restore()
        XCTAssertEqual(purchase.purchaseCount, 0)
        XCTAssertEqual(purchase.restoreCount, 1)
        XCTAssertEqual(model.snapshot.screen, "passphrase")
    }

    func testCancellationAndScenarioChangeCannotAdvanceOldRequest() async {
        let auth = DelayedAuthProvider()
        let model = HostedPreviewModel(auth: auth, checkpoint: nil)
        model.advance("hosted_start")
        let task = Task { await model.signIn() }
        await auth.waitUntilStarted()
        model.start(scenario: "lapsed")
        auth.resume()
        await task.value
        XCTAssertEqual(model.snapshot.screen, "welcome")
        XCTAssertEqual(model.snapshot.scenario, "lapsed")
        XCTAssertTrue(model.generatedPassphrase.isEmpty)
    }

    func testSecretClearingAndResumeReentry() {
        let model = HostedPreviewModel(checkpoint: nil)
        model.generatedPassphrase = "secret"; model.confirmationPassphrase = "secret"; model.acknowledgement = true
        model.clearSecrets()
        XCTAssertTrue(model.generatedPassphrase.isEmpty)
        XCTAssertTrue(model.confirmationPassphrase.isEmpty)
        XCTAssertFalse(model.acknowledgement)
        model.advance("hosted_start")
        model.resume()
        XCTAssertEqual(model.snapshot.screen, "signin")
        // After resume, secrets should remain empty and no autosuggest
        XCTAssertTrue(model.generatedPassphrase.isEmpty)
        XCTAssertTrue(model.customPassphrase.isEmpty)
        XCTAssertFalse(model.acknowledgement)
    }

    func testDeleteAccountIsReducerOwnedAndReturnsToWelcome() async {
        let model = HostedPreviewModel(checkpoint: nil)
        model.advance("hosted_start")
        await model.signIn()
        await model.restore()
        model.acknowledgement = true
        model.acceptPassphrase()
        model.confirmationPassphrase = model.generatedPassphrase
        model.confirmPassphrase()
        await model.provision()
        model.advance("permissions_done")
        XCTAssertEqual(model.snapshot.screen, "settings")
        model.advance("delete_account")
        XCTAssertEqual(model.snapshot.screen, "delete_account")
        model.advance("deletion_confirmed")
        XCTAssertEqual(model.snapshot.screen, "welcome")
    }

    func testPendingApprovalAndValidation() async {
        let model = HostedPreviewModel(checkpoint: nil)
        model.start(scenario: "approval_denied")
        model.advance("hosted_start")
        await model.signIn()
        XCTAssertEqual(model.snapshot.screen, "join")
        XCTAssertEqual(model.snapshot.approval_state, "denied")
        // Can advance through passphrase fallback
        model.advance("passphrase_fallback")
        XCTAssertEqual(model.snapshot.screen, "unlock")
    }

    func testUnavailableStoreShowsRetryOnly() async {
        let model = HostedPreviewModel(checkpoint: nil)
        model.start(scenario: "store_unavailable")
        model.advance("hosted_start")
        await model.signIn()
        XCTAssertEqual(model.snapshot.screen, "subscribe")
        XCTAssertTrue(model.storeUnavailable)
        await model.subscribe()
        XCTAssertEqual(model.snapshot.entitlement_state, "none")
        model.advance("store_retry")
        XCTAssertEqual(model.snapshot.screen, "subscribe")
    }

    func testProvisionFailureAndRetry() async {
        let model = HostedPreviewModel(checkpoint: nil)
        model.start(scenario: "provision_retry")
        model.advance("hosted_start")
        await model.signIn()
        XCTAssertEqual(model.snapshot.screen, "provisioning")
        XCTAssertEqual(model.snapshot.status_key, "provisioning_error")
        let operation = model.snapshot.operation_id
        await model.retryProvision()
        XCTAssertEqual(model.snapshot.screen, "permissions")
        XCTAssertEqual(model.snapshot.operation_id, operation)
    }

    func testSignoutClearsSecrets() {
        let model = HostedPreviewModel(checkpoint: nil)
        model.generatedPassphrase = "secret"
        model.advance("hosted_start")
        model.advance("signout")
        XCTAssertTrue(model.generatedPassphrase.isEmpty)
        XCTAssertNil(model.account)
    }

    func testResumeWithPendingApprovalRetainsState() async {
        let model = HostedPreviewModel(checkpoint: nil)
        model.start(scenario: "returning")
        model.advance("hosted_start")
        await model.signIn()
        model.advance("show_approval")
        model.resume()
        XCTAssertEqual(model.snapshot.screen, "join")
        XCTAssertEqual(model.snapshot.approval_state, "awaiting")
    }

    func testBackgroundSetsNoSecrets() async {
        let model = HostedPreviewModel(checkpoint: nil)
        model.advance("hosted_start")
        await model.signIn(); await model.subscribe()
        model.acknowledgement = true; model.acceptPassphrase()
        XCTAssertEqual(model.snapshot.screen, "confirm")
        model.resume()
        XCTAssertEqual(model.snapshot.screen, "passphrase")
        XCTAssertTrue(model.generatedPassphrase.isEmpty)
        XCTAssertTrue(model.customPassphrase.isEmpty)
        XCTAssertTrue(model.confirmationPassphrase.isEmpty)
        let restored = HostedPreviewModel(checkpoint: model.rawCheckpoint)
        XCTAssertTrue(restored.generatedPassphrase.isEmpty)
    }

    func testLocalPassphraseValidationRejectsWeak() async {
        let model = HostedPreviewModel(checkpoint: nil)
        model.advance("hosted_start")
        await model.signIn()
        await model.restore()
        model.customPassphrase = "a"
        model.acknowledgement = true
        model.acceptPassphrase()
        XCTAssertEqual(model.localError, "passphrase_weak")
    }

    func testExpiredEntitlementShowsRenewalAction() async {
        let model = HostedPreviewModel(checkpoint: nil)
        model.advance("hosted_start")
        await model.signIn()
        await model.restore()
        model.acknowledgement = true
        model.acceptPassphrase()
        model.confirmationPassphrase = model.generatedPassphrase
        model.confirmPassphrase()
        await model.provision()
        model.advance("permissions_done")
        model.advance("entitlement_expired")
        XCTAssertEqual(model.snapshot.screen, "settings")
        XCTAssertEqual(model.snapshot.entitlement_state, "expired")
        model.beginRenewal()
        XCTAssertEqual(model.snapshot.screen, "subscribe")
        XCTAssertEqual(model.snapshot.account_state, "existing")
    }

    func testPaidBackAndContinueSkipsStoreAndFurtherVerification() async {
        let store = CountingPurchaseProvider()
        let model = HostedPreviewModel(purchase: store, checkpoint: nil)
        model.advance("hosted_start"); await model.signIn(); await model.subscribe()
        model.advance("back"); await model.subscribe()
        XCTAssertEqual(model.snapshot.screen, "passphrase")
        XCTAssertNil(model.snapshot.status_key)
        XCTAssertEqual(store.purchaseCount, 1)
        XCTAssertEqual(store.restoreCount, 0)
    }

    func testVerificationResumeReconcilesThroughStoreAndService() async {
        let model = HostedPreviewModel(checkpoint: nil)
        model.advance("hosted_start"); await model.signIn()
        model.advance("purchase_succeeded"); model.advance("verify_entitlement")
        let resumed = HostedPreviewModel(checkpoint: model.rawCheckpoint)
        await resumed.automaticWork()
        XCTAssertEqual(resumed.snapshot.screen, "passphrase")
        XCTAssertNil(resumed.localError)
    }
}

@MainActor private final class CountingPurchaseProvider: HostedPreviewPurchaseProvider {
    var purchaseCount = 0; var restoreCount = 0
    let displayPrice = "$4.99/month"
    func purchase(for account: HostedPreviewAccount) async throws -> HostedPreviewPurchase { purchaseCount += 1; return .init(accountID: account.id, provider: account.provider, displayPrice: displayPrice, restored: false) }
    func restore(for account: HostedPreviewAccount) async throws -> HostedPreviewPurchase { restoreCount += 1; return .init(accountID: account.id, provider: account.provider, displayPrice: displayPrice, restored: true) }
}

@MainActor private final class DelayedAuthProvider: HostedPreviewAuthProvider {
    private var continuation: CheckedContinuation<Void, Never>?
    private var readiness: CheckedContinuation<Void, Never>?
    func signIn() async throws -> HostedPreviewAccount {
        await withCheckedContinuation {
            continuation = $0
            readiness?.resume()
            readiness = nil
        }
        return .init(id: "preview-account", provider: "preview")
    }
    func waitUntilStarted() async {
        guard continuation == nil else { return }
        await withCheckedContinuation { readiness = $0 }
    }
    func resume() { continuation?.resume(); continuation = nil }
}
