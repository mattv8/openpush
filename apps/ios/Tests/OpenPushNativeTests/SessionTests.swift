import Foundation
import OpenPushBindings
import Testing
@testable import OpenPushNative

/// Import, Keychain layout, unlock and key-cache restore through a real SQLCipher core.
@Suite struct SessionTests {
    let passphrase = "io synthetic vault passphrase"

    func session(_ server: FakeServer, store: InMemorySecureStore, deviceId: String, directory: URL) -> NativeSession {
        NativeSession(
            secureStore: store,
            transport: DeviceScopedTransport(server: server, deviceId: deviceId),
            databaseDirectory: directory,
            allowLoopbackHTTP: false
        )
    }

    @Test func importVerifiesIdentityUnlocksWithSharedPassphraseAndRestoresKeysWithoutIt() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let deviceId = UUID().uuidString.lowercased()
        let directory = temporaryDirectory()
        let first = session(server, store: store, deviceId: deviceId, directory: directory)

        let imported = try await first.importCredential(server.credential(deviceId: deviceId))
        #expect(server.paths == ["GET /v1/vault"])
        #expect(server.requests.first?.headers["Authorization"] == "Bearer \(server.token)")
        #expect(imported.databaseOpen)
        #expect(!imported.keysUnlocked, "an open database does not mean the vault keys are unlocked")
        let identity = try #require(imported.identity)
        let databaseKey = try #require(store.value(identity.account("database-key")))
        #expect(databaseKey.count == 32)
        #expect(store.value(identity.account("device-token")) == Data(server.token.utf8))

        await #expect(throws: ClientError.core(.WrongPassphrase)) { try await first.unlock(passphrase: "wrong passphrase") }
        let unlocked = try await first.unlock(passphrase: passphrase)
        #expect(unlocked.keysUnlocked)
        #expect(store.value(identity.account("key-cache-1")) != nil)
        #expect(!store.accounts.contains { store.value($0).map { String(decoding: $0, as: UTF8.self).contains(passphrase) } ?? false })

        // Re-import of the same identity refreshes metadata but never replaces the database key.
        _ = try await first.importCredential(server.credential(deviceId: deviceId))
        #expect(store.value(identity.account("database-key")) == databaseKey)
        await first.close()

        // A fresh process: the Keychain key cache restores the keys without the passphrase.
        let second = session(server, store: store, deviceId: deviceId, directory: directory)
        let reopened = try await second.open()
        #expect(reopened.databaseOpen && reopened.keysUnlocked && reopened.rejectedKeyCaches == 0)
        await second.close()
    }

    @Test func nothingIsStoredWhenTheServerAnswersForAnotherDevice() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let session = session(server, store: store, deviceId: UUID().uuidString.lowercased(), directory: temporaryDirectory())
        await #expect(throws: ClientError.identityMismatch) {
            try await session.importCredential(server.credential(deviceId: UUID().uuidString.lowercased()))
        }
        #expect(store.accounts.isEmpty)
    }

    @Test func nothingIsStoredWhenTheTokenIsRejected() async throws {
        let server = try FakeServer(passphrase: passphrase)
        server.mutate { $0.override = { request in HTTPResponse(status: 401, url: request.url, body: Data("{}".utf8)) } }
        let store = InMemorySecureStore()
        let deviceId = UUID().uuidString.lowercased()
        let session = session(server, store: store, deviceId: deviceId, directory: temporaryDirectory())
        await #expect(throws: ClientError.unauthorized) { try await session.importCredential(server.credential(deviceId: deviceId)) }
        #expect(store.accounts.isEmpty)
    }

    @Test func aSecondEnrollmentIsRefusedAndTheFirstIsKept() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let deviceId = UUID().uuidString.lowercased()
        let first = session(server, store: store, deviceId: deviceId, directory: temporaryDirectory())
        _ = try await first.importCredential(server.credential(deviceId: deviceId))
        let before = store.accounts

        let otherDevice = UUID().uuidString.lowercased()
        let other = session(server, store: store, deviceId: otherDevice, directory: temporaryDirectory())
        await #expect(throws: ClientError.alreadyEnrolled) { try await other.importCredential(server.credential(deviceId: otherDevice)) }
        #expect(store.accounts == before)
        await first.close()
    }

    @Test func foregroundSyncThroughTheSessionUsesTheStoredToken() async throws {
        let server = try FakeServer(passphrase: passphrase)
        let store = InMemorySecureStore()
        let deviceId = UUID().uuidString.lowercased()
        let session = session(server, store: store, deviceId: deviceId, directory: temporaryDirectory())
        _ = try await session.importCredential(server.credential(deviceId: deviceId))
        _ = try await session.unlock(passphrase: passphrase)
        let report = try await session.syncOnce()
        #expect(report.complete && report.uploaded == 0 && report.receiveCursor == "0")
        #expect(server.paths.last == "GET /v1/events")
        await session.close()
        await #expect(throws: ClientError.notEnrolled) { try await session.syncOnce() }
    }
}
