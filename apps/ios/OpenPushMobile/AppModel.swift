import Foundation
import Observation
#if canImport(OpenPushNative)
import OpenPushNative
#endif

/// Main-actor UI state over the `NativeSession` actor. Holds no secrets: the passphrase lives
/// only in the SecureField binding until it is handed to the core once.
@MainActor @Observable
final class AppModel {
    enum Activity: Equatable { case idle, importing, unlocking, syncing, refreshing, disconnecting }

    private(set) var status = SessionStatus()
    private(set) var activity = Activity.idle
    private(set) var lastError: String?
    private(set) var lastSync: SyncReport?
    private(set) var lastSyncDate: Date?
    /// The active enrollment cannot be opened (e.g. its database is gone after a reinstall);
    /// disconnecting is the way out.
    private(set) var enrollmentBlocked = false
    let telephony = TelephonyEligibility.evaluate()

    private let session: NativeSession

    init() {
        #if DEBUG
        let allowLoopbackHTTP = true
        #else
        let allowLoopbackHTTP = false
        #endif
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        session = NativeSession(
            secureStore: KeychainSecureStore(),
            transport: URLSessionTransport(),
            databaseDirectory: support.appendingPathComponent("OpenPush", isDirectory: true),
            allowLoopbackHTTP: allowLoopbackHTTP
        )
    }

    func load() async {
        await run(.idle) { try await $0.open() }
    }

    func importCredential(from result: Result<URL, any Error>) async {
        guard case .success(let url) = result else {
            lastError = "The credential file could not be selected."
            return
        }
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        await run(.importing) { try await $0.importCredential(file: url) }
    }

    func refresh() async {
        await run(.refreshing) { try await $0.refreshEnrollment() }
    }

    /// Non-destructive: the encrypted database, keys and queued work stay archived on this device.
    func disconnect() async {
        await run(.disconnecting) { try await $0.disconnect() }
        lastSync = nil
        lastSyncDate = nil
    }

    func unlock(passphrase: String) async {
        await run(.unlocking) { try await $0.unlock(passphrase: passphrase) }
    }

    /// One bounded pass while the app is in the foreground. SwiftUI cancels the calling task when
    /// the scene leaves the foreground; there is no background or long-lived connection.
    func syncInForeground() async {
        guard status.databaseOpen, status.keysUnlocked, activity == .idle else { return }
        activity = .syncing
        defer { activity = .idle }
        do {
            let report = try await session.syncOnce()
            lastSync = report
            lastSyncDate = Date()
            lastError = report.uploadRefused.map { "Upload refused; the message stays queued. \($0.userMessage)" }
        } catch ClientError.canceled {
            // Leaving the foreground is not a failure.
        } catch is CancellationError {
        } catch {
            lastError = (error as? ClientError)?.userMessage ?? "Sync failed."
        }
        status = (try? await session.open()) ?? status
    }

    private func run(_ activity: Activity, _ body: (NativeSession) async throws -> SessionStatus) async {
        self.activity = activity
        defer { self.activity = .idle }
        do {
            status = try await body(session)
            lastError = nil
            enrollmentBlocked = false
        } catch {
            let failure = error as? ClientError
            lastError = failure?.userMessage ?? "Unexpected error."
            switch failure {
            case .localDatabaseMissing, .missingSecret, .identityMismatch: enrollmentBlocked = true
            default: break
            }
        }
    }
}
