import Foundation
import Security

/// Server-verified, non-secret enrollment metadata (public key profile and encrypted header).
struct EnrollmentRecord: Codable, Equatable, Sendable {
    var identity: EnrollmentIdentity
    var role: String
    var keyEpoch: UInt32
    var profileJson: String
    var headerJson: String
}

/// Keychain layout. Every item is bound to origin, vault and device in its account name:
/// `device-token|…`, `database-key|…`, `database-created|…`, `enrollment|…`, `key-cache-<epoch>|…`;
/// plus one `active-enrollment` pointer. Passphrases are never stored.
///
/// Disconnecting removes only the pointer. Per-identity items (and the encrypted database they open)
/// stay as an archive, so re-importing that identity reopens its unsent local work.
struct EnrollmentStore: Sendable {
    static let activeAccount = "active-enrollment"
    static let databaseKeyBytes = 32
    /// Older epochs whose caches are still imported (decrypt-only after cutover).
    static let retainedCacheEpochs: UInt32 = 64

    let secure: any SecureStore

    func activeIdentity() throws(ClientError) -> EnrollmentIdentity? {
        guard let data = try secure.read(Self.activeAccount) else { return nil }
        return try decode(EnrollmentIdentity.self, data, "active enrollment")
    }

    func record(_ identity: EnrollmentIdentity) throws(ClientError) -> EnrollmentRecord {
        guard let data = try secure.read(identity.account("enrollment")) else { throw .missingSecret("enrollment record") }
        let record = try decode(EnrollmentRecord.self, data, "enrollment record")
        guard record.identity == identity else { throw .identityMismatch }
        return record
    }

    func token(_ identity: EnrollmentIdentity) throws(ClientError) -> String {
        guard let data = try secure.read(identity.account("device-token")),
              let token = String(data: data, encoding: .utf8)
        else { throw .missingSecret("device token") }
        return token
    }

    /// Persists a verified enrollment and makes it active. Database keys are untouched.
    func activate(_ record: EnrollmentRecord, token: String) throws(ClientError) {
        try secure.upsert(record.identity.account("device-token"), Data(token.utf8))
        try saveRecord(record)
        try secure.upsert(Self.activeAccount, encode(record.identity))
    }

    func saveRecord(_ record: EnrollmentRecord) throws(ClientError) {
        try secure.upsert(record.identity.account("enrollment"), encode(record))
    }

    /// Removes only the active pointer; the identity's archive is kept.
    func deactivate() throws(ClientError) {
        try secure.delete(Self.activeAccount)
    }

    /// True once a database was created for this identity. Its file can then never be silently
    /// recreated (e.g. after reinstall, when the Keychain survives but the container does not).
    func databaseWasCreated(_ identity: EnrollmentIdentity) throws(ClientError) -> Bool {
        try secure.read(identity.account("database-created")) != nil
    }

    func markDatabaseCreated(_ identity: EnrollmentIdentity) throws(ClientError) {
        _ = try secure.addIfAbsent(identity.account("database-created"), Data([1]))
    }

    /// The SQLCipher key. A new random key is created only when `mayCreate` (no database file
    /// exists); an existing key is never replaced.
    func databaseKey(_ identity: EnrollmentIdentity, mayCreate: Bool) throws(ClientError) -> Data {
        let account = identity.account("database-key")
        if let key = try secure.read(account) {
            guard key.count == Self.databaseKeyBytes else { throw .missingSecret("database key") }
            return key
        }
        guard mayCreate else { throw .missingSecret("database key") }
        var key = Data(count: Self.databaseKeyBytes)
        let status = key.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, Self.databaseKeyBytes, $0.baseAddress!) }
        guard status == errSecSuccess else { throw .secureStorage(status: status) }
        defer { key.resetBytes(in: 0..<key.count) }
        _ = try secure.addIfAbsent(account, key)
        // Read back: a concurrent writer may have won, and its key is the one to use.
        return try databaseKey(identity, mayCreate: false)
    }

    func saveKeyCache(_ cache: Data, epoch: UInt32, for identity: EnrollmentIdentity) throws(ClientError) {
        try secure.upsert(identity.account("key-cache-\(epoch)"), cache)
    }

    func keyCache(_ identity: EnrollmentIdentity, epoch: UInt32) throws(ClientError) -> Data? {
        try secure.read(identity.account("key-cache-\(epoch)"))
    }

    /// Stored opaque key caches, newest epoch first.
    func keyCaches(_ identity: EnrollmentIdentity, through epoch: UInt32) throws(ClientError) -> [(epoch: UInt32, cache: Data)] {
        let oldest = epoch > Self.retainedCacheEpochs ? epoch - Self.retainedCacheEpochs + 1 : 1
        var caches: [(epoch: UInt32, cache: Data)] = []
        for candidate in stride(from: epoch, through: oldest, by: -1) {
            if let cache = try keyCache(identity, epoch: candidate) { caches.append((candidate, cache)) }
        }
        return caches
    }

    private func encode(_ value: some Encodable) throws(ClientError) -> Data {
        do { return try JSONEncoder().encode(value) } catch { throw .invalidResponse("unencodable enrollment") }
    }

    private func decode<T: Decodable>(_ type: T.Type, _ data: Data, _ what: String) throws(ClientError) -> T {
        do { return try JSONDecoder().decode(type, from: data) } catch { throw .missingSecret(what) }
    }
}
