import Foundation

/// A canonical `scheme://host[:port]` origin. HTTPS is required; plain HTTP is accepted only for
/// loopback hosts and only when the caller explicitly allows it (debug builds).
public struct ServerOrigin: Hashable, Sendable, CustomStringConvertible {
    public let serialized: String
    let scheme: String
    let host: String
    let port: Int?

    public init(canonical value: String, allowLoopbackHTTP: Bool) throws(ClientError) {
        guard let parts = URLComponents(string: value),
              let scheme = parts.scheme?.lowercased(), let rawHost = parts.host, !rawHost.isEmpty,
              parts.user == nil, parts.password == nil, parts.path.isEmpty,
              parts.query == nil, parts.fragment == nil
        else { throw .invalidCredential("origin") }
        let host = rawHost.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        switch scheme {
        case "https": break
        case "http" where allowLoopbackHTTP && ["localhost", "127.0.0.1", "::1"].contains(host): break
        case "http": throw .insecureOrigin
        default: throw .invalidCredential("origin")
        }
        let defaultPort = scheme == "https" ? 443 : 80
        let port = parts.port == defaultPort ? nil : parts.port
        let hostPart = host.contains(":") ? "[\(host)]" : host
        let serialized = "\(scheme)://\(hostPart)\(port.map { ":\($0)" } ?? "")"
        // Exact canonical form, as written by the pairing tools (no trailing slash, default port omitted).
        guard serialized == value else { throw .invalidCredential("origin is not canonical") }
        self.serialized = serialized
        self.scheme = scheme
        self.host = host
        self.port = port
    }

    public var description: String { serialized }

    /// Builds a URL on this origin. `path` is a fixed API path chosen by this module, never server input.
    func url(_ path: String, query: [URLQueryItem] = []) -> URL {
        var parts = URLComponents()
        parts.scheme = scheme
        parts.host = host.contains(":") ? "[\(host)]" : host
        parts.port = port
        parts.path = path
        if !query.isEmpty { parts.queryItems = query }
        guard let url = parts.url else { preconditionFailure("fixed API path must form a URL") }
        return url
    }

    func contains(_ url: URL?) -> Bool {
        guard let url, let parts = URLComponents(url: url, resolvingAgainstBaseURL: false) else { return false }
        let defaultPort = scheme == "https" ? 443 : 80
        let port = parts.port == defaultPort ? nil : parts.port
        return parts.scheme?.lowercased() == scheme
            && parts.host?.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "[]")) == host
            && port == self.port
    }
}
