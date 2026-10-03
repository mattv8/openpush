import Foundation

/// The server receives only these two route fields. `manageCredential` stays on this phone so it
/// can revoke its relay registration without making that provider credential a server secret.
public struct RelayWakeRoute: Codable, Equatable, Sendable {
    public let routeId: String
    public let manageCredential: String
    public let wakeCredential: String
}

public protocol RelayWakeRoutePublisher: Sendable {
    func publish(routeId: String, wakeCredential: String) async throws
}

/// Optional APNs relay enrollment. The host creates this only after explicit opt-in; no provider
/// token or route credential is exposed to SwiftUI or persisted outside the Keychain.
public actor RelayWakeClient {
    private let origin: ServerOrigin
    private let transport: any HTTPTransport
    private let secure: any SecureStore
    private let publisher: any RelayWakeRoutePublisher
    private let provider: String

    public init(origin: ServerOrigin, transport: any HTTPTransport, secure: any SecureStore,
                publisher: any RelayWakeRoutePublisher, provider: String = "apns") {
        self.origin = origin; self.transport = transport; self.secure = secure
        self.publisher = publisher; self.provider = provider
    }

    /// Registers a current APNs token and saves the opaque registration ID while waiting for push.
    @discardableResult public func beginEnrollment(deviceToken: String) async throws -> Bool {
        guard provider == "apns", !deviceToken.isEmpty, deviceToken.utf8.count <= 4096 else { return false }
        let response = try await request("POST", "/v1/registrations", ["provider": provider, "device_token": deviceToken])
        guard response.status == 201,
              let body = try json(response.body), let id = body["registration_id"] as? String,
              Self.uuid(id) != nil else { return false }
        try secure.upsert(account("pending-registration"), Data(id.utf8))
        return true
    }

    /// Called only for a provider-delivered `kind=challenge` push. A malformed or stale challenge
    /// simply leaves the optional relay disabled; it never affects authoritative sync.
    @discardableResult public func confirm(challenge: String, installationPublicIdentity: String? = nil) async throws -> Bool {
        guard Self.secret(challenge), let data = try secure.read(account("pending-registration")),
              let id = String(data: data, encoding: .utf8), Self.uuid(id) != nil else { return false }
        var body: [String: Any] = ["challenge": challenge]
        if let installationPublicIdentity, !installationPublicIdentity.isEmpty { body["installation_public_identity"] = installationPublicIdentity }
        let response = try await request("POST", "/v1/registrations/\(id)/confirm", body)
        guard response.status == 201, let value = try json(response.body),
              let routeId = value["route_id"] as? String, Self.uuid(routeId) != nil,
              let manage = value["manage_credential"] as? String, Self.secret(manage),
              let wake = value["wake_credential"] as? String, Self.secret(wake) else { return false }
        let route = RelayWakeRoute(routeId: routeId, manageCredential: manage, wakeCredential: wake)
        try secure.upsert(account("route"), try JSONEncoder().encode(route))
        // Keep native credentials if the server route update is temporarily unavailable; the next
        // opt-in/foreground retry can publish the same route without re-registering a provider token.
        try await publisher.publish(routeId: route.routeId, wakeCredential: route.wakeCredential)
        try secure.delete(account("pending-registration"))
        return true
    }

    public func route() throws -> RelayWakeRoute? {
        guard let data = try secure.read(account("route")) else { return nil }
        return try? JSONDecoder().decode(RelayWakeRoute.self, from: data)
    }

    /// Relay revocation is best effort: the authoritative server revoke still removes its route.
    public func revoke() async {
        guard let route = try? route() else { return }
        _ = try? await request("POST", "/v1/routes/\(route.routeId)/revoke", ["manage_credential": route.manageCredential])
        try? secure.delete(account("route"))
        try? secure.delete(account("pending-registration"))
    }

    private func request(_ method: String, _ path: String, _ object: [String: Any]) async throws -> HTTPResponse {
        try Task.checkCancellation()
        let body: Data
        do { body = try JSONSerialization.data(withJSONObject: object, options: [.withoutEscapingSlashes]) }
        catch { throw ClientError.invalidResponse("relay request") }
        let response = try await transport.send(HTTPRequest(method: method, url: origin.url(path), headers: ["Accept": "application/json", "Content-Type": "application/json"], body: body, maxResponseBytes: 16 * 1024))
        guard origin.contains(response.url) else { throw ClientError.originMismatch }
        return response
    }

    private func json(_ data: Data) throws -> [String: Any]? {
        guard let value = try? JSONSerialization.jsonObject(with: data) else { return nil }
        return value as? [String: Any]
    }
    private func account(_ name: String) -> String { "relay-wake-v1|\(origin.serialized)|\(name)" }
    private static func uuid(_ value: String) -> UUID? { UUID(uuidString: value).flatMap { $0.uuidString.lowercased() == value.lowercased() ? $0 : nil } }
    private static func secret(_ value: String) -> Bool { value.count == 64 && value.allSatisfy { $0.isHexDigit } }
}
