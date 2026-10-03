import Foundation
import PeppyBindings
import Testing
@testable import PeppyNative

/// Enrollment races, refresh, disconnect/re-enroll and reinstall recovery, with a real SQLCipher core.
@Suite struct RecoveryTests {
    let passphrase = "io synthetic vault passphrase"

    func session(_ transport: any HTTPTransport, _ store: InMemorySecureStore, _ directory: URL) -> NativeSession {
        NativeSession(secureStore: store, transport: transport, databaseDirectory: directory, allowLoopbackHTTP: false)
    }

    /// Publishes one message from another device so a sync has something to apply.
    func publish(_ server: FakeServer, _ body: String) async throws -> NativeCaptured {
        let producer = try openUnlockedClient(server, passphrase: passphrase)
        defer { try? producer.dispose() }
        let captured = try producer.captureIncoming(sms: NativeIncomingSms(
            conversationId: nil, senderAddress: "+15550002222", body: body, providerMessageId: UUID().uuidString, imported: false
        ))
        _ = try await ForegroundSync(client: producer, server: serverClient(server), vaultId: server.vaultId).run()
        return captured
    }

    @Test func concurrentImportsOfDifferentDevicesLeaveOneCoherentEnrollment() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let first = UUID().uuidString.lowercased()
        let second = UUID().uuidString.lowercased()
        let firstToken = String(repeating: "1a", count: 48)
        let secondToken = String(repeating: "2b", count: 48)
        server.mutate { $0.tokens = [firstToken, secondToken] }
        let transport = SlowTransport(inner: MultiDeviceTransport(
            server: server, deviceForToken: [firstToken: first, secondToken: second]
        ))
        let session = session(transport, store, temporaryDirectory())
        let firstCredential = server.credential(deviceId: first, token: firstToken)
        let secondCredential = server.credential(deviceId: second, token: secondToken)

        async let a = attempt { try await session.importCredential(firstCredential) }
        async let b = attempt { try await session.importCredential(secondCredential) }
        let results = await [a, b]
        let winners = results.compactMap { try? $0.get() }
        #expect(winners.count == 1)
        let failure = results.compactMap { result -> ClientError? in
            if case .failure(let error) = result { return error as? ClientError } else { return nil }
        }
        #expect(failure == [.enrollmentChangeInProgress] || failure == [.alreadyEnrolled])

        let active = try #require(try EnrollmentStore(secure: store).activeIdentity())
        #expect(winners.first?.identity == active)
        let status = try await session.open()
        #expect(status.identity == active && status.databaseOpen)
        let enrolled = store.accounts.filter { $0.hasPrefix("enrollment|") }
        #expect(enrolled == [active.account("enrollment")], "the losing import saved nothing")
        await session.close()
    }

    @Test func refreshUpdatesTheRecordAndRevocationLeavesItUntouched() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let device = UUID().uuidString.lowercased()
        let session = session(DeviceScopedTransport(server: server, deviceId: device), store, temporaryDirectory())
        _ = try await session.importCredential(server.credential(deviceId: device))
        _ = try await session.unlock(passphrase: passphrase)

        var vault = server.vault(deviceId: device)
        vault["role"] = "gateway"
        let gatewayVault = try JSONSerialization.data(withJSONObject: vault)
        server.mutate { $0.override = { request in
            request.url.path == "/v1/vault" ? HTTPResponse(status: 200, url: request.url, body: gatewayVault) : nil
        } }
        let refreshed = try await session.refreshEnrollment()
        #expect(refreshed.role == "gateway" && refreshed.keysUnlocked)

        let before = store.snapshot
        server.mutate { $0.override = { request in HTTPResponse(status: 401, url: request.url, body: Data("{}".utf8)) } }
        await #expect(throws: ClientError.unauthorized) { try await session.refreshEnrollment() }
        #expect(store.snapshot == before)
        let status = try await session.open()
        #expect(status.role == "gateway" && status.databaseOpen)
        await session.close()
    }

    @Test func aRefreshedNewerEpochNeedsAManualUnlockAndIsNeverActivatedFromACache() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let device = UUID().uuidString.lowercased()
        let session = session(DeviceScopedTransport(server: server, deviceId: device), store, temporaryDirectory())
        _ = try await session.importCredential(server.credential(deviceId: device))
        _ = try await session.unlock(passphrase: passphrase)

        var vault = server.vault(deviceId: device)
        vault["key_epoch"] = 2
        let rotated = try JSONSerialization.data(withJSONObject: vault)
        server.mutate { $0.override = { request in
            request.url.path == "/v1/vault" ? HTTPResponse(status: 200, url: request.url, body: rotated) : nil
        } }
        let refreshed = try await session.refreshEnrollment()
        #expect(refreshed.keyEpoch == 2 && !refreshed.keysUnlocked, "epoch 2 keys require the passphrase")
        let identity = try #require(refreshed.identity)
        #expect(store.value(identity.account("key-cache-2")) == nil)
        #expect(store.value(identity.account("key-cache-1")) != nil, "older caches stay for decryption")
        await session.close()
    }

    @Test func disconnectArchivesLocalWorkAndReimportReopensIt() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let directory = temporaryDirectory()
        let device = UUID().uuidString.lowercased()
        let session = session(DeviceScopedTransport(server: server, deviceId: device), store, directory)
        _ = try await session.importCredential(server.credential(deviceId: device))
        _ = try await session.unlock(passphrase: passphrase)
        _ = try await publish(server, "kept across disconnect")
        _ = try await session.syncOnce()
        #expect(try await session.open().conversations == 1)

        let disconnected = try await session.disconnect()
        #expect(disconnected.identity == nil && !disconnected.databaseOpen)
        let identity = EnrollmentIdentity(origin: FakeServer.origin, vaultId: server.vaultId, deviceId: device)
        #expect(try EnrollmentStore(secure: store).activeIdentity() == nil)
        for purpose in ["database-key", "database-created", "device-token", "enrollment", "key-cache-1"] {
            #expect(store.value(identity.account(purpose)) != nil, "\(purpose) is archived, not deleted")
        }
        let files = try FileManager.default.contentsOfDirectory(atPath: directory.path)
        #expect(files.contains("\(server.vaultId)-\(device).sqlcipher"))

        let reopened = try await session.importCredential(server.credential(deviceId: device))
        #expect(reopened.keysUnlocked && reopened.conversations == 1, "the archived database and key cache are reused")

        // After another disconnect a different device can enroll, with its own database.
        _ = try await session.disconnect()
        let other = UUID().uuidString.lowercased()
        let next = self.session(DeviceScopedTransport(server: server, deviceId: other), store, directory)
        let enrolled = try await next.importCredential(server.credential(deviceId: other))
        #expect(enrolled.identity?.deviceId == other && !enrolled.keysUnlocked && enrolled.conversations == 0)
        await next.close()
    }

    @Test func aMissingDatabaseAfterReinstallFailsClosed() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let device = UUID().uuidString.lowercased()
        let transport = DeviceScopedTransport(server: server, deviceId: device)
        let installed = temporaryDirectory()
        let first = session(transport, store, installed)
        _ = try await first.importCredential(server.credential(deviceId: device))
        _ = try await first.unlock(passphrase: passphrase)
        await first.close()

        // Reinstall: the Keychain survives, the app container (and database) does not.
        let reinstalled = session(transport, store, temporaryDirectory())
        let keychain = store.snapshot
        await #expect(throws: ClientError.localDatabaseMissing) { try await reinstalled.open() }
        await #expect(throws: ClientError.localDatabaseMissing) {
            try await reinstalled.importCredential(server.credential(deviceId: device))
        }
        #expect(store.snapshot == keychain, "nothing is overwritten or recreated")

        _ = try await reinstalled.disconnect()
        let newDevice = UUID().uuidString.lowercased()
        let paired = session(DeviceScopedTransport(server: server, deviceId: newDevice), store, temporaryDirectory())
        let status = try await paired.importCredential(server.credential(deviceId: newDevice))
        #expect(status.databaseOpen && status.identity?.deviceId == newDevice)
        await paired.close()
    }

    @Test func aDatabaseWithoutItsKeyIsNeverReplaced() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let device = UUID().uuidString.lowercased()
        let directory = temporaryDirectory()
        let first = session(DeviceScopedTransport(server: server, deviceId: device), store, directory)
        let status = try await first.importCredential(server.credential(deviceId: device))
        await first.close()
        let identity = try #require(status.identity)
        let file = directory.appendingPathComponent("\(identity.vaultId)-\(identity.deviceId).sqlcipher")
        let bytes = try Data(contentsOf: file)

        try store.delete(identity.account("database-key"))
        let second = session(DeviceScopedTransport(server: server, deviceId: device), store, directory)
        await #expect(throws: ClientError.missingSecret("database key")) { try await second.open() }
        #expect(store.value(identity.account("database-key")) == nil)
        #expect(try Data(contentsOf: file) == bytes)
    }
}
