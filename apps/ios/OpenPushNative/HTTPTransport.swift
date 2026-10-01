import Foundation

/// Descriptions redact the `Authorization` header and omit the body.
public struct HTTPRequest: Sendable, CustomStringConvertible, CustomDebugStringConvertible, CustomReflectable {
    public var method: String
    public var url: URL
    public var headers: [String: String]
    public var body: Data?
    /// Response bodies longer than this are discarded with `ClientError.responseTooLarge`.
    public var maxResponseBytes: Int

    public var description: String { "\(method) \(url.absoluteString) headers=\(redactedHeaders) body=\(body?.count ?? 0)B" }
    public var debugDescription: String { description }
    public var customMirror: Mirror {
        Mirror(self, children: ["method": method, "url": url, "headers": redactedHeaders, "bodyBytes": body?.count ?? 0])
    }

    private var redactedHeaders: [String: String] {
        headers.reduce(into: [:]) { $0[$1.key] = $1.key.lowercased() == "authorization" ? "<redacted>" : $1.value }
    }
}

public struct HTTPResponse: Sendable {
    public var status: Int
    public var url: URL?
    public var body: Data
}

/// Network boundary. Implementations throw `ClientError` only and must never follow redirects.
public protocol HTTPTransport: Sendable {
    func send(_ request: HTTPRequest) async throws -> HTTPResponse
}

/// Ephemeral URLSession: no cookies, cache or credential storage; redirects refused; bounded bodies.
/// Cancelling the calling Swift task cancels the request.
public final class URLSessionTransport: HTTPTransport {
    private static let chunkBytes = 64 * 1024
    private let session: URLSession

    public init(configuration: URLSessionConfiguration = .ephemeral) {
        configuration.httpCookieStorage = nil
        configuration.httpShouldSetCookies = false
        configuration.urlCache = nil
        configuration.urlCredentialStorage = nil
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.timeoutIntervalForRequest = 20
        configuration.timeoutIntervalForResource = 60
        configuration.waitsForConnectivity = false
        session = URLSession(configuration: configuration, delegate: RedirectRefusal(), delegateQueue: nil)
    }

    deinit { session.invalidateAndCancel() }

    public func send(_ request: HTTPRequest) async throws -> HTTPResponse {
        var urlRequest = URLRequest(url: request.url)
        urlRequest.httpMethod = request.method
        urlRequest.httpBody = request.body
        for (name, value) in request.headers { urlRequest.setValue(value, forHTTPHeaderField: name) }
        do {
            let (bytes, response) = try await session.bytes(for: urlRequest)
            guard let http = response as? HTTPURLResponse else { throw ClientError.invalidResponse("not HTTP") }
            if (300..<400).contains(http.statusCode) { throw ClientError.redirectRefused }
            let limit = request.maxResponseBytes
            if http.expectedContentLength > Int64(limit) { throw ClientError.responseTooLarge(limit: limit) }
            // Reserve up to the declared length (never past the cap) and append in 64 KiB chunks.
            var body = Data(capacity: Int(min(max(http.expectedContentLength, 0), Int64(limit))))
            var chunk: [UInt8] = []
            chunk.reserveCapacity(Self.chunkBytes)
            for try await byte in bytes {
                guard body.count + chunk.count < limit else { throw ClientError.responseTooLarge(limit: limit) }
                chunk.append(byte)
                if chunk.count == Self.chunkBytes {
                    body.append(contentsOf: chunk)
                    chunk.removeAll(keepingCapacity: true)
                }
            }
            body.append(contentsOf: chunk)
            return HTTPResponse(status: http.statusCode, url: http.url, body: body)
        } catch {
            throw ClientError.wrap(error)
        }
    }
}

final class RedirectRefusal: NSObject, URLSessionTaskDelegate, Sendable {
    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest
    ) async -> URLRequest? {
        nil
    }
}
