import Foundation

/// Main-actor state shared by the APNs expiry callback and its background task. Installing after
/// expiry immediately cancels the new task, closing the launch/expiry race without shared mutable
/// state in UIKit's sendable expiration closure.
@MainActor
public final class BackgroundWakeCancellation {
    private var expired = false
    private var task: Task<Void, Never>?

    public init() {}

    public func install(_ task: Task<Void, Never>) {
        self.task = task
        if expired { task.cancel() }
    }

    public func expire() {
        expired = true
        task?.cancel()
    }
}
