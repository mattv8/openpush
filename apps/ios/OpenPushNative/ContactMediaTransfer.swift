import Foundation
#if canImport(OpenPushBindings)
import OpenPushBindings
#endif

public struct ContactMediaReport: Sendable, Equatable {
    public var uploaded = 0
    public var registered = 0
    public var downloaded = 0
    /// Remote objects that answered 404 (released or never finalized); not retried hot.
    public var unavailable = 0
    public var reclaimed = 0
    /// Reclaim candidates not yet provable by the server's replay floor, or 409 (backoff in core).
    public var reclaimDeferred = 0
    public var failed = 0

    public init() {}
}

/// Contact photo transfer over the existing encrypted attachment API, driven by the core queues:
/// - upload: reserve (`reference_tracking:true`) → PUT ciphertext → finalize → `mark_attachment_uploaded`
/// - publish: seal → `POST /references` for every registration → acknowledge (only on 2xx); the core
///   withholds each photo-bearing envelope until all of its registrations are acknowledged
/// - download: verified ciphertext → `install_downloaded_attachment`
/// - reclaim: `DELETE` with the server's current compaction generation and replay floor
/// Plaintext never leaves the core except through `open_native_plaintext_file` for an OS write.
struct ContactMediaTransfer: Sendable {
    /// Contact photo ciphertext is a ≤64 KiB JPEG plus envelope overhead.
    static let maxCipherBytes = 256 * 1024

    let client: NativeClient
    let server: ServerClient
    /// Protected scratch directory for downloaded ciphertext (deleted after install).
    let scratch: URL

    // MARK: Upload and reference registration

    func uploadAndRegister(meter: inout RequestMeter, report: inout ContactMediaReport) async throws {
        let state = try Self.object(client.contactPhotoTransferStateJson())
        let uploads = (state["uploads"] as? [[String: Any]] ?? []).compactMap { $0["attachment_id"] as? String }
        if !uploads.isEmpty {
            let pending = Dictionary(try client.pendingUploads().map { ($0.attachmentId, $0) }, uniquingKeysWith: { first, _ in first })
            for id in uploads {
                guard let object = pending[id] else { continue }
                if try await upload(object, meter: &meter) { report.uploaded += 1 } else { report.failed += 1 }
            }
            // Sealing runs when the outbox is read; it creates the registration rows for photo rows.
            _ = try client.pendingOutboxJsonBatch(limit: 200)
        }
        try await register(meter: &meter, report: &report)
    }

    /// One reference-tracked upload. Returns false when the server refused this object; transport,
    /// authorization and budget failures throw.
    func upload(_ object: NativeCipherObject, meter: inout RequestMeter) async throws -> Bool {
        guard object.ciphertextBytes > 0, object.ciphertextBytes <= Self.maxCipherBytes else { return false }
        let path = try client.nativeCipherFileForUpload(attachmentId: object.attachmentId)
        let data = try Data(contentsOf: URL(fileURLWithPath: path))
        guard UInt64(data.count) == object.ciphertextBytes else { return false }
        // The local id doubles as the reservation id, so a retry after a crash is idempotent.
        let reserve = try JSONSerialization.data(withJSONObject: [
            "attachment_id": object.attachmentId,
            "declared_ciphertext_bytes": data.count,
            "declared_ciphertext_sha256": object.ciphertextSha256,
            "reference_tracking": true,
        ])
        let reserved = try await server.exchange(
            "POST", "/v1/attachments/reserve", body: reserve, contentType: "application/json",
            limit: ServerClient.maxSmallBytes, meter: &meter
        )
        guard (200..<300).contains(reserved.status),
              let remote = (try? JSONSerialization.jsonObject(with: reserved.body) as? [String: Any])?["attachment_id"] as? String
        else { return false }
        let put = try await server.exchange(
            "PUT", "/v1/attachments/\(remote)/upload", body: data, contentType: "application/octet-stream",
            limit: ServerClient.maxSmallBytes, meter: &meter
        )
        guard (200..<300).contains(put.status) else { return false }
        let finalized = try await server.exchange(
            "POST", "/v1/attachments/\(remote)/finalize", limit: ServerClient.maxSmallBytes, meter: &meter
        )
        guard (200..<300).contains(finalized.status) else { return false }
        try client.markAttachmentUploaded(attachmentId: object.attachmentId, remoteObjectId: remote)
        return true
    }

    func register(meter: inout RequestMeter, report: inout ContactMediaReport) async throws {
        let state = try Self.object(client.contactPhotoTransferStateJson())
        for row in state["registrations"] as? [[String: Any]] ?? [] {
            guard let envelope = row["envelope_id"] as? String, let remote = row["attachment_id"] as? String,
                  let producer = row["producer_device_id"] as? String, let sequence = row["producer_sequence"] as? String
            else { continue }
            let body = try JSONSerialization.data(withJSONObject: [
                "references": [["producer_device_id": producer, "producer_sequence": sequence]],
            ])
            let response = try await server.exchange(
                "POST", "/v1/attachments/\(remote)/references", body: body, contentType: "application/json",
                limit: ServerClient.maxSmallBytes, meter: &meter
            )
            // Only a 2xx proves the server holds the reference; anything else keeps the envelope withheld.
            guard (200..<300).contains(response.status) else { report.failed += 1; continue }
            _ = try client.acknowledgeContactPhotoReferenceJson(inputJson: Self.json([
                "schema_version": 1, "envelope_id": envelope, "attachment_id": remote,
            ]))
            report.registered += 1
        }
    }

    // MARK: Download

    /// Downloads the requested pending attachments (all when `only` is nil). Returns ids whose
    /// remote object is gone (404), which callers must not retry hot.
    func download(only: Set<String>?, meter: inout RequestMeter, report: inout ContactMediaReport) async throws -> Set<String> {
        var gone = Set<String>()
        for object in try client.pendingDownloads() where only?.contains(object.attachmentId) ?? true {
            guard let remote = object.remoteObjectId, object.ciphertextBytes <= Self.maxCipherBytes else { continue }
            let response = try await server.exchange(
                "GET", "/v1/attachments/\(remote)", limit: Int(object.ciphertextBytes), meter: &meter
            )
            switch response.status {
            case 200:
                try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
                let file = scratch.appendingPathComponent("\(UUID().uuidString).opss")
                defer { try? FileManager.default.removeItem(at: file) }
                try response.body.write(to: file, options: [.atomic])
                // The core verifies size and SHA-256 before installing.
                try client.installDownloadedAttachment(attachmentId: object.attachmentId, downloadedPath: file.path)
                report.downloaded += 1
            case 404:
                gone.insert(object.attachmentId)
                report.unavailable += 1
            default:
                report.failed += 1
            }
        }
        return gone
    }

    // MARK: Reclaim

    func reclaim(meter: inout RequestMeter, report: inout ContactMediaReport) async throws {
        let state = try Self.object(client.contactPhotoTransferStateJson())
        let candidates = state["reclaims"] as? [[String: Any]] ?? []
        report.reclaimDeferred += state["reclaims_deferred"] as? Int ?? 0
        guard !candidates.isEmpty else { return }
        let cut = try await server.get("/v1/snapshot", limit: ServerClient.maxSmallBytes, meter: &meter)
        guard cut["compaction_supported"] as? Bool == true, let generation = cut["compaction_generation"] as? String,
              let floor = UInt64(try cut.string("replay_floor_cursor"))
        else {
            report.reclaimDeferred += candidates.count
            return
        }
        for candidate in candidates {
            guard let local = candidate["attachment_id"] as? String, let remote = candidate["remote_object_id"] as? String,
                  let after = (candidate["release_after_cursor"] as? String).flatMap(UInt64.init), floor >= after
            else {
                // Not every own reference has been echoed back yet, or the floor has not passed it.
                report.reclaimDeferred += 1
                continue
            }
            let body = try JSONSerialization.data(withJSONObject: [
                "compaction_generation": generation, "release_before_cursor": String(floor),
            ])
            let response = try await server.exchange(
                "DELETE", "/v1/attachments/\(remote)", body: body, contentType: "application/json",
                limit: ServerClient.maxSmallBytes, meter: &meter
            )
            guard [204, 404, 409].contains(response.status) else { report.failed += 1; continue }
            let ack = try Self.object(client.acknowledgeContactPhotoReclaimJson(inputJson: Self.json([
                "schema_version": 1, "attachment_id": local, "http_status": response.status,
            ])))
            if ack["status"] as? String == "completed" { report.reclaimed += 1 } else { report.reclaimDeferred += 1 }
        }
    }

    static func json(_ value: [String: Any]) throws -> String {
        String(decoding: try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]), as: UTF8.self)
    }

    static func object(_ text: String) throws -> [String: Any] {
        guard let value = try JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any] else {
            throw ContactsCoordinatorError.invalidCoreResponse("json")
        }
        return value
    }
}
