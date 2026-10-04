import Foundation
import Observation
#if canImport(PeppyBindings)
import PeppyBindings
#endif

@MainActor @Observable
final class HostedPreviewModel {
    struct Snapshot: Codable, Equatable {
        let version: Int; let scenario: String; let screen: String; let account_state: String
        let entitlement_state: String; let provider: String; let operation_id: String?
        let status_key: String?; let approval_state: String; let unlocked: Bool; let rejected: Bool
        let resume_screen: String?
        let resume_account_state: String?
    }

    private static let checkpointV2Key = "peppy.hosted-preview.checkpoint.v2"
    private static let checkpointV1Key = "peppy.hosted-preview.checkpoint.v1"
    private let auth: any HostedPreviewAuthProvider
    private let purchase: any HostedPreviewPurchaseProvider
    private let service: any HostedPreviewService
    private let checkpointStore: any HostedPreviewCheckpointStore
    private var lastPurchase: HostedPreviewPurchase?
    private var generation = 0
    private var restorationRequested = false
    private var checkpointSeed: HostedPreviewCheckpointSeed

    private(set) var rawCheckpoint: String
    private(set) var account: HostedPreviewAccount?
    private(set) var snapshot: Snapshot
    private(set) var isWorking = false
    var generatedPassphrase = ""
    var customPassphrase = ""
    var usesCustomPassphrase = false
    var confirmationPassphrase = ""
    var acknowledgement = false
    var revealPassphrase = false
    var localError: String?

    init(
        auth: any HostedPreviewAuthProvider = FakeHostedPreviewAuthProvider(),
        purchase: any HostedPreviewPurchaseProvider = FakeHostedPreviewPurchaseProvider(),
        service: any HostedPreviewService = FakeHostedPreviewService(),
        checkpointStore: any HostedPreviewCheckpointStore = UserDefaultsHostedPreviewCheckpointStore(),
        checkpoint: String? = nil
    ) {
        self.auth = auth
        self.purchase = purchase
        self.service = service
        self.checkpointStore = checkpointStore
        let stored = checkpoint ?? checkpointStore.value(forKey: Self.checkpointV2Key) ?? checkpointStore.value(forKey: Self.checkpointV1Key)
        let raw = stored.map(hostedPreviewV2Resume) ?? hostedPreviewV2Start(scenario: "new")
        let validated = Self.decode(raw)
        checkpointSeed = Self.seed(from: validated)
        rawCheckpoint = raw
        snapshot = validated
        restorationRequested = stored != nil
        checkpointStore.set(raw, forKey: Self.checkpointV2Key)
    }

    static func initializeIfNeeded(_ current: HostedPreviewModel?, factory: () -> HostedPreviewModel = { HostedPreviewModel() }) -> HostedPreviewModel {
        current ?? factory()
    }

    var price: String { purchase.displayPrice }
    var hasResumableEntitlement: Bool { ["active", "grace", "billing_retry"].contains(snapshot.entitlement_state) }
    var storeUnavailable: Bool { snapshot.status_key == "hosted_subscribe_store_unavailable" }
    var isLookupError: Bool { snapshot.status_key == "hosted_account_check_failed" || storeUnavailable }

    func start(scenario: String = "new") {
        invalidateWork()
        clearSecrets()
        account = nil
        lastPurchase = nil
        localError = nil
        restorationRequested = false
        apply(hostedPreviewV2Start(scenario: scenario))
        checkpointSeed = seed(from: snapshot)
    }

    func backgrounded() {
        invalidateWork()
        clearSecrets()
        account = nil
        lastPurchase = nil
        localError = nil
        restorationRequested = true
        apply(hostedPreviewV2Resume(snapshot: rawCheckpoint))
    }

    func prepareForForeground() {
        if snapshot.screen == "purchase_pending" { advance("account_retry") }
    }

    func foregrounded() async {
        prepareForForeground()
        await automaticWork()
    }

    func advance(_ event: String) {
        if ["signout", "deletion_confirmed", "reset", "back", "cancel"].contains(event) {
            invalidateWork()
            clearSecrets()
        }
        if ["signout", "deletion_confirmed", "reset"].contains(event) {
            auth.signOut()
            account = nil
            lastPurchase = nil
            restorationRequested = false
        }
        localError = nil
        let previous = rawCheckpoint
        apply(hostedPreviewV2Advance(snapshot: rawCheckpoint, event: event))
        if rawCheckpoint != previous, !snapshot.rejected, isPositiveFixtureState(snapshot) {
            checkpointSeed = seed(from: snapshot)
        }
    }

    func signIn(provider: String = "preview") async {
        await work { token in
            let signedIn = try await self.auth.signIn(provider: provider)
            guard self.isCurrent(token) else { return }
            guard signedIn.provider == provider else {
                self.localError = "preview_error"
                return
            }
            self.account = signedIn
            self.advance("signed_in")
            try await self.lookup(token: token, account: signedIn)
        }
    }

    func subscribe() async {
        await work { token in
            let account = try self.requireAccount()
            let result = try await self.purchase.purchase(for: account)
            guard self.isCurrent(token) else { return }
            guard self.matches(result.accountID, result.provider, account) else { throw PreviewAdapterError.invalidAccount }
            self.lastPurchase = result
            self.advance("purchase_succeeded")
            self.advance("verify_entitlement")
            try await self.validate(result, account: account, token: token, generateSuggestion: true)
        }
    }

    func retryLookup() async { advance("account_retry"); await automaticWork() }
    func retryProvision() async { advance("provision_retry"); await automaticWork() }
    func retryPreparation() async { advance("sync_retry"); await automaticWork() }
    func retryVerification() async { localError = nil; await automaticWork() }

    func automaticWork() async {
        guard !isWorking else { return }
        while !Task.isCancelled {
            if snapshot.screen == "signin", restorationRequested {
                restorationRequested = false
                await work { token in
                    guard let restored = try await self.auth.restoreSession() else { return }
                    guard self.isCurrent(token) else { return }
                    self.account = restored
                    self.advance("session_restored")
                    try await self.lookup(token: token, account: restored)
                }
            } else if snapshot.screen == "checking_account", snapshot.status_key == nil {
                await work { token in try await self.lookup(token: token, account: try self.requireAccount()) }
            } else if snapshot.screen == "subscription_verifying", localError == nil {
                await verifyEntitlement()
            } else if snapshot.screen == "provisioning", snapshot.status_key == nil, localError == nil {
                await provision()
            } else if snapshot.screen == "syncing", snapshot.status_key == nil, localError == nil {
                await prepare()
            } else {
                return
            }
            if isWorking { return }
        }
    }

    func provision() async {
        await work { token in
            guard let id = self.snapshot.operation_id else { return }
            let account = try self.requireAccount()
            let result = try await self.service.provision(operationID: id, account: account)
            guard self.isCurrent(token) else { return }
            guard result.id == id, self.matches(result.accountID, result.provider, account) else { throw PreviewAdapterError.invalidOperation }
            self.advance("provision_finished")
        }
    }

    func prepare() async {
        await work { token in
            let account = try self.requireAccount()
            let result = try await self.service.prepare(account: account)
            guard self.isCurrent(token) else { return }
            guard self.matches(result.accountID, result.provider, account) else { throw PreviewAdapterError.invalidAccount }
            self.advance("sync_finished")
        }
    }

    func acceptPassphrase() {
        let value = customPassphrase.isEmpty ? generatedPassphrase : customPassphrase
        guard hostedPreviewPassphraseAcceptable(passphrase: value), acknowledgement else { localError = "passphrase_weak"; return }
        advance("local_passphrase_accepted")
    }

    func confirmPassphrase(unlock: Bool = false) {
        if unlock {
            guard !confirmationPassphrase.isEmpty else { localError = "passphrase_mismatch"; return }
        } else {
            let value = customPassphrase.isEmpty ? generatedPassphrase : customPassphrase
            guard !value.isEmpty, value == confirmationPassphrase else { localError = "passphrase_mismatch"; return }
        }
        clearSecrets()
        advance(unlock ? "unlock_succeeded" : "passphrase_confirmed")
    }

    func generatePassphrase() {
        generatedPassphrase = hostedPreviewPassphrase()
        customPassphrase = ""
        usesCustomPassphrase = false
        acknowledgement = false
        localError = nil
    }

    func chooseCustomPassphrase() {
        customPassphrase = ""
        generatedPassphrase = ""
        usesCustomPassphrase = true
        acknowledgement = false
        localError = nil
    }

    func clearSecrets() {
        generatedPassphrase = ""
        customPassphrase = ""
        usesCustomPassphrase = false
        confirmationPassphrase = ""
        acknowledgement = false
        revealPassphrase = false
    }

    func cancelWork() { invalidateWork(); clearSecrets() }

    private func verifyEntitlement() async {
        await work { token in
            let account = try self.requireAccount()
            let receipt: HostedPreviewPurchase
            if let lastPurchase = self.lastPurchase {
                receipt = lastPurchase
            } else if let recovered = try await self.service.recoverPurchase(account: account) {
                guard self.isCurrent(token) else { return }
                receipt = recovered
                self.lastPurchase = recovered
            } else {
                throw PreviewAdapterError.missingPurchase
            }
            guard self.matches(receipt.accountID, receipt.provider, account) else { throw PreviewAdapterError.invalidAccount }
            if self.snapshot.entitlement_state == "store_succeeded_unverified" { self.advance("verify_entitlement") }
            try await self.validate(receipt, account: account, token: token, generateSuggestion: false)
        }
    }

    private func validate(
        _ receipt: HostedPreviewPurchase,
        account: HostedPreviewAccount,
        token: Int,
        generateSuggestion: Bool
    ) async throws {
        let valid = try await service.validate(entitlement: receipt, account: account)
        guard isCurrent(token) else { return }
        advance(valid ? "entitlement_verified" : "entitlement_rejected")
        if valid, generateSuggestion { prepareNewPassphrase() }
    }

    private func lookup(token: Int, account: HostedPreviewAccount) async throws {
        let result = try await service.lookup(account: account, scenario: snapshot.scenario, checkpoint: checkpointSeed)
        guard isCurrent(token) else { return }
        guard matches(result.accountID, result.provider, account) else { throw PreviewAdapterError.invalidAccount }
        advance(result.event)
    }

    private func requireAccount() throws -> HostedPreviewAccount {
        guard let account else { throw PreviewAdapterError.missingAccount }
        return account
    }

    private func matches(_ id: String, _ provider: String, _ account: HostedPreviewAccount) -> Bool {
        id == account.id && provider == account.provider
    }

    private func invalidateWork() { generation &+= 1; isWorking = false }

    private func apply(_ raw: String) {
        rawCheckpoint = raw
        snapshot = Self.decode(raw)
        checkpointStore.set(raw, forKey: Self.checkpointV2Key)
    }

    private func prepareNewPassphrase() {
        if snapshot.screen == "passphrase", !usesCustomPassphrase, generatedPassphrase.isEmpty { generatePassphrase() }
    }

    private func isCurrent(_ token: Int) -> Bool { token == generation && !Task.isCancelled }

    private func work(_ body: @escaping (Int) async throws -> Void) async {
        guard !isWorking else { return }
        let token = generation
        isWorking = true
        localError = nil
        defer { if token == generation { isWorking = false } }
        do {
            try Task.checkCancellation()
            try await body(token)
            try Task.checkCancellation()
        } catch is CancellationError {
        } catch {
            guard token == generation else { return }
            if snapshot.screen == "provisioning" { advance("provision_failed") }
            else if snapshot.screen == "syncing" { advance("sync_failed") }
            else if snapshot.screen == "checking_account" { advance("account_lookup_failed") }
            else { localError = "preview_error" }
        }
    }

    private static func seed(from snapshot: Snapshot) -> HostedPreviewCheckpointSeed {
        let classifiedStates = ["new_account", "incomplete", "existing"]
        let accountState: String
        if classifiedStates.contains(snapshot.account_state) {
            accountState = snapshot.account_state
        } else if snapshot.account_state == "signed_in", let hint = snapshot.resume_account_state,
                  classifiedStates.contains(hint) {
            accountState = hint
        } else if snapshot.account_state == "signed_in", let resumeScreen = snapshot.resume_screen,
                  ["passphrase", "provisioning"].contains(resumeScreen) {
            accountState = "incomplete"
        } else if snapshot.account_state == "signed_in", let resumeScreen = snapshot.resume_screen,
                  ["join", "unlock"].contains(resumeScreen) {
            accountState = "existing"
        } else {
            accountState = snapshot.account_state
        }
        return .init(accountState: accountState, entitlementState: snapshot.entitlement_state, operationID: snapshot.operation_id)
    }

    private func seed(from snapshot: Snapshot) -> HostedPreviewCheckpointSeed { Self.seed(from: snapshot) }

    private func isPositiveFixtureState(_ snapshot: Snapshot) -> Bool {
        ["new_account", "incomplete", "existing"].contains(snapshot.account_state)
    }

    private static func decode(_ raw: String) -> Snapshot {
        (try? JSONDecoder().decode(Snapshot.self, from: Data(raw.utf8)))
            ?? Snapshot(version: 2, scenario: "new", screen: "welcome", account_state: "anonymous", entitlement_state: "none", provider: "preview", operation_id: nil, status_key: "preview_error", approval_state: "not_requested", unlocked: false, rejected: true, resume_screen: nil, resume_account_state: nil)
    }
}

@MainActor
final class HostedPreviewAutomaticWorkDriver {
    private let model: HostedPreviewModel
    private var isForeground = false
    private var pending = false
    private var task: Task<Void, Never>?
    private var activeRunID: UUID?

    init(model: HostedPreviewModel) { self.model = model }

    func enterForeground(resuming: Bool) async {
        isForeground = true
        if resuming { model.prepareForForeground() }
        await schedule()
    }

    func enterBackground() {
        isForeground = false
        pending = false
        activeRunID = nil
        task?.cancel()
        task = nil
        model.backgrounded()
    }

    func requestSchedule() {
        guard isForeground else { return }
        pending = true
        guard task == nil else { return }
        let runID = UUID()
        activeRunID = runID
        task = Task { [weak self] in
            guard let self else { return }
            await self.drain()
            self.finished(runID: runID)
        }
    }

    func schedule() async {
        requestSchedule()
        let running = task
        await running?.value
    }

    func cancelCurrentWork() {
        pending = false
        activeRunID = nil
        task?.cancel()
        task = nil
        model.cancelWork()
    }

    func cancel() {
        isForeground = false
        cancelCurrentWork()
    }

    private func drain() async {
        while isForeground, pending, !Task.isCancelled {
            pending = false
            await model.automaticWork()
        }
    }

    private func finished(runID: UUID) {
        guard activeRunID == runID else { return }
        task = nil
        activeRunID = nil
    }
}

private enum PreviewAdapterError: Error { case missingAccount, missingPurchase, invalidAccount, invalidOperation }
