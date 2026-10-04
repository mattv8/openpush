import XCTest
import PeppyBindings
@testable import PeppyHostedPreview

@MainActor
final class HostedPreviewModelTests: XCTestCase {
    func testAutomaticDriverDoesNotCancelOwningLookupWhenScreenChanges() async {
        let resumable = hostedPreviewV2Advance(snapshot: hostedPreviewV2Start(scenario: "new"), event: "hosted_start")
        let store = MemoryStore(values: [MemoryStore.v2: resumable])
        let service = DeferredService(event: "account_new")
        let model = HostedPreviewModel(auth: CountingAuth(), service: service, checkpointStore: store)
        let driver = HostedPreviewAutomaticWorkDriver(model: model)
        let foreground = Task { await driver.enterForeground(resuming: false) }
        await service.waitForLookup()
        driver.requestSchedule()
        service.finishLookup()
        await foreground.value
        XCTAssertEqual(model.snapshot.screen, "subscribe")
        XCTAssertEqual(service.lookupCount, 1)
        XCTAssertNil(model.localError)
    }

    func testDriverBlocksCallsWhileBackgrounded() async {
        let store = MemoryStore(values: [MemoryStore.v2: hostedPreviewV2Start(scenario: "new")])
        let auth = CountingAuth(); let service = RecordingService()
        let model = HostedPreviewModel(auth: auth, service: service, checkpointStore: store)
        let driver = HostedPreviewAutomaticWorkDriver(model: model)
        await driver.schedule()
        XCTAssertEqual(auth.restoreCount, 0)
        XCTAssertEqual(service.lookupCount, 0)
    }

    func testPendingForegroundPerformsExactlyOneLookupWithoutLoop() async {
        let service = RecordingService(events: ["account_pending", "account_pending"])
        let model = HostedPreviewModel(service: service, checkpointStore: MemoryStore(), checkpoint: hostedPreviewV2Start(scenario: "pending"))
        model.advance("hosted_start"); await model.signIn()
        let driver = HostedPreviewAutomaticWorkDriver(model: model)
        await driver.enterForeground(resuming: true)
        XCTAssertEqual(model.snapshot.screen, "purchase_pending")
        XCTAssertEqual(service.lookupCount, 2)
        await driver.schedule()
        XCTAssertEqual(service.lookupCount, 2)
    }

    func testBackgroundCancelsLateLookupUntilNextForeground() async {
        let service = DeferredService(event: "account_new")
        let resumable = hostedPreviewV2Advance(snapshot: hostedPreviewV2Start(scenario: "new"), event: "hosted_start")
        let model = HostedPreviewModel(auth: CountingAuth(), service: service, checkpointStore: MemoryStore(values: [MemoryStore.v2: resumable]))
        let driver = HostedPreviewAutomaticWorkDriver(model: model)
        let foreground = Task { await driver.enterForeground(resuming: false) }
        await service.waitForLookup(); driver.enterBackground(); service.finishLookup(); await foreground.value
        XCTAssertEqual(model.snapshot.screen, "signin")
        XCTAssertEqual(service.lookupCount, 1)
        await driver.enterForeground(resuming: true)
        XCTAssertEqual(service.lookupCount, 2)
        XCTAssertEqual(model.snapshot.screen, "subscribe")
    }

    func testPaidBackgroundAndBackNeverRepurchase() async {
        let purchases = CountingPurchase(); let model = HostedPreviewModel(purchase: purchases, checkpointStore: MemoryStore(), checkpoint: hostedPreviewV2Start(scenario: "new"))
        model.advance("hosted_start"); await model.signIn(); await model.subscribe()
        model.backgrounded(); await model.automaticWork()
        XCTAssertEqual(model.snapshot.screen, "passphrase")
        model.advance("back"); model.advance("hosted_start"); await model.signIn()
        XCTAssertEqual(model.snapshot.screen, "passphrase")
        XCTAssertEqual(purchases.count, 1)
    }

    func testInterruptedProvisioningReusesOperationID() async throws {
        let service = RecordingService(); let model = HostedPreviewModel(service: service, checkpointStore: MemoryStore(), checkpoint: hostedPreviewV2Start(scenario: "new"))
        model.advance("hosted_start"); await model.signIn(); await model.subscribe()
        model.acknowledgement = true; model.acceptPassphrase(); model.confirmationPassphrase = model.generatedPassphrase; model.confirmPassphrase()
        let id = try XCTUnwrap(model.snapshot.operation_id)
        model.backgrounded(); await model.automaticWork()
        XCTAssertEqual(service.provisionedIDs, [id])
        XCTAssertEqual(model.snapshot.screen, "permissions")
    }

    func testLookupFailurePreservesPaidFactsForRetry() async {
        let service = RecordingService(events: ["account_new"])
        let model = HostedPreviewModel(service: service, checkpointStore: MemoryStore(), checkpoint: hostedPreviewV2Start(scenario: "new"))
        model.advance("hosted_start"); await model.signIn(); await model.subscribe()
        service.lookupErrors = 1; model.backgrounded(); await model.automaticWork()
        XCTAssertEqual(model.snapshot.status_key, "hosted_account_check_failed")
        await model.retryLookup()
        XCTAssertEqual(model.snapshot.screen, "passphrase")
        XCTAssertEqual(service.seeds.last?.entitlementState, "active")
    }

    func testValidationRetryUsesReceiptWithoutRepurchase() async {
        let purchases = CountingPurchase(); let service = RecordingService(validationErrors: 1)
        let model = HostedPreviewModel(purchase: purchases, service: service, checkpointStore: MemoryStore(), checkpoint: hostedPreviewV2Start(scenario: "new"))
        model.advance("hosted_start"); await model.signIn(); await model.subscribe()
        XCTAssertEqual(model.snapshot.screen, "subscription_verifying")
        await model.retryVerification()
        XCTAssertEqual(model.snapshot.screen, "passphrase")
        XCTAssertEqual(purchases.count, 1)
        XCTAssertEqual(service.validationCount, 2)
    }

    func testFreshResumeRecoversUnverifiedReceiptWithoutPurchase() async {
        let store = MemoryStore(); let purchases = CountingPurchase(); let failing = RecordingService(validationErrors: 1)
        let auth = FakeHostedPreviewAuthProvider()
        let first = HostedPreviewModel(auth: auth, purchase: purchases, service: failing, checkpointStore: store, checkpoint: hostedPreviewV2Start(scenario: "new"))
        first.advance("hosted_start"); await first.signIn(); await first.subscribe()
        XCTAssertEqual(first.snapshot.entitlement_state, "verifying")
        let recovered = RecordingService(); let resumed = HostedPreviewModel(auth: auth, purchase: purchases, service: recovered, checkpointStore: store)
        await resumed.automaticWork()
        XCTAssertEqual(resumed.snapshot.screen, "passphrase")
        XCTAssertEqual(purchases.count, 1)
        XCTAssertEqual(recovered.recoveryCount, 1)
        XCTAssertTrue(resumed.generatedPassphrase.isEmpty)
    }

    func testColdLookupErrorRestoresIncompletePaidFixtureWithoutPurchase() async {
        let store = MemoryStore(values: [MemoryStore.v2: coldCheckingCheckpoint(
            scenario: "new",
            resolution: "account_incomplete_active",
            failsLookup: true
        )])
        let purchases = CountingPurchase()
        let model = HostedPreviewModel(auth: CountingAuth(), purchase: purchases, service: FakeHostedPreviewService(), checkpointStore: store)

        await model.automaticWork()

        XCTAssertEqual(model.snapshot.screen, "passphrase")
        XCTAssertEqual(model.snapshot.account_state, "incomplete")
        XCTAssertEqual(purchases.count, 0)
    }

    func testColdSuspendedLookupRestoresExistingPendingFixtureWithoutPurchase() async {
        let store = MemoryStore(values: [MemoryStore.v2: coldCheckingCheckpoint(
            scenario: "pending",
            resolution: "account_existing_pending"
        )])
        let purchases = CountingPurchase()
        let model = HostedPreviewModel(auth: CountingAuth(), purchase: purchases, service: FakeHostedPreviewService(), checkpointStore: store)

        await model.automaticWork()

        XCTAssertEqual(model.snapshot.screen, "purchase_pending")
        XCTAssertEqual(model.snapshot.account_state, "existing")
        XCTAssertEqual(purchases.count, 0)
    }

    func testColdLookupErrorRestoresExistingVerificationWithoutPurchase() async {
        let store = MemoryStore(values: [MemoryStore.v2: coldCheckingCheckpoint(
            scenario: "returning",
            resolution: "account_existing_verification_pending",
            failsLookup: true
        )])
        let purchases = CountingPurchase()
        let model = HostedPreviewModel(auth: CountingAuth(), purchase: purchases, service: FakeHostedPreviewService(), checkpointStore: store)

        await model.automaticWork()

        XCTAssertEqual(model.snapshot.screen, "join")
        XCTAssertEqual(model.snapshot.account_state, "existing")
        XCTAssertEqual(purchases.count, 0)
    }

    private func coldCheckingCheckpoint(scenario: String, resolution: String, failsLookup: Bool = false) -> String {
        var raw = hostedPreviewV2Start(scenario: scenario)
        for event in ["hosted_start", "signed_in", resolution] {
            raw = hostedPreviewV2Advance(snapshot: raw, event: event)
        }
        raw = hostedPreviewV2Resume(snapshot: raw)
        raw = hostedPreviewV2Advance(snapshot: raw, event: "session_restored")
        if failsLookup { raw = hostedPreviewV2Advance(snapshot: raw, event: "account_lookup_failed") }
        return raw
    }

    func testColdExistingUnavailableAndRejectedEntitlementRetainExistingIdentity() async {
        let unavailableStore = MemoryStore(values: [MemoryStore.v2: coldCheckingCheckpoint(
            scenario: "store_unavailable",
            resolution: "account_existing_store_unavailable",
            failsLookup: true
        )])
        let unavailablePurchases = CountingPurchase()
        let unavailable = HostedPreviewModel(
            auth: CountingAuth(),
            purchase: unavailablePurchases,
            service: FakeHostedPreviewService(),
            checkpointStore: unavailableStore
        )
        await unavailable.automaticWork()
        XCTAssertEqual(unavailable.snapshot.screen, "subscribe")
        XCTAssertEqual(unavailable.snapshot.account_state, "existing")
        XCTAssertEqual(unavailable.snapshot.entitlement_state, "unavailable")
        XCTAssertEqual(unavailablePurchases.count, 0)

        var rejected = hostedPreviewV2Start(scenario: "returning")
        for event in ["hosted_start", "signed_in", "account_existing_verification_pending", "verify_entitlement", "entitlement_rejected"] {
            rejected = hostedPreviewV2Advance(snapshot: rejected, event: event)
        }
        rejected = hostedPreviewV2Resume(snapshot: rejected)
        rejected = hostedPreviewV2Advance(snapshot: rejected, event: "session_restored")
        rejected = hostedPreviewV2Advance(snapshot: rejected, event: "account_lookup_failed")
        let rejectedPurchases = CountingPurchase()
        let rejectedModel = HostedPreviewModel(
            auth: CountingAuth(),
            purchase: rejectedPurchases,
            service: FakeHostedPreviewService(),
            checkpointStore: MemoryStore(values: [MemoryStore.v2: rejected])
        )
        await rejectedModel.automaticWork()
        XCTAssertEqual(rejectedModel.snapshot.screen, "lapsed")
        XCTAssertEqual(rejectedModel.snapshot.account_state, "existing")
        XCTAssertEqual(rejectedPurchases.count, 0)
    }

    func testColdAmbiguousPaidPendingAndUnavailableFactsFailLookup() async {
        let cases = [
            ("new", "account_incomplete_active"),
            ("pending", "account_existing_pending"),
            ("store_unavailable", "account_existing_store_unavailable")
        ]
        for (scenario, resolution) in cases {
            let noHint = checkpoint(coldCheckingCheckpoint(scenario: scenario, resolution: resolution), resumeAccountState: NSNull())
            let ambiguous = resolution == "account_incomplete_active"
                ? checkpoint(noHint, resumeScreen: NSNull())
                : noHint
            let purchases = CountingPurchase()
            let model = HostedPreviewModel(
                auth: CountingAuth(),
                purchase: purchases,
                service: FakeHostedPreviewService(),
                checkpointStore: MemoryStore(values: [MemoryStore.v2: ambiguous])
            )

            await model.automaticWork()

            XCTAssertEqual(model.snapshot.screen, "checking_account", resolution)
            XCTAssertEqual(model.snapshot.status_key, "hosted_account_check_failed", resolution)
            XCTAssertEqual(purchases.count, 0, resolution)
        }
    }

    func testLegacyColdProvisioningCheckpointWithoutOptionalHintUsesResumeScreenFallback() async {
        let legacy = checkpoint(
            coldCheckingCheckpoint(scenario: "new", resolution: "account_incomplete_provisioning"),
            resumeAccountState: nil
        )
        let purchases = CountingPurchase()
        let model = HostedPreviewModel(
            auth: CountingAuth(),
            purchase: purchases,
            service: FakeHostedPreviewService(),
            checkpointStore: MemoryStore(values: [MemoryStore.v2: legacy])
        )

        await model.automaticWork()

        XCTAssertEqual(model.snapshot.screen, "permissions")
        XCTAssertEqual(model.snapshot.account_state, "existing")
        XCTAssertEqual(purchases.count, 0)
    }

    private func checkpoint(_ raw: String, resumeAccountState: Any?) -> String {
        var object = try! JSONSerialization.jsonObject(with: Data(raw.utf8)) as! [String: Any]
        if let resumeAccountState { object["resume_account_state"] = resumeAccountState }
        else { object.removeValue(forKey: "resume_account_state") }
        return String(decoding: try! JSONSerialization.data(withJSONObject: object), as: UTF8.self)
    }

    private func checkpoint(_ raw: String, resumeScreen: Any?) -> String {
        var object = try! JSONSerialization.jsonObject(with: Data(raw.utf8)) as! [String: Any]
        if let resumeScreen { object["resume_screen"] = resumeScreen }
        else { object.removeValue(forKey: "resume_screen") }
        return String(decoding: try! JSONSerialization.data(withJSONObject: object), as: UTF8.self)
    }

    func testSelectedAppleAndGoogleProvidersBindDistinctAccountsAndRejectWrongProvider() async throws {
        let fake = FakeHostedPreviewAuthProvider()
        let coldSession = try await fake.restoreSession()
        XCTAssertNil(coldSession)
        let apple = try await fake.signIn(provider: "apple")
        XCTAssertEqual(apple, HostedPreviewAccount(id: "preview-apple-account", provider: "apple"))
        let google = try await fake.signIn(provider: "google")
        XCTAssertEqual(google, HostedPreviewAccount(id: "preview-google-account", provider: "google"))

        let wrong = HostedPreviewModel(
            auth: WrongProviderAuth(),
            service: RecordingService(events: ["account_new"]),
            checkpointStore: MemoryStore(),
            checkpoint: hostedPreviewV2Start(scenario: "new")
        )
        wrong.advance("hosted_start")
        await wrong.signIn(provider: "google")
        XCTAssertEqual(wrong.snapshot.screen, "signin")
        XCTAssertNil(wrong.account)
        XCTAssertEqual(wrong.localError, "preview_error")
    }

    func testExplicitSessionEndingClearsRestorationButBackgroundDoesNotSignOut() async throws {
        let auth = FakeHostedPreviewAuthProvider()
        let model = HostedPreviewModel(
            auth: auth,
            service: RecordingService(events: ["account_new", "account_new"]),
            checkpointStore: MemoryStore(),
            checkpoint: hostedPreviewV2Start(scenario: "new")
        )
        model.advance("hosted_start"); await model.signIn(provider: "google")
        model.backgrounded()
        let backgroundSession = try await auth.restoreSession()
        XCTAssertNotNil(backgroundSession)
        model.advance("signout")
        let signedOutSession = try await auth.restoreSession()
        XCTAssertNil(signedOutSession)
        model.advance("hosted_start"); model.backgrounded(); await model.automaticWork()
        XCTAssertEqual(model.snapshot.screen, "signin")
        XCTAssertNil(model.account)

        await model.signIn(provider: "apple"); model.advance("reset")
        let resetSession = try await auth.restoreSession()
        XCTAssertNil(resetSession)
        model.advance("hosted_start"); await model.signIn(provider: "apple"); model.advance("deletion_confirmed")
        let deletedSession = try await auth.restoreSession()
        XCTAssertNil(deletedSession)
    }

    func testProviderOnlyMismatchStaysUnclassified() async {
        let model = HostedPreviewModel(service: RecordingService(provider: "other"), checkpointStore: MemoryStore(), checkpoint: hostedPreviewV2Start(scenario: "new"))
        model.advance("hosted_start"); await model.signIn()
        XCTAssertEqual(model.snapshot.account_state, "signed_in")
        XCTAssertEqual(model.snapshot.status_key, "hosted_account_check_failed")
    }

    func testMalformedAndUnclassifiedV1NeverSeedPaidFacts() async {
        let malformed = HostedPreviewModel(checkpointStore: MemoryStore(values: [MemoryStore.v2: "{bad"]))
        XCTAssertEqual(malformed.snapshot.screen, "welcome")
        XCTAssertEqual(malformed.snapshot.entitlement_state, "none")
        var unclassified = hostedPreviewStart(scenario: "new")
        unclassified = hostedPreviewAdvance(snapshot: unclassified, event: "hosted_start")
        unclassified = hostedPreviewAdvance(snapshot: unclassified, event: "signed_in")
        let v1Store = MemoryStore(values: [MemoryStore.v1: unclassified])
        let v1 = HostedPreviewModel(auth: CountingAuth(), checkpointStore: v1Store); await v1.automaticWork()
        XCTAssertEqual(v1.snapshot.screen, "checking_account")
        XCTAssertEqual(v1.snapshot.status_key, "hosted_account_check_failed")
        XCTAssertEqual(v1.snapshot.entitlement_state, "none")
    }

    func testV2PrecedesV1AndDoesNotOverwriteV1() {
        let v1 = hostedPreviewStart(scenario: "returning")
        let store = MemoryStore(values: [MemoryStore.v1: v1, MemoryStore.v2: hostedPreviewV2Start(scenario: "lapsed")])
        let model = HostedPreviewModel(checkpointStore: store)
        XCTAssertEqual(model.snapshot.scenario, "lapsed")
        XCTAssertEqual(store.values[MemoryStore.v1], v1)
    }

    func testLazyOptionalInitializationConstructsAndPersistsOnce() {
        let store = MemoryStore(); var creations = 0; var model: HostedPreviewModel?
        model = HostedPreviewModel.initializeIfNeeded(model) { creations += 1; return HostedPreviewModel(checkpointStore: store, checkpoint: hostedPreviewV2Start(scenario: "new")) }
        model = HostedPreviewModel.initializeIfNeeded(model) { creations += 1; return HostedPreviewModel(checkpointStore: store, checkpoint: hostedPreviewV2Start(scenario: "lapsed")) }
        XCTAssertEqual(creations, 1); XCTAssertEqual(store.setCount, 1); XCTAssertEqual(model?.snapshot.scenario, "new")
    }

    func testNoncooperativeProvisionAndPreparationLateResultsAreDiscardedByDriver() async {
        var provisioning = hostedPreviewV2Start(scenario: "new")
        for event in ["hosted_start", "signed_in", "account_incomplete_provisioning"] {
            provisioning = hostedPreviewV2Advance(snapshot: provisioning, event: event)
        }
        let provisionService = DeferredService(event: "account_incomplete_provisioning", deferLookup: false)
        let provisionModel = HostedPreviewModel(auth: CountingAuth(), service: provisionService, checkpointStore: MemoryStore(), checkpoint: provisioning)
        let provisionDriver = HostedPreviewAutomaticWorkDriver(model: provisionModel)
        let provisionRun = Task { await provisionDriver.enterForeground(resuming: false) }
        await provisionService.waitForProvision()
        provisionDriver.cancelCurrentWork()
        provisionModel.start(scenario: "returning")
        provisionService.finishProvision()
        await provisionRun.value
        XCTAssertEqual(provisionModel.snapshot.screen, "welcome")

        let preparationService = DeferredService(event: "account_existing_active", deferLookup: false, deferPreparation: true)
        let preparationModel = HostedPreviewModel(service: preparationService, checkpointStore: MemoryStore(), checkpoint: hostedPreviewV2Start(scenario: "returning"))
        preparationModel.advance("hosted_start")
        await preparationModel.signIn()
        preparationModel.advance("approval_granted")
        preparationModel.confirmationPassphrase = "preview unlock"
        preparationModel.confirmPassphrase(unlock: true)
        XCTAssertEqual(preparationModel.snapshot.screen, "syncing")

        let preparationDriver = HostedPreviewAutomaticWorkDriver(model: preparationModel)
        let preparationRun = Task { await preparationDriver.enterForeground(resuming: false) }
        await preparationService.waitForPreparation()
        preparationDriver.cancelCurrentWork()
        preparationModel.start(scenario: "new")
        preparationService.finishPreparation()
        await preparationRun.value
        XCTAssertEqual(preparationModel.snapshot.screen, "welcome")
    }

}

@MainActor private final class MemoryStore: HostedPreviewCheckpointStore {
    static let v1 = "peppy.hosted-preview.checkpoint.v1", v2 = "peppy.hosted-preview.checkpoint.v2"
    var values: [String: String]; private(set) var setCount = 0
    init(values: [String: String] = [:]) { self.values = values }
    func value(forKey key: String) -> String? { values[key] }
    func set(_ value: String, forKey key: String) { setCount += 1; values[key] = value }
}
private extension HostedPreviewAccount { static let preview = HostedPreviewAccount(id: "preview-account", provider: "preview") }
@MainActor private final class CountingAuth: HostedPreviewAuthProvider {
    private(set) var restoreCount = 0
    private var session: HostedPreviewAccount?
    init(session: HostedPreviewAccount? = .preview) { self.session = session }
    func signIn(provider: String) async throws -> HostedPreviewAccount {
        let account = HostedPreviewAccount(id: "preview-\(provider)-account", provider: provider)
        session = account
        return account
    }
    func restoreSession() async throws -> HostedPreviewAccount? { restoreCount += 1; return session }
    func signOut() { session = nil }
}
@MainActor private final class WrongProviderAuth: HostedPreviewAuthProvider {
    func signIn(provider: String) async throws -> HostedPreviewAccount { .init(id: "wrong", provider: "apple") }
    func restoreSession() async throws -> HostedPreviewAccount? { nil }
    func signOut() {}
}
@MainActor private final class CountingPurchase: HostedPreviewPurchaseProvider {
    private(set) var count = 0; let displayPrice = "$4.99/month"
    func purchase(for account: HostedPreviewAccount) async throws -> HostedPreviewPurchase { count += 1; return .init(accountID: account.id, provider: account.provider, displayPrice: displayPrice) }
}
@MainActor private final class RecordingService: HostedPreviewService {
    var events: [String], lookupErrors = 0; let provider: String?; var validationErrors: Int
    private(set) var lookupCount = 0, validationCount = 0, recoveryCount = 0
    private(set) var provisionedIDs: [String] = [], seeds: [HostedPreviewCheckpointSeed] = []
    init(events: [String] = [], provider: String? = nil, validationErrors: Int = 0) { self.events = events; self.provider = provider; self.validationErrors = validationErrors }
    func lookup(account: HostedPreviewAccount, scenario: String, checkpoint: HostedPreviewCheckpointSeed) async throws -> HostedPreviewLookup {
        lookupCount += 1; seeds.append(checkpoint)
        if lookupErrors > 0 { lookupErrors -= 1; throw Failure.failed }
        let event: String
        if !events.isEmpty { event = events.removeFirst() }
        else if ["store_succeeded_unverified", "verifying"].contains(checkpoint.entitlementState) { event = checkpoint.accountState == "existing" ? "account_existing_verification_pending" : "account_verification_pending" }
        else if ["active", "grace", "billing_retry"].contains(checkpoint.entitlementState) { event = checkpoint.accountState == "existing" ? "account_existing_active" : checkpoint.operationID == nil ? "account_incomplete_active" : "account_incomplete_provisioning" }
        else { event = scenario == "returning" ? "account_existing_active" : "account_new" }
        return .init(accountID: account.id, provider: provider ?? account.provider, event: event)
    }
    func recoverPurchase(account: HostedPreviewAccount) async throws -> HostedPreviewPurchase? { recoveryCount += 1; return .init(accountID: account.id, provider: account.provider, displayPrice: "$4.99/month") }
    func validate(entitlement purchase: HostedPreviewPurchase, account: HostedPreviewAccount) async throws -> Bool { validationCount += 1; if validationErrors > 0 { validationErrors -= 1; throw Failure.failed }; return true }
    func provision(operationID: String, account: HostedPreviewAccount) async throws -> HostedPreviewOperation { provisionedIDs.append(operationID); return .init(id: operationID, accountID: account.id, provider: account.provider) }
    func prepare(account: HostedPreviewAccount) async throws -> HostedPreviewPreparation { .init(accountID: account.id, provider: account.provider) }
    enum Failure: Error { case failed }
}
@MainActor private final class DeferredService: HostedPreviewService {
    let event: String; let deferLookup: Bool; let deferPreparation: Bool; private(set) var lookupCount = 0
    private var lookup: CheckedContinuation<Void, Never>?, lookupWaiter: CheckedContinuation<Void, Never>?
    private var provision: CheckedContinuation<Void, Never>?, provisionWaiter: CheckedContinuation<Void, Never>?
    private var preparation: CheckedContinuation<Void, Never>?, preparationWaiter: CheckedContinuation<Void, Never>?
    init(event: String, deferLookup: Bool = true, deferPreparation: Bool = false) {
        self.event = event; self.deferLookup = deferLookup; self.deferPreparation = deferPreparation
    }
    func lookup(account: HostedPreviewAccount, scenario: String, checkpoint: HostedPreviewCheckpointSeed) async throws -> HostedPreviewLookup {
        lookupCount += 1
        if deferLookup, lookupCount == 1 { await withCheckedContinuation { lookup = $0; lookupWaiter?.resume(); lookupWaiter = nil } }
        return .init(accountID: account.id, provider: account.provider, event: event)
    }
    func recoverPurchase(account: HostedPreviewAccount) async throws -> HostedPreviewPurchase? { .init(accountID: account.id, provider: account.provider, displayPrice: "$4.99/month") }
    func validate(entitlement purchase: HostedPreviewPurchase, account: HostedPreviewAccount) async throws -> Bool { true }
    func provision(operationID: String, account: HostedPreviewAccount) async throws -> HostedPreviewOperation { await withCheckedContinuation { provision = $0; provisionWaiter?.resume(); provisionWaiter = nil }; return .init(id: operationID, accountID: account.id, provider: account.provider) }
    func prepare(account: HostedPreviewAccount) async throws -> HostedPreviewPreparation {
        if deferPreparation { await withCheckedContinuation { preparation = $0; preparationWaiter?.resume(); preparationWaiter = nil } }
        return .init(accountID: account.id, provider: account.provider)
    }
    func waitForLookup() async { if lookup == nil { await withCheckedContinuation { lookupWaiter = $0 } } }
    func finishLookup() { lookup?.resume(); lookup = nil }
    func waitForProvision() async { if provision == nil { await withCheckedContinuation { provisionWaiter = $0 } } }
    func finishProvision() { provision?.resume(); provision = nil }
    func waitForPreparation() async { if preparation == nil { await withCheckedContinuation { preparationWaiter = $0 } } }
    func finishPreparation() { preparation?.resume(); preparation = nil }
}
