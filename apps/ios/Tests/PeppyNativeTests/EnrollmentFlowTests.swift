import CryptoKit
import Foundation
import PeppyBindings
import Testing
@testable import PeppyNative

@Suite struct EnrollmentFlowTests {
    let passphrase = "enrollment contract passphrase"
    let intent = String(repeating: "a", count: 43)

    @Test func acceptsOnlyExactSharedQrJSON() async throws {
        let fixture = try FakeServer(passphrase: passphrase)
        let transport = PairingContractTransport(fixture: fixture, intent: intent)
        let session = makeSession(transport)

        let claim = try await session.claimPairingIntent(qrData: qr())
        #expect(claim.origin == FakeServer.origin)
        #expect(transport.claimRequest?["requested_role"] as? String == "gateway")
        #expect((transport.claimRequest?["public_key"] as? [String: Any])?["ed25519_public_key"] as? String != nil)

        for bad in [
            Data("{\"https_origin\":\"https://push.example.test\",\"intent_token\":\"\(intent)\",\"extra\":true}".utf8),
            Data("{\"https_origin\":\"http://push.example.test\",\"intent_token\":\"\(intent)\"}".utf8),
            Data("{\"https_origin\":\"https://push.example.test/path\",\"intent_token\":\"\(intent)\"}".utf8),
        ] {
            await #expect(throws: ClientError.self) { try await session.claimPairingIntent(qrData: bad) }
        }
    }

    @Test func rejectsTamperedServerDigestAndSasBeforePersistingClaim() async throws {
        let fixture = try FakeServer(passphrase: passphrase)
        for tamper in [PairingContractTransport.Tamper.digest, .sas] {
            let store = InMemorySecureStore()
            let transport = PairingContractTransport(fixture: fixture, intent: intent, tamper: tamper)
            let session = makeSession(transport, store: store)
            await #expect(throws: (any Error).self) { try await session.claimPairingIntent(qrData: qr()) }
            #expect(!store.accounts.contains { $0.contains("claim-secret") })
        }
    }

    @Test func pendingApprovalThenConsumesSignedProofAndImportsCredential() async throws {
        let fixture = try FakeServer(passphrase: passphrase)
        let transport = PairingContractTransport(fixture: fixture, intent: intent)
        let session = makeSession(transport)
        let claim = try await session.claimPairingIntent(qrData: qr())

        await #expect(throws: ClientError.pairingAwaitingApproval) { try await session.completePairing(claim) }
        transport.approved = true
        let status = try await session.completePairing(claim)
        #expect(status.identity?.deviceId == claim.deviceId)
        #expect(transport.proofWasVerified)
        #expect(transport.paths == ["claim", "challenge", "challenge", "consume", "vault"])
    }

    @Test(arguments: [0, 4_294_967_296])
    func rejectsInvalidOrOverflowChallengeEpochWithoutConsume(_ epoch: Int) async throws {
        let fixture = try FakeServer(passphrase: passphrase)
        let transport = PairingContractTransport(fixture: fixture, intent: intent, challengeEpoch: epoch)
        let session = makeSession(transport)
        let claim = try await session.claimPairingIntent(qrData: qr())
        transport.approved = true
        await #expect(throws: (any Error).self) { try await session.completePairing(claim) }
        #expect(!transport.paths.contains("consume"))
    }

    @Test func rejectsInvalidRosterEpochWithoutTrapping() async throws {
        let fixture = try FakeServer(passphrase: passphrase)
        let transport = PairingContractTransport(fixture: fixture, intent: intent, rosterEpoch: -1)
        let session = makeSession(transport)
        let id = UUID().uuidString.lowercased()
        transport.vaultDeviceId = id
        _ = try await session.importCredential(fixture.credential(deviceId: id))
        await #expect(throws: ClientError.invalidResponse("device roster")) { try await session.devices() }
    }

    @Test func selfRevokeLostResponseKeepsCurrentIdentityArchivedUntilConfirmed() async throws {
        let fixture = try FakeServer(passphrase: passphrase)
        let transport = PairingContractTransport(fixture: fixture, intent: intent, selfRevokeStatus: 401)
        let store = InMemorySecureStore()
        let session = makeSession(transport, store: store)
        let id = UUID().uuidString.lowercased()
        transport.vaultDeviceId = id
        _ = try await session.importCredential(fixture.credential(deviceId: id))
        let before = store.accounts
        await #expect(throws: ClientError.unauthorized) { try await session.revokeDevice(id) }
        #expect(store.accounts == before, "an unconfirmed self-revoke must not discard the only active archive")
        #expect((try await session.open()).identity?.deviceId == id)
    }

    private func qr() -> Data { Data("{\"https_origin\":\"https://push.example.test\",\"intent_token\":\"\(intent)\"}".utf8) }
    private func makeSession(_ transport: PairingContractTransport, store: InMemorySecureStore = InMemorySecureStore()) -> NativeSession {
        NativeSession(secureStore: store, transport: transport, databaseDirectory: temporaryDirectory(), allowLoopbackHTTP: false)
    }
}

/// A deliberately strict server-side view of the pairing REST contract. It validates the public-key
/// digest, owner-approved challenge, and Ed25519 proof instead of duplicating NativeSession logic.
private final class PairingContractTransport: HTTPTransport, @unchecked Sendable {
    enum Tamper { case digest, sas }
    let fixture: FakeServer
    let intent: String
    let challengeEpoch: Int
    let rosterEpoch: Int
    let selfRevokeStatus: Int
    let tamper: Tamper?
    var approved = false
    private(set) var paths: [String] = []
    private(set) var claimRequest: [String: Any]?
    private(set) var proofWasVerified = false
    private var deviceId = ""
    private var publicKey = Data()
    private var keyDigest = ""
    var vaultDeviceId: String?
    private let challenge = String(repeating: "A", count: 43)

    init(fixture: FakeServer, intent: String, tamper: Tamper? = nil, challengeEpoch: Int = 1, rosterEpoch: Int = 1, selfRevokeStatus: Int = 200) {
        self.fixture = fixture; self.intent = intent; self.tamper = tamper
        self.challengeEpoch = challengeEpoch; self.rosterEpoch = rosterEpoch; self.selfRevokeStatus = selfRevokeStatus
    }

    func send(_ request: HTTPRequest) async throws -> HTTPResponse {
        let path = request.url.path
        func reply(_ status: Int, _ object: [String: Any]) -> HTTPResponse { fixture.reply(request, status, object) }
        if path == "/v1/pairing/intents/\(intent)/claim" {
            paths.append("claim")
            let body = try JSONSerialization.jsonObject(with: request.body!) as! [String: Any]
            claimRequest = body
            deviceId = body["device_id"] as! String
            let encoded = ((body["public_key"] as! [String: Any])["ed25519_public_key"] as! String)
            publicKey = Data(base64URLEncoded: encoded)!
            keyDigest = SHA256.hash(data: publicKey).map { String(format: "%02x", $0) }.joined()
            let responseDigest = tamper == .digest ? String(repeating: "0", count: 64) : keyDigest
            let sas = pairingSas(intent: intent, digest: responseDigest, device: deviceId)
            return reply(200, ["key_digest": responseDigest, "sas": tamper == .sas ? "000000" : sas, "claim_secret": String(repeating: "c", count: 43)])
        }
        if path == "/v1/pairing/intents/\(intent)/challenge" {
            paths.append("challenge")
            guard approved else { return reply(401, ["code": "pairing_challenge_unavailable"]) }
            return reply(200, ["challenge_token": challenge, "vault_id": fixture.vaultId, "profile_fingerprint": String(repeating: "d", count: 64), "key_epoch": challengeEpoch, "requested_role": "gateway"])
        }
        if path == "/v1/pairing/consume" {
            paths.append("consume")
            let body = try JSONSerialization.jsonObject(with: request.body!) as! [String: Any]
            let signature = Data(base64URLEncoded: body["signature"] as! String)!
            let proof = try pairingProofBytes(challengeToken: challenge, vaultId: fixture.vaultId, deviceId: deviceId, profileFingerprint: String(repeating: "d", count: 64), keyEpoch: UInt32(challengeEpoch), approvedRole: "gateway")
            proofWasVerified = try Curve25519.Signing.PublicKey(rawRepresentation: publicKey).isValidSignature(signature, for: proof)
            guard proofWasVerified else { return reply(401, ["code": "bad_proof"]) }
            return reply(200, ["vault_id": fixture.vaultId, "device_id": deviceId, "device_token": fixture.token])
        }
        if path == "/v1/vault" { paths.append("vault"); return reply(200, fixture.vault(deviceId: vaultDeviceId ?? deviceId)) }
        if path == "/v1/devices" { return reply(200, ["devices": [["device_id": deviceId, "role": "gateway", "revoked": false, "key_epoch": rosterEpoch]]]) }
        if path.hasSuffix("/revoke") { return reply(selfRevokeStatus, ["code": "unauthorized"]) }
        return reply(404, ["code": "not_found"])
    }

    private func pairingSas(intent: String, digest: String, device: String) -> String {
        var data = Data("peppy-pairing-sas-v1\0\(intent)\(digest)".utf8)
        let uuid = UUID(uuidString: device)!
        withUnsafeBytes(of: uuid.uuid) { data.append(contentsOf: $0) }
        defer { data.resetBytes(in: 0..<data.count) }
        let value = SHA256.hash(data: data).prefix(4).reduce(UInt32(0)) { ($0 << 8) | UInt32($1) } % 1_000_000
        return String(format: "%06u", value)
    }
}

private extension Data {
    init?(base64URLEncoded value: String) {
        var base64 = value.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        base64 += String(repeating: "=", count: (4 - base64.count % 4) % 4)
        self.init(base64Encoded: base64)
    }
}
