import Foundation
#if canImport(OpenPushBindings)
import OpenPushBindings
#endif

/// What the UI may show. Opening the encrypted database does not imply the vault keys are unlocked.
public struct SessionStatus: Equatable, Sendable {
    public var identity: EnrollmentIdentity?
    public var role: String?
    public var keyEpoch: UInt32?
    public var databaseOpen = false
    public var keysUnlocked = false
    /// Stored key caches that the core rejected while reopening (kept, not deleted).
    public var rejectedKeyCaches = 0
    /// Nil when the core could not list conversations.
    public var conversations: Int?
}

/// Owns the single process-wide `NativeClient`, its Keychain-held secrets and foreground sync.
/// All message state lives in the Rust core; this actor only moves opaque values.
///
/// Enrollment changes (import, refresh, disconnect) are serialized: one runs at a time, and every
/// identity decision is re-checked on the actor after the network await, right before it is saved.
public actor NativeSession {
    private let store: EnrollmentStore
    private let transport: any HTTPTransport
    private let databaseDirectory: URL
    private let allowLoopbackHTTP: Bool
    private var client: NativeClient?
    private var record: EnrollmentRecord?
    private var status = SessionStatus()
    private var syncing = false
    private var changingEnrollment = false

    public init(
        secureStore: any SecureStore,
        transport: any HTTPTransport,
        databaseDirectory: URL,
        allowLoopbackHTTP: Bool
    ) {
        store = EnrollmentStore(secure: secureStore)
        self.transport = transport
        self.databaseDirectory = databaseDirectory
        self.allowLoopbackHTTP = allowLoopbackHTTP
    }

    // MARK: Enrollment

    /// Reads a credential file (bounded), verifies it against its server and persists it.
    public func importCredential(file: URL) async throws -> SessionStatus {
        let data: Data
        do {
            let handle = try FileHandle(forReadingFrom: file)
            defer { try? handle.close() }
            data = try handle.read(upToCount: DeviceCredential.maxFileBytes + 1) ?? Data()
        } catch {
            throw ClientError.invalidCredential("unreadable file")
        }
        return try await importCredential(data)
    }

    /// Imports a credential for a new enrollment, or replaces the token of the active one
    /// (same origin, vault and device). A different active enrollment must be disconnected first.
    public func importCredential(_ data: Data) async throws -> SessionStatus {
        let credential = try DeviceCredential.parse(data, allowLoopbackHTTP: allowLoopbackHTTP)
        return try await changeEnrollment {
            try self.requireAdoptable(credential.identity)
            let verified = try await EnrollmentVerifier(transport: self.transport)
                .verify(origin: credential.origin, token: credential.token, identity: credential.identity)
            // Re-checked after the await: another change may have completed meanwhile.
            try self.requireAdoptable(credential.identity)
            try self.store.activate(verified, token: credential.token)
            return try self.adopt(verified)
        }
    }

    /// Re-reads the active enrollment's profile, epoch and role from the server with the stored token.
    public func refreshEnrollment() async throws -> SessionStatus {
        try await changeEnrollment {
            guard let identity = try self.store.activeIdentity() else { throw ClientError.notEnrolled }
            let verified = try await EnrollmentVerifier(transport: self.transport).verify(
                origin: try ServerOrigin(canonical: identity.origin, allowLoopbackHTTP: self.allowLoopbackHTTP),
                token: try self.store.token(identity),
                identity: identity
            )
            guard try self.store.activeIdentity() == identity else { throw ClientError.identityMismatch }
            try self.store.saveRecord(verified)
            return try self.adopt(verified)
        }
    }

    /// Stops using the active enrollment. Nothing is deleted: the encrypted database, its key,
    /// key caches, token and unacknowledged outbox stay archived under that identity, and
    /// re-importing the same credential reopens them.
    public func disconnect() async throws -> SessionStatus {
        try await changeEnrollment {
            self.close()
            try self.store.deactivate()
            return self.status
        }
    }

    private func changeEnrollment(_ body: () async throws -> SessionStatus) async throws -> SessionStatus {
        guard !changingEnrollment else { throw ClientError.enrollmentChangeInProgress }
        changingEnrollment = true
        defer { changingEnrollment = false }
        do { return try await body() } catch { throw ClientError.wrap(error) }
    }

    /// The identity may become (or stay) active: nothing else is active and its database, if one
    /// was ever created, still exists.
    private func requireAdoptable(_ identity: EnrollmentIdentity) throws {
        if let active = try store.activeIdentity(), active != identity { throw ClientError.alreadyEnrolled }
        if let open = record?.identity, open != identity { throw ClientError.alreadyEnrolled }
        if try store.databaseWasCreated(identity), !databaseExists(identity) { throw ClientError.localDatabaseMissing }
    }

    /// Applies a verified record to the open client, or opens it.
    private func adopt(_ verified: EnrollmentRecord) throws -> SessionStatus {
        guard client != nil, record?.identity == verified.identity else { return try open() }
        let epochChanged = record?.keyEpoch != verified.keyEpoch
        record = verified
        status.role = verified.role
        status.keyEpoch = verified.keyEpoch
        if epochChanged {
            // Keys for a new epoch come only from its cache or from unlocking with the passphrase.
            status.keysUnlocked = false
            if let cache = try store.keyCache(verified.identity, epoch: verified.keyEpoch) {
                status.keysUnlocked = (try? client?.importNativeKeyCacheFromNativeStorage(bytes: cache)) != nil
            }
        }
        refreshCount()
        return status
    }

    // MARK: Database and keys

    /// Opens the active enrollment's database with its Keychain key and restores stored key caches.
    @discardableResult
    public func open() throws -> SessionStatus {
        do {
            guard let identity = try store.activeIdentity() else {
                close()
                return status
            }
            if let open = record?.identity, open != identity { throw ClientError.identityMismatch }
            let record = try store.record(identity)
            if client == nil {
                client = try openClient(identity)
                status = SessionStatus(identity: identity, databaseOpen: true)
                for (epoch, cache) in try store.keyCaches(identity, through: record.keyEpoch) {
                    do {
                        try client?.importNativeKeyCacheFromNativeStorage(bytes: cache)
                        if epoch == record.keyEpoch { status.keysUnlocked = true }
                    } catch {
                        status.rejectedKeyCaches += 1
                    }
                }
            }
            self.record = record
            status.role = record.role
            status.keyEpoch = record.keyEpoch
            refreshCount()
            return status
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// Unlocks with the vault's existing shared passphrase. The passphrase is passed to the core
    /// once and never stored; only the core's opaque key cache is kept in the Keychain.
    ///
    /// Only this manual unlock activates the server-verified epoch of the current record (a no-op
    /// when it is already active). Import, refresh and key-cache restoration never activate an epoch.
    public func unlock(passphrase: String) throws -> SessionStatus {
        guard let client, let record else { throw ClientError.notEnrolled }
        do {
            try client.unlock(profileJson: record.profileJson, headerJson: record.headerJson, passphrase: passphrase)
            try client.activateVerifiedEpoch(epoch: record.keyEpoch)
            let cache = try client.exportNativeKeyCacheForNativeStorage(epoch: record.keyEpoch)
            try store.saveKeyCache(cache, epoch: record.keyEpoch, for: record.identity)
        } catch {
            throw ClientError.wrap(error)
        }
        status.keysUnlocked = true
        refreshCount()
        return status
    }

    // MARK: Sync

    /// One bounded foreground pass. Cancel the calling task to stop it.
    public func syncOnce(budget: SyncBudget = SyncBudget()) async throws -> SyncReport {
        guard let client, let record else { throw ClientError.notEnrolled }
        guard !syncing else { throw ClientError.syncInProgress }
        syncing = true
        defer { syncing = false }
        let identity = record.identity
        do {
            let server = ServerClient(
                origin: try ServerOrigin(canonical: identity.origin, allowLoopbackHTTP: allowLoopbackHTTP),
                token: try store.token(identity),
                transport: transport
            )
            let report = try await ForegroundSync(client: client, server: server, vaultId: identity.vaultId, budget: budget).run()
            refreshCount()
            return report
        } catch {
            throw ClientError.wrap(error)
        }
    }

    // MARK: Contacts

    /// One bounded contact pass for foreground, change notifications and BackgroundTasks. It shares
    /// the sync guard, so it never overlaps a message sync. A background relaunch reopens the cached
    /// enrollment and key caches; locked keys (including a new epoch awaiting its passphrase) return
    /// `.needsUnlock` without capturing anything. Incoming records are applied before any permit.
    public func contactsPass(
        reason: ContactsPassReason,
        provider: any ContactsProvider,
        preferences: any ContactsPreferencesStore,
        budget: SyncBudget = SyncBudget(),
        isCancelled: @escaping @Sendable () -> Bool
    ) async throws -> ContactsSyncReport {
        guard preferences.load().enabled else { return ContactsSyncReport(outcome: .disabled) }
        if client == nil { _ = try open() }
        guard let client, let record else { throw ClientError.notEnrolled }
        guard status.keysUnlocked else { return ContactsSyncReport(outcome: .needsUnlock) }
        guard !syncing else { return ContactsSyncReport(outcome: .busy) }
        syncing = true
        defer { syncing = false }
        let identity = record.identity
        do {
            let server = ServerClient(
                origin: try ServerOrigin(canonical: identity.origin, allowLoopbackHTTP: allowLoopbackHTTP),
                token: try store.token(identity),
                transport: transport
            )
            let sync = ForegroundSync(client: client, server: server, vaultId: identity.vaultId, budget: budget)
            var syncError: ClientError?
            // Incoming records (requests, approvals, a repair snapshot) are applied before any permit.
            do {
                let pulled = try await sync.run()
                if pulled.contactRepairRequired, pulled.repairUnavailable { syncError = .unexpected("contact repair needs a compaction-capable server") }
            } catch {
                let wrapped = ClientError.wrap(error)
                if wrapped == .canceled { throw wrapped }
                syncError = wrapped // Offline is not fatal: capture stays local and uploads next time.
            }
            let scratch = databaseDirectory.appendingPathComponent("contact-media-tmp", isDirectory: true)
            try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
            try Self.protectUntilFirstUnlock(scratch)
            let media = syncError == nil ? ContactsMediaSession(
                transfer: ContactMediaTransfer(client: client, server: server, scratch: scratch),
                meter: RequestMeter(limit: budget.maxRequests)
            ) : nil
            var coordinator = NativeContactsCoordinator(
                client: client, deviceId: identity.deviceId, provider: provider, preferences: preferences
            )
            coordinator.scratch = scratch
            coordinator.media = media
            var report = try await coordinator.run(reason: reason, isCancelled: isCancelled)
            if let media, syncError == nil, !isCancelled(), !Task.isCancelled {
                // Ciphertext first, then references, so photo-bearing envelopes may publish.
                do {
                    try await media.transfer.uploadAndRegister(meter: &media.meter, report: &media.report)
                    try await media.transfer.reclaim(meter: &media.meter, report: &media.report)
                } catch {
                    let wrapped = ClientError.wrap(error)
                    if wrapped == .canceled { throw wrapped }
                    if wrapped != .budgetExhausted { syncError = wrapped }
                }
                report.media = media.report
            }
            if syncError == nil, !isCancelled(), !Task.isCancelled {
                do { _ = try await sync.run() } catch { syncError = ClientError.wrap(error) }
            }
            report.syncError = syncError?.userMessage
            refreshCount()
            return report
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// Owner book summary for settings: sanitized core state plus pending owner requests.
    public func contactsOverview() throws -> ContactsOverview {
        guard let client, let record else { throw ClientError.notEnrolled }
        do {
            let coordinator = NativeContactsCoordinator(
                client: client, deviceId: record.identity.deviceId, provider: NoContactsProvider(),
                preferences: InMemoryContactsPreferences()
            )
            return try coordinator.overview()
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// Owner policy for remote edits: `auto`, `confirm` or `off`. Stored and published by the core.
    public func setContactsRemoteEdits(_ mode: String) throws -> ContactsOverview {
        guard let client, let record else { throw ClientError.notEnrolled }
        do {
            let coordinator = NativeContactsCoordinator(
                client: client, deviceId: record.identity.deviceId, provider: NoContactsProvider(),
                preferences: InMemoryContactsPreferences()
            )
            try coordinator.setRemoteEdits(mode)
            return try coordinator.overview()
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// The account (OS container) new contacts are saved to; an owner setting stored by the core.
    public func setContactsDefaultAccount(_ accountId: String) throws -> ContactsOverview {
        guard let client, let record else { throw ClientError.notEnrolled }
        do {
            let coordinator = NativeContactsCoordinator(
                client: client, deviceId: record.identity.deviceId, provider: NoContactsProvider(),
                preferences: InMemoryContactsPreferences()
            )
            try coordinator.setDefaultAccount(accountId)
            return try coordinator.overview()
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// A local, explicit owner decision. Approval does not write: the next pass takes the permit.
    public func decideContactRequest(_ requestId: String, approve: Bool, scanHold: Bool = false) throws -> ContactsOverview {
        guard let client, let record else { throw ClientError.notEnrolled }
        do {
            let coordinator = NativeContactsCoordinator(
                client: client, deviceId: record.identity.deviceId, provider: NoContactsProvider(),
                preferences: InMemoryContactsPreferences()
            )
            try coordinator.decide(requestId, approve: approve, scanHold: scanHold)
            return try coordinator.overview()
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// Publishes the owned book as retired and turns syncing off. OS contacts are untouched; enabling
    /// again starts a new book.
    public func retireContacts(preferences: any ContactsPreferencesStore) throws -> ContactsOverview {
        guard let client, let record else { throw ClientError.notEnrolled }
        guard !syncing else { throw ClientError.syncInProgress }
        do {
            let coordinator = NativeContactsCoordinator(
                client: client, deviceId: record.identity.deviceId, provider: NoContactsProvider(),
                preferences: preferences
            )
            try coordinator.retire()
            var prefs = preferences.load()
            prefs.enabled = false
            preferences.save(prefs)
            return try coordinator.overview()
        } catch {
            throw ClientError.wrap(error)
        }
    }

    public func conversations() throws -> [NativeConversation] {
        guard let client else { throw ClientError.notEnrolled }
        do { return try client.listConversations() } catch { throw ClientError.wrap(error) }
    }

    /// Releases the client. Nothing on disk or in the Keychain changes.
    public func close() {
        try? client?.dispose()
        client = nil
        record = nil
        status = SessionStatus()
    }

    private func refreshCount() {
        status.conversations = try? client?.listConversations().count
    }

    private func databaseURL(_ identity: EnrollmentIdentity) -> URL {
        databaseDirectory.appendingPathComponent("\(identity.vaultId)-\(identity.deviceId).sqlcipher")
    }

    private func databaseExists(_ identity: EnrollmentIdentity) -> Bool {
        FileManager.default.fileExists(atPath: databaseURL(identity).path)
    }

    /// Background passes run while the device is locked after its first unlock, so the database,
    /// its sidecars and media must use `completeUntilFirstUserAuthentication`. New files inherit the
    /// directory's class; existing ones are set explicitly. A no-op off iOS.
    static func protectUntilFirstUnlock(_ directory: URL) throws {
        #if os(iOS)
        let attributes: [FileAttributeKey: Any] = [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication]
        let manager = FileManager.default
        try manager.setAttributes(attributes, ofItemAtPath: directory.path)
        guard let items = manager.enumerator(at: directory, includingPropertiesForKeys: nil) else { return }
        for case let item as URL in items {
            try manager.setAttributes(attributes, ofItemAtPath: item.path)
        }
        #endif
    }

    /// Fails closed when this identity's database was created before but is gone, or when a
    /// database file exists without its key; neither is ever replaced by a fresh database.
    private func openClient(_ identity: EnrollmentIdentity) throws -> NativeClient {
        let exists = databaseExists(identity)
        if try store.databaseWasCreated(identity), !exists { throw ClientError.localDatabaseMissing }
        var key = try store.databaseKey(identity, mayCreate: !exists)
        defer { key.resetBytes(in: 0..<key.count) }
        do {
            try FileManager.default.createDirectory(at: databaseDirectory, withIntermediateDirectories: true)
            var directory = databaseDirectory
            var values = URLResourceValues()
            // The database key is device-only, so a backed-up copy of the database could never be opened.
            values.isExcludedFromBackup = true
            try directory.setResourceValues(values)
            try Self.protectUntilFirstUnlock(databaseDirectory)
            let opened = try openNativeClient(config: NativeOpenConfig(
                databasePath: databaseURL(identity).path, vaultId: identity.vaultId, deviceId: identity.deviceId, databaseKey: key
            ))
            // SQLCipher creates its -wal/-shm sidecars and media directories on open.
            try Self.protectUntilFirstUnlock(databaseDirectory)
            try store.markDatabaseCreated(identity)
            return opened
        } catch {
            throw ClientError.wrap(error)
        }
    }
}
