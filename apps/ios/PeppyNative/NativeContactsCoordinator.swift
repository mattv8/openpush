import Foundation
#if canImport(PeppyBindings)
import PeppyBindings
#endif

/// Native-only contact preference: the user's opt-in. Books, policy, the new-contact account,
/// revisions, item IDs, scan checkpoints, history tokens and the edit ledger live in the Rust core.
public struct ContactsPreferences: Sendable, Equatable {
    public var enabled = false

    public init(enabled: Bool = false) {
        self.enabled = enabled
    }
}

public protocol ContactsPreferencesStore: Sendable {
    func load() -> ContactsPreferences
    func save(_ preferences: ContactsPreferences)
}

public final class UserDefaultsContactsPreferences: ContactsPreferencesStore, @unchecked Sendable {
    private let defaults: UserDefaults
    private let lock = NSLock()

    public init(defaults: UserDefaults = .standard) { self.defaults = defaults }

    public func load() -> ContactsPreferences {
        lock.withLock { ContactsPreferences(enabled: defaults.bool(forKey: "contacts.enabled")) }
    }

    public func save(_ value: ContactsPreferences) {
        lock.withLock { defaults.set(value.enabled, forKey: "contacts.enabled") }
    }
}

public final class InMemoryContactsPreferences: ContactsPreferencesStore, @unchecked Sendable {
    private let lock = NSLock()
    private var value: ContactsPreferences
    public init(_ value: ContactsPreferences = ContactsPreferences()) { self.value = value }
    public func load() -> ContactsPreferences { lock.withLock { value } }
    public func save(_ newValue: ContactsPreferences) { lock.withLock { value = newValue } }
}

public struct ContactsSyncReport: Sendable, Equatable {
    public enum Outcome: Sendable, Equatable {
        case disabled
        /// Vault keys are locked (or a new epoch needs the passphrase); nothing was captured.
        case needsUnlock
        case busy
        /// The current server does not provide fenced compaction snapshots.
        case serverIncompatible
        /// Local historical contact state is still being prepared for compaction.
        case backfillPending
        /// A latched projection repair has not promoted yet; no capture or OS write ran.
        case repairPending
        /// Access is not granted; the book was published as unavailable, never emptied.
        case noAccess
        /// OS change history showed no contact changes since the last checkpoint.
        case unchanged
        /// Only the contacts named by OS change history were captured.
        case incremental
        case completed
        /// Cancelled, bounded or limited: captured content was kept, nothing was inferred as deleted.
        case partial
    }

    public var outcome: Outcome
    public var access: ContactsAccess?
    public var bookId: String?
    public var captured = 0
    public var authoritative = false
    public var deleted = 0
    /// Removals the core held for approval (mass disappearance).
    public var held = 0
    public var limitExceeded = false
    public var photosCaptured = 0
    public var photosRemoved = 0
    public var photoFailures = 0
    /// Contacts still waiting for a photo check (history backlog or an unfinished sweep).
    public var photoWorkRemaining = false
    public var applied = 0
    public var failed = 0
    public var outcomeUnknown = 0
    public var awaitingApproval: [String] = []
    /// Permits waiting for a photo download (`waiting_media`).
    public var waitingMedia = 0
    public var media = ContactMediaReport()
    public var projectionFailure: String?
    /// Core readiness state which prevented contact capture or OS writes.
    public var readinessState: String?
    /// Retention is paused until every vault device has declared compaction fencing.
    public var retentionPaused = false
    public var syncError: String?

    public init(outcome: Outcome) { self.outcome = outcome }
}

/// A pending owner decision or in-flight edit. Scan holds are mass-disappearance approvals.
public struct ContactsPendingRequest: Sendable, Equatable, Identifiable {
    public let id: String
    /// `requested`, `approved`, `applying`, `outcome_unknown` or `awaiting_approval`.
    public let state: String
    public let requester: String
    public let expiresAt: Date?
    /// `create`, `update`, `delete` or `scan_deletions`.
    public let kind: String
    public let displayName: String?
    public let fieldPaths: [String]
    public let isScanHold: Bool
    public let count: Int?
    public let sampleNames: [String]
}

public struct ContactsAccount: Sendable, Equatable, Identifiable {
    /// Native-only OS container id; never leaves the phone.
    public let id: String
    public let name: String
    public let writable: Bool
}

/// Owner-book state for settings. Account ids are native-only and come from owner settings.
public struct ContactsOverview: Sendable, Equatable {
    public var bookId: String?
    public var state: String?
    public var remoteEdits: String?
    public var contactCount: Int?
    public var accounts: [ContactsAccount] = []
    public var defaultAccountId: String?
    public var pending: [ContactsPendingRequest] = []

    public init() {}
}

/// Provider for core-only operations (settings, approvals, retire) that never touch the OS.
struct NoContactsProvider: ContactsProvider {
    func access() async throws -> ContactsAccess { .notDetermined }
    func containers() async throws -> [ContactContainer] { [] }
    func fetchAll(isCancelled: @Sendable () -> Bool) async throws -> ContactsFetch { ContactsFetch(contacts: [], complete: false) }
    func contact(id: String) async throws -> PlatformContact? { nil }
    func historyToken() async -> Data? { nil }
    func photo(id: String, thumbnail: Bool) async throws -> ContactPhotoRead { .none }
    func save(_ write: ContactsWrite) async throws -> ContactsWriteResult { .outOfGrant }
}

enum ContactsCoordinatorError: Error, Equatable {
    case invalidCoreResponse(String)
}

/// Owner-local opaque checkpoint stored by the core (`contact_scan_state_json`), never synced.
struct ContactsCheckpoint: Equatable {
    /// OS change-history token read before the last complete scan or applied history batch.
    var historyToken: Data?
    /// Source keys from change history whose photo has not been checked yet.
    var pendingPhotoKeys: [String] = []
    /// A photo sweep over the core book (offset into `contact_book_view`), started by a complete scan.
    var sweepActive = false
    var sweepOffset = 0
    /// Attachment ids whose download answered 404, with the unix time; retried after a day.
    var deadDownloads: [String: Int] = [:]
    /// A full scan is a durable, bounded walk. `scanCursor` is the last source id observed.
    var scanId: String?
    var scanCursor: String?
    var scanHistoryToken: Data?
    var scanIncomplete = false

    static let deadDownloadRetry = 24 * 60 * 60
    static let maxPendingPhotoKeys = 400

    init() {}

    init(json: [String: Any]?) {
        guard let json else { return }
        historyToken = (json["history_token"] as? String).flatMap { Data(base64Encoded: $0) }
        pendingPhotoKeys = json["pending_photo_keys"] as? [String] ?? []
        sweepActive = json["sweep_active"] as? Bool ?? false
        sweepOffset = json["sweep_offset"] as? Int ?? 0
        deadDownloads = json["dead_downloads"] as? [String: Int] ?? [:]
        scanId = json["scan_id"] as? String
        scanCursor = json["scan_cursor"] as? String
        scanHistoryToken = (json["scan_history_token"] as? String).flatMap { Data(base64Encoded: $0) }
        scanIncomplete = json["scan_incomplete"] as? Bool ?? false
    }

    var json: [String: Any] {
        var out: [String: Any] = [
            "version": 1, "pending_photo_keys": Array(pendingPhotoKeys.prefix(Self.maxPendingPhotoKeys)),
            "sweep_active": sweepActive, "sweep_offset": sweepOffset, "dead_downloads": deadDownloads,
            "scan_incomplete": scanIncomplete,
        ]
        if let historyToken { out["history_token"] = historyToken.base64EncodedString() }
        if let scanHistoryToken { out["scan_history_token"] = scanHistoryToken.base64EncodedString() }
        if let scanId { out["scan_id"] = scanId }
        if let scanCursor { out["scan_cursor"] = scanCursor }
        return out
    }

    mutating func addPhotoKeys(_ keys: [String]) {
        for key in keys where !pendingPhotoKeys.contains(key) { pendingPhotoKeys.append(key) }
        if pendingPhotoKeys.count > Self.maxPendingPhotoKeys {
            // Overflow: a full sweep covers everything instead.
            pendingPhotoKeys.removeAll()
            sweepActive = true
            sweepOffset = 0
        }
    }

    func isDead(_ id: String, now: Int) -> Bool { deadDownloads[id].map { now - $0 < Self.deadDownloadRetry } ?? false }

    mutating func pruneDead(now: Int) {
        deadDownloads = deadDownloads.filter { now - $0.value < Self.deadDownloadRetry }
        while deadDownloads.count > 100, let oldest = deadDownloads.min(by: { $0.value < $1.value }) {
            deadDownloads.removeValue(forKey: oldest.key)
        }
    }
}

/// Network access for photo transfers within the pass budget.
final class ContactsMediaSession: @unchecked Sendable {
    let transfer: ContactMediaTransfer
    var meter: RequestMeter
    var report = ContactMediaReport()

    init(transfer: ContactMediaTransfer, meter: RequestMeter) {
        self.transfer = transfer
        self.meter = meter
    }
}

/// Per-pass photo bounds: OS photo reads and contacts examined by the sweep.
struct ContactsPhotoBudget: Sendable {
    var reads: Int
    var sweep: Int

    static func `for`(_ reason: ContactsPassReason) -> ContactsPhotoBudget {
        switch reason {
        case .backgroundRefresh: ContactsPhotoBudget(reads: 10, sweep: 100)
        case .changeNotification, .foreground: ContactsPhotoBudget(reads: 50, sweep: 500)
        case .backgroundProcessing: ContactsPhotoBudget(reads: 400, sweep: 5_000)
        }
    }
}

/// One bounded contact pass against the already-open `NativeClient`:
/// repair gate → book state → OS change history fast path (≤200 changed contacts, no deletions)
/// or full scan (observe/finish) → bounded photo work → owner edit permits.
/// Never retries an ambiguous write and never infers deletions from a partial or limited scan.
struct NativeContactsCoordinator {
    static let batchSize = 200
    static let historyLimit = 200

    let client: NativeClient
    let deviceId: String
    let provider: any ContactsProvider
    let preferences: any ContactsPreferencesStore
    /// Protected directory for transient photo files handed to the core normalizer.
    var scratch: URL = FileManager.default.temporaryDirectory.appendingPathComponent("peppy-contact-photos", isDirectory: true)
    var media: ContactsMediaSession?
    var photoBudget: ContactsPhotoBudget?

    func run(reason: ContactsPassReason, isCancelled: @escaping @Sendable () -> Bool) async throws -> ContactsSyncReport {
        guard preferences.load().enabled else { return ContactsSyncReport(outcome: .disabled) }
        let readiness = try Self.object(client.contactSyncReadinessJson())
        let contactsReady = readiness["contacts_ready"] as? Bool ?? false
        let supported = readiness["server_supported"] as? Bool ?? false
        let active = readiness["server_active"] as? Bool ?? false
        guard contactsReady else {
            let state = readiness["state"] as? String ?? "needs_unlock"
            let outcome: ContactsSyncReport.Outcome = switch state {
            case "server_unsupported": .serverIncompatible
            case "backfill_pending": .backfillPending
            default: .needsUnlock
            }
            var report = ContactsSyncReport(outcome: outcome)
            report.readinessState = state
            report.retentionPaused = supported && !active
            return report
        }
        if try client.contactRepairRequired() {
            // The projection is rebuilt from a fenced snapshot first; republishing now would fight it.
            var report = ContactsSyncReport(outcome: .repairPending)
            if let status = try client.snapshotProjectionStatus(), status.state == .failed {
                report.projectionFailure = status.reason ?? "failed"
            }
            report.retentionPaused = supported && !active
            return report
        }
        let existing = try ownedBook()
        let access = try await provider.access()
        let bookId = existing?.id ?? UUID().uuidString.lowercased()
        let generation = existing?.generation ?? "1"
        var report = ContactsSyncReport(outcome: .partial)
        report.retentionPaused = supported && !active
        report.access = access
        report.bookId = bookId
        guard access.mayRead else {
            // Permission loss publishes the book state only; existing contacts are never removed.
            _ = try capture(book: bookBody(id: bookId, generation: generation, access: access, containers: []), entries: [])
            report.outcome = .noAccess
            return report
        }
        let book = bookBody(id: bookId, generation: generation, access: access, containers: try await provider.containers())
        // Book state is republished by the core only when it changed (or on its heartbeat).
        _ = try capture(book: book, entries: [])
        var checkpoint = existing == nil ? ContactsCheckpoint() : try readCheckpoint(bookId)
        let original = checkpoint
        let budget = photoBudget ?? .for(reason)

        var handled = false
        if existing != nil, checkpoint.scanId == nil, access == .full, let token = checkpoint.historyToken {
            handled = try await incremental(since: token, book: book, checkpoint: &checkpoint, report: &report, isCancelled: isCancelled)
        }
        if !handled {
            try await fullScan(reason: reason, book: book, bookId: bookId, generation: generation,
                               checkpoint: &checkpoint, report: &report, isCancelled: isCancelled)
        }
        if checkpoint != original { try writeCheckpoint(bookId, checkpoint) }
        if !isCancelled() {
            try await photoWork(book: book, bookId: bookId, budget: budget, checkpoint: &checkpoint, report: &report, isCancelled: isCancelled)
        }
        if !isCancelled() {
            try await processRequests(book: book, bookId: bookId, access: report.access ?? access,
                                      checkpoint: &checkpoint, report: &report, isCancelled: isCancelled)
        }
        report.photoWorkRemaining = !checkpoint.pendingPhotoKeys.isEmpty || checkpoint.sweepActive
        try writeCheckpoint(bookId, checkpoint)
        if let media { report.media = media.report }
        return report
    }

    /// OS change history fast path. Returns false when a full scan is required instead. The token
    /// advances only together with the durable photo backlog for the changed contacts.
    private func incremental(
        since token: Data, book: [String: Any], checkpoint: inout ContactsCheckpoint, report: inout ContactsSyncReport,
        isCancelled: @escaping @Sendable () -> Bool
    ) async throws -> Bool {
        guard case let .changed(ids, next) = await provider.changes(since: token, limit: Self.historyLimit) else { return false }
        var contacts: [PlatformContact] = []
        for id in ids {
            guard !isCancelled() else { return true }
            // A changed contact that is no longer readable (deleted, moved out of grant) needs the full scan.
            guard let contact = try await provider.contact(id: id) else { return false }
            contacts.append(contact)
        }
        guard try await provider.access() == .full else { return false }
        let accepted = try captureRepresentable(book: book, contacts: contacts, report: &report)
        checkpoint.addPhotoKeys(accepted)
        guard accepted.count == contacts.count else {
            report.outcome = .partial
            return true
        }
        checkpoint.historyToken = next
        report.authoritative = false
        report.outcome = ids.isEmpty ? .unchanged : .incremental
        return true
    }

    private func fullScan(
        reason: ContactsPassReason, book: [String: Any], bookId: String, generation: String,
        checkpoint: inout ContactsCheckpoint, report: inout ContactsSyncReport, isCancelled: @escaping @Sendable () -> Bool
    ) async throws {
        let tokenBefore = await provider.historyToken()
        let accessBefore = try await provider.access()
        if checkpoint.scanId != nil && (accessBefore != .full || checkpoint.scanHistoryToken != tokenBefore) {
            try abandonScan(&checkpoint)
            try writeCheckpoint(bookId, checkpoint)
        }
        let page: ContactsPage?
        let input: ContactsPassInput
        if accessBefore == .full {
            let fetched = try await provider.fetchPage(after: checkpoint.scanCursor, limit: Self.batchSize, isCancelled: isCancelled)
            page = fetched
            let accessAfter = try await provider.access()
            input = ContactsPassInput(
                reason: reason, access: accessAfter, contacts: fetched.contacts, containers: [],
                cancelled: isCancelled(), limitExceeded: fetched.limitExceeded,
                authoritative: accessAfter == .full && fetched.complete && !isCancelled()
            )
        } else {
            page = nil
            input = try await ContactsPass(provider: provider).capture(reason: reason, isCancelled: isCancelled)
        }
        report.access = input.access
        report.limitExceeded = input.limitExceeded
        if input.access != .full { try abandonScan(&checkpoint) }
        guard input.access.mayRead else {
            _ = try capture(book: bookBody(id: bookId, generation: generation, access: input.access, containers: []), entries: [])
            report.outcome = .noAccess
            return
        }
        var interrupted = false
        if input.access == .full {
            // Membership is recorded across bounded passes.  The source-id cursor makes repeats
            // harmless and avoids restarting at the beginning after a background expiration.
            let scanId = checkpoint.scanId ?? UUID().uuidString.lowercased()
            if checkpoint.scanId == nil {
                checkpoint.scanId = scanId
                checkpoint.scanCursor = nil
                checkpoint.scanHistoryToken = tokenBefore
                checkpoint.scanIncomplete = false
            }
            _ = try call(client.beginContactScan, [
                "schema_version": 1, "scan_id": scanId, "book_id": bookId,
                "generation": generation, "access": "full", "authoritative": true,
            ])
            // Persist the scan identity before observing members so a process interruption
            // resumes this membership set, rather than leaking a new scan on every restart.
            try writeCheckpoint(bookId, checkpoint)
            if !input.authoritative && !isCancelled() { checkpoint.scanIncomplete = true }
            let ordered = input.contacts.sorted { $0.id < $1.id }
            let pending = ordered.filter { contact in
                checkpoint.scanCursor.map { contact.id > $0 } ?? true
            }
            let maximum = reason == .backgroundRefresh ? 200 : Self.batchSize
            for (index, contact) in pending.prefix(maximum).enumerated() {
                if isCancelled() { interrupted = true; break }
                // One unrepresentable record never wedges the book.  It makes this scan
                // non-authoritative, but other records and pending owner work still progress.
                do {
                    _ = try call(client.observeContactScan, ["schema_version": 1, "scan_id": scanId, "source": Self.textEntry(contact)])
                    report.captured += 1
                } catch MobileBindingsError.InvalidRequest {
                    checkpoint.scanIncomplete = true
                    report.limitExceeded = true
                }
                checkpoint.scanCursor = contact.id
                if index % 20 == 19 { try writeCheckpoint(bookId, checkpoint) }
            }
            if !interrupted && !isCancelled(), let page { checkpoint.scanCursor = page.nextCursor }
            let exhausted = !(page?.hasMore ?? (pending.count > maximum))
            let tokenAfter = await provider.historyToken()
            let unchangedHistory = checkpoint.scanHistoryToken == tokenAfter
            let complete = exhausted && input.authoritative && unchangedHistory && !checkpoint.scanIncomplete && !interrupted && !isCancelled()
            guard exhausted && !interrupted && !isCancelled() else {
                report.outcome = .partial
                return
            }
            let finished = try call(client.finishContactScan, ["schema_version": 1, "scan_id": scanId, "complete": complete])
            report.deleted = finished["deleted"] as? Int ?? 0
            report.held = finished["held"] as? Int ?? 0
            report.authoritative = complete
            report.outcome = complete ? .completed : .partial
            checkpoint.scanId = nil
            checkpoint.scanCursor = nil
            checkpoint.scanHistoryToken = nil
            checkpoint.scanIncomplete = false
            if complete {
                // Only a token read before a complete scan may anchor later history.
                checkpoint.historyToken = tokenBefore
                checkpoint.pendingPhotoKeys.removeAll()
                checkpoint.sweepActive = true
                checkpoint.sweepOffset = 0
            }
        } else {
            let limitedBook = bookBody(id: bookId, generation: generation, access: input.access, containers: try await provider.containers())
            var start = 0
            while start < input.contacts.count {
                if isCancelled() { break }
                let batch = Array(input.contacts[start..<min(start + Self.batchSize, input.contacts.count)])
                start += batch.count
                let accepted = try captureRepresentable(book: limitedBook, contacts: batch, report: &report)
                checkpoint.addPhotoKeys(accepted)
            }
            // A limited grant never anchors history; its photos are checked through the pending list.
            checkpoint.historyToken = nil
            report.outcome = .partial
        }
    }

    private func abandonScan(_ checkpoint: inout ContactsCheckpoint) throws {
        if let id = checkpoint.scanId {
            _ = try call(client.finishContactScan, ["schema_version": 1, "scan_id": id, "complete": false])
        }
        checkpoint.scanId = nil
        checkpoint.scanCursor = nil
        checkpoint.scanHistoryToken = nil
        checkpoint.scanIncomplete = false
        checkpoint.historyToken = nil
    }

    /// Reject only unrepresentable records, without losing progress on the rest of a batch.
    private func captureRepresentable(book: [String: Any], contacts: [PlatformContact], report: inout ContactsSyncReport) throws -> [String] {
        var accepted: [String] = []
        for contact in contacts {
            do {
                _ = try capture(book: book, entries: [Self.textEntry(contact)])
                accepted.append(contact.id)
                report.captured += 1
            } catch MobileBindingsError.InvalidRequest {
                report.limitExceeded = true
            }
        }
        return accepted
    }

    // MARK: Photos

    /// Bounded photo work: the history backlog first, then the sweep. The checkpoint is saved
    /// as it advances, so an expired task resumes where it stopped.
    private func photoWork(
        book: [String: Any], bookId: String, budget: ContactsPhotoBudget, checkpoint: inout ContactsCheckpoint,
        report: inout ContactsSyncReport, isCancelled: @escaping @Sendable () -> Bool
    ) async throws {
        var reads = 0
        var examined = 0
        while let key = checkpoint.pendingPhotoKeys.first, reads < budget.reads, !isCancelled() {
            try await checkPhoto(sourceKey: key, core: nil, book: book, bookId: bookId, reads: &reads, report: &report)
            checkpoint.pendingPhotoKeys.removeFirst()
            if examined % 20 == 0 { try writeCheckpoint(bookId, checkpoint) }
            examined += 1
        }
        while checkpoint.sweepActive, reads < budget.reads, examined < budget.sweep, !isCancelled() {
            let page = try Self.object(client.contactBookView(inputJson: Self.json([
                "book_id": bookId, "offset": checkpoint.sweepOffset, "limit": 50,
            ])))
            let contacts = page["contacts"] as? [[String: Any]] ?? []
            if contacts.isEmpty {
                checkpoint.sweepActive = false
                break
            }
            for item in contacts {
                guard reads < budget.reads, examined < budget.sweep, !isCancelled() else { break }
                if let contactId = item["id"] as? String,
                   let context = try? sourceContext(bookId: bookId, contactId: contactId) {
                    try await checkPhoto(sourceKey: context.sourceKey, core: context, book: book, bookId: bookId,
                                         reads: &reads, report: &report)
                }
                checkpoint.sweepOffset += 1
                examined += 1
            }
            try writeCheckpoint(bookId, checkpoint)
        }
    }

    /// Compares the OS photo with the owner-local fingerprint and captures a change. An explicit
    /// `null` photo is sent only when the OS confirms the contact has none.
    func checkPhoto(
        sourceKey: String, core: SourceContext?, book: [String: Any], bookId: String, reads: inout Int,
        report: inout ContactsSyncReport
    ) async throws {
        guard let contact = try await provider.contact(id: sourceKey) else { return }
        let context: SourceContext
        if let core {
            context = core
        } else if let found = try? sourceContext(bookId: bookId, sourceKey: sourceKey) {
            context = found
        } else {
            return
        }
        let storedHash = context.provenance["photo_source_hash"] as? String
        let corePhoto = context.observed["photo"] != nil && !(context.observed["photo"] is NSNull)
        var observed: ContactPhotoRead = .none
        if contact.hasImage {
            reads += 1
            observed = try await provider.photo(id: sourceKey, thumbnail: false)
        }
        switch observed {
        case .none:
            guard corePhoto || storedHash != nil else { return }
            _ = try capture(book: book, entries: [Self.entry(contact, photo: .remove, hash: nil)])
            report.photosRemoved += 1
        case .tooLarge:
            report.photoFailures += 1
        case let .data(data):
            let hash = ContactPhotoSource.sourceHash(data)
            if hash == storedHash && corePhoto { return }
            guard let attachmentId = prepare(data) else { report.photoFailures += 1; return }
            _ = try capture(book: book, entries: [Self.entry(contact, photo: .set(attachmentId), hash: hash)])
            report.photosCaptured += 1
        }
    }

    /// OS bytes → core normalizer (bounded, encrypted, tracked). Nil when the image is unusable.
    func prepare(_ data: Data) -> String? {
        guard let input = try? ContactPhotoSource.normalizerInput(data) else { return nil }
        let file = scratch.appendingPathComponent("\(UUID().uuidString).img")
        defer { try? FileManager.default.removeItem(at: file) }
        do {
            try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
            try input.write(to: file, options: [.atomic])
            return try client.prepareContactPhoto(path: file.path).attachmentId
        } catch {
            return nil
        }
    }

    enum EntryPhoto { case keep, set(String), remove }

    /// A platform entry: text fields, optional photo intent and owner-local provenance.
    static func entry(_ contact: PlatformContact, photo: EntryPhoto, hash: String?) -> [String: Any] {
        var fields = ContactsCoreMapping.fields(contact)
        var provenance: [String: Any] = [:]
        if let container = contact.containerId { provenance["account_id"] = container }
        switch photo {
        case .keep: break
        case let .set(id):
            fields["photo"] = ["attachment_id": id]
            provenance["photo_source_hash"] = hash ?? NSNull()
        case .remove:
            fields["photo"] = NSNull()
            provenance["photo_source_hash"] = NSNull()
        }
        var entry: [String: Any] = ["source_key": contact.id, "fields": fields]
        if !provenance.isEmpty { entry["provenance"] = provenance }
        return entry
    }

    static func textEntry(_ contact: PlatformContact) -> [String: Any] { entry(contact, photo: .keep, hash: nil) }

    // MARK: Checkpoint

    func readCheckpoint(_ bookId: String) throws -> ContactsCheckpoint {
        let out = try call(client.contactScanStateJson, ["schema_version": 1, "book_id": bookId])
        return ContactsCheckpoint(json: out["checkpoint"] as? [String: Any])
    }

    func writeCheckpoint(_ bookId: String, _ checkpoint: ContactsCheckpoint) throws {
        _ = try call(client.contactScanStateJson, ["schema_version": 1, "book_id": bookId, "checkpoint": checkpoint.json])
    }

    // MARK: Settings and approvals (core-only, never an OS write)

    func overview() throws -> ContactsOverview {
        var overview = ContactsOverview()
        let books = try Self.object(client.listContactBooksJson())["books"] as? [[String: Any]] ?? []
        guard let book = books.first(where: { $0["owner_device_id"] as? String == deviceId && $0["state"] as? String != "retired" }),
              let id = book["id"] as? String
        else { return overview }
        overview.bookId = id
        overview.state = book["state"] as? String
        overview.contactCount = book["contact_count"] as? Int
        let settings = try settings(bookId: id)
        overview.remoteEdits = settings["effective_remote_edits"] as? String
            ?? book["effective_remote_edits"] as? String ?? "auto"
        overview.defaultAccountId = settings["default_account_id"] as? String
        overview.accounts = (settings["accounts"] as? [[String: Any]] ?? []).compactMap { account in
            guard let id = account["id"] as? String else { return nil }
            return ContactsAccount(id: id, name: account["name"] as? String ?? id, writable: account["writable"] as? Bool ?? true)
        }
        let listed = try Self.object(client.listContactRequestsJson(input: Self.json(["book_id": id])))
        let open: Set<String> = ["requested", "approved", "applying", "outcome_unknown", "awaiting_approval"]
        overview.pending = (listed["requests"] as? [[String: Any]] ?? []).compactMap { row in
            guard let state = row["state"] as? String, open.contains(state) else { return nil }
            let kind = row["kind"] as? String ?? "update"
            let isScan = kind == "scan_deletions"
            guard let id = (isScan ? row["scan_id"] : row["request_id"]) as? String else { return nil }
            let expires = (row["expires_at"] as? NSNumber).map { Date(timeIntervalSince1970: $0.doubleValue) }
            return ContactsPendingRequest(
                id: id, state: state, requester: row["requester"] as? String ?? "", expiresAt: expires, kind: kind,
                displayName: row["display_name"] as? String, fieldPaths: row["field_paths"] as? [String] ?? [],
                isScanHold: isScan, count: row["count"] as? Int, sampleNames: row["sample_names"] as? [String] ?? []
            )
        }
        return overview
    }

    func settings(bookId: String) throws -> [String: Any] {
        try Self.object(client.contactSettingsJson(input: Self.json(["book_id": bookId])))
    }

    func setRemoteEdits(_ mode: String) throws {
        guard ["auto", "confirm", "off"].contains(mode), let book = try ownedBook() else {
            throw ContactsCoordinatorError.invalidCoreResponse("no owned contact book")
        }
        _ = try Self.object(client.contactSettingsJson(input: Self.json(["book_id": book.id, "policy": ["remote_edits": mode]])))
    }

    /// The account for new contacts is an owner setting in the core (a listed, writable account).
    func setDefaultAccount(_ accountId: String) throws {
        guard let book = try ownedBook() else { throw ContactsCoordinatorError.invalidCoreResponse("no owned contact book") }
        _ = try Self.object(client.contactSettingsJson(input: Self.json(["book_id": book.id, "default_account_id": accountId])))
    }

    /// Edit requests are decided by `request_id`; mass-disappearance holds by `scan_id`.
    func decide(_ id: String, approve: Bool, scanHold: Bool = false) throws {
        _ = try Self.object(client.contactApprovalJson(input: Self.json([scanHold ? "scan_id" : "request_id": id, "approve": approve])))
    }

    func retire() throws {
        guard let book = try ownedBook() else { return }
        let body = bookBody(id: book.id, generation: book.generation, access: .denied, containers: [], state: "retired")
        _ = try capture(book: body, entries: [])
    }

    // MARK: Book

    struct OwnedBook: Equatable { let id: String; let generation: String }

    func ownedBook() throws -> OwnedBook? {
        let books = try Self.object(client.listContactBooksJson())["books"] as? [[String: Any]] ?? []
        return books.lazy.filter { $0["owner_device_id"] as? String == deviceId && $0["state"] as? String != "retired" }
            .compactMap { book -> OwnedBook? in
                guard let id = book["id"] as? String, let generation = book["generation"] as? String else { return nil }
                return OwnedBook(id: id, generation: generation)
            }
            .first
    }

    /// The raw native book. No policy is sent (the core default is Auto and owner settings win);
    /// `default_account_id` is the OS default, which an owner setting overrides while listed.
    func bookBody(
        id: String, generation: String, access: ContactsAccess, containers: [ContactContainer], state: String? = nil
    ) -> [String: Any] {
        var book: [String: Any] = [
            "id": id, "owner_device_id": deviceId, "generation": generation,
            "state": state ?? Self.bookState(access),
            // Notes need an entitlement this app does not hold.
            "capabilities": ["read": access.mayRead, "write": access.mayRead, "photo": true, "notes": false, "birthday": true],
            "accounts": containers.map { ["id": $0.id, "name": $0.name, "writable": true] as [String: Any] },
        ]
        if let fallback = containers.first(where: \.isDefault)?.id { book["default_account_id"] = fallback }
        return book
    }

    static func bookState(_ access: ContactsAccess) -> String {
        switch access {
        case .full: "active"
        case .limited: "limited"
        case .denied, .restricted, .notDetermined: "unavailable"
        }
    }

    /// Captures one bounded batch of platform entries; returns core contact IDs in input order.
    @discardableResult
    func capture(book: [String: Any], entries: [[String: Any]]) throws -> [String] {
        precondition(entries.count <= Self.batchSize)
        let out = try call(client.capturePlatformContactsJson, ["schema_version": 1, "book": book, "contacts": entries])
        let ids = (out["contacts"] as? [[String: Any]] ?? []).compactMap { $0["id"] as? String }
        guard ids.count == entries.count else { throw ContactsCoordinatorError.invalidCoreResponse("capture") }
        return ids
    }

    struct SourceContext {
        let sourceKey: String
        let observed: [String: Any]
        let fieldSources: [[String: Any]]
        let provenance: [String: Any]
    }

    func sourceContext(bookId: String, contactId: String) throws -> SourceContext {
        try sourceContext(["schema_version": 1, "book_id": bookId, "contact_id": contactId])
    }

    func sourceContext(bookId: String, sourceKey: String) throws -> SourceContext {
        try sourceContext(["schema_version": 1, "book_id": bookId, "source_key": sourceKey])
    }

    private func sourceContext(_ input: [String: Any]) throws -> SourceContext {
        let out = try call(client.contactSourceContextJson, input)
        guard let key = out["source_key"] as? String, let observed = out["observed"] as? [String: Any] else {
            throw ContactsCoordinatorError.invalidCoreResponse("source context")
        }
        return SourceContext(
            sourceKey: key, observed: observed, fieldSources: out["field_sources"] as? [[String: Any]] ?? [],
            provenance: out["provenance"] as? [String: Any] ?? [:]
        )
    }

    // MARK: Owner edit permits

    func processRequests(
        book: [String: Any], bookId: String, access: ContactsAccess, checkpoint: inout ContactsCheckpoint,
        report: inout ContactsSyncReport, isCancelled: @escaping @Sendable () -> Bool
    ) async throws {
        let listed = try Self.object(client.listContactRequestsJson(input: Self.json(["book_id": bookId])))
        let open: Set<String> = ["requested", "approved", "applying", "outcome_unknown", "awaiting_approval"]
        let now = Int(Date().timeIntervalSince1970)
        checkpoint.pruneDead(now: now)
        for row in listed["requests"] as? [[String: Any]] ?? [] {
            guard !isCancelled() else { return }
            guard row["kind"] as? String != "scan_deletions", let id = row["request_id"] as? String,
                  let state = row["state"] as? String, open.contains(state) else { continue }
            var permit = try call(client.nextContactApplyPermit, ["schema_version": 1, "request_id": id])
            if permit["status"] as? String == "waiting_media", let attachment = permit["attachment_id"] as? String {
                // The photo must be downloaded and verified locally before the core issues a permit.
                guard let media, !checkpoint.isDead(attachment, now: now) else { report.waitingMedia += 1; continue }
                let gone = try await media.transfer.download(only: [attachment], meter: &media.meter, report: &media.report)
                if gone.contains(attachment) { checkpoint.deadDownloads[attachment] = now }
                permit = try call(client.nextContactApplyPermit, ["schema_version": 1, "request_id": id])
            }
            switch permit["status"] as? String {
            case "permit":
                guard let request = permit["request"] as? [String: Any] else { throw ContactsCoordinatorError.invalidCoreResponse("permit") }
                let outcome = await apply(request: request, current: permit["current"] as? [String: Any], requestId: id, book: book, bookId: bookId, access: access, isCancelled: isCancelled)
                try record(outcome, requestId: id, report: &report)
            case "outcome_unknown":
                try await settleUnknown(permit, requestId: id, book: book, bookId: bookId, access: access, report: &report, isCancelled: isCancelled)
            case "awaiting_approval":
                report.awaitingApproval.append(id)
            case "waiting_media":
                report.waitingMedia += 1
            default:
                break
            }
        }
    }

    /// Never re-issues a write. Settles only with OS evidence: a delete whose contact is gone, or a
    /// create whose recorded pre-write evidence identifies exactly one new contact (or none).
    private func settleUnknown(
        _ permit: [String: Any], requestId: String, book: [String: Any], bookId: String, access: ContactsAccess,
        report: inout ContactsSyncReport, isCancelled: @escaping @Sendable () -> Bool
    ) async throws {
        guard access == .full, let request = permit["request"] as? [String: Any] else { report.outcomeUnknown += 1; return }
        switch request["kind"] as? String {
        case "delete":
            if let key = (permit["source"] as? [String: Any])?["source_key"] as? String,
               try await provider.contact(id: key) == nil {
                try record(.applied(source: nil), requestId: requestId, report: &report)
                return
            }
        case "create":
            if let before = ((permit["evidence"] as? [String: Any])?["before_source_ids"] as? [String]).map(Set.init),
               let now = try? await createMatches(request, isCancelled: isCancelled), now.complete {
                let created = Set(now.contacts.map(\.id)).subtracting(before)
                if created.count == 1, let id = created.first, let contact = try await provider.contact(id: id) {
                    try record(.applied(source: try await observedEntry(contact, photo: Self.photoAttachment(request).map(EntryPhoto.set) ?? .keep)),
                               requestId: requestId, report: &report)
                    return
                }
            }
        default:
            break
        }
        report.outcomeUnknown += 1
    }

    enum ApplyOutcome {
        case applied(source: [String: Any]?)
        case unknown
        case failed(String)
    }

    static func photoAttachment(_ request: [String: Any]) -> String? {
        let value = (request["photo_op"] as? [String: Any])?["attachment_id"] ?? request["photo"]
        if let id = value as? String { return id }
        return (value as? [String: Any])?["attachment_id"] as? String
    }

    /// Plaintext of a locally verified contact photo for an OS write.
    func photoBytes(_ attachmentId: String) throws -> Data {
        let handle = try client.openNativePlaintextFile(attachmentId: attachmentId)
        defer { try? handle.dispose() }
        let data = try Data(contentsOf: URL(fileURLWithPath: try handle.nativePlaintextPath()))
        guard data.count <= ContactsLimits.maxPhotoBytes else { throw ContactsAdapterError.photoTooLarge(byteCount: data.count) }
        return data
    }

    /// Performs one permitted platform write and gathers post-write OS evidence.
    func apply(request: [String: Any], current: [String: Any]? = nil, requestId: String, book: [String: Any], bookId: String, access: ContactsAccess, isCancelled: @escaping @Sendable () -> Bool = { Task.isCancelled }) async -> ApplyOutcome {
        guard !isCancelled() else { return .failed("canceled") }
        do {
            let kind = request["kind"] as? String
            let photoOp = (request["photo_op"] as? [String: Any])?["op"] as? String
            var photoId: String?
            let photo: ContactPhotoChange
            switch photoOp {
            case "remove": photo = .remove
            case "set":
                guard let id = Self.photoAttachment(request) else { return .failed("invalid_request") }
                photoId = id
                guard let bytes = try? photoBytes(id) else { return .failed("photo_unavailable") }
                photo = .set(bytes)
            case nil:
                if kind == "create", let id = Self.photoAttachment(request) {
                    photoId = id
                    guard let bytes = try? photoBytes(id) else { return .failed("photo_unavailable") }
                    photo = .set(bytes)
                } else {
                    photo = .keep
                }
            default: return .failed("invalid_request")
            }
            let result: ContactsWriteResult
            var sourceKey: String?
            switch kind {
            case "create":
                var fields = request
                for key in ["schema_version", "request_id", "target_owner", "book_id", "kind", "contact_id", "base_revision",
                            "expected_old", "patches", "photo_op", "expires_at", "provenance", "photo"] { fields.removeValue(forKey: key) }
                if let notes = fields["notes"] as? String, !notes.isEmpty { return .failed("unsupported_field") }
                let contact = ContactsCoreMapping.platform(fields, id: "", knownItemIds: [:])
                // Pre-write evidence lets an interrupted create be settled later without a retry.
                if access == .full, let before = try? await createMatches(request, verifyPhoto: false, isCancelled: isCancelled), before.complete, before.contacts.count <= 200 {
                    // Best effort: without evidence an interrupted create simply stays outcome-unknown.
                    _ = try? call(client.contactApplyEvidenceJson, [
                        "schema_version": 1, "request_id": requestId, "before_source_ids": before.contacts.map(\.id),
                    ])
                }
                let container = try settings(bookId: bookId)["default_account_id"] as? String ?? book["default_account_id"] as? String
                guard !isCancelled() else { return .failed("canceled") }
                result = try await provider.save(.create(contact: contact, containerId: container, photo: photo))
            case "update":
                guard let contactId = request["contact_id"] as? String else { return .failed("invalid_request") }
                let context = try sourceContext(bookId: bookId, contactId: contactId)
                sourceKey = context.sourceKey
                guard let fresh = try await provider.contact(id: context.sourceKey) else { return .failed(access == .limited ? "out_of_grant" : "not_found") }
                guard currentMatches(fresh, current: current, context: context) else {
                    _ = try? capture(book: book, entries: [Self.textEntry(fresh)])
                    return .failed("stale_item")
                }
                if photoOp != nil, !(await photoMatches(fresh, current: current, context: context)) {
                    var reads = 0
                    var report = ContactsSyncReport(outcome: .partial)
                    if !isCancelled() { try? await checkPhoto(sourceKey: context.sourceKey, core: context, book: book, bookId: bookId, reads: &reads, report: &report) }
                    return .failed("stale_item")
                }
                let desired = try ContactsCoreMapping.applying(
                    request["patches"] as? [[String: Any]] ?? [], to: ContactsCoreMapping.fields(fresh), fieldSources: context.fieldSources
                )
                var contact = ContactsCoreMapping.platform(
                    desired, id: context.sourceKey, knownItemIds: ContactsCoreMapping.nativeIds(context.fieldSources)
                )
                ContactsCoreMapping.preserveUnrepresentedNativeValues(from: fresh, in: &contact, patches: request["patches"] as? [[String: Any]] ?? [])
                guard !isCancelled() else { return .failed("canceled") }
                result = try await provider.save(.update(id: context.sourceKey, contact: contact, photo: photo))
            case "delete":
                guard let contactId = request["contact_id"] as? String else { return .failed("invalid_request") }
                let context = try sourceContext(bookId: bookId, contactId: contactId)
                guard let fresh = try await provider.contact(id: context.sourceKey) else { return .failed(access == .limited ? "out_of_grant" : "not_found") }
                guard currentMatches(fresh, current: current, context: context) else {
                    _ = try? capture(book: book, entries: [Self.textEntry(fresh)])
                    return .failed("stale_item")
                }
                guard await photoMatches(fresh, current: current, context: context) else { return .failed("stale_item") }
                guard !isCancelled() else { return .failed("canceled") }
                result = try await provider.save(.delete(id: context.sourceKey))
                switch result {
                case .applied, .notFound: return .applied(source: nil)
                default: break
                }
            default:
                return .failed("invalid_request")
            }
            switch result {
            case let .applied(id):
                guard let written = try? await provider.contact(id: sourceKey ?? id) else { return .unknown }
                let photoState: EntryPhoto = switch photo {
                case .keep: .keep
                case .remove: .remove
                case .set: photoId.map(EntryPhoto.set) ?? .keep
                }
                do { return .applied(source: try await observedEntry(written, photo: photoState)) }
                catch { return .unknown }
            case .outcomeUnknown: return .unknown
            case .outOfGrant: return .failed("out_of_grant")
            case .readOnly: return .failed("read_only_account")
            case .notFound: return .failed("not_found")
            }
        } catch ContactsCoreMapping.PatchError.unsupported {
            return .failed("unsupported_field")
        } catch ContactsCoreMapping.PatchError.missingItem {
            return .failed("stale_item")
        } catch ContactsAdapterError.photoTooLarge {
            return .failed("photo_too_large")
        } catch {
            // Contacts framework save requests are atomic: a thrown save did not write. Errors after a
            // write are reported as `.applied`/`.unknown` above, never as failures.
            return .failed("platform_error")
        }
    }

    func createMatches(_ request: [String: Any], verifyPhoto: Bool = true, isCancelled: @escaping @Sendable () -> Bool) async throws -> ContactsFetch {
        let all = try await provider.fetchAll(isCancelled: isCancelled)
        let expected = ContactsCoreMapping.platform(request, id: "", knownItemIds: [:])
        let expectedPhoto = try (verifyPhoto ? Self.photoAttachment(request) : nil).map {
            ContactPhotoSource.sourceHash(try photoBytes($0))
        }
        var matches: [PlatformContact] = []
        for candidate in all.contacts where ContactsCoreMapping.createContentMatches(candidate, expected) {
            if isCancelled() { return ContactsFetch(contacts: matches, complete: false) }
            if let expectedPhoto {
                guard case let .data(bytes) = try await provider.photo(id: candidate.id, thumbnail: false),
                      ContactPhotoSource.sourceHash(bytes) == expectedPhoto else { continue }
            }
            matches.append(candidate)
            if matches.count > 200 { return ContactsFetch(contacts: matches, complete: false, limitExceeded: true) }
        }
        // Before-write evidence intentionally includes all text matches, even with a different
        // photo, so changing an existing contact's image cannot make it look newly created.
        return ContactsFetch(contacts: matches, complete: all.complete && !all.limitExceeded, limitExceeded: all.limitExceeded)
    }

    private func currentMatches(_ fresh: PlatformContact, current: [String: Any]?, context: SourceContext) -> Bool {
        guard let current else { return false }
        let mapped = ContactsCoreMapping.coreBody(observed: ContactsCoreMapping.fields(fresh), fieldSources: context.fieldSources)
        return ContactsCoreMapping.modeledBody(mapped) == ContactsCoreMapping.modeledBody(current)
    }

    private func photoMatches(_ fresh: PlatformContact, current: [String: Any]?, context: SourceContext) async -> Bool {
        let stored = context.provenance["photo_source_hash"] as? String
        let hasStoredPhoto = current?["photo"] != nil && !(current?["photo"] is NSNull)
        guard fresh.hasImage else { return stored == nil && !hasStoredPhoto }
        guard let observed = try? await provider.photo(id: fresh.id, thumbnail: false),
              case let .data(bytes) = observed else { return false }
        return stored == ContactPhotoSource.sourceHash(bytes)
    }

    /// The post-write platform entry for `observed_source`: OS text plus the photo intent, with the
    /// fingerprint of the bytes the OS now holds.
    func observedEntry(_ contact: PlatformContact, photo: EntryPhoto) async throws -> [String: Any] {
        switch photo {
        case .keep:
            return Self.entry(contact, photo: photo, hash: nil)
        case .remove:
            guard !contact.hasImage else { throw ContactsCoordinatorError.invalidCoreResponse("photo removal not observed") }
            return Self.entry(contact, photo: .remove, hash: nil)
        case let .set(requested):
            guard case let .data(bytes) = try await provider.photo(id: contact.id, thumbnail: false) else {
                throw ContactsCoordinatorError.invalidCoreResponse("saved photo unavailable")
            }
            let hash = ContactPhotoSource.sourceHash(bytes)
            let reference: String
            if hash == ContactPhotoSource.sourceHash(try photoBytes(requested)) {
                reference = requested
            } else {
                // The OS may re-encode or replace the image. Publish what was actually read,
                // never pair the requested attachment with a different provider fingerprint.
                guard let actual = prepare(bytes) else { throw ContactsCoordinatorError.invalidCoreResponse("saved photo invalid") }
                reference = actual
            }
            return Self.entry(contact, photo: .set(reference), hash: hash)
        }
    }

    private func record(_ outcome: ApplyOutcome, requestId: String, report: inout ContactsSyncReport) throws {
        var input: [String: Any] = ["schema_version": 1, "request_id": requestId]
        switch outcome {
        case let .applied(source):
            input["outcome"] = "applied"
            if let source { input["observed_source"] = source }
            report.applied += 1
        case .unknown:
            input["outcome"] = "unknown"
            report.outcomeUnknown += 1
        case let .failed(reason):
            input["outcome"] = "failed"
            input["reason"] = reason
            report.failed += 1
        }
        _ = try call(client.reconcileContactApply, input)
    }

    // MARK: JSON

    private func call(_ method: (String) throws -> String, _ input: [String: Any]) throws -> [String: Any] {
        try Self.object(method(Self.json(input)))
    }

    static func json(_ value: [String: Any]) throws -> String {
        String(decoding: try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]), as: UTF8.self)
    }

    static func object(_ text: String) throws -> [String: Any] {
        guard let value = try JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any] else {
            throw ContactsCoordinatorError.invalidCoreResponse("json")
        }
        return value
    }
}

/// Pure conversion between provider contacts and core contact JSON. Labels are canonical tokens for
/// standard Contacts labels and verbatim for custom ones, so round trips are deterministic.
enum ContactsCoreMapping {
    enum PatchError: Error, Equatable { case unsupported, missingItem }

    private static let labels: [(raw: String, token: String)] = [
        ("_$!<Mobile>!$_", "mobile"), ("iPhone", "iphone"), ("_$!<Home>!$_", "home"), ("_$!<Work>!$_", "work"),
        ("_$!<Main>!$_", "main"), ("_$!<HomeFAX>!$_", "home_fax"), ("_$!<WorkFAX>!$_", "work_fax"),
        ("_$!<OtherFAX>!$_", "other_fax"), ("_$!<Pager>!$_", "pager"), ("_$!<Other>!$_", "other"),
        ("_$!<School>!$_", "school"), ("_$!<AppleWatch>!$_", "apple_watch"),
    ]

    static func token(_ raw: String?) -> String? {
        guard let raw else { return nil }
        return labels.first { $0.raw == raw }?.token ?? raw
    }

    static func raw(_ token: String?) -> String? {
        guard let token else { return nil }
        return labels.first { $0.token == token }?.raw ?? token
    }

    static var addressKeys: [(json: String, path: WritableKeyPath<PostalAddressValue, String>)] {
        [
            ("street", \.street), ("sub_locality", \.subLocality), ("city", \.city),
            ("sub_administrative_area", \.subAdministrativeArea), ("state", \.state), ("postal_code", \.postalCode),
            ("country", \.country), ("iso_country_code", \.isoCountryCode),
        ]
    }

    /// Core `fields` for capture. `photo` is never set here: callers add it only after inspecting
    /// the OS photo (an omitted photo keeps the stored one in core).
    static func fields(_ c: PlatformContact) -> [String: Any] {
        var out: [String: Any] = ["display_name": displayName(c)]
        var name: [String: Any] = [:]
        for (key, value) in [("given", c.givenName), ("family", c.familyName), ("middle", c.middleName),
                             ("prefix", c.namePrefix), ("suffix", c.nameSuffix)] where !value.isEmpty {
            name[key] = value
        }
        if !name.isEmpty { out["name"] = name }
        if !c.nickname.isEmpty { out["nickname"] = c.nickname }
        if !c.organization.isEmpty { out["organization"] = c.organization }
        if !c.jobTitle.isEmpty { out["title"] = c.jobTitle }
        let item = { (f: ContactField) -> [String: Any] in
            var v: [String: Any] = ["id": f.id, "value": f.value]
            if let label = token(f.label) { v["label"] = label }
            return v
        }
        if let birthday = c.birthday, let month = birthday.month, let day = birthday.day {
            var value: [String: Any] = ["month": month, "day": day]
            if let year = birthday.year { value["year"] = year }
            out["birthday"] = value
        }
        if !c.phones.isEmpty { out["phones"] = c.phones.map(item) }
        if !c.emails.isEmpty { out["emails"] = c.emails.map(item) }
        if !c.postalAddresses.isEmpty {
            out["addresses"] = c.postalAddresses.map { a -> [String: Any] in
                var v: [String: Any] = ["id": a.id]
                if let label = token(a.label) { v["label"] = label }
                for key in addressKeys where !a.value[keyPath: key.path].isEmpty { v[key.json] = a.value[keyPath: key.path] }
                return v
            }
        }
        return out
    }

    static func displayName(_ c: PlatformContact) -> String {
        let person = [c.namePrefix, c.givenName, c.middleName, c.familyName, c.nameSuffix].filter { !$0.isEmpty }.joined(separator: " ")
        let candidates = [person, c.nickname, c.organization, c.phones.first?.value ?? "", c.emails.first?.value ?? ""]
        return String((candidates.first { !$0.isEmpty } ?? "").prefix(1024))
    }

    static func modeledBody(_ body: [String: Any]) -> String {
        let keys = ["display_name", "name", "nickname", "organization", "title", "birthday", "phones", "emails", "addresses"]
        let value = Dictionary(uniqueKeysWithValues: keys.compactMap { key in body[key].map { (key, $0) } })
        return (try? String(decoding: JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]), as: UTF8.self)) ?? ""
    }

    static func createContentMatches(_ candidate: PlatformContact, _ expected: PlatformContact) -> Bool {
        func content(_ contact: PlatformContact) -> String {
            var body = fields(contact)
            for key in ["phones", "emails", "addresses"] {
                if let items = body[key] as? [[String: Any]] {
                    body[key] = items.map { item in
                        var value = item
                        value.removeValue(forKey: "id")
                        return value
                    }
                }
            }
            return modeledBody(body)
        }
        return content(candidate) == content(expected)
    }

    static func preserveUnrepresentedNativeValues(from fresh: PlatformContact, in desired: inout PlatformContact, patches: [[String: Any]]) {
        let touched = Set(patches.compactMap { $0["path"] as? String })
        if !touched.contains("birthday"), fresh.birthday?.month == nil || fresh.birthday?.day == nil { desired.birthday = fresh.birthday }
        func labels(_ fresh: [ContactField], _ desired: [ContactField], field: String) -> [ContactField] {
            let byId = Dictionary(uniqueKeysWithValues: fresh.map { ($0.id, $0.label) })
            return desired.map { item in
                guard !item.id.isEmpty, let label = byId[item.id], token(label) == token(item.label) else { return item }
                return ContactField(id: item.id, label: label, value: item.value)
            }
        }
        desired.phones = labels(fresh.phones, desired.phones, field: "phones")
        desired.emails = labels(fresh.emails, desired.emails, field: "emails")
        let addressLabels = Dictionary(uniqueKeysWithValues: fresh.postalAddresses.map { ($0.id, $0.label) })
        desired.postalAddresses = desired.postalAddresses.map { item in
            guard let label = addressLabels[item.id], token(label) == token(item.label) else { return item }
            return ContactAddressField(id: item.id, label: label, value: item.value)
        }
    }

    /// Desired provider contact from core JSON. Items whose id is not a known native item become new.
    static func platform(_ f: [String: Any], id: String, knownItemIds: [String: Set<String>]) -> PlatformContact {
        let name = f["name"] as? [String: Any] ?? [:]
        let text = { (key: String) in f[key] as? String ?? "" }
        let itemId = { (field: String, item: [String: Any]) -> String in
            let value = item["id"] as? String ?? ""
            return knownItemIds[field, default: []].contains(value) ? value : ""
        }
        let list = { (field: String) -> [ContactField] in
            (f[field] as? [[String: Any]] ?? []).map {
                ContactField(id: itemId(field, $0), label: raw($0["label"] as? String), value: $0["value"] as? String ?? "")
            }
        }
        let addresses = (f["addresses"] as? [[String: Any]] ?? []).map { item -> ContactAddressField in
            var value = PostalAddressValue()
            for key in addressKeys { value[keyPath: key.path] = item[key.json] as? String ?? "" }
            return ContactAddressField(id: itemId("addresses", item), label: raw(item["label"] as? String), value: value)
        }
        let birthday = (f["birthday"] as? [String: Any]).flatMap { b -> ContactBirthday? in
            guard let month = b["month"] as? Int, let day = b["day"] as? Int else { return nil }
            return ContactBirthday(year: b["year"] as? Int, month: month, day: day)
        }
        return PlatformContact(
            id: id, givenName: name["given"] as? String ?? "", middleName: name["middle"] as? String ?? "",
            familyName: name["family"] as? String ?? "", namePrefix: name["prefix"] as? String ?? "",
            nameSuffix: name["suffix"] as? String ?? "", nickname: text("nickname"), organization: text("organization"),
            jobTitle: text("title"), phones: list("phones"), emails: list("emails"), postalAddresses: addresses,
            birthday: birthday
        )
    }

    static func nativeIds(_ fieldSources: [[String: Any]]) -> [String: Set<String>] {
        var out: [String: Set<String>] = [:]
        for source in fieldSources {
            if let field = source["field"] as? String, let id = source["source_id"] as? String { out[field, default: []].insert(id) }
        }
        return out
    }

    /// Native → core item IDs, so a scan observation matches the captured body exactly.
    static func coreBody(observed: [String: Any], fieldSources: [[String: Any]]) -> [String: Any] {
        var map: [String: [String: String]] = [:]
        for source in fieldSources {
            if let field = source["field"] as? String, let native = source["source_id"] as? String, let core = source["id"] as? String {
                map[field, default: [:]][native] = core
            }
        }
        var body = observed
        for field in ["phones", "emails", "addresses"] {
            guard let items = body[field] as? [[String: Any]] else { continue }
            body[field] = items.map { item -> [String: Any] in
                var item = item
                if let native = item["id"] as? String, let core = map[field]?[native] { item["id"] = core }
                return item
            }
        }
        return body
    }

    /// Applies validated core patches to the observed (native-ID) contact. Core item IDs in paths are
    /// translated through the owner's field map. `display_name` is derived on iOS and not written.
    static func applying(_ patches: [[String: Any]], to observed: [String: Any], fieldSources: [[String: Any]]) throws -> [String: Any] {
        var toNative: [String: [String: String]] = [:]
        for source in fieldSources {
            if let field = source["field"] as? String, let native = source["source_id"] as? String, let core = source["id"] as? String {
                toNative[field, default: [:]][core] = native
            }
        }
        var body = observed
        for patch in patches {
            guard let op = patch["op"] as? String, let path = patch["path"] as? String else { throw PatchError.unsupported }
            let value = patch["value"]
            switch path {
            case "display_name": continue
            case "notes": throw PatchError.unsupported
            case "nickname", "organization", "title":
                body[path] = op == "replace" ? value : nil
                continue
            case "birthday":
                body[path] = op == "remove" ? nil : value
                continue
            default: break
            }
            if path.hasPrefix("name.") {
                var name = body["name"] as? [String: Any] ?? [:]
                name[String(path.dropFirst(5))] = op == "replace" ? value : nil
                body["name"] = name
                continue
            }
            if ["phones", "emails", "addresses"].contains(path), op == "add", let item = value as? [String: Any] {
                body[path] = (body[path] as? [[String: Any]] ?? []) + [item]
                continue
            }
            guard let open = path.firstIndex(of: "["), path.hasSuffix("]") else { throw PatchError.unsupported }
            let field = String(path[..<open])
            let core = String(path[path.index(after: open)..<path.index(before: path.endIndex)])
            let native = toNative[field]?[core] ?? core
            var items = body[field] as? [[String: Any]] ?? []
            guard let index = items.firstIndex(where: { $0["id"] as? String == native }) else { throw PatchError.missingItem }
            switch op {
            case "remove": items.remove(at: index)
            case "replace":
                guard var item = value as? [String: Any] else { throw PatchError.unsupported }
                item["id"] = native
                items[index] = item
            default: throw PatchError.unsupported
            }
            body[field] = items
        }
        return body
    }
}
