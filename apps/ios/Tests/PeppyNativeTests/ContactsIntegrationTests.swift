import Foundation
import PeppyBindings
import Testing
@testable import PeppyNative

/// In-memory Contacts store that behaves like the corrected adapter: non-unified identifiers,
/// labeled values keep their ids, new values get fresh ids. Never touches a real contact store.
actor WritableFakeProvider: ContactsProvider {
    var state: ContactsAccess = .full
    var book: [PlatformContact] = []
    var photos: [String: Data] = [:]
    var visible: Set<String>?
    var createOutcome: ContactsWriteResult?
    /// When set, a create that reports `createOutcome` still lands in the store (ambiguous success).
    var createLandsAnyway = false
    var saves: [ContactsWrite] = []
    var photoReads = 0
    var failPhotoAfterSave = false
    var hidePhotoAfterSave = false
    var legacyFetchLimit: Int?
    /// OS change history: (sequence, contact id, deleted). Off unless `historyEnabled`.
    var historyEnabled = false
    private var log: [(seq: Int, id: String, deleted: Bool)] = []
    private var seq = 1
    private var next = 0

    func configure(_ body: @Sendable (isolated WritableFakeProvider) -> Void) { body(self) }
    func access() async throws -> ContactsAccess { state }
    func containers() async throws -> [ContactContainer] {
        state.mayRead ? [ContactContainer(id: "icloud", name: "iCloud", isDefault: true), ContactContainer(id: "gmail", name: "Gmail", isDefault: false)] : []
    }
    private func shown(_ c: PlatformContact) -> PlatformContact {
        var c = c
        c.hasImage = photos[c.id] != nil
        return c
    }
    func fetchAll(isCancelled: @Sendable () -> Bool) async throws -> ContactsFetch {
        var out: [PlatformContact] = []
        for contact in book where visible?.contains(contact.id) ?? true {
            if let legacyFetchLimit, out.count >= legacyFetchLimit { return ContactsFetch(contacts: out, complete: false) }
            if isCancelled() { return ContactsFetch(contacts: out, complete: false) }
            out.append(shown(contact))
        }
        return ContactsFetch(contacts: out, complete: true)
    }
    func contact(id: String) async throws -> PlatformContact? {
        guard visible?.contains(id) ?? true else { return nil }
        return book.first { $0.id == id }.map(shown)
    }
    func fetchPage(after: String?, limit: Int, isCancelled: @Sendable () -> Bool) async throws -> ContactsPage {
        let remaining = book.filter { visible?.contains($0.id) ?? true }.sorted { $0.id < $1.id }
            .filter { contact in after.map { contact.id > $0 } ?? true }
        let page = Array(remaining.prefix(limit)).map(shown)
        return ContactsPage(contacts: page, nextCursor: page.last?.id ?? after,
                            hasMore: remaining.count > page.count, complete: !isCancelled())
    }
    var currentToken: Data { Data(String(seq).utf8) }
    func historyToken() async -> Data? { currentToken }
    func changes(since token: Data, limit: Int) async -> ContactsHistoryChanges {
        guard historyEnabled, let from = Int(String(decoding: token, as: UTF8.self)) else { return .fullScanRequired }
        var ids: [String] = []
        for event in log where event.seq > from {
            if event.deleted { return .fullScanRequired }
            if !ids.contains(event.id) { ids.append(event.id) }
        }
        return ids.count > limit ? .fullScanRequired : .changed(ids: ids, token: currentToken)
    }
    func photo(id: String, thumbnail: Bool) async throws -> ContactPhotoRead {
        photoReads += 1
        if failPhotoAfterSave && !saves.isEmpty { throw ClientError.invalidResponse("photo unavailable") }
        if hidePhotoAfterSave && !saves.isEmpty { return .none }
        return photos[id].map(ContactPhotoRead.data) ?? .none
    }
    func identifiers(matchingName name: String) async throws -> [String]? {
        book.filter { ContactsCoreMapping.displayName($0) == name }.map(\.id)
    }

    private func changed(_ id: String, deleted: Bool = false) {
        seq += 1
        log.append((seq, id, deleted))
    }

    func save(_ write: ContactsWrite) async throws -> ContactsWriteResult {
        saves.append(write)
        let apply = { (id: String, change: ContactPhotoChange) in
            switch change {
            case .keep: break
            case let .set(data): self.photos[id] = data
            case .remove: self.photos[id] = nil
            }
        }
        switch write {
        case let .create(contact, containerId, photo):
            if let createOutcome, !createLandsAnyway { return createOutcome }
            let id = fresh("os")
            book.append(identified(contact, id: id, containerId: containerId))
            apply(id, photo)
            changed(id)
            return createOutcome ?? .applied(id: id)
        case let .update(id, contact, photo):
            guard let index = book.firstIndex(where: { $0.id == id }) else { return .notFound }
            book[index] = identified(contact, id: id, containerId: book[index].containerId)
            apply(id, photo)
            changed(id)
            return .applied(id: id)
        case let .delete(id):
            guard let index = book.firstIndex(where: { $0.id == id }) else { return .notFound }
            book.remove(at: index)
            photos[id] = nil
            changed(id, deleted: true)
            return .applied(id: id)
        }
    }

    /// Limited-grant provider whose saves report the contact as outside the grant.
    struct OutOfGrant: ContactsProvider {
        func access() async throws -> ContactsAccess { .limited }
        func containers() async throws -> [ContactContainer] { [] }
        func fetchAll(isCancelled: @Sendable () -> Bool) async throws -> ContactsFetch { ContactsFetch(contacts: [], complete: true) }
        func contact(id: String) async throws -> PlatformContact? { nil }
        func historyToken() async -> Data? { nil }
        func photo(id: String, thumbnail: Bool) async throws -> ContactPhotoRead { .none }
        func save(_ write: ContactsWrite) async throws -> ContactsWriteResult { .outOfGrant }
    }

    func add(_ contact: PlatformContact) { book.append(contact); changed(contact.id) }
    func remove(_ id: String) { book.removeAll { $0.id == id }; photos[id] = nil; changed(id, deleted: true) }
    func rename(_ id: String, given: String) {
        guard let index = book.firstIndex(where: { $0.id == id }) else { return }
        book[index].givenName = given
        changed(id)
    }
    func setPhoto(_ id: String, _ data: Data?) { photos[id] = data; changed(id) }

    private func fresh(_ prefix: String) -> String { next += 1; return "\(prefix)-\(next)" }
    private func identified(_ c: PlatformContact, id: String, containerId: String?) -> PlatformContact {
        var c = c
        c = PlatformContact(
            id: id, containerId: containerId, givenName: c.givenName, middleName: c.middleName, familyName: c.familyName,
            namePrefix: c.namePrefix, nameSuffix: c.nameSuffix, nickname: c.nickname, organization: c.organization,
            jobTitle: c.jobTitle,
            phones: c.phones.map { ContactField(id: $0.id.isEmpty ? fresh("ph") : $0.id, label: $0.label, value: $0.value) },
            emails: c.emails.map { ContactField(id: $0.id.isEmpty ? fresh("em") : $0.id, label: $0.label, value: $0.value) },
            postalAddresses: c.postalAddresses.map { ContactAddressField(id: $0.id.isEmpty ? fresh("ad") : $0.id, label: $0.label, value: $0.value) },
            birthday: c.birthday
        )
        return c
    }
}

private final class Flag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    private var count = 0
    func get() -> Bool { lock.withLock { value } }
    func set() { lock.withLock { value = true } }
    func after(_ calls: Int) -> Bool { lock.withLock { count += 1; return count > calls } }
}

func sampleContacts(_ count: Int) -> [PlatformContact] {
    (0..<count).map { index in
        PlatformContact(
            id: "os-c\(index)", containerId: "icloud", givenName: "Given\(index)", familyName: "Family",
            phones: [ContactField(id: "ph-c\(index)", label: "_$!<Mobile>!$_", value: "+1202555\(String(format: "%04d", index))")],
            postalAddresses: index == 0 ? [ContactAddressField(id: "ad-c0", label: "_$!<Home>!$_", value: PostalAddressValue(street: "1 Main", city: "Springfield"))] : []
        )
    }
}

/// Real SQLCipher core + fake OS provider: capture, scans, permits and reconciliation.
@Suite(.serialized) struct ContactsIntegrationTests {
    let passphrase = "io synthetic vault passphrase"

    struct Owner {
        let server: FakeServer
        let deviceId: String
        let client: NativeClient
        let provider: WritableFakeProvider
        let preferences: InMemoryContactsPreferences
        var coordinator: NativeContactsCoordinator {
            NativeContactsCoordinator(client: client, deviceId: deviceId, provider: provider, preferences: preferences)
        }
        func run(_ reason: ContactsPassReason = .foreground, cancelled: @escaping @Sendable () -> Bool = { false }) async throws -> ContactsSyncReport {
            try await coordinator.run(reason: reason, isCancelled: cancelled)
        }
        func view() throws -> [[String: Any]] {
            let bookId = try #require(try coordinator.ownedBook()?.id)
            let out = try NativeContactsCoordinator.object(client.contactBookView(inputJson: NativeContactsCoordinator.json(["book_id": bookId, "limit": 200])))
            return out["contacts"] as? [[String: Any]] ?? []
        }
    }

    func owner(_ contacts: [PlatformContact]) async throws -> Owner {
        let server = try FakeServer(passphrase: passphrase)
        let deviceId = UUID().uuidString.lowercased()
        let provider = WritableFakeProvider()
        await provider.configure { $0.book = contacts }
        return Owner(
            server: server, deviceId: deviceId, client: try openUnlockedClient(server, passphrase: passphrase, deviceId: deviceId),
            provider: provider, preferences: InMemoryContactsPreferences(ContactsPreferences(enabled: true))
        )
    }

    @Test func disabledPassTouchesNothing() async throws {
        let owner = try await owner(sampleContacts(2))
        owner.preferences.save(ContactsPreferences(enabled: false))
        let report = try await owner.run()
        #expect(report.outcome == .disabled)
        #expect(try owner.coordinator.ownedBook() == nil)
    }

    @Test func inactiveCompactionRosterPausesRetentionButDoesNotBlockContacts() async throws {
        let owner = try await owner(sampleContacts(2))
        _ = try owner.client.setServerCompactionState(supported: true, active: false)

        let report = try await owner.run()

        #expect(report.outcome == .completed && report.captured == 2)
        #expect(report.retentionPaused)
    }

    @Test func captureMintsStableCoreIdentityAndCompletesAuthoritativeScan() async throws {
        let owner = try await owner(sampleContacts(3))
        let first = try await owner.run()
        #expect(first.outcome == .completed && first.authoritative && first.captured == 3 && first.deleted == 0)
        let view = try owner.view()
        #expect(view.count == 3)
        #expect(!view.contains { ($0["id"] as? String)?.hasPrefix("os-") ?? true }, "core mints contact ids")
        let phones = view.flatMap { $0["phones"] as? [[String: Any]] ?? [] }
        #expect(phones.allSatisfy { ($0["label"] as? String) == "mobile" && !(($0["id"] as? String) ?? "").hasPrefix("ph-") })
        let encoded = try owner.client.contactBookView(inputJson: NativeContactsCoordinator.json(["book_id": try #require(first.bookId)]))
        #expect(!encoded.contains("os-c0") && !encoded.contains("icloud"), "provider ids never leave the owner tables")

        let revisions = Dictionary(uniqueKeysWithValues: view.map { ($0["id"] as! String, $0["revision"] as! String) })
        let second = try await owner.run()
        #expect(second.outcome == .completed && second.bookId == first.bookId)
        let again = try owner.view()
        #expect(Dictionary(uniqueKeysWithValues: again.map { ($0["id"] as! String, $0["revision"] as! String) }) == revisions,
                "an unchanged scan mints no revisions (observation matches the captured body)")
    }

    @Test func onlyCompleteFullAccessScansRemoveContacts() async throws {
        let owner = try await owner(sampleContacts(30))
        _ = try await owner.run()
        await owner.provider.remove("os-c1")

        // Limited grant showing a few contacts: nothing is inferred as deleted.
        await owner.provider.configure { $0.state = .limited; $0.visible = ["os-c0", "os-c2"] }
        let limited = try await owner.run()
        #expect(limited.outcome == .partial && !limited.authoritative && limited.deleted == 0)
        #expect(try owner.view().count == 30)

        // Cancelled while capturing: partial, no deletions.
        await owner.provider.configure { $0.state = .full; $0.visible = nil }
        let flag = Flag()
        let cancelled = try await owner.run(cancelled: { flag.after(3) })
        #expect(cancelled.outcome == .partial && cancelled.deleted == 0)
        #expect(try owner.view().count == 30)

        // Revoked: published unavailable, never emptied.
        await owner.provider.configure { $0.state = .denied }
        let revoked = try await owner.run()
        #expect(revoked.outcome == .noAccess)
        #expect(try owner.view().count == 30)
        let books = try NativeContactsCoordinator.object(owner.client.listContactBooksJson())["books"] as! [[String: Any]]
        #expect(books.first?["state"] as? String == "unavailable")

        // Complete full scan removes exactly the missing contact.
        await owner.provider.configure { $0.state = .full }
        let complete = try await owner.run()
        #expect(complete.outcome == .completed && complete.deleted == 1)
        #expect(try owner.view().count == 29)
    }

    @Test func historyFastPathCapturesOnlyChangedContactsAndFallsBackOnDeletion() async throws {
        let owner = try await owner(sampleContacts(5))
        await owner.provider.configure { $0.historyEnabled = true }
        let first = try await owner.run()
        #expect(first.outcome == .completed)
        let unchanged = try await owner.run(.backgroundRefresh)
        #expect(unchanged.outcome == .unchanged && unchanged.captured == 0)

        await owner.provider.rename("os-c3", given: "Renamed")
        await owner.provider.add(PlatformContact(id: "os-new", containerId: "icloud", givenName: "New"))
        let incremental = try await owner.run(.backgroundRefresh)
        #expect(incremental.outcome == .incremental && incremental.captured == 2 && !incremental.authoritative)
        let names = try owner.view().compactMap { ($0["name"] as? [String: Any])?["given"] as? String }
        #expect(names.contains("Renamed") && names.contains("New") && names.count == 6)

        // A deletion in history is never applied from history: the full scan (and its policy) handles it.
        await owner.provider.remove("os-c1")
        let fallback = try await owner.run(.backgroundRefresh)
        #expect(fallback.outcome == .completed && fallback.deleted == 1)
        #expect(try owner.view().count == 5)
    }

    @Test func limitedAccessNeverAnchorsHistoryOrDeletes() async throws {
        let owner = try await owner(sampleContacts(4))
        await owner.provider.configure { $0.historyEnabled = true }
        _ = try await owner.run()
        await owner.provider.configure { $0.state = .limited; $0.visible = ["os-c0"] }
        let limited = try await owner.run(.backgroundRefresh)
        #expect(limited.outcome == .partial && limited.deleted == 0)
        #expect(try owner.view().count == 4)
        let checkpoint = try owner.coordinator.readCheckpoint(try #require(limited.bookId))
        #expect(checkpoint.historyToken == nil, "a limited scan clears the history anchor")
    }

    // MARK: Edits from another device

    func requester(_ owner: Owner) throws -> NativeClient {
        try openUnlockedClient(owner.server, passphrase: passphrase)
    }

    func deliver(from requester: NativeClient, to owner: Owner) async throws {
        _ = try await ForegroundSync(client: requester, server: serverClient(owner.server), vaultId: owner.server.vaultId).run()
        _ = try await ForegroundSync(client: owner.client, server: serverClient(owner.server), vaultId: owner.server.vaultId).run()
    }

    func request(_ owner: Owner, bookId: String, _ body: [String: Any]) -> [String: Any] {
        var request: [String: Any] = ["schema_version": 1, "request_id": UUID().uuidString.lowercased(), "target_owner": owner.deviceId, "book_id": bookId]
        request.merge(body) { $1 }
        return request
    }

    func ledgerState(_ owner: Owner, _ requestId: String) throws -> String? {
        let bookId = try #require(try owner.coordinator.ownedBook()?.id)
        let listed = try NativeContactsCoordinator.object(owner.client.listContactRequestsJson(input: NativeContactsCoordinator.json(["book_id": bookId])))
        return (listed["requests"] as? [[String: Any]])?.first { $0["request_id"] as? String == requestId }?["state"] as? String
    }

    @Test func confirmedUpdateKeepsUnrelatedItemsAndReconcilesObservedState() async throws {
        let owner = try await owner(sampleContacts(2))
        let report = try await owner.run()
        let bookId = try #require(report.bookId)
        #expect(try owner.coordinator.overview().remoteEdits == "auto", "a new book uses the core default")
        try owner.coordinator.setRemoteEdits("confirm")
        let target = try #require(try owner.view().first { ($0["name"] as? [String: Any])?["given"] as? String == "Given0" })
        let phone = try #require((target["phones"] as? [[String: Any]])?.first)
        let requester = try requester(owner)
        let edit = request(owner, bookId: bookId, [
            "kind": "update", "contact_id": target["id"]!, "base_revision": target["revision"]!,
            "patches": [
                ["op": "replace", "path": "name.given", "value": "Augusta"],
                ["op": "replace", "path": "phones[\(phone["id"] as! String)]", "value": ["id": phone["id"]!, "label": "work", "value": "+12025559999"]],
                ["op": "add", "path": "emails", "value": ["id": "requester-item", "label": "home", "value": "ada@example.com"]],
            ],
        ])
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json(edit))
        try await deliver(from: requester, to: owner)

        let waiting = try await owner.run()
        #expect(waiting.awaitingApproval == [edit["request_id"] as! String])
        #expect(await owner.provider.saves.isEmpty, "confirm mode never writes before approval")

        try owner.coordinator.decide(edit["request_id"] as! String, approve: true)
        let applied = try await owner.run()
        #expect(applied.applied == 1 && applied.failed == 0)
        let written = try #require(await owner.provider.book.first { $0.id == "os-c0" })
        #expect(written.givenName == "Augusta")
        #expect(written.phones.map(\.id) == ["ph-c0"], "edited item keeps its OS identity")
        #expect(written.phones.first?.label == "_$!<Work>!$_" && written.phones.first?.value == "+12025559999")
        #expect(written.emails.count == 1 && written.emails[0].id.hasPrefix("em-"), "added item is new in the OS")
        #expect(written.postalAddresses.map(\.id) == ["ad-c0"], "unpatched items are untouched")
        #expect(try ledgerState(owner, edit["request_id"] as! String) == "applied")
        let after = try #require(try owner.view().first { $0["id"] as? String == target["id"] as? String })
        #expect((after["name"] as? [String: Any])?["given"] as? String == "Augusta")
        // A further pass takes no new permit for the terminal request.
        _ = try await owner.run()
        #expect(await owner.provider.saves.count == 1)
    }

    @Test func autoCreateAndDeleteUseOSEvidence() async throws {
        let owner = try await owner(sampleContacts(25))
        let bookId = try #require(try await owner.run().bookId)
        try owner.coordinator.setRemoteEdits("auto")
        let requester = try requester(owner)
        let create = request(owner, bookId: bookId, [
            "kind": "create", "display_name": "Grace Hopper", "name": ["given": "Grace", "family": "Hopper"],
            "phones": [["id": "r1", "label": "mobile", "value": "+12025550111"]],
        ])
        let victim = try #require(try owner.view().first)
        let delete = request(owner, bookId: bookId, ["kind": "delete", "contact_id": victim["id"]!, "base_revision": victim["revision"]!])
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json(create))
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json(delete))
        try await deliver(from: requester, to: owner)

        let report = try await owner.run()
        #expect(report.applied == 2 && report.failed == 0 && report.outcomeUnknown == 0)
        let created = try #require(await owner.provider.book.first { $0.givenName == "Grace" })
        #expect(created.containerId == "icloud" && created.phones.first?.label == "_$!<Mobile>!$_")
        #expect(try ledgerState(owner, create["request_id"] as! String) == "applied")
        #expect(try ledgerState(owner, delete["request_id"] as! String) == "applied")
        let names = try owner.view().compactMap { ($0["name"] as? [String: Any])?["given"] as? String }
        #expect(names.contains("Grace") && !names.contains((victim["name"] as? [String: Any])?["given"] as? String ?? "?"))
        #expect(names.count == 25)
    }

    @Test func ambiguousCreatesAreSettledFromEvidenceAndNeverRewritten() async throws {
        let owner = try await owner(sampleContacts(1))
        let bookId = try #require(try await owner.run().bookId)
        let requester = try requester(owner)
        // One create that did not land and one that landed, both reported as ambiguous by the OS.
        let lost = request(owner, bookId: bookId, ["kind": "create", "display_name": "Maybe", "name": ["given": "Maybe"]])
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json(lost))
        try await deliver(from: requester, to: owner)
        await owner.provider.configure { $0.createOutcome = .outcomeUnknown }
        let first = try await owner.run()
        #expect(first.outcomeUnknown == 1 && first.applied == 0)
        let second = try await owner.run()
        #expect(second.outcomeUnknown == 1, "absence from a bounded content search is never proof a create did not land")
        #expect(try ledgerState(owner, lost["request_id"] as! String) == "outcome_unknown")

        let landed = request(owner, bookId: bookId, ["kind": "create", "display_name": "Landed", "name": ["given": "Landed"]])
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json(landed))
        try await deliver(from: requester, to: owner)
        await owner.provider.configure { $0.createLandsAnyway = true }
        let third = try await owner.run()
        #expect(third.outcomeUnknown == 2, "the earlier uncertain create remains unresolved")
        let fourth = try await owner.run()
        #expect(fourth.applied == 1, "exactly one new contact matches the evidence")
        #expect(try ledgerState(owner, landed["request_id"] as! String) == "applied")
        #expect(await owner.provider.saves.count == 2, "ambiguous creates are never written again")
        #expect(try owner.view().count == 2)
    }

    @Test func staleOSContactIsCapturedAndNeverOverwritten() async throws {
        let owner = try await owner(sampleContacts(1))
        let initial = try await owner.run()
        let target = try #require(try owner.view().first)
        let requester = try requester(owner)
        let edit = request(owner, bookId: try #require(initial.bookId), [
            "kind": "update", "contact_id": target["id"]!, "base_revision": target["revision"]!,
            "patches": [["op": "replace", "path": "nickname", "value": "desktop"]],
        ])
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json(edit))
        try await deliver(from: requester, to: owner)
        await owner.provider.configure {
            $0.historyEnabled = true
            // Model an OS change not yet visible in the incremental history feed.
            $0.book[0].givenName = "Phone edit"
        }

        let report = try await owner.run()
        #expect(report.failed == 1 && report.applied == 0)
        #expect(await owner.provider.saves.isEmpty)
        #expect((await owner.provider.book.first { $0.id == "os-c0" })?.givenName == "Phone edit")
        #expect(try ledgerState(owner, edit["request_id"] as! String) == "failed")
    }

    @Test func fullScanResumesUsingDurableCursor() async throws {
        let owner = try await owner(sampleContacts(401))
        let first = try await owner.run(.backgroundRefresh)
        #expect(first.outcome == .partial && first.captured == 200)
        let state = try owner.coordinator.readCheckpoint(try #require(first.bookId))
        #expect(state.scanId != nil && state.scanCursor != nil)
        let second = try await owner.run(.backgroundRefresh)
        #expect(second.outcome == .partial && second.captured == 200)
        let third = try await owner.run(.backgroundRefresh)
        #expect(third.outcome == .completed && third.captured == 1)
    }

    @Test func fullScanDoesNotRepeatABudgetLimitedProviderPrefix() async throws {
        let owner = try await owner(sampleContacts(401))
        await owner.provider.configure { $0.legacyFetchLimit = 100 }
        let first = try await owner.run(.backgroundRefresh)
        let second = try await owner.run(.backgroundRefresh)
        let third = try await owner.run(.backgroundRefresh)
        #expect(first.captured == 200 && second.captured == 200)
        #expect(third.captured == 1 && third.outcome == .completed)
    }

    @Test func aPhotoOnlyEditCannotOverwriteAnUncapturedPhonePhoto() async throws {
        let owner = try await owner(sampleContacts(1))
        let bookId = try #require(try await owner.run().bookId)
        let target = try #require(try owner.view().first)
        let requester = try requester(owner)
        let edit = request(owner, bookId: bookId, [
            "kind": "update", "contact_id": target["id"]!, "base_revision": target["revision"]!,
            "patches": [], "photo_op": ["op": "remove"],
        ])
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json(edit))
        try await deliver(from: requester, to: owner)
        let permit = try NativeContactsCoordinator.object(owner.client.nextContactApplyPermit(inputJson: NativeContactsCoordinator.json(["request_id": edit["request_id"]!])))
        #expect(permit["status"] as? String == "permit")
        let image = try #require(syntheticImage())
        await owner.provider.configure { $0.photos["os-c0"] = image }
        let book = owner.coordinator.bookBody(id: bookId, generation: "1", access: .full, containers: try await owner.provider.containers())
        let result = await owner.coordinator.apply(request: edit, current: permit["current"] as? [String: Any], requestId: edit["request_id"] as! String, book: book, bookId: bookId, access: .full)
        guard case .failed("stale_item") = result else { Issue.record("new phone photo must win"); return }
        #expect(await owner.provider.saves.isEmpty)
        #expect(await owner.provider.photos["os-c0"] == image)
    }

    @Test(arguments: [true, false]) func postSavePhotoReadFailureStaysUnknown(throwsOnRead: Bool) async throws {
        let owner = try await owner(sampleContacts(1))
        let bookId = try #require(try await owner.run().bookId)
        let file = temporaryDirectory().appendingPathComponent("photo.png")
        try #require(syntheticImage()).write(to: file)
        let photo = try owner.client.prepareContactPhoto(path: file.path).attachmentId
        await owner.provider.configure { $0.failPhotoAfterSave = throwsOnRead; $0.hidePhotoAfterSave = !throwsOnRead }
        let create = request(owner, bookId: bookId, [
            "kind": "create", "display_name": "Saved", "name": ["given": "Saved"],
            "photo": ["attachment_id": photo],
        ])
        let result = await owner.coordinator.apply(request: create, requestId: create["request_id"] as! String, book: [:], bookId: bookId, access: .full)
        guard case .unknown = result else { Issue.record("a committed write must not be reported as failed"); return }
        #expect(await owner.provider.saves.count == 1)
    }

    @Test func createRecoveryRequiresTheRequestedPhotoAsWellAsText() async throws {
        let owner = try await owner([])
        let file = temporaryDirectory().appendingPathComponent("wanted.png")
        try #require(syntheticImage()).write(to: file)
        let id = try owner.client.prepareContactPhoto(path: file.path).attachmentId
        let wrong = try #require(syntheticImage(red: 0.1))
        await owner.provider.configure {
            $0.book = [PlatformContact(id: "candidate", givenName: "New")]
            $0.photos["candidate"] = wrong
        }
        let request: [String: Any] = ["name": ["given": "New"], "photo": ["attachment_id": id]]
        #expect(try await owner.coordinator.createMatches(request, isCancelled: { false }).contacts.isEmpty)
        let wanted = try owner.coordinator.photoBytes(id)
        await owner.provider.configure { $0.photos["candidate"] = wanted }
        #expect(try await owner.coordinator.createMatches(request, isCancelled: { false }).contacts.count == 1)
    }

    @Test(arguments: ["phones", "emails"])
    func interruptedAddressOnlyCreateUsesContentNotProviderItemIDs(field: String) async throws {
        let owner = try await owner(sampleContacts(1))
        let bookId = try #require(try await owner.run().bookId)
        let requester = try requester(owner)
        let value = field == "phones" ? "+12025558888" : "new@example.test"
        let create = request(owner, bookId: bookId, [
            "kind": "create", "display_name": value,
            field: [["id": "draft-item", "label": "work", "value": value]],
        ])
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json(create))
        try await deliver(from: requester, to: owner)
        await owner.provider.configure { $0.createOutcome = .outcomeUnknown; $0.createLandsAnyway = true }
        #expect(try await owner.run().outcomeUnknown == 1)
        #expect(try await owner.run().applied == 1)
        #expect(await owner.provider.saves.count == 1)
    }

    @Test(arguments: [ContactsAccess.full, .limited])
    func oversizedContactDoesNotBlockOtherCaptures(access: ContactsAccess) async throws {
        let owner = try await owner(sampleContacts(1))
        let bookId = try #require(try await owner.run().bookId)
        let before = try owner.coordinator.readCheckpoint(bookId).historyToken
        await owner.provider.configure { $0.historyEnabled = true; $0.state = access }
        await owner.provider.add(PlatformContact(
            id: "oversized", givenName: "Too many emails",
            emails: (0..<21).map { ContactField(id: "e\($0)", label: nil, value: "a\($0)@example.test") }
        ))
        await owner.provider.add(PlatformContact(id: "good", givenName: "Still captured"))

        let report = try await owner.run()
        #expect(report.limitExceeded && !report.authoritative)
        #expect(try owner.view().contains { $0["display_name"] as? String == "Still captured" })
        if access == .full { #expect(try owner.coordinator.readCheckpoint(bookId).historyToken == before) }
        #expect(report.deleted == 0)
    }

    @Test func unsupportedAndOutOfGrantEditsFailExplicitly() async throws {
        let owner = try await owner(sampleContacts(2))
        let bookId = try #require(try await owner.run().bookId)
        try owner.coordinator.setRemoteEdits("auto")
        let target = try #require(try owner.view().first)
        let requester = try requester(owner)
        let notes = request(owner, bookId: bookId, [
            "kind": "update", "contact_id": target["id"]!, "base_revision": target["revision"]!,
            "patches": [["op": "replace", "path": "notes", "value": "secret"]],
        ])
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json(notes))
        try await deliver(from: requester, to: owner)
        let report = try await owner.run()
        #expect(report.failed == 1 && report.applied == 0)
        #expect(await owner.provider.saves.isEmpty)
        #expect(try ledgerState(owner, notes["request_id"] as! String) == "failed")

        // A photo whose plaintext is not available locally fails before any write. (The core only
        // issues such a permit after verifying the download; this checks the write step directly.)
        let photo = request(owner, bookId: bookId, [
            "kind": "update", "contact_id": target["id"]!, "base_revision": target["revision"]!,
            "photo_op": ["op": "set", "attachment_id": UUID().uuidString.lowercased()],
        ])
        let outcome = await owner.coordinator.apply(request: photo, requestId: photo["request_id"] as! String, book: [:], bookId: bookId, access: .full)
        guard case .failed("photo_unavailable") = outcome else { Issue.record("photo set must fail explicitly"); return }
        #expect(await owner.provider.saves.isEmpty)

        // Limited grant: an update of a contact outside the grant fails as out_of_grant.
        await owner.provider.configure { $0.state = .limited }
        let update = request(owner, bookId: bookId, [
            "kind": "update", "contact_id": target["id"]!, "base_revision": target["revision"]!,
            "patches": [["op": "replace", "path": "nickname", "value": "x"]],
        ])
        let limited = WritableFakeProvider.OutOfGrant()
        let coordinator = NativeContactsCoordinator(client: owner.client, deviceId: owner.deviceId, provider: limited, preferences: owner.preferences)
        let denied = await coordinator.apply(request: update, requestId: update["request_id"] as! String, book: [:], bookId: bookId, access: .limited)
        guard case .failed("out_of_grant") = denied else { Issue.record("out-of-grant must fail explicitly"); return }
    }

    @Test func retireKeepsOSContactsAndStartsANewBookOnReenable() async throws {
        let owner = try await owner(sampleContacts(2))
        let first = try #require(try await owner.run().bookId)
        try owner.coordinator.retire()
        #expect(try owner.coordinator.ownedBook() == nil)
        let osCount = await owner.provider.book.count
        let saves = await owner.provider.saves.count
        #expect(osCount == 2 && saves == 0)
        let next = try await owner.run()
        #expect(next.bookId != first && next.captured == 2)
    }
}

/// Session-level pass: background relaunch reopen, unlock gating and real HTTP upload.
@Suite struct ContactsSessionTests {
    let passphrase = "io synthetic vault passphrase"

    @Test func passNeedsUnlockThenSyncsAndUploads() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let deviceId = UUID().uuidString.lowercased()
        let directory = temporaryDirectory()
        let transport = DeviceScopedTransport(server: server, deviceId: deviceId)
        let first = NativeSession(secureStore: store, transport: transport, databaseDirectory: directory, allowLoopbackHTTP: false)
        _ = try await first.importCredential(server.credential(deviceId: deviceId))
        let provider = WritableFakeProvider()
        await provider.configure { $0.book = sampleContacts(2) }
        let preferences = InMemoryContactsPreferences(ContactsPreferences(enabled: true))

        let locked = try await first.contactsPass(reason: .foreground, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(locked.outcome == .needsUnlock)
        _ = try await first.unlock(passphrase: passphrase)
        await first.close()

        // A background relaunch: a new session opens the cached enrollment and key cache itself.
        let relaunched = NativeSession(secureStore: store, transport: transport, databaseDirectory: directory, allowLoopbackHTTP: false)
        let report = try await relaunched.contactsPass(reason: .backgroundProcessing, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(report.outcome == .completed && report.captured == 2 && report.syncError == nil)
        #expect(server.paths.filter { $0 == "POST /v1/events" }.count >= 3, "book state and contacts were uploaded")
        let overview = try await relaunched.contactsOverview()
        #expect(overview.contactCount == 2 && overview.remoteEdits == "auto")
        let updated = try await relaunched.setContactsRemoteEdits("off")
        #expect(updated.remoteEdits == "off")
        await relaunched.close()
    }
}

@Suite struct ContactsSchedulingTests {
    @Test func aRequestDuringAPassQueuesExactlyOneFollowUp() async throws {
        let calls = Recorder()
        let coordinator = ContactsPassCoordinator { reason, _ in
            await calls.append(reason)
            try await Task.sleep(for: .milliseconds(100))
            return true
        }
        async let first = coordinator.run(.foreground)
        try await Task.sleep(for: .milliseconds(20))
        let second = try await coordinator.run(.changeNotification)
        let third = try await coordinator.run(.changeNotification)
        #expect(!second && !third)
        #expect(try await first)
        #expect(await calls.values == [.foreground, .changeNotification])
    }

    @Test func cancellationDropsTheFollowUp() async throws {
        let calls = Recorder()
        let coordinator = ContactsPassCoordinator { reason, isCancelled in
            await calls.append(reason)
            try await Task.sleep(for: .milliseconds(100))
            return !isCancelled()
        }
        async let first = coordinator.run(.backgroundRefresh)
        try await Task.sleep(for: .milliseconds(20))
        _ = try await coordinator.run(.changeNotification)
        await coordinator.cancel()
        #expect(try await first == false)
        #expect(await calls.values == [.backgroundRefresh])
    }

    @Test func backgroundTaskCompletionHappensOnce() {
        let results = Recorder.Sync()
        let once = OnceCompletion { results.append($0) }
        once.complete(false)
        once.complete(true)
        #expect(results.values == [false])
    }
}

actor Recorder {
    var values: [ContactsPassReason] = []
    func append(_ value: ContactsPassReason) { values.append(value) }

    final class Sync: @unchecked Sendable {
        private let lock = NSLock()
        private var stored: [Bool] = []
        var values: [Bool] { lock.withLock { stored } }
        func append(_ value: Bool) { lock.withLock { stored.append(value) } }
    }
}

@Suite struct ContactsCoreMappingTests {
    @Test func labelsRoundTripAsTokensAndCustomLabelsStayVerbatim() {
        #expect(ContactsCoreMapping.token("_$!<Mobile>!$_") == "mobile")
        #expect(ContactsCoreMapping.raw("mobile") == "_$!<Mobile>!$_")
        #expect(ContactsCoreMapping.token("Boat") == "Boat" && ContactsCoreMapping.raw("Boat") == "Boat")
        #expect(ContactsCoreMapping.token(nil) == nil)
    }

    @Test func fieldsCarryPartialBirthdayButNeverAPhotoAndDeriveDisplayName() {
        let contact = PlatformContact(id: "x", organization: "ACME", birthday: ContactBirthday(month: 2, day: 29), hasImage: true)
        let fields = ContactsCoreMapping.fields(contact)
        #expect(fields["display_name"] as? String == "ACME")
        #expect(fields["photo"] == nil, "photos are added only after the OS photo was inspected")
        let birthday = fields["birthday"] as? [String: Any]
        #expect(birthday?["month"] as? Int == 2 && birthday?["day"] as? Int == 29 && birthday?["year"] == nil)
        let back = ContactsCoreMapping.platform(fields, id: "x", knownItemIds: [:])
        #expect(back.birthday == ContactBirthday(year: nil, month: 2, day: 29))
    }

    @Test func patchesTranslateCoreItemIdsToNativeIds() throws {
        let observed: [String: Any] = ["id": "c", "phones": [["id": "native-1", "value": "1"], ["id": "native-2", "value": "2"]]]
        let sources: [[String: Any]] = [["field": "phones", "id": "core-1", "source_id": "native-1"], ["field": "phones", "id": "core-2", "source_id": "native-2"]]
        let desired = try ContactsCoreMapping.applying([
            ["op": "remove", "path": "phones[core-2]"],
            ["op": "replace", "path": "phones[core-1]", "value": ["id": "core-1", "value": "9"]],
            ["op": "replace", "path": "display_name", "value": "ignored"],
        ], to: observed, fieldSources: sources)
        let phones = desired["phones"] as? [[String: Any]] ?? []
        #expect(phones.count == 1 && phones[0]["id"] as? String == "native-1" && phones[0]["value"] as? String == "9")
        #expect(throws: ContactsCoreMapping.PatchError.missingItem) {
            try ContactsCoreMapping.applying([["op": "remove", "path": "phones[gone]"]], to: observed, fieldSources: sources)
        }
        #expect(throws: ContactsCoreMapping.PatchError.unsupported) {
            try ContactsCoreMapping.applying([["op": "replace", "path": "notes", "value": "x"]], to: observed, fieldSources: sources)
        }
        let core = ContactsCoreMapping.coreBody(observed: observed, fieldSources: sources)
        #expect((core["phones"] as? [[String: Any]])?.compactMap { $0["id"] as? String } == ["core-1", "core-2"])
    }
}
