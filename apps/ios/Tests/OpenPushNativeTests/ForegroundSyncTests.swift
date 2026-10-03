import Foundation
import OpenPushBindings
import Testing
@testable import OpenPushNative

/// Bounded foreground sync against the server contract, with real core clients on both ends.
@Suite struct ForegroundSyncTests {
    let passphrase = "io synthetic vault passphrase"

    func capture(_ client: NativeClient, _ body: String) throws -> NativeCaptured {
        try client.captureIncoming(sms: NativeIncomingSms(
            conversationId: nil, senderAddress: "+15550001111", body: body, providerMessageId: UUID().uuidString, imported: false
        ))
    }

    func sync(_ client: NativeClient, _ server: FakeServer, budget: SyncBudget = SyncBudget()) async throws -> SyncReport {
        try await ForegroundSync(client: client, server: serverClient(server), vaultId: server.vaultId, budget: budget).run()
    }

    @Test func outboxUploadsAreAckedAndAnotherDeviceReceivesThem() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let producer = try openUnlockedClient(server, passphrase: passphrase)
        let captured = try capture(producer, "hello from the producer")

        let sent = try await sync(producer, server)
        #expect(sent.uploaded == 1)
        #expect(sent.duplicates == 1, "the producer's own echo is a duplicate")
        #expect(try producer.pendingOutboxJson().isEmpty)
        #expect(sent.receiveCursor == "1")

        let receiver = try openUnlockedClient(server, passphrase: passphrase)
        let received = try await sync(receiver, server)
        #expect(received.journaled == 1 && received.applied == 1 && received.complete)
        let messages = try receiver.messages(conversationId: captured.conversationId)
        #expect(messages.map(\.body) == ["hello from the producer"])

        let again = try await sync(receiver, server)
        #expect(again.journaled == 0 && again.applied == 0 && again.receiveCursor == "1")
        try producer.dispose()
        try receiver.dispose()
    }

    @Test func expiredCursorsResyncThroughAStagedSnapshot() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let producer = try openUnlockedClient(server, passphrase: passphrase)
        for index in 0..<5 { _ = try capture(producer, "history \(index)") }
        _ = try await sync(producer, server)
        server.mutate { $0.replayFloor = 3 }

        let fresh = try openUnlockedClient(server, passphrase: passphrase)
        var budget = SyncBudget()
        budget.pageLimit = 2
        let report = try await sync(fresh, server, budget: budget)
        #expect(server.paths.contains("GET /v1/snapshot"))
        #expect(report.snapshotPublished && report.snapshotRecords == 5 && report.snapshotRemaining == 0)
        #expect(report.applied == 5 && report.receiveCursor == "5" && report.complete)
        #expect(try fresh.listConversations().count == 1)
        try producer.dispose()
        try fresh.dispose()
    }

    @Test func aSnapshotInterruptedByTheBudgetResumesOnTheNextPass() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let producer = try openUnlockedClient(server, passphrase: passphrase)
        for index in 0..<4 { _ = try capture(producer, "history \(index)") }
        _ = try await sync(producer, server)
        server.mutate { $0.replayFloor = 2 }

        let fresh = try openUnlockedClient(server, passphrase: passphrase)
        var budget = SyncBudget()
        budget.pageLimit = 1
        budget.maxRequests = 6 // declaration, probe, replay 409, snapshot cut, two pages
        let partial = try await sync(fresh, server, budget: budget)
        #expect(!partial.complete && !partial.snapshotPublished && partial.requests == 6)
        #expect(try fresh.snapshotProgress()?.receivedRecords == 2)

        budget.maxRequests = 24
        let resumed = try await sync(fresh, server, budget: budget)
        #expect(resumed.snapshotPublished && resumed.complete && resumed.receiveCursor == "4")
        try producer.dispose()
        try fresh.dispose()
    }

    @Test func uploadsAreBoundedPerPass() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let client = try openUnlockedClient(server, passphrase: passphrase)
        _ = try capture(client, "one")
        _ = try capture(client, "two")
        var budget = SyncBudget()
        budget.maxUploads = 1
        let first = try await sync(client, server, budget: budget)
        #expect(first.uploaded == 1 && !first.complete)
        #expect(try client.pendingOutboxJson().count == 1)
        let second = try await sync(client, server, budget: budget)
        #expect(second.uploaded == 1 && second.complete)
        try client.dispose()
    }

    @Test func refusedRedirectsAndForeignOriginsLeaveTheOutboxUnacknowledged() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let client = try openUnlockedClient(server, passphrase: passphrase)
        _ = try capture(client, "kept")

        server.mutate { $0.override = { _ in HTTPResponse(status: 200, url: URL(string: "https://evil.example.test/v1/events"), body: Data("{\"cursor\":\"1\"}".utf8)) } }
        await #expect(throws: ClientError.originMismatch) { try await sync(client, server) }
        #expect(try client.pendingOutboxJson().count == 1)

        let refusing = RefusingTransport()
        await #expect(throws: ClientError.redirectRefused) {
            try await ForegroundSync(client: client, server: serverClient(server, transport: refusing), vaultId: server.vaultId).run()
        }
        #expect(try client.pendingOutboxJson().count == 1)
        try client.dispose()
    }

    @Test func cancellationStopsBeforeAnyRequestOrAck() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let client = try openUnlockedClient(server, passphrase: passphrase)
        _ = try capture(client, "kept")
        let task = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await ForegroundSync(client: client, server: serverClient(server), vaultId: server.vaultId).run()
        }
        await #expect(throws: CancellationError.self) { try await task.value }
        #expect(server.paths.isEmpty)
        #expect(try client.pendingOutboxJson().count == 1)
        try client.dispose()
    }

    @Test func aRefusedUploadStaysQueuedAndReceiveStillRuns() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let producer = try openUnlockedClient(server, passphrase: passphrase)
        let captured = try capture(producer, "from the producer")
        _ = try await sync(producer, server)

        let client = try openUnlockedClient(server, passphrase: passphrase)
        _ = try capture(client, "refused by the server")
        server.mutate { $0.override = { request in
            request.method == "POST"
                ? HTTPResponse(status: 422, url: request.url, body: Data("{\"code\":\"invalid_envelope\"}".utf8))
                : nil
        } }
        let report = try await sync(client, server)
        #expect(report.uploadRefused == .server(status: 422, code: "invalid_envelope"))
        #expect(report.uploaded == 0 && !report.complete)
        #expect(report.journaled == 1 && report.applied == 1)
        #expect(try client.messages(conversationId: captured.conversationId).map(\.body) == ["from the producer"])
        #expect(try client.pendingOutboxJson().count == 1, "refused work is kept, not acknowledged")

        server.mutate { $0.override = { request in
            request.method == "POST" ? HTTPResponse(status: 401, url: request.url, body: Data("{}".utf8)) : nil
        } }
        await #expect(throws: ClientError.unauthorized) { try await sync(client, server) }
        #expect(try client.pendingOutboxJson().count == 1)
        try producer.dispose()
        try client.dispose()
    }

    @Test func repeatedResyncAnswersCannotExceedTheRequestBudget() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let producer = try openUnlockedClient(server, passphrase: passphrase)
        for index in 0..<3 { _ = try capture(producer, "history \(index)") }
        _ = try await sync(producer, server)
        server.mutate { $0.replayFloor = 1_000 } // every replay request answers resync_required

        let fresh = try openUnlockedClient(server, passphrase: passphrase)
        var budget = SyncBudget()
        budget.maxRequests = 9
        let before = server.paths.count
        let report = try await sync(fresh, server, budget: budget)
        #expect(!report.complete)
        #expect(report.requests == 9 && server.paths.count - before == 9)
        #expect(report.snapshotPublished && report.receiveCursor == "3")
        try producer.dispose()
        try fresh.dispose()
    }

    @Test func aPublishedSnapshotDrainsBeforeLiveReplay() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let producer = try openUnlockedClient(server, passphrase: passphrase)
        for index in 0..<5 { _ = try capture(producer, "history \(index)") }
        _ = try await sync(producer, server)
        server.mutate { $0.replayFloor = 3 }

        let fresh = try openUnlockedClient(server, passphrase: passphrase)
        var budget = SyncBudget()
        budget.applyLimit = 2
        budget.maxApplyRounds = 2 // drain check, pre-snapshot check; none left to drain
        let first = try await sync(fresh, server, budget: budget)
        #expect(first.snapshotPublished && first.snapshotRemaining == 5 && !first.complete)

        budget.maxApplyRounds = 1

        let before = server.paths.count
        let second = try await sync(fresh, server, budget: budget)
        #expect(second.snapshotRemaining > 0 && !second.complete)
        #expect(
            Array(server.paths.dropFirst(before)) == ["POST /v1/compaction/capability", "GET /v1/snapshot"],
            "the capability probe runs, but live replay stays blocked while published records are undrained"
        )

        let last = try await sync(fresh, server)
        #expect(last.snapshotRemaining == 0 && last.complete && last.receiveCursor == "5")
        #expect(server.paths.count > before)
        #expect(try fresh.listConversations().count == 1)
        try producer.dispose()
        try fresh.dispose()
    }

    @Test func capabilityProbeRequestsAreMeteredAndLegacy404IsNotAuthenticationFailure() async throws {
        let meteredServer = try FakeServer(passphrase: passphrase)
        let meteredClient = try openUnlockedClient(meteredServer, passphrase: passphrase)
        var budget = SyncBudget()
        budget.maxRequests = 2

        let metered = try await sync(meteredClient, meteredServer, budget: budget)
        #expect(!metered.complete && metered.requests == 2)
        #expect(meteredServer.paths == ["POST /v1/compaction/capability", "GET /v1/snapshot"])

        let legacyServer = try FakeServer(passphrase: passphrase)
        let legacyClient = try openUnlockedClient(legacyServer, passphrase: passphrase)
        legacyServer.mutate { $0.override = { request in
            request.url.path == "/v1/compaction/capability"
                ? legacyServer.reply(request, 404, ["code": "not_found"])
                : nil
        } }
        let legacy = try await sync(legacyClient, legacyServer)
        #expect(legacy.complete)
        #expect(legacyServer.paths == ["POST /v1/compaction/capability", "GET /v1/events"])

        let unauthorizedServer = try FakeServer(passphrase: passphrase)
        let unauthorizedClient = try openUnlockedClient(unauthorizedServer, passphrase: passphrase)
        unauthorizedServer.mutate { $0.override = { request in
            request.url.path == "/v1/compaction/capability"
                ? unauthorizedServer.reply(request, 401, ["code": "unauthorized"])
                : nil
        } }
        await #expect(throws: ClientError.unauthorized) { try await sync(unauthorizedClient, unauthorizedServer) }
        #expect(unauthorizedServer.paths == ["POST /v1/compaction/capability"])

        try meteredClient.dispose()
        try legacyClient.dispose()
        try unauthorizedClient.dispose()
    }

    @Test func snapshotCompactionRosterStateIsRecordedWithoutAddingRequests() async throws {
        let server = try FakeServer(passphrase: passphrase)
        server.mutate { $0.compactionActive = false }
        let client = try openUnlockedClient(server, passphrase: passphrase)
        var budget = SyncBudget()
        budget.maxRequests = 2

        let report = try await sync(client, server, budget: budget)
        let readiness = try NativeContactsCoordinator.object(client.contactSyncReadinessJson())

        #expect(!report.complete && report.requests == 2)
        #expect(readiness["server_supported"] as? Bool == true)
        #expect(readiness["server_active"] as? Bool == false)
        #expect(readiness["contacts_ready"] as? Bool == true)
        try client.dispose()
    }

    @Test func requestAndClientDiagnosticsRedactTheToken() throws {
        let server = try FakeServer(passphrase: passphrase)
        let client = try serverClient(server)
        let request = HTTPRequest(
            method: "GET", url: URL(string: "https://push.example.test/v1/vault")!,
            headers: ["Authorization": "Bearer \(server.token)", "Accept": "application/json"], body: nil, maxResponseBytes: 1
        )
        for value in [request as Any, client as Any] {
            var dumped = ""
            dump(value, to: &dumped)
            for text in [String(describing: value), String(reflecting: value), dumped] {
                #expect(!text.contains(server.token))
            }
        }
        #expect(String(describing: request).contains("application/json"))
    }
}

struct RefusingTransport: HTTPTransport {
    func send(_ request: HTTPRequest) async throws -> HTTPResponse { throw ClientError.redirectRefused }
}
