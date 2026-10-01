import Foundation
import Testing
@testable import OpenPushNative

/// Serves canned responses to `URLSessionTransport` without a network.
final class StubProtocol: URLProtocol, @unchecked Sendable {
    nonisolated(unsafe) static var response: (status: Int, headers: [String: String], body: Data) = (200, [:], Data())
    nonisolated(unsafe) static var hang = false

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        if Self.hang { return }
        let reply = Self.response
        let http = HTTPURLResponse(url: request.url!, statusCode: reply.status, httpVersion: "HTTP/1.1", headerFields: reply.headers)!
        client?.urlProtocol(self, didReceive: http, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: reply.body)
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}

@Suite(.serialized) struct TransportTests {
    func transport() -> URLSessionTransport {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubProtocol.self]
        return URLSessionTransport(configuration: configuration)
    }

    func request(limit: Int = 1024) -> HTTPRequest {
        HTTPRequest(method: "GET", url: URL(string: "https://push.example.test/v1/vault")!, headers: [:], body: nil, maxResponseBytes: limit)
    }

    @Test func boundedBodiesAreReturned() async throws {
        StubProtocol.hang = false
        StubProtocol.response = (200, [:], Data("{\"ok\":true}".utf8))
        let response = try await transport().send(request())
        #expect(response.status == 200 && response.body == Data("{\"ok\":true}".utf8))
    }

    @Test func oversizedBodiesAreDiscarded() async throws {
        StubProtocol.hang = false
        StubProtocol.response = (200, [:], Data(repeating: 0x41, count: 2048))
        await #expect(throws: ClientError.responseTooLarge(limit: 1024)) { try await transport().send(request()) }
        StubProtocol.response = (200, ["Content-Length": "4096"], Data(repeating: 0x41, count: 4096))
        await #expect(throws: ClientError.responseTooLarge(limit: 1024)) { try await transport().send(request()) }
    }

    @Test func redirectResponsesAreRefused() async throws {
        StubProtocol.hang = false
        StubProtocol.response = (302, ["Location": "https://evil.example.test/"], Data())
        await #expect(throws: ClientError.redirectRefused) { try await transport().send(request()) }
        let followed = await RedirectRefusal().urlSession(
            URLSession.shared,
            task: URLSession.shared.dataTask(with: URL(string: "https://push.example.test")!),
            willPerformHTTPRedirection: HTTPURLResponse(url: URL(string: "https://push.example.test")!, statusCode: 302, httpVersion: nil, headerFields: nil)!,
            newRequest: URLRequest(url: URL(string: "https://evil.example.test")!)
        )
        #expect(followed == nil)
    }

    @Test func cancellingTheTaskCancelsTheRequest() async throws {
        StubProtocol.hang = true
        defer { StubProtocol.hang = false }
        let transport = transport()
        let task = Task { try await transport.send(request()) }
        try await Task.sleep(for: .milliseconds(100))
        task.cancel()
        await #expect(throws: ClientError.canceled) { try await task.value }
    }
}
