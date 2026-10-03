import Testing
@testable import PeppyNative

@Suite struct BackgroundWakeCancellationTests {
    @Test func expiryBeforeInstallCancelsTheNewTask() async {
        let holder = await MainActor.run { BackgroundWakeCancellation() }
        await holder.expire()
        let task = Task<Void, Never> { try? await Task.sleep(for: .seconds(60)) }
        await holder.install(task)
        #expect(task.isCancelled)
    }
}
