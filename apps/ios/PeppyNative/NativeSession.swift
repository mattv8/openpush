import Foundation
#if canImport(PeppyBindings)
import PeppyBindings
#endif

/// What the UI may show. Opening the encrypted database does not imply the vault keys are unlocked.
public struct SessionStatus: Equatable, Sendable {
    public var identity: EnrollmentIdentity?
    public var role: String?
    public var keyEpoch: UInt32?
    public var databaseOpen = false
    public var keysUnlocked = false
    /// Stored key caches that the core rejected while reopening (kept, not deleted).
    public var rejectedKeyCaches = 0
    /// Nil when the core could not list conversations.
    public var conversations: Int?
}

public struct EnrollmentClaim: Equatable, Sendable {
    public let origin: String
    public let intentToken: String
    public let deviceId: String
    public let keyDigest: String
    public let sas: String
}

public struct DeviceRosterItem: Identifiable, Equatable, Sendable {
    public let id: String
    public let role: String
    public let revoked: Bool
    public let keyEpoch: UInt32
}

/// Owns the single process-wide `NativeClient`, its Keychain-held secrets and foreground sync.
/// All message state lives in the Rust core; this actor only moves opaque values.
///
/// Enrollment changes (import, refresh, disconnect) are serialized: one runs at a time, and every
/// identity decision is re-checked on the actor after the network await, right before it is saved.
public actor NativeSession {
    private let store: EnrollmentStore
    private let transport: any HTTPTransport
    private let databaseDirectory: URL
    private let allowLoopbackHTTP: Bool
    private var client: NativeClient?
    private var record: EnrollmentRecord?
    private var status = SessionStatus()
    private var syncing = false
    private var changingEnrollment = false

    public init(
        secureStore: any SecureStore,
        transport: any HTTPTransport,
        databaseDirectory: URL,
        allowLoopbackHTTP: Bool
    ) {
        store = EnrollmentStore(secure: secureStore)
        self.transport = transport
        self.databaseDirectory = databaseDirectory
        self.allowLoopbackHTTP = allowLoopbackHTTP
    }

    // MARK: Enrollment

    /// Reads a credential file (bounded), verifies it against its server and persists it.
    public func importCredential(file: URL) async throws -> SessionStatus {
        let data: Data
        do {
            let handle = try FileHandle(forReadingFrom: file)
            defer { try? handle.close() }
            data = try handle.read(upToCount: DeviceCredential.maxFileBytes + 1) ?? Data()
        } catch {
            throw ClientError.invalidCredential("unreadable file")
        }
        return try await importCredential(data)
    }

    /// Imports a credential for a new enrollment, or replaces the token of the active one
    /// (same origin, vault and device). A different active enrollment must be disconnected first.
    public func importCredential(_ data: Data) async throws -> SessionStatus {
        let credential = try DeviceCredential.parse(data, allowLoopbackHTTP: allowLoopbackHTTP)
        return try await changeEnrollment {
            try self.requireAdoptable(credential.identity)
            let verified = try await EnrollmentVerifier(transport: self.transport)
                .verify(origin: credential.origin, token: credential.token, identity: credential.identity)
            // Re-checked after the await: another change may have completed meanwhile.
            try self.requireAdoptable(credential.identity)
            try self.store.activate(verified, token: credential.token)
            return try self.adopt(verified)
        }
    }

    /// Re-reads the active enrollment's profile, epoch and role from the server with the stored token.
    public func refreshEnrollment() async throws -> SessionStatus {
        try await changeEnrollment {
            guard let identity = try self.store.activeIdentity() else { throw ClientError.notEnrolled }
            let verified = try await EnrollmentVerifier(transport: self.transport).verify(
                origin: try ServerOrigin(canonical: identity.origin, allowLoopbackHTTP: self.allowLoopbackHTTP),
                token: try self.store.token(identity),
                identity: identity
            )
            guard try self.store.activeIdentity() == identity else { throw ClientError.identityMismatch }
            try self.store.saveRecord(verified)
            return try self.adopt(verified)
        }
    }

    /// Stops using the active enrollment. Nothing is deleted: the encrypted database, its key,
    /// key caches, token and unacknowledged outbox stay archived under that identity, and
    /// re-importing the same credential reopens them.
    public func disconnect() async throws -> SessionStatus {
        try await changeEnrollment {
            let storedIdentity = try self.store.activeIdentity()
            guard let active = self.record?.identity ?? storedIdentity else {
                self.close(); return self.status
            }
            let server = try self.authenticatedServer(active)
            var meter = RequestMeter(limit: 2)
            let response = try await server.exchange("POST", "/v1/devices/\(active.deviceId)/revoke", body: Data("{}".utf8), contentType: "application/json", limit: ServerClient.maxSmallBytes, meter: &meter)
            // A 401 for this identity is already proof its credential is unusable; archive it.
            guard (200..<300).contains(response.status) || response.status == 401 else { throw ClientError.server(status: response.status, code: nil) }
            guard try self.store.activeIdentity() == active else { throw ClientError.identityMismatch }
            self.close()
            try self.store.deactivate()
            return self.status
        }
    }

    /// Explicit offline escape hatch. It archives local state but leaves the server credential live.
    public func disconnectLocalOnly() async throws -> SessionStatus {
        try await changeEnrollment {
            let storedIdentity = try self.store.activeIdentity()
            let expected = self.record?.identity ?? storedIdentity
            guard expected == nil || storedIdentity == expected else { throw ClientError.identityMismatch }
            self.close()
            try self.store.deactivate()
            return self.status
        }
    }

    /// Parses only the JSON QR contract. Unknown fields, non-HTTPS origins and malformed/short
    /// intent tokens are refused before a key is generated or any request is made.
    public func claimPairingIntent(qrData: Data) async throws -> EnrollmentClaim {
        guard qrData.count <= 4096,
              let value = try? JSONSerialization.jsonObject(with: qrData) as? [String: Any],
              Set(value.keys) == ["https_origin", "intent_token"],
              let rawOrigin = value["https_origin"] as? String,
              let intent = value["intent_token"] as? String,
              intent.utf8.count == 43,
              intent.utf8.allSatisfy({ $0.isASCIIBase64URL }) else { throw ClientError.invalidCredential("pairing QR") }
        let origin = try ServerOrigin(canonical: rawOrigin, allowLoopbackHTTP: allowLoopbackHTTP)
        var keepPairingSecrets = false
        defer {
            if !keepPairingSecrets { try? store.clearPairingSecrets(origin: origin.serialized, intent: intent) }
        }
        let seed: Data
        let key: NativeEnrollmentKey
        if let existing = try store.pairingSecret("seed", origin: origin.serialized, intent: intent) {
            seed = existing
            key = try nativeEnrollmentKeyFromNativeSecureStorage(seed: seed)
        } else {
            key = try generateNativeEnrollmentKey()
            seed = key.exportSeedForNativeSecureStorage()
            guard seed.count == 32 else { throw ClientError.invalidCredential("pairing key") }
            try store.savePairingSecret(seed, purpose: "seed", origin: origin.serialized, intent: intent)
        }
        let deviceId: String
        if let persisted = try store.pairingSecret("device-id", origin: origin.serialized, intent: intent),
           let value = String(data: persisted, encoding: .utf8), UUID(uuidString: value) != nil {
            deviceId = value
        } else {
            deviceId = UUID().uuidString.lowercased()
            try store.savePairingSecret(Data(deviceId.utf8), purpose: "device-id", origin: origin.serialized, intent: intent)
        }
        let publicKey = try key.publicKeyBase64url()
        let response = try await anonymousJSON(origin: origin, method: "POST", path: "/v1/pairing/intents/\(intent)/claim", object: [
            "device_id": deviceId, "public_key": ["ed25519_public_key": publicKey], "requested_role": "gateway",
        ])
        let serverDigest = try response.string("key_digest")
        // This generated API first compares the server digest with this phone's public key, then
        // computes the SAS locally. Do not trust a server-provided display code.
        let sas = try key.pairingSas(intentToken: intent, deviceId: deviceId, serverKeyDigest: serverDigest)
        if let serverSas = response["sas"] as? String, serverSas != sas { throw ClientError.invalidResponse("pairing SAS") }
        let secret = try response.string("claim_secret")
        try store.savePairingSecret(Data(secret.utf8), purpose: "claim-secret", origin: origin.serialized, intent: intent)
        keepPairingSecrets = true
        return EnrollmentClaim(origin: origin.serialized, intentToken: intent, deviceId: deviceId, keyDigest: serverDigest, sas: sas)
    }

    /// Retrieves an owner-approved challenge, signs canonical Rust-owned proof bytes and consumes
    /// it. The signing seed and claim secret never leave the Keychain/native process.
    public func completePairing(_ claim: EnrollmentClaim) async throws -> SessionStatus {
        do {
            return try await completePairingAfterApproval(claim)
        } catch ClientError.pairingAwaitingApproval {
            throw ClientError.pairingAwaitingApproval
        } catch {
            try? store.clearPairingSecrets(origin: claim.origin, intent: claim.intentToken)
            throw error
        }
    }

    public func cancelPairing(_ claim: EnrollmentClaim) {
        try? store.clearPairingSecrets(origin: claim.origin, intent: claim.intentToken)
    }

    private func completePairingAfterApproval(_ claim: EnrollmentClaim) async throws -> SessionStatus {
        let origin = try ServerOrigin(canonical: claim.origin, allowLoopbackHTTP: allowLoopbackHTTP)
        guard let seed = try store.pairingSecret("seed", origin: claim.origin, intent: claim.intentToken),
              let secretData = try store.pairingSecret("claim-secret", origin: claim.origin, intent: claim.intentToken),
              let secret = String(data: secretData, encoding: .utf8) else { throw ClientError.missingSecret("pairing claim") }
        let key = try nativeEnrollmentKeyFromNativeSecureStorage(seed: seed)
        let publicKey = try key.publicKeyBase64url()
        let challenge = try await anonymousJSON(origin: origin, method: "POST", path: "/v1/pairing/intents/\(claim.intentToken)/challenge", object: [
            "device_id": claim.deviceId, "key_digest": claim.keyDigest, "claim_secret": secret,
        ])
        let token = try challenge.string("challenge_token")
        let vault = try challenge.string("vault_id")
        let fingerprint = try challenge.string("profile_fingerprint")
        guard let epoch = UInt32(exactly: try challenge.integer("key_epoch")) else { throw ClientError.invalidResponse("key_epoch") }
        let role = try challenge.string("requested_role")
        let proof = try pairingProofBytes(challengeToken: token, vaultId: vault, deviceId: claim.deviceId, profileFingerprint: fingerprint, keyEpoch: epoch, approvedRole: role)
        let signature = try key.signPairingProof(proof: proof)
        let credential = try await anonymousJSON(origin: origin, method: "POST", path: "/v1/pairing/consume", object: [
            "challenge_token": token, "device_id": claim.deviceId, "public_key": ["ed25519_public_key": publicKey],
            "profile_fingerprint": fingerprint, "key_epoch": Int(epoch), "signature": signature,
        ])
        let credentialData = try JSONSerialization.data(withJSONObject: [
            "version": 1, "origin": claim.origin, "vaultId": credential.string("vault_id"),
            "deviceId": credential.string("device_id"), "deviceToken": credential.string("device_token"),
        ])
        let parsed = try DeviceCredential.parse(credentialData, allowLoopbackHTTP: allowLoopbackHTTP)
        guard parsed.identity.deviceId == claim.deviceId else { throw ClientError.identityMismatch }
        try store.saveSigningKey(seed, for: parsed.identity)
        let result = try await importCredential(credentialData)
        try store.clearPairingSecrets(origin: claim.origin, intent: claim.intentToken)
        return result
    }

    public func devices() async throws -> [DeviceRosterItem] {
        guard let record else { throw ClientError.notEnrolled }
        var meter = RequestMeter(limit: 2)
        let server = try authenticatedServer(record.identity)
        let result = try await server.get("/v1/devices", limit: ServerClient.maxSmallBytes, meter: &meter)
        return try result.objects("devices").map { item in
            let id = try item.string("device_id")
            guard UUID(uuidString: id) != nil,
                  let epoch = UInt32(exactly: try item.integer("key_epoch")) else { throw ClientError.invalidResponse("device roster") }
            return DeviceRosterItem(id: id, role: try item.string("role"), revoked: item["revoked"] as? Bool ?? false, keyEpoch: epoch)
        }
    }

    public func revokeDevice(_ deviceId: String) async throws -> SessionStatus {
        guard UUID(uuidString: deviceId) != nil else { throw ClientError.invalidCredential("device id") }
        return try await changeEnrollment {
            guard let record = self.record else { throw ClientError.notEnrolled }
            var meter = RequestMeter(limit: 2)
            let server = try self.authenticatedServer(record.identity)
            let response = try await server.exchange("POST", "/v1/devices/\(deviceId)/revoke", body: Data("{}".utf8), contentType: "application/json", limit: ServerClient.maxSmallBytes, meter: &meter)
            guard (200..<300).contains(response.status) || (deviceId == record.identity.deviceId && response.status == 401) else { throw ClientError.server(status: response.status, code: nil) }
            if deviceId == record.identity.deviceId {
                guard self.record?.identity == record.identity, try self.store.activeIdentity() == record.identity else { throw ClientError.identityMismatch }
                self.close(); try self.store.deactivate()
            }
            return self.status
        }
    }

    public func deleteVault(vaultId: String) async throws {
        guard let record else { throw ClientError.notEnrolled }
        var meter = RequestMeter(limit: 2)
        let server = try authenticatedServer(record.identity)
        let response = try await server.exchange("DELETE", "/v1/vault", body: try JSONSerialization.data(withJSONObject: ["vault_id": vaultId]), contentType: "application/json", limit: ServerClient.maxSmallBytes, meter: &meter)
        guard (200..<300).contains(response.status) else { throw ClientError.server(status: response.status, code: nil) }
        _ = try await disconnect()
    }

    /// Authenticated server publication deliberately receives only the route ID and wake
    /// credential; provider/manage credentials remain in the relay Keychain record.
    public func publishWakeRoute(routeId: String, wakeCredential: String) async throws {
        guard let record else { throw ClientError.notEnrolled }
        var meter = RequestMeter(limit: 2)
        let server = try authenticatedServer(record.identity)
        let response = try await server.exchange(
            "PUT", "/v1/devices/self/wake-route",
            body: try JSONSerialization.data(withJSONObject: ["route_id": routeId, "wake_credential": wakeCredential]),
            contentType: "application/json", limit: ServerClient.maxSmallBytes, meter: &meter
        )
        guard (200..<300).contains(response.status) else { throw ClientError.server(status: response.status, code: nil) }
    }

    private func changeEnrollment(_ body: () async throws -> SessionStatus) async throws -> SessionStatus {
        guard !changingEnrollment else { throw ClientError.enrollmentChangeInProgress }
        changingEnrollment = true
        defer { changingEnrollment = false }
        do { return try await body() } catch { throw ClientError.wrap(error) }
    }

    /// The identity may become (or stay) active: nothing else is active and its database, if one
    /// was ever created, still exists.
    private func requireAdoptable(_ identity: EnrollmentIdentity) throws {
        if let active = try store.activeIdentity(), active != identity { throw ClientError.alreadyEnrolled }
        if let open = record?.identity, open != identity { throw ClientError.alreadyEnrolled }
        if try store.databaseWasCreated(identity), !databaseExists(identity) { throw ClientError.localDatabaseMissing }
    }

    /// Applies a verified record to the open client, or opens it.
    private func adopt(_ verified: EnrollmentRecord) throws -> SessionStatus {
        guard client != nil, record?.identity == verified.identity else { return try open() }
        let epochChanged = record?.keyEpoch != verified.keyEpoch
        record = verified
        status.role = verified.role
        status.keyEpoch = verified.keyEpoch
        if epochChanged {
            // Keys for a new epoch come only from its cache or from unlocking with the passphrase.
            status.keysUnlocked = false
            if let cache = try store.keyCache(verified.identity, epoch: verified.keyEpoch) {
                status.keysUnlocked = (try? client?.importNativeKeyCacheFromNativeStorage(bytes: cache)) != nil
            }
        }
        refreshCount()
        return status
    }

    // MARK: Database and keys

    /// Opens the active enrollment's database with its Keychain key and restores stored key caches.
    @discardableResult
    public func open() throws -> SessionStatus {
        do {
            guard let identity = try store.activeIdentity() else {
                close()
                return status
            }
            if let open = record?.identity, open != identity { throw ClientError.identityMismatch }
            let record = try store.record(identity)
            if client == nil {
                client = try openClient(identity)
                status = SessionStatus(identity: identity, databaseOpen: true)
                for (epoch, cache) in try store.keyCaches(identity, through: record.keyEpoch) {
                    do {
                        try client?.importNativeKeyCacheFromNativeStorage(bytes: cache)
                        if epoch == record.keyEpoch { status.keysUnlocked = true }
                    } catch {
                        status.rejectedKeyCaches += 1
                    }
                }
            }
            self.record = record
            status.role = record.role
            status.keyEpoch = record.keyEpoch
            refreshCount()
            return status
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// Unlocks with the vault's existing shared passphrase. The passphrase is passed to the core
    /// once and never stored; only the core's opaque key cache is kept in the Keychain.
    ///
    /// Only this manual unlock activates the server-verified epoch of the current record (a no-op
    /// when it is already active). Import, refresh and key-cache restoration never activate an epoch.
    public func unlock(passphrase: String) throws -> SessionStatus {
        guard let client, let record else { throw ClientError.notEnrolled }
        do {
            try client.unlock(profileJson: record.profileJson, headerJson: record.headerJson, passphrase: passphrase)
            try client.activateVerifiedEpoch(epoch: record.keyEpoch)
            let cache = try client.exportNativeKeyCacheForNativeStorage(epoch: record.keyEpoch)
            try store.saveKeyCache(cache, epoch: record.keyEpoch, for: record.identity)
        } catch {
            throw ClientError.wrap(error)
        }
        status.keysUnlocked = true
        refreshCount()
        return status
    }

    // MARK: Sync

    /// One bounded foreground pass. Cancel the calling task to stop it.
    public func syncOnce(budget: SyncBudget = SyncBudget()) async throws -> SyncReport {
        guard let client, let record else { throw ClientError.notEnrolled }
        guard !syncing else { throw ClientError.syncInProgress }
        syncing = true
        defer { syncing = false }
        let identity = record.identity
        do {
            let server = ServerClient(
                origin: try ServerOrigin(canonical: identity.origin, allowLoopbackHTTP: allowLoopbackHTTP),
                token: try store.token(identity),
                transport: transport
            )
            let report = try await ForegroundSync(client: client, server: server, vaultId: identity.vaultId, budget: budget).run()
            refreshCount()
            return report
        } catch {
            throw ClientError.wrap(error)
        }
    }

    // MARK: Contacts

    /// One bounded contact pass for foreground, change notifications and BackgroundTasks. It shares
    /// the sync guard, so it never overlaps a message sync. A background relaunch reopens the cached
    /// enrollment and key caches; locked keys (including a new epoch awaiting its passphrase) return
    /// `.needsUnlock` without capturing anything. Incoming records are applied before any permit.
    public func contactsPass(
        reason: ContactsPassReason,
        provider: any ContactsProvider,
        preferences: any ContactsPreferencesStore,
        budget: SyncBudget = SyncBudget(),
        isCancelled: @escaping @Sendable () -> Bool
    ) async throws -> ContactsSyncReport {
        guard preferences.load().enabled else { return ContactsSyncReport(outcome: .disabled) }
        if client == nil { _ = try open() }
        guard let client, let record else { throw ClientError.notEnrolled }
        guard status.keysUnlocked else { return ContactsSyncReport(outcome: .needsUnlock) }
        guard !syncing else { return ContactsSyncReport(outcome: .busy) }
        syncing = true
        defer { syncing = false }
        let identity = record.identity
        do {
            let server = ServerClient(
                origin: try ServerOrigin(canonical: identity.origin, allowLoopbackHTTP: allowLoopbackHTTP),
                token: try store.token(identity),
                transport: transport
            )
            let sync = ForegroundSync(client: client, server: server, vaultId: identity.vaultId, budget: budget)
            var syncError: ClientError?
            // Incoming records (requests, approvals, a repair snapshot) are applied before any permit.
            do {
                let pulled = try await sync.run()
                if pulled.contactRepairRequired, pulled.repairUnavailable { syncError = .unexpected("contact repair needs a compaction-capable server") }
            } catch {
                let wrapped = ClientError.wrap(error)
                if wrapped == .canceled { throw wrapped }
                syncError = wrapped // Offline is not fatal: capture stays local and uploads next time.
            }
            let scratch = databaseDirectory.appendingPathComponent("contact-media-tmp", isDirectory: true)
            try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
            try Self.protectUntilFirstUnlock(scratch)
            let media = syncError == nil ? ContactsMediaSession(
                transfer: ContactMediaTransfer(client: client, server: server, scratch: scratch),
                meter: RequestMeter(limit: budget.maxRequests)
            ) : nil
            var coordinator = NativeContactsCoordinator(
                client: client, deviceId: identity.deviceId, provider: provider, preferences: preferences
            )
            coordinator.scratch = scratch
            coordinator.media = media
            var report = try await coordinator.run(reason: reason, isCancelled: isCancelled)
            if let media, syncError == nil, !isCancelled(), !Task.isCancelled {
                // Ciphertext first, then references, so photo-bearing envelopes may publish.
                do {
                    try await media.transfer.uploadAndRegister(meter: &media.meter, report: &media.report)
                    try await media.transfer.reclaim(meter: &media.meter, report: &media.report)
                } catch {
                    let wrapped = ClientError.wrap(error)
                    if wrapped == .canceled { throw wrapped }
                    if wrapped != .budgetExhausted { syncError = wrapped }
                }
                report.media = media.report
            }
            if syncError == nil, !isCancelled(), !Task.isCancelled {
                do { _ = try await sync.run() } catch { syncError = ClientError.wrap(error) }
            }
            report.syncError = syncError?.userMessage
            refreshCount()
            return report
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// Owner book summary for settings: sanitized core state plus pending owner requests.
    public func contactsOverview() throws -> ContactsOverview {
        guard let client, let record else { throw ClientError.notEnrolled }
        do {
            let coordinator = NativeContactsCoordinator(
                client: client, deviceId: record.identity.deviceId, provider: NoContactsProvider(),
                preferences: InMemoryContactsPreferences()
            )
            return try coordinator.overview()
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// Owner policy for remote edits: `auto`, `confirm` or `off`. Stored and published by the core.
    public func setContactsRemoteEdits(_ mode: String) throws -> ContactsOverview {
        guard let client, let record else { throw ClientError.notEnrolled }
        do {
            let coordinator = NativeContactsCoordinator(
                client: client, deviceId: record.identity.deviceId, provider: NoContactsProvider(),
                preferences: InMemoryContactsPreferences()
            )
            try coordinator.setRemoteEdits(mode)
            return try coordinator.overview()
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// The account (OS container) new contacts are saved to; an owner setting stored by the core.
    public func setContactsDefaultAccount(_ accountId: String) throws -> ContactsOverview {
        guard let client, let record else { throw ClientError.notEnrolled }
        do {
            let coordinator = NativeContactsCoordinator(
                client: client, deviceId: record.identity.deviceId, provider: NoContactsProvider(),
                preferences: InMemoryContactsPreferences()
            )
            try coordinator.setDefaultAccount(accountId)
            return try coordinator.overview()
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// A local, explicit owner decision. Approval does not write: the next pass takes the permit.
    public func decideContactRequest(_ requestId: String, approve: Bool, scanHold: Bool = false) throws -> ContactsOverview {
        guard let client, let record else { throw ClientError.notEnrolled }
        do {
            let coordinator = NativeContactsCoordinator(
                client: client, deviceId: record.identity.deviceId, provider: NoContactsProvider(),
                preferences: InMemoryContactsPreferences()
            )
            try coordinator.decide(requestId, approve: approve, scanHold: scanHold)
            return try coordinator.overview()
        } catch {
            throw ClientError.wrap(error)
        }
    }

    /// Publishes the owned book as retired and turns syncing off. OS contacts are untouched; enabling
    /// again starts a new book.
    public func retireContacts(preferences: any ContactsPreferencesStore) throws -> ContactsOverview {
        guard let client, let record else { throw ClientError.notEnrolled }
        guard !syncing else { throw ClientError.syncInProgress }
        do {
            let coordinator = NativeContactsCoordinator(
                client: client, deviceId: record.identity.deviceId, provider: NoContactsProvider(),
                preferences: preferences
            )
            try coordinator.retire()
            var prefs = preferences.load()
            prefs.enabled = false
            preferences.save(prefs)
            return try coordinator.overview()
        } catch {
            throw ClientError.wrap(error)
        }
    }

    public func conversations() throws -> [NativeConversation] {
        guard let client else { throw ClientError.notEnrolled }
        do { return try client.listConversations() } catch { throw ClientError.wrap(error) }
    }

    /// Durable gateway policy is owned by the core. iOS only reads it to present honest,
    /// capability-gated settings; it never substitutes local preference rules.
    public func gatewaySettings() throws -> NativeGatewaySettings {
        guard let client else { throw ClientError.notEnrolled }
        do { return try client.gatewaySettings() } catch { throw ClientError.wrap(error) }
    }

    public func gatewayCapabilities() throws -> NativeGatewayCapabilities {
        guard let client else { throw ClientError.notEnrolled }
        do {
            return try client.gatewayCapabilities(
                platform: .ios,
                facts: NativeGatewayHostFacts(
                    wifiConnected: false,
                    notificationListenerAvailable: false,
                    smsAvailable: false,
                    notificationIsSilent: false
                )
            )
        } catch { throw ClientError.wrap(error) }
    }

    private func authenticatedServer(_ identity: EnrollmentIdentity) throws -> ServerClient {
        try ServerClient(
            origin: ServerOrigin(canonical: identity.origin, allowLoopbackHTTP: allowLoopbackHTTP),
            token: store.token(identity), transport: transport
        )
    }

    private func anonymousJSON(origin: ServerOrigin, method: String, path: String, object: [String: Any]) async throws -> [String: Any] {
        let body = try JSONSerialization.data(withJSONObject: object, options: [.withoutEscapingSlashes])
        let response = try await transport.send(HTTPRequest(method: method, url: origin.url(path), headers: ["Accept": "application/json", "Content-Type": "application/json"], body: body, maxResponseBytes: ServerClient.maxSmallBytes))
        guard origin.contains(response.url) else { throw ClientError.originMismatch }
        guard let result = try? JSONSerialization.jsonObject(with: response.body) as? [String: Any] else { throw ClientError.invalidResponse(path) }
        guard (200..<300).contains(response.status) else {
            if path.hasSuffix("/challenge"), response.status == 401, result["code"] as? String == "pairing_challenge_unavailable" {
                throw ClientError.pairingAwaitingApproval
            }
            throw ClientError.server(status: response.status, code: result["code"] as? String)
        }
        return result
    }

    /// Releases the client. Nothing on disk or in the Keychain changes.
    public func close() {
        try? client?.dispose()
        client = nil
        record = nil
        status = SessionStatus()
    }

    private func refreshCount() {
        status.conversations = try? client?.listConversations().count
    }

    private func databaseURL(_ identity: EnrollmentIdentity) -> URL {
        databaseDirectory.appendingPathComponent("\(identity.vaultId)-\(identity.deviceId).sqlcipher")
    }

    private func databaseExists(_ identity: EnrollmentIdentity) -> Bool {
        FileManager.default.fileExists(atPath: databaseURL(identity).path)
    }

    /// Background passes run while the device is locked after its first unlock, so the database,
    /// its sidecars and media must use `completeUntilFirstUserAuthentication`. New files inherit the
    /// directory's class; existing ones are set explicitly. A no-op off iOS.
    static func protectUntilFirstUnlock(_ directory: URL) throws {
        #if os(iOS)
        let attributes: [FileAttributeKey: Any] = [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication]
        let manager = FileManager.default
        try manager.setAttributes(attributes, ofItemAtPath: directory.path)
        guard let items = manager.enumerator(at: directory, includingPropertiesForKeys: nil) else { return }
        for case let item as URL in items {
            try manager.setAttributes(attributes, ofItemAtPath: item.path)
        }
        #endif
    }

    /// Fails closed when this identity's database was created before but is gone, or when a
    /// database file exists without its key; neither is ever replaced by a fresh database.
    private func openClient(_ identity: EnrollmentIdentity) throws -> NativeClient {
        let exists = databaseExists(identity)
        if try store.databaseWasCreated(identity), !exists { throw ClientError.localDatabaseMissing }
        var key = try store.databaseKey(identity, mayCreate: !exists)
        defer { key.resetBytes(in: 0..<key.count) }
        do {
            try FileManager.default.createDirectory(at: databaseDirectory, withIntermediateDirectories: true)
            var directory = databaseDirectory
            var values = URLResourceValues()
            // The database key is device-only, so a backed-up copy of the database could never be opened.
            values.isExcludedFromBackup = true
            try directory.setResourceValues(values)
            try Self.protectUntilFirstUnlock(databaseDirectory)
            let opened = try openNativeClient(config: NativeOpenConfig(
                databasePath: databaseURL(identity).path, vaultId: identity.vaultId, deviceId: identity.deviceId, databaseKey: key
            ))
            // SQLCipher creates its -wal/-shm sidecars and media directories on open.
            try Self.protectUntilFirstUnlock(databaseDirectory)
            try store.markDatabaseCreated(identity)
            return opened
        } catch {
            throw ClientError.wrap(error)
        }
    }
}

public struct SessionRelayWakeRoutePublisher: RelayWakeRoutePublisher {
    private let session: NativeSession
    public init(session: NativeSession) { self.session = session }
    public func publish(routeId: String, wakeCredential: String) async throws {
        try await session.publishWakeRoute(routeId: routeId, wakeCredential: wakeCredential)
    }
}

private extension UInt8 {
    var isASCIIBase64URL: Bool {
        (65...90).contains(self) || (97...122).contains(self) || (48...57).contains(self) || self == 45 || self == 95
    }
}
