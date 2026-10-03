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
    /// Contact sync settings and last pass, shown by `ContactsSection`.
    var contacts = ContactsState()

    let session: NativeSession
    let contactsPreferences = UserDefaultsContactsPreferences()
    let contactsProvider: any ContactsProvider = CNContactStoreProvider()
    /// One contact pass at a time across foreground, change notifications and BackgroundTasks.
    @ObservationIgnored private(set) var contactPasses: ContactsPassCoordinator!
    #if os(iOS)
    /// Retained for the process lifetime; BackgroundTasks hold no other reference.
    @ObservationIgnored private var contactsScheduler: ContactsBackgroundScheduler?
    #endif
    @ObservationIgnored var contactsObserver: (any NSObjectProtocol)?

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
        let session = self.session
        let provider = contactsProvider
        let preferences = contactsPreferences
        contactPasses = ContactsPassCoordinator { [weak self] reason, isCancelled in
            var budget = SyncBudget()
            if reason == .backgroundRefresh {
                // BGAppRefresh grants about 25 seconds; keep the network share small.
                budget.maxRequests = 8
                budget.maxUploads = 4
                budget.maxApplyRounds = 4
            }
            let report = try await session.contactsPass(
                reason: reason, provider: provider, preferences: preferences, budget: budget, isCancelled: isCancelled
            )
            await self?.contactsPassFinished(report)
            return [.completed, .unchanged, .incremental].contains(report.outcome)
        }
        contacts.enabled = preferences.load().enabled
    }

    /// Must run during launch, before the app finishes launching (BackgroundTasks requirement).
    func registerBackgroundTasks() {
        #if os(iOS)
        let scheduler = ContactsBackgroundScheduler(coordinator: contactPasses)
        scheduler.register()
        contactsScheduler = scheduler
        if contacts.enabled { scheduler.scheduleAll() }
        #endif
    }

    func scheduleContactsBackgroundWork() {
        #if os(iOS)
        contactsScheduler?.scheduleAll()
        #endif
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
        } catch ClientError.syncInProgress {
            // A contact pass holds the sync guard; it uploads and receives too.
        } catch is CancellationError {
        } catch {
            lastError = (error as? ClientError)?.userMessage ?? "Sync failed."
        }
        status = (try? await session.open()) ?? status
        await runContactsPass(.foreground)
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
