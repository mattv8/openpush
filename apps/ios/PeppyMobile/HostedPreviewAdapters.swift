import Foundation

/// Preview seams exchange only simulated account, purchase, and operation facts.
/// They never receive a passphrase or a real enrollment credential.
@MainActor protocol HostedPreviewAuthProvider { func signIn() async throws -> HostedPreviewAccount }
@MainActor protocol HostedPreviewPurchaseProvider {
    func purchase(for account: HostedPreviewAccount) async throws -> HostedPreviewPurchase
    func restore(for account: HostedPreviewAccount) async throws -> HostedPreviewPurchase
    var displayPrice: String { get }
}
@MainActor protocol HostedPreviewService {
    func validate(entitlement purchase: HostedPreviewPurchase, account: HostedPreviewAccount) async throws -> Bool
    func provision(operationID: String, account: HostedPreviewAccount) async throws -> HostedPreviewOperation
}

struct HostedPreviewAccount: Sendable, Equatable { let id: String; let provider: String }
struct HostedPreviewPurchase: Sendable, Equatable { let accountID: String; let provider: String; let displayPrice: String; let restored: Bool }
struct HostedPreviewOperation: Sendable, Equatable { let id: String; let accountID: String }

struct FakeHostedPreviewAuthProvider: HostedPreviewAuthProvider {
    func signIn() async throws -> HostedPreviewAccount { .init(id: "preview-account", provider: "preview") }
}

struct FakeHostedPreviewPurchaseProvider: HostedPreviewPurchaseProvider {
    let displayPrice = "$4.99/month · Preview price"
    func purchase(for account: HostedPreviewAccount) async throws -> HostedPreviewPurchase {
        try await Task.sleep(for: .milliseconds(250)); try Task.checkCancellation()
        return .init(accountID: account.id, provider: account.provider, displayPrice: displayPrice, restored: false)
    }
    func restore(for account: HostedPreviewAccount) async throws -> HostedPreviewPurchase {
        try await Task.sleep(for: .milliseconds(150)); try Task.checkCancellation()
        return .init(accountID: account.id, provider: account.provider, displayPrice: displayPrice, restored: true)
    }
}

struct FakeHostedPreviewService: HostedPreviewService {
    func validate(entitlement purchase: HostedPreviewPurchase, account: HostedPreviewAccount) async throws -> Bool {
        try Task.checkCancellation()
        return purchase.accountID == account.id && purchase.provider == account.provider
    }
    func provision(operationID: String, account: HostedPreviewAccount) async throws -> HostedPreviewOperation {
        try await Task.sleep(for: .milliseconds(350)); try Task.checkCancellation()
        return .init(id: operationID, accountID: account.id)
    }
}
