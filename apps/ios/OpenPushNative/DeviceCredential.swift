import Foundation

/// The vault/device a credential and its local secrets are bound to.
public struct EnrollmentIdentity: Hashable, Sendable, Codable {
    public let origin: String
    public let vaultId: String
    public let deviceId: String

    /// Secure-store account name for one secret purpose of this enrollment.
    func account(_ purpose: String) -> String { "\(purpose)|\(origin)|\(vaultId)|\(deviceId)" }
}

/// A strict v1 device credential file, as written by operator/simulator pairing:
/// `{"version":1,"origin":…,"vaultId":…,"deviceId":…,"deviceToken":<96 hex>}`.
public struct DeviceCredential: Sendable, CustomStringConvertible, CustomDebugStringConvertible, CustomReflectable {
    public static let maxFileBytes = 4096
    public let origin: ServerOrigin
    public let identity: EnrollmentIdentity
    let token: String

    public static func parse(_ data: Data, allowLoopbackHTTP: Bool) throws(ClientError) -> DeviceCredential {
        guard data.count <= maxFileBytes else { throw .invalidCredential("file too large") }
        guard let object = try? JSONSerialization.jsonObject(with: data),
              let fields = object as? [String: Any]
        else { throw .invalidCredential("not a JSON object") }
        guard Set(fields.keys) == ["version", "origin", "vaultId", "deviceId", "deviceToken"] else {
            throw .invalidCredential("unexpected fields")
        }
        guard let version = fields["version"] as? NSNumber, !version.isBoolean, version == 1 else {
            throw .invalidCredential("unsupported version")
        }
        guard let rawOrigin = fields["origin"] as? String else { throw .invalidCredential("origin") }
        let origin = try ServerOrigin(canonical: rawOrigin, allowLoopbackHTTP: allowLoopbackHTTP)
        guard let vault = (fields["vaultId"] as? String).flatMap(UUID.init(uuidString:)),
              let device = (fields["deviceId"] as? String).flatMap(UUID.init(uuidString:))
        else { throw .invalidCredential("vault or device id") }
        guard let token = fields["deviceToken"] as? String, token.utf8.count == 96,
              token.utf8.allSatisfy(\.isASCIIHexDigit)
        else { throw .invalidCredential("device token") }
        return DeviceCredential(
            origin: origin,
            identity: EnrollmentIdentity(
                origin: origin.serialized,
                vaultId: vault.uuidString.lowercased(),
                deviceId: device.uuidString.lowercased()
            ),
            token: token
        )
    }

    public var description: String { "DeviceCredential(\(identity.origin), vault \(identity.vaultId), device \(identity.deviceId), token <redacted>)" }
    public var debugDescription: String { description }
    public var customMirror: Mirror { Mirror(self, children: ["identity": identity, "token": "<redacted>"]) }
}

extension NSNumber {
    var isBoolean: Bool { CFGetTypeID(self) == CFBooleanGetTypeID() }
}

extension UInt8 {
    var isASCIIHexDigit: Bool { (48...57).contains(self) || (65...70).contains(self) || (97...102).contains(self) }
}
