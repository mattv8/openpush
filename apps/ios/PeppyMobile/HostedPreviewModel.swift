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
    }

    private static let checkpointKey = "peppy.hosted-preview.checkpoint.v1"
    private let auth: any HostedPreviewAuthProvider
    private let purchase: any HostedPreviewPurchaseProvider
    private let service: any HostedPreviewService
    private var lastPurchase: HostedPreviewPurchase?
    private(set) var rawCheckpoint: String
    private(set) var account: HostedPreviewAccount?
    private var generation = 0
    private(set) var snapshot: Snapshot
    private(set) var isWorking = false
    var generatedPassphrase = ""
    var customPassphrase = ""
    var usesCustomPassphrase = false
    var confirmationPassphrase = ""
    var acknowledgement = false
    var revealPassphrase = false
    var localError: String?

    init(auth: any HostedPreviewAuthProvider = FakeHostedPreviewAuthProvider(), purchase: any HostedPreviewPurchaseProvider = FakeHostedPreviewPurchaseProvider(), service: any HostedPreviewService = FakeHostedPreviewService(), checkpoint: String? = UserDefaults.standard.string(forKey: checkpointKey)) {
        self.auth = auth; self.purchase = purchase; self.service = service
        let raw = checkpoint.map(hostedPreviewResume) ?? hostedPreviewStart(scenario: "new")
        rawCheckpoint = raw
        snapshot = Self.decode(raw)
        persist(raw)
        if ["signed_in", "new_account", "existing"].contains(snapshot.account_state) { account = HostedPreviewAccount(id: "preview-account", provider: "preview") }
    }

    var price: String { purchase.displayPrice }
    var hasResumableEntitlement: Bool { ["active", "grace", "billing_retry"].contains(snapshot.entitlement_state) }
    var storeUnavailable: Bool { snapshot.status_key == "hosted_subscribe_store_unavailable" }
    func start(scenario: String = "new") { invalidateWork(); clearSecrets(); account = nil; lastPurchase = nil; localError = nil; apply(hostedPreviewStart(scenario: scenario)) }
    func reset() { advance("reset") }
    func resume() { invalidateWork(); clearSecrets(); localError = nil; apply(hostedPreviewResume(snapshot: rawCheckpoint)) }
    func advance(_ event: String) {
        if ["signout", "deletion_confirmed", "reset", "back", "cancel"].contains(event) {
            invalidateWork()
            clearSecrets()
        }
        if ["signout", "deletion_confirmed", "reset"].contains(event) {
            account = nil
            lastPurchase = nil
        }
        localError = nil
        apply(hostedPreviewAdvance(snapshot: rawCheckpoint, event: event))
    }

    func signIn() async {
        await perform { token in
            let account = try await self.auth.signIn()
            guard self.isCurrent(token) else { return }
            self.account = account
            self.advance("signed_in")
        }
    }
    func subscribe() async { await completePurchase(restoring: false) }
    func restore() async { await completePurchase(restoring: true) }
    private func completePurchase(restoring: Bool) async {
        guard !storeUnavailable else { return }
        await perform { token in
            let account = try self.requireAccount()
            if self.hasResumableEntitlement {
                self.advance("restore_succeeded")
                self.prepareNewPassphrase()
                return
            }
            let result = try await (restoring ? self.purchase.restore(for: account) : self.purchase.purchase(for: account))
            guard self.isCurrent(token) else { return }
            guard result.accountID == account.id, result.provider == account.provider else { throw PreviewAdapterError.invalidAccount }
            self.lastPurchase = result
            self.advance(restoring ? "restore_succeeded" : "purchase_succeeded")
            try await self.verifyPurchase(token: token, account: account, result: result)
        }
    }

    private func verifyPurchase(token: Int, account: HostedPreviewAccount, result: HostedPreviewPurchase) async throws {
        guard result.accountID == account.id, result.provider == account.provider else { throw PreviewAdapterError.invalidAccount }
        if snapshot.entitlement_state == "store_succeeded_unverified" { advance("verify_entitlement") }
        let valid = try await service.validate(entitlement: result, account: account)
        guard isCurrent(token) else { return }
        advance(valid ? "entitlement_verified" : "entitlement_rejected")
        if valid { prepareNewPassphrase() }
    }

    func automaticWork() async {
        guard !isWorking else { return }
        if snapshot.screen == "provisioning", snapshot.status_key == nil, localError == nil {
            await provision()
        } else if snapshot.screen == "subscription_verifying" {
            await perform { token in
                let account = try self.requireAccount()
                let result: HostedPreviewPurchase
                if let saved = self.lastPurchase { result = saved }
                else { result = try await self.purchase.restore(for: account) }
                guard self.isCurrent(token) else { return }
                try await self.verifyPurchase(token: token, account: account, result: result)
            }
        }
    }

    func retryProvision() async {
        advance("provision_retry")
        await provision()
    }

    func beginRenewal() {
        advance("resubscribe")
    }

    func provision() async {
        await perform { token in
            guard let id = self.snapshot.operation_id else { return }
            let account = try self.requireAccount()
            let result = try await self.service.provision(operationID: id, account: account)
            guard self.isCurrent(token) else { return }
            guard result.id == id, result.accountID == account.id else { throw PreviewAdapterError.invalidOperation }
            self.advance("provision_finished")
        }
    }
    func acceptPassphrase() {
        let value = customPassphrase.isEmpty ? generatedPassphrase : customPassphrase
        guard hostedPreviewPassphraseAcceptable(passphrase: value), acknowledgement else { localError = "passphrase_weak"; return }
        localError = nil; advance("local_passphrase_accepted")
    }
    func confirmPassphrase(unlock: Bool = false) {
        if unlock {
            // Returning-device proof is deliberately local simulation only; no prior create value is required.
            guard !confirmationPassphrase.isEmpty else { localError = "passphrase_mismatch"; return }
        } else {
            let value = customPassphrase.isEmpty ? generatedPassphrase : customPassphrase
            guard !value.isEmpty, value == confirmationPassphrase else { localError = "passphrase_mismatch"; return }
        }
        localError = nil; clearSecrets(); advance("passphrase_confirmed")
    }
    func generatePassphrase() { generatedPassphrase = hostedPreviewPassphrase(); customPassphrase = ""; usesCustomPassphrase = false; acknowledgement = false; localError = nil }
    func chooseCustomPassphrase() { customPassphrase = ""; generatedPassphrase = ""; usesCustomPassphrase = true; acknowledgement = false; localError = nil }
    func clearSecrets() { generatedPassphrase = ""; customPassphrase = ""; usesCustomPassphrase = false; confirmationPassphrase = ""; acknowledgement = false; revealPassphrase = false }
    func cancelWork() { invalidateWork(); clearSecrets() }

    private func requireAccount() throws -> HostedPreviewAccount { guard let account else { throw PreviewAdapterError.missingAccount }; return account }
    private func invalidateWork() { generation &+= 1; isWorking = false }
    private func apply(_ raw: String) {
        rawCheckpoint = raw // Preserve core JSON verbatim: optional null fields are contract-significant.
        snapshot = Self.decode(raw)
        persist(raw)
    }
    /// Only an explicit new/continued setup action may create a suggestion, never resume/background.
    private func prepareNewPassphrase() {
        if snapshot.screen == "passphrase", !usesCustomPassphrase, generatedPassphrase.isEmpty { generatePassphrase() }
    }
    private func persist(_ raw: String) { UserDefaults.standard.set(raw, forKey: Self.checkpointKey) }
    private func isCurrent(_ token: Int) -> Bool { token == generation && !Task.isCancelled }
    private func perform(_ body: @escaping (Int) async throws -> Void) async {
        guard !isWorking else { return }
        let token = generation; isWorking = true; localError = nil; defer { if token == generation { isWorking = false } }
        do { try Task.checkCancellation(); try await body(token); try Task.checkCancellation() }
        catch is CancellationError { }
        catch {
            if token == generation {
                if snapshot.screen == "provisioning" { advance("provision_failed") }
                else { localError = "preview_error" }
            }
        }
    }
    private static func decode(_ raw: String) -> Snapshot {
        (try? JSONDecoder().decode(Snapshot.self, from: Data(raw.utf8))) ?? Snapshot(version: 1, scenario: "new", screen: "welcome", account_state: "anonymous", entitlement_state: "none", provider: "preview", operation_id: nil, status_key: "preview_error", approval_state: "not_requested", unlocked: false, rejected: true)
    }
}

private enum PreviewAdapterError: Error { case missingAccount, invalidAccount, invalidOperation }
