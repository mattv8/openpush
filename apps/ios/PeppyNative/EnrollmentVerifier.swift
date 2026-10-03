import Foundation

/// Fetches `GET /v1/vault` with a device token and checks that the server answers for exactly the
/// expected vault and device. It writes nothing; the session persists the result.
struct EnrollmentVerifier: Sendable {
    let transport: any HTTPTransport

    func verify(origin: ServerOrigin, token: String, identity: EnrollmentIdentity) async throws -> EnrollmentRecord {
        let server = ServerClient(origin: origin, token: token, transport: transport)
        var requests = RequestMeter(limit: 1)
        let vault = try await server.get("/v1/vault", limit: ServerClient.maxVaultBytes, meter: &requests)
        guard let vaultId = UUID(uuidString: try vault.string("vault_id")),
              let deviceId = UUID(uuidString: try vault.string("device_id")),
              vaultId.uuidString.lowercased() == identity.vaultId,
              deviceId.uuidString.lowercased() == identity.deviceId
        else { throw ClientError.identityMismatch }
        guard let keyEpoch = UInt32(exactly: try vault.integer("key_epoch")), keyEpoch >= 1 else {
            throw ClientError.invalidResponse("key_epoch")
        }
        let profile = try vault.object("public_key_profile").jsonData()
        guard let header = Data(base64Encoded: try vault.string("encrypted_vault_check_header")),
              !header.isEmpty, let headerJson = String(data: header, encoding: .utf8)
        else { throw ClientError.invalidResponse("encrypted_vault_check_header") }
        return EnrollmentRecord(
            identity: identity,
            role: try vault.string("role"),
            keyEpoch: keyEpoch,
            profileJson: String(decoding: profile, as: UTF8.self),
            headerJson: headerJson
        )
    }
}
