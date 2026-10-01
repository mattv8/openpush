import Foundation

/// Carrier (SMS/MMS/RCS) capability of this iOS build. It is derived from what the build actually
/// contains, not from user claims, so it cannot report an entitlement, carrier service or
/// default-app selection that was never granted or verified.
public struct TelephonyEligibility: Equatable, Sendable {
    public enum Blocker: Equatable, Sendable {
        /// TelephonyMessagingKit needs iOS 26 or later.
        case osTooOld(major: Int)
        /// The app does not declare Apple's carrier-messaging entitlement (none has been granted).
        case entitlementNotDeclared
        /// Default-messaging-app selection and EU account/device/location eligibility are not checked by this build.
        case defaultAppAndRegionUnverified
        /// No carrier send/receive executor exists in this foundation; core commands are never executed.
        case carrierExecutorNotImplemented
    }

    /// Facts compiled into this build. Neither is configurable at runtime.
    static let entitlementDeclared = false
    static let carrierExecutorImplemented = false

    public let blockers: [Blocker]

    /// True only when nothing blocks carrier execution. Always false in this foundation.
    public var canExecuteCarrierCommands: Bool { blockers.isEmpty }

    public static func evaluate(osMajorVersion: Int = ProcessInfo.processInfo.operatingSystemVersion.majorVersion) -> TelephonyEligibility {
        evaluate(osMajorVersion: osMajorVersion, entitlementDeclared: entitlementDeclared, executorImplemented: carrierExecutorImplemented)
    }

    static func evaluate(osMajorVersion: Int, entitlementDeclared: Bool, executorImplemented: Bool) -> TelephonyEligibility {
        var blockers: [Blocker] = []
        if osMajorVersion < 26 { blockers.append(.osTooOld(major: osMajorVersion)) }
        if !entitlementDeclared { blockers.append(.entitlementNotDeclared) }
        // Even with an entitlement, eligibility is per user/device/region and is not verified here.
        blockers.append(.defaultAppAndRegionUnverified)
        if !executorImplemented { blockers.append(.carrierExecutorNotImplemented) }
        return TelephonyEligibility(blockers: blockers)
    }
}

extension TelephonyEligibility.Blocker {
    public var explanation: String {
        switch self {
        case .osTooOld(let major): "Requires iOS 26 or later (this device runs \(major))."
        case .entitlementNotDeclared: "This build has no Apple carrier-messaging entitlement."
        case .defaultAppAndRegionUnverified: "Default messaging app selection and EU eligibility are not verified."
        case .carrierExecutorNotImplemented: "Carrier sending is not implemented on iOS; send commands stay queued."
        }
    }
}
