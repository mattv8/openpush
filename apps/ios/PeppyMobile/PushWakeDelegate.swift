import Foundation
#if canImport(UIKit)
import UIKit
import UserNotifications
#if canImport(PeppyNative)
import PeppyNative
#endif

/// App-owned wiring for optional APNs wake hints. Configure once at launch after an explicit user
/// opt-in. It does not create a socket or promise background execution.
@MainActor
final class PushWakeDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    private static weak var model: AppModel?

    static func install(model: AppModel) {
        self.model = model
    }

    /// Called only after the Settings opt-in created a relay client.
    func registerForRemoteNotifications() {
        UIApplication.shared.registerForRemoteNotifications()
    }

    func application(_ application: UIApplication, didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        if Self.model?.relay != nil { registerForRemoteNotifications() }
        return true
    }

    func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken token: Data) {
        let value = token.map { String(format: "%02x", $0) }.joined()
        guard let relay = Self.model?.relay else { return }
        Task { @MainActor in _ = try? await relay.beginEnrollment(deviceToken: value) }
    }

    func application(_ application: UIApplication, didReceiveRemoteNotification userInfo: [AnyHashable: Any], fetchCompletionHandler completionHandler: @escaping (UIBackgroundFetchResult) -> Void) {
        let callback = BackgroundFetchCallback(completionHandler)
        let completion = OnceCompletion { callback.call($0 ? .newData : .failed) }
        guard let peppy = userInfo["peppy"] as? [AnyHashable: Any],
              let kind = peppy["kind"] as? String else { completion.complete(false); return }
        let cancellation = BackgroundWakeCancellation()
        let background = application.beginBackgroundTask(withName: "peppy-push-wake") {
            Task { @MainActor in
                cancellation.expire()
                completion.complete(false)
            }
        }
        let task = Task { @MainActor [weak application] in
            defer { if background != .invalid { application?.endBackgroundTask(background) } }
            guard !Task.isCancelled else { return }
            let model = Self.model
            switch kind {
            case "challenge":
                let value = peppy["value"] as? String ?? ""
                completion.complete((try? await model?.relay?.confirm(challenge: value)) ?? false)
            case "wake":
                // Both passes are bounded by their native coordinators; cancellation propagates on expiry.
                let synced = await model?.wakeSync() ?? false
                let contactsDone = await model?.wakeContacts() ?? true
                completion.complete(synced || contactsDone)
            default: completion.complete(false)
            }
        }
        cancellation.install(task)
    }
}

/// UIKit's completion closure is not `Sendable`; keep it behind this one-shot-safe wrapper before
/// handing it to the sendable completion coordinator used by the background worker.
private final class BackgroundFetchCallback: @unchecked Sendable {
    private let handler: (UIBackgroundFetchResult) -> Void
    init(_ handler: @escaping (UIBackgroundFetchResult) -> Void) { self.handler = handler }
    func call(_ result: UIBackgroundFetchResult) { handler(result) }
}
#endif
