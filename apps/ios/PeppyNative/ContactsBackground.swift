import Foundation

/// Sendable wrapper for cancellation state; allows Sendable closures to check it.
/// Thread-safe via NSLock; marked @unchecked because lock protects access.
private final class CancellationFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var _cancelled = false

    func check() -> Bool {
        lock.withLock { _cancelled }
    }

    func set() {
        lock.withLock { _cancelled = true }
    }
}

/// Serializes foreground, change-notification and BackgroundTasks attempts into one cancellation-aware
/// pass. Checkpointing remains in Rust; this actor intentionally has no plaintext persistence.
///
/// A request that arrives while a pass runs is not dropped: it queues exactly one follow-up pass,
/// which runs after the current one unless that pass was cancelled.
public actor ContactsPassCoordinator {
    public typealias Work = @Sendable (ContactsPassReason, @escaping @Sendable () -> Bool) async throws -> Bool
    private var running = false
    private var followUp: ContactsPassReason?
    private var cancellationFlag: CancellationFlag?
    private let work: Work

    public init(work: @escaping Work) { self.work = work }

    /// Returns the last pass's result, or false when this request was queued behind a running pass.
    @discardableResult
    public func run(_ reason: ContactsPassReason) async throws -> Bool {
        guard !running else {
            followUp = followUp ?? reason
            return false
        }
        running = true
        defer { running = false; cancellationFlag = nil }
        var reason = reason
        while true {
            let flag = CancellationFlag()
            cancellationFlag = flag
            // Cancelling the calling task (e.g. the scene leaving the foreground) also stops the pass.
            let result = try await work(reason, { flag.check() || Task.isCancelled })
            guard let next = followUp, !flag.check(), !Task.isCancelled else { return result }
            followUp = nil
            reason = next
        }
    }

    public func cancel() {
        followUp = nil
        cancellationFlag?.set()
    }
}

/// Completes a background task exactly once, whether the pass finishes or the system expires it.
final class OnceCompletion: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false
    private let body: @Sendable (Bool) -> Void

    init(_ body: @escaping @Sendable (Bool) -> Void) { self.body = body }

    func complete(_ success: Bool) {
        let first = lock.withLock { () -> Bool in
            defer { done = true }
            return !done
        }
        if first { body(success) }
    }
}

#if os(iOS)
import BackgroundTasks

/// Registers the contact BackgroundTasks. Create it at app launch, keep it for the process lifetime,
/// and call `register()` before the app finishes launching.
@available(iOS 13.0, *)
@MainActor
public final class ContactsBackgroundScheduler {
    public static let refreshIdentifier = "dev.peppy.mobile.contacts-refresh"
    public static let processingIdentifier = "dev.peppy.mobile.contacts-processing"
    /// No latency promise: iOS decides when (and whether) these run.
    public static let refreshInterval: TimeInterval = 30 * 60
    public static let processingInterval: TimeInterval = 6 * 60 * 60
    private let coordinator: ContactsPassCoordinator

    public init(coordinator: ContactsPassCoordinator) { self.coordinator = coordinator }

    /// BackgroundTasks invokes handlers on the supplied queue. Keeping registration and task
    /// ownership on the main actor prevents the SDK's non-Sendable `BGTask` from crossing actors.
    public func register() {
        BGTaskScheduler.shared.register(forTaskWithIdentifier: Self.refreshIdentifier, using: .main) { task in
            MainActor.assumeIsolated { self.run(task, reason: .backgroundRefresh) }
        }
        BGTaskScheduler.shared.register(forTaskWithIdentifier: Self.processingIdentifier, using: .main) { task in
            MainActor.assumeIsolated { self.run(task, reason: .backgroundProcessing) }
        }
    }

    /// Submits both requests; a pending request with the same identifier is replaced.
    public func scheduleAll() {
        try? scheduleRefresh()
        try? scheduleProcessing()
    }

    public func scheduleRefresh() throws {
        let request = BGAppRefreshTaskRequest(identifier: Self.refreshIdentifier)
        request.earliestBeginDate = Date(timeIntervalSinceNow: Self.refreshInterval)
        try BGTaskScheduler.shared.submit(request)
    }

    public func scheduleProcessing() throws {
        let request = BGProcessingTaskRequest(identifier: Self.processingIdentifier)
        request.earliestBeginDate = Date(timeIntervalSinceNow: Self.processingInterval)
        request.requiresNetworkConnectivity = true
        request.requiresExternalPower = false
        try BGTaskScheduler.shared.submit(request)
    }

    private func run(_ task: BGTask, reason: ContactsPassReason) {
        // Reschedule first so an expired or failed pass still leaves the next attempt queued.
        if reason == .backgroundRefresh { try? scheduleRefresh() } else { try? scheduleProcessing() }
        BackgroundTaskLifecycle(task: task, coordinator: coordinator).start(reason: reason)
    }
}

/// Main-actor ownership is the sendability boundary for the SDK's non-Sendable `BGTask`.
/// Completion and expiration are serialized here, so `setTaskCompleted` is called exactly once.
@MainActor
private final class BackgroundTaskLifecycle {
    private let task: BGTask
    private let coordinator: ContactsPassCoordinator
    private var work: Task<Void, Never>?
    private var completed = false

    init(task: BGTask, coordinator: ContactsPassCoordinator) {
        self.task = task
        self.coordinator = coordinator
    }

    func start(reason: ContactsPassReason) {
        // BGTask retains this handler, which retains the lifecycle until `complete` breaks the cycle.
        task.expirationHandler = { @Sendable [self] in
            Task { @MainActor [self] in expire() }
        }
        work = Task { @MainActor [self] in
            let success = (try? await coordinator.run(reason)) ?? false
            complete(success)
        }
    }

    private func expire() {
        guard !completed else { return }
        work?.cancel()
        Task { await coordinator.cancel() }
        complete(false)
    }

    private func complete(_ success: Bool) {
        guard !completed else { return }
        completed = true
        task.expirationHandler = nil
        task.setTaskCompleted(success: success)
        work = nil
    }
}
#endif
