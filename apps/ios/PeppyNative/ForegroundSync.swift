import Foundation
#if canImport(PeppyBindings)
import PeppyBindings
#endif

/// Limits for one foreground pass. Requests and apply rounds are shared by every step, so repeated
/// resync answers cannot extend a pass. Anything left over is picked up by the next pass.
public struct SyncBudget: Sendable {
    /// Server requests per pass (uploads, replay pages, snapshot cuts and pages).
    public var maxRequests = 24
    /// Upload requests per pass, within `maxRequests`, so receive is never starved.
    public var maxUploads = 16
    /// Records per server page (the server allows at most 200; the core accepts at most 500).
    public var pageLimit = 100
    /// `applyPending` calls per pass.
    public var maxApplyRounds = 16
    public var applyLimit: UInt64 = 500

    public init() {}
}

public struct SyncReport: Equatable, Sendable {
    public var uploaded = 0
    /// Why uploads stopped early, if the server refused an envelope. The envelope stays queued
    /// (unacknowledged) and receive still runs so the core can reconcile it.
    public var uploadRefused: ClientError?
    public var journaled = 0
    public var duplicates = 0
    public var quarantinedOnIngest = 0
    public var applied: UInt64 = 0
    /// Journal rows an authoritative snapshot proved superseded (suppressed, not applied).
    public var superseded: UInt64 = 0
    public var quarantined: UInt64 = 0
    public var waitingForKeys: UInt64 = 0
    public var snapshotRecords = 0
    public var snapshotPublished = false
    public var snapshotRemaining: UInt64 = 0
    /// Commands addressed to this device. They are counted, never executed (see `TelephonyEligibility`).
    public var carrierCommandsNotExecuted = 0
    public var receiveCursor = "0"
    public var requests = 0
    public var applyRounds = 0
    /// False when the budget ran out; the next pass continues.
    public var complete = true
    /// The core latched a contact/notification projection repair that has not promoted yet.
    public var contactRepairRequired = false
    /// A repair snapshot was started in this pass.
    public var repairSnapshotStarted = false
    /// The newest authoritative projection failed (`snapshot_projection_status` reason).
    public var projectionFailure: String?
    /// The server offers no compaction snapshot, so a latched repair cannot run yet.
    public var repairUnavailable = false
}

/// One bounded, origin-bound foreground pass:
/// upload sealed outbox → finish staged snapshot → drain published snapshot → live replay (or a new
/// snapshot on `resync_required`) → bounded apply. All message state goes through the core facade.
/// Cancelling the calling task stops the pass between steps and cancels in-flight requests.
struct ForegroundSync: Sendable {
    let client: NativeClient
    let server: ServerClient
    let vaultId: String
    var budget = SyncBudget()

    /// Per-pass mutable state.
    private struct Pass {
        var report = SyncReport()
        var meter: RequestMeter
        var applyRounds = 0
    }

    func run() async throws -> SyncReport {
        var pass = Pass(meter: RequestMeter(limit: budget.maxRequests))
        do {
            try await probeSnapshotCapability(&pass)
            try await uploadOutbox(&pass)
            if let staged = try client.snapshotProgress() {
                try await importSnapshot(resuming: staged, restartOnResync: true, &pass)
            }
            // Published snapshot records must drain before live replay continues.
            try applyPending(&pass)
            try await repairIfRequired(&pass)
            if try !snapshotPending(pass) { try await receive(&pass) }
            try applyPending(&pass)
        } catch ClientError.budgetExhausted {
            pass.report.complete = false
        }
        if try snapshotPending(pass) { pass.report.complete = false }
        pass.report.contactRepairRequired = try client.contactRepairRequired()
        if let status = try client.snapshotProjectionStatus(), status.state == .failed {
            pass.report.projectionFailure = status.reason ?? "failed"
        }
        pass.report.carrierCommandsNotExecuted = try client.pendingCommands().count
        pass.report.receiveCursor = try client.receiveCursor()
        pass.report.requests = pass.meter.used
        pass.report.applyRounds = pass.applyRounds
        return pass.report
    }

    /// A metadata-only probe keeps compactable producers fail-closed when no resync is needed.
    private func probeSnapshotCapability(_ pass: inout Pass) async throws {
        do {
            try await server.postNoContent("/v1/compaction/capability", meter: &pass.meter)
        } catch let error as ClientError {
            switch error {
            case .unauthorized, .forbidden: throw error
            default:
                _ = try client.setServerCompactionState(supported: false, active: false)
                return
            }
        } catch {
            // A 404 is an older server; unavailable capability negotiation remains fail-closed.
            _ = try client.setServerCompactionState(supported: false, active: false)
            return
        }
        let cut: [String: Any]
        do {
            cut = try await server.get("/v1/snapshot", limit: ServerClient.maxSmallBytes, meter: &pass.meter)
        } catch let error as ClientError {
            switch error {
            case .unauthorized, .forbidden: throw error
            default:
                _ = try client.setServerCompactionState(supported: false, active: false)
                return
            }
        } catch {
            _ = try client.setServerCompactionState(supported: false, active: false)
            return
        }
        let supported = try compactionGeneration(cut) != nil
        let active: Bool
        if let value = cut["compaction_active"] {
            guard let enabled = value as? Bool else { throw ClientError.invalidResponse("compaction_active") }
            active = enabled
        } else {
            active = false
        }
        // This also performs one bounded local frontier-backfill step on each successful probe.
        _ = try client.setServerCompactionState(supported: supported, active: active)
    }

    private func compactionGeneration(_ cut: [String: Any]) throws -> String? {
        guard let supported = cut["compaction_supported"] else { return nil }
        guard let enabled = supported as? Bool else { throw ClientError.invalidResponse("compaction_supported") }
        guard enabled else { return nil }
        let value = try cut.string("compaction_generation")
        guard let generation = UInt64(value), String(generation) == value else {
            throw ClientError.invalidResponse("compaction_generation")
        }
        return value
    }

    private func uploadOutbox(_ pass: inout Pass) async throws {
        let batch = try client.pendingOutboxJsonBatch(limit: UInt64(budget.maxUploads) + 1)
        if batch.count > budget.maxUploads { pass.report.complete = false }
        for json in batch.prefix(budget.maxUploads) {
            try Task.checkCancellation()
            let data = Data(json.utf8)
            guard let envelope = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else {
                throw ClientError.invalidResponse("core outbox envelope")
            }
            let path = envelope["purpose"] as? String == "command" ? "/v1/commands" : "/v1/events"
            do {
                let accepted = try await server.post(path, json: data, meter: &pass.meter)
                _ = try accepted.decimal("cursor")
            } catch let error as ClientError where Self.refusesEnvelopeOnly(error) {
                // Keep the envelope queued and in order; receive still runs.
                pass.report.uploadRefused = error
                pass.report.complete = false
                return
            }
            try client.ackOutbox(envelopeId: try envelope.string("envelope_id"))
            pass.report.uploaded += 1
        }
    }

    /// Failures that concern one envelope rather than the connection, credential or task.
    /// Authorization, cancellation, origin/redirect violations, network faults and budget
    /// exhaustion still stop the pass.
    static func refusesEnvelopeOnly(_ error: ClientError) -> Bool {
        switch error {
        case .server, .requestTooLarge, .responseTooLarge, .invalidResponse: true
        default: false
        }
    }

    private func receive(_ pass: inout Pass) async throws {
        var after = try localCursor()
        while true {
            let page: [String: Any]
            do {
                page = try await server.get("/v1/events", query: [
                    URLQueryItem(name: "after", value: String(after)),
                    URLQueryItem(name: "limit", value: String(budget.pageLimit)),
                ], limit: ServerClient.maxPageBytes, meter: &pass.meter)
            } catch ClientError.resyncRequired {
                try await startSnapshot(restartOnResync: true, &pass)
                if try snapshotPending(pass) { return }
                after = try localCursor()
                continue
            }
            var last = after
            for event in try page.objects("events") {
                try Task.checkCancellation()
                let cursor = try event.decimal("cursor")
                guard cursor > last else { throw ClientError.invalidResponse("event cursor order") }
                last = cursor
                let result = try client.ingestRaw(envelopeJson: try event.object("envelope").jsonData(), cursor: String(cursor))
                switch result.state {
                case .journaled: pass.report.journaled += 1
                case .duplicate: pass.report.duplicates += 1
                case .quarantined: pass.report.quarantinedOnIngest += 1
                }
            }
            guard let next = try page.optionalDecimal("next_after") else { return }
            // Continue after the page actually served, even if the core cursor stops at a gap.
            guard next >= last, next > after else { throw ClientError.invalidResponse("next_after") }
            after = next
        }
    }

    private func localCursor() throws -> UInt64 {
        guard let cursor = UInt64(try client.receiveCursor()) else { throw ClientError.invalidResponse("receive cursor") }
        return cursor
    }

    /// A latched repair fetches exactly one fenced (compaction) snapshot per pass, before any
    /// contact effects. A projection that is still draining/staging is left to finish, and a
    /// failure caused by locked keys is reported rather than refetched.
    private func repairIfRequired(_ pass: inout Pass) async throws {
        guard try client.contactRepairRequired(), try !snapshotPending(pass) else { return }
        if let status = try client.snapshotProjectionStatus() {
            switch status.state {
            case .draining, .staging: return
            case .failed where status.reason == "keys_unavailable":
                pass.report.projectionFailure = status.reason
                return
            default: break
            }
        }
        pass.report.repairSnapshotStarted = true
        try await startSnapshot(restartOnResync: true, requireCompaction: true, &pass)
    }

    /// Fixes a server cut and stages it from the start. A published generation that is still
    /// draining is finished first, because the core refuses a new snapshot until it is.
    private func startSnapshot(restartOnResync: Bool, requireCompaction: Bool = false, _ pass: inout Pass) async throws {
        // Without apply rounds left the drain state is unknown, so no new generation is begun.
        guard pass.applyRounds < budget.maxApplyRounds else { throw ClientError.budgetExhausted }
        try applyPending(&pass)
        guard pass.report.snapshotRemaining == 0 else { return }
        let cut = try await server.get("/v1/snapshot", limit: ServerClient.maxSmallBytes, meter: &pass.meter)
        guard try cut.integer("snapshot_version") == 1 else { throw ClientError.invalidResponse("snapshot_version") }
        guard UUID(uuidString: try cut.string("vault_id"))?.uuidString.lowercased() == vaultId else {
            throw ClientError.identityMismatch
        }
        let generation = try compactionGeneration(cut)
        if requireCompaction && generation == nil {
            // A legacy snapshot cannot clear the repair latch; wait for a compaction-capable server.
            pass.report.repairUnavailable = true
            pass.report.repairSnapshotStarted = false
            return
        }
        let progress = try client.beginSnapshotWithCompaction(
            highWater: String(try cut.decimal("high_water_cursor")),
            recordCount: try cut.decimal("record_count"),
            purpose: .resync,
            serverCompactionGeneration: generation
        )
        try await importSnapshot(resuming: progress, restartOnResync: restartOnResync, &pass)
    }

    /// A staged generation or undrained published records must finish before live replay continues.
    private func snapshotPending(_ pass: Pass) throws -> Bool {
        try client.snapshotProgress() != nil || pass.report.snapshotRemaining > 0
    }

    private func importSnapshot(
        resuming start: NativeSnapshotProgress, restartOnResync: Bool, _ pass: inout Pass
    ) async throws {
        var progress = start
        while progress.receivedRecords < progress.expectedRecords {
            let page: [String: Any]
            do {
                page = try await server.get("/v1/snapshot/records", query: [
                    URLQueryItem(name: "high_water", value: progress.highWater),
                    URLQueryItem(name: "after", value: progress.lastCursor),
                    URLQueryItem(name: "limit", value: String(min(budget.pageLimit, 200))),
                    URLQueryItem(name: "compaction_generation", value: progress.serverCompactionGeneration),
                ], limit: ServerClient.maxPageBytes, meter: &pass.meter)
            } catch ClientError.resyncRequired(let reason) {
                // The cut is no longer valid on the server (e.g. rollback); restage once from a new cut.
                guard restartOnResync else { throw ClientError.resyncRequired(reason: reason) }
                return try await startSnapshot(restartOnResync: false, &pass)
            }
            let records = try page.objects("records").map { record in
                NativeRawSnapshotRecord(
                    cursor: String(try record.decimal("cursor")),
                    envelopeJson: try record.object("envelope").jsonData()
                )
            }
            guard !records.isEmpty else { throw ClientError.invalidResponse("short snapshot") }
            try Task.checkCancellation()
            progress = try client.appendSnapshotRawPage(generation: progress.generation, records: records)
            pass.report.snapshotRecords += records.count
        }
        _ = try client.finishSnapshot(generation: progress.generation)
        pass.report.snapshotPublished = true
        // Published, nothing drained yet; `applyPending` refines this.
        pass.report.snapshotRemaining = progress.expectedRecords
        try applyPending(&pass)
    }

    /// Drains published snapshot records and applies journaled ones until idle or out of rounds.
    private func applyPending(_ pass: inout Pass) throws {
        while pass.applyRounds < budget.maxApplyRounds {
            try Task.checkCancellation()
            pass.applyRounds += 1
            let step = try client.applyPending(limit: budget.applyLimit)
            pass.report.applied += step.applied
            pass.report.superseded += step.superseded
            pass.report.quarantined += step.quarantined
            pass.report.waitingForKeys = step.waitingForKeys
            pass.report.snapshotRemaining = step.snapshotRemaining
            if step.applied == 0 && step.drained == 0 && step.quarantined == 0 && step.superseded == 0 && step.snapshotRemaining == 0 { return }
        }
        pass.report.complete = false
    }
}
