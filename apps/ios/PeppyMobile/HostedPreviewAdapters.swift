import Foundation

@MainActor protocol HostedPreviewCheckpointStore {
    func value(forKey key: String) -> String?
    func set(_ value: String, forKey key: String)
}

@MainActor struct UserDefaultsHostedPreviewCheckpointStore: HostedPreviewCheckpointStore {
    func value(forKey key: String) -> String? { UserDefaults.standard.string(forKey: key) }
    func set(_ value: String, forKey key: String) { UserDefaults.standard.set(value, forKey: key) }
}

/// Preview seams carry only simulated, account-bound facts. They never accept
/// passphrases, credentials, receipts, or production service data.
@MainActor protocol HostedPreviewAuthProvider {
    func signIn(provider: String) async throws -> HostedPreviewAccount
    func restoreSession() async throws -> HostedPreviewAccount?
    func signOut()
}

@MainActor protocol HostedPreviewPurchaseProvider {
    func purchase(for account: HostedPreviewAccount) async throws -> HostedPreviewPurchase
    var displayPrice: String { get }
}

@MainActor protocol HostedPreviewService {
    func lookup(account: HostedPreviewAccount, scenario: String, checkpoint: HostedPreviewCheckpointSeed) async throws -> HostedPreviewLookup
    func recoverPurchase(account: HostedPreviewAccount) async throws -> HostedPreviewPurchase?
    func validate(entitlement purchase: HostedPreviewPurchase, account: HostedPreviewAccount) async throws -> Bool
    func provision(operationID: String, account: HostedPreviewAccount) async throws -> HostedPreviewOperation
    func prepare(account: HostedPreviewAccount) async throws -> HostedPreviewPreparation
}

struct HostedPreviewAccount: Sendable, Equatable { let id: String; let provider: String }
struct HostedPreviewPurchase: Sendable, Equatable { let accountID: String; let provider: String; let displayPrice: String }
struct HostedPreviewOperation: Sendable, Equatable { let id: String; let accountID: String; let provider: String }
struct HostedPreviewPreparation: Sendable, Equatable { let accountID: String; let provider: String }
struct HostedPreviewCheckpointSeed: Sendable, Equatable {
    let accountState: String
    let entitlementState: String
    let operationID: String?
}
struct HostedPreviewLookup: Sendable, Equatable { let accountID: String; let provider: String; let event: String }

final class FakeHostedPreviewAuthProvider: HostedPreviewAuthProvider {
    private var session: HostedPreviewAccount?

    init(session: HostedPreviewAccount? = nil) { self.session = session }

    func signIn(provider: String) async throws -> HostedPreviewAccount {
        let account = HostedPreviewAccount(id: "preview-\(provider)-account", provider: provider)
        session = account
        return account
    }

    func restoreSession() async throws -> HostedPreviewAccount? { session }
    func signOut() { session = nil }
}

struct FakeHostedPreviewPurchaseProvider: HostedPreviewPurchaseProvider {
    let displayPrice = "$4.99/month - Preview price"
    func purchase(for account: HostedPreviewAccount) async throws -> HostedPreviewPurchase {
        try await Task.sleep(for: .milliseconds(25))
        try Task.checkCancellation()
        return .init(accountID: account.id, provider: account.provider, displayPrice: displayPrice)
    }
}

struct FakeHostedPreviewService: HostedPreviewService {
    func lookup(account: HostedPreviewAccount, scenario: String, checkpoint: HostedPreviewCheckpointSeed) async throws -> HostedPreviewLookup {
        try Task.checkCancellation()
        let classification = checkpoint.accountState
        let event: String
        switch checkpoint.entitlementState {
        case "store_succeeded_unverified", "verifying":
            event = classification == "existing"
                ? "account_existing_verification_pending"
                : classification == "new_account"
                    ? "account_verification_pending"
                    : "account_lookup_failed"
        case "store_pending":
            event = classification == "existing"
                ? "account_existing_pending"
                : classification == "new_account"
                    ? "account_pending"
                    : "account_lookup_failed"
        case "active", "grace", "billing_retry":
            if classification == "existing" {
                event = checkpoint.entitlementState == "grace"
                    ? "account_existing_grace"
                    : checkpoint.entitlementState == "billing_retry"
                        ? "account_existing_billing_retry"
                        : "account_existing_active"
            } else if ["new_account", "incomplete"].contains(classification) {
                if checkpoint.operationID != nil {
                    event = checkpoint.entitlementState == "grace"
                        ? "account_incomplete_provisioning_grace"
                        : checkpoint.entitlementState == "billing_retry"
                            ? "account_incomplete_provisioning_billing_retry"
                            : "account_incomplete_provisioning"
                } else {
                    event = checkpoint.entitlementState == "grace"
                        ? "account_incomplete_grace"
                        : checkpoint.entitlementState == "billing_retry"
                            ? "account_incomplete_billing_retry"
                            : "account_incomplete_active"
                }
            } else {
                event = "account_lookup_failed"
            }
        case "unavailable":
            event = classification == "existing"
                ? "account_existing_store_unavailable"
                : classification == "new_account"
                    ? "account_store_unavailable"
                    : classification == "incomplete"
                        ? "account_incomplete_store_unavailable"
                        : "account_lookup_failed"
        case "expired", "revoked":
            event = classification == "existing"
                ? checkpoint.entitlementState == "revoked" ? "account_lapsed_revoked" : "account_lapsed"
                : "account_lookup_failed"
        case "none" where classification == "existing":
            event = "account_lapsed"
        case "none" where classification == "new_account":
            event = "account_new"
        case "none" where classification == "anonymous":
            event = switch scenario {
            case "returning", "approval_denied": "account_existing_active"
            case "lapsed": "account_lapsed"
            case "pending": "account_pending"
            case "store_unavailable": "account_store_unavailable"
            case "provision_retry": "account_incomplete_provisioning"
            default: "account_new"
            }
        default:
            event = "account_lookup_failed"
        }
        return .init(accountID: account.id, provider: account.provider, event: event)
    }

    /// Preview-only synthetic receipt recovery. The account-bound lookup fact is
    /// the authority for this fake; no production receipt is reconstructed here.
    func recoverPurchase(account: HostedPreviewAccount) async throws -> HostedPreviewPurchase? {
        try Task.checkCancellation()
        return .init(accountID: account.id, provider: account.provider, displayPrice: "$4.99/month - Preview price")
    }

    func validate(entitlement purchase: HostedPreviewPurchase, account: HostedPreviewAccount) async throws -> Bool {
        try Task.checkCancellation()
        return purchase.accountID == account.id && purchase.provider == account.provider
    }

    func provision(operationID: String, account: HostedPreviewAccount) async throws -> HostedPreviewOperation {
        try await Task.sleep(for: .milliseconds(25))
        try Task.checkCancellation()
        return .init(id: operationID, accountID: account.id, provider: account.provider)
    }

    func prepare(account: HostedPreviewAccount) async throws -> HostedPreviewPreparation {
        try await Task.sleep(for: .milliseconds(25))
        try Task.checkCancellation()
        return .init(accountID: account.id, provider: account.provider)
    }
}
