import Testing
import Foundation
@testable import PeppyNative

@Suite struct RelayWakeClientTests {
    @Test func confirmationStoresManageCredentialAndPublishesOnlyWakeRoute() async throws {
        let secure = InMemorySecureStore()
        let published = PublishedRoute()
        let transport = RelayTransport { request in
            if request.url.path == "/v1/registrations" { return response(request, 201, ["registration_id": "00000000-0000-4000-8000-000000000001"]) }
            return response(request, 201, ["route_id": "00000000-0000-4000-8000-000000000002", "manage_credential": String(repeating: "a", count: 64), "wake_credential": String(repeating: "b", count: 64)])
        }
        let client = RelayWakeClient(origin: try ServerOrigin(canonical: FakeServer.origin, allowLoopbackHTTP: false), transport: transport, secure: secure, publisher: published)
        #expect(try await client.beginEnrollment(deviceToken: "apns-token"))
        #expect(try await client.confirm(challenge: String(repeating: "f", count: 64)))
        let publishedRoute = await published.value
        #expect(publishedRoute?.0 == "00000000-0000-4000-8000-000000000002")
        #expect(publishedRoute?.1 == String(repeating: "b", count: 64))
        #expect(try await client.route()?.manageCredential == String(repeating: "a", count: 64))
    }
}

private struct RelayTransport: HTTPTransport {
    let handler: @Sendable (HTTPRequest) -> HTTPResponse
    func send(_ request: HTTPRequest) async throws -> HTTPResponse { handler(request) }
}
private actor PublishedRoute: RelayWakeRoutePublisher {
    var value: (String, String)?
    func publish(routeId: String, wakeCredential: String) async throws { value = (routeId, wakeCredential) }
}
private func response(_ request: HTTPRequest, _ status: Int, _ body: [String: Any]) -> HTTPResponse {
    HTTPResponse(status: status, url: request.url, body: try! JSONSerialization.data(withJSONObject: body))
}
