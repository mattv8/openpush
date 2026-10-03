import CoreGraphics
import Foundation
import ImageIO
import OpenPushBindings
import Testing
import UniformTypeIdentifiers
@testable import OpenPushNative

/// A small synthetic image in `type` (no real contact data).
func syntheticImage(_ type: UTType = .png, side: Int = 96, red: CGFloat = 0.8) -> Data? {
    guard let context = CGContext(
        data: nil, width: side, height: side, bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else { return nil }
    context.setFillColor(CGColor(red: red, green: 0.3, blue: 0.2, alpha: 1))
    context.fill(CGRect(x: 0, y: 0, width: side, height: side))
    guard let image = context.makeImage() else { return nil }
    let out = NSMutableData()
    guard let destination = CGImageDestinationCreateWithData(out, type.identifier as CFString, 1, nil) else { return nil }
    CGImageDestinationAddImage(destination, image, nil)
    return CGImageDestinationFinalize(destination) ? out as Data : nil
}

@Suite struct ContactPhotoSourceTests {
    @Test func coreFormatsPassThroughAndOthersConvertToBoundedJPEG() throws {
        let png = try #require(syntheticImage(.png))
        #expect(ContactPhotoSource.format(png) == .png)
        #expect(try ContactPhotoSource.normalizerInput(png) == png)
        let jpeg = try #require(syntheticImage(.jpeg))
        #expect(ContactPhotoSource.format(jpeg) == .jpeg)
        // TIFF stands in for HEIC on hosts without a HEIC encoder; both go through the thumbnail path.
        var converted = 0
        for type in [UTType.heic, .tiff] {
            guard let source = syntheticImage(type, side: 1_200), source.count <= ContactsLimits.maxPhotoBytes else { continue }
            converted += 1
            let converted = try ContactPhotoSource.normalizerInput(source)
            #expect(ContactPhotoSource.format(converted) == .jpeg)
            let image = try #require(CGImageSourceCreateWithData(converted as CFData, nil))
            let props = try #require(CGImageSourceCopyPropertiesAtIndex(image, 0, nil) as? [CFString: Any])
            #expect((props[kCGImagePropertyPixelWidth] as? Int ?? 0) <= ContactPhotoSource.conversionPixels)
        }
        #expect(converted >= 1, "at least one non-core format exercised the conversion path")
        #expect(throws: ContactPhotoSource.Failure.tooLarge) { try ContactPhotoSource.normalizerInput(Data(count: ContactsLimits.maxPhotoBytes + 1)) }
        #expect(throws: ContactPhotoSource.Failure.unsupported) { try ContactPhotoSource.normalizerInput(Data("not an image".utf8)) }
        #expect(ContactPhotoSource.sourceHash(png).hasPrefix("sha256:"))
    }
}

/// Photo capture through the real core (prepare → tracked attachment → capture), no network.
@Suite(.serialized) struct ContactsPhotoCaptureTests {
    let passphrase = "io synthetic vault passphrase"

    func owner(_ contacts: [PlatformContact], photos: [String: Data], budget: ContactsPhotoBudget? = nil) async throws
        -> (NativeContactsCoordinator, WritableFakeProvider, NativeClient)
    {
        let server = try FakeServer(passphrase: passphrase)
        let deviceId = UUID().uuidString.lowercased()
        let client = try openUnlockedClient(server, passphrase: passphrase, deviceId: deviceId)
        let provider = WritableFakeProvider()
        await provider.configure { $0.book = contacts; $0.photos = photos; $0.historyEnabled = true }
        var coordinator = NativeContactsCoordinator(
            client: client, deviceId: deviceId, provider: provider, preferences: InMemoryContactsPreferences(ContactsPreferences(enabled: true))
        )
        coordinator.scratch = temporaryDirectory()
        coordinator.photoBudget = budget
        return (coordinator, provider, client)
    }

    func view(_ client: NativeClient, _ bookId: String) throws -> (json: String, contacts: [[String: Any]]) {
        let json = try client.contactBookView(inputJson: NativeContactsCoordinator.json(["book_id": bookId, "limit": 200]))
        return (json, try NativeContactsCoordinator.object(json)["contacts"] as? [[String: Any]] ?? [])
    }

    @Test func photosAreCapturedOnceRemovedOnlyWhenTheOSConfirmsAndHashesStayLocal() async throws {
        let png = try #require(syntheticImage())
        let (coordinator, provider, client) = try await owner(sampleContacts(2), photos: ["os-c0": png])
        let first = try await coordinator.run(reason: .foreground, isCancelled: { false })
        #expect(first.outcome == .completed && first.photosCaptured == 1 && first.photoFailures == 0)
        let bookId = try #require(first.bookId)
        let shown = try view(client, bookId)
        let photo = try #require(shown.contacts.compactMap { $0["photo"] as? [String: Any] }.first)
        let attachment = try #require(photo["attachment_id"] as? String)
        #expect(Set(photo.keys) == ["attachment_id", "available"])
        #expect(!shown.json.contains("sha256:") && !shown.json.contains("history_token") && !shown.json.contains("os-c0"))
        let transfer = try NativeContactsCoordinator.object(client.contactPhotoTransferStateJson())
        #expect((transfer["uploads"] as? [[String: Any]])?.contains { $0["attachment_id"] as? String == attachment } == true)

        // Unchanged OS photo: fingerprint matches, nothing is prepared again.
        await provider.rename("os-c0", given: "Renamed")
        let second = try await coordinator.run(reason: .backgroundRefresh, isCancelled: { false })
        #expect(second.outcome == .incremental && second.photosCaptured == 0 && second.photosRemoved == 0)
        #expect(try view(client, bookId).contacts.contains { ($0["photo"] as? [String: Any])?["attachment_id"] as? String == attachment })

        // The OS confirms the photo is gone: an explicit removal.
        await provider.setPhoto("os-c0", nil)
        let third = try await coordinator.run(reason: .backgroundRefresh, isCancelled: { false })
        #expect(third.photosRemoved == 1)
        #expect(try view(client, bookId).contacts.allSatisfy { $0["photo"] == nil })
    }

    @Test func pendingPhotoWorkResumesEvenWhenHistoryIsUnchanged() async throws {
        let photos = Dictionary(uniqueKeysWithValues: (0..<3).map { ("os-c\($0)", syntheticImage(red: CGFloat($0) / 4)!) })
        let (coordinator, provider, _) = try await owner(sampleContacts(3), photos: photos, budget: ContactsPhotoBudget(reads: 1, sweep: 100))
        let first = try await coordinator.run(reason: .foreground, isCancelled: { false })
        #expect(first.photosCaptured == 1 && first.photoWorkRemaining)
        let second = try await coordinator.run(reason: .backgroundRefresh, isCancelled: { false })
        #expect(second.outcome == .unchanged && second.photosCaptured == 1 && second.photoWorkRemaining)
        let third = try await coordinator.run(reason: .backgroundRefresh, isCancelled: { false })
        #expect(third.photosCaptured == 1)
        let fourth = try await coordinator.run(reason: .backgroundRefresh, isCancelled: { false })
        #expect(fourth.photosCaptured == 0 && !fourth.photoWorkRemaining)
        #expect(await provider.photoReads == 3, "each photo was read exactly once")
    }

    @Test func aLatchedRepairBlocksEffectsUntilAFencedSnapshotPromotes() async throws {
        let (coordinator, provider, client) = try await owner(sampleContacts(2), photos: [:])
        try client.requestContactRepair()
        let blocked = try await coordinator.run(reason: .foreground, isCancelled: { false })
        #expect(blocked.outcome == .repairPending && blocked.captured == 0)
        #expect(try coordinator.ownedBook() == nil, "nothing is republished while repair is pending")
        _ = provider
        // Promote an empty fenced (compaction) snapshot directly; ContactsRepairSyncTests covers HTTP.
        let progress = try client.beginSnapshotWithCompaction(highWater: "0", recordCount: 0, purpose: .resync, serverCompactionGeneration: "1")
        _ = try client.finishSnapshot(generation: progress.generation)
        _ = try client.applyPending(limit: 100)
        #expect(try !client.contactRepairRequired())
        let resumed = try await coordinator.run(reason: .foreground, isCancelled: { false })
        #expect(resumed.outcome == .completed && resumed.captured == 2)
    }
}

/// Real core + fake HTTP server: reserve/upload/finalize/references order, peer photo edits,
/// downloads, 404 handling and owner settings across reopen.
@Suite(.serialized) struct ContactsMediaHTTPTests {
    let passphrase = "io synthetic vault passphrase"

    func session(_ server: FakeServer, deviceId: String = UUID().uuidString.lowercased(), store: InMemorySecureStore = InMemorySecureStore(),
                 directory: URL = temporaryDirectory()) async throws -> NativeSession {
        let session = NativeSession(
            secureStore: store, transport: DeviceScopedTransport(server: server, deviceId: deviceId),
            databaseDirectory: directory, allowLoopbackHTTP: false
        )
        _ = try await session.importCredential(server.credential(deviceId: deviceId))
        _ = try await session.unlock(passphrase: passphrase)
        return session
    }

    func sync(_ client: NativeClient, _ server: FakeServer) async throws {
        _ = try await ForegroundSync(client: client, server: serverClient(server), vaultId: server.vaultId).run()
    }

    @Test func ownerPhotoUploadsAndRegistersBeforeItsEventPublishesAndAPeerDownloadsIt() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let owner = try await session(server)
        let provider = WritableFakeProvider()
        let png = try #require(syntheticImage())
        await provider.configure { $0.book = sampleContacts(2); $0.photos = ["os-c0": png] }
        let preferences = InMemoryContactsPreferences(ContactsPreferences(enabled: true))
        let report = try await owner.contactsPass(reason: .backgroundProcessing, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(report.outcome == .completed && report.photosCaptured == 1 && report.syncError == nil)
        #expect(report.media.uploaded == 1 && report.media.registered >= 1 && report.media.failed == 0)

        let paths = server.paths
        func index(_ prefix: String) -> Int? { paths.firstIndex { $0.hasPrefix(prefix) } }
        let reserve = try #require(index("POST /v1/attachments/reserve"))
        let upload = try #require(paths.firstIndex { $0.hasPrefix("PUT /v1/attachments/") && $0.hasSuffix("/upload") })
        let finalize = try #require(paths.firstIndex { $0.hasSuffix("/finalize") })
        let references = try #require(paths.firstIndex { $0.hasSuffix("/references") })
        let lastEvent = try #require(paths.lastIndex(of: "POST /v1/events"))
        #expect(reserve < upload && upload < finalize && finalize < references && references < lastEvent)
        let stored = try #require(server.attachments.values.first)
        #expect(stored.finalized && !stored.references.isEmpty)

        // A second pass re-uploads nothing.
        let again = try await owner.contactsPass(reason: .backgroundProcessing, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(again.media.uploaded == 0 && again.photosCaptured == 0)
        #expect(server.paths.filter { $0 == "POST /v1/attachments/reserve" }.count == 1)

        // Another device receives the contact with a downloadable, verified photo.
        let peer = try openUnlockedClient(server, passphrase: passphrase)
        try await sync(peer, server)
        let bookId = try #require(report.bookId)
        let before = try NativeContactsCoordinator.object(peer.contactBookView(inputJson: NativeContactsCoordinator.json(["book_id": bookId])))
        let peerPhoto = try #require((before["contacts"] as? [[String: Any]])?.compactMap { $0["photo"] as? [String: Any] }.first)
        #expect(peerPhoto["available"] as? Bool == false)
        var meter = RequestMeter(limit: 10)
        var media = ContactMediaReport()
        let gone = try await ContactMediaTransfer(client: peer, server: try serverClient(server), scratch: temporaryDirectory())
            .download(only: nil, meter: &meter, report: &media)
        #expect(gone.isEmpty && media.downloaded == 1)
        let after = try NativeContactsCoordinator.object(peer.contactBookView(inputJson: NativeContactsCoordinator.json(["book_id": bookId])))
        #expect((after["contacts"] as? [[String: Any]])?.contains { ($0["photo"] as? [String: Any])?["available"] as? Bool == true } == true)
        await owner.close()
    }

    @Test func aPeerPhotoEditWaitsForTheVerifiedDownloadThenWritesTheOS() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let ownerDevice = UUID().uuidString.lowercased()
        let owner = try await session(server, deviceId: ownerDevice)
        let provider = WritableFakeProvider()
        await provider.configure { $0.book = sampleContacts(2) }
        let preferences = InMemoryContactsPreferences(ContactsPreferences(enabled: true))
        let first = try await owner.contactsPass(reason: .foreground, provider: provider, preferences: preferences, isCancelled: { false })
        let bookId = try #require(first.bookId)

        // The requester prepares a photo, uploads it and registers its reference before the request publishes.
        let requester = try openUnlockedClient(server, passphrase: passphrase)
        try await sync(requester, server)
        let view = try NativeContactsCoordinator.object(requester.contactBookView(inputJson: NativeContactsCoordinator.json(["book_id": bookId])))
        let target = try #require((view["contacts"] as? [[String: Any]])?.first { ($0["name"] as? [String: Any])?["given"] as? String == "Given0" })
        let file = temporaryDirectory().appendingPathComponent("photo.png")
        try #require(syntheticImage(side: 300)).write(to: file)
        let prepared = try requester.preparePhotoForTest(file)
        let requestId = UUID().uuidString.lowercased()
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json([
            "schema_version": 1, "request_id": requestId, "target_owner": ownerDevice, "book_id": bookId, "kind": "update",
            "contact_id": target["id"]!, "base_revision": target["revision"]!,
            "photo_op": ["op": "set", "attachment_id": prepared],
        ]))
        var meter = RequestMeter(limit: 20)
        var media = ContactMediaReport()
        try await ContactMediaTransfer(client: requester, server: try serverClient(server), scratch: temporaryDirectory())
            .uploadAndRegister(meter: &meter, report: &media)
        #expect(media.uploaded == 1 && media.registered >= 1)
        try await sync(requester, server)

        let report = try await owner.contactsPass(reason: .foreground, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(report.applied == 1 && report.waitingMedia == 0 && report.failed == 0)
        #expect(report.media.downloaded == 1)
        let written = try #require(await provider.photos["os-c0"])
        #expect(ContactPhotoSource.format(written) == .jpeg, "the OS receives the core-normalized JPEG")
        #expect(await provider.saves.count == 1)
        await owner.close()
    }

    @Test func aMissingRemotePhotoIsNotRetriedHot() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let ownerDevice = UUID().uuidString.lowercased()
        let owner = try await session(server, deviceId: ownerDevice)
        let provider = WritableFakeProvider()
        await provider.configure { $0.book = sampleContacts(1) }
        let preferences = InMemoryContactsPreferences(ContactsPreferences(enabled: true))
        let bookId = try #require(try await owner.contactsPass(reason: .foreground, provider: provider, preferences: preferences, isCancelled: { false }).bookId)
        let requester = try openUnlockedClient(server, passphrase: passphrase)
        try await sync(requester, server)
        let view = try NativeContactsCoordinator.object(requester.contactBookView(inputJson: NativeContactsCoordinator.json(["book_id": bookId])))
        let target = try #require((view["contacts"] as? [[String: Any]])?.first)
        let file = temporaryDirectory().appendingPathComponent("photo.png")
        try #require(syntheticImage()).write(to: file)
        let prepared = try requester.preparePhotoForTest(file)
        _ = try requester.requestContactEdit(inputJson: NativeContactsCoordinator.json([
            "schema_version": 1, "request_id": UUID().uuidString.lowercased(), "target_owner": ownerDevice, "book_id": bookId,
            "kind": "update", "contact_id": target["id"]!, "base_revision": target["revision"]!,
            "photo_op": ["op": "set", "attachment_id": prepared],
        ]))
        var meter = RequestMeter(limit: 20)
        var media = ContactMediaReport()
        try await ContactMediaTransfer(client: requester, server: try serverClient(server), scratch: temporaryDirectory())
            .uploadAndRegister(meter: &meter, report: &media)
        try await sync(requester, server)
        server.mutate { $0.attachments.removeAll() } // released remotely before the owner fetched it

        let first = try await owner.contactsPass(reason: .foreground, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(first.waitingMedia == 1 && first.media.unavailable == 1 && first.applied == 0)
        let gets = server.paths.filter { $0.hasPrefix("GET /v1/attachments/") }.count
        let second = try await owner.contactsPass(reason: .foreground, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(second.waitingMedia == 1)
        #expect(server.paths.filter { $0.hasPrefix("GET /v1/attachments/") }.count == gets, "a 404 is not fetched again within a day")
        #expect(await provider.saves.isEmpty)
        await owner.close()
    }

    @Test func aReplacedPhotoIsReclaimedOnlyOnceTheServerReplayFloorCoversItsReferences() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let owner = try await session(server)
        let provider = WritableFakeProvider()
        await provider.configure { $0.book = sampleContacts(1); $0.photos = ["os-c0": syntheticImage()!] }
        let preferences = InMemoryContactsPreferences(ContactsPreferences(enabled: true))
        let first = try await owner.contactsPass(reason: .backgroundProcessing, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(first.media.uploaded == 1)
        await provider.configure { $0.photos = [:] }
        let removed = try await owner.contactsPass(reason: .backgroundProcessing, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(removed.photosRemoved == 1 && removed.media.reclaimed == 0)
        #expect(!server.paths.contains { $0.hasPrefix("DELETE ") }, "no release before the replay floor covers the references")

        server.mutate { server in
            server.replayFloor = server.log.last?.cursor ?? 0
            server.releaseStatus = 204
        }
        let released = try await owner.contactsPass(reason: .backgroundProcessing, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(released.media.reclaimed == 1)
        #expect(server.paths.filter { $0.hasPrefix("DELETE /v1/attachments/") }.count == 1)
        #expect(server.attachments.isEmpty)
        let quiet = try await owner.contactsPass(reason: .backgroundProcessing, provider: provider, preferences: preferences, isCancelled: { false })
        #expect(quiet.media.reclaimed == 0 && server.paths.filter { $0.hasPrefix("DELETE ") }.count == 1)
        await owner.close()
    }

    @Test func ownerSettingsLiveInTheCoreAndSurviveReopen() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let directory = temporaryDirectory()
        let deviceId = UUID().uuidString.lowercased()
        let owner = try await session(server, deviceId: deviceId, store: store, directory: directory)
        let provider = WritableFakeProvider()
        await provider.configure { $0.book = sampleContacts(1) }
        let preferences = InMemoryContactsPreferences(ContactsPreferences(enabled: true))
        _ = try await owner.contactsPass(reason: .foreground, provider: provider, preferences: preferences, isCancelled: { false })
        let initial = try await owner.contactsOverview()
        #expect(initial.remoteEdits == "auto" && initial.defaultAccountId == "icloud")
        #expect(Set(initial.accounts.map(\.id)) == ["icloud", "gmail"])
        _ = try await owner.setContactsDefaultAccount("gmail")
        await owner.close()

        let reopened = NativeSession(
            secureStore: store, transport: DeviceScopedTransport(server: server, deviceId: deviceId),
            databaseDirectory: directory, allowLoopbackHTTP: false
        )
        _ = try await reopened.open()
        // A later capture (which sends the OS default) does not override the owner setting.
        _ = try await reopened.contactsPass(reason: .foreground, provider: provider, preferences: preferences, isCancelled: { false })
        let after = try await reopened.contactsOverview()
        #expect(after.remoteEdits == "auto" && after.defaultAccountId == "gmail")
        await reopened.close()
    }
}

/// A fenced snapshot through the real HTTP pass clears a latched repair.
@Suite struct ContactsRepairSyncTests {
    let passphrase = "io synthetic vault passphrase"

    @Test func foregroundSyncFetchesOneFencedSnapshotForALatchedRepair() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let client = try openUnlockedClient(server, passphrase: passphrase)
        try client.requestContactRepair()
        let report = try await ForegroundSync(client: client, server: serverClient(server), vaultId: server.vaultId).run()
        #expect(report.repairSnapshotStarted && !report.contactRepairRequired && report.projectionFailure == nil)
        #expect(server.paths.contains("GET /v1/snapshot"))
        let again = try await ForegroundSync(client: client, server: serverClient(server), vaultId: server.vaultId).run()
        #expect(!again.repairSnapshotStarted)
    }
}

extension NativeClient {
    func preparePhotoForTest(_ file: URL) throws -> String {
        try prepareContactPhoto(path: file.path).attachmentId
    }
}
