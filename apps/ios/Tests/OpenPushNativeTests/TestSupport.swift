import Foundation
import OpenPushBindings
@testable import OpenPushNative

/// In-memory replacement for the Keychain (the OS boundary only).
final class InMemorySecureStore: SecureStore, @unchecked Sendable {
    private let lock = NSLock()
    private var items: [String: Data] = [:]

    var accounts: Set<String> { lock.withLock { Set(items.keys) } }
    func value(_ account: String) -> Data? { lock.withLock { items[account] } }

    func read(_ account: String) throws(ClientError) -> Data? { value(account) }
    func addIfAbsent(_ account: String, _ data: Data) throws(ClientError) -> Bool {
        lock.withLock {
            guard items[account] == nil else { return false }
            items[account] = data
            return true
        }
    }
    func upsert(_ account: String, _ data: Data) throws(ClientError) { lock.withLock { items[account] = data } }
    func delete(_ account: String) throws(ClientError) { _ = lock.withLock { items.removeValue(forKey: account) } }
    var snapshot: [String: Data] { lock.withLock { items } }
}

/// A local stand-in for the OpenPush server's JSON contract: vault, ingest, replay and snapshot.
/// It stores envelopes as received and serves them back exactly like the transport log.
final class FakeServer: HTTPTransport, @unchecked Sendable {
    static let origin = "https://push.example.test"
    let token = String(repeating: "ab", count: 48)
    let vaultId: String
    let material: NativeVaultMaterial
    let lock = NSLock()
    /// Bearer tokens the fake accepts (the real server maps each token to one device).
    var tokens: Set<String>
    var log: [(cursor: UInt64, envelope: [String: Any])] = []
    /// Replay requests below this cursor answer `409 resync_required`.
    var replayFloor: UInt64 = 0
    /// Whether all vault devices have declared compaction fencing.
    var compactionActive = true
    var override: (@Sendable (HTTPRequest) -> HTTPResponse?)?
    /// Attachment objects by id: declared size/hash, uploaded ciphertext, finalized, registered references.
    var attachments: [String: (declared: Int, sha256: String, body: Data?, finalized: Bool, references: [String])] = [:]
    /// Status for `DELETE /v1/attachments/{id}` (release); the real server proves compaction first.
    var releaseStatus = 409
    private(set) var requests: [HTTPRequest] = []

    init(vaultId: String = UUID().uuidString.lowercased(), passphrase: String) throws {
        self.vaultId = vaultId
        tokens = [token]
        material = try createSmokeVaultMaterial(vaultId: vaultId, passphrase: passphrase)
    }

    func credential(deviceId: String, origin: String = FakeServer.origin, token: String? = nil) -> Data {
        let object: [String: Any] = [
            "version": 1, "origin": origin, "vaultId": vaultId, "deviceId": deviceId, "deviceToken": token ?? self.token,
        ]
        return try! JSONSerialization.data(withJSONObject: object)
    }

    var paths: [String] { lock.withLock { requests.map { "\($0.method) \($0.url.path)" } } }
    func mutate(_ body: (FakeServer) -> Void) { lock.withLock { body(self) } }

    func send(_ request: HTTPRequest) async throws -> HTTPResponse {
        try lock.withLock { try handle(request) }
    }

    private func handle(_ request: HTTPRequest) throws -> HTTPResponse {
        requests.append(request)
        if let response = override?(request) { return response }
        guard let bearer = request.headers["Authorization"], bearer.hasPrefix("Bearer "),
              tokens.contains(String(bearer.dropFirst(7)))
        else { return reply(request, 401, ["code": "unauthorized"]) }
        let query = Dictionary(
            uniqueKeysWithValues: (URLComponents(url: request.url, resolvingAgainstBaseURL: false)?.queryItems ?? [])
                .map { ($0.name, $0.value ?? "") }
        )
        let high = log.last?.cursor ?? 0
        if request.url.path.hasPrefix("/v1/attachments") { return attachment(request) }
        switch (request.method, request.url.path) {
        case ("GET", "/v1/vault"):
            let deviceId = request.headers["X-Test-Device"] ?? ""
            return reply(request, 200, vault(deviceId: deviceId))
        case ("POST", "/v1/events"), ("POST", "/v1/commands"):
            let envelope = try JSONSerialization.jsonObject(with: request.body ?? Data()) as! [String: Any]
            let id = envelope["envelope_id"] as! String
            if let existing = log.first(where: { $0.envelope["envelope_id"] as? String == id }) {
                return reply(request, 200, ["cursor": String(existing.cursor), "duplicate": true])
            }
            log.append((high + 1, envelope))
            return reply(request, 200, ["cursor": String(high + 1), "duplicate": false])
        case ("GET", "/v1/events"):
            let after = UInt64(query["after"] ?? "0")!
            if after < replayFloor {
                return reply(request, 409, [
                    "code": "resync_required", "reason": "cursor_expired",
                    "high_water_cursor": String(high), "replay_floor_cursor": String(replayFloor),
                    "snapshot_path": "/v1/snapshot",
                ])
            }
            let (rows, next) = page(after: after, upper: high, limit: Int(query["limit"] ?? "100")!)
            return reply(request, 200, [
                "high_water_cursor": String(high), "replay_floor_cursor": String(replayFloor),
                "next_after": next as Any, "events": rows,
            ])
        case ("GET", "/v1/snapshot"):
            return reply(request, 200, [
                "snapshot_version": 1, "compaction_supported": true, "compaction_active": compactionActive, "compaction_generation": "0",
                "vault_id": vaultId, "high_water_cursor": String(high),
                "record_count": String(log.count), "replay_floor_cursor": String(replayFloor),
                "key_epoch": 1, "profile_fingerprint": "unused", "max_page_size": 200,
            ])
        case ("POST", "/v1/compaction/capability"):
            return HTTPResponse(status: 204, url: request.url, body: Data())
        case ("GET", "/v1/snapshot/records"):
            let upper = UInt64(query["high_water"]!)!
            let (rows, next) = page(after: UInt64(query["after"] ?? "0")!, upper: upper, limit: Int(query["limit"] ?? "100")!)
            return reply(request, 200, ["high_water_cursor": String(upper), "next_after": next as Any, "records": rows])
        default:
            return reply(request, 404, ["code": "not_found"])
        }
    }

    /// The attachment contract: reserve → PUT upload → finalize → GET; references require a
    /// finalized object; DELETE answers `releaseStatus`.
    private func attachment(_ request: HTTPRequest) -> HTTPResponse {
        let parts = request.url.path.split(separator: "/").map(String.init) // v1, attachments, id?, action?
        let json = (request.body.flatMap { try? JSONSerialization.jsonObject(with: $0) }) as? [String: Any] ?? [:]
        switch (request.method, parts.count, parts.last) {
        case ("POST", 3, "reserve"):
            guard json["reference_tracking"] as? Bool == true, // contact photos are always reference-tracked
                  let bytes = json["declared_ciphertext_bytes"] as? Int, let sha = json["declared_ciphertext_sha256"] as? String
            else { return reply(request, 400, ["code": "invalid_reservation"]) }
            let id = json["attachment_id"] as? String ?? UUID().uuidString.lowercased()
            if let existing = attachments[id], existing.declared != bytes || existing.sha256 != sha {
                return reply(request, 409, ["code": "attachment_reservation_conflict"])
            }
            if attachments[id] == nil { attachments[id] = (bytes, sha, nil, false, []) }
            return reply(request, 200, ["attachment_id": id, "expires_in_seconds": 3600])
        case ("PUT", 4, "upload"):
            guard var object = attachments[parts[2]], !object.finalized, let body = request.body, body.count == object.declared
            else { return reply(request, 404, ["code": "upload_reservation_not_found"]) }
            object.body = body
            attachments[parts[2]] = object
            return HTTPResponse(status: 204, url: request.url, body: Data())
        case ("POST", 4, "finalize"):
            guard var object = attachments[parts[2]], object.body != nil
            else { return reply(request, 404, ["code": "upload_reservation_not_found"]) }
            object.finalized = true
            attachments[parts[2]] = object
            return reply(request, 200, ["attachment_id": parts[2]])
        case ("POST", 4, "references"):
            guard var object = attachments[parts[2]], object.finalized,
                  let references = json["references"] as? [[String: Any]], !references.isEmpty
            else { return reply(request, 404, ["code": "attachment_not_found"]) }
            object.references += references.compactMap { $0["producer_sequence"] as? String }
            attachments[parts[2]] = object
            return HTTPResponse(status: 204, url: request.url, body: Data())
        case ("GET", 3, _):
            guard let object = attachments[parts[2]], object.finalized, let body = object.body
            else { return reply(request, 404, ["code": "attachment_not_found"]) }
            return HTTPResponse(status: 200, url: request.url, body: body)
        case ("DELETE", 3, _):
            guard json["compaction_generation"] is String, json["release_before_cursor"] is String
            else { return reply(request, 400, ["code": "invalid_release"]) }
            if releaseStatus == 204 { attachments[parts[2]] = nil }
            return HTTPResponse(status: releaseStatus, url: request.url, body: Data("{}".utf8))
        default:
            return reply(request, 404, ["code": "not_found"])
        }
    }

    func vault(deviceId: String) -> [String: Any] {
        [
            "vault_id": vaultId,
            "device_id": deviceId,
            "role": "device",
            "key_epoch": 1,
            "profile_fingerprint": "unused",
            "public_key_profile": try! JSONSerialization.jsonObject(with: Data(material.profileJson.utf8)),
            "encrypted_vault_check_header": Data(material.headerJson.utf8).base64EncodedString(),
        ]
    }

    private func page(after: UInt64, upper: UInt64, limit: Int) -> ([[String: Any]], Any) {
        let rows = log.filter { $0.cursor > after && $0.cursor <= upper }.prefix(limit)
        let next: Any = rows.last.map { $0.cursor < upper ? String($0.cursor) as Any : NSNull() } ?? NSNull()
        return (rows.map { ["cursor": String($0.cursor), "envelope": $0.envelope] }, next)
    }

    func reply(_ request: HTTPRequest, _ status: Int, _ object: [String: Any]) -> HTTPResponse {
        HTTPResponse(status: status, url: request.url, body: try! JSONSerialization.data(withJSONObject: object))
    }
}

/// The FakeServer answers `/v1/vault` for the device named in this header; the real server derives
/// it from the bearer token. The transport wrapper injects it so the importer stays unmodified.
struct DeviceScopedTransport: HTTPTransport {
    let server: FakeServer
    let deviceId: String
    func send(_ request: HTTPRequest) async throws -> HTTPResponse {
        var request = request
        request.headers["X-Test-Device"] = deviceId
        return try await server.send(request)
    }
}

/// Suspends before forwarding, so concurrent enrollment changes overlap at the network await.
struct SlowTransport: HTTPTransport {
    let inner: any HTTPTransport
    func send(_ request: HTTPRequest) async throws -> HTTPResponse {
        try await Task.sleep(for: .milliseconds(100))
        return try await inner.send(request)
    }
}

/// Routes `/v1/vault` answers per device, like the real server does per token.
struct MultiDeviceTransport: HTTPTransport {
    let server: FakeServer
    let deviceForToken: [String: String]
    func send(_ request: HTTPRequest) async throws -> HTTPResponse {
        var request = request
        let token = String((request.headers["Authorization"] ?? "").dropFirst(7))
        request.headers["X-Test-Device"] = deviceForToken[token] ?? ""
        return try await server.send(request)
    }
}

func attempt(_ body: () async throws -> SessionStatus) async -> Result<SessionStatus, any Error> {
    do { return .success(try await body()) } catch { return .failure(error) }
}

func temporaryDirectory() -> URL {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent("openpush-io-tests-\(UUID().uuidString)")
    try! FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}

/// Opens and unlocks a real SQLCipher core client for the fake server's vault.
func openUnlockedClient(_ server: FakeServer, passphrase: String, deviceId: String = UUID().uuidString.lowercased()) throws -> NativeClient {
    let client = try openNativeClient(config: NativeOpenConfig(
        databasePath: temporaryDirectory().appendingPathComponent("core.db").path,
        vaultId: server.vaultId,
        deviceId: deviceId,
        databaseKey: Data((0..<32).map { _ in UInt8.random(in: 0...255) })
    ))
    try client.unlock(profileJson: server.material.profileJson, headerJson: server.material.headerJson, passphrase: passphrase)
    _ = try client.setServerCompactionState(supported: true, active: server.compactionActive)
    return client
}

func serverClient(_ server: FakeServer, transport: (any HTTPTransport)? = nil) throws -> ServerClient {
    ServerClient(
        origin: try ServerOrigin(canonical: FakeServer.origin, allowLoopbackHTTP: false),
        token: server.token,
        transport: transport ?? server
    )
}
