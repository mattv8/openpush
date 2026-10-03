import Foundation
import Testing
@testable import PeppyNative

@Suite struct CredentialTests {
    let token = String(repeating: "0f", count: 48)
    let vault = "6F0D2C8E-6E4B-4D7B-9E1A-0B7C2D4E6F80"
    let device = "1b2c3d4e-5f60-4718-8a9b-0c1d2e3f4a5b"

    func json(_ overrides: [String: Any] = [:], drop: String? = nil) -> Data {
        var object: [String: Any] = [
            "version": 1, "origin": "https://push.example.test", "vaultId": vault, "deviceId": device, "deviceToken": token,
        ]
        for (key, value) in overrides { object[key] = value }
        if let drop { object.removeValue(forKey: drop) }
        return try! JSONSerialization.data(withJSONObject: object)
    }

    @Test func parsesStrictV1AndCanonicalizesIdentity() throws {
        let credential = try DeviceCredential.parse(json(), allowLoopbackHTTP: false)
        #expect(credential.identity == EnrollmentIdentity(
            origin: "https://push.example.test", vaultId: vault.lowercased(), deviceId: device
        ))
        #expect(credential.token == token)
    }

    @Test func neverPrintsTheToken() throws {
        let credential = try DeviceCredential.parse(json(), allowLoopbackHTTP: false)
        var dumped = ""
        dump(credential, to: &dumped)
        for text in [String(describing: credential), String(reflecting: credential), dumped] {
            #expect(!text.contains(token))
        }
    }

    static let malformed: [String: (overrides: [String: String], drop: String?)] = [
        "extra field": (["extra": "x"], nil),
        "missing token": ([:], "deviceToken"),
        "version string": (["version": "1"], nil),
        "short token": (["deviceToken": "abc"], nil),
        "non-hex token": (["deviceToken": String(repeating: "zz", count: 48)], nil),
        "bad uuid": (["vaultId": "vault"], nil),
        "trailing slash": (["origin": "https://push.example.test/"], nil),
        "path": (["origin": "https://push.example.test/api"], nil),
        "uppercase host": (["origin": "https://Push.example.test"], nil),
        "default port": (["origin": "https://push.example.test:443"], nil),
        "userinfo": (["origin": "https://user@push.example.test"], nil),
        "query": (["origin": "https://push.example.test?x=1"], nil),
        "scheme": (["origin": "ftp://push.example.test"], nil),
    ]

    @Test(arguments: malformed.keys.sorted())
    func rejectsMalformedCredentials(_ name: String) {
        let (overrides, drop) = Self.malformed[name]!
        #expect(throws: ClientError.self, "\(name)") {
            try DeviceCredential.parse(json(overrides, drop: drop), allowLoopbackHTTP: true)
        }
    }

    @Test func rejectsNonNumericVersions() {
        #expect(throws: ClientError.invalidCredential("unsupported version")) {
            try DeviceCredential.parse(json(["version": true]), allowLoopbackHTTP: false)
        }
        #expect(throws: ClientError.invalidCredential("unsupported version")) {
            try DeviceCredential.parse(json(["version": 2]), allowLoopbackHTTP: false)
        }
    }

    @Test func rejectsOversizedFiles() {
        let padded = json() + Data(repeating: 0x20, count: DeviceCredential.maxFileBytes)
        #expect(throws: ClientError.invalidCredential("file too large")) { try DeviceCredential.parse(padded, allowLoopbackHTTP: false) }
    }

    @Test func plainHTTPOnlyForExplicitLoopback() throws {
        #expect(throws: ClientError.insecureOrigin) {
            try ServerOrigin(canonical: "http://push.example.test", allowLoopbackHTTP: true)
        }
        #expect(throws: ClientError.insecureOrigin) {
            try ServerOrigin(canonical: "http://127.0.0.1:8080", allowLoopbackHTTP: false)
        }
        let loopback = try ServerOrigin(canonical: "http://127.0.0.1:8080", allowLoopbackHTTP: true)
        #expect(loopback.url("/v1/vault").absoluteString == "http://127.0.0.1:8080/v1/vault")
        let https = try ServerOrigin(canonical: "https://push.example.test:8443", allowLoopbackHTTP: false)
        #expect(https.contains(URL(string: "https://PUSH.example.test:8443/v1/events")))
        #expect(!https.contains(URL(string: "https://push.example.test/v1/events")))
        #expect(!https.contains(URL(string: "https://evil.example.test:8443/v1/events")))
    }

    @Test func keychainItemsAreDeviceOnlyAfterFirstUnlockAndNeverSynchronized() {
        let store = KeychainSecureStore(service: "test.service")
        let query = store.query("database-key|x")
        #expect(query[kSecAttrSynchronizable as String] as? Bool == false)
        #expect(query[kSecAttrService as String] as? String == "test.service")
        #expect(query[kSecAttrAccount as String] as? String == "database-key|x")
        #expect(KeychainSecureStore.accessibility == kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly)
    }

    @Test func telephonyIsNeverExecutableInThisFoundation() {
        #expect(!TelephonyEligibility.evaluate().canExecuteCarrierCommands)
        let best = TelephonyEligibility.evaluate(osMajorVersion: 26, entitlementDeclared: true, executorImplemented: true)
        #expect(best.blockers == [.defaultAppAndRegionUnverified])
        #expect(!best.canExecuteCarrierCommands)
        let actual = TelephonyEligibility.evaluate(osMajorVersion: 18)
        #expect(actual.blockers == [
            .osTooOld(major: 18), .entitlementNotDeclared, .defaultAppAndRegionUnverified, .carrierExecutorNotImplemented,
        ])
    }
}
