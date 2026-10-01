import Foundation

/// The shared request allowance of one operation. Every server call spends one request.
struct RequestMeter: Sendable {
    let limit: Int
    private(set) var used = 0

    init(limit: Int) { self.limit = limit }

    var exhausted: Bool { used >= limit }

    mutating func spend() throws {
        guard used < limit else { throw ClientError.budgetExhausted }
        used += 1
    }
}

/// Bearer-authenticated JSON calls bound to one origin. Paths are fixed by this module.
/// Its descriptions never include the token.
struct ServerClient: Sendable, CustomStringConvertible, CustomDebugStringConvertible, CustomReflectable {
    static let maxVaultBytes = 4 * 1024 * 1024
    /// Server pages hold at most 8 MiB of envelopes plus at least one row.
    static let maxPageBytes = 12 * 1024 * 1024
    static let maxSmallBytes = 64 * 1024
    static let maxEnvelopeBytes = 4 * 1024 * 1024

    let origin: ServerOrigin
    let token: String
    let transport: any HTTPTransport

    var description: String { "ServerClient(\(origin), token <redacted>)" }
    var debugDescription: String { description }
    var customMirror: Mirror { Mirror(self, children: ["origin": origin, "token": "<redacted>"]) }

    func get(_ path: String, query: [URLQueryItem] = [], limit: Int, meter: inout RequestMeter) async throws -> [String: Any] {
        try await call("GET", path, query: query, body: nil, limit: limit, meter: &meter)
    }

    func post(_ path: String, json: Data, limit: Int = maxSmallBytes, meter: inout RequestMeter) async throws -> [String: Any] {
        guard json.count <= Self.maxEnvelopeBytes else { throw ClientError.requestTooLarge(limit: Self.maxEnvelopeBytes) }
        return try await call("POST", path, query: [], body: json, limit: limit, meter: &meter)
    }

    private func call(
        _ method: String, _ path: String, query: [URLQueryItem], body: Data?, limit: Int, meter: inout RequestMeter
    ) async throws -> [String: Any] {
        try Task.checkCancellation()
        try meter.spend()
        var headers = ["Authorization": "Bearer \(token)", "Accept": "application/json"]
        if body != nil { headers["Content-Type"] = "application/json" }
        let response = try await transport.send(HTTPRequest(
            method: method, url: origin.url(path, query: query), headers: headers, body: body, maxResponseBytes: limit
        ))
        guard origin.contains(response.url) else { throw ClientError.originMismatch }
        let object = (try? JSONSerialization.jsonObject(with: response.body)) as? [String: Any]
        switch response.status {
        case 200..<300:
            guard let object else { throw ClientError.invalidResponse("\(path) body") }
            return object
        case 401: throw ClientError.unauthorized
        case 403: throw ClientError.forbidden
        case 409 where object?["code"] as? String == "resync_required":
            throw ClientError.resyncRequired(reason: object?["reason"] as? String ?? "unknown")
        default:
            throw ClientError.server(status: response.status, code: object?["code"] as? String)
        }
    }
}

/// Typed accessors for server JSON; every failure is `ClientError.invalidResponse`.
extension Dictionary where Key == String, Value == Any {
    func string(_ key: String) throws -> String {
        guard let value = self[key] as? String else { throw ClientError.invalidResponse(key) }
        return value
    }

    /// Cursors and counts are decimal strings on the wire.
    func decimal(_ key: String) throws -> UInt64 {
        guard let value = UInt64(try string(key)) else { throw ClientError.invalidResponse(key) }
        return value
    }

    func optionalDecimal(_ key: String) throws -> UInt64? {
        if self[key] == nil || self[key] is NSNull { return nil }
        return try decimal(key)
    }

    func integer(_ key: String) throws -> Int {
        guard let number = self[key] as? NSNumber, !number.isBoolean,
              let value = Int(exactly: number.doubleValue) else { throw ClientError.invalidResponse(key) }
        return value
    }

    func objects(_ key: String) throws -> [[String: Any]] {
        guard let value = self[key] as? [[String: Any]] else { throw ClientError.invalidResponse(key) }
        return value
    }

    func object(_ key: String) throws -> [String: Any] {
        guard let value = self[key] as? [String: Any] else { throw ClientError.invalidResponse(key) }
        return value
    }

    func jsonData() throws -> Data {
        do { return try JSONSerialization.data(withJSONObject: self, options: [.withoutEscapingSlashes]) }
        catch { throw ClientError.invalidResponse("envelope") }
    }
}
