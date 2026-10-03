import Foundation
#if canImport(PeppyBindings)
import PeppyBindings
#endif

/// Every failure the native layer reports. Values never contain tokens, keys or passphrases.
public enum ClientError: Error, Equatable, Sendable {
    case invalidCredential(String)
    case insecureOrigin
    case identityMismatch
    case alreadyEnrolled
    case notEnrolled
    case missingSecret(String)
    case unauthorized
    case forbidden
    case server(status: Int, code: String?)
    case resyncRequired(reason: String)
    case redirectRefused
    case originMismatch
    case responseTooLarge(limit: Int)
    case requestTooLarge(limit: Int)
    case invalidResponse(String)
    case network(code: Int)
    case canceled
    case secureStorage(status: Int32)
    case core(MobileBindingsError)
    case syncInProgress
    case enrollmentChangeInProgress
    /// The request or apply allowance of this pass is spent; the next pass continues.
    case budgetExhausted
    /// This identity's local database existed but is gone (e.g. app reinstalled while the Keychain
    /// survived). It is never silently recreated: disconnect and pair as a new device.
    case localDatabaseMissing
    /// A local (non-network) failure; only the error type name is kept.
    case unexpected(String)

    /// Maps cancellation, generated core errors and URL errors; anything else keeps only its type name.
    static func wrap(_ error: any Error) -> ClientError {
        switch error {
        case let error as ClientError: return error
        case let error as MobileBindingsError: return .core(error)
        case is CancellationError: return .canceled
        case let error as URLError where error.code == .cancelled: return .canceled
        case let error as URLError: return .network(code: error.code.rawValue)
        default: return .unexpected(String(describing: type(of: error)))
        }
    }

    public var userMessage: String {
        switch self {
        case .invalidCredential(let reason): "The credential file is not a valid Peppy v1 credential (\(reason))."
        case .insecureOrigin: "The credential's server must use HTTPS."
        case .identityMismatch: "The server answered for a different vault or device. Nothing was saved."
        case .alreadyEnrolled: "This device already holds a different enrollment."
        case .notEnrolled: "Import a device credential first."
        case .missingSecret(let name): "The \(name) is missing from the Keychain on this device."
        case .unauthorized: "The server rejected this device credential (revoked or unknown)."
        case .forbidden: "This device is not allowed to perform that request."
        case .server(let status, let code): "Server error \(status)\(code.map { " (\($0))" } ?? "")."
        case .resyncRequired(let reason): "The server requires a snapshot resync (\(reason))."
        case .redirectRefused: "The server attempted a redirect; it was refused."
        case .originMismatch: "A response came from an unexpected origin; it was refused."
        case .responseTooLarge(let limit): "A server response exceeded \(limit) bytes and was discarded."
        case .requestTooLarge(let limit): "A queued envelope exceeded \(limit) bytes and was not sent."
        case .invalidResponse(let what): "The server returned an invalid response (\(what))."
        case .network(let code): "Network request failed (URLError \(code))."
        case .canceled: "Canceled."
        case .secureStorage(let status): "Keychain access failed (OSStatus \(status))."
        case .core(let error): Self.coreMessage(error)
        case .syncInProgress: "A sync pass is already running."
        case .enrollmentChangeInProgress: "Another enrollment change is in progress."
        case .budgetExhausted: "This pass reached its work limit; the next pass continues."
        case .localDatabaseMissing: "This device's local database is missing (for example after reinstalling). It is not recreated for the same device: disconnect, then pair this phone as a new device."
        case .unexpected(let kind): "Unexpected local error (\(kind))."
        }
    }

    private static func coreMessage(_ error: MobileBindingsError) -> String {
        switch error {
        case .WrongPassphrase: "That is not the vault passphrase."
        case .KeysUnavailable: "Vault keys are locked. Enter the vault passphrase to unlock."
        case .WrongDatabaseKey, .InvalidDatabaseKey: "The local database key does not open the local database."
        case .IdentityMismatch: "Local data belongs to a different vault or device."
        case .InvalidProfile: "The vault key profile is invalid or does not match."
        case .InvalidKeyCache: "A stored key cache was rejected."
        case .UnsupportedSchema: "The local database was written by a newer version."
        case .Closed: "The local client is closed."
        case .Conflict: "Local state conflicts with the server (snapshot or envelope conflict)."
        default: "Local client error: \(error)."
        }
    }
}
